pub mod playwright_proxy;

use std::collections::BTreeMap;
use std::env;
use std::path::PathBuf;
use std::process::Stdio;
use std::sync::{Arc, OnceLock, Weak};
use std::time::Duration;

use async_trait::async_trait;
use linkrpc::prelude::{JsonRpcError, error_codes};
use serde::Deserialize;
use serde_json::Value;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::process::{Child, Command};
use tokio::sync::{Mutex, broadcast, mpsc};
use tokio::time::timeout;
use url::Url;

use crate::api::capability::{
    Capability, CapabilityError, CapabilityKind, CapabilitySummary, DebugCapability,
    DebugOpenRequest, DebugSessionHandle, PauseFutureChildrenCapability,
};
use crate::cdp::CdpClient;
use crate::debugger::cdp_runtime::{CdpConnection, CdpDebuggerSession, CdpRuntimeError, RootCdpEvent};
use crate::debugger::debugger_engine::SessionKey;
use crate::connection::discovery::{
    LeaseObserver, PauseChildrenKey, PauseChildrenLease, PauseChildrenLeaseRegistry,
};
use crate::connection::discovery::process_tree_source::ProcessTreeTargetSource;
use crate::debugger::resource_graph::ResourceId;
use crate::api::service_api::{
    CdpStdioTopology, ConnectionConfiguration, PlaywrightChannel, TargetSnapshot,
};
use crate::connection::transport::session_transport::RawCdpSession;
use crate::connection::transport::stdio_transport::CdpStdioTransport;
use crate::service::virtual_browser_root::{TargetEndpoint, VirtualBrowserRoot};

const PLAYWRIGHT_STARTUP_TIMEOUT: Duration = Duration::from_secs(30);
const RAW_CDP_SESSION_TIMEOUT: Duration = Duration::from_secs(30);
const PLAYWRIGHT_HELPER: &str = include_str!("playwright.mjs");
const CHROME_HELPER: &str = include_str!("chrome.mjs");
const NODE_HELPER: &str = include_str!("node.mjs");
const PROCESS_TREE_HELPER: &str = include_str!("process_tree.mjs");

pub struct ConnectionRuntime {
    cdp: Arc<CdpConnection>,
    provider: Option<Mutex<Child>>,
    direct_debugger: bool,
    connection_generation: u64,
    root_endpoint: String,
    /// Present when this connection is a process tree fronted by a virtual browser root. The
    /// runtime keeps it to answer the two questions CDP's `Target` domain cannot express:
    /// which OS process backs a target, and whether it is genuinely paused at startup.
    virtual_root: Option<Arc<VirtualBrowserRoot>>,
    raw_sessions: std::sync::Mutex<BTreeMap<(String, String), RawAttachment>>,
    direct_debuggers: std::sync::Mutex<BTreeMap<String, Arc<CdpConnection>>>,
    direct_debugger_endpoints: std::sync::Mutex<BTreeMap<String, String>>,
    direct_debugger_attach_lock: Mutex<()>,
    provider_target_events: Mutex<Option<mpsc::UnboundedReceiver<ProviderTargetEvent>>>,
    provider_target_sender: mpsc::UnboundedSender<ProviderTargetEvent>,
    pause_future_children: OnceLock<Arc<ConnectionPauseFutureChildrenCapability>>,
}

struct RawAttachment {
    session: Arc<RawCdpSession>,
    endpoint: Option<Arc<TargetEndpoint>>,
}

#[derive(Clone, Debug)]
pub enum ProviderTargetEvent {
    Upsert(TargetSnapshot),
    Removed(String),
}

pub struct DirectDebuggerAttachment {
    pub session: CdpDebuggerSession,
    pub stole_external_owner: bool,
}

/// The session opened for one target, plus whether a debugger dbgjs does not own was evicted.
pub struct TargetAttachment {
    pub session_id: String,
    pub stole_external_owner: bool,
}

struct PauseDemandObserver {
    runtime: Weak<ConnectionRuntime>,
}

impl LeaseObserver<PauseChildrenKey> for PauseDemandObserver {
    fn demand_changed(&self, _key: &PauseChildrenKey, demand: usize) {
        if demand != 0 {
            return;
        }
        let runtime = self.runtime.clone();
        tokio::spawn(async move {
            if let Some(runtime) = runtime.upgrade() {
                runtime.set_wait_for_debugger_on_start(false).await;
            }
        });
    }
}

struct ConnectionPauseFutureChildrenCapability {
    runtime: Weak<ConnectionRuntime>,
    root: ResourceId,
    leases: PauseChildrenLeaseRegistry,
    routes: std::sync::Mutex<BTreeMap<ResourceId, String>>,
}

impl Capability for ConnectionPauseFutureChildrenCapability {
    fn kind(&self) -> CapabilityKind {
        CapabilityKind::PauseFutureChildren
    }

    fn summary(&self) -> CapabilitySummary {
        CapabilitySummary::new("Pause future child targets")
    }
}

#[async_trait]
impl PauseFutureChildrenCapability for ConnectionPauseFutureChildrenCapability {
    async fn arm(&self) -> Result<PauseChildrenLease, CapabilityError> {
        let runtime = self.runtime.upgrade().ok_or_else(|| {
            CapabilityError::Unavailable("connection is no longer available".to_owned())
        })?;
        if runtime.virtual_root.is_none() {
            return Err(CapabilityError::Unavailable(
                "connection cannot pause future child targets".to_owned(),
            ));
        }
        let key = PauseChildrenKey(self.root.clone());
        let lease = self.leases.acquire(key);
        if lease.demand() == 1 {
            runtime.set_wait_for_debugger_on_start(true).await;
        }
        Ok(lease)
    }

    async fn resume(&self, resource: &ResourceId) -> Result<(), CapabilityError> {
        let runtime_target_id = self
            .routes
            .lock()
            .unwrap()
            .get(resource)
            .cloned()
            .ok_or_else(|| {
                CapabilityError::Unavailable(format!(
                    "resource '{resource}' is not a child of this connection"
                ))
            })?;
        let runtime = self.runtime.upgrade().ok_or_else(|| {
            CapabilityError::Unavailable("connection is no longer available".to_owned())
        })?;
        let attached = runtime
            .attach_to_target(&runtime_target_id, false)
            .await
            .map_err(|error| CapabilityError::Failed(error.to_string()))?;
        let session = runtime
            .open_session(SessionKey {
                connection_generation: runtime.connection_generation,
                session_id: attached.session_id.clone(),
            })
            .map_err(|error| CapabilityError::Failed(error.to_string()))?;
        let result = session
            .raw_request("Runtime.runIfWaitingForDebugger", serde_json::json!({}))
            .await
            .map_err(|error| CapabilityError::Failed(format!("{error:?}")));
        let close_result = runtime
            .close_attached_session(None, &attached.session_id)
            .await;
        match (result, close_result) {
            (Ok(_), Ok(())) => Ok(()),
            (Err(error), Ok(())) => Err(error),
            (Ok(_), Err(error)) => Err(CapabilityError::Failed(error.to_string())),
            (Err(operation), Err(close)) => Err(CapabilityError::Failed(format!(
                "{operation}; additionally failed to close the debug session: {close}"
            ))),
        }
    }
}

pub struct ConnectionTargetDebugCapability {
    runtime: Weak<ConnectionRuntime>,
    resource: ResourceId,
    runtime_target_id: String,
    direct: bool,
    pending: std::sync::Mutex<BTreeMap<String, CdpDebuggerSession>>,
}

impl ConnectionTargetDebugCapability {
    fn new(
        runtime: &Arc<ConnectionRuntime>,
        resource: ResourceId,
        runtime_target_id: String,
    ) -> Arc<Self> {
        Arc::new(Self {
            runtime: Arc::downgrade(runtime),
            resource,
            runtime_target_id,
            direct: runtime.is_direct_debugger(),
            pending: std::sync::Mutex::new(BTreeMap::new()),
        })
    }

