use super::*;

#[link_rpc_interface(id = "dev.dbgjs.cdp-access")]
pub trait CdpAccessApi {
    async fn raw_cdp_request(
        context_id: String,
        connection_id: String,
        target_id: String,
        method: String,
        params: serde_json::Value,
        validate: bool,
    ) -> Result<serde_json::Value, JsonRpcError>;

    async fn raw_cdp_session_request(
        context_id: String,
        connection_id: String,
        target_id: String,
        session_id: String,
        method: String,
        params: serde_json::Value,
        validate: bool,
    ) -> Result<serde_json::Value, JsonRpcError>;
}
