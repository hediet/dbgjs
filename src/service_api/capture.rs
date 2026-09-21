use super::*;

#[link_rpc_interface(id = "dev.dbgjs.capture")]
pub trait CaptureApi {
    async fn list_captures(context_id: String) -> Result<Vec<CaptureSnapshot>, JsonRpcError>;

    async fn get_capture(
        context_id: String,
        capture_name: String,
    ) -> Result<CaptureSnapshot, JsonRpcError>;

    async fn delete_capture(context_id: String, capture_name: String)
    -> Result<bool, JsonRpcError>;

    async fn get_stored_coverage(
        context_id: String,
        capture_name: String,
        source_path: Option<String>,
        target_id: Option<String>,
        connection_id: Option<String>,
        path_glob: Option<String>,
        exclude_capture_id: Option<String>,
    ) -> Result<CoverageSnapshot, JsonRpcError>;

    async fn get_stored_cpu_profile(
        context_id: String,
        capture_name: String,
        source_path: Option<String>,
        target_id: Option<String>,
        connection_id: Option<String>,
    ) -> Result<CpuProfileSnapshot, JsonRpcError>;

    async fn get_stored_heap_classes(
        context_id: String,
        capture_name: String,
        filter: Option<String>,
        target_id: Option<String>,
        connection_id: Option<String>,
    ) -> Result<HeapClassSnapshot, JsonRpcError>;

    async fn supply_stored_heap_source_map(
        context_id: String,
        capture_name: String,
        supply: HeapSourceMapSupply,
    ) -> Result<(), JsonRpcError>;
}
