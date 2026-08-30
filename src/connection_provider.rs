use std::collections::BTreeMap;
use std::env;
use std::path::PathBuf;
use std::process::Stdio;
use std::sync::{Arc, Weak};
use std::time::Duration;

use serde::Deserialize;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::process::{Child, Command};
use tokio::sync::{Mutex, mpsc};
use tokio::time::timeout;
use url::Url;

use crate::cdp::CdpClient;
use crate::cdp_runtime::{CdpConnection, CdpDebuggerSession, CdpRuntimeError, RootCdpEvent};
use crate::debugger_engine::SessionKey;
use crate::electron_renderer_transport::ElectronRendererBridge;
use crate::playwright_proxy::PlaywrightCdpSource;
use crate::service_api::{
    CdpStdioTopology, ConnectionConfiguration, PlaywrightChannel, TargetSnapshot,
};
use crate::stdio_transport::CdpStdioTransport;

const PLAYWRIGHT_STARTUP_TIMEOUT: Duration = Duration::from_secs(30);
const PLAYWRIGHT_HELPER: &str = include_str!("providers/playwright.mjs");
const CHROME_HELPER: &str = include_str!("providers/chrome.mjs");
const NODE_HELPER: &str = include_str!("providers/node.mjs");
const PROCESS_TREE_HELPER: &str = include_str!("providers/process_tree.mjs");