    fn runtime(&self) -> Result<Arc<ConnectionRuntime>, CapabilityError> {
        self.runtime
            .upgrade()
            .ok_or_else(|| CapabilityError::Unavailable("connection is closed".to_owned()))
    }
}

impl Capability for ConnectionTargetDebugCapability {
    fn kind(&self) -> CapabilityKind {
        CapabilityKind::Debug
    }

    fn summary(&self) -> CapabilitySummary {
        CapabilitySummary::new("CDP debugger")
            .with_detail("targetId", Value::from(self.runtime_target_id.clone()))
            .with_detail("direct", Value::from(self.direct))
    }
}

#[async_trait]
impl DebugCapability for ConnectionTargetDebugCapability {
    async fn open(&self, request: DebugOpenRequest) -> Result<DebugSessionHandle, CapabilityError> {
        let runtime = self.runtime()?;
        let (session, stole_existing_owner) = if self.direct {
            let attachment = runtime
                .take_direct_debugger_session(&self.runtime_target_id)
                .await
                .map_err(|error| CapabilityError::Failed(error.to_string()))?
                .ok_or_else(|| {
                    CapabilityError::Unavailable(
                        "direct debugger target has no endpoint".to_owned(),
                    )
                })?;
            (attachment.session, attachment.stole_external_owner)
        } else {
            let attachment = runtime
                .attach_to_target(&self.runtime_target_id, request.steal_existing_owner)
                .await
                .map_err(|error| CapabilityError::Failed(error.to_string()))?;
            let key = SessionKey {
                connection_generation: runtime.connection_generation,
                session_id: attachment.session_id.clone(),
            };
            let session = runtime
                .open_session(key)
                .map_err(|error| CapabilityError::Failed(error.to_string()))?;
            (session, attachment.stole_external_owner)
        };
        let session_id = session.key().session_id.clone();
        self.pending
            .lock()
            .unwrap()
            .insert(session_id.clone(), session);
        Ok(DebugSessionHandle {
            session_id,
            resource: self.resource.clone(),
            stole_existing_owner,
        })
    }

    fn take_session(
        &self,
        handle: &DebugSessionHandle,
    ) -> Result<CdpDebuggerSession, CapabilityError> {
        if handle.resource != self.resource {
            return Err(CapabilityError::Rejected(
                "debug session belongs to another resource".to_owned(),
            ));
        }
        self.pending
            .lock()
            .unwrap()
            .remove(&handle.session_id)
            .ok_or_else(|| {
                CapabilityError::Unavailable(format!(
                    "debug session {} was already consumed or closed",
                    handle.session_id
                ))
            })
    }

    async fn close(&self, handle: &DebugSessionHandle) -> Result<(), CapabilityError> {
        self.pending.lock().unwrap().remove(&handle.session_id);
        self.runtime()?
            .close_attached_session(
                self.direct.then_some(self.runtime_target_id.as_str()),
                &handle.session_id,
            )
            .await
            .map_err(|error| CapabilityError::Failed(error.to_string()))?;
        Ok(())
    }

    fn waiting_for_debugger(&self) -> bool {
        self.runtime
            .upgrade()
            .is_some_and(|runtime| runtime.target_waiting_for_debugger(&self.runtime_target_id))
    }
}

impl ConnectionRuntime {
    pub async fn connect(
        configuration: &ConnectionConfiguration,
        connection_generation: u64,
    ) -> Result<Arc<Self>, ConnectionProviderError> {
        if let ConnectionConfiguration::Stdio {
            command,
            args,
            cwd,
            env,
            topology,
        } = configuration
        {
            return Self::connect_stdio(command, args, cwd, env, *topology, connection_generation)
                .await;
        }
        if let ConnectionConfiguration::ProcessTree { root_pid }
        | ConnectionConfiguration::ScopedProcessTree { root_pid, .. } = configuration
        {
            return Self::connect_process_tree(*root_pid, connection_generation).await;
        }
        let (endpoint, provider, direct_debugger, provider_events) = match configuration {
            ConnectionConfiguration::DirectCdp { endpoint } => {
                (endpoint.clone(), None, false, None)
            }
            ConnectionConfiguration::NodeInspector { endpoint } => {
                (endpoint.clone(), None, true, None)
            }
            ConnectionConfiguration::Process { process_id } => {
                let launch = launch_provider(
                    PROCESS_TREE_HELPER,
                    "process",
                    true,
                    [
                        ("DBGJS_PROCESS_ROOT_PID", process_id.to_string()),
                        ("DBGJS_PROCESS_MODE", "single".to_owned()),
                    ],
                )
                .await?;
                (launch.endpoint, Some(launch.child), true, launch.events)
            }
            ConnectionConfiguration::Playwright {
                url,
                playwright_package,
                channel,
                headless,
                ignore_https_errors,
            } => {
                let (endpoint, provider) = launch_playwright(
                    url,
                    playwright_package.as_deref(),
                    channel,
                    *headless,
                    *ignore_https_errors,
                )
                .await?;
                (endpoint, provider, false, None)
            }
            ConnectionConfiguration::Chrome {
                url,
                executable,
                headless,
                user_data_dir,
                args,
            } => {
                let launch = launch_provider(
                    CHROME_HELPER,
                    "Chrome",
                    false,
                    [
                        ("DBGJS_PROVIDER_URL", url.clone()),
                        ("DBGJS_CHROME_EXECUTABLE", executable.clone()),
                        (
                            "DBGJS_PROVIDER_MODE",
                            if *headless { "headless" } else { "headed" }.to_owned(),
                        ),
                        (
                            "DBGJS_CHROME_USER_DATA_DIR",
                            user_data_dir.clone().unwrap_or_default(),
                        ),
                        (
                            "DBGJS_CHROME_ARGS",
                            serde_json::to_string(args).expect("Chrome arguments always serialize"),
                        ),
                    ],
                )
                .await?;
                (launch.endpoint, Some(launch.child), false, None)
            }
            ConnectionConfiguration::Node {
                program,
                args,
                cwd,
                runtime_executable,
                runtime_args,
                env,
            } => {
                let launch = launch_provider(
                    NODE_HELPER,
                    "Node.js",
                    true,
                    [
                        ("DBGJS_NODE_PROGRAM", program.clone()),
                        (
                            "DBGJS_NODE_ARGS",
                            serde_json::to_string(args)
                                .expect("Node.js arguments always serialize"),
                        ),
                        ("DBGJS_NODE_CWD", cwd.clone()),
                        ("DBGJS_NODE_EXECUTABLE", runtime_executable.clone()),
                        (
                            "DBGJS_NODE_RUNTIME_ARGS",
                            serde_json::to_string(runtime_args)
                                .expect("Node.js runtime arguments always serialize"),
                        ),
                        (
                            "DBGJS_NODE_ENV",
                            serde_json::to_string(env)
                                .expect("Node.js environment always serializes"),
                        ),
                    ],
                )
                .await?;
                (launch.endpoint, Some(launch.child), true, launch.events)
            }
            ConnectionConfiguration::ProcessTree { .. }
            | ConnectionConfiguration::ScopedProcessTree { .. }
            | ConnectionConfiguration::Stdio { .. } => {
                unreachable!()
            }
        };
        let cdp_result = if direct_debugger {
            CdpConnection::connect_root_debugger(
                &endpoint,
                connection_generation,
                "$node-root".to_owned(),
            )
            .await
        } else {
            CdpConnection::connect(&endpoint).await
        };
        let cdp = match cdp_result {
            Ok(cdp) => Arc::new(cdp),
            Err(error) => {
                if let Some(mut provider) = provider {
                    terminate_provider(&mut provider).await;
                }
                return Err(error.into());
            }
        };
        let (provider_target_sender, provider_target_receiver) = mpsc::unbounded_channel();
        let runtime = Arc::new(Self {
            cdp,
            provider: provider.map(Mutex::new),
            direct_debugger,
            connection_generation,
            root_endpoint: endpoint,
            virtual_root: None,
            raw_sessions: std::sync::Mutex::new(BTreeMap::new()),
            direct_debuggers: std::sync::Mutex::new(BTreeMap::new()),
            direct_debugger_endpoints: std::sync::Mutex::new(BTreeMap::new()),
            direct_debugger_attach_lock: Mutex::new(()),
            provider_target_events: Mutex::new(Some(provider_target_receiver)),
            provider_target_sender,
            pause_future_children: OnceLock::new(),
        });
        if let Some(events) = provider_events {
            runtime.supervise_provider_events(events, connection_generation);
        }
        Ok(runtime)
    }

