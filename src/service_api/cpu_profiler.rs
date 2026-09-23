use super::*;

#[link_rpc_interface(id = "dev.dbgjs.cpu-profiler")]
pub trait CpuProfilerApi {
    async fn start_cpu_profile(
        target_ref: TargetRef,
        sampling_interval_micros: Option<u64>,
    ) -> Result<bool, CpuProfilerError>;

    async fn stop_cpu_profile(
        target_ref: TargetRef,
        capture_id: Option<String>,
    ) -> Result<CpuProfileSnapshot, CpuProfilerError>;

    async fn get_cpu_profile(
        target_ref: TargetRef,
        capture_id: String,
        source_path: Option<String>,
        no_cache: bool,
        project: bool,
    ) -> Result<CpuProfileSnapshot, CpuProfilerError>;
}
