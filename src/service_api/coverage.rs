use super::*;

#[link_rpc_interface(id = "dev.dbgjs.coverage")]
pub trait CoverageApi {
    async fn start_coverage(
        context_id: String,
        connection_id: String,
        target_id: String,
    ) -> Result<bool, JsonRpcError>;

    async fn take_coverage(
        context_id: String,
        connection_id: String,
        target_id: String,
        capture_id: Option<String>,
        raw: Option<bool>,
    ) -> Result<CoverageSnapshot, JsonRpcError>;

    async fn stop_coverage(
        context_id: String,
        connection_id: String,
        target_id: String,
        capture_id: Option<String>,
    ) -> Result<CoverageSnapshot, JsonRpcError>;

    async fn finish_coverage(
        context_id: String,
        connection_id: String,
        target_id: String,
        capture_id: Option<String>,
    ) -> Result<bool, JsonRpcError>;

    async fn get_coverage(
        context_id: String,
        connection_id: String,
        target_id: String,
        capture_id: String,
        source_path: Option<String>,
        no_cache: bool,
    ) -> Result<CoverageSnapshot, JsonRpcError>;
}