    /// A process tree is exposed as a virtual browser root: the connection speaks the ordinary
    /// CDP `Target` domain, and everything host specific (OS descendant scanning, Electron
    /// renderer discovery) lives behind [`ProcessTreeTargetSource`].
    async fn connect_process_tree(
        root_pid: u32,
        connection_generation: u64,
    ) -> Result<Arc<Self>, ConnectionProviderError> {
        let mut launch = launch_provider(
            PROCESS_TREE_HELPER,
            "process tree",
            true,
            [
                ("DBGJS_PROCESS_ROOT_PID", root_pid.to_string()),
                ("DBGJS_PROCESS_MODE", "tree".to_owned()),
            ],
        )
        .await?;
        let Some(control) = launch.child.stdin.take() else {
            terminate_provider(&mut launch.child).await;
            return Err(ConnectionProviderError::MissingStdioStdin);
        };
        let events = launch
            .events
            .expect("the process tree provider always captures events");
        let started = ProcessTreeTargetSource::start(
            root_pid,
            &launch.endpoint,
            launch.browser_endpoint.as_deref(),
            control,
            events,
        )
        .await;
        let (source, source_events) = match started {
            Ok(started) => started,
            Err(error) => {
                terminate_provider(&mut launch.child).await;
                return Err(ConnectionProviderError::ProcessTree(error));
            }
        };
        let virtual_root = match VirtualBrowserRoot::start(source, source_events) {
            Ok(root) => root,
            Err(error) => {
                terminate_provider(&mut launch.child).await;
                return Err(ConnectionProviderError::ProcessTree(error));
            }
        };
        let cdp = match CdpConnection::connect_transport(virtual_root.clone()).await {
            Ok(cdp) => Arc::new(cdp),
            Err(error) => {
                terminate_provider(&mut launch.child).await;
                return Err(error.into());
            }
        };
        let (provider_target_sender, provider_target_receiver) = mpsc::unbounded_channel();
        Ok(Arc::new(Self {
            cdp,
            provider: Some(Mutex::new(launch.child)),
            direct_debugger: false,
            connection_generation,
            root_endpoint: launch.endpoint,
            virtual_root: Some(virtual_root),
            raw_sessions: std::sync::Mutex::new(BTreeMap::new()),
            direct_debuggers: std::sync::Mutex::new(BTreeMap::new()),
            direct_debugger_endpoints: std::sync::Mutex::new(BTreeMap::new()),
            direct_debugger_attach_lock: Mutex::new(()),
            provider_target_events: Mutex::new(Some(provider_target_receiver)),
            provider_target_sender,
            pause_future_children: OnceLock::new(),
        }))
    }

    async fn connect_stdio(
        command: &str,
        args: &[String],
        cwd: &str,
        env: &BTreeMap<String, String>,
        topology: CdpStdioTopology,
        connection_generation: u64,
    ) -> Result<Arc<Self>, ConnectionProviderError> {
        let (transport, mut child) = launch_stdio(command, args, cwd, env).await?;
        let direct_debugger = topology == CdpStdioTopology::Target;
        let cdp = if direct_debugger {
            CdpConnection::connect_root_debugger_transport(
                transport,
                connection_generation,
                "$node-root".to_owned(),
            )
            .await
        } else {
            CdpConnection::connect_transport(transport).await
        };
        let cdp = match cdp {
            Ok(cdp) => Arc::new(cdp),
            Err(error) => {
                terminate_provider(&mut child).await;
                return Err(error.into());
            }
        };
        let (provider_target_sender, provider_target_events) = mpsc::unbounded_channel();
        Ok(Arc::new(Self {
            cdp,
            provider: Some(Mutex::new(child)),
            direct_debugger,
            connection_generation,
            root_endpoint: format!("stdio:{command}"),
            virtual_root: None,
            raw_sessions: std::sync::Mutex::new(BTreeMap::new()),
            direct_debuggers: std::sync::Mutex::new(BTreeMap::new()),
            direct_debugger_endpoints: std::sync::Mutex::new(BTreeMap::new()),
            direct_debugger_attach_lock: Mutex::new(()),
            provider_target_events: Mutex::new(Some(provider_target_events)),
            provider_target_sender,
            pause_future_children: OnceLock::new(),
        }))
    }

    pub fn root(&self) -> &CdpClient<linkrpc::connection::channel::Channel> {
        self.cdp.root()
    }

    pub fn open_session(&self, session: SessionKey) -> Result<CdpDebuggerSession, CdpRuntimeError> {
        self.cdp.open_session(session)
    }

    pub fn debug_capability(
        self: &Arc<Self>,
        resource: ResourceId,
        runtime_target_id: String,
    ) -> Arc<dyn DebugCapability> {
        ConnectionTargetDebugCapability::new(self, resource, runtime_target_id)
    }

    pub fn pause_future_children_capability(
        self: &Arc<Self>,
        root: ResourceId,
        routes: BTreeMap<ResourceId, String>,
    ) -> Option<Arc<dyn PauseFutureChildrenCapability>> {
        self.virtual_root.as_ref()?;
        let capability = self.pause_future_children.get_or_init(|| {
            Arc::new(ConnectionPauseFutureChildrenCapability {
                runtime: Arc::downgrade(self),
                root,
                leases: PauseChildrenLeaseRegistry::with_observer(Arc::new(PauseDemandObserver {
                    runtime: Arc::downgrade(self),
                })),
                routes: std::sync::Mutex::new(BTreeMap::new()),
            })
        });
        *capability.routes.lock().unwrap() = routes;
        Some(capability.clone())
    }

    async fn set_wait_for_debugger_on_start(&self, enabled: bool) {
        if let Some(root) = &self.virtual_root {
            root.set_wait_for_debugger_on_start(enabled).await;
        }
    }

    pub async fn take_direct_debugger_session(
        self: &Arc<Self>,
        target_id: &str,
    ) -> Result<Option<DirectDebuggerAttachment>, CdpRuntimeError> {
        let _guard = self.direct_debugger_attach_lock.lock().await;
        if target_id == "$node-root"
            && let Some(session) = self.cdp.take_root_debugger_session()
        {
            return Ok(Some(DirectDebuggerAttachment {
                session,
                stole_external_owner: false,
            }));
        }
        if let Some(session) = self
            .direct_debuggers
            .lock()
            .unwrap()
            .get(target_id)
            .and_then(|connection| connection.take_root_debugger_session())
        {
            return Ok(Some(DirectDebuggerAttachment {
                session,
                stole_external_owner: false,
            }));
        }

        let endpoint = if target_id == "$node-root" {
            Some(self.root_endpoint.clone())
        } else {
            self.direct_debugger_endpoints
                .lock()
                .unwrap()
                .get(target_id)
                .cloned()
        };
        let Some(endpoint) = endpoint else {
            return Ok(None);
        };
        let connection = Arc::new(
            CdpConnection::connect_root_debugger(
                &endpoint,
                self.connection_generation,
                target_id.to_owned(),
            )
            .await?,
        );
        let session = connection
            .take_root_debugger_session()
            .expect("a new direct debugger connection has a root session");
        self.direct_debuggers
            .lock()
            .unwrap()
            .insert(target_id.to_owned(), connection.clone());
        supervise_direct_debugger(Arc::downgrade(self), target_id.to_owned(), connection);
        Ok(Some(DirectDebuggerAttachment {
            session,
            stole_external_owner: false,
        }))
    }

