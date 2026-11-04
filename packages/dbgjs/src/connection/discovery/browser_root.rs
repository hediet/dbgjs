//! Observes a Chromium browser root without opening a debugging connection.
//!
//! Chrome 144+ can expose request-based remote debugging: the browser listens locally, but every
//! WebSocket client must be approved in a native "Allow remote debugging?" dialog, and the HTTP
//! discovery endpoints (`/json/version`, `/json/list`) answer 404. Connecting merely to list tabs
//! would therefore show that dialog. This module only reads the profile's `DevToolsActivePort`
//! file, probes the HTTP endpoint and, on Windows, reads window titles, none of which prompt.

use std::path::Path;
use std::time::Duration;

use crate::api::service_api::{BrowserRemoteDebugging, BrowserRootSnapshot};

const PROBE_TIMEOUT: Duration = Duration::from_secs(2);

pub async fn observe_browser_root(
    process_id: u32,
    user_data_dir: Option<String>,
    remote_debugging_port: Option<String>,
) -> BrowserRootSnapshot {
    BrowserRootSnapshot {
        remote_debugging: remote_debugging_state(user_data_dir, remote_debugging_port).await,
        window_titles: window_titles(process_id).await,
    }
}

async fn remote_debugging_state(
    user_data_dir: Option<String>,
    remote_debugging_port: Option<String>,
) -> BrowserRemoteDebugging {
    let user_data_dir = user_data_dir.filter(|directory| Path::new(directory).is_absolute());
    // Chromium writes the bound port to the profile for both the chrome://inspect setting and
    // `--remote-debugging-port=0`; a fixed port on the command line also works without it.
    let port = match &user_data_dir {
        Some(directory) => read_devtools_active_port(Path::new(directory)).await,
        None => None,
    }
    .or_else(|| {
        remote_debugging_port
            .and_then(|port| port.parse::<u16>().ok())
            .filter(|port| *port != 0)
    });
    let unreachable = || match &user_data_dir {
        Some(directory) => BrowserRemoteDebugging::Off {
            user_data_dir: directory.clone(),
        },
        None => BrowserRemoteDebugging::Unknown,
    };
    let Some(port) = port else {
        return unreachable();
    };
    match probe_http_discovery(port).await {
        HttpDiscovery::Open => BrowserRemoteDebugging::Open { port },
        HttpDiscovery::Withheld => BrowserRemoteDebugging::ApprovalRequired { port },
        HttpDiscovery::Unreachable => unreachable(),
    }
}

async fn read_devtools_active_port(user_data_dir: &Path) -> Option<u16> {
    let contents = tokio::fs::read_to_string(user_data_dir.join("DevToolsActivePort"))
        .await
        .ok()?;
    parse_devtools_active_port(&contents)
}

fn parse_devtools_active_port(contents: &str) -> Option<u16> {
    let mut lines = contents.lines();
    let port = lines.next()?.trim();
    let path = lines.next()?.trim();
    if !port.bytes().all(|byte| byte.is_ascii_digit())
        || !path.starts_with("/devtools/browser/")
    {
        return None;
    }
    port.parse::<u16>().ok().filter(|port| *port != 0)
}

#[derive(Debug, PartialEq, Eq)]
enum HttpDiscovery {
    /// `/json/version` answers: a classic debugging port that needs no approval.
    Open,
    /// The listener refuses HTTP discovery, as request-based remote debugging does.
    Withheld,
    /// Nothing usable listens; the profile's port file is stale.
    Unreachable,
}

async fn probe_http_discovery(port: u16) -> HttpDiscovery {
    let Ok(client) = reqwest::Client::builder()
        .no_proxy()
        .timeout(PROBE_TIMEOUT)
        .build()
    else {
        return HttpDiscovery::Unreachable;
    };
    match client
        .get(format!("http://127.0.0.1:{port}/json/version"))
        .send()
        .await
    {
        Ok(response) if response.status().is_success() => HttpDiscovery::Open,
        Ok(response) if response.status() == reqwest::StatusCode::NOT_FOUND => {
            HttpDiscovery::Withheld
        }
        _ => HttpDiscovery::Unreachable,
    }
}

#[cfg(windows)]
async fn window_titles(process_id: u32) -> Vec<String> {
    tokio::task::spawn_blocking(move || visible_window_titles(process_id))
        .await
        .unwrap_or_default()
}

#[cfg(not(windows))]
async fn window_titles(_process_id: u32) -> Vec<String> {
    Vec::new()
}

