//! CDP relay dispatchers: a virtual browser-root exposing every target in a context
//! ([`start_context_relay`]), and a direct-root passthrough exposing exactly one target
//! ([`start_target_relay`]). Both speak [`crate::session_transport::CdpEnvelope`] over an
//! accepted [`crate::relay_transport::RelayServerTransport`] and reuse the existing
//! [`CdpSessionMux`]/`Channel` machinery that `CdpConnection` uses when jsdbg is itself the CDP
//! client, just with the roles reversed: here jsdbg is the CDP server.

use std::collections::{BTreeMap, HashMap};
use std::sync::Arc;
use std::sync::OnceLock;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};

use async_trait::async_trait;
use hubrpc::connection::channel::{Channel, RequestHandler};
use hubrpc::prelude::{JsonRpcError, error_codes};
use serde::de::DeserializeOwned;
use serde_json::Value;
use tokio::sync::broadcast::error::RecvError;
use tokio::sync::{Mutex, oneshot, watch};
use tokio::task::JoinHandle;

use crate::cdp::{
    BrowserGetVersionResult, TargetAttachToTargetParams, TargetAttachToTargetResult,
    TargetAttachedToTargetParams, TargetDetachFromTargetParams, TargetDetachFromTargetResult,
    TargetDetachedFromTargetParams, TargetGetTargetsResult, TargetSetAutoAttachParams,
    TargetSetAutoAttachResult, TargetSetDiscoverTargetsParams, TargetSetDiscoverTargetsResult,
    TargetTargetCreatedParams, TargetTargetDestroyedParams, TargetTargetInfo,
    TargetTargetInfoChangedParams,
};
use crate::cdp_transport::ManagedCdpTransport;
use crate::debugger_service::DebuggerService;
use crate::relay_transport::{DEFAULT_ACCEPT_TIMEOUT, RelayListener, RelayServerTransport};
use crate::service_api::TargetSnapshot;
use crate::session_transport::CdpSessionMux;
use crate::target_debugger::TargetDebuggerHandle;

/// A running relay's control handle. `websocket_url` is returned to the RPC caller; `cancel`
/// lets the service force the relay closed (explicit `close_relay`, context deletion, service
/// stop); `completion` resolves once the relay has fully torn down, however it ended.
pub struct RelaySession {
    pub websocket_url: String,
    pub cancel: watch::Sender<bool>,
    pub completion: oneshot::Receiver<()>,
}

/// Starts a virtual browser-root relay exposing every target across every connection in
/// `context_id` as one CDP endpoint, over one authenticated loopback WebSocket.
pub async fn start_context_relay(
    service: DebuggerService,
    context_id: String,
    token: String,
) -> Result<RelaySession, crate::relay_transport::RelayTransportError> {
    let listener = crate::relay_transport::bind("context", &token).await?;
    Ok(spawn(listener, move |transport, cancel| {
        run_context_relay(service, context_id, transport, cancel)
    }))
}

/// Starts a direct-root relay exposing exactly one already-attachable target as CDP, over one
/// authenticated loopback WebSocket. The target is attached (if not already) before the
/// listener is created, so attachment failures surface synchronously to the RPC caller.
pub async fn start_target_relay(
    service: DebuggerService,
    context_id: String,
    connection_id: String,
    target_id: String,
    token: String,
) -> Result<RelaySession, crate::relay_transport::RelayTransportError> {
    let listener = crate::relay_transport::bind("target", &token).await?;
    Ok(spawn(listener, move |transport, cancel| {
        run_target_relay(
            service,
            context_id,
            connection_id,
            target_id,
            transport,
            cancel,
        )
    }))
}

/// Binds the accept-then-dispatch lifecycle shared by both relay flavors: wait for one
/// authenticated client (bounded by [`DEFAULT_ACCEPT_TIMEOUT`] and `cancel`), then hand it to
/// `dispatch`. Always resolves `completion`, whether a client ever connected or not.
fn spawn<F, Fut>(listener: RelayListener, dispatch: F) -> RelaySession
where
    F: FnOnce(Arc<RelayServerTransport>, watch::Receiver<bool>) -> Fut + Send + 'static,
    Fut: std::future::Future<Output = ()> + Send + 'static,
{
    let websocket_url = listener.websocket_url.clone();
    let (cancel, mut cancel_receiver) = watch::channel(false);
    let (completion_sender, completion) = oneshot::channel();
    tokio::spawn(async move {
        if let Ok(Some(transport)) = listener
            .accept(DEFAULT_ACCEPT_TIMEOUT, &mut cancel_receiver)
            .await
        {
            dispatch(Arc::new(transport), cancel_receiver).await;
        }
        let _ = completion_sender.send(());
    });
    RelaySession {
        websocket_url,
        cancel,
        completion,
    }
}

