//! A provider-neutral virtual browser root.
//!
//! Chrome exposes a process tree of targets through the CDP `Target` domain; dbgjs uses the same
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
use linkrpc::connection::channel::{Channel, RequestHandler};
use linkrpc::prelude::{
    CallCtx, InterfaceHandler, JsonRpcError, MessageTransport, RpcCallError, TransportError, error_codes,
};
use serde_json::Value;
use tokio::sync::{Mutex, mpsc, watch};
use tokio::task::JoinHandle;

use crate::cdp::{
    BrowserGetVersionResult, CdpClient, CdpEventsClient, TargetAttachToTargetResult,
    TargetDetachFromTargetResult, TargetGetTargetsResult, TargetSessionId,
    TargetSetAutoAttachResult, TargetSetDiscoverTargetsResult, TargetTargetFilter, TargetTargetId,
};
use crate::cdp_transport::{ManagedCdpTransport, closed_transport_error};
use crate::service_api::TargetSnapshot;
use crate::session_transport::{CdpEnvelope, CdpSessionMux, RawCdpSession};
use crate::target_domain::{invalid_params, normalize_typed_cdp_params, target_info_from_snapshot};

/// One target a [`TargetSource`] knows about. `snapshot` is the provider-neutral CDP `TargetInfo`;
/// the remaining fields carry per-target state the debugger service needs but
/// CDP's `TargetInfo` cannot express.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct HostTarget {
    pub snapshot: TargetSnapshot,
    /// The OS process hosting the target, when the source knows it. Used to recognize the same
    /// physical target across connections.
    pub process_id: Option<u32>,
    /// The live Electron BrowserWindow whose primary webContents is this target.
    pub primary_window_id: Option<u32>,
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
    /// Whether a debugger that dbgjs does not own had to be evicted to get this attachment.
    pub stole_external_owner: bool,
}

/// The session [`VirtualBrowserRoot::attach_target`] opened.
pub struct AttachedSession {
    pub session_id: String,
    pub stole_external_owner: bool,
}

pub struct TargetObservation {
    pub revision: u64,
    pub targets: Vec<HostTarget>,
    pub revisions: watch::Receiver<u64>,
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

    /// Turns root-level metadata discovery on or off. This must not open target endpoints.
    async fn set_discovery(&self, enabled: bool);