pub struct ConnectionRuntime {
    cdp: Arc<CdpConnection>,
    provider: Option<Mutex<Child>>,
    direct_debugger: bool,
    connection_generation: u64,
    root_endpoint: String,
    direct_debuggers: std::sync::Mutex<BTreeMap<String, Arc<CdpConnection>>>,
    direct_debugger_endpoints: std::sync::Mutex<BTreeMap<String, String>>,
    renderer_processes: std::sync::Mutex<BTreeMap<String, u32>>,
    renderer_bridge: Option<Arc<ElectronRendererBridge>>,
    direct_debugger_attach_lock: Mutex<()>,
    provider_target_events: Mutex<Option<mpsc::UnboundedReceiver<ProviderTargetEvent>>>,
    provider_target_sender: mpsc::UnboundedSender<ProviderTargetEvent>,
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
        let (endpoint, provider, direct_debugger, provider_events, renderer_bridge) =
            match configuration {
                ConnectionConfiguration::DirectCdp { endpoint } => {
                    (endpoint.clone(), None, false, None, false)
                }
                ConnectionConfiguration::NodeInspector { endpoint } => {
                    (endpoint.clone(), None, true, None, false)
                }
                ConnectionConfiguration::Process { process_id } => {
                    let launch = launch_provider(
                        PROCESS_TREE_HELPER,
                        "process",
                        true,
                        [
                            ("JSDBG_PROCESS_ROOT_PID", process_id.to_string()),
                            ("JSDBG_PROCESS_MODE", "single".to_owned()),
                        ],
                    )
                    .await?;
                    (
                        launch.endpoint,
                        Some(launch.child),
                        true,
                        launch.events,
                        false,
                    )
                }
                ConnectionConfiguration::ProcessTree { root_pid } => {
                    let launch = launch_provider(
                        PROCESS_TREE_HELPER,
                        "process tree",
                        true,
                        [
                            ("JSDBG_PROCESS_ROOT_PID", root_pid.to_string()),
                            ("JSDBG_PROCESS_MODE", "tree".to_owned()),
                        ],
                    )
                    .await?;
                    (
                        launch.endpoint,
                        Some(launch.child),
                        true,
                        launch.events,
                        true,
                    )
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
                    (endpoint, provider, false, None, false)
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
                            ("JSDBG_PROVIDER_URL", url.clone()),
                            ("JSDBG_CHROME_EXECUTABLE", executable.clone()),
                            (
                                "JSDBG_PROVIDER_MODE",
                                if *headless { "headless" } else { "headed" }.to_owned(),
                            ),
                            (
                                "JSDBG_CHROME_USER_DATA_DIR",
                                user_data_dir.clone().unwrap_or_default(),
                            ),
                            (
                                "JSDBG_CHROME_ARGS",
                                serde_json::to_string(args)
                                    .expect("Chrome arguments always serialize"),
                            ),
                        ],
                    )
                    .await?;
                    (launch.endpoint, Some(launch.child), false, None, false)
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
                            ("JSDBG_NODE_PROGRAM", program.clone()),
                            (
                                "JSDBG_NODE_ARGS",
                                serde_json::to_string(args)
                                    .expect("Node.js arguments always serialize"),
                            ),
                            ("JSDBG_NODE_CWD", cwd.clone()),
                            ("JSDBG_NODE_EXECUTABLE", runtime_executable.clone()),
                            (
                                "JSDBG_NODE_RUNTIME_ARGS",
                                serde_json::to_string(runtime_args)
                                    .expect("Node.js runtime arguments always serialize"),
                            ),
                            (
                                "JSDBG_NODE_ENV",
                                serde_json::to_string(env)
                                    .expect("Node.js environment always serializes"),
                            ),
                        ],
                    )
                    .await?;
                    (
                        launch.endpoint,
                        Some(launch.child),
                        true,
                        launch.events,
                        false,
                    )
                }
                ConnectionConfiguration::Stdio { .. } => unreachable!(),
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
        let (provider_target_sender, provider_target_events) = mpsc::unbounded_channel();
        let renderer_bridge = if renderer_bridge {
            match ElectronRendererBridge::install(cdp.clone()).await {
                Ok(bridge) => Some(bridge),
                Err(error) => {
                    eprintln!("Electron renderer bridge is unavailable: {error}");
                    None
                }
            }
        } else {
            None
        };
        let runtime = Arc::new(Self {
            cdp,
            provider: provider.map(Mutex::new),
            direct_debugger,
            connection_generation,
            root_endpoint: endpoint,
            direct_debuggers: std::sync::Mutex::new(BTreeMap::new()),
            direct_debugger_endpoints: std::sync::Mutex::new(BTreeMap::new()),
            renderer_processes: std::sync::Mutex::new(BTreeMap::new()),
            renderer_bridge,
            direct_debugger_attach_lock: Mutex::new(()),
            provider_target_events: Mutex::new(Some(provider_target_events)),
            provider_target_sender,
        });
        if let Some(events) = provider_events {
            runtime.supervise_provider_events(events, connection_generation);
        }
        Ok(runtime)
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
            direct_debuggers: std::sync::Mutex::new(BTreeMap::new()),
            direct_debugger_endpoints: std::sync::Mutex::new(BTreeMap::new()),
            renderer_processes: std::sync::Mutex::new(BTreeMap::new()),
            renderer_bridge: None,
            direct_debugger_attach_lock: Mutex::new(()),
            provider_target_events: Mutex::new(Some(provider_target_events)),
            provider_target_sender,
        }))
    }

    pub fn root(&self) -> &CdpClient<hubrpc::connection::channel::Channel> {
        self.cdp.root()
    }

    pub fn open_session(&self, session: SessionKey) -> Result<CdpDebuggerSession, CdpRuntimeError> {
        self.cdp.open_session(session)
    }

    pub async fn take_direct_debugger_session(
        self: &Arc<Self>,
        target_id: &str,
        force: bool,
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
            let renderer_process_id = self
                .renderer_processes
                .lock()
                .unwrap()
                .get(target_id)
                .copied();
            let Some(renderer_process_id) = renderer_process_id else {
                return Ok(None);
            };
            let bridge = self.renderer_bridge.as_ref().ok_or_else(|| {
                CdpRuntimeError::Transport(
                    "Electron renderer bridge is unavailable for this process tree".into(),
                )
            })?;
            let target = bridge
                .target_for_process(renderer_process_id)
                .await
                .map_err(CdpRuntimeError::Transport)?;
            let (transport, stole_external_owner) = bridge
                .attach(target_id.to_owned(), &target, force)
                .await
                .map_err(CdpRuntimeError::Transport)?;
            let connection = Arc::new(
                CdpConnection::connect_root_debugger_transport(
                    transport,
                    self.connection_generation,
                    target_id.to_owned(),
                )
                .await?,
            );
            let session = connection
                .take_root_debugger_session()
                .expect("a new renderer debugger connection has a root session");
            self.direct_debuggers
                .lock()
                .unwrap()
                .insert(target_id.to_owned(), connection.clone());
            supervise_direct_debugger(Arc::downgrade(self), target_id.to_owned(), connection);
            return Ok(Some(DirectDebuggerAttachment {
                session,
                stole_external_owner,
            }));
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

    pub fn is_direct_debugger(&self) -> bool {
        self.direct_debugger
    }

    pub fn playwright_cdp_source(&self) -> Result<PlaywrightCdpSource, ConnectionProviderError> {
        if self.direct_debugger {
            return Err(ConnectionProviderError::PlaywrightRequiresBrowserRoot);
        }
        if self.root_endpoint.starts_with("stdio:") {
            return Err(ConnectionProviderError::PlaywrightRequiresWebSocketRoot);
        }
        Ok(PlaywrightCdpSource::BrowserRoot {
            endpoint: self.root_endpoint.clone(),
        })
    }

    pub fn renderer_process_id(&self, target_id: &str) -> Option<u32> {
        self.renderer_processes
            .lock()
            .unwrap()
            .get(target_id)
            .copied()
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
            Result<RootCdpEvent, crate::cdp_runtime::CdpRuntimeEventError>,
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
        let direct_debuggers = std::mem::take(&mut *self.direct_debuggers.lock().unwrap());
        for connection in direct_debuggers.into_values() {
            connection.close().await;
        }
        if let Some(bridge) = &self.renderer_bridge {
            bridge.dispose().await;
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
                        runtime
                            .renderer_processes
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
                    ProviderEvent::RendererTarget {
                        target_id,
                        parent_target_id,
                        target_type,
                        mut title,
                        mut url,
                        renderer_process_id,
                    } => {
                        runtime
                            .renderer_processes
                            .lock()
                            .unwrap()
                            .insert(target_id.clone(), renderer_process_id);
                        if let Some(bridge) = &runtime.renderer_bridge
                            && let Ok(target) = bridge.target_for_process(renderer_process_id).await
                        {
                            title = target.title;
                            url = target.url;
                        }
                        let _ = runtime
                            .provider_target_sender
                            .send(ProviderTargetEvent::Upsert(node_target_snapshot(
                                target_id,
                                parent_target_id,
                                target_type,
                                title,
                                url,
                            )));
                    }
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
    let node = env::var_os("JSDBG_NODE").unwrap_or_else(|| "node".into());
    let mut command = Command::new(&node);
    command
        .arg("--input-type=module")
        .arg("--eval")
        .arg(PLAYWRIGHT_HELPER)
        .env("JSDBG_PLAYWRIGHT_PACKAGE", playwright_package)
        .env("JSDBG_PROVIDER_URL", url)
        .env("JSDBG_PROVIDER_CHANNEL", playwright_channel(channel))
        .env(
            "JSDBG_PROVIDER_MODE",
            if headless { "headless" } else { "headed" },
        )
        .env(
            "JSDBG_PROVIDER_IGNORE_HTTPS_ERRORS",
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
    let node = env::var_os("JSDBG_NODE").unwrap_or_else(|| "node".into());
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
        child,
        events,
    })
}

pub fn find_playwright_package() -> Result<PathBuf, ConnectionProviderError> {
    if let Some(path) = env::var_os("JSDBG_PLAYWRIGHT_PACKAGE") {
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
    error: Option<String>,
}

struct ProviderLaunch {
    endpoint: String,
    child: Child,
    events: Option<mpsc::UnboundedReceiver<ProviderEvent>>,
}

#[derive(Debug, Deserialize)]
#[serde(
    tag = "kind",
    rename_all = "camelCase",
    rename_all_fields = "camelCase"
)]
enum ProviderEvent {
    NodeTarget {
        target_id: String,
        parent_target_id: String,
        target_type: Option<String>,
        title: String,
        url: String,
        endpoint: Option<String>,
    },
    NodeTargetRemoved {
        target_id: String,
    },
    RendererTarget {
        target_id: String,
        parent_target_id: String,
        target_type: Option<String>,
        title: String,
        url: String,
        renderer_process_id: u32,
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
    #[error("failed to launch connection process with {executable}: {source}")]
    Spawn {
        executable: PathBuf,
        source: std::io::Error,
    },
    #[error(
        "Playwright package entrypoint was not found at {0}; install dependencies or set JSDBG_PLAYWRIGHT_PACKAGE"
    )]
    PlaywrightPackageNotFound(PathBuf),
    #[error(
        "Playwright currently requires a browser-root CDP connection; direct Node and Electron renderer targets are not yet supported"
    )]
    PlaywrightRequiresBrowserRoot,
    #[error(
        "Playwright currently requires a WebSocket browser-root CDP connection; stdio browser connections are not yet supported"
    )]
    PlaywrightRequiresWebSocketRoot,
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
    #[error(transparent)]
    Cdp(#[from] CdpRuntimeError),
}
