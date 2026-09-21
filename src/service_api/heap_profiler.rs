use super::*;

#[link_rpc_interface(id = "dev.dbgjs.heap-profiler")]
pub trait HeapProfilerApi {
    #[output_stream(HeapSnapshotProgress)]
    async fn take_heap_snapshot(
        context_id: String,
        connection_id: String,
        target_id: String,
        path: String,
        capture_numeric_value: bool,
        expose_internals: bool,
    ) -> Result<HeapSnapshotResult, JsonRpcError>;

    #[output_stream(HeapSnapshotProgress)]
    async fn capture_heap_snapshot(
        context_id: String,
        connection_id: String,
        target_id: String,
        capture_id: Option<String>,
        capture_numeric_value: bool,
        expose_internals: bool,
    ) -> Result<HeapCaptureResult, JsonRpcError>;

    async fn get_heap_classes(
        context_id: String,
        connection_id: String,
        target_id: String,
        capture_id: String,
        filter: Option<String>,
        no_cache: bool,
    ) -> Result<HeapClassSnapshot, JsonRpcError>;

    async fn select_promises(
        context_id: String,
        connection_id: String,
        target_id: String,
        capture_id: String,
        state: Option<PromiseState>,
        limit: u32,
        max_preview_length: u32,
    ) -> Result<PromiseSelectionSnapshot, JsonRpcError>;

    async fn select_heap_nodes(
        context_id: String,
        connection_id: String,
        target_id: String,
        capture_id: String,
        selector: HeapNodeSelector,
        max_string_length: Option<u32>,
        include_dominators: bool,
    ) -> Result<HeapNodeSelectionSnapshot, JsonRpcError>;

    async fn get_heap_references(
        context_id: String,
        connection_id: String,
        target_id: String,
        reference: String,
        direction: HeapReferenceDirection,
        edge_policy: HeapEdgePolicy,
        limit: u32,
        max_string_length: Option<u32>,
    ) -> Result<HeapReferencesSnapshot, JsonRpcError>;

    async fn get_heap_path(
        context_id: String,
        connection_id: String,
        target_id: String,
        from: String,
        to: String,
        options: HeapPathOptions,
        max_string_length: Option<u32>,
    ) -> Result<Option<HeapPathSnapshot>, JsonRpcError>;

    async fn get_heap_dominator_chain(
        context_id: String,
        connection_id: String,
        target_id: String,
        reference: String,
        max_string_length: Option<u32>,
    ) -> Result<HeapDominatorSnapshot, JsonRpcError>;

    async fn aggregate_heap_snapshot(
        context_id: String,
        connection_id: String,
        target_id: String,
        capture_id: String,
        by: HeapAggregateBy,
        limit: u32,
        max_string_length: Option<u32>,
    ) -> Result<HeapAggregateSnapshot, JsonRpcError>;

    async fn diff_heap_snapshots(
        context_id: String,
        connection_id: String,
        target_id: String,
        older_capture_id: String,
        newer_capture_id: String,
        by: HeapAggregateBy,
        limit: u32,
        max_string_length: Option<u32>,
    ) -> Result<HeapDiffSnapshot, JsonRpcError>;

    async fn get_heap_snapshot_progress(
        context_id: String,
        connection_id: String,
        target_id: String,
    ) -> Result<Option<HeapSnapshotProgress>, JsonRpcError>;
}
