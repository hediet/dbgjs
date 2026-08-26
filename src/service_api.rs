use hubrpc::prelude::{JsonRpcError, hub_rpc_interface};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct ServiceInfo {
    pub process_id: u32,
    pub agent_instance_id: String,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct ProcessTreeSnapshot {
    pub root_process_id: u32,
    pub processes: Vec<ProcessSnapshot>,
    pub runtime_metadata_available: bool,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct ProcessSnapshot {
    pub process_id: u32,
    pub parent_process_id: Option<u32>,
    #[serde(default)]
    pub attachable: bool,
    #[serde(default)]
    pub debug_target_id: Option<String>,
    pub name: String,
    pub command_line: String,
    pub creation_date: String,
    pub role: ProcessRole,
    pub display_name: Option<String>,
    pub window_id: Option<u32>,
    pub window_title: Option<String>,
    #[serde(default)]
    pub cpu_percent: Option<u32>,
    #[serde(default)]
    pub memory_bytes: Option<u64>,
    #[serde(default)]
    pub agent_sessions: Vec<AgentSessionSnapshot>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct AgentSessionSnapshot {
    pub internal_id: String,
    pub chat_uri: Option<String>,
    pub title: Option<String>,
    pub working_directories: Vec<String>,
    pub disconnected: Option<bool>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "kebab-case")]
pub enum ProcessRole {
    VscodeMain,
    Renderer,
    ExtensionHost,
    NodeUtility,
    Node,
    TypeScriptServer,
    TypeScriptInstaller,
    LanguageServer,
    PtyHost,
    FileWatcher,
    AgentHost,
    Copilot,
    Claude,
    Codex,
    Agent,
    Gpu,
    NetworkService,
    AudioService,
    Crashpad,
    Utility,
    Other,
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
    pub target_forest: Vec<TargetNodeSnapshot>,
    pub breakpoints: Vec<BreakpointSnapshot>,
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct MutationOptions {
    pub expected_revision: Option<u64>,
    pub request_id: Option<String>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(tag = "kind", rename_all = "camelCase")]
pub enum ObservationCursor {
    Current,
    After { revision: u64 },
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct ContextObservation {
    pub snapshot: ContextSnapshot,
    pub events: Vec<ContextEventSnapshot>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct ContextEventSnapshot {
    pub revision: u64,
    pub kind: String,
    pub subject_id: Option<String>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(tag = "kind", rename_all = "camelCase")]
pub enum ObservationResult {
    Items {
        items: Vec<ContextObservation>,
    },
    HistoryGap {
        requested_revision: u64,
        oldest_available_revision: u64,
        current: ContextSnapshot,
    },
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
#[serde(
    tag = "kind",
    rename_all = "camelCase",
    rename_all_fields = "camelCase"
)]
pub enum ConnectionConfiguration {
    DirectCdp {
        endpoint: String,
    },
    NodeInspector {
        endpoint: String,
    },
    Process {
        process_id: u32,
    },
    ProcessTree {
        root_pid: u32,
    },
    Playwright {
        url: String,
        #[serde(default, alias = "playwright_package")]
        playwright_package: Option<String>,
        channel: PlaywrightChannel,
        headless: bool,
        #[serde(default, alias = "ignore_https_errors")]
        ignore_https_errors: bool,
    },
    Chrome {
        url: String,
        executable: String,
        headless: bool,
        #[serde(default, alias = "user_data_dir")]
        user_data_dir: Option<String>,
        args: Vec<String>,
    },
    Node {
        program: String,
        args: Vec<String>,
        cwd: String,
        #[serde(alias = "runtime_executable")]
        runtime_executable: String,
        #[serde(default, alias = "runtime_args")]
        runtime_args: Vec<String>,
        env: BTreeMap<String, String>,
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
#[serde(rename_all = "camelCase")]
pub struct TargetNodeSnapshot {
    pub connection_id: String,
    pub connection_generation: u64,
    pub target: TargetSnapshot,
    pub parent_target_id: Option<String>,
}

impl ConnectionSnapshot {
    pub fn target_forest(&self) -> Vec<TargetNodeSnapshot> {
        let targets = self
            .targets
            .iter()
            .map(|target| (target.target_id.as_str(), target))
            .collect::<BTreeMap<_, _>>();
        let mut children = BTreeMap::<&str, Vec<&str>>::new();
        let mut roots = Vec::new();

        for target in &self.targets {
            let parent = target
                .parent_id
                .as_deref()
                .filter(|parent| targets.contains_key(parent))
                .or_else(|| {
                    target
                        .opener_id
                        .as_deref()
                        .filter(|parent| targets.contains_key(parent))
                });
            if let Some(parent) = parent {
                children.entry(parent).or_default().push(&target.target_id);
            } else {
                roots.push(target.target_id.as_str());
            }
        }
        roots.sort_unstable();
        for child_ids in children.values_mut() {
            child_ids.sort_unstable();
        }

        let mut visited = std::collections::BTreeSet::new();
        let mut forest = Vec::with_capacity(targets.len());
        for target_id in roots {
            self.append_target_node(
                target_id,
                None,
                &targets,
                &children,
                &mut visited,
                &mut forest,
            );
        }
        for target_id in targets.keys() {
            if !visited.contains(*target_id) {
                self.append_target_node(
                    target_id,
                    None,
                    &targets,
                    &children,
                    &mut visited,
                    &mut forest,
                );
            }
        }
        forest
    }

    fn append_target_node(
        &self,
        target_id: &str,
        parent_target_id: Option<&str>,
        targets: &BTreeMap<&str, &TargetSnapshot>,
        children: &BTreeMap<&str, Vec<&str>>,
        visited: &mut std::collections::BTreeSet<String>,
        forest: &mut Vec<TargetNodeSnapshot>,
    ) {
        if !visited.insert(target_id.to_owned()) {
            return;
        }
        let target = targets
            .get(target_id)
            .expect("target forest only contains known targets");
        forest.push(TargetNodeSnapshot {
            connection_id: self.id.clone(),
            connection_generation: self.generation,
            target: (*target).clone(),
            parent_target_id: parent_target_id.map(str::to_owned),
        });
        for child in children.get(target_id).into_iter().flatten() {
            self.append_target_node(child, Some(target_id), targets, children, visited, forest);
        }
    }
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
    pub enabled: bool,
    pub condition: Option<String>,
    pub target_selector: Option<String>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub enum BreakpointStatus {
    Unconfirmed,
    Disabled,
    Pending,
    PartiallyBound { application_count: u32 },
    Bound { application_count: u32 },
    Failed { message: String },
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct BreakpointSpec {
    pub source_path: String,
    pub line: u32,
    pub column: u32,
    pub enabled: bool,
    pub condition: Option<String>,
    pub target_selector: Option<String>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct SourceSnapshotInfo {
    pub path: String,
    pub kind: String,
    pub status: String,
    pub connection_id: Option<String>,
    pub target_id: Option<String>,
    pub source_map_url: Option<String>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct SourceContentSnapshot {
    pub path: String,
    pub content: String,
    pub start_line: u32,
    pub end_line: u32,
    pub total_lines: u32,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct SourceMatchSnapshot {
    pub path: String,
    pub line: u32,
    pub column: u32,
    pub text: String,
    pub before_context: Vec<String>,
    pub after_context: Vec<String>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct SourceSearchOptions {
    pub pattern: String,
    pub path: Option<String>,
    pub regex: bool,
    pub case_sensitive: bool,
    pub max_results: u32,
    pub context_lines: u32,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct SourceSearchSnapshot {
    pub matches: Vec<SourceMatchSnapshot>,
    pub omitted_matches: u64,
    pub searched_sources: u32,
    pub skipped_sources: u32,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct SourceDisplayOptions {
    pub line: Option<u32>,
    pub context_lines: u32,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct SourceProjectionPathSnapshot {
    pub generated_url: String,
    pub steps: Vec<String>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct SourceGraphViewSnapshot {
    pub connection_id: String,
    pub target_id: String,
    pub generated_url: String,
    pub source_path: String,
    pub role: String,
    pub kind: String,
    pub primary_provenance: String,
    pub alternative_provenance: Vec<String>,
    pub projection_paths: Vec<SourceProjectionPathSnapshot>,
    pub resolved_source_count: u32,
    pub diagnostics: Vec<String>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct CompactedSourceGraphSnapshot {
    pub roots: Vec<u32>,
    pub nodes: Vec<CompactedSourceNodeSnapshot>,
    pub edges: Vec<CompactedSourceEdgeSnapshot>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct CompactedSourceNodeSnapshot {
    pub id: u32,
    pub prefix: String,
    pub source_count: u32,
    pub runtime_internal: bool,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct CompactedSourceEdgeSnapshot {
    pub derived: u32,
    pub basis: u32,
    pub kind: String,
    pub mapping_count: u32,
    pub suffix_rewrite: Option<SourceSuffixRewriteSnapshot>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct SourceSuffixRewriteSnapshot {
    pub from: String,
    pub to: String,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct SourceMappingSnapshot {
    pub connection_id: String,
    pub target_id: String,
    pub source_url: String,
    pub line: u32,
    pub column: u32,
    pub direction: String,
    pub quality: String,
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
    pub object_id: Option<String>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct ScreenshotSnapshot {
    pub media_type: String,
    pub data_base64: String,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct ScopeSnapshot {
    pub index: u32,
    pub kind: String,
    pub name: Option<String>,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct VariableSnapshot {
    pub name: String,
    pub kind: String,
    pub value: Option<serde_json::Value>,
    pub unserializable_value: Option<String>,
    pub description: Option<String>,
    pub object_id: Option<String>,
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

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct CpuProfileSnapshot {
    pub capture_id: String,
    pub sampling_interval_micros: Option<u64>,
    pub start_time_micros: f64,
    pub end_time_micros: f64,
    pub nodes: Vec<CpuProfileNodeSnapshot>,
    pub samples: Vec<i64>,
    pub time_deltas_micros: Vec<u64>,
    #[serde(default)]
    pub functions: Vec<CpuProfileFunctionSnapshot>,
    #[serde(default)]
    pub analysis: Option<CpuProfileAnalysisSnapshot>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct CpuProfileNodeSnapshot {
    pub id: i64,
    pub call_frame: CpuProfileCallFrameSnapshot,
    pub hit_count: Option<i64>,
    pub children: Vec<i64>,
    pub deopt_reason: Option<String>,
    pub position_ticks: Vec<CpuProfilePositionTickSnapshot>,
    pub authored_location: Option<SourceLocation>,
    pub breadcrumb: Option<String>,
    pub self_time_micros: u64,
    pub total_time_micros: u64,
    pub sample_count: u64,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct CpuProfileCallFrameSnapshot {
    pub function_name: String,
    pub script_id: String,
    pub url: String,
    pub line_number: i64,
    pub column_number: i64,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct CpuProfilePositionTickSnapshot {
    pub line: i64,
    pub ticks: i64,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct CpuProfileFunctionSnapshot {
    pub name: String,
    pub breadcrumb: Option<String>,
    pub generated_location: SourceLocation,
    pub authored_location: Option<SourceLocation>,
    pub self_time_micros: u64,
    pub total_time_micros: u64,
    pub sample_count: u64,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct CpuProfileAnalysisSnapshot {
    pub duration_micros: u64,
    pub source_map_cache_hits: u64,
    pub source_map_cache_misses: u64,
    pub source_map_cache_bypasses: u64,
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
    pub timing: HeapSnapshotTiming,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct HeapCaptureResult {
    pub capture_id: String,
    pub bytes_written: u64,
    pub timing: HeapSnapshotTiming,
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct HeapSnapshotTiming {
    pub taking_duration_micros: u64,
    pub retrieving_duration_micros: u64,
}

impl HeapSnapshotTiming {
    pub fn total_duration_micros(&self) -> u64 {
        self.taking_duration_micros
            .saturating_add(self.retrieving_duration_micros)
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct HeapClassSnapshot {
    pub capture_id: String,
    pub total_instances: u64,
    pub total_shallow_size: u64,
    pub classes: Vec<HeapClassSnapshotEntry>,
    pub analysis: HeapClassAnalysisSnapshot,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct HeapClassAnalysisSnapshot {
    #[serde(default)]
    pub snapshot_timing: Option<HeapSnapshotTiming>,
    pub parse_duration_micros: u64,
    pub projection_duration_micros: u64,
    pub source_map_hydration_duration_micros: u64,
    pub constructor_group_count: u64,
    pub used_cached_groups: bool,
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

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct HeapNodeSelector {
    pub heap_object_id: Option<String>,
    pub node_type: Option<String>,
    pub name: Option<String>,
    pub name_regex: Option<String>,
    pub string_contains: Option<String>,
    pub string_regex: Option<String>,
    pub min_shallow_size: Option<u64>,
    pub max_shallow_size: Option<u64>,
    pub limit: Option<u32>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct HeapNodeLocationSnapshot {
    pub script_id: i64,
    pub line: u32,
    pub column: u32,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct HeapNodeSnapshot {
    pub reference: String,
    pub node_index: u32,
    pub node_type: String,
    pub heap_object_id: String,
    pub name: String,
    pub string_value: Option<String>,
    pub string_truncated: bool,
    pub shallow_size: u64,
    pub outgoing_reference_count: u64,
    pub incoming_reference_count: u64,
    pub locations: Vec<HeapNodeLocationSnapshot>,
    pub immediate_dominator: Option<String>,
    pub retained_size: Option<u64>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct HeapNodeSelectionSnapshot {
    pub capture_id: String,
    pub total_nodes: u64,
    pub total_edges: u64,
    pub nodes: Vec<HeapNodeSnapshot>,
    pub graph_parse_duration_micros: u64,
    pub used_cached_graph: bool,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub enum HeapReferenceDirection {
    Incoming,
    #[default]
    Outgoing,
    Both,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub enum HeapEdgePolicy {
    #[default]
    Strong,
    All,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct HeapReferenceSnapshot {
    pub edge_index: u32,
    pub edge_type: String,
    pub name: Option<String>,
    pub name_or_index: u64,
    pub source: String,
    pub target: String,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct HeapReferencesSnapshot {
    pub capture_id: String,
    pub node: HeapNodeSnapshot,
    pub direction: HeapReferenceDirection,
    pub edge_policy: HeapEdgePolicy,
    pub references: Vec<HeapReferenceSnapshot>,
    pub omitted_reference_count: u64,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub enum HeapPathDirection {
    #[default]
    Outgoing,
    Incoming,
    Either,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub enum HeapPathCost {
    #[default]
    Edges,
    Readable,
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct HeapPathOptions {
    pub direction: HeapPathDirection,
    pub edge_policy: HeapEdgePolicy,
    pub cost: HeapPathCost,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub enum HeapTraversalDirection {
    Outgoing,
    Incoming,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct HeapPathStepSnapshot {
    pub from: String,
    pub to: String,
    pub edge_index: u32,
    pub edge_type: String,
    pub name: Option<String>,
    pub name_or_index: u64,
    pub direction: HeapTraversalDirection,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct HeapPathSnapshot {
    pub capture_id: String,
    pub from: String,
    pub to: String,
    pub cost: u64,
    pub nodes: Vec<HeapNodeSnapshot>,
    pub steps: Vec<HeapPathStepSnapshot>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct HeapDominatorSnapshot {
    pub capture_id: String,
    pub node: HeapNodeSnapshot,
    pub chain: Vec<HeapNodeSnapshot>,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub enum HeapAggregateBy {
    #[default]
    NodeType,
    Name,
    StringValue,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct HeapAggregateEntrySnapshot {
    pub key: String,
    pub key_truncated: bool,
    pub count: u64,
    pub shallow_size: u64,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct HeapAggregateSnapshot {
    pub capture_id: String,
    pub by: HeapAggregateBy,
    pub entries: Vec<HeapAggregateEntrySnapshot>,
    pub omitted_entry_count: u64,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct HeapDiffEntrySnapshot {
    pub key: String,
    pub key_truncated: bool,
    pub count_delta: i64,
    pub shallow_size_delta: i64,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct HeapDiffSnapshot {
    pub older_capture_id: String,
    pub newer_capture_id: String,
    pub by: HeapAggregateBy,
    pub entries: Vec<HeapDiffEntrySnapshot>,
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
    pub scopes: Vec<ScopeSnapshot>,
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
#[serde(
    tag = "kind",
    rename_all = "camelCase",
    rename_all_fields = "camelCase"
)]
pub enum TargetWaitPredicate {
    Changed {
        #[serde(alias = "after_revision")]
        after_revision: u64,
    },
    Running,
    BreakpointInstalled {
        #[serde(alias = "breakpoint_id")]
        breakpoint_id: String,
    },
    Paused {
        #[serde(alias = "after_epoch")]
        after_epoch: u64,
    },
}

#[hub_rpc_interface(id = "dev.hediet.cdp-debugger")]
pub trait DebuggerServiceApi {
    async fn service_info() -> Result<ServiceInfo, JsonRpcError>;

    async fn discover_vscode_process_trees() -> Result<Vec<ProcessTreeSnapshot>, JsonRpcError>;

    async fn list_contexts() -> Result<Vec<ContextSummary>, JsonRpcError>;

    async fn put_context(
        context_id: String,
        display_name: Option<String>,
    ) -> Result<ContextSnapshot, JsonRpcError>;

    async fn get_context(context_id: String) -> Result<ContextSnapshot, JsonRpcError>;

    async fn observe_context(
        context_id: String,
        cursor: ObservationCursor,
        timeout_ms: u64,
    ) -> Result<ObservationResult, JsonRpcError>;

    async fn delete_context(
        context_id: String,
        options: MutationOptions,
    ) -> Result<bool, JsonRpcError>;

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

    async fn delete_connection(
        context_id: String,
        connection_id: String,
        options: MutationOptions,
    ) -> Result<ContextSnapshot, JsonRpcError>;

    async fn put_breakpoint(
        context_id: String,
        breakpoint_id: String,
        source_path: String,
        line: u32,
        column: u32,
    ) -> Result<ContextSnapshot, JsonRpcError>;

    async fn put_breakpoint_spec(
        context_id: String,
        breakpoint_id: String,
        specification: BreakpointSpec,
        options: MutationOptions,
    ) -> Result<ContextSnapshot, JsonRpcError>;

    async fn delete_breakpoint(
        context_id: String,
        breakpoint_id: String,
        options: MutationOptions,
    ) -> Result<ContextSnapshot, JsonRpcError>;

    async fn list_sources(
        context_id: String,
        path: Option<String>,
    ) -> Result<Vec<SourceSnapshotInfo>, JsonRpcError>;

    async fn show_source_graph(
        context_id: String,
    ) -> Result<CompactedSourceGraphSnapshot, JsonRpcError>;

    async fn show_source(
        context_id: String,
        path: String,
        options: SourceDisplayOptions,
    ) -> Result<SourceContentSnapshot, JsonRpcError>;

    async fn grep_sources(
        context_id: String,
        options: SourceSearchOptions,
    ) -> Result<SourceSearchSnapshot, JsonRpcError>;

    async fn explain_source(
        context_id: String,
        path: String,
    ) -> Result<Vec<SourceGraphViewSnapshot>, JsonRpcError>;

    async fn map_source(
        context_id: String,
        path: String,
        line: u32,
        column: u32,
    ) -> Result<Vec<SourceMappingSnapshot>, JsonRpcError>;

    async fn evict_source_caches(context_id: String) -> Result<u32, JsonRpcError>;

    async fn export_sources(
        context_id: String,
        destination: String,
    ) -> Result<Vec<String>, JsonRpcError>;

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

    async fn observe_target(
        context_id: String,
        connection_id: String,
        target_id: String,
        after_revision: u64,
        timeout_ms: u64,
    ) -> Result<Option<TargetDebuggerSnapshot>, JsonRpcError>;

    async fn release_target(
        context_id: String,
        connection_id: String,
        target_id: String,
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

    async fn get_scope_variables(
        context_id: String,
        connection_id: String,
        target_id: String,
        pause_epoch: u64,
        frame_index: u32,
        scope_index: u32,
    ) -> Result<Vec<VariableSnapshot>, JsonRpcError>;

    async fn get_object_properties(
        context_id: String,
        connection_id: String,
        target_id: String,
        pause_epoch: Option<u64>,
        object_id: String,
    ) -> Result<Vec<VariableSnapshot>, JsonRpcError>;

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

    async fn capture_screenshot(
        context_id: String,
        connection_id: String,
        target_id: String,
    ) -> Result<ScreenshotSnapshot, JsonRpcError>;

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

    async fn shutdown() -> Result<bool, JsonRpcError>;
}

#[cfg(test)]
mod tests {
    use super::{
        ConnectionConfiguration, ConnectionSnapshot, ConnectionStatus, TargetSnapshot,
        TargetWaitPredicate,
    };

    #[test]
    fn target_wait_predicate_uses_camel_case_fields() {
        let predicate = TargetWaitPredicate::Paused { after_epoch: 7 };
        assert_eq!(
            serde_json::to_value(&predicate).unwrap(),
            serde_json::json!({ "kind": "paused", "afterEpoch": 7 })
        );
        assert_eq!(
            serde_json::from_value::<TargetWaitPredicate>(
                serde_json::json!({ "kind": "paused", "after_epoch": 7 })
            )
            .unwrap(),
            predicate
        );
    }

    #[test]
    fn target_forest_uses_parent_then_opener_and_breaks_cycles() {
        let connection = ConnectionSnapshot {
            id: "connection".to_owned(),
            configuration: ConnectionConfiguration::DirectCdp {
                endpoint: "ws://example".to_owned(),
            },
            generation: 7,
            status: ConnectionStatus::Disconnected,
            targets: vec![
                target("z-root", None, None),
                target("child-by-opener", Some("missing"), Some("z-root")),
                target("child-by-parent", Some("z-root"), Some("other")),
                target("cycle-b", Some("cycle-a"), None),
                target("cycle-a", Some("cycle-b"), None),
                target("a-root", None, None),
            ],
        };

        let forest = connection.target_forest();
        assert_eq!(
            forest
                .iter()
                .filter(|node| node.parent_target_id.is_none())
                .map(|node| node.target.target_id.as_str())
                .collect::<Vec<_>>(),
            vec!["a-root", "z-root", "cycle-a"],
        );
        let z_root = forest
            .iter()
            .find(|node| node.target.target_id == "z-root")
            .unwrap();
        assert_eq!(z_root.connection_id, "connection");
        assert_eq!(z_root.connection_generation, 7);
        assert_eq!(
            forest
                .iter()
                .filter(|node| node.parent_target_id.as_deref() == Some("z-root"))
                .map(|node| node.target.target_id.as_str())
                .collect::<Vec<_>>(),
            vec!["child-by-opener", "child-by-parent"],
        );
        assert_eq!(
            forest
                .iter()
                .find(|node| node.target.target_id == "cycle-b")
                .unwrap()
                .parent_target_id
                .as_deref(),
            Some("cycle-a"),
        );
    }

    fn target(target_id: &str, parent_id: Option<&str>, opener_id: Option<&str>) -> TargetSnapshot {
        TargetSnapshot {
            target_id: target_id.to_owned(),
            target_type: "node".to_owned(),
            title: target_id.to_owned(),
            url: String::new(),
            attached: true,
            parent_id: parent_id.map(str::to_owned),
            opener_id: opener_id.map(str::to_owned),
            browser_context_id: None,
            subtype: None,
        }
    }
}