    /// Observes only the immediate children of one already-open target. Implementations must not
    /// open the target or recursively observe returned children.
    async fn set_child_discovery(&self, target_id: &str, enabled: bool);

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
    closed: watch::Sender<Option<String>>,
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
        let (closed, _) = watch::channel(None);
        Ok(Arc::new(Self {
            transport,
            mux,
            channel,
            notifications,
            owns_transport: true,
            parent_client: None,
            owned_session_id: None,
            closed,
            tasks: std::sync::Mutex::new(vec![mux_task, channel_task]),
        }))
    }

    pub async fn attach_child(&self, target_id: &str) -> Result<Arc<Self>, String> {
        let mut parent_events = self.subscribe();
        let attached = self
            .client()
            .target()
            .attach_to_target(target_id.to_owned(), Some(true), None)
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
        let (closed, _) = watch::channel(None);
        let native_id = session_id.clone();
        let closed_on_detach = closed.clone();
        let mux_on_detach = self.mux.clone();
        let detach_task = tokio::spawn(async move {
            while let Some((method, params)) = parent_events.recv().await {
                if method == "Target.detachedFromTarget"
                    && params.get("sessionId").and_then(Value::as_str) == Some(&native_id)
                {
                    closed_on_detach.send_replace(Some(format!(
                        "native CDP session '{native_id}' detached; reattach the target"
                    )));
                    mux_on_detach.retire_session(&native_id);
                    return;
                }
            }
        });
        Ok(Arc::new(Self {
            transport: self.transport.clone(),
            mux: self.mux.clone(),
            channel,
            notifications,
            owns_transport: false,
            parent_client: Some(self.client()),
            owned_session_id: Some(session_id),
            closed,
            tasks: std::sync::Mutex::new(vec![channel_task, detach_task]),
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

    pub fn open_raw_session(&self, session_id: String) -> Result<Arc<RawCdpSession>, String> {
        RawCdpSession::open(&self.mux, session_id).map_err(|error| error.to_string())
    }

    async fn call(&self, method: &str, params: Value) -> Result<Value, JsonRpcError> {
        if let Some(reason) = self.close_reason().await {
            return Err(JsonRpcError::new(error_codes::PEER_DISCONNECTED, reason));
        }
        self.channel.call(method, params).await
    }

    pub async fn close_reason(&self) -> Option<String> {
        if let Some(reason) = self.closed.borrow().clone() {
            return Some(reason);
        }
        self.transport.close_reason().lock().await.clone()
    }

    pub async fn wait_closed(&self) -> String {
        let mut closed = self.closed.subscribe();
        tokio::select! {
            reason = self.transport.wait_closed() => reason,
            _ = async {
                while closed.borrow_and_update().is_none() {
                    if closed.changed().await.is_err() { break; }
                }
            } => closed.borrow().clone().unwrap_or_else(|| "CDP child session closed".to_owned()),
        }
    }

    pub async fn close(&self) {
        let was_closed = self.close_reason().await.is_some();
        self.closed.send_replace(Some("CDP target endpoint closed; reattach the target".to_owned()));
        if let Some(session_id) = &self.owned_session_id {
            if !was_closed && let Some(parent) = &self.parent_client {
                let _ = tokio::time::timeout(
                    std::time::Duration::from_secs(2),
                    parent.target().detach_from_target(Some(session_id.clone()), None),
                ).await;
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

    pub async fn refresh_targets(&self) -> TargetObservation {
        self.state.refresh_known().await;
        self.observe_targets()
    }

    pub fn observe_targets(&self) -> TargetObservation {
        self.state.observe_targets()
    }

    pub async fn set_target_discovery(&self, enabled: bool) {
        self.state.discover.store(enabled, Ordering::Relaxed);
        self.state.sync_discovery_demand().await;
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
        let (closed_attachment_tx, mut closed_attachment_rx) = mpsc::unbounded_channel();
        let state = Arc::new(VirtualRootState::new(source, mux.clone(), closed_attachment_tx));
        let root_channel = Channel::new(
            Box::new(mux.open_root().map_err(|error| error.to_string())?),
            Box::new(RootHandler::new(state.clone())),
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
        state.track(tokio::spawn({
            let state = state.clone();
            async move {
                while let Some(closed) = closed_attachment_rx.recv().await {
                    let target_id = state.sessions.lock().unwrap().get(&closed.session_id)
                        .filter(|session| Arc::ptr_eq(&session.endpoint, &closed.endpoint))
                        .map(|session| session.target_id.clone());
                    if let Some(target_id) = target_id {
                        state.closed_targets.lock().unwrap().insert(target_id, closed.reason);
                        state.detach(Some(&closed.session_id), None).await;
                    }
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

    pub fn target_primary_window_id(&self, target_id: &str) -> Option<u32> {
        self.state
            .known
            .lock()
            .unwrap()
            .get(target_id)?
            .primary_window_id
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

    pub fn target_endpoint(&self, target_id: &str) -> Option<Arc<TargetEndpoint>> {
        self.state.sessions.lock().unwrap().values()
            .find(|session| session.target_id == target_id)
            .map(|session| session.endpoint.clone())
    }

    pub fn owns_target_endpoint(&self, target_id: &str, endpoint: &Arc<TargetEndpoint>) -> bool {
        self.state.sessions.lock().unwrap().values().any(|session| {
            session.target_id == target_id && Arc::ptr_eq(&session.endpoint, endpoint)
        })
    }

    pub fn closed_target_reason(&self, target_id: &str) -> Option<String> {
        self.state.closed_targets.lock().unwrap().get(target_id).cloned()
    }

    /// Attaches to one target, in-process. CDP's `Target.attachToTarget` has no way to express
    /// "evict a debugger dbgjs does not own", so the debugger service - which sits on the other
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
    endpoint: Arc<TargetEndpoint>,
    tasks: Vec<JoinHandle<()>>,
}

struct ClosedAttachment {
    session_id: String,
    endpoint: Arc<TargetEndpoint>,
    reason: String,
}

struct VirtualRootState {
    source: Arc<dyn TargetSource>,
    mux: CdpSessionMux,
    root_channel: OnceLock<Channel>,
    known: std::sync::Mutex<BTreeMap<String, HostTarget>>,
    closed_targets: std::sync::Mutex<BTreeMap<String, String>>,
    revisions: watch::Sender<u64>,
    sessions: std::sync::Mutex<BTreeMap<String, RootSession>>,
    closed_attachment_tx: mpsc::UnboundedSender<ClosedAttachment>,
    discover: AtomicBool,
    auto_attach: AtomicBool,
    next_session_id: AtomicU64,
    tasks: std::sync::Mutex<Vec<JoinHandle<()>>>,
}

impl VirtualRootState {
    fn new(
        source: Arc<dyn TargetSource>,
        mux: CdpSessionMux,
        closed_attachment_tx: mpsc::UnboundedSender<ClosedAttachment>,
    ) -> Self {
        let (revisions, _) = watch::channel(0);
        Self {
            source,
            mux,
            root_channel: OnceLock::new(),
            known: std::sync::Mutex::new(BTreeMap::new()),
            closed_targets: std::sync::Mutex::new(BTreeMap::new()),
            revisions,
            sessions: std::sync::Mutex::new(BTreeMap::new()),
            closed_attachment_tx,
            discover: AtomicBool::new(false),
            auto_attach: AtomicBool::new(false),
            next_session_id: AtomicU64::new(1),
            tasks: std::sync::Mutex::new(Vec::new()),
        }
    }

    fn track(&self, task: JoinHandle<()>) {
        self.tasks.lock().unwrap().push(task);
    }

    fn root_events_client(&self) -> CdpEventsClient<Channel> {
        CdpEventsClient::root(
            self.root_channel
                .get()
                .expect("root channel is set before target events are emitted")
                .clone(),
        )
    }

    /// Discovery only runs while a client asked for it, either explicitly or implicitly by
    /// enabling auto-attach.
    async fn sync_discovery_demand(&self) {
        let required =
            self.discover.load(Ordering::Relaxed) || self.auto_attach.load(Ordering::Relaxed);
        self.source.set_discovery(required).await;
        let attached_targets = self
            .sessions
            .lock()
            .unwrap()
            .values()
            .map(|session| session.target_id.clone())
            .collect::<std::collections::BTreeSet<_>>();
        for target_id in attached_targets {
            self.source.set_child_discovery(&target_id, required).await;
        }
    }

    async fn refresh_known(&self) -> Vec<HostTarget> {
        let listed = self.source.list_targets().await;
        let mut known = self.known.lock().unwrap();
        let attached = self.sessions.lock().unwrap().values()
            .map(|session| session.target_id.clone())
            .collect::<std::collections::BTreeSet<_>>();
        let replacement = listed
            .into_iter()
            .map(|mut target| {
                target.snapshot.attached |= attached.contains(&target.snapshot.target_id);
                (target.snapshot.target_id.clone(), target)
            })
            .collect();
        if *known != replacement {
            *known = replacement;
            self.advance_revision();
        }
        known.values().cloned().collect()
    }

    fn observe_targets(&self) -> TargetObservation {
        let known = self.known.lock().unwrap();
        let revisions = self.revisions.subscribe();
        let revision = *revisions.borrow();
        TargetObservation {
            revision,
            targets: known.values().cloned().collect(),
            revisions,
        }
    }

    fn advance_revision(&self) {
        let revision = *self.revisions.borrow() + 1;
        self.revisions.send_replace(revision);
    }

    async fn apply_source_event(&self, event: TargetSourceEvent) {
        match event {
            TargetSourceEvent::Upserted(mut target) => {
                let (is_new, changed) = {
                    let mut known = self.known.lock().unwrap();
                    match known.get(target.target_id()) {
                        Some(previous) => {
                            target.snapshot.attached |= self.sessions.lock().unwrap().values()
                                .any(|session| session.target_id == target.snapshot.target_id);
                            let changed = previous != &target;
                            known.insert(target.snapshot.target_id.clone(), target.clone());
                            if changed {
                                self.advance_revision();
                            }
                            (false, changed)
                        }
                        None => {
                            known.insert(target.snapshot.target_id.clone(), target.clone());
                            self.advance_revision();
                            (true, true)
                        }
                    }
                };
                if self.discover.load(Ordering::Relaxed) && changed {
                    if is_new {
                        let _ = self
                            .root_events_client()
                            .target()
                            .target_created(target_info_from_snapshot(&target.snapshot))
                            .await;
                    } else {
                        let _ = self
                            .root_events_client()
                            .target()
                            .target_info_changed(target_info_from_snapshot(&target.snapshot))
                            .await;
                    }
                }
                if is_new && self.auto_attach.load(Ordering::Relaxed) {
                    let _ = self.attach_target(target.target_id(), false).await;
                }
            }
            TargetSourceEvent::Removed(target_id) => {
                self.closed_targets.lock().unwrap().remove(&target_id);
                let existed = {
                    let mut known = self.known.lock().unwrap();
                    let existed = known.remove(&target_id).is_some();
                    if existed {
                        self.advance_revision();
                    }
                    existed
                };
                self.detach_sessions_for_target(&target_id).await;
                if existed && self.discover.load(Ordering::Relaxed) {
                    let _ = self
                        .root_events_client()
                        .target()
                        .target_destroyed(target_id.clone())
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
        let closing_endpoint = attachment.endpoint.clone();
        let closed_attachment_tx = self.closed_attachment_tx.clone();
        let closed_session_id = session_id.clone();
        self.sessions.lock().unwrap().insert(
            session_id.clone(),
            RootSession {
                target_id: target_id.to_owned(),
                endpoint: attachment.endpoint.clone(),
                tasks,
            },
        );
        self.closed_targets.lock().unwrap().remove(target_id);
        let close_task = tokio::spawn(async move {
            let reason = closing_endpoint.wait_closed().await;
            let _ = closed_attachment_tx.send(ClosedAttachment {
                session_id: closed_session_id,
                endpoint: closing_endpoint,
                reason,
            });
        });
        if let Some(session) = self.sessions.lock().unwrap().get_mut(&session_id) {
            session.tasks.push(close_task);
        }
        if self.discover.load(Ordering::Relaxed) || self.auto_attach.load(Ordering::Relaxed) {
            self.source.set_child_discovery(target_id, true).await;
        }
        if let Some(known) = self.known.lock().unwrap().get_mut(target_id) {
            known.snapshot.attached = true;
        }
        let mut target_info = target_info_from_snapshot(&target.snapshot);
        target_info.attached = true;
        let _ = self
            .root_events_client()
            .target()
            .attached_to_target(session_id.clone(), target_info, target.waiting_for_debugger)
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
        let target_still_attached = self
            .sessions
            .lock()
            .unwrap()
            .values()
            .any(|candidate| candidate.target_id == session.target_id);
        if !target_still_attached {
            self.source
                .set_child_discovery(&session.target_id, false)
                .await;
            self.source.detach(&session.target_id).await;
        }
        if !target_still_attached
            && let Some(known) = self.known.lock().unwrap().get_mut(&session.target_id)
        {
            known.snapshot.attached = false;
        }
        let _ = self
            .root_events_client()
            .target()
            .detached_from_target(session_id, Some(session.target_id))
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

#[async_trait]
impl crate::cdp::browser::BrowserService for VirtualRootState {
    async fn get_version(&self, _ctx: &CallCtx) -> Result<BrowserGetVersionResult, RpcCallError> {
        Ok(BrowserGetVersionResult {
            protocol_version: "1.3".to_owned(),
            product: self.source.product(),
            revision: String::new(),
            user_agent: format!("dbgjs-virtual-browser-root/{}", env!("CARGO_PKG_VERSION")),
            js_version: String::new(),
        })
    }
}

#[async_trait]
impl crate::cdp::target::TargetService for VirtualRootState {
    async fn get_targets(
        &self,
        _ctx: &CallCtx,
        _filter: Option<TargetTargetFilter>,
    ) -> Result<TargetGetTargetsResult, RpcCallError> {
        let targets = self.refresh_known().await;
        Ok(TargetGetTargetsResult {
            target_infos: targets
                .iter()
                .map(|target| target_info_from_snapshot(&target.snapshot))
                .collect(),
        })
    }

    async fn set_discover_targets(
        &self,
        _ctx: &CallCtx,
        discover: bool,
        _filter: Option<TargetTargetFilter>,
    ) -> Result<TargetSetDiscoverTargetsResult, RpcCallError> {
        let was_enabled = self.discover.swap(discover, Ordering::Relaxed);
        self.sync_discovery_demand().await;
        if discover && !was_enabled {
            for target in self.refresh_known().await {
                let _ = self
                    .root_events_client()
                    .target()
                    .target_created(target_info_from_snapshot(&target.snapshot))
                    .await;
            }
        }
        Ok(TargetSetDiscoverTargetsResult::new())
    }

    async fn set_auto_attach(
        &self,
        _ctx: &CallCtx,
        auto_attach: bool,
        wait_for_debugger_on_start: bool,
        _flatten: Option<bool>,
        _filter: Option<TargetTargetFilter>,
    ) -> Result<TargetSetAutoAttachResult, RpcCallError> {
        let was_enabled = self.auto_attach.swap(auto_attach, Ordering::Relaxed);
        self.sync_discovery_demand().await;
        self.source
            .set_wait_for_debugger_on_start(auto_attach && wait_for_debugger_on_start)
            .await;
        if auto_attach && !was_enabled {
            for target in self.refresh_known().await {
                if !target.snapshot.attached {
                    let _ = self.attach_target(target.target_id(), false).await;
                }
            }
        }
        Ok(TargetSetAutoAttachResult::new())
    }

    async fn attach_to_target(
        &self,
        _ctx: &CallCtx,
        target_id: TargetTargetId,
        _flatten: Option<bool>,
        _dbgjs_auto_attach: Option<bool>,
    ) -> Result<TargetAttachToTargetResult, RpcCallError> {
        let attached = self
            .attach_target(&target_id, false)
            .await
            .map_err(RpcCallError::Local)?;
        Ok(TargetAttachToTargetResult {
            session_id: attached.session_id,
        })
    }

    async fn detach_from_target(
        &self,
        _ctx: &CallCtx,
        session_id: Option<TargetSessionId>,
        target_id: Option<TargetTargetId>,
    ) -> Result<TargetDetachFromTargetResult, RpcCallError> {
        self.detach(session_id.as_deref(), target_id.as_deref())
            .await;
        Ok(TargetDetachFromTargetResult::new())
    }
}

struct RootHandler(linkrpc::binding::InterfaceRouter);

impl RootHandler {
    fn new(state: Arc<VirtualRootState>) -> Self {
        let router = linkrpc::binding::InterfaceRouter::new();
        crate::cdp::browser::DOMAIN
            .register(
                &router,
                Arc::new(crate::cdp::browser::BrowserServer::new(state.clone())),
            )
            .expect("generated Browser binding is valid");
        crate::cdp::target::DOMAIN
            .register(
                &router,
                Arc::new(crate::cdp::target::TargetServer::new(state)),
            )
            .expect("generated Target binding is distinct");
        Self(router)
    }
}

#[async_trait]
impl RequestHandler for RootHandler {
    async fn handle_request(&self, method: String, params: Value) -> Result<Value, JsonRpcError> {
        InterfaceHandler::handle_request(
            &self.0,
            &method,
            normalize_typed_cdp_params(params),
            CallCtx::default(),
        )
        .await
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
    use linkrpc::connection::channel::RejectingHandler;
    use linkrpc::prelude::{JsonRpcMessage, JsonRpcNotification, JsonRpcResponse};
    use linkrpc::protocol::jsonrpc::ResponsePayload;
    use serde_json::json;
    use std::sync::atomic::AtomicBool;
    use std::sync::Mutex as StdMutex;

    /// A target endpoint that answers every request with the method it received, so session
    /// routing can be asserted without a real debuggee.
    struct EchoTransport {
        inbound: Mutex<mpsc::UnboundedReceiver<CdpEnvelope>>,
        outbound: mpsc::UnboundedSender<CdpEnvelope>,
        close_reason: Arc<Mutex<Option<String>>>,
        closed: watch::Sender<bool>,
        unresponsive: AtomicBool,
    }

    impl EchoTransport {
        fn new() -> Arc<Self> {
            let (outbound, inbound) = mpsc::unbounded_channel();
            let (closed, _) = watch::channel(false);
            Arc::new(Self {
                inbound: Mutex::new(inbound),
                outbound,
                close_reason: Arc::new(Mutex::new(None)),
                closed,
                unresponsive: AtomicBool::new(false),
            })
        }

        fn lose(&self) {
            self.closed.send_replace(true);
        }

        fn notify(&self, method: &str, params: Value) {
            self.outbound.send(CdpEnvelope {
                session_id: None,
                message: JsonRpcMessage::Notification(JsonRpcNotification {
                    method: method.to_owned(),
                    params: Some(params),
                }),
            }).unwrap();
        }
    }

    #[async_trait]
    impl MessageTransport<CdpEnvelope, CdpEnvelope> for EchoTransport {
        async fn send(&self, message: CdpEnvelope) -> Result<(), TransportError> {
            if *self.closed.borrow() {
                return Err(TransportError::Closed);
            }
            let JsonRpcMessage::Request(request) = message.message else {
                return Ok(());
            };
            if self.unresponsive.load(Ordering::Relaxed) {
                return Ok(());
            }
            let result = if request.method == "Target.attachToTarget" {
                json!({"sessionId": "native-child"})
            } else {
                json!({ "echo": request.method, "sessionId": message.session_id })
            };
            self.outbound
                .send(CdpEnvelope {
                    session_id: message.session_id,
                    message: JsonRpcMessage::Response(JsonRpcResponse {
                        id: Some(request.id),
                        payload: ResponsePayload::Result(result),
                    }),
                })
                .map_err(|_| TransportError::Closed)
        }

        async fn recv(&self) -> Option<CdpEnvelope> {
            let mut closed = self.closed.subscribe();
            let mut inbound = self.inbound.lock().await;
            tokio::select! {
                message = inbound.recv() => message,
                _ = closed.changed() => None,
            }
        }
    }

    #[async_trait]
    impl ManagedCdpTransport for EchoTransport {
        fn close_reason(&self) -> Arc<Mutex<Option<String>>> {
            self.close_reason.clone()
        }

        async fn wait_closed(&self) -> String {
            let mut closed = self.closed.subscribe();
            while !*closed.borrow_and_update() {
                if closed.changed().await.is_err() {
                    break;
                }
            }
            "child CDP transport closed".to_owned()
        }

        async fn close(&self) {
            self.lose();
        }
    }

    #[derive(Default)]
    struct StubCalls {
        discovery: Vec<bool>,
        child_discovery: Vec<(String, bool)>,
        wait_for_debugger_on_start: Vec<bool>,
        list_calls: usize,
        attached: Vec<String>,
        detached: Vec<String>,
    }

    struct StubSource {
        targets: StdMutex<Vec<HostTarget>>,
        calls: Arc<StdMutex<StubCalls>>,
        endpoints: StdMutex<BTreeMap<String, Arc<EchoTransport>>>,
    }

    impl StubSource {
        fn new(targets: Vec<HostTarget>) -> (Arc<Self>, Arc<StdMutex<StubCalls>>) {
            let calls = Arc::new(StdMutex::new(StubCalls::default()));
            (
                Arc::new(Self {
                    targets: StdMutex::new(targets),
                    calls: calls.clone(),
                    endpoints: StdMutex::new(BTreeMap::new()),
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

        async fn set_child_discovery(&self, target_id: &str, enabled: bool) {
            self.calls
                .lock()
                .unwrap()
                .child_discovery
                .push((target_id.to_owned(), enabled));
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
            let transport = EchoTransport::new();
            self.endpoints.lock().unwrap().insert(target_id.to_owned(), transport.clone());
            Ok(TargetAttachment {
                endpoint: TargetEndpoint::open(transport)?,
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
            primary_window_id: None,
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
    async fn typed_root_accepts_null_for_parameterless_methods_but_not_required_params() {
        let harness = Harness::start(vec![host_target("$node-root", 4242)]);

        let version = harness
            .client
            .call("Browser.getVersion", Value::Null)
            .await
            .unwrap();
        assert_eq!(version["product"], "Process 4242");

        let targets = harness
            .client
            .call("Target.getTargets", Value::Null)
            .await
            .unwrap();
        assert_eq!(targets["targetInfos"].as_array().unwrap().len(), 1);

        let error = harness
            .client
            .call("Target.attachToTarget", Value::Null)
            .await
            .unwrap_err();
        assert_eq!(error.code, error_codes::INVALID_PARAMS);
        assert!(error.message.contains("targetId"), "{error:?}");
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
    async fn target_observation_is_revisioned_and_does_not_signal_unchanged_refreshes() {
        let harness = Harness::start(vec![host_target("$node-root", 4242)]);
        let mut observation = harness.root.observe_targets();
        assert_eq!(observation.revision, 0);
        assert!(observation.targets.is_empty());

        let refreshed = harness.root.refresh_targets().await;
        assert_eq!(refreshed.revision, 1);
        assert_eq!(refreshed.targets.len(), 1);
        observation.revisions.changed().await.unwrap();
        assert_eq!(*observation.revisions.borrow(), refreshed.revision);

        let unchanged = harness.root.refresh_targets().await;
        assert_eq!(unchanged.revision, refreshed.revision);
        assert!(
            tokio::time::timeout(
                std::time::Duration::from_millis(10),
                observation.revisions.changed(),
            )
            .await
            .is_err()
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
    async fn child_discovery_follows_only_attached_targets() {
        let harness = Harness::start(vec![
            host_target("renderer-a", 10),
            host_target("renderer-b", 20),
        ]);
        harness.call("Target.getTargets", json!({})).await;
        harness
            .call("Target.setDiscoverTargets", json!({ "discover": true }))
            .await;
        let attached = harness
            .call("Target.attachToTarget", json!({ "targetId": "renderer-a" }))
            .await;
        let session_id = attached["sessionId"].as_str().unwrap();

        assert_eq!(
            harness.calls.lock().unwrap().child_discovery,
            vec![("renderer-a".to_owned(), true)]
        );

        harness
            .call(
                "Target.detachFromTarget",
                json!({ "sessionId": session_id }),
            )
            .await;
        assert_eq!(
            harness.calls.lock().unwrap().child_discovery,
            vec![
                ("renderer-a".to_owned(), true),
                ("renderer-a".to_owned(), false),
            ]
        );
    }

    #[tokio::test]
    async fn dead_child_attachment_retires_without_closing_host_or_sibling() {
        let harness = Harness::start(vec![host_target("renderer-a", 10), host_target("renderer-b", 20)]);
        harness.call("Target.getTargets", json!({})).await;
        let a = harness.call("Target.attachToTarget", json!({"targetId":"renderer-a"})).await;
        let b = harness.call("Target.attachToTarget", json!({"targetId":"renderer-b"})).await;
        let a_id = a["sessionId"].as_str().unwrap().to_owned();
        let b_id = b["sessionId"].as_str().unwrap().to_owned();
        let old_endpoint = harness.root.target_endpoint("renderer-a").unwrap();
        let a_channel = Channel::new(Box::new(harness.mux.open_session(a_id.clone()).unwrap()), Box::new(RejectingHandler));
        let b_channel = Channel::new(Box::new(harness.mux.open_session(b_id.clone()).unwrap()), Box::new(RejectingHandler));
        for channel in [a_channel.clone(), b_channel.clone()] {
            tokio::spawn(async move { channel.run().await });
        }
        assert_eq!(a_channel.call("Runtime.evaluate", json!({})).await.unwrap()["echo"], "Runtime.evaluate");
        let a_transport = harness.source.endpoints.lock().unwrap()["renderer-a"].clone();
        a_transport.unresponsive.store(true, Ordering::Relaxed);
        let pending = tokio::spawn({
            let channel = a_channel.clone();
            async move { channel.call("Runtime.evaluate", json!({})).await }
        });
        harness.settle().await;
        a_transport.lose();
        tokio::time::timeout(std::time::Duration::from_secs(2), async {
            loop {
                if !harness.root.refresh_targets().await.targets.iter().find(|t| t.target_id() == "renderer-a").unwrap().snapshot.attached {
                    break;
                }
                tokio::task::yield_now().await;
            }
        }).await.expect("dead child must cease being advertised as attached");
        assert_eq!(b_channel.call("Runtime.evaluate", json!({})).await.unwrap()["echo"], "Runtime.evaluate");
        assert_eq!(harness.call("Browser.getVersion", json!({})).await["product"], "Process 4242");
        assert!(harness.events.events.lock().unwrap().iter().any(|(method, params)| {
            method == "Target.detachedFromTarget" && params["sessionId"] == a_id
        }));
        harness.mux.retire_session(&a_id);
        assert!(tokio::time::timeout(std::time::Duration::from_millis(50), pending).await.is_ok(),
            "requests pending on the dead child must settle promptly");
        assert!(tokio::time::timeout(std::time::Duration::from_millis(50), a_channel.call("Runtime.evaluate", json!({}))).await.is_ok(),
            "dead child requests must settle promptly");
        let fresh = harness.call("Target.attachToTarget", json!({"targetId":"renderer-a"})).await;
        let fresh_id = fresh["sessionId"].as_str().unwrap().to_owned();
        let fresh_channel = Channel::new(Box::new(harness.mux.open_session(fresh_id).unwrap()), Box::new(RejectingHandler));
        tokio::spawn({ let channel = fresh_channel.clone(); async move { channel.run().await } });
        assert_eq!(fresh_channel.call("Runtime.evaluate", json!({})).await.unwrap()["echo"], "Runtime.evaluate");
        harness.root.state.closed_attachment_tx.send(ClosedAttachment {
            session_id: a_id,
            endpoint: old_endpoint,
            reason: "late old endpoint close".into(),
        }).unwrap();
        harness.settle().await;
        assert_eq!(fresh_channel.call("Runtime.evaluate", json!({})).await.unwrap()["echo"], "Runtime.evaluate");
        assert!(harness.root.observe_targets().targets.iter().find(|t| t.target_id() == "renderer-a").unwrap().snapshot.attached);
    }

    #[tokio::test]
    async fn raw_native_session_uses_its_own_endpoint_for_repeated_and_concurrent_requests() {
        let harness = Harness::start(vec![host_target("renderer-a", 10)]);
        harness.call("Target.getTargets", json!({})).await;
        let virtual_id = harness.call("Target.attachToTarget", json!({"targetId":"renderer-a"})).await["sessionId"].as_str().unwrap().to_owned();
        let parent = Channel::new(Box::new(harness.mux.open_session(virtual_id).unwrap()), Box::new(RejectingHandler));
        tokio::spawn({ let parent = parent.clone(); async move { parent.run().await } });
        let attach = parent.call("Target.attachToTarget", json!({"targetId":"iframe", "flatten":true})).await.unwrap();
        let native_id = attach["sessionId"].as_str().unwrap().to_owned();
        let endpoint = harness.root.target_endpoint("renderer-a").unwrap();
        let native = endpoint.open_raw_session(native_id.clone()).unwrap();
        for _ in 0..3 {
            let result = native.request("Runtime.evaluate", json!({"expression":"1+1"}), std::time::Duration::from_secs(1)).await.unwrap();
            assert_eq!(result["echo"], "Runtime.evaluate");
            assert_eq!(result["sessionId"], native_id);
        }
        let (first, second) = tokio::join!(
            native.request("Runtime.evaluate", json!({"expression":"2"}), std::time::Duration::from_secs(1)),
            native.request("Runtime.evaluate", json!({"expression":"3"}), std::time::Duration::from_secs(1)),
        );
        assert_eq!(first.unwrap()["sessionId"], native_id);
        assert_eq!(second.unwrap()["sessionId"], native_id);
        native.close();
        assert!(native.request("Runtime.evaluate", json!({}), std::time::Duration::from_millis(20)).await.is_err());
    }

    #[tokio::test]
    async fn identical_native_ids_on_distinct_endpoints_never_cross_route() {
        let harness = Harness::start(vec![host_target("renderer-a", 10), host_target("renderer-b", 20)]);
        harness.call("Target.getTargets", json!({})).await;
        for target in ["renderer-a", "renderer-b"] {
            harness.call("Target.attachToTarget", json!({"targetId":target})).await;
        }
        let a = harness.root.target_endpoint("renderer-a").unwrap();
        let b = harness.root.target_endpoint("renderer-b").unwrap();
        let a_session = a.open_raw_session("native-child".to_owned()).unwrap();
        let b_session = b.open_raw_session("native-child".to_owned()).unwrap();
        a_session.close();
        assert_eq!(b_session.request("Runtime.evaluate", json!({}), std::time::Duration::from_secs(1)).await.unwrap()["echo"], "Runtime.evaluate");
    }

    #[tokio::test]
    async fn native_child_detach_by_session_id_closes_only_the_child_route() {
        let transport = EchoTransport::new();
        let parent = TargetEndpoint::open(transport.clone()).unwrap();
        let child = parent.attach_child("iframe").await.unwrap();
        assert_eq!(child.call("Runtime.evaluate", json!({})).await.unwrap()["echo"], "Runtime.evaluate");
        transport.notify("Target.detachedFromTarget", json!({"sessionId":"native-child"}));
        let reason = tokio::time::timeout(std::time::Duration::from_secs(1), child.wait_closed()).await.unwrap();
        assert!(reason.contains("native-child"), "{reason}");
        assert_eq!(parent.call("Runtime.evaluate", json!({})).await.unwrap()["echo"], "Runtime.evaluate");
    }

    #[tokio::test]
    async fn timed_out_raw_request_does_not_retire_other_sessions_or_lock_channel() {
        let transport = EchoTransport::new();
        let endpoint = TargetEndpoint::open(transport.clone()).unwrap();
        let child = endpoint.open_raw_session("native-child".to_owned()).unwrap();
        transport.unresponsive.store(true, Ordering::Relaxed);
        let error = child.request("Runtime.evaluate", json!({}), std::time::Duration::from_millis(20)).await.unwrap_err();
        assert_eq!(error.code, error_codes::REQUEST_TIMEOUT);
        transport.unresponsive.store(false, Ordering::Relaxed);
        assert_eq!(child.request("Runtime.evaluate", json!({}), std::time::Duration::from_secs(1)).await.unwrap()["echo"], "Runtime.evaluate");
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
    async fn live_primary_window_ownership_is_updated_and_removed() {
        let harness = Harness::start(Vec::new());
        let mut target = host_target("renderer-1", 77);
        target.primary_window_id = Some(7);
        harness
            .sender
            .send(TargetSourceEvent::Upserted(target.clone()))
            .unwrap();
        harness.settle().await;
        assert_eq!(harness.root.target_primary_window_id("renderer-1"), Some(7));
        target.primary_window_id = None;
        harness
            .sender
            .send(TargetSourceEvent::Upserted(target))
            .unwrap();
        harness.settle().await;
        assert_eq!(harness.root.target_primary_window_id("renderer-1"), None);
        harness
            .sender
            .send(TargetSourceEvent::Removed("renderer-1".to_owned()))
            .unwrap();
        harness.settle().await;
        assert_eq!(harness.root.target_primary_window_id("renderer-1"), None);
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
