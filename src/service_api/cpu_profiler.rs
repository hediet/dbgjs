use super::*;

#[link_rpc_interface(id = "dev.dbgjs.cpu-profiler")]
pub trait CpuProfilerApi {
    async fn start_cpu_profile(
        context_id: String,
        connection_id: String,
        target_id: String,
        sampling_interval_micros: Option<u64>,
    ) -> Result<bool, JsonRpcError>;

    async fn stop_cpu_profile(
        context_id: String,
        connection_id: String,
        target_id: String,
        capture_id: Option<String>,
    ) -> Result<CpuProfileSnapshot, JsonRpcError>;

    async fn get_cpu_profile(
        context_id: String,
        connection_id: String,
        target_id: String,
        capture_id: String,
        source_path: Option<String>,
        no_cache: bool,
        project: bool,
    ) -> Result<CpuProfileSnapshot, JsonRpcError>;
}
