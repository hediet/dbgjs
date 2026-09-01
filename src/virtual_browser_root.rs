//! A provider-neutral virtual browser root.
//!
//! Chrome exposes a process tree of targets through the CDP `Target` domain; jsdbg uses the same
//! shape for every host that can host more than one debuggable target (today: an OS process tree,
//! possibly an Electron application). A [`TargetSource`] discovers targets and hands out one CDP
//! transport per target; [`VirtualBrowserRoot`] turns that into a browser-shaped CDP endpoint that
//! [`crate::cdp_runtime::CdpConnection`] can consume exactly like a real browser WebSocket.
//!
//! Discovery is demand driven. The source is only asked to discover while a client has enabled
//! `Target.setDiscoverTargets` or `Target.setAutoAttach`; `Target.getTargets` is a one-shot query
//! that never leaves discovery running behind.

use std::collections::BTreeMap;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, OnceLock};

use async_trait::async_trait;
use hubrpc::connection::channel::{Channel, RequestHandler};
use hubrpc::prelude::{error_codes, JsonRpcError, MessageTransport, TransportError};
use serde_json::Value;
use tokio::sync::{mpsc, watch, Mutex};
use tokio::task::JoinHandle;

use crate::cdp::{
    BrowserGetVersionResult, CdpClient, TargetAttachToTargetParams, TargetAttachToTargetResult,
    TargetAttachedToTargetParams, TargetDetachFromTargetParams, TargetDetachFromTargetResult,
    TargetDetachedFromTargetParams, TargetGetTargetsResult, TargetSetAutoAttachParams,
    TargetSetAutoAttachResult, TargetSetDiscoverTargetsParams, TargetSetDiscoverTargetsResult,
    TargetTargetCreatedParams, TargetTargetDestroyedParams, TargetTargetInfoChangedParams,
};
use crate::cdp_transport::{closed_transport_error, ManagedCdpTransport};
use crate::service_api::TargetSnapshot;
use crate::session_transport::{CdpEnvelope, CdpSessionMux};
use crate::target_domain::{from_json, invalid_params, target_info_from_snapshot, to_json};

/// One target a [`TargetSource`] knows about. `snapshot` is the provider-neutral CDP `TargetInfo`;
/// the remaining fields carry the two pieces of per-target state the debugger service needs but
/// CDP's `TargetInfo` cannot express.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct HostTarget {
    pub snapshot: TargetSnapshot,
    /// The OS process hosting the target, when the source knows it. Used to recognize the same
    /// physical target across connections.
    pub process_id: Option<u32>,
    /// Whether the target's startup is genuinely blocked waiting for a debugger to resume it.
    pub waiting_for_debugger: bool,
}

impl HostTarget {
    pub fn target_id(&self) -> &str {
        &self.snapshot.target_id
    }
}

#[derive(Clone, Debug)]
pub enum TargetSourceEvent {
    Upserted(HostTarget),
    Removed(String),
}

/// The result of attaching to one target through a [`TargetSource`].
pub struct TargetAttachment {
    pub endpoint: Arc<TargetEndpoint>,
    /// Whether a debugger that jsdbg does not own had to be evicted to get this attachment.
    pub stole_external_owner: bool,
}

/// The session [`VirtualBrowserRoot::attach_target`] opened.
pub struct AttachedSession {
    pub session_id: String,
    pub stole_external_owner: bool,
}

/// Everything the virtual root needs from a host in order to speak the `Target` domain on its
/// behalf. Implementations own all host-specific knowledge (OS scanning, Electron webContents,
/// inspector activation); nothing above this trait knows what an Electron renderer is.
#[async_trait]
pub trait TargetSource: Send + Sync + 'static {
    /// Reported as `Browser.getVersion`'s product, which the service uses as the connection title.
    fn product(&self) -> String;

    /// One-shot enumeration. Must not enable continuous discovery.
    async fn list_targets(&self) -> Vec<HostTarget>;

    /// Turns continuous discovery on or off. Called whenever the aggregate demand of
    /// `Target.setDiscoverTargets` and `Target.setAutoAttach` changes.
    async fn set_discovery(&self, enabled: bool);

    /// Arms or disarms genuine startup blocking for targets created from now on.
    async fn set_wait_for_debugger_on_start(&self, enabled: bool);

    /// Opens a CDP endpoint for one target. `force` allows evicting a foreign debugger.
    async fn attach(&self, target_id: &str, force: bool) -> Result<TargetAttachment, String>;

    /// Releases whatever `attach` acquired for `target_id`.
    async fn detach(&self, target_id: &str);

    /// Resolves once the host itself is gone, yielding the reason.
    async fn wait_closed(&self) -> String;

    async fn close(&self);
}

#[derive(Default)]
struct EndpointNotifications {
    sinks: std::sync::Mutex<Vec<mpsc::UnboundedSender<(String, Value)>>>,
}

