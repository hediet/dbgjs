use super::*;

#[link_rpc_interface(id = "dev.dbgjs.coverage")]
pub trait CoverageApi {
    async fn start_coverage(target_ref: TargetRef) -> Result<bool, CoverageError>;

    async fn take_coverage(
        target_ref: TargetRef,
        capture_id: Option<String>,
    ) -> Result<CoverageSnapshot, CoverageError>;

    async fn stop_coverage(
        target_ref: TargetRef,
        capture_id: Option<String>,
    ) -> Result<CoverageSnapshot, CoverageError>;

    async fn finish_coverage(
        target_ref: TargetRef,
        capture_id: Option<String>,
    ) -> Result<bool, CoverageError>;

    async fn get_coverage(
        target_ref: TargetRef,
        capture_id: String,
        source_path: Option<String>,
        no_cache: bool,
    ) -> Result<CoverageSnapshot, CoverageError>;
}