/// Titles of the visible, titled top-level windows owned by `process_id`. Chromium's browser
/// process owns its windows, and each title is that window's active tab title.
#[cfg(windows)]
fn visible_window_titles(process_id: u32) -> Vec<String> {
    use windows_sys::Win32::Foundation::{HWND, LPARAM};
    use windows_sys::Win32::UI::WindowsAndMessaging::{
        EnumWindows, GetWindowTextLengthW, GetWindowTextW, GetWindowThreadProcessId,
        IsWindowVisible,
    };
    use windows_sys::core::BOOL;

    struct Search {
        process_id: u32,
        titles: Vec<String>,
    }

    unsafe extern "system" fn visit(window: HWND, search: LPARAM) -> BOOL {
        // SAFETY: `search` is the `&mut Search` passed to `EnumWindows` below, which outlives
        // the synchronous enumeration.
        let search = unsafe { &mut *(search as *mut Search) };
        let mut owner = 0;
        // SAFETY: `window` is a live handle supplied by `EnumWindows`.
        unsafe { GetWindowThreadProcessId(window, &mut owner) };
        if owner != search.process_id || unsafe { IsWindowVisible(window) } == 0 {
            return 1;
        }
        let length = unsafe { GetWindowTextLengthW(window) };
        if length > 0 {
            let mut buffer = vec![0u16; length as usize + 1];
            let copied =
                unsafe { GetWindowTextW(window, buffer.as_mut_ptr(), buffer.len() as i32) };
            if copied > 0 {
                search
                    .titles
                    .push(String::from_utf16_lossy(&buffer[..copied as usize]));
            }
        }
        1
    }

    let mut search = Search {
        process_id,
        titles: Vec::new(),
    };
    // SAFETY: the callback only dereferences `search`, which is live for the whole call.
    unsafe { EnumWindows(Some(visit), &mut search as *mut Search as LPARAM) };
    search.titles
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::net::TcpListener;

    #[test]
    fn parses_only_well_formed_port_files() {
        assert_eq!(parse_devtools_active_port("9222\n/devtools/browser/a-1\n"), Some(9222));
        assert_eq!(parse_devtools_active_port("9222\r\n/devtools/browser/a-1\r\n"), Some(9222));
        for contents in ["", "9222", "0\n/devtools/browser/a", "65536\n/devtools/browser/a",
            "x\n/devtools/browser/a", "9222\n/json/version"]
        {
            assert_eq!(parse_devtools_active_port(contents), None, "{contents:?}");
        }
    }

    async fn http_server(status: &'static str) -> u16 {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        tokio::spawn(async move {
            while let Ok((mut stream, _)) = listener.accept().await {
                let mut request = [0; 1024];
                let _ = stream.read(&mut request).await;
                let body = if status.starts_with("200") {
                    r#"{"webSocketDebuggerUrl":"ws://127.0.0.1/devtools/browser/x"}"#
                } else {
                    ""
                };
                let _ = stream
                    .write_all(
                        format!(
                            "HTTP/1.1 {status}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                            body.len()
                        )
                        .as_bytes(),
                    )
                    .await;
            }
        });
        port
    }

    #[tokio::test]
    async fn classifies_profile_endpoints_without_connecting_a_debugger() {
        let profile = tempfile::tempdir().unwrap();
        let directory = profile.path().to_string_lossy().into_owned();
        let state = |port: Option<&str>| {
            let directory = directory.clone();
            let port = port.map(str::to_owned);
            async move { remote_debugging_state(Some(directory), port).await }
        };

        assert_eq!(
            state(None).await,
            BrowserRemoteDebugging::Off {
                user_data_dir: directory.clone()
            }
        );
        let withheld = http_server("404 Not Found").await;
        std::fs::write(
            profile.path().join("DevToolsActivePort"),
            format!("{withheld}\n/devtools/browser/id\n"),
        )
        .unwrap();
        assert_eq!(
            state(None).await,
            BrowserRemoteDebugging::ApprovalRequired { port: withheld }
        );
        let open = http_server("200 OK").await;
        std::fs::write(
            profile.path().join("DevToolsActivePort"),
            format!("{open}\n/devtools/browser/id\n"),
        )
        .unwrap();
        assert_eq!(state(None).await, BrowserRemoteDebugging::Open { port: open });

        let stale = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let stale_port = stale.local_addr().unwrap().port();
        drop(stale);
        std::fs::write(
            profile.path().join("DevToolsActivePort"),
            format!("{stale_port}\n/devtools/browser/id\n"),
        )
        .unwrap();
        assert!(matches!(state(None).await, BrowserRemoteDebugging::Off { .. }));
    }

    #[tokio::test]
    async fn without_a_profile_only_an_explicit_port_is_observable() {
        assert_eq!(
            remote_debugging_state(None, None).await,
            BrowserRemoteDebugging::Unknown
        );
        assert_eq!(
            remote_debugging_state(Some("relative".to_owned()), None).await,
            BrowserRemoteDebugging::Unknown
        );
        let open = http_server("200 OK").await;
        assert_eq!(
            remote_debugging_state(None, Some(open.to_string())).await,
            BrowserRemoteDebugging::Open { port: open }
        );
        assert_eq!(
            remote_debugging_state(None, Some("0".to_owned())).await,
            BrowserRemoteDebugging::Unknown
        );
    }
}