impl EndpointNotifications {
    fn publish(&self, method: String, params: Value) {
        self.sinks
            .lock()
            .unwrap()
            .retain(|sink| sink.send((method.clone(), params.clone())).is_ok());
    }
}

struct EndpointHandler {
    notifications: Arc<EndpointNotifications>,
}

#[async_trait]
impl RequestHandler for EndpointHandler {
    async fn handle_request(&self, method: String, _params: Value) -> Result<Value, JsonRpcError> {
        Err(JsonRpcError::new(
            error_codes::METHOD_NOT_FOUND,
            format!("unexpected request from a debuggee target: {method}"),
        ))
    }

    async fn handle_notification(&self, method: String, params: Value) {
        self.notifications.publish(method, params);
    }
}

/// One target's CDP endpoint: a request/response channel plus a fan-out of its events. Endpoints
/// are shared - the Electron bridge and an attached debugger session both talk to the Electron
/// main process through the same endpoint.
pub struct TargetEndpoint {
    transport: Arc<dyn ManagedCdpTransport>,
    mux: CdpSessionMux,
    channel: Channel,
    notifications: Arc<EndpointNotifications>,
    owns_transport: bool,
    parent_client: Option<CdpClient<Channel>>,
    owned_session_id: Option<String>,
    tasks: std::sync::Mutex<Vec<JoinHandle<()>>>,
}

impl TargetEndpoint {
    pub fn open(transport: Arc<dyn ManagedCdpTransport>) -> Result<Arc<Self>, String> {
        let mux = CdpSessionMux::new(transport.clone());
        let notifications = Arc::new(EndpointNotifications::default());
        let channel = Channel::new(
            Box::new(mux.open_root().map_err(|error| error.to_string())?),
            Box::new(EndpointHandler {
                notifications: notifications.clone(),
            }),
        );
        let mux_task = tokio::spawn({
            let mux = mux.clone();
            async move { mux.run().await }
        });
        let channel_task = tokio::spawn({
            let channel = channel.clone();
            async move { channel.run().await }
        });
        Ok(Arc::new(Self {
            transport,
            mux,
            channel,
            notifications,
            owns_transport: true,
            parent_client: None,
            owned_session_id: None,
            tasks: std::sync::Mutex::new(vec![mux_task, channel_task]),
        }))
    }

    pub async fn attach_child(&self, target_id: &str) -> Result<Arc<Self>, String> {
        let mut params = TargetAttachToTargetParams::new(target_id.to_owned());
        params.flatten = Some(true);
        let attached = self
            .client()
            .target_attach_to_target(params)
            .await
            .map_err(|error| format!("Target.attachToTarget failed: {error:?}"))?;
        let session_id = attached.session_id;
        let session = self
            .mux
            .open_session(session_id.clone())
            .map_err(|error| error.to_string())?;
        let notifications = Arc::new(EndpointNotifications::default());
        let channel = Channel::new(
            Box::new(session),
            Box::new(EndpointHandler {
                notifications: notifications.clone(),
            }),
        );
        let channel_task = tokio::spawn({
            let channel = channel.clone();
            async move { channel.run().await }
        });
        Ok(Arc::new(Self {
            transport: self.transport.clone(),
            mux: self.mux.clone(),
            channel,
            notifications,
            owns_transport: false,
            parent_client: Some(self.client()),
            owned_session_id: Some(session_id),
            tasks: std::sync::Mutex::new(vec![channel_task]),
        }))
    }

    pub fn client(&self) -> CdpClient<Channel> {
        CdpClient::root(self.channel.clone())
    }

    pub fn subscribe(&self) -> mpsc::UnboundedReceiver<(String, Value)> {
        let (sender, receiver) = mpsc::unbounded_channel();
        self.notifications.sinks.lock().unwrap().push(sender);
        receiver
    }

    async fn call(&self, method: &str, params: Value) -> Result<Value, JsonRpcError> {
        self.channel.call(method, params).await
    }

    pub async fn wait_closed(&self) -> String {
        self.transport.wait_closed().await
    }

    pub async fn close(&self) {
        if let Some(session_id) = &self.owned_session_id {
            if let Some(parent) = &self.parent_client {
                let mut params = TargetDetachFromTargetParams::new();
                params.session_id = Some(session_id.clone());
                let _ = parent.target_detach_from_target(params).await;
            }
            self.mux.retire_session(session_id);
        } else if self.owns_transport {
            self.transport.close().await;
            self.mux.dispose();
        }
        for task in std::mem::take(&mut *self.tasks.lock().unwrap()) {
            task.abort();
        }
    }
}

/// The server half of the in-memory pipe between the virtual root and its CDP client.
struct ServerPipe {
    outbound: mpsc::UnboundedSender<CdpEnvelope>,
    inbound: Mutex<mpsc::UnboundedReceiver<CdpEnvelope>>,
}

