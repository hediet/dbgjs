//! `hubrpc-tokio` — tokio-based transports for hubrpc.
//!
//! Ships the north-star [`NdjsonTransport`] (newline-delimited JSON-RPC framing + `hello`
//! preamble) over any async byte stream, plus Unix-domain-socket constructors. Higher layers
//! (`HubRpcConnection`, the hub client) are runtime-agnostic and sit on top of this.

pub mod connect;
pub mod listener;
pub mod ndjson;
pub mod ws;

pub use connect::{
    connect_to_env_endpoint, connect_to_env_endpoint_with, connect_to_hub, resolve_connection,
    ConnectError, ConnectOptions, ResolvedConnection, RetryOptions, HUBRPC_ENDPOINT_VAR,
    HUBRPC_TOKEN_VAR,
};
pub use listener::HubListener;
pub use ndjson::{NdjsonTransport, Preamble};
pub use ws::{connect_ws, WebSocketTransport};

#[cfg(unix)]
pub mod unix;

#[cfg(windows)]
pub mod windows;
