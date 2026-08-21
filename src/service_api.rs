use hubrpc::prelude::{JsonRpcError, hub_rpc_interface};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct ServiceInfo {
    pub process_id: u32,
    pub agent_instance_id: String,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct ContextSummary {
    pub agent_instance_id: String,
    pub id: String,
    pub display_name: String,
    pub revision: u64,
    pub connection_count: u32,
    pub breakpoint_count: u32,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct ContextSnapshot {
    pub agent_instance_id: String,
    pub id: String,
    pub display_name: String,
    pub revision: u64,
    pub connections: Vec<ConnectionSnapshot>,
    pub breakpoints: Vec<BreakpointSnapshot>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct ConnectionSnapshot {
    pub id: String,
    pub configuration: ConnectionConfiguration,
    pub generation: u64,
    pub status: ConnectionStatus,
    pub targets: Vec<TargetSnapshot>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(tag = "kind", rename_all = "camelCase")]
pub enum ConnectionConfiguration {
    DirectCdp {
        endpoint: String,
    },
    Playwright {
        url: String,
        channel: PlaywrightChannel,
        headless: bool,
        #[serde(default)]
        ignore_https_errors: bool,
    },
}

impl From<&str> for ConnectionConfiguration {
    fn from(endpoint: &str) -> Self {
        Self::DirectCdp {
            endpoint: endpoint.to_owned(),
        }
    }
}

impl From<String> for ConnectionConfiguration {
    fn from(endpoint: String) -> Self {
        Self::DirectCdp { endpoint }
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub enum PlaywrightChannel {
    Bundled,
    Chrome,
    ChromeBeta,
    ChromeDev,
    ChromeCanary,
    Msedge,
    MsedgeBeta,
    MsedgeDev,
    MsedgeCanary,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct TargetSnapshot {
    pub target_id: String,
    pub target_type: String,
    pub title: String,
    pub url: String,
    pub attached: bool,
    pub parent_id: Option<String>,
    pub opener_id: Option<String>,
    pub browser_context_id: Option<String>,
    pub subtype: Option<String>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(tag = "kind", rename_all = "camelCase")]
pub enum ConnectionStatus {
    Disconnected,
    Connecting,
    Disconnecting,
    Connected {
        product: String,
        #[serde(rename = "protocolVersion")]
        #[schemars(rename = "protocolVersion")]
        protocol_version: String,
    },
    Failed {
        message: String,
    },
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct BreakpointSnapshot {
    pub id: String,
    pub source_path: String,
    pub line: u32,
    pub column: u32,
    pub status: BreakpointStatus,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub enum BreakpointStatus {
    Unconfirmed,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct TargetDebuggerSnapshot {
    pub context_id: String,
    pub connection_id: String,
    pub target_id: String,
    pub connection_generation: u64,
    pub revision: u64,
    pub phase: TargetDebuggerPhase,
    pub scripts: Vec<TargetScriptSnapshot>,
    pub breakpoints: Vec<TargetBreakpointSnapshot>,
    pub logs: Vec<ConsoleMessageSnapshot>,
    pub pause: Option<PauseSnapshot>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct ConsoleMessageSnapshot {
    pub index: u64,
    pub values: Vec<String>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(tag = "kind", rename_all = "camelCase")]
pub enum TargetDebuggerPhase {
    Running,
    Paused { epoch: u64 },
    Resuming { epoch: u64 },
    Failed { message: String },
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct TargetBreakpointSnapshot {
    pub id: String,
    pub source_url: String,
    pub line: u32,
    pub column: u32,
    pub status: TargetBreakpointStatus,
    pub source: Option<SourceExcerpt>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct LogpointSpec {
    pub id: String,
    pub source_url: String,
    pub line: u32,
    pub column: u32,
    pub expression: String,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(tag = "kind", rename_all = "camelCase")]
pub enum TargetBreakpointStatus {
    Pending,
    Installed { binding_count: u32 },
    Failed { message: String },
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct TargetScriptSnapshot {
    pub url: String,
    pub source_map_url: Option<String>,
    pub status: TargetScriptStatus,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(tag = "kind", rename_all = "camelCase")]
pub enum TargetScriptStatus {
    Unresolved,
    Pending,
    Resolved { authored_sources: Vec<String> },
    Failed { message: String },
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct PauseSnapshot {
    pub epoch: u64,
    pub reason: String,
    pub frames: Vec<FrameSnapshot>,
    pub source: Option<SourceExcerpt>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct SourceExcerpt {
    pub source_url: String,
    pub breadcrumb: Option<String>,
    pub current_line: u32,
    pub lines: Vec<SourceExcerptLine>,
    pub highlight_start: u32,
    pub highlight_length: u32,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct SourceExcerptLine {
    pub line: u32,
    pub text: String,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct EvaluationSnapshot {
    pub expression: String,
    pub kind: String,
    pub value: Option<serde_json::Value>,
    pub unserializable_value: Option<String>,
    pub description: Option<String>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct CoverageSnapshot {
    pub timestamp_micros: u64,
    pub sources: Vec<CoverageSourceSnapshot>,
    #[serde(default)]
    pub analysis: Option<CoverageAnalysisSnapshot>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct CoverageAnalysisSnapshot {
    pub duration_micros: u64,
    pub source_map_cache_hits: u64,
    pub source_map_cache_misses: u64,
    pub source_map_cache_bypasses: u64,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct CoverageSourceSnapshot {
    pub script_id: String,
    pub generated_url: String,
    pub associated_authored_source: Option<String>,
    pub functions: Vec<CoverageFunctionSnapshot>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct CoverageFunctionSnapshot {
    pub name: String,
    pub block_coverage: bool,
    pub root_start_offset: u32,
    pub root_end_offset: u32,
    pub ranges: Vec<CoverageRangeSnapshot>,
    #[serde(default)]
    pub effective_ranges: Vec<CoverageRangeSnapshot>,
    pub authored_location: Option<SourceLocation>,
    pub breadcrumb: Option<String>,
    pub generated_location: Option<SourceLocation>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct CoverageRangeSnapshot {
    pub start_offset: u32,
    pub end_offset: u32,
    pub count: u64,
    pub authored_start: Option<SourceLocation>,
    pub authored_end: Option<SourceLocation>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct HeapSnapshotProgress {
    pub done: i64,
    pub total: i64,
    pub finished: Option<bool>,
    pub bytes_written: u64,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct HeapSnapshotResult {
    pub path: String,
    pub bytes_written: u64,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct HeapCaptureResult {
    pub capture_id: String,
    pub bytes_written: u64,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct HeapClassSnapshot {
    pub capture_id: String,
    pub total_instances: u64,
    pub total_shallow_size: u64,
    pub classes: Vec<HeapClassSnapshotEntry>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct HeapClassSnapshotEntry {
    pub name: String,
    pub source_url: String,
    pub location: SourceLocation,
    pub generated_name: String,
    pub instance_count: u64,
    pub shallow_size: u64,
    pub instances: Vec<HeapInstanceSnapshot>,
    pub omitted_instance_count: u64,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct HeapInstanceSnapshot {
    pub alias: String,
    pub heap_object_id: String,
    pub shallow_size: u64,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub enum StepKind {
    Into,
    Over,
    Out,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct FrameSnapshot {
    pub index: u32,
    pub function_name: String,
    pub raw: SourceLocation,
    pub projected: FrameProjectionSnapshot,
    pub breadcrumb: Option<String>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct SourceLocation {
    pub source_url: String,
    pub line: u32,
    pub column: u32,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(tag = "kind", rename_all = "camelCase")]
pub enum FrameProjectionSnapshot {
    Raw,
    Pending,
    Resolved { location: SourceLocation },
    Failed { message: String },
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(tag = "kind", rename_all = "camelCase")]
pub enum TargetWaitPredicate {
    Running,
    BreakpointInstalled { breakpoint_id: String },
    Paused { after_epoch: u64 },
}

#[hub_rpc_interface(id = "dev.hediet.cdp-debugger")]
pub trait DebuggerServiceApi {
    async fn service_info() -> Result<ServiceInfo, JsonRpcError>;

    async fn list_contexts() -> Result<Vec<ContextSummary>, JsonRpcError>;

    async fn put_context(
        context_id: String,
        display_name: Option<String>,
    ) -> Result<ContextSnapshot, JsonRpcError>;

    async fn get_context(context_id: String) -> Result<ContextSnapshot, JsonRpcError>;

    async fn put_connection(
        context_id: String,
        connection_id: String,
        configuration: ConnectionConfiguration,
    ) -> Result<ContextSnapshot, JsonRpcError>;

    async fn connect_connection(
        context_id: String,
        connection_id: String,
    ) -> Result<ContextSnapshot, JsonRpcError>;

    async fn disconnect_connection(
        context_id: String,
        connection_id: String,
    ) -> Result<ContextSnapshot, JsonRpcError>;

    async fn put_breakpoint(
        context_id: String,
        breakpoint_id: String,
        source_path: String,
        line: u32,
        column: u32,
    ) -> Result<ContextSnapshot, JsonRpcError>;

    async fn attach_target(
        context_id: String,
        connection_id: String,
        target_id: String,
    ) -> Result<TargetDebuggerSnapshot, JsonRpcError>;

    async fn get_target(
        context_id: String,
        connection_id: String,
        target_id: String,
    ) -> Result<TargetDebuggerSnapshot, JsonRpcError>;

    async fn wait_target(
        context_id: String,
        connection_id: String,
        target_id: String,
        predicate: TargetWaitPredicate,
        timeout_ms: u64,
    ) -> Result<TargetDebuggerSnapshot, JsonRpcError>;

    async fn resume_target(
        context_id: String,
        connection_id: String,
        target_id: String,
        pause_epoch: u64,
    ) -> Result<TargetDebuggerSnapshot, JsonRpcError>;

    async fn step_target(
        context_id: String,
        connection_id: String,
        target_id: String,
        pause_epoch: u64,
        kind: StepKind,
    ) -> Result<TargetDebuggerSnapshot, JsonRpcError>;

    async fn evaluate_target(
        context_id: String,
        connection_id: String,
        target_id: String,
        pause_epoch: Option<u64>,
        frame_index: u32,
        expression: String,
    ) -> Result<EvaluationSnapshot, JsonRpcError>;

    async fn set_logpoint(
        context_id: String,
        connection_id: String,
        target_id: String,
        logpoint_id: String,
        source_url: String,
        line: u32,
        column: u32,
        expression: String,
    ) -> Result<TargetDebuggerSnapshot, JsonRpcError>;

    async fn set_logpoints(
        context_id: String,
        connection_id: String,
        target_id: String,
        logpoints: Vec<LogpointSpec>,
    ) -> Result<TargetDebuggerSnapshot, JsonRpcError>;

    async fn click_target(
        context_id: String,
        connection_id: String,
        target_id: String,
        selector: String,
    ) -> Result<bool, JsonRpcError>;

    async fn key_target(
        context_id: String,
        connection_id: String,
        target_id: String,
        chord: String,
    ) -> Result<bool, JsonRpcError>;

    async fn type_target(
        context_id: String,
        connection_id: String,
        target_id: String,
        text: String,
    ) -> Result<bool, JsonRpcError>;

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
        exclude_capture_id: Option<String>,
    ) -> Result<CoverageSnapshot, JsonRpcError>;

    async fn stop_coverage(
        context_id: String,
        connection_id: String,
        target_id: String,
        exclude_capture_id: Option<String>,
    ) -> Result<CoverageSnapshot, JsonRpcError>;

    async fn finish_coverage(
        context_id: String,
        connection_id: String,
        target_id: String,
        exclude_capture_id: Option<String>,
    ) -> Result<bool, JsonRpcError>;

    async fn get_coverage(
        context_id: String,
        connection_id: String,
        target_id: String,
        capture_id: String,
        source_path: Option<String>,
        no_cache: bool,
    ) -> Result<CoverageSnapshot, JsonRpcError>;

    async fn take_heap_snapshot(
        context_id: String,
        connection_id: String,
        target_id: String,
        path: String,
        capture_numeric_value: bool,
        expose_internals: bool,
    ) -> Result<HeapSnapshotResult, JsonRpcError>;

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

    async fn get_heap_snapshot_progress(
        context_id: String,
        connection_id: String,
        target_id: String,
    ) -> Result<Option<HeapSnapshotProgress>, JsonRpcError>;

    async fn shutdown() -> Result<bool, JsonRpcError>;
}