#[async_trait]
impl MessageTransport<CdpEnvelope, CdpEnvelope> for ServerPipe {
    async fn send(&self, message: CdpEnvelope) -> Result<(), TransportError> {
        self.outbound
            .send(message)
            .map_err(|_| TransportError::Closed)
    }

    async fn recv(&self) -> Option<CdpEnvelope> {
        self.inbound.lock().await.recv().await
    }
}

/// A browser-shaped CDP transport over a [`TargetSource`]. Handed to
/// [`crate::cdp_runtime::CdpConnection::connect_transport`] like any other managed transport.
pub struct VirtualBrowserRoot {
    outbound: mpsc::UnboundedSender<CdpEnvelope>,
    inbound: Mutex<mpsc::UnboundedReceiver<CdpEnvelope>>,
    close_reason: Arc<Mutex<Option<String>>>,
    closed: watch::Sender<Option<String>>,
    state: Arc<VirtualRootState>,
}

impl VirtualBrowserRoot {
    pub async fn set_wait_for_debugger_on_start(&self, enabled: bool) {
        self.state
            .source
            .set_wait_for_debugger_on_start(enabled)
            .await;
    }

    pub fn start(
        source: Arc<dyn TargetSource>,
        mut events: mpsc::UnboundedReceiver<TargetSourceEvent>,
    ) -> Result<Arc<Self>, String> {
        let (to_server, server_inbound) = mpsc::unbounded_channel();
        let (to_client, client_inbound) = mpsc::unbounded_channel();
        let mux = CdpSessionMux::new(Arc::new(ServerPipe {
            outbound: to_client,
            inbound: Mutex::new(server_inbound),
        }));
        let state = Arc::new(VirtualRootState::new(source, mux.clone()));
        let root_channel = Channel::new(
            Box::new(mux.open_root().map_err(|error| error.to_string())?),
            Box::new(RootHandler(state.clone())),
        );
        let _ = state.root_channel.set(root_channel.clone());
        state.track(tokio::spawn({
            let mux = mux.clone();
            async move { mux.run().await }
        }));
        state.track(tokio::spawn(async move { root_channel.run().await }));
        state.track(tokio::spawn({
            let state = state.clone();
            async move {
                while let Some(event) = events.recv().await {
                    state.apply_source_event(event).await;
                }
            }
        }));

        let (closed, _) = watch::channel(None);
        let root = Arc::new(Self {
            outbound: to_server,
            inbound: Mutex::new(client_inbound),
            close_reason: Arc::new(Mutex::new(None)),
            closed,
            state: state.clone(),
        });
        let supervised = Arc::downgrade(&root);
        let supervisor_state = state.clone();
        state.track(tokio::spawn(async move {
            let reason = supervisor_state.source.wait_closed().await;
            if let Some(root) = supervised.upgrade() {
                root.mark_closed(reason).await;
            }
        }));
        Ok(root)
    }

    /// The OS process hosting `target_id`, when the source knows it.
    pub fn target_process_id(&self, target_id: &str) -> Option<u32> {
        self.state.known.lock().unwrap().get(target_id)?.process_id
    }

    /// Whether `target_id` is genuinely paused waiting for a debugger to resume it.
    pub fn target_waiting_for_debugger(&self, target_id: &str) -> bool {
        self.state
            .known
            .lock()
            .unwrap()
            .get(target_id)
            .is_some_and(|target| target.waiting_for_debugger)
    }

    /// Attaches to one target, in-process. CDP's `Target.attachToTarget` has no way to express
    /// "evict a debugger jsdbg does not own", so the debugger service - which sits on the other
    /// end of this transport anyway - asks for a forced attachment directly instead of inventing
    /// a non-standard protocol extension that external CDP clients would not understand.
    pub async fn attach_target(
        &self,
        target_id: &str,
        force: bool,
    ) -> Result<AttachedSession, JsonRpcError> {
        self.state.attach_target(target_id, force).await
    }

    async fn mark_closed(&self, reason: impl Into<String>) {
        let reason = reason.into();
        let mut close_reason = self.close_reason.lock().await;
        if close_reason.is_none() {
            *close_reason = Some(reason.clone());
            self.closed.send_replace(Some(reason));
        }
    }
}

#[async_trait]
impl MessageTransport<CdpEnvelope, CdpEnvelope> for VirtualBrowserRoot {
    async fn send(&self, message: CdpEnvelope) -> Result<(), TransportError> {
        if let Some(reason) = self.close_reason.lock().await.clone() {
            return Err(closed_transport_error(reason));
        }
        self.outbound
            .send(message)
            .map_err(|_| TransportError::Closed)
    }

    async fn recv(&self) -> Option<CdpEnvelope> {
        if self.close_reason.lock().await.is_some() {
            return None;
        }
        let mut closed = self.closed.subscribe();
        let mut inbound = self.inbound.lock().await;
        tokio::select! {
            message = inbound.recv() => message,
            _ = closed.changed() => None,
        }
    }
}