    pub async fn close_direct_debugger(&self, target_id: &str) -> bool {
        let connection = self.direct_debuggers.lock().unwrap().remove(target_id);
        if let Some(connection) = connection {
            connection.close().await;
            true
        } else {
            false
        }
    }

    pub async fn close_attached_session(
        &self,
        direct_target_id: Option<&str>,
        session_id: &str,
    ) -> Result<(), CdpRuntimeError> {
        if let Some(target_id) = direct_target_id {
            self.close_direct_debugger(target_id).await;
            return Ok(());
        }
        self.retire_session(session_id);
        self.root()
            .target()
            .detach_from_target(Some(session_id.to_owned()), None)
            .await
            .map_err(|error| {
                CdpRuntimeError::Transport(format!("Target.detachFromTarget failed: {error:?}"))
            })?;
        Ok(())
    }

    pub fn is_direct_debugger(&self) -> bool {
        self.direct_debugger
    }

    pub fn generation(&self) -> u64 {
        self.connection_generation
    }

    /// True when the CDP root of this connection is dbgjs's own virtual browser root rather than
    /// a real browser.
    pub fn is_virtual_root(&self) -> bool {
        self.virtual_root.is_some()
    }

    pub fn register_raw_session(
        self: &Arc<Self>,
        target_id: &str,
        session_id: &str,
        mut events: broadcast::Receiver<crate::debugger::cdp_runtime::RawCdpEvent>,
    ) -> Result<(), String> {
        let key = (target_id.to_owned(), session_id.to_owned());
        let mut sessions = self.raw_sessions.lock().unwrap();
        if sessions.contains_key(&key) {
            return Err(format!("raw CDP session '{session_id}' is already registered for target '{target_id}'"));
        }
        let endpoint = self.virtual_root.as_ref().map(|root| {
            root.target_endpoint(target_id).ok_or_else(|| {
                format!("target '{target_id}' no longer has a live endpoint; attach again")
            })
        }).transpose()?;
        let session = match &endpoint {
            Some(endpoint) => endpoint.open_raw_session(session_id.to_owned())?,
            None => self.cdp.open_raw_session(session_id.to_owned())
                .map_err(|error| error.to_string())?,
        };
        sessions.insert(key.clone(), RawAttachment { session: session.clone(), endpoint });
        drop(sessions);

        let failure_owner = Arc::downgrade(self);
        let failure_key = key.clone();
        let failed_session = session.clone();
        tokio::spawn(async move {
            if let Some(reason) = failed_session.wait_failed().await {
                eprintln!(
                    "raw CDP session '{}' failed: {reason}; reconnect before reusing this native ID",
                    failure_key.1
                );
                if let Some(owner) = failure_owner.upgrade() {
                    owner.retire_raw_session_if_same(
                        &failure_key.0,
                        &failure_key.1,
                        &failed_session,
                    );
                }
            }
        });

        let owner = Arc::downgrade(self);
        tokio::spawn(async move {
            loop {
                let event = match events.recv().await {
                    Ok(event) => event,
                    Err(broadcast::error::RecvError::Lagged(count)) => {
                        eprintln!(
                            "raw CDP session '{id}' lost {count} lifecycle events; retiring attachment, reattach required",
                            id = key.1
                        );
                        if let Some(owner) = owner.upgrade() {
                            owner.retire_raw_session_if_same(&key.0, &key.1, &session);
                        }
                        break;
                    }
                    Err(broadcast::error::RecvError::Closed) => {
                        if let Some(owner) = owner.upgrade() {
                            owner.retire_raw_session_if_same(&key.0, &key.1, &session);
                        }
                        break;
                    }
                };
                if event.method == "Target.detachedFromTarget"
                    && event.params.get("sessionId").and_then(Value::as_str) == Some(&key.1)
                {
                    if let Some(owner) = owner.upgrade() {
                        owner.retire_raw_session_if_same(&key.0, &key.1, &session);
                    }
                    break;
                }
            }
        });
        Ok(())
    }

    fn retire_raw_session_if_same(
        &self,
        target_id: &str,
        session_id: &str,
        expected: &Arc<RawCdpSession>,
    ) {
        let key = (target_id.to_owned(), session_id.to_owned());
        let mut sessions = self.raw_sessions.lock().unwrap();
        if sessions.get(&key).is_some_and(|attachment| Arc::ptr_eq(&attachment.session, expected))
            && let Some(attachment) = sessions.remove(&key)
        {
            attachment.session.close();
        }
    }

    pub fn retire_raw_session(&self, target_id: &str, session_id: &str) {
        if let Some(attachment) = self.raw_sessions.lock().unwrap()
            .remove(&(target_id.to_owned(), session_id.to_owned()))
        {
            attachment.session.close();
        }
    }

    pub fn has_raw_session(&self, target_id: &str, session_id: &str) -> bool {
        self.raw_sessions.lock().unwrap().contains_key(&(target_id.to_owned(), session_id.to_owned()))
    }

    pub fn retire_raw_sessions_for_target(&self, target_id: &str) {
        let mut sessions = self.raw_sessions.lock().unwrap();
        let keys = sessions.keys().filter(|(owner, _)| owner == target_id).cloned().collect::<Vec<_>>();
        for key in keys {
            if let Some(attachment) = sessions.remove(&key) {
                attachment.session.close();
            }
        }
    }

    pub async fn raw_session_request(
        &self,
        target_id: &str,
        session_id: &str,
        method: &str,
        params: Value,
    ) -> Result<Value, JsonRpcError> {
        let (session, endpoint) = {
            let sessions = self.raw_sessions.lock().unwrap();
            let attachment = sessions.get(&(target_id.to_owned(), session_id.to_owned()))
                .ok_or_else(|| JsonRpcError::new(
                    error_codes::INVALID_PARAMS,
                    format!("unknown or detached raw CDP session '{session_id}' for target '{target_id}'; attach again, or reconnect if its native route failed"),
                ))?;
            (attachment.session.clone(), attachment.endpoint.clone())
        };
        if let Some(endpoint) = &endpoint {
            if !self.virtual_root.as_ref().is_some_and(|root| root.owns_target_endpoint(target_id, endpoint))
                || endpoint.close_reason().await.is_some()
            {
                self.retire_raw_session_if_same(target_id, session_id, &session);
                return Err(JsonRpcError::new(
                    error_codes::PEER_DISCONNECTED,
                    format!("raw CDP session '{session_id}' lost its target endpoint; reattach target '{target_id}'"),
                ));
            }
        }
        let result = if let Some(endpoint) = endpoint {
            tokio::select! {
                result = session.request(method, params, RAW_CDP_SESSION_TIMEOUT) => result,
                reason = endpoint.wait_closed() => Err(JsonRpcError::new(
                    error_codes::PEER_DISCONNECTED,
                    format!("raw CDP session '{session_id}' lost its child endpoint: {reason}; reattach target '{target_id}'"),
                )),
            }
        } else {
            session.request(method, params, RAW_CDP_SESSION_TIMEOUT).await
        };
        result
    }

    /// The OS process behind a target, when the connection can tell. Real browsers do not expose
    /// this through `Target`, so only the virtual root answers.
    pub fn target_process_id(&self, target_id: &str) -> Option<u32> {
        self.virtual_root
            .as_ref()
            .and_then(|root| root.target_process_id(target_id))
    }

    pub fn closed_target_reason(&self, target_id: &str) -> Option<String> {
        self.virtual_root.as_ref()?.closed_target_reason(target_id)
    }

    pub fn target_primary_window_id(&self, target_id: &str) -> Option<u32> {
        self.virtual_root
            .as_ref()
            .and_then(|root| root.target_primary_window_id(target_id))
    }

