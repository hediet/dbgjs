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

pub struct ConnectionRuntime {
    cdp: Arc<CdpConnection>,
    provider: Option<Mutex<Child>>,
}

impl ConnectionRuntime {
    pub async fn connect(
        configuration: &ConnectionConfiguration,
    ) -> Result<Arc<Self>, ConnectionProviderError> {
        let (endpoint, provider) = match configuration {
            ConnectionConfiguration::DirectCdp { endpoint } => (endpoint.clone(), None),
            ConnectionConfiguration::Playwright {
                url,
                channel,
                headless,
                ignore_https_errors,
            } => launch_playwright(url, channel, *headless, *ignore_https_errors).await?,
        };
        let cdp = match CdpConnection::connect(&endpoint).await {
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
        }))
    }

    pub fn root(&self) -> &CdpClient<hubrpc::connection::channel::Channel> {
        self.cdp.root()
    }

    pub fn open_session(&self, session: SessionKey) -> Result<CdpDebuggerSession, CdpRuntimeError> {
        self.cdp.open_session(session)
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
        ConnectionConfiguration::Playwright { url, .. } => {
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
    }
    Ok(())
}

async fn launch_playwright(
    url: &str,
    channel: &PlaywrightChannel,
    headless: bool,
    ignore_https_errors: bool,
) -> Result<(String, Option<Child>), ConnectionProviderError> {
    let playwright_package = find_playwright_package()?;
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
        let _ = child.wait().await;
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
    #[error(transparent)]
    Cdp(#[from] CdpRuntimeError),
}