#[async_trait]
impl ManagedCdpTransport for VirtualBrowserRoot {
    fn close_reason(&self) -> Arc<Mutex<Option<String>>> {
        self.close_reason.clone()
    }

    async fn wait_closed(&self) -> String {
        let mut closed = self.closed.subscribe();
        loop {
            if let Some(reason) = closed.borrow_and_update().clone() {
                return reason;
            }
            if closed.changed().await.is_err() {
                return "virtual browser root closed".to_owned();
            }
        }
    }

    async fn close(&self) {
        self.mark_closed("virtual browser root closed").await;
        self.state.shutdown().await;
    }
}

/// One attached target: the flattened CDP session the client talks to, and the plumbing that keeps
/// it fed with the target's events.
struct RootSession {
    target_id: String,
    tasks: Vec<JoinHandle<()>>,
}

struct VirtualRootState {
    source: Arc<dyn TargetSource>,
    mux: CdpSessionMux,
    root_channel: OnceLock<Channel>,
    known: std::sync::Mutex<BTreeMap<String, HostTarget>>,
    sessions: std::sync::Mutex<BTreeMap<String, RootSession>>,
    discover: AtomicBool,
    auto_attach: AtomicBool,
    next_session_id: AtomicU64,
    tasks: std::sync::Mutex<Vec<JoinHandle<()>>>,
}

impl VirtualRootState {
    fn new(source: Arc<dyn TargetSource>, mux: CdpSessionMux) -> Self {
        Self {
            source,
            mux,
            root_channel: OnceLock::new(),
            known: std::sync::Mutex::new(BTreeMap::new()),
            sessions: std::sync::Mutex::new(BTreeMap::new()),
            discover: AtomicBool::new(false),
            auto_attach: AtomicBool::new(false),
            next_session_id: AtomicU64::new(1),
            tasks: std::sync::Mutex::new(Vec::new()),
        }
    }

    fn track(&self, task: JoinHandle<()>) {
        self.tasks.lock().unwrap().push(task);
    }

    async fn notify_root(&self, method: &str, params: Result<Value, JsonRpcError>) {
        if let (Some(channel), Ok(params)) = (self.root_channel.get(), params) {
            let _ = channel.notify(method, params).await;
        }
    }

    /// Discovery only runs while a client asked for it, either explicitly or implicitly by
    /// enabling auto-attach.
    async fn sync_discovery_demand(&self) {
        let required =
            self.discover.load(Ordering::Relaxed) || self.auto_attach.load(Ordering::Relaxed);
        self.source.set_discovery(required).await;
    }

    async fn refresh_known(&self) -> Vec<HostTarget> {
        let listed = self.source.list_targets().await;
        let mut known = self.known.lock().unwrap();
        let attached = known
            .iter()
            .filter(|(_, target)| target.snapshot.attached)
            .map(|(id, _)| id.clone())
            .collect::<Vec<_>>();
        *known = listed
            .into_iter()
            .map(|mut target| {
                target.snapshot.attached |= attached.contains(&target.snapshot.target_id);
                (target.snapshot.target_id.clone(), target)
            })
            .collect();
        known.values().cloned().collect()
    }

    async fn apply_source_event(&self, event: TargetSourceEvent) {
        match event {
            TargetSourceEvent::Upserted(mut target) => {
                let (is_new, changed) = {
                    let mut known = self.known.lock().unwrap();
                    match known.get(target.target_id()) {
                        Some(previous) => {
                            target.snapshot.attached |= previous.snapshot.attached;
                            let changed = previous != &target;
                            known.insert(target.snapshot.target_id.clone(), target.clone());
                            (false, changed)
                        }
                        None => {
                            known.insert(target.snapshot.target_id.clone(), target.clone());
                            (true, true)
                        }
                    }
                };
                if self.discover.load(Ordering::Relaxed) && changed {
                    if is_new {
                        self.notify_root(
                            "Target.targetCreated",
                            to_json(TargetTargetCreatedParams {
                                target_info: target_info_from_snapshot(&target.snapshot),
                            }),
                        )
                        .await;
                    } else {
                        self.notify_root(
                            "Target.targetInfoChanged",
                            to_json(TargetTargetInfoChangedParams {
                                target_info: target_info_from_snapshot(&target.snapshot),
                            }),
                        )
                        .await;
                    }
                }
                if is_new && self.auto_attach.load(Ordering::Relaxed) {
                    let _ = self.attach_target(target.target_id(), false).await;
                }
            }
            TargetSourceEvent::Removed(target_id) => {
                let existed = self.known.lock().unwrap().remove(&target_id).is_some();
                self.detach_sessions_for_target(&target_id).await;
                if existed && self.discover.load(Ordering::Relaxed) {
                    self.notify_root(
                        "Target.targetDestroyed",
                        to_json(TargetTargetDestroyedParams {
                            target_id: target_id.clone(),
                        }),
                    )
                    .await;
                }
            }
        }
    }