    pub async fn refresh_targets(&self) -> Option<crate::service::virtual_browser_root::TargetObservation> {
        Some(self.virtual_root.as_ref()?.refresh_targets().await)
    }

    pub fn observe_targets(&self) -> Option<crate::service::virtual_browser_root::TargetObservation> {
        Some(self.virtual_root.as_ref()?.observe_targets())
    }

    pub async fn set_target_discovery(&self, enabled: bool) -> bool {
        let Some(root) = &self.virtual_root else {
            return false;
        };
        root.set_target_discovery(enabled).await;
        true
    }

    /// True when the target is genuinely blocked waiting for a debugger to resume it.
    pub fn target_waiting_for_debugger(&self, target_id: &str) -> bool {
        self.virtual_root
            .as_ref()
            .is_some_and(|root| root.target_waiting_for_debugger(target_id))
    }

    /// Attaches to a target through the connection's `Target` domain. Virtual roots are called
    /// in process because CDP has no way to express "evict the debugger that owns this target",
    /// which dbgjs needs for `force`.
    pub async fn attach_to_target(
        &self,
        target_id: &str,
        force: bool,
    ) -> Result<TargetAttachment, CdpRuntimeError> {
        if let Some(root) = &self.virtual_root {
            let attached = root
                .attach_target(target_id, force)
                .await
                .map_err(|error| CdpRuntimeError::Transport(error.message))?;
            return Ok(TargetAttachment {
                session_id: attached.session_id,
                stole_external_owner: attached.stole_external_owner,
            });
        }
        let attached = self
            .cdp
            .root()
            .target()
            .attach_to_target(target_id.to_owned(), Some(true), None)
            .await
            .map_err(|error| {
                CdpRuntimeError::Transport(format!("Target.attachToTarget failed: {error:?}"))
            })?;
        Ok(TargetAttachment {
            session_id: attached.session_id,
            stole_external_owner: false,
        })
    }

    pub fn retire_session(&self, session_id: &str) {
        self.cdp.retire_session(session_id);
    }

    pub async fn wait_closed(&self) -> String {
        self.cdp.wait_closed().await
    }

    pub async fn take_root_events(
        &self,
    ) -> Option<
        tokio::sync::mpsc::UnboundedReceiver<
            Result<RootCdpEvent, crate::debugger::cdp_runtime::CdpRuntimeEventError>,
        >,
    > {
        self.cdp.take_root_events().await
    }

    pub async fn take_provider_target_events(
        &self,
    ) -> Option<mpsc::UnboundedReceiver<ProviderTargetEvent>> {
        self.provider_target_events.lock().await.take()
    }

    pub async fn close(&self) {
        for attachment in std::mem::take(&mut *self.raw_sessions.lock().unwrap()).into_values() {
            attachment.session.close();
        }
        let direct_debuggers = std::mem::take(&mut *self.direct_debuggers.lock().unwrap());
        for connection in direct_debuggers.into_values() {
            connection.close().await;
        }
        self.cdp.close().await;
        if let Some(provider) = &self.provider {
            let mut provider = provider.lock().await;
            terminate_provider(&mut provider).await;
        }
    }

    fn supervise_provider_events(
        self: &Arc<Self>,
        mut events: mpsc::UnboundedReceiver<ProviderEvent>,
        connection_generation: u64,
    ) {
        let runtime = Arc::downgrade(self);
        tokio::spawn(async move {
            while let Some(event) = events.recv().await {
                let Some(runtime) = runtime.upgrade() else {
                    return;
                };
                match event {
                    ProviderEvent::NodeTarget {
                        target_id,
                        parent_target_id,
                        target_type,
                        title,
                        url,
                        endpoint,
                        process_id: _,
                    } => {
                        let Some(endpoint) = endpoint else {
                            let _ =
                                runtime
                                    .provider_target_sender
                                    .send(ProviderTargetEvent::Upsert(node_target_snapshot(
                                        target_id,
                                        parent_target_id,
                                        target_type,
                                        title,
                                        url,
                                    )));
                            continue;
                        };
                        runtime
                            .direct_debugger_endpoints
                            .lock()
                            .unwrap()
                            .insert(target_id.clone(), endpoint.clone());
                        if runtime
                            .direct_debuggers
                            .lock()
                            .unwrap()
                            .contains_key(&target_id)
                        {
                            let _ =
                                runtime
                                    .provider_target_sender
                                    .send(ProviderTargetEvent::Upsert(node_target_snapshot(
                                        target_id,
                                        parent_target_id,
                                        target_type,
                                        title,
                                        url,
                                    )));
                            continue;
                        }
                        let connection = match CdpConnection::connect_root_debugger(
                            &endpoint,
                            connection_generation,
                            target_id.clone(),
                        )
                        .await
                        {
                            Ok(connection) => Arc::new(connection),
                            Err(error) => {
                                eprintln!(
                                    "failed to connect discovered Node.js target {target_id}: {error}"
                                );
                                continue;
                            }
                        };
                        runtime
                            .direct_debuggers
                            .lock()
                            .unwrap()
                            .insert(target_id.clone(), connection.clone());
                        let _ = runtime
                            .provider_target_sender
                            .send(ProviderTargetEvent::Upsert(node_target_snapshot(
                                target_id.clone(),
                                parent_target_id,
                                target_type,
                                title,
                                url,
                            )));
                        supervise_direct_debugger(Arc::downgrade(&runtime), target_id, connection);
                    }
                    ProviderEvent::NodeTargetRemoved { target_id } => {
                        runtime
                            .direct_debugger_endpoints
                            .lock()
                            .unwrap()
                            .remove(&target_id);
                        let connection =
                            runtime.direct_debuggers.lock().unwrap().remove(&target_id);
                        if let Some(connection) = connection {
                            connection.close().await;
                        }
                        let _ = runtime
                            .provider_target_sender
                            .send(ProviderTargetEvent::Removed(target_id));
                    }
                    ProviderEvent::ScanComplete { .. } => {}
                    ProviderEvent::ActivationComplete { .. } => {}
                }
            }
        });
    }
}

fn supervise_direct_debugger(
    runtime: Weak<ConnectionRuntime>,
    target_id: String,
    connection: Arc<CdpConnection>,
) {
    tokio::spawn(async move {
        connection.wait_closed().await;
        let Some(runtime) = runtime.upgrade() else {
            return;
        };
        let mut debuggers = runtime.direct_debuggers.lock().unwrap();
        if debuggers
            .get(&target_id)
            .is_some_and(|current| Arc::ptr_eq(current, &connection))
        {
            debuggers.remove(&target_id);
        }
    });
}

fn node_target_snapshot(
    target_id: String,
    parent_id: String,
    target_type: Option<String>,
    title: String,
    url: String,
) -> TargetSnapshot {
    let target_type = target_type.unwrap_or_else(|| "node".to_owned());
    let subtype = match target_type.as_str() {
        "node" => Some("child-process".to_owned()),
        "page" => Some("electron-renderer".to_owned()),
        _ => None,
    };
    TargetSnapshot {
        target_id,
        target_type,
        title,
        url,
        attached: false,
        parent_id: Some(parent_id),
        opener_id: None,
        browser_context_id: None,
        subtype,
    }
}