async fn run_target_relay(
    service: DebuggerService,
    context_id: String,
    connection_id: String,
    target_id: String,
    transport: Arc<RelayServerTransport>,
    mut cancel: watch::Receiver<bool>,
) {
    let Ok(handle) = service
        .relay_target_handle(&context_id, &connection_id, &target_id)
        .await
    else {
        return;
    };
    let mux = CdpSessionMux::new(transport.clone());
    let Ok(root_transport) = mux.open_root() else {
        return;
    };
    let replay_channel = Arc::new(OnceLock::new());
    let root_channel = Channel::new(
        Box::new(root_transport),
        Box::new(TargetForwardingHandler {
            handle: handle.clone(),
            replay_channel: replay_channel.clone(),
            replayed: AtomicBool::new(false),
        }),
    );
    let _ = replay_channel.set(root_channel.clone());

    let mut mux_task = tokio::spawn({
        let mux = mux.clone();
        async move { mux.run().await }
    });
    let mut channel_task = tokio::spawn({
        let root_channel = root_channel.clone();
        async move { root_channel.run().await }
    });
    let mut transport_closed = Box::pin(transport.wait_closed());
    let mut raw_events = handle.subscribe_raw_events();
    loop {
        tokio::select! {
            _ = cancel.changed() => break,
            result = &mut channel_task => { let _ = result; break; }
            result = &mut mux_task => { let _ = result; break; }
            _ = &mut transport_closed => break,
            event = raw_events.recv() => {
                match event {
                    Ok(event) => { let _ = root_channel.notify(&event.method, event.params).await; }
                    Err(RecvError::Lagged(_)) => continue,
                    Err(RecvError::Closed) => break,
                }
            }
        }
    }
    transport.close().await;
    mux.dispose();
    channel_task.abort();
    mux_task.abort();
}

async fn run_context_relay(
    service: DebuggerService,
    context_id: String,
    transport: Arc<RelayServerTransport>,
    mut cancel: watch::Receiver<bool>,
) {
    let mux = CdpSessionMux::new(transport.clone());
    let Ok(root_transport) = mux.open_root() else {
        return;
    };
    let state = Arc::new(ContextRelayState::new(service, context_id, mux.clone()));
    let root_channel = Channel::new(
        Box::new(root_transport),
        Box::new(RootHandler(state.clone())),
    );
    state.set_root_channel(root_channel.clone());
    state.prime_known_targets().await;

    let mut mux_task = tokio::spawn({
        let mux = mux.clone();
        async move { mux.run().await }
    });
    let mut channel_task = tokio::spawn({
        let root_channel = root_channel.clone();
        async move { root_channel.run().await }
    });
    let mut transport_closed = Box::pin(transport.wait_closed());
    let mut revision = state.service.relay_revision_signal();
    loop {
        tokio::select! {
            _ = cancel.changed() => break,
            result = &mut channel_task => { let _ = result; break; }
            result = &mut mux_task => { let _ = result; break; }
            _ = &mut transport_closed => break,
            changed = revision.changed() => {
                if changed.is_err() { break; }
                state.sync_targets().await;
            }
        }
    }
    transport.close().await;
    mux.dispose();
    channel_task.abort();
    mux_task.abort();
    state.dispose().await;
}

/// One flattened session the context relay opened for an attached target: the mux channel that
/// carries its opaque method/param traffic (torn down via `mux.retire_session`) plus the task
/// mirroring its raw CDP events onto that session.
struct RelayTargetSession {
    target_id: String,
    forward_task: JoinHandle<()>,
}

/// Mutable state backing the virtual browser root: which targets are known, whether discovery
/// and auto-attach are enabled, and the flattened sessions currently open.
struct ContextRelayState {
    service: DebuggerService,
    context_id: String,
    mux: CdpSessionMux,
    root_channel: OnceLock<Channel>,
    sessions: Mutex<HashMap<String, RelayTargetSession>>,
    known_targets: Mutex<BTreeMap<String, (String, TargetSnapshot)>>,
    discover: AtomicBool,
    auto_attach: AtomicBool,
    next_session_id: AtomicU64,
}

impl ContextRelayState {
    fn new(service: DebuggerService, context_id: String, mux: CdpSessionMux) -> Self {
        Self {
            service,
            context_id,
            mux,
            root_channel: OnceLock::new(),
            sessions: Mutex::new(HashMap::new()),
            known_targets: Mutex::new(BTreeMap::new()),
            discover: AtomicBool::new(false),
            auto_attach: AtomicBool::new(false),
            next_session_id: AtomicU64::new(1),
        }
    }

