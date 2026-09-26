use super::*;

#[link_rpc_interface(id = "dev.dbgjs.heap-profiler")]
pub trait HeapProfilerApi {
    #[output_stream(HeapSnapshotProgress)]
    async fn take_heap_snapshot(
        target_ref: TargetRef,
        path: String,
        capture_numeric_value: bool,
        expose_internals: bool,
    ) -> Result<HeapSnapshotResult, HeapProfilerError>;

    #[output_stream(HeapSnapshotProgress)]
    async fn capture_heap_snapshot(
        target_ref: TargetRef,
        capture_id: Option<String>,
        capture_numeric_value: bool,
        expose_internals: bool,
    ) -> Result<HeapCaptureResult, HeapProfilerError>;

    async fn get_heap_classes(
        target_ref: TargetRef,
        capture_id: String,
        filter: Option<String>,
        no_cache: bool,
    ) -> Result<HeapClassSnapshot, HeapProfilerError>;

    async fn select_promises(
        target_ref: TargetRef,
        capture_id: String,
        state: Option<PromiseState>,
        limit: u32,
        max_preview_length: u32,
    ) -> Result<PromiseSelectionSnapshot, HeapProfilerError>;

    async fn select_heap_nodes(
        target_ref: TargetRef,
        capture_id: String,
        selector: HeapNodeSelector,
        max_string_length: Option<u32>,
        include_dominators: bool,
    ) -> Result<HeapNodeSelectionSnapshot, HeapProfilerError>;

    async fn get_heap_references(
        target_ref: TargetRef,
        reference: String,
        direction: HeapReferenceDirection,
        edge_policy: HeapEdgePolicy,
        limit: u32,
        max_string_length: Option<u32>,
    ) -> Result<HeapReferencesSnapshot, HeapProfilerError>;

    async fn get_heap_path(
        target_ref: TargetRef,
        from: String,
        to: String,
        options: HeapPathOptions,
        max_string_length: Option<u32>,
    ) -> Result<Option<HeapPathSnapshot>, HeapProfilerError>;

    async fn get_heap_dominator_chain(
        target_ref: TargetRef,
        reference: String,
        max_string_length: Option<u32>,
    ) -> Result<HeapDominatorSnapshot, HeapProfilerError>;

    async fn aggregate_heap_snapshot(
        target_ref: TargetRef,
        capture_id: String,
        by: HeapAggregateBy,
        limit: u32,
        max_string_length: Option<u32>,
    ) -> Result<HeapAggregateSnapshot, HeapProfilerError>;

    async fn diff_heap_snapshots(
        target_ref: TargetRef,
        older_capture_id: String,
        newer_capture_id: String,
        by: HeapAggregateBy,
        limit: u32,
        max_string_length: Option<u32>,
    ) -> Result<HeapDiffSnapshot, HeapProfilerError>;

    async fn get_heap_snapshot_progress(
        target_ref: TargetRef,
    ) -> Result<Option<HeapSnapshotProgress>, HeapProfilerError>;
}
