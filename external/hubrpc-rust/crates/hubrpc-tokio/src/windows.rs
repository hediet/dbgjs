//! Windows named-pipe constructors for the NDJSON transport — the Windows counterpart to
//! [`unix`](crate::unix).
//!
//! Dialing produces an [`NdjsonTransport`] ready for the caller to write its `hello` preamble;
//! accepting produces one ready for the server to [`read_preamble`](NdjsonTransport::read_preamble).
//! Both are thin wrappers over [`NdjsonTransport::from_stream`] — all framing lives there, so this
//! module is just pipe plumbing. Pipe addresses use the `\\.\pipe\…` form (the same path
//! [`parse_endpoint_uri`](hubrpc::prelude::parse_endpoint_uri) yields for an `npipe://` URI).

use std::io;

use tokio::net::windows::named_pipe::{ClientOptions, NamedPipeServer, ServerOptions};

use crate::ndjson::NdjsonTransport;

/// Dial a named pipe and wrap it in an NDJSON transport. Write the `hello` preamble before RPC.
pub async fn connect_pipe(addr: &str) -> io::Result<NdjsonTransport> {
    let client = ClientOptions::new().open(addr)?;
    Ok(NdjsonTransport::from_stream(client))
}

/// A named-pipe listener that yields NDJSON transports per accepted connection.
///
/// Named pipes have no standalone "listener" object: a server instance *is* a connection once a
/// client opens it. So this keeps one pending instance around, hands it out on `accept`, and
/// immediately creates the next — the same "always have one waiting" pattern the tokio docs use.
pub struct NamedPipeHubListener {
    addr: String,
    pending: Option<NamedPipeServer>,
}

impl NamedPipeHubListener {
    /// Bind a listener at `addr` (a `\\.\pipe\…` path). Fails if the pipe name is already taken.
    pub fn bind(addr: &str) -> io::Result<Self> {
        let pending = ServerOptions::new()
            .first_pipe_instance(true)
            .create(addr)?;
        Ok(NamedPipeHubListener {
            addr: addr.to_string(),
            pending: Some(pending),
        })
    }

    /// Accept the next connection. Read its `hello` preamble before serving RPC.
    pub async fn accept(&mut self) -> io::Result<NdjsonTransport> {
        let server = self
            .pending
            .take()
            .expect("listener always holds a pending pipe instance");
        server.connect().await?;
        // Queue the next instance so a follow-up client can connect while this one is served.
        self.pending = Some(ServerOptions::new().create(&self.addr)?);
        Ok(NdjsonTransport::from_stream(server))
    }
}