    fn set_root_channel(&self, channel: Channel) {
        let _ = self.root_channel.set(channel);
    }

    fn root_channel(&self) -> &Channel {
        self.root_channel
            .get()
            .expect("root channel is set before the client can send any request")
    }

    /// Silently seeds the known-target baseline so the first revision-triggered diff after a
    /// client enables discovery does not treat every pre-existing target as newly created.
    async fn prime_known_targets(&self) {
        if let Ok(targets) = self.service.relay_targets(&self.context_id).await {
            *self.known_targets.lock().await = index_targets(targets);
        }
    }

    async fn dispose(&self) {
        for (_, session) in std::mem::take(&mut *self.sessions.lock().await) {
            session.forward_task.abort();
        }
    }

    /// Re-reads the context's canonical target set and mirrors the delta as `Target.*`
    /// discovery events (gated by `discover`) and, for newly appeared targets, auto-attach
    /// (gated by `auto_attach`). Called each time the context-wide revision signal changes.
    async fn sync_targets(&self) {
        let Ok(current) = self.service.relay_targets(&self.context_id).await else {
            return;
        };
        let current = index_targets(current);
        let discover = self.discover.load(Ordering::Relaxed);
        let auto_attach = self.auto_attach.load(Ordering::Relaxed);

        let (created, changed, destroyed) = {
            let known = self.known_targets.lock().await;
            let created = current
                .iter()
                .filter(|(target_id, _)| !known.contains_key(*target_id))
                .map(|(_, entry)| entry.clone())
                .collect::<Vec<_>>();
            let changed = current
                .iter()
                .filter_map(|(target_id, (_, snapshot))| {
                    let previous = known.get(target_id)?;
                    (&previous.1 != snapshot).then(|| snapshot.clone())
                })
                .collect::<Vec<_>>();
            let destroyed = known
                .keys()
                .filter(|target_id| !current.contains_key(*target_id))
                .cloned()
                .collect::<Vec<_>>();
            (created, changed, destroyed)
        };

        for (connection_id, snapshot) in &created {
            if discover {
                self.notify_target_created(snapshot).await;
            }
            if auto_attach {
                self.ensure_session(connection_id, snapshot, true).await;
            }
        }
        if discover {
            for snapshot in &changed {
                self.notify_target_changed(snapshot).await;
            }
        }
        for target_id in &destroyed {
            if discover {
                self.notify_target_destroyed(target_id).await;
            }
            self.detach_sessions_for_target(target_id).await;
        }

        *self.known_targets.lock().await = current;
    }

    async fn lookup_target(&self, target_id: &str) -> Option<(String, TargetSnapshot)> {
        if let Some(found) = self.known_targets.lock().await.get(target_id).cloned() {
            return Some(found);
        }
        self.service
            .relay_targets(&self.context_id)
            .await
            .ok()?
            .into_iter()
            .find(|(_, snapshot)| snapshot.target_id == target_id)
    }

    /// Attaches (if needed) and opens a new flattened session for `target_id`, wiring opaque
    /// forwarding and raw event mirroring. Attachment failures are swallowed for the
    /// auto-attach path (best-effort for targets that momentarily disappear).
    async fn ensure_session(
        &self,
        connection_id: &str,
        snapshot: &TargetSnapshot,
        waiting_for_debugger: bool,
    ) -> Option<String> {
        let handle = self
            .service
            .relay_ensure_attached(&self.context_id, connection_id, &snapshot.target_id)
            .await
            .ok()?;
        Some(
            self.open_session(connection_id, snapshot, handle, waiting_for_debugger)
                .await,
        )
    }