pub fn validate_configuration(
    configuration: &ConnectionConfiguration,
) -> Result<(), ConnectionProviderError> {
    match configuration {
        ConnectionConfiguration::DirectCdp { endpoint }
        | ConnectionConfiguration::NodeInspector { endpoint } => {
            let url =
                Url::parse(endpoint).map_err(|source| ConnectionProviderError::InvalidUrl {
                    kind: "CDP endpoint",
                    value: endpoint.clone(),
                    source,
                })?;
            if !matches!(url.scheme(), "ws" | "wss") {
                return Err(ConnectionProviderError::UnsupportedCdpScheme(
                    url.scheme().to_owned(),
                ));
            }
        }
        ConnectionConfiguration::Process { process_id } => {
            if *process_id == 0 {
                return Err(ConnectionProviderError::InvalidProcessId(*process_id));
            }
        }
        ConnectionConfiguration::ProcessTree { root_pid } => {
            if *root_pid == 0 {
                return Err(ConnectionProviderError::InvalidProcessId(*root_pid));
            }
        }
        ConnectionConfiguration::ScopedProcessTree {
            root_pid,
            target_id,
        } => {
            if *root_pid == 0 {
                return Err(ConnectionProviderError::InvalidProcessId(*root_pid));
            }
            if target_id.is_empty() {
                return Err(ConnectionProviderError::EmptyProcessTreeTarget);
            }
        }
        ConnectionConfiguration::Playwright { url, .. }
        | ConnectionConfiguration::Chrome { url, .. } => {
            let parsed = Url::parse(url).map_err(|source| ConnectionProviderError::InvalidUrl {
                kind: "page URL",
                value: url.clone(),
                source,
            })?;
            if !matches!(parsed.scheme(), "http" | "https" | "file") {
                return Err(ConnectionProviderError::UnsupportedPageScheme(
                    parsed.scheme().to_owned(),
                ));
            }
        }
        ConnectionConfiguration::Node {
            program,
            cwd,
            runtime_executable,
            ..
        } => {
            if program.is_empty() {
                return Err(ConnectionProviderError::EmptyNodeProgram);
            }
            if cwd.is_empty() {
                return Err(ConnectionProviderError::EmptyNodeCwd);
            }
            if runtime_executable.is_empty() {
                return Err(ConnectionProviderError::EmptyNodeExecutable);
            }
        }
        ConnectionConfiguration::Stdio { command, cwd, .. } => {
            if command.is_empty() {
                return Err(ConnectionProviderError::EmptyStdioCommand);
            }
            if cwd.is_empty() {
                return Err(ConnectionProviderError::EmptyStdioCwd);
            }
        }
    }
    Ok(())
}

async fn launch_stdio(
    executable: &str,
    args: &[String],
    cwd: &str,
    environment: &BTreeMap<String, String>,
) -> Result<(Arc<CdpStdioTransport>, Child), ConnectionProviderError> {
    let mut command = Command::new(executable);
    command
        .args(args)
        .current_dir(cwd)
        .envs(environment)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::inherit())
        .kill_on_drop(true);
    configure_provider_process(&mut command);
    let mut child = command
        .spawn()
        .map_err(|source| ConnectionProviderError::Spawn {
            executable: PathBuf::from(executable),
            source,
        })?;
    let stdin = child
        .stdin
        .take()
        .ok_or(ConnectionProviderError::MissingStdioStdin)?;
    let stdout = child
        .stdout
        .take()
        .ok_or(ConnectionProviderError::MissingProviderStdout)?;
    Ok((
        Arc::new(CdpStdioTransport::from_child_stdio(stdout, stdin)),
        child,
    ))
}

async fn launch_playwright(
    url: &str,
    configured_package: Option<&str>,
    channel: &PlaywrightChannel,
    headless: bool,
    ignore_https_errors: bool,
) -> Result<(String, Option<Child>), ConnectionProviderError> {
    let playwright_package = match configured_package {
        Some(path) => {
            let path = PathBuf::from(path);
            if !path.is_file() {
                return Err(ConnectionProviderError::PlaywrightPackageNotFound(path));
            }
            path
        }
        None => find_playwright_package()?,
    };
    let node = env::var_os("DBGJS_NODE").unwrap_or_else(|| "node".into());
    let mut command = Command::new(&node);
    command
        .arg("--input-type=module")
        .arg("--eval")
        .arg(PLAYWRIGHT_HELPER)
        .env("DBGJS_PLAYWRIGHT_PACKAGE", playwright_package)
        .env("DBGJS_PROVIDER_URL", url)
        .env("DBGJS_PROVIDER_CHANNEL", playwright_channel(channel))
        .env(
            "DBGJS_PROVIDER_MODE",
            if headless { "headless" } else { "headed" },
        )
        .env(
            "DBGJS_PROVIDER_IGNORE_HTTPS_ERRORS",
            if ignore_https_errors { "true" } else { "false" },
        )
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .kill_on_drop(true);
    configure_provider_process(&mut command);
    let mut child = command
        .spawn()
        .map_err(|source| ConnectionProviderError::Spawn {
            executable: PathBuf::from(node),
            source,
        })?;
    let stdout = child
        .stdout
        .take()
        .ok_or(ConnectionProviderError::MissingProviderStdout)?;
    let mut lines = BufReader::new(stdout).lines();
    let line = match timeout(PLAYWRIGHT_STARTUP_TIMEOUT, lines.next_line()).await {
        Err(_) => {
            terminate_provider(&mut child).await;
            return Err(ConnectionProviderError::StartupTimeout);
        }

        Ok(Err(error)) => {
            terminate_provider(&mut child).await;
            return Err(ConnectionProviderError::ProviderIo(error));
        }
        Ok(Ok(None)) => {
            let code = child
                .try_wait()
                .ok()
                .flatten()
                .and_then(|status| status.code());
            terminate_provider(&mut child).await;
            return Err(ConnectionProviderError::ProviderExited(code));
        }
        Ok(Ok(Some(line))) => line,
    };
    let ready: PlaywrightReady = match serde_json::from_str(&line) {
        Ok(ready) => ready,
        Err(error) => {
            terminate_provider(&mut child).await;
            return Err(ConnectionProviderError::InvalidHandshake(error));
        }
    };
    if let Some(error) = ready.error {
        terminate_provider(&mut child).await;
        return Err(ConnectionProviderError::ProviderStartup(error));
    }
    let endpoint = match ready.endpoint {
        Some(endpoint) => endpoint,
        None => {
            terminate_provider(&mut child).await;
            return Err(ConnectionProviderError::MissingProviderEndpoint);
        }
    };
    Ok((endpoint, Some(child)))
}

async fn launch_provider<const N: usize>(
    helper: &str,
    provider_name: &'static str,
    capture_events: bool,
    environment: [(&str, String); N],
) -> Result<ProviderLaunch, ConnectionProviderError> {
    let node = env::var_os("DBGJS_NODE").unwrap_or_else(|| "node".into());
    let mut command = Command::new(&node);
    command
        .arg("--input-type=module")
        .arg("--eval")
        .arg(helper)
        .envs(environment)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(if capture_events {
            Stdio::inherit()
        } else {
            Stdio::null()
        })
        .kill_on_drop(true);
    configure_provider_process(&mut command);
    let mut child = command
        .spawn()
        .map_err(|source| ConnectionProviderError::Spawn {
            executable: PathBuf::from(node),
            source,
        })?;
    let stdout = child
        .stdout
        .take()
        .ok_or(ConnectionProviderError::MissingProviderStdout)?;
    let mut lines = BufReader::new(stdout).lines();
    let line = match timeout(PLAYWRIGHT_STARTUP_TIMEOUT, lines.next_line()).await {
        Err(_) => {
            terminate_provider(&mut child).await;
            return Err(ConnectionProviderError::NamedStartupTimeout(provider_name));
        }
        Ok(Err(error)) => {
            terminate_provider(&mut child).await;
            return Err(ConnectionProviderError::ProviderIo(error));
        }
        Ok(Ok(None)) => {
            let code = child
                .try_wait()
                .ok()
                .flatten()
                .and_then(|status| status.code());
            terminate_provider(&mut child).await;
            return Err(ConnectionProviderError::NamedProviderExited(
                provider_name,
                code,
            ));
        }
        Ok(Ok(Some(line))) => line,
    };
    let ready: PlaywrightReady =
        serde_json::from_str(&line).map_err(ConnectionProviderError::InvalidHandshake)?;
    if let Some(error) = ready.error {
        terminate_provider(&mut child).await;
        return Err(ConnectionProviderError::NamedProviderStartup(
            provider_name,
            error,
        ));
    }
    let endpoint = ready
        .endpoint
        .ok_or(ConnectionProviderError::MissingProviderEndpoint)?;
    let events = capture_events.then(|| {
        let (sender, receiver) = mpsc::unbounded_channel();
        tokio::spawn(async move {
            while let Ok(Some(line)) = lines.next_line().await {
                match serde_json::from_str::<ProviderEvent>(&line) {
                    Ok(event) => {
                        if sender.send(event).is_err() {
                            break;
                        }
                    }
                    Err(error) => {
                        eprintln!("ignored invalid {provider_name} provider event: {error}");
                    }
                }
            }
        });
        receiver
    });
    Ok(ProviderLaunch {
        endpoint,
        browser_endpoint: ready.browser_endpoint,
        child,
        events,
    })
}

