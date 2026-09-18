//! Server-side dispatch primitives: the per-call [`CallCtx`] handed to handlers, and the
//! [`InterfaceHandler`] trait an implementation registers for an interface.
//!
//! The macro's `serve()` adapter will generate an [`InterfaceHandler`] that decodes params into
//! typed Rust values and encodes results back to JSON; hand-written handlers implement it directly.

use async_trait::async_trait;

use crate::connection::interface_def::InterfaceDefinition;
use crate::protocol::json_value::JsonValue;
use crate::protocol::jsonrpc::{JsonRpcError, RequestId};

/// Per-call context passed to every handler invocation.
///
/// Carries the wire request id (for requests) and is the seam where attested caller identity and
/// a cancellation token will be attached in later milestones (see `docs/examples.md` §4).
#[derive(Debug, Clone, Default)]
pub struct CallCtx {
    request_id: Option<RequestId>,
}

impl CallCtx {
    pub fn new(request_id: Option<RequestId>) -> Self {
        CallCtx { request_id }
    }

    /// The wire request id, if this call is a request (notifications have none).
    pub fn request_id(&self) -> Option<&RequestId> {
        self.request_id.as_ref()
    }
}

/// The server-side implementation of one interface. Dispatches by local member name.
///
/// Implementors own param decoding / result encoding (and any application-level validation).
/// A request to an unknown member should return [`JsonRpcError`] with
/// [`crate::protocol::jsonrpc::error_codes::METHOD_NOT_FOUND`]; an unknown notification is dropped.
#[async_trait]
pub trait InterfaceHandler: Send + Sync {
    /// Handle a request member, returning the JSON result or a JSON-RPC error.
    async fn handle_request(
        &self,
        member: &str,
        params: JsonValue,
        ctx: CallCtx,
    ) -> Result<JsonValue, JsonRpcError>;

    /// Handle a notification member. Errors are swallowed (notifications have no response).
    async fn handle_notification(&self, _member: &str, _params: JsonValue, _ctx: CallCtx) {}
}

/// A handler that also knows its own [`InterfaceDefinition`].
///
/// The `#[link_rpc_interface]` macro implements this for the generated `…Server` adapter, letting
/// [`LinkRpcConnection::register_service`](crate::connection::hub_connection::LinkRpcConnection::register_service)
/// register an interface from its handler alone — no separate `interface()` argument.
pub trait ServiceExport: InterfaceHandler {
    /// The interface definition this handler serves (its content hash is the interface identity).
    fn interface() -> InterfaceDefinition
    where
        Self: Sized;
}