    async fn open_session(
        &self,
        connection_id: &str,
        snapshot: &TargetSnapshot,
        handle: TargetDebuggerHandle,
        waiting_for_debugger: bool,
    ) -> String {
        let session_id = format!(
            "relay-session-{}",
            self.next_session_id.fetch_add(1, Ordering::Relaxed)
        );
        if let Ok(session_transport) = self.mux.open_session(session_id.clone()) {
            let replay_channel = Arc::new(OnceLock::new());
            let session_channel = Channel::new(
                Box::new(session_transport),
                Box::new(TargetForwardingHandler {
                    handle: handle.clone(),
                    replay_channel: replay_channel.clone(),
                    replayed: AtomicBool::new(false),
                }),
            );
            let _ = replay_channel.set(session_channel.clone());
            let run_channel = session_channel.clone();
            tokio::spawn(async move { run_channel.run().await });

            let mut raw_events = handle.subscribe_raw_events();
            let forward_channel = session_channel;
            let forward_task = tokio::spawn(async move {
                loop {
                    match raw_events.recv().await {
                        Ok(event) => {
                            let _ = forward_channel.notify(&event.method, event.params).await;
                        }
                        Err(RecvError::Lagged(_)) => continue,
                        Err(RecvError::Closed) => break,
                    }
                }
            });
            self.sessions.lock().await.insert(
                session_id.clone(),
                RelayTargetSession {
                    target_id: snapshot.target_id.clone(),
                    forward_task,
                },
            );
        }
        let _ = connection_id;
        let mut target_info = target_info_from_snapshot(snapshot);
        target_info.attached = true;
        let params = TargetAttachedToTargetParams {
            session_id: session_id.clone(),
            target_info,
            waiting_for_debugger,
        };
        if let Ok(value) = serde_json::to_value(params) {
            let _ = self
                .root_channel()
                .notify("Target.attachedToTarget", value)
                .await;
        }
        session_id
    }

    async fn detach(&self, session_id: Option<&str>, target_id: Option<&str>) {
        let matching_id = if let Some(session_id) = session_id {
            Some(session_id.to_owned())
        } else if let Some(target_id) = target_id {
            self.sessions
                .lock()
                .await
                .iter()
                .find(|(_, session)| session.target_id == target_id)
                .map(|(id, _)| id.clone())
        } else {
            None
        };
        let Some(session_id) = matching_id else {
            return;
        };
        let Some(session) = self.sessions.lock().await.remove(&session_id) else {
            return;
        };
        session.forward_task.abort();
        self.mux.retire_session(&session_id);
        let params = TargetDetachedFromTargetParams {
            session_id: session_id.clone(),
            target_id: Some(session.target_id),
        };
        if let Ok(value) = serde_json::to_value(params) {
            let _ = self
                .root_channel()
                .notify("Target.detachedFromTarget", value)
                .await;
        }
    }

    async fn detach_sessions_for_target(&self, target_id: &str) {
        let matching = self
            .sessions
            .lock()
            .await
            .iter()
            .filter(|(_, session)| session.target_id == target_id)
            .map(|(id, _)| id.clone())
            .collect::<Vec<_>>();
        for session_id in matching {
            self.detach(Some(&session_id), None).await;
        }
    }

    async fn notify_target_created(&self, snapshot: &TargetSnapshot) {
        let params = TargetTargetCreatedParams {
            target_info: target_info_from_snapshot(snapshot),
        };
        if let Ok(value) = serde_json::to_value(params) {
            let _ = self
                .root_channel()
                .notify("Target.targetCreated", value)
                .await;
        }
    }

    async fn notify_target_changed(&self, snapshot: &TargetSnapshot) {
        let params = TargetTargetInfoChangedParams {
            target_info: target_info_from_snapshot(snapshot),
        };
        if let Ok(value) = serde_json::to_value(params) {
            let _ = self
                .root_channel()
                .notify("Target.targetInfoChanged", value)
                .await;
        }
    }

    async fn notify_target_destroyed(&self, target_id: &str) {
        let params = TargetTargetDestroyedParams {
            target_id: target_id.to_owned(),
        };
        if let Ok(value) = serde_json::to_value(params) {
            let _ = self
                .root_channel()
                .notify("Target.targetDestroyed", value)
                .await;
        }
    }