pub fn find_playwright_package() -> Result<PathBuf, ConnectionProviderError> {
    if let Some(path) = env::var_os("DBGJS_PLAYWRIGHT_PACKAGE") {
        let path = PathBuf::from(path);
        if path.is_file() {
            return Ok(path);
        }
        return Err(ConnectionProviderError::PlaywrightPackageNotFound(path));
    }

    let roots = [
        env::current_dir().ok(),
        env::current_exe()
            .ok()
            .and_then(|path| path.parent().map(ToOwned::to_owned)),
    ];
    for root in roots.into_iter().flatten() {
        for ancestor in root.ancestors() {
            let candidate = ancestor
                .join("node_modules")
                .join("playwright")
                .join("index.mjs");
            if candidate.is_file() {
                return Ok(candidate);
            }
        }
    }
    Err(ConnectionProviderError::PlaywrightPackageNotFound(
        PathBuf::from("node_modules/playwright/index.mjs"),
    ))
}

#[cfg(unix)]
fn configure_provider_process(command: &mut Command) {
    command.process_group(0);
}

#[cfg(windows)]
fn configure_provider_process(command: &mut Command) {
    const CREATE_NEW_PROCESS_GROUP: u32 = 0x0000_0200;
    const CREATE_NO_WINDOW: u32 = 0x0800_0000;
    command.creation_flags(CREATE_NEW_PROCESS_GROUP | CREATE_NO_WINDOW);
}

#[cfg(not(any(unix, windows)))]
fn configure_provider_process(_command: &mut Command) {}

fn playwright_channel(channel: &PlaywrightChannel) -> &'static str {
    match channel {
        PlaywrightChannel::Bundled => "bundled",
        PlaywrightChannel::Chrome => "chrome",
        PlaywrightChannel::ChromeBeta => "chrome-beta",
        PlaywrightChannel::ChromeDev => "chrome-dev",
        PlaywrightChannel::ChromeCanary => "chrome-canary",
        PlaywrightChannel::Msedge => "msedge",
        PlaywrightChannel::MsedgeBeta => "msedge-beta",
        PlaywrightChannel::MsedgeDev => "msedge-dev",
        PlaywrightChannel::MsedgeCanary => "msedge-canary",
    }
}

async fn terminate_provider(child: &mut Child) {
    let process_id = child.id();
    if let Some(mut stdin) = child.stdin.take() {
        let _ = stdin.shutdown().await;
    }
    if timeout(Duration::from_secs(5), child.wait()).await.is_err() {
        terminate_provider_tree(child).await;
        let _ = child.wait().await;
    }
    terminate_provider_group(process_id).await;
}

#[cfg(unix)]
async fn terminate_provider_group(process_id: Option<u32>) {
    if let Some(process_id) = process_id {
        unsafe {
            libc::kill(-(process_id as i32), libc::SIGKILL);
        }
    }
}

#[cfg(windows)]
async fn terminate_provider_group(_process_id: Option<u32>) {}

#[cfg(not(any(unix, windows)))]
async fn terminate_provider_group(_process_id: Option<u32>) {}

#[cfg(unix)]
async fn terminate_provider_tree(child: &mut Child) {
    if let Some(process_id) = child.id() {
        unsafe {
            libc::kill(-(process_id as i32), libc::SIGKILL);
        }
    }
    let _ = child.start_kill();
}

#[cfg(windows)]
async fn terminate_provider_tree(child: &mut Child) {
    if let Some(process_id) = child.id() {
        let _ = Command::new("taskkill")
            .args(["/PID", &process_id.to_string(), "/T", "/F"])
            .status()
            .await;
    }
    let _ = child.start_kill();
}

#[cfg(not(any(unix, windows)))]
async fn terminate_provider_tree(child: &mut Child) {
    let _ = child.start_kill();
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct PlaywrightReady {
    endpoint: Option<String>,
    browser_endpoint: Option<String>,
    error: Option<String>,
}

pub(crate) struct ProviderLaunch {
    pub endpoint: String,
    pub browser_endpoint: Option<String>,
    pub child: Child,
    pub events: Option<mpsc::UnboundedReceiver<ProviderEvent>>,
}

#[derive(Debug, Deserialize)]
#[serde(
    tag = "kind",
    rename_all = "camelCase",
    rename_all_fields = "camelCase"
)]
pub(crate) enum ProviderEvent {
    NodeTarget {
        target_id: String,
        parent_target_id: String,
        target_type: Option<String>,
        title: String,
        url: String,
        endpoint: Option<String>,
        process_id: Option<u32>,
    },
    NodeTargetRemoved {
        target_id: String,
    },
    ScanComplete {
        id: Option<u64>,
    },
    ActivationComplete {
        id: Option<u64>,
        target_id: String,
        endpoint: Option<String>,
        error: Option<String>,
    },
}

