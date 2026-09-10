//! A minimal client-call abstraction used by generated interface clients.
//!
//! [`RpcCall`] is the seam a generated typed client drives: it issues a request
//! or a notification addressed by a **raw wire method name**. Because the method
//! name is opaque to this trait, generated clients can address hubrpc members
//! (`interface::member`, `service::interface::member`) *or* bare, root-addressed
//! method names such as the `Domain.method` names a CDP channel expects.
//!
//! Both [`Channel`](crate::connection::channel::Channel) and
//! [`HubRpcConnection`](crate::connection::hub_connection::HubRpcConnection)
//! implement it, so a generated client can drive either directly.

use async_trait::async_trait;

use crate::protocol::json_value::JsonValue;
use crate::protocol::jsonrpc::JsonRpcError;

/// Issues request/notification calls by raw wire method name.
///
/// Blanket-implemented for references, boxes and arcs so a generated client can
/// hold a `&C`, `Arc<C>`, or `Box<dyn RpcCall>` interchangeably.
#[async_trait]
pub trait RpcCall: Send + Sync {
    /// Issue a request addressed by `method` and await its result.
    async fn call(&self, method: &str, params: JsonValue) -> Result<JsonValue, JsonRpcError>;

    /// Fire a notification addressed by `method` (no response expected).
    async fn notify(&self, method: &str, params: JsonValue) -> Result<(), JsonRpcError>;
}

#[async_trait]
impl<T: RpcCall + ?Sized> RpcCall for &T {
    async fn call(&self, method: &str, params: JsonValue) -> Result<JsonValue, JsonRpcError> {
        (**self).call(method, params).await
    }
    async fn notify(&self, method: &str, params: JsonValue) -> Result<(), JsonRpcError> {
        (**self).notify(method, params).await
    }
}

#[async_trait]
impl<T: RpcCall + ?Sized> RpcCall for std::sync::Arc<T> {
    async fn call(&self, method: &str, params: JsonValue) -> Result<JsonValue, JsonRpcError> {
        (**self).call(method, params).await
    }
    async fn notify(&self, method: &str, params: JsonValue) -> Result<(), JsonRpcError> {
        (**self).notify(method, params).await
    }
}

#[async_trait]
impl<T: RpcCall + ?Sized> RpcCall for Box<T> {
    async fn call(&self, method: &str, params: JsonValue) -> Result<JsonValue, JsonRpcError> {
        (**self).call(method, params).await
    }
    async fn notify(&self, method: &str, params: JsonValue) -> Result<(), JsonRpcError> {
        (**self).notify(method, params).await
    }
}

#[async_trait]
impl RpcCall for crate::connection::channel::Channel {
    async fn call(&self, method: &str, params: JsonValue) -> Result<JsonValue, JsonRpcError> {
        crate::connection::channel::Channel::call(self, method, params).await
    }
    async fn notify(&self, method: &str, params: JsonValue) -> Result<(), JsonRpcError> {
        crate::connection::channel::Channel::notify(self, method, params).await
    }
}

#[async_trait]
impl RpcCall for crate::connection::hub_connection::HubRpcConnection {
    async fn call(&self, method: &str, params: JsonValue) -> Result<JsonValue, JsonRpcError> {
        crate::connection::hub_connection::HubRpcConnection::call(self, method, params).await
    }
    async fn notify(&self, method: &str, params: JsonValue) -> Result<(), JsonRpcError> {
        crate::connection::hub_connection::HubRpcConnection::notify(self, method, params).await
    }
}
