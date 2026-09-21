use super::*;

#[link_rpc_interface(id = "dev.dbgjs.relay")]
pub trait RelayApi {
    async fn open_playwright_proxy(
        context_id: String,
        connection_id: String,
        target_id: String,
        expected_generation: u64,
    ) -> Result<PlaywrightProxyEndpoint, JsonRpcError>;

    async fn close_playwright_proxy(proxy_id: String) -> Result<bool, JsonRpcError>;

    /// Opens a virtual browser-root CDP relay exposing every target across every connection in
    /// `context_id` as one endpoint. Takes exclusive relay ownership of the context immediately:
    /// ordinary local target debugging commands fail until the relay closes. Does not restart
    /// any underlying connection; existing attachments and future ones stay lazy.
    async fn open_context_relay(context_id: String) -> Result<RelayEndpoint, JsonRpcError>;

    /// Opens a direct-root CDP relay exposing exactly one target. Takes the same exclusive
    /// relay ownership of the target's owning context as `open_context_relay`.
    async fn open_target_relay(
        context_id: String,
        connection_id: String,
        target_id: String,
    ) -> Result<RelayEndpoint, JsonRpcError>;

    /// Closes a relay opened by `open_context_relay` or `open_target_relay`, restoring ordinary
    /// local access to its context. Returns `false` if the relay was already closed.
    async fn close_relay(relay_id: String) -> Result<bool, JsonRpcError>;
}