    async fn attach_target(
        &self,
        target_id: &str,
        force: bool,
    ) -> Result<AttachedSession, JsonRpcError> {
        let Some(target) = self.known.lock().unwrap().get(target_id).cloned() else {
            return Err(invalid_params(format!(
                "no such target '{target_id}' in this process tree"
            )));
        };
        let attachment = self
            .source
            .attach(target_id, force)
            .await
            .map_err(|error| JsonRpcError::new(error_codes::INTERNAL_ERROR, error))?;
        let session_id = format!(
            "virtual-session-{}",
            self.next_session_id.fetch_add(1, Ordering::Relaxed)
        );
        let session_transport = self
            .mux
            .open_session(session_id.clone())
            .map_err(|error| JsonRpcError::new(error_codes::INTERNAL_ERROR, error.to_string()))?;
        let session_channel = Channel::new(
            Box::new(session_transport),
            Box::new(SessionHandler {
                endpoint: attachment.endpoint.clone(),
            }),
        );
        let mut tasks = vec![tokio::spawn({
            let channel = session_channel.clone();
            async move { channel.run().await }
        })];
        let mut notifications = attachment.endpoint.subscribe();
        tasks.push(tokio::spawn({
            let channel = session_channel.clone();
            async move {
                while let Some((method, params)) = notifications.recv().await {
                    let _ = channel.notify(&method, params).await;
                }
            }
        }));
        self.sessions.lock().unwrap().insert(
            session_id.clone(),
            RootSession {
                target_id: target_id.to_owned(),
                tasks,
            },
        );
        if let Some(known) = self.known.lock().unwrap().get_mut(target_id) {
            known.snapshot.attached = true;
        }
        let mut target_info = target_info_from_snapshot(&target.snapshot);
        target_info.attached = true;
        self.notify_root(
            "Target.attachedToTarget",
            to_json(TargetAttachedToTargetParams {
                session_id: session_id.clone(),
                target_info,
                waiting_for_debugger: target.waiting_for_debugger,
            }),
        )
        .await;
        Ok(AttachedSession {
            session_id,
            stole_external_owner: attachment.stole_external_owner,
        })
    }

    async fn detach(&self, session_id: Option<&str>, target_id: Option<&str>) {
        let matching = {
            let sessions = self.sessions.lock().unwrap();
            match (session_id, target_id) {
                (Some(session_id), _) => sessions
                    .contains_key(session_id)
                    .then(|| session_id.to_owned()),
                (None, Some(target_id)) => sessions
                    .iter()
                    .find(|(_, session)| session.target_id == target_id)
                    .map(|(id, _)| id.clone()),
                (None, None) => None,
            }
        };
        let Some(session_id) = matching else {
            return;
        };
        let Some(session) = self.sessions.lock().unwrap().remove(&session_id) else {
            return;
        };
        for task in session.tasks {
            task.abort();
        }
        self.mux.retire_session(&session_id);
        self.source.detach(&session.target_id).await;
        if let Some(known) = self.known.lock().unwrap().get_mut(&session.target_id) {
            known.snapshot.attached = false;
        }
        self.notify_root(
            "Target.detachedFromTarget",
            to_json(TargetDetachedFromTargetParams {
                session_id,
                target_id: Some(session.target_id),
            }),
        )
        .await;
    }

    async fn detach_sessions_for_target(&self, target_id: &str) {
        let matching = self
            .sessions
            .lock()
            .unwrap()
            .iter()
            .filter(|(_, session)| session.target_id == target_id)
            .map(|(id, _)| id.clone())
            .collect::<Vec<_>>();
        for session_id in matching {
            self.detach(Some(&session_id), None).await;
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
                product: self.source.product(),
                revision: String::new(),
                user_agent: format!("jsdbg-virtual-browser-root/{}", env!("CARGO_PKG_VERSION")),
                js_version: String::new(),
            }),
            "Target.getTargets" => {
                let targets = self.refresh_known().await;
                to_json(TargetGetTargetsResult {
                    target_infos: targets
                        .iter()
                        .map(|target| target_info_from_snapshot(&target.snapshot))
                        .collect(),
                })
            }
            "Target.setDiscoverTargets" => {
                let request: TargetSetDiscoverTargetsParams = from_json(params)?;
                let was_enabled = self.discover.swap(request.discover, Ordering::Relaxed);
                self.sync_discovery_demand().await;
                if request.discover && !was_enabled {
                    for target in self.refresh_known().await {
                        self.notify_root(
                            "Target.targetCreated",
                            to_json(TargetTargetCreatedParams {
                                target_info: target_info_from_snapshot(&target.snapshot),
                            }),
                        )
                        .await;
                    }
                }
                to_json(TargetSetDiscoverTargetsResult::new())
            }
            "Target.setAutoAttach" => {
                let request: TargetSetAutoAttachParams = from_json(params)?;
                let was_enabled = self
                    .auto_attach
                    .swap(request.auto_attach, Ordering::Relaxed);
                self.sync_discovery_demand().await;
                self.source
                    .set_wait_for_debugger_on_start(
                        request.auto_attach && request.wait_for_debugger_on_start,
                    )
                    .await;
                if request.auto_attach && !was_enabled {
                    for target in self.refresh_known().await {
                        if !target.snapshot.attached {
                            let _ = self.attach_target(target.target_id(), false).await;
                        }
                    }
                }
                to_json(TargetSetAutoAttachResult::new())
            }
            "Target.attachToTarget" => {
                let request: TargetAttachToTargetParams = from_json(params)?;
                let attached = self.attach_target(&request.target_id, false).await?;
                to_json(TargetAttachToTargetResult {
                    session_id: attached.session_id,
                })
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
                    "jsdbg's virtual browser root does not implement '{method}'; attach to a target and send target-scoped commands instead"
                ),
            )),
        }
    }

    async fn shutdown(&self) {
        for (_, session) in std::mem::take(&mut *self.sessions.lock().unwrap()) {
            for task in session.tasks {
                task.abort();
            }
        }
        self.source.close().await;
        self.mux.dispose();
        for task in std::mem::take(&mut *self.tasks.lock().unwrap()) {
            task.abort();
        }
    }
}

