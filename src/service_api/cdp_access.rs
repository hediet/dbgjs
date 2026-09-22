use super::*;

#[link_rpc_interface(id = "dev.dbgjs.cdp-access")]
pub trait CdpAccessApi {
    async fn raw_cdp_request(
        target_ref: TargetRef,
        method: String,
        params: serde_json::Value,
        validate: bool,
    ) -> Result<serde_json::Value, JsonRpcError>;

    async fn raw_cdp_session_request(
        target_ref: TargetRef,
        session_id: String,
        method: String,
        params: serde_json::Value,
        validate: bool,
    ) -> Result<serde_json::Value, JsonRpcError>;
}
