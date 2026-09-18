//! A cross-platform endpoint listener: a Unix-domain socket on Unix, a Windows named pipe on
//! Windows. Both yield the same [`NdjsonTransport`], so callers (and the examples) bind and accept
//! identically without any `#[cfg]` of their own — the OS-appropriate address is the only
//! difference, and that is just a string.

use std::io;

use crate::ndjson::NdjsonTransport;

/// A listener over the platform's local IPC primitive (Unix socket or Windows named pipe).
///
/// `address` is a filesystem path on Unix (e.g. `/tmp/app.sock`) and a `\\.\pipe\…` name on
/// Windows; [`format_endpoint_uri`](linkrpc::prelude::format_endpoint_uri) turns either into the
/// matching `unix:` / `npipe://` URI for `LINKRPC_ENDPOINT`.
pub struct HubListener {
    #[cfg(unix)]
    inner: crate::unix::UnixHubListener,
    #[cfg(windows)]
    inner: crate::windows::NamedPipeHubListener,
}

impl HubListener {
    /// Bind a listener at `address` (a Unix socket path or a `\\.\pipe\…` name).
    pub fn bind(address: &str) -> io::Result<Self> {
        #[cfg(unix)]
        {
            Ok(HubListener {
                inner: crate::unix::UnixHubListener::bind(address)?,
            })
        }
        #[cfg(windows)]
        {
            Ok(HubListener {
                inner: crate::windows::NamedPipeHubListener::bind(address)?,
            })
        }
        #[cfg(not(any(unix, windows)))]
        {
            let _ = address;
            Err(io::Error::new(
                io::ErrorKind::Unsupported,
                "local IPC endpoints require Unix or Windows",
            ))
        }
    }

    /// Accept the next connection. Read its `hello` preamble before serving RPC.
    pub async fn accept(&mut self) -> io::Result<NdjsonTransport> {
        #[cfg(any(unix, windows))]
        {
            self.inner.accept().await
        }
        #[cfg(not(any(unix, windows)))]
        {
            Err(io::Error::new(
                io::ErrorKind::Unsupported,
                "local IPC endpoints require Unix or Windows",
            ))
        }
    }
}