#[derive(Debug, thiserror::Error)]
pub enum ConnectionProviderError {
    #[error("invalid {kind} '{value}': {source}")]
    InvalidUrl {
        kind: &'static str,
        value: String,
        source: url::ParseError,
    },
    #[error("direct CDP endpoints must use ws:// or wss://, not {0}://")]
    UnsupportedCdpScheme(String),
    #[error("Playwright page URLs must use http://, https://, or file://, not {0}://")]
    UnsupportedPageScheme(String),
    #[error("process IDs must be greater than zero, got {0}")]
    InvalidProcessId(u32),
    #[error("scoped process-tree target IDs must not be empty")]
    EmptyProcessTreeTarget,
    #[error("failed to launch connection process with {executable}: {source}")]
    Spawn {
        executable: PathBuf,
        source: std::io::Error,
    },
    #[error(
        "Playwright package entrypoint was not found at {0}; install dependencies or set DBGJS_PLAYWRIGHT_PACKAGE"
    )]
    PlaywrightPackageNotFound(PathBuf),
    #[error("Playwright provider did not expose stdout")]
    MissingProviderStdout,
    #[error("Playwright provider startup timed out")]
    StartupTimeout,
    #[error("Playwright provider exited before reporting its CDP endpoint (code {0:?})")]
    ProviderExited(Option<i32>),
    #[error("Playwright provider I/O failed: {0}")]
    ProviderIo(std::io::Error),
    #[error("Playwright provider returned an invalid startup handshake: {0}")]
    InvalidHandshake(serde_json::Error),
    #[error("Playwright provider failed during startup: {0}")]
    ProviderStartup(String),
    #[error("Playwright provider startup handshake did not contain a CDP endpoint")]
    MissingProviderEndpoint,
    #[error("{0} provider startup timed out")]
    NamedStartupTimeout(&'static str),
    #[error("{0} provider exited before reporting its CDP endpoint (code {1:?})")]
    NamedProviderExited(&'static str, Option<i32>),
    #[error("{0} provider failed during startup: {1}")]
    NamedProviderStartup(&'static str, String),
    #[error("Node.js launch program must not be empty")]
    EmptyNodeProgram,
    #[error("Node.js launch cwd must not be empty")]
    EmptyNodeCwd,
    #[error("Node.js runtime executable must not be empty")]
    EmptyNodeExecutable,
    #[error("stdio CDP command must not be empty")]
    EmptyStdioCommand,
    #[error("stdio CDP cwd must not be empty")]
    EmptyStdioCwd,
    #[error("stdio CDP process did not expose stdin")]
    MissingStdioStdin,
    #[error("process tree connection failed: {0}")]
    ProcessTree(String),
    #[error(transparent)]
    Cdp(#[from] CdpRuntimeError),
}

#[cfg(test)]
pub(crate) mod raw_session_tests {
    use super::*;
    use crate::connection::transport::cdp_transport::ManagedCdpTransport;
    use crate::connection::transport::session_transport::CdpEnvelope;
    use linkrpc::prelude::{MessageTransport, TransportError};
    use tokio::sync::Mutex;

    struct IdleTransport(Arc<Mutex<Option<String>>>);

    #[async_trait]
    impl MessageTransport<CdpEnvelope, CdpEnvelope> for IdleTransport {
        async fn send(&self, _: CdpEnvelope) -> Result<(), TransportError> {
            Ok(())
        }
        async fn recv(&self) -> Option<CdpEnvelope> {
            std::future::pending().await
        }
    }

    #[async_trait]
    impl ManagedCdpTransport for IdleTransport {
        fn close_reason(&self) -> Arc<Mutex<Option<String>>> {
            self.0.clone()
        }
        async fn wait_closed(&self) -> String {
            std::future::pending().await
        }
        async fn close(&self) {}
    }

    pub(crate) async fn runtime(generation: u64) -> Arc<ConnectionRuntime> {
        let transport = Arc::new(IdleTransport(Arc::new(Mutex::new(None))));
        let cdp = Arc::new(CdpConnection::connect_transport(transport).await.unwrap());
        let (provider_target_sender, provider_target_events) = mpsc::unbounded_channel();
        Arc::new(ConnectionRuntime {
            cdp,
            provider: None,
            direct_debugger: false,
            connection_generation: generation,
            root_endpoint: "test".into(),
            virtual_root: None,
            raw_sessions: std::sync::Mutex::new(BTreeMap::new()),
            direct_debuggers: std::sync::Mutex::new(BTreeMap::new()),
            direct_debugger_endpoints: std::sync::Mutex::new(BTreeMap::new()),
            direct_debugger_attach_lock: Mutex::new(()),
            provider_target_events: Mutex::new(Some(provider_target_events)),
            provider_target_sender,
            pause_future_children: OnceLock::new(),
        })
    }

    #[tokio::test]
    async fn raw_session_is_scoped_to_owner_and_generation() {
        let first = runtime(1).await;
        let second = runtime(2).await;
        let session = first.cdp.open_raw_session("native".into()).unwrap();
        first.raw_sessions.lock().unwrap().insert(
            ("owner".into(), "native".into()),
            RawAttachment { session, endpoint: None },
        );
        for (runtime, target, id) in [
            (&first, "sibling", "native"),
            (&first, "owner", "unknown"),
            (&second, "owner", "native"),
        ] {
            let result = tokio::time::timeout(
                Duration::from_millis(50),
                runtime.raw_session_request(target, id, "Runtime.evaluate", Value::Null),
            )
            .await
            .expect("unknown, wrong-owner and stale IDs must fail promptly");
            assert_eq!(result.unwrap_err().code, error_codes::INVALID_PARAMS);
        }
        first.retire_raw_session("owner", "native");
        let retired = first.raw_session_request("owner", "native", "Runtime.evaluate", Value::Null).await.unwrap_err();
        assert_eq!(retired.code, error_codes::INVALID_PARAMS);
        first.close().await;
        second.close().await;
    }

    #[tokio::test]
    async fn raw_session_can_reopen_native_id_after_detach() {
        let runtime = runtime(1).await;
        let (events, _) = broadcast::channel(8);
        runtime.register_raw_session("owner", "native", events.subscribe()).unwrap();
        assert!(runtime.register_raw_session("owner", "native", events.subscribe()).is_err());
        events.send(crate::debugger::cdp_runtime::RawCdpEvent {
            session: SessionKey { connection_generation: 1, session_id: "root".into() },
            method: "Target.detachedFromTarget".into(),
            params: serde_json::json!({"sessionId": "native"}),
        }).unwrap();
        runtime.retire_raw_session("owner", "native");
        runtime.register_raw_session("owner", "native", events.subscribe())
            .expect("a new native attachment may reuse a detached session ID");
        tokio::task::yield_now().await;
        assert_eq!(runtime.raw_sessions.lock().unwrap().len(), 1);
        events.send(crate::debugger::cdp_runtime::RawCdpEvent {
            session: SessionKey { connection_generation: 1, session_id: "root".into() },
            method: "Target.detachedFromTarget".into(),
            params: serde_json::json!({"sessionId": "native"}),
        }).unwrap();
        tokio::time::timeout(Duration::from_millis(100), async {
            while runtime.has_raw_session("owner", "native") {
                tokio::task::yield_now().await;
            }
        }).await.expect("native detach must retire only the current attachment");
        runtime.close().await;
    }

    #[tokio::test]
    async fn raw_session_setup_failure_does_not_register_an_unroutable_session() {
        let runtime = runtime(1).await;
        let (events, _) = broadcast::channel(8);
        assert!(runtime.register_raw_session("owner", "$cdp-root", events.subscribe()).is_err());
        assert!(!runtime.has_raw_session("owner", "$cdp-root"));
        runtime.close().await;
    }

    #[tokio::test]
    async fn lost_lifecycle_event_retires_raw_attachment_instead_of_keeping_stale_route() {
        let runtime = runtime(1).await;
        let (events, _) = broadcast::channel(1);
        let receiver = events.subscribe();
        for index in 0..2 {
            events.send(crate::debugger::cdp_runtime::RawCdpEvent {
                session: SessionKey { connection_generation: 1, session_id: "root".into() },
                method: "Target.attachedToTarget".into(),
                params: serde_json::json!({"index": index}),
            }).unwrap();
        }
        runtime.register_raw_session("owner", "native", receiver).unwrap();
        runtime
            .register_raw_session("sibling", "other-native", events.subscribe())
            .unwrap();
        tokio::time::timeout(Duration::from_millis(100), async {
            while runtime.has_raw_session("owner", "native") {
                tokio::task::yield_now().await;
            }
        }).await.expect("lost lifecycle notifications require explicit retirement and reattach");
        assert!(
            runtime.has_raw_session("sibling", "other-native"),
            "losing one attachment's lifecycle must not retire a healthy sibling"
        );
        runtime.close().await;
    }

    #[tokio::test]
    async fn failed_raw_route_does_not_leave_an_unreattachable_live_attachment() {
        let runtime = runtime(1).await;
        let (events, _) = broadcast::channel(8);
        runtime.register_raw_session("owner", "native", events.subscribe()).unwrap();
        let session = runtime
            .raw_sessions
            .lock()
            .unwrap()
            .get(&("owner".into(), "native".into()))
            .unwrap()
            .session
            .clone();
        for _ in 0..256 {
            assert_eq!(
                session
                    .request("Runtime.enable", Value::Null, Duration::ZERO)
                    .await
                    .unwrap_err()
                    .code,
                error_codes::REQUEST_TIMEOUT
            );
        }
        assert_eq!(
            session.request("Runtime.enable", Value::Null, Duration::ZERO)
                .await.unwrap_err().code,
            error_codes::PEER_DISCONNECTED
        );
        tokio::time::timeout(Duration::from_millis(100), async {
            while runtime.has_raw_session("owner", "native") {
                tokio::task::yield_now().await;
            }
        }).await.expect("failed route must retire the registered attachment");
        assert!(runtime.raw_session_request("owner", "native", "Runtime.enable", Value::Null)
            .await.unwrap_err().message.contains("reconnect"));
        let reopen = runtime.register_raw_session("owner", "native", events.subscribe())
            .unwrap_err();
        assert!(reopen.contains("reconnect"), "{reopen}");
        runtime.close().await;
    }
}
