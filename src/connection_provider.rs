use std::env;
use std::path::PathBuf;
use std::process::Stdio;
use std::sync::Arc;
use std::time::Duration;

use serde::Deserialize;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::process::{Child, Command};
use tokio::sync::Mutex;
use tokio::time::timeout;
use url::Url;

use crate::cdp::CdpClient;
use crate::cdp_runtime::{CdpConnection, CdpDebuggerSession, CdpRuntimeError, RootCdpEvent};
use crate::debugger_engine::SessionKey;
use crate::service_api::{ConnectionConfiguration, PlaywrightChannel};

const PLAYWRIGHT_STARTUP_TIMEOUT: Duration = Duration::from_secs(30);
const PLAYWRIGHT_HELPER: &str = include_str!("providers/playwright.mjs");
const CHROME_HELPER: &str = include_str!("providers/chrome.mjs");
const NODE_HELPER: &str = include_str!("providers/node.mjs");

pub struct ConnectionRuntime {
    cdp: Arc<CdpConnection>,
    provider: Option<Mutex<Child>>,
    direct_debugger: bool,
}

impl ConnectionRuntime {
    pub async fn connect(
        configuration: &ConnectionConfiguration,
        connection_generation: u64,
    ) -> Result<Arc<Self>, ConnectionProviderError> {
        let (endpoint, provider, direct_debugger) = match configuration {
            ConnectionConfiguration::DirectCdp { endpoint } => (endpoint.clone(), None, false),
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
                (endpoint, provider, false)
            }
            ConnectionConfiguration::Chrome {
                url,
                executable,
                headless,
                user_data_dir,
                args,
            } => {
                let (endpoint, provider) = launch_provider(
                    CHROME_HELPER,
                    "Chrome",
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
                            serde_json::to_string(args).expect("Chrome arguments always serialize"),
                        ),
                    ],
                )
                .await?;
                (endpoint, provider, false)
            }
            ConnectionConfiguration::Node {
                program,
                args,
                cwd,
                runtime_executable,
                runtime_args,
                env,
            } => {
                let (endpoint, provider) = launch_provider(
                    NODE_HELPER,
                    "Node.js",
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
                (endpoint, provider, true)
            }
        };
        let cdp = match if direct_debugger {
            CdpConnection::connect_root_debugger(&endpoint, connection_generation).await
        } else {
            CdpConnection::connect(&endpoint).await
        } {
            Ok(cdp) => Arc::new(cdp),
            Err(error) => {
                if let Some(mut provider) = provider {
                    terminate_provider(&mut provider).await;
                }
                return Err(error.into());
            }
        };
        Ok(Arc::new(Self {
            cdp,
            provider: provider.map(Mutex::new),
            direct_debugger,
        }))
    }

    pub fn root(&self) -> &CdpClient<hubrpc::connection::channel::Channel> {
        self.cdp.root()
    }

    pub fn open_session(&self, session: SessionKey) -> Result<CdpDebuggerSession, CdpRuntimeError> {
        self.cdp.open_session(session)
    }

    pub fn take_root_debugger_session(&self) -> Option<CdpDebuggerSession> {
        self.cdp.take_root_debugger_session()
    }

    pub fn is_direct_debugger(&self) -> bool {
        self.direct_debugger
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

    pub async fn close(&self) {
        self.cdp.close().await;
        if let Some(provider) = &self.provider {
            let mut provider = provider.lock().await;
            terminate_provider(&mut provider).await;
        }
    }
}

pub fn validate_configuration(
    configuration: &ConnectionConfiguration,
) -> Result<(), ConnectionProviderError> {
    match configuration {
        ConnectionConfiguration::DirectCdp { endpoint } => {
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
    }
    Ok(())
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
    environment: [(&str, String); N],
) -> Result<(String, Option<Child>), ConnectionProviderError> {
    let node = env::var_os("JSDBG_NODE").unwrap_or_else(|| "node".into());
    let mut command = Command::new(&node);
    command
        .arg("--input-type=module")
        .arg("--eval")
        .arg(helper)
        .envs(environment)
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
    Ok((endpoint, Some(child)))
}

fn find_playwright_package() -> Result<PathBuf, ConnectionProviderError> {
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
    command.creation_flags(CREATE_NEW_PROCESS_GROUP);
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
    if let Some(mut stdin) = child.stdin.take() {
        let _ = stdin.shutdown().await;
    }
    if timeout(Duration::from_secs(5), child.wait()).await.is_err() {
        terminate_provider_tree(child).await;
        let _ = child.wait().await;
    }
}

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
    #[error("failed to launch Playwright provider with {executable}: {source}")]
    Spawn {
        executable: PathBuf,
        source: std::io::Error,
    },
    #[error(
        "Playwright package entrypoint was not found at {0}; install dependencies or set JSDBG_PLAYWRIGHT_PACKAGE"
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
    #[error(transparent)]
    Cdp(#[from] CdpRuntimeError),
}