    async fn handle_root_request(
        &self,
        method: String,
        params: Value,
    ) -> Result<Value, JsonRpcError> {
        match method.as_str() {
            "Browser.getVersion" => to_json(BrowserGetVersionResult {
                protocol_version: "1.3".to_owned(),
                product: format!("jsdbg-context-relay/{}", env!("CARGO_PKG_VERSION")),
                revision: String::new(),
                user_agent: "jsdbg-context-relay".to_owned(),
                js_version: String::new(),
            }),
            "Target.getTargets" => {
                let targets = self.service.relay_targets(&self.context_id).await?;
                to_json(TargetGetTargetsResult {
                    target_infos: targets
                        .iter()
                        .map(|(_, snapshot)| target_info_from_snapshot(snapshot))
                        .collect(),
                })
            }
            "Target.setDiscoverTargets" => {
                let request: TargetSetDiscoverTargetsParams = from_json(params)?;
                let was_enabled = self.discover.swap(request.discover, Ordering::Relaxed);
                if request.discover && !was_enabled {
                    let targets = self
                        .known_targets
                        .lock()
                        .await
                        .values()
                        .cloned()
                        .collect::<Vec<_>>();
                    for (_, snapshot) in &targets {
                        self.notify_target_created(snapshot).await;
                    }
                }
                to_json(TargetSetDiscoverTargetsResult::new())
            }
            "Target.setAutoAttach" => {
                let request: TargetSetAutoAttachParams = from_json(params)?;
                let was_enabled = self
                    .auto_attach
                    .swap(request.auto_attach, Ordering::Relaxed);
                if request.auto_attach && !was_enabled {
                    let targets = self
                        .known_targets
                        .lock()
                        .await
                        .values()
                        .cloned()
                        .collect::<Vec<_>>();
                    for (connection_id, snapshot) in &targets {
                        self.ensure_session(
                            connection_id,
                            snapshot,
                            request.wait_for_debugger_on_start,
                        )
                        .await;
                    }
                }
                to_json(TargetSetAutoAttachResult::new())
            }
            "Target.attachToTarget" => {
                let request: TargetAttachToTargetParams = from_json(params)?;
                let Some((connection_id, snapshot)) = self.lookup_target(&request.target_id).await
                else {
                    return Err(invalid_params(format!(
                        "no such target '{}' in this relay's context",
                        request.target_id
                    )));
                };
                let handle = self
                    .service
                    .relay_ensure_attached(&self.context_id, &connection_id, &snapshot.target_id)
                    .await?;
                let session_id = self
                    .open_session(&connection_id, &snapshot, handle, false)
                    .await;
                to_json(TargetAttachToTargetResult { session_id })
            }
            "Target.detachFromTarget" => {
                let request: TargetDetachFromTargetParams = from_json(params)?;
                self.detach(request.session_id.as_deref(), request.target_id.as_deref())
                    .await;
                to_json(TargetDetachFromTargetResult::new())
            }
            _ => Err(JsonRpcError::new(
                error_codes::METHOD_NOT_FOUND,
                format!(
                    "jsdbg context relay's virtual root does not implement '{method}'; attach to a target and send target-scoped commands instead"
                ),
            )),
        }
    }
}

fn index_targets(
    targets: Vec<(String, TargetSnapshot)>,
) -> BTreeMap<String, (String, TargetSnapshot)> {
    targets
        .into_iter()
        .map(|(connection_id, snapshot)| (snapshot.target_id.clone(), (connection_id, snapshot)))
        .collect()
}

fn target_info_from_snapshot(snapshot: &TargetSnapshot) -> TargetTargetInfo {
    let mut info = TargetTargetInfo::new(
        snapshot.target_id.clone(),
        snapshot.target_type.clone(),
        snapshot.title.clone(),
        snapshot.url.clone(),
        false,
        false,
    );
    info.parent_id = snapshot.parent_id.clone();
    info.opener_id = snapshot.opener_id.clone();
    info.browser_context_id = snapshot.browser_context_id.clone();
    info.subtype = snapshot.subtype.clone();
    info
}

fn to_json(value: impl serde::Serialize) -> Result<Value, JsonRpcError> {
    serde_json::to_value(value)
        .map_err(|error| JsonRpcError::new(error_codes::INTERNAL_ERROR, error.to_string()))
}

fn from_json<T: DeserializeOwned>(params: Value) -> Result<T, JsonRpcError> {
    serde_json::from_value(params).map_err(|error| invalid_params(error.to_string()))
}

fn invalid_params(message: impl Into<String>) -> JsonRpcError {
    JsonRpcError::new(error_codes::INVALID_PARAMS, message.into())
}

struct RootHandler(Arc<ContextRelayState>);

#[async_trait]
impl RequestHandler for RootHandler {
    async fn handle_request(&self, method: String, params: Value) -> Result<Value, JsonRpcError> {
        self.0.handle_root_request(method, params).await
    }
}

/// Forwards every request opaquely to one target's debugger handle. Used for the flattened
/// session of an attached target under a context relay, and directly as the root handler for a
/// target relay (which has no `Target.*` domain of its own - the root *is* the target).
struct TargetForwardingHandler {
    handle: TargetDebuggerHandle,
    replay_channel: Arc<OnceLock<Channel>>,
    replayed: AtomicBool,
}

#[async_trait]
impl RequestHandler for TargetForwardingHandler {
    async fn handle_request(&self, method: String, params: Value) -> Result<Value, JsonRpcError> {
        let result = self.handle.raw_cdp_request(method.clone(), params).await?;
        if method == "Debugger.enable" && !self.replayed.swap(true, Ordering::Relaxed) {
            let events = self.handle.raw_event_history().lock().unwrap().clone();
            if let Some(channel) = self.replay_channel.get() {
                for event in events {
                    let _ = channel.notify(&event.method, event.params).await;
                }
            }
        }
        Ok(result)
    }
}
