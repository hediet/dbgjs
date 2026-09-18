//! Connection layer.
//!
//! - [`channel`] — request/response correlation over a `MessageTransport` (phase 1).
//! - [`endpoint`] — `parse_endpoint_uri`/`format_endpoint_uri`: `LINKRPC_ENDPOINT` URI grammar.
//! - [`interface_def`] — `InterfaceDefinition`: typed members → wire schema + content hash.
//! - [`dispatch`] — `CallCtx` + the `InterfaceHandler` trait a server implements.
//! - [`reflection`] — the `hubrpc.defaults` / `directory` / `schemas` interfaces.
//! - [`hub_connection`] — `LinkRpcConnection`: `::`-routed registry on top of a `Channel`.

pub mod channel;
pub mod dispatch;
pub mod endpoint;
pub mod hub_connection;
pub mod interface_def;
pub mod reflection;