struct RootHandler(Arc<VirtualRootState>);

#[async_trait]
impl RequestHandler for RootHandler {
    async fn handle_request(&self, method: String, params: Value) -> Result<Value, JsonRpcError> {
        self.0.handle_root_request(method, params).await
    }
}

/// Forwards every session-scoped request opaquely to the target's own CDP endpoint.
struct SessionHandler {
    endpoint: Arc<TargetEndpoint>,
}

#[async_trait]
impl RequestHandler for SessionHandler {
    async fn handle_request(&self, method: String, params: Value) -> Result<Value, JsonRpcError> {
        self.endpoint.call(&method, params).await
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use hubrpc::connection::channel::RejectingHandler;
    use hubrpc::prelude::{JsonRpcMessage, JsonRpcResponse};
    use hubrpc::protocol::jsonrpc::ResponsePayload;
    use serde_json::json;
    use std::sync::Mutex as StdMutex;

    /// A target endpoint that answers every request with the method it received, so session
    /// routing can be asserted without a real debuggee.
    struct EchoTransport {
        inbound: Mutex<mpsc::UnboundedReceiver<CdpEnvelope>>,
        outbound: mpsc::UnboundedSender<CdpEnvelope>,
        close_reason: Arc<Mutex<Option<String>>>,
    }

    impl EchoTransport {
        fn new() -> Arc<Self> {
            let (outbound, inbound) = mpsc::unbounded_channel();
            Arc::new(Self {
                inbound: Mutex::new(inbound),
                outbound,
                close_reason: Arc::new(Mutex::new(None)),
            })
        }
    }

    #[async_trait]
    impl MessageTransport<CdpEnvelope, CdpEnvelope> for EchoTransport {
        async fn send(&self, message: CdpEnvelope) -> Result<(), TransportError> {
            let JsonRpcMessage::Request(request) = message.message else {
                return Ok(());
            };
            self.outbound
                .send(CdpEnvelope {
                    session_id: message.session_id,
                    message: JsonRpcMessage::Response(JsonRpcResponse {
                        id: Some(request.id),
                        payload: ResponsePayload::Result(json!({ "echo": request.method })),
                    }),
                })
                .map_err(|_| TransportError::Closed)
        }

        async fn recv(&self) -> Option<CdpEnvelope> {
            self.inbound.lock().await.recv().await
        }
    }

    #[async_trait]
    impl ManagedCdpTransport for EchoTransport {
        fn close_reason(&self) -> Arc<Mutex<Option<String>>> {
            self.close_reason.clone()
        }

        async fn wait_closed(&self) -> String {
            std::future::pending().await
        }

        async fn close(&self) {}
    }

    #[derive(Default)]
    struct StubCalls {
        discovery: Vec<bool>,
        wait_for_debugger_on_start: Vec<bool>,
        list_calls: usize,
        attached: Vec<String>,
        detached: Vec<String>,
    }

    struct StubSource {
        targets: StdMutex<Vec<HostTarget>>,
        calls: Arc<StdMutex<StubCalls>>,
    }

    impl StubSource {
        fn new(targets: Vec<HostTarget>) -> (Arc<Self>, Arc<StdMutex<StubCalls>>) {
            let calls = Arc::new(StdMutex::new(StubCalls::default()));
            (
                Arc::new(Self {
                    targets: StdMutex::new(targets),
                    calls: calls.clone(),
                }),
                calls,
            )
        }
    }

    #[async_trait]
    impl TargetSource for StubSource {
        fn product(&self) -> String {
            "Process 4242".to_owned()
        }

        async fn list_targets(&self) -> Vec<HostTarget> {
            self.calls.lock().unwrap().list_calls += 1;
            self.targets.lock().unwrap().clone()
        }

        async fn set_discovery(&self, enabled: bool) {
            self.calls.lock().unwrap().discovery.push(enabled);
        }

        async fn set_wait_for_debugger_on_start(&self, enabled: bool) {
            self.calls
                .lock()
                .unwrap()
                .wait_for_debugger_on_start
                .push(enabled);
        }

        async fn attach(&self, target_id: &str, force: bool) -> Result<TargetAttachment, String> {
            self.calls
                .lock()
                .unwrap()
                .attached
                .push(target_id.to_owned());
            Ok(TargetAttachment {
                endpoint: TargetEndpoint::open(EchoTransport::new())?,
                stole_external_owner: force,
            })
        }

        async fn detach(&self, target_id: &str) {
            self.calls
                .lock()
                .unwrap()
                .detached
                .push(target_id.to_owned());
        }

        async fn wait_closed(&self) -> String {
            std::future::pending().await
        }

        async fn close(&self) {}
    }

    fn host_target(target_id: &str, process_id: u32) -> HostTarget {
        HostTarget {
            snapshot: TargetSnapshot {
                target_id: target_id.to_owned(),
                target_type: "node".to_owned(),
                title: target_id.to_owned(),
                url: format!("process://{process_id}"),
                attached: false,
                parent_id: None,
                opener_id: None,
                browser_context_id: None,
                subtype: None,
            },
            process_id: Some(process_id),
            waiting_for_debugger: false,
        }
    }

    #[derive(Default)]
    struct EventCollector {
        events: StdMutex<Vec<(String, Value)>>,
    }

    #[async_trait]
    impl RequestHandler for EventCollector {
        async fn handle_request(
            &self,
            method: String,
            _params: Value,
        ) -> Result<Value, JsonRpcError> {
            panic!("unexpected request from the virtual root: {method}");
        }

        async fn handle_notification(&self, method: String, params: Value) {
            self.events.lock().unwrap().push((method, params));
        }
    }

    struct SharedCollector(Arc<EventCollector>);

    #[async_trait]
    impl RequestHandler for SharedCollector {
        async fn handle_request(
            &self,
            method: String,
            params: Value,
        ) -> Result<Value, JsonRpcError> {
            self.0.handle_request(method, params).await
        }

        async fn handle_notification(&self, method: String, params: Value) {
            self.0.handle_notification(method, params).await;
        }
    }

    struct Harness {
        root: Arc<VirtualBrowserRoot>,
        client: Channel,
        mux: CdpSessionMux,
        events: Arc<EventCollector>,
        calls: Arc<StdMutex<StubCalls>>,
        source: Arc<StubSource>,
        sender: mpsc::UnboundedSender<TargetSourceEvent>,
    }

    impl Harness {
        fn start(targets: Vec<HostTarget>) -> Self {
            let (source, calls) = StubSource::new(targets);
            let (sender, receiver) = mpsc::unbounded_channel();
            let root = VirtualBrowserRoot::start(source.clone(), receiver).unwrap();
            let mux = CdpSessionMux::new(root.clone());
            let events = Arc::new(EventCollector::default());
            let client = Channel::new(
                Box::new(mux.open_root().unwrap()),
                Box::new(SharedCollector(events.clone())),
            );
            tokio::spawn({
                let mux = mux.clone();
                async move { mux.run().await }
            });
            tokio::spawn({
                let client = client.clone();
                async move { client.run().await }
            });
            Self {
                root,
                client,
                mux,
                events,
                calls,
                source,
                sender,
            }
        }

        async fn call(&self, method: &str, params: Value) -> Value {
            self.client.call(method, params).await.unwrap()
        }

        async fn settle(&self) {
            for _ in 0..32 {
                tokio::task::yield_now().await;
            }
        }

        fn methods(&self) -> Vec<String> {
            self.events
                .events
                .lock()
                .unwrap()
                .iter()
                .map(|(method, _)| method.clone())
                .collect()
        }
    }

    #[tokio::test]
    async fn browser_get_version_reports_the_source_product() {
        let harness = Harness::start(vec![host_target("$node-root", 4242)]);
        let version = harness.call("Browser.getVersion", json!({})).await;
        assert_eq!(version["product"], "Process 4242");
    }

    #[tokio::test]
    async fn get_targets_is_one_shot_and_never_enables_discovery() {
        let harness = Harness::start(vec![
            host_target("$node-root", 4242),
            host_target("pid-77", 77),
        ]);
        let targets = harness.call("Target.getTargets", json!({})).await;
        let infos = targets["targetInfos"].as_array().unwrap();
        assert_eq!(infos.len(), 2);
        assert_eq!(infos[0]["targetId"], "$node-root");
        assert!(
            harness.calls.lock().unwrap().discovery.is_empty(),
            "a one-shot query must not turn discovery on"
        );
    }

    #[tokio::test]
    async fn discovery_is_demand_driven() {
        let harness = Harness::start(vec![host_target("$node-root", 4242)]);
        harness
            .call("Target.setDiscoverTargets", json!({ "discover": true }))
            .await;
        assert_eq!(harness.calls.lock().unwrap().discovery, vec![true]);

        // Auto-attach demands discovery too, so turning explicit discovery off while auto-attach
        // is on must keep the source discovering.
        harness
            .call(
                "Target.setAutoAttach",
                json!({ "autoAttach": true, "waitForDebuggerOnStart": true, "flatten": true }),
            )
            .await;
        harness
            .call("Target.setDiscoverTargets", json!({ "discover": false }))
            .await;
        assert_eq!(
            harness.calls.lock().unwrap().discovery,
            vec![true, true, true]
        );
        assert_eq!(
            harness.calls.lock().unwrap().wait_for_debugger_on_start,
            vec![true]
        );

        harness
            .call(
                "Target.setAutoAttach",
                json!({ "autoAttach": false, "waitForDebuggerOnStart": false, "flatten": true }),
            )
            .await;
        assert_eq!(
            harness.calls.lock().unwrap().discovery,
            vec![true, true, true, false],
            "discovery must stop once nothing demands it"
        );
    }

    #[tokio::test]
    async fn enabling_discovery_replays_known_targets_and_pushes_updates() {
        let harness = Harness::start(vec![host_target("$node-root", 4242)]);
        harness
            .call("Target.setDiscoverTargets", json!({ "discover": true }))
            .await;
        harness.settle().await;
        assert_eq!(harness.methods(), vec!["Target.targetCreated"]);

        harness
            .sender
            .send(TargetSourceEvent::Upserted(host_target("pid-77", 77)))
            .unwrap();
        harness.settle().await;
        harness
            .sender
            .send(TargetSourceEvent::Removed("pid-77".to_owned()))
            .unwrap();
        harness.settle().await;
        assert_eq!(
            harness.methods(),
            vec![
                "Target.targetCreated",
                "Target.targetCreated",
                "Target.targetDestroyed"
            ]
        );
    }

    #[tokio::test]
    async fn events_stay_silent_while_discovery_is_off() {
        let harness = Harness::start(vec![host_target("$node-root", 4242)]);
        harness
            .sender
            .send(TargetSourceEvent::Upserted(host_target("pid-77", 77)))
            .unwrap();
        harness.settle().await;
        assert!(harness.methods().is_empty());
        assert_eq!(harness.root.target_process_id("pid-77"), Some(77));
    }

    #[tokio::test]
    async fn attaching_routes_session_traffic_to_the_target_endpoint() {
        let harness = Harness::start(vec![host_target("$node-root", 4242)]);
        harness.call("Target.getTargets", json!({})).await;
        let attached = harness
            .call("Target.attachToTarget", json!({ "targetId": "$node-root" }))
            .await;
        let session_id = attached["sessionId"].as_str().unwrap().to_owned();

        let session = Channel::new(
            Box::new(harness.mux.open_session(session_id.clone()).unwrap()),
            Box::new(RejectingHandler),
        );
        tokio::spawn({
            let session = session.clone();
            async move { session.run().await }
        });
        let response = session.call("Debugger.enable", json!({})).await.unwrap();
        assert_eq!(response["echo"], "Debugger.enable");
        assert_eq!(harness.calls.lock().unwrap().attached, vec!["$node-root"]);

        harness
            .call(
                "Target.detachFromTarget",
                json!({ "sessionId": session_id.clone() }),
            )
            .await;
        assert_eq!(harness.calls.lock().unwrap().detached, vec!["$node-root"]);
    }

    #[tokio::test]
    async fn forced_attachment_reports_a_stolen_owner() {
        let harness = Harness::start(vec![host_target("$node-root", 4242)]);
        harness.call("Target.getTargets", json!({})).await;
        let attached = harness
            .root
            .attach_target("$node-root", true)
            .await
            .unwrap();
        assert!(attached.stole_external_owner);
        assert!(attached.session_id.starts_with("virtual-session-"));
    }

    #[tokio::test]
    async fn attaching_to_an_unknown_target_fails() {
        let harness = Harness::start(vec![host_target("$node-root", 4242)]);
        let error = harness
            .client
            .call("Target.attachToTarget", json!({ "targetId": "ghost" }))
            .await
            .unwrap_err();
        assert!(error.message.contains("no such target"), "{error:?}");
        assert_eq!(harness.source.product(), "Process 4242");
    }

    #[tokio::test]
    async fn waiting_for_debugger_is_reported_from_the_source() {
        let mut blocked = host_target("renderer-3", 99);
        blocked.waiting_for_debugger = true;
        let harness = Harness::start(vec![host_target("$node-root", 4242)]);
        harness
            .sender
            .send(TargetSourceEvent::Upserted(blocked))
            .unwrap();
        harness.settle().await;
        assert!(harness.root.target_waiting_for_debugger("renderer-3"));
        assert!(!harness.root.target_waiting_for_debugger("$node-root"));
    }
}
