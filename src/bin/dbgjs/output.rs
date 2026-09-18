use dbgjs::service_api::{
    AgentSessionSnapshot, BreakpointPendingReason, BreakpointSnapshot, BreakpointStatus,
    CaptureSnapshot, CompactedSourceEdgeSnapshot, CompactedSourceGraphSnapshot,
    CompactedSourceNodeSnapshot, ConnectionConfiguration, ConnectionStatus, ConsoleMessageSnapshot,
    ContextSnapshot, ContextSummary, CoverageSnapshot, CpuProfileFunctionSnapshot,
    CpuProfileSnapshot, EvaluationSnapshot, FrameProjectionSnapshot, HeapAggregateSnapshot,
    HeapCaptureResult, HeapClassSnapshot, HeapClassSnapshotEntry, HeapDiffSnapshot,
    HeapDominatorSnapshot, HeapNodeSelectionSnapshot, HeapNodeSnapshot, HeapPathSnapshot,
    HeapReferencesSnapshot, HeapSnapshotProgress, HeapSnapshotResult, ObservationResult,
    PlaywrightChannel, ProcessRole, ProcessRootKind, ProcessSnapshot, ProcessTargetSnapshot,
    ProcessTreeSnapshot, PromiseSelectionSnapshot, PromiseSnapshot, ResourceGraphSnapshot,
    ServiceInfo, SourceContentSnapshot, SourceExcerpt, SourceFormattingMode,
    SourceFormattingSettings, SourceGraphViewSnapshot, SourceLocation, SourceMappingSnapshot,
    SourceSearchSnapshot, SourceSnapshotInfo, SourceTreeSnapshot, TargetAttachmentOutcome,
    TargetAttachmentResult, TargetBreakpointStatus, TargetDebuggerPhase, TargetDebuggerSnapshot,
    TargetSnapshot, UncompactedProjectionSnapshot, UncompactedSourceEdgeSnapshot,
    UncompactedSourceGraphSnapshot, UncompactedSourceNodeSnapshot,
    UncompactedSourceRevisionSnapshot, ValueSnapshot,
};
use dbgjs::coverage_filter::CoveragePathFilter;
use serde::Serialize;
use std::collections::{BTreeMap, BTreeSet};
use std::fmt::Write as _;
use std::io::IsTerminal;
use std::path::Path;

use super::bounded_tree::{BoundedTree, BoundedTreeStyle, TreeAggregate, TreeRenderOptions};

#[derive(Clone, Copy)]
pub enum OutputFormat {
    Human,
    Json,
}

pub struct CoverageOutputOptions<'a> {
    pub path: Option<&'a str>,
    pub path_glob: Option<&'a str>,
    pub all: bool,
    pub max_lines: usize,
    pub trim_width: bool,
}

#[derive(Clone, Copy)]
pub enum CpuProfileView {
    Functions,
    Files,
}

#[derive(Clone, Copy)]
pub enum CpuProfileSort {
    SelfTime,
    TotalTime,
}

pub struct CpuProfileOutputOptions<'a> {
    pub path: Option<&'a str>,
    pub view: CpuProfileView,
    pub sort: CpuProfileSort,
    pub max_lines: usize,
}

#[derive(Clone, Copy)]
pub struct ProcessTreeOutputOptions<'a> {
    pub root_kind: ProcessRootKind,
    pub command_line: bool,
    pub stats: bool,
    pub filter: Option<&'a str>,
    pub trim_width: bool,
}

impl Default for ProcessTreeOutputOptions<'_> {
    fn default() -> Self {
        Self {
            root_kind: ProcessRootKind::Vscode,
            command_line: true,
            stats: false,
            filter: None,
            trim_width: true,
        }
    }
}

pub struct HeapClassOutputOptions {
    pub all: bool,
    pub max_lines: usize,
    pub instances: bool,
    pub sort_by_instances: bool,
    pub trim_width: bool,
}

#[derive(Clone, Copy)]
pub struct SourceTreeOutputOptions {
    pub all: bool,
    pub max_lines: usize,
    pub trim_width: bool,
}

#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ConnectionListEntry {
    pub id: String,
    pub selected: bool,
    pub configuration: ConnectionConfiguration,
    pub generation: u64,
    pub status: ConnectionStatus,
    pub target_count: usize,
}

#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ConnectionListOutput {
    pub agent_instance_id: String,
    pub context_id: String,
    pub revision: u64,
    pub connections: Vec<ConnectionListEntry>,
}

#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct TargetListEntry {
    pub connection_id: String,
    pub connection_generation: u64,
    pub selected: bool,
    pub parent_target_id: Option<String>,
    #[serde(flatten)]
    pub target: TargetSnapshot,
}

#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct TargetListOutput {
    pub agent_instance_id: String,
    pub context_id: String,
    pub revision: u64,
    pub targets: Vec<TargetListEntry>,
}

impl OutputFormat {
    pub fn print_heap_map_supplied(&self, capture: &str, script: &str) -> Result<(), serde_json::Error> {
        match self {
            Self::Json => println!("{}", serde_json::to_string(&serde_json::json!({
                "captureId": capture, "scriptId": script, "sourceMapSaved": true
            }))?),
            Self::Human => println!("Source map saved for script:{script} in heap capture '{capture}'."),
        }
        Ok(())
    }

    pub fn is_json(self) -> bool {
        matches!(self, Self::Json)
    }

    pub fn print_eval(&self, value: &ValueSnapshot, full: bool) -> Result<(), serde_json::Error> {
        self.print(value)?;
        if matches!(self, Self::Human) {
            if let Some(guidance) = eval_truncation_guidance(value, full) {
                println!("{guidance}");
            }
        }
        Ok(())
    }

    pub fn from_arguments(arguments: &mut Vec<String>) -> Self {
        if arguments
            .first()
            .is_some_and(|argument| argument == "--json")
        {
            arguments.remove(0);
            Self::Json
        } else {
            Self::Human
        }
    }

    pub fn print_target(
        &self,
        value: &TargetDebuggerSnapshot,
        selector: &str,
    ) -> Result<(), serde_json::Error> {
        match self {
            Self::Human => print_target_human(value, selector),
            Self::Json => println!("{}", serde_json::to_string_pretty(value)?),
        }
        Ok(())
    }

    pub fn print_context_deleted(
        &self,
        context_id: &str,
        deleted: bool,
    ) -> Result<(), serde_json::Error> {
        match self {
            Self::Human => {
                if deleted {
                    println!("Deleted context {context_id}.");
                } else {
                    println!("Context {context_id} was not deleted.");
                }
            }
            Self::Json => println!("{}", serde_json::to_string_pretty(&deleted)?),
        }
        Ok(())
    }

    pub fn print_breakpoint(
        &self,
        context: &ContextSnapshot,
        breakpoint_id: &str,
        sources: &[SourceExcerpt],
    ) -> Result<(), serde_json::Error> {
        let breakpoint = context
            .breakpoints
            .iter()
            .find(|breakpoint| breakpoint.id == breakpoint_id)
            .expect("updated context contains the requested breakpoint");
        match self {
            Self::Human => {
                print_breakpoint_human(breakpoint);
                for source in sources {
                    println!();
                    print_source_excerpt("Source", source);
                }
            }
            Self::Json => println!("{}", serde_json::to_string_pretty(breakpoint)?),
        }
        Ok(())
    }

    pub fn print_target_with_breakpoint_sources(
        &self,
        snapshot: &TargetDebuggerSnapshot,
        selector: &str,
        breakpoint_ids: &[String],
    ) -> Result<(), serde_json::Error> {
        match self {
            Self::Human => {
                print_target_human(snapshot, selector);
                for breakpoint in snapshot
                    .breakpoints
                    .iter()
                    .filter(|breakpoint| breakpoint_ids.contains(&breakpoint.id))
                {
                    if let Some(source) = &breakpoint.source {
                        println!();
                        print_source_excerpt(&format!("Breakpoint {}", breakpoint.id), source);
                    } else if !matches!(breakpoint.status, TargetBreakpointStatus::Installed { .. })
                    {
                        println!(
                            "Breakpoint {}: {}",
                            breakpoint.id,
                            target_breakpoint_status(&breakpoint.status)
                        );
                    }
                }
                Ok(())
            }
            Self::Json => self.print(snapshot),
        }
    }

    pub fn print_target_with_watches(
        &self,
        value: &TargetDebuggerSnapshot,
        selector: &str,
        watches: &[EvaluationSnapshot],
    ) -> Result<(), serde_json::Error> {
        match self {
            Self::Human => {
                self.print_target(value, selector)?;
                if !watches.is_empty() {
                    println!("Watches:");
                    for watch in watches {
                        println!("  {}: {}", watch.expression, render_evaluation(watch));
                    }
                }
            }
            Self::Json => println!(
                "{}",
                serde_json::to_string_pretty(&serde_json::json!({
                    "target": value,
                    "watches": watches,
                }))?
            ),
        }
        Ok(())
    }

    pub fn print_coverage_capture(
        &self,
        value: &CoverageSnapshot,
        capture_id: &str,
    ) -> Result<(), serde_json::Error> {
        match self {
            Self::Human => println!("Captured {capture_id}."),
            Self::Json => println!("{}", serde_json::to_string_pretty(value)?),
        }
        Ok(())
    }

    pub fn print_coverage(
        &self,
        value: &CoverageSnapshot,
        options: CoverageOutputOptions<'_>,
    ) -> Result<(), Box<dyn std::error::Error>> {
        let filter = CoveragePathFilter::new(options.path, options.path_glob)
            .map_err(|error| std::io::Error::new(std::io::ErrorKind::InvalidInput, error))?;
        let filtered = if options.path.is_some() || options.path_glob.is_some() {
            let mut filtered = value.clone();
            filter.apply(&mut filtered)
                .map_err(|error| std::io::Error::new(std::io::ErrorKind::InvalidInput, error))?;
            Some(filtered)
        } else {
            None
        };
        let value = filtered.as_ref().unwrap_or(value);
        match self {
            Self::Human => print_coverage_human(value, options),
            Self::Json => println!("{}", serde_json::to_string_pretty(value)?),
        }
        Ok(())
    }

    pub fn print_logs(
        &self,
        snapshot: &dbgjs::service_api::TargetLogSnapshot,
        after: u64,
        limit: usize,
    ) -> Result<u64, serde_json::Error> {
        let logs = &snapshot.messages;
        let (skipped, displayed) = page_logs(logs, after, limit);
        let next = logs.last().map_or(after, |message| message.index.max(after));
        let evicted_since_cursor = logs.first().map_or(0, |message| {
            message.index.saturating_sub(after.saturating_add(1))
        });
        match self {
            Self::Human => {
                println!("{}", log_coverage_human(snapshot, displayed.is_empty(), after));
                if skipped > 0 {
                    println!(
                        "[...skipped {skipped} entries: {evicted_since_cursor} evicted, {} omitted by limit...]",
                        skipped.saturating_sub(evicted_since_cursor)
                    );
                }
                for message in &displayed {
                    println!("[{}] {}", message.index, message.values.join(" "));
                }
            }
            Self::Json => println!(
                "{}",
                serde_json::to_string_pretty(&serde_json::json!({
                    "after": after,
                    "skipped": skipped,
                    "evictedSinceCursor": evicted_since_cursor,
                    "omittedByLimit": skipped.saturating_sub(evicted_since_cursor),
                    "messages": displayed,
                    "nextCursor": next,
                    "contextId": snapshot.context_id,
                    "connectionId": snapshot.connection_id,
                    "targetId": snapshot.target_id,
                    "connectionGeneration": snapshot.connection_generation,
                    "capture": snapshot.capture,
                }))?
            ),
        }
        Ok(next)
    }

    pub fn print_coverage_stopped(&self, capture_id: &str) -> Result<(), serde_json::Error> {
        match self {
            Self::Human => println!("Coverage recording stopped. Captured {capture_id}."),
            Self::Json => println!(
                "{}",
                serde_json::to_string_pretty(&serde_json::json!({ "captureId": capture_id }))?
            ),
        }
        Ok(())
    }

    pub fn print_cpu_profile(
        &self,
        value: &CpuProfileSnapshot,
        options: CpuProfileOutputOptions<'_>,
    ) -> Result<(), serde_json::Error> {
        match self {
            Self::Human => print_cpu_profile_human(value, options),
            Self::Json => println!("{}", serde_json::to_string_pretty(value)?),
        }
        Ok(())
    }

    pub fn print_cpu_profile_started(
        &self,
        sampling_interval_micros: Option<u64>,
    ) -> Result<(), serde_json::Error> {
        match self {
            Self::Human => match sampling_interval_micros {
                Some(interval) => {
                    println!("CPU profile recording started ({interval}us sampling interval).")
                }
                None => {
                    println!("CPU profile recording started (runtime default sampling interval).")
                }
            },
            Self::Json => println!(
                "{}",
                serde_json::to_string_pretty(&serde_json::json!({
                    "started": true,
                    "samplingIntervalMicros": sampling_interval_micros,
                }))?
            ),
        }
        Ok(())
    }

    pub fn print_cpu_profile_stopped(
        &self,
        profile: &CpuProfileSnapshot,
    ) -> Result<(), serde_json::Error> {
        match self {
            Self::Human => println!(
                "CPU profile recording stopped. Captured {}.",
                profile.capture_id
            ),
            Self::Json => println!("{}", serde_json::to_string_pretty(profile)?),
        }
        Ok(())
    }

    pub fn print_cpu_profile_exported(&self, path: &Path) -> Result<(), serde_json::Error> {
        match self {
            Self::Human => println!("Exported CPU profile to {}.", path.display()),
            Self::Json => println!(
                "{}",
                serde_json::to_string_pretty(&serde_json::json!({
                    "path": path,
                }))?
            ),
        }
        Ok(())
    }

    pub fn print_screenshot_captured(
        &self,
        path: &Path,
        byte_length: usize,
        width: u32,
        height: u32,
        media_type: &str,
    ) -> Result<(), serde_json::Error> {
        match self {
            Self::Human => println!(
                "Captured {width}x{height} screenshot to {}.",
                path.display()
            ),
            Self::Json => println!(
                "{}",
                serde_json::to_string_pretty(&serde_json::json!({
                    "path": path,
                    "mediaType": media_type,
                    "byteLength": byte_length,
                    "width": width,
                    "height": height,
                }))?
            ),
        }
        Ok(())
    }

    pub fn print<T>(&self, value: &T) -> Result<(), serde_json::Error>
    where
        T: HumanOutput + Serialize,
    {
        match self {
            Self::Human => value.print_human(),
            Self::Json => println!("{}", serde_json::to_string_pretty(value)?),
        }
        Ok(())
    }

    pub fn print_process_trees(
        &self,
        trees: &[ProcessTreeSnapshot],
        options: ProcessTreeOutputOptions<'_>,
    ) -> Result<(), serde_json::Error> {
        match self {
            Self::Human => print_process_trees_human(trees, options),
            Self::Json => println!(
                "{}",
                serde_json::to_string_pretty(&process_trees_json(trees, options)?)?
            ),
        }
        Ok(())
    }

    pub fn print_heap_snapshot_progress(
        &self,
        progress: &HeapSnapshotProgress,
    ) -> Result<(), serde_json::Error> {
        match self {
            Self::Human => {
                if progress.total > 0 {
                    eprintln!(
                        "Heap snapshot: {}/{} ({:.1}%), {} bytes",
                        progress.done,
                        progress.total,
                        progress.done as f64 * 100.0 / progress.total as f64,
                        progress.bytes_written
                    );
                } else {
                    eprintln!(
                        "Heap snapshot: {}/{} objects, {} bytes",
                        progress.done, progress.total, progress.bytes_written
                    );
                }
            }
            Self::Json => eprintln!("{}", serde_json::to_string(progress)?),
        }
        Ok(())
    }

    pub fn print_heap_classes(
        &self,
        snapshot: &HeapClassSnapshot,
        options: HeapClassOutputOptions,
    ) -> Result<(), serde_json::Error> {
        match self {
            Self::Human => print_heap_classes_human(snapshot, options),
            Self::Json => println!("{}", serde_json::to_string_pretty(snapshot)?),
        }
        Ok(())
    }

    pub fn print_heap_show(
        &self,
        snapshot: &HeapReferencesSnapshot,
    ) -> Result<(), serde_json::Error> {
        match self {
            Self::Human => {
                for line in heap_show_lines(snapshot) {
                    println!("{line}");
                }
            }
            Self::Json => println!("{}", serde_json::to_string_pretty(snapshot)?),
        }
        Ok(())
    }

    pub fn print_source_tree(
        &self,
        snapshot: &SourceTreeSnapshot,
        options: SourceTreeOutputOptions,
    ) -> Result<(), serde_json::Error> {
        match self {
            Self::Human => print_source_tree_human(snapshot, options),
            Self::Json => println!("{}", serde_json::to_string_pretty(snapshot)?),
        }
        Ok(())
    }
}

fn log_coverage_human(
    snapshot: &dbgjs::service_api::TargetLogSnapshot,
    empty: bool,
    after: u64,
) -> String {
    use dbgjs::service_api::LogCaptureStatus;
    let capture = &snapshot.capture;
    let status = match capture.status {
        LogCaptureStatus::Active => "active",
        LogCaptureStatus::Inactive => "inactive (target is not attached)",
        LogCaptureStatus::Stopped => "stopped",
        LogCaptureStatus::Unknown => "unknown",
    };
    let mut text = format!(
        "Log capture: {status}; target {}/{}/{}; generation {}.\nCollected events: {}.\nStarted (Unix ms): {}; session: {}; capture: {}.\nRetained: {}; evicted: {}; dropped before retention: {}.\nBrowser diagnostics (Log.entryAdded), uncaught exceptions (Runtime.exceptionThrown), and network failures are not collected.",
        snapshot.context_id, snapshot.connection_id, snapshot.target_id, snapshot.connection_generation,
        if capture.collected_events.is_empty() { "none".to_owned() } else { capture.collected_events.join(", ") },
        capture.started_at_unix_ms.map_or_else(|| "unknown".to_owned(), |value| value.to_string()),
        capture.session_id.as_deref().unwrap_or("unknown"),
        capture.capture_id.as_deref().unwrap_or("unknown"),
        snapshot.messages.len(),
        capture.evicted_count.map_or_else(|| "unknown".to_owned(), |value| value.to_string()),
        capture.dropped_count.map_or_else(|| "unknown".to_owned(), |value| value.to_string()),
    );
    if empty {
        text.push_str(if after == 0 {
            "\nNo captured entries to display; this does not mean no errors occurred."
        } else {
            "\nNo captured entries to display after the cursor; this does not mean no errors occurred."
        });
    }
    text
}

fn page_logs(
    logs: &[ConsoleMessageSnapshot],
    after: u64,
    limit: usize,
) -> (u64, Vec<&ConsoleMessageSnapshot>) {
    let unseen = logs
        .iter()
        .filter(|message| message.index > after)
        .collect::<Vec<_>>();
    let evicted = unseen.first().map_or(0, |message| {
        message.index.saturating_sub(after.saturating_add(1))
    });
    let retained_skipped = unseen.len().saturating_sub(limit);
    (
        evicted.saturating_add(retained_skipped as u64),
        unseen.into_iter().skip(retained_skipped).collect(),
    )
}

pub trait HumanOutput {
    fn print_human(&self);
}

impl HumanOutput for ServiceInfo {
    fn print_human(&self) {
        println!("Debugger service is running.");
        println!("  Process: {}", self.process_id);
    }
}

impl HumanOutput for ResourceGraphSnapshot {
    fn print_human(&self) {
        println!(
            "Resource graph revision {} ({} resources, {} relations)",
            self.revision,
            self.resources.len(),
            self.relations.len()
        );
        for resource in &self.resources {
            let kinds = resource.kinds.join(",");
            let capabilities = resource
                .capabilities
                .iter()
                .map(|capability| capability.kind.as_str())
                .collect::<Vec<_>>()
                .join(",");
            let label = resource.label.as_deref().unwrap_or("");
            println!(
                "  {}  [{}] {}{}",
                resource.id,
                kinds,
                label,
                if capabilities.is_empty() {
                    String::new()
                } else {
                    format!("  <{capabilities}>")
                }
            );
        }
        for relation in &self.relations {
            println!("  {} -{}-> {}", relation.from, relation.kind, relation.to);
        }
    }
}

impl HumanOutput for bool {
    fn print_human(&self) {
        println!(
            "{}",
            if *self {
                "Debugger service stopped."
            } else {
                "Debugger service did not stop."
            }
        );
    }
}

impl HumanOutput for ObservationResult {
    fn print_human(&self) {
        println!(
            "{}",
            serde_json::to_string_pretty(self).expect("serializable")
        );
    }
}

impl HumanOutput for Vec<SourceSnapshotInfo> {
    fn print_human(&self) {
        for source in self {
            println!("{}  [{}:{}]", source.path, source.kind, source.status);
        }
    }
}

impl HumanOutput for SourceContentSnapshot {
    fn print_human(&self) {
        if self.start_line == 1 && self.end_line == self.total_lines {
            print!("{}", self.content);
            return;
        }

        println!(
            "{} (lines {}-{} of {})",
            self.path, self.start_line, self.end_line, self.total_lines
        );
        for (index, line) in self.content.lines().enumerate() {
            println!("{:>6} | {line}", self.start_line + index as u32);
        }
    }
}

impl HumanOutput for SourceFormattingSettings {
    fn print_human(&self) {
        println!(
            "DEFAULT  {}",
            source_formatting_mode_label(self.default_mode)
        );
        if self.rules.is_empty() {
            return;
        }
        println!();
        println!("RULE    MODE  TARGET  URL");
        for rule in &self.rules {
            println!(
                "{:<7} {:<5} {:<7} {}",
                rule.id,
                source_formatting_mode_label(rule.mode),
                rule.target_pattern.as_deref().unwrap_or("*"),
                rule.url_pattern.as_deref().unwrap_or("*")
            );
        }
    }
}

fn source_formatting_mode_label(mode: SourceFormattingMode) -> &'static str {
    match mode {
        SourceFormattingMode::Off => "off",
        SourceFormattingMode::Auto => "auto",
        SourceFormattingMode::On => "on",
    }
}

impl HumanOutput for SourceSearchSnapshot {
    fn print_human(&self) {
        print!("{}", render_source_search(self));
    }
}

fn render_source_search(snapshot: &SourceSearchSnapshot) -> String {
    let mut output = String::new();
    for item in &snapshot.matches {
        writeln!(output, "{}:{}:{}", item.path, item.line, item.column).unwrap();
        let first_context_line = item.line.saturating_sub(item.before_context.len() as u32);
        let width = (item.line + item.after_context.len() as u32).to_string().len();
        for (index, text) in item.before_context.iter().enumerate() {
            let line = first_context_line + index as u32;
            writeln!(output, "    {line:>width$} | {text}").unwrap();
        }
        writeln!(output, "  > {:>width$} | {}", item.line, item.text).unwrap();
        for (index, text) in item.after_context.iter().enumerate() {
            let line = item.line + index as u32 + 1;
            writeln!(output, "    {line:>width$} | {text}").unwrap();
        }
        output.push('\n');
    }
    if snapshot.omitted_matches > 0 {
        writeln!(
            output,
            "... {} additional matches omitted; increase --max-results",
            snapshot.omitted_matches
        )
        .unwrap();
    }
    if let Some(message) = source_search_incomplete_message(snapshot) {
        writeln!(output, "{message}").unwrap();
    }
    writeln!(
        output,
        "{} source(s) searched, {} skipped",
        snapshot.searched_sources, snapshot.skipped_sources
    )
    .unwrap();
    for source in &snapshot.skipped {
        writeln!(
            output,
            "Skipped {} ({}): {}",
            source.path, source.kind, source.reason
        )
        .unwrap();
    }
    output
}

fn source_search_incomplete_message(snapshot: &SourceSearchSnapshot) -> Option<String> {
    (snapshot.searched_sources == 0 && snapshot.skipped_sources > 0).then(|| {
        format!(
            "Search incomplete: no sources were searched because all {} candidate source(s) were skipped.",
            snapshot.skipped_sources
        )
    })
}

impl HumanOutput for Vec<SourceGraphViewSnapshot> {
    fn print_human(&self) {
        if self.is_empty() {
            println!("No resolved source-map view contains this source.");
            return;
        }
        for view in self {
            println!(
                "{} / {}  {} source",
                view.connection_id, view.target_id, view.role
            );
            println!("  Source: {}", view.source_path);
            println!("  Runtime: {}", view.generated_url);
            println!("  Kind: {}", view.kind);
            println!("  Content: {}", view.primary_provenance);
            for alternative in &view.alternative_provenance {
                println!("  Alternative content: {alternative}");
            }
            println!("  View: {} resolved source(s)", view.resolved_source_count);
            for projection in &view.projection_paths {
                println!("  Projection: {}", projection.generated_url);
                for step in &projection.steps {
                    println!("    -> {step}");
                }
                println!("    -> {}", view.source_path);
            }
            for diagnostic in &view.diagnostics {
                println!("  Diagnostic: {diagnostic}");
            }
        }
    }
}

impl HumanOutput for CompactedSourceGraphSnapshot {
    fn print_human(&self) {
        print!("{}", render_compacted_source_graph(self));
    }
}

impl HumanOutput for UncompactedSourceGraphSnapshot {
    fn print_human(&self) {
        print!("{}", render_uncompacted_source_graph(self));
    }
}

#[derive(Clone, Default)]
struct SourceTreeMetrics {
    sources: usize,
    snapshots: usize,
}

impl TreeAggregate for SourceTreeMetrics {
    fn merge(&mut self, other: &Self) {
        self.sources += other.sources;
        self.snapshots += other.snapshots;
    }
}

struct SourceTreeStyle;

impl BoundedTreeStyle<SourceTreeMetrics, ()> for SourceTreeStyle {
    fn sort_weight(&self, aggregate: &SourceTreeMetrics) -> u64 {
        aggregate.sources as u64
    }

    fn expansion_weight(&self, node: &BoundedTree<SourceTreeMetrics, ()>, _: bool) -> u64 {
        if node.children().is_empty() {
            0
        } else {
            node.aggregate().sources as u64
        }
    }

    fn render_node(
        &self,
        label: &str,
        node: &BoundedTree<SourceTreeMetrics, ()>,
        _: &str,
        _: bool,
    ) -> String {
        if node.leaf().is_some() {
            let snapshots = node.aggregate().snapshots;
            if snapshots > 1 {
                format!("{label}  [{snapshots} snapshots]")
            } else {
                label.to_owned()
            }
        } else {
            format!(
                "{label}/  [{}]",
                source_tree_metrics_label(node.aggregate())
            )
        }
    }

    fn render_leaf_children(
        &self,
        _: &str,
        _: &BoundedTree<SourceTreeMetrics, ()>,
        _: usize,
        _: bool,
    ) -> Vec<String> {
        Vec::new()
    }

    fn render_omitted(
        &self,
        hidden_items: usize,
        _: usize,
        aggregate: &SourceTreeMetrics,
    ) -> String {
        format!(
            "[{hidden_items} items, {}]",
            source_tree_metrics_label(aggregate)
        )
    }

    fn render_all_pruned(
        &self,
        child_count: usize,
        _: usize,
        aggregate: &SourceTreeMetrics,
    ) -> String {
        format!(
            "[all {child_count} children pruned, {}]",
            source_tree_metrics_label(aggregate)
        )
    }
}

fn print_source_tree_human(snapshot: &SourceTreeSnapshot, options: SourceTreeOutputOptions) {
    for line in source_tree_lines(snapshot, options) {
        println!("{line}");
    }
}

fn source_tree_lines(
    snapshot: &SourceTreeSnapshot,
    options: SourceTreeOutputOptions,
) -> Vec<String> {
    if snapshot.sources.is_empty() {
        return vec![format!(
            "No {} sources are currently observed.",
            match snapshot.kind {
                dbgjs::service_api::SourceTreeKind::Loaded => "loaded",
                dbgjs::service_api::SourceTreeKind::SourceMapped => "source-mapped",
                dbgjs::service_api::SourceTreeKind::Formatted => "formatted",
                dbgjs::service_api::SourceTreeKind::Resolved => "resolved",
            }
        )];
    }
    let mut by_uri = BTreeMap::<String, usize>::new();
    for source in &snapshot.sources {
        *by_uri.entry(source.uri.clone()).or_default() += 1;
    }
    let mut tree = BoundedTree::default();
    for (uri, snapshot_count) in by_uri {
        tree.insert(
            source_tree_components(&uri),
            SourceTreeMetrics {
                sources: 1,
                snapshots: snapshot_count,
            },
            (),
        );
    }
    let maximum_lines = if options.all {
        usize::MAX
    } else {
        options.max_lines
    };
    let trim_width = options.trim_width && !options.all;
    tree.render_with_options(
        &SourceTreeStyle,
        false,
        TreeRenderOptions::terminal(maximum_lines, trim_width),
    )
}

fn source_tree_metrics_label(metrics: &SourceTreeMetrics) -> String {
    if metrics.snapshots > metrics.sources {
        format!(
            "{} source{}, {} snapshots",
            metrics.sources,
            if metrics.sources == 1 { "" } else { "s" },
            metrics.snapshots
        )
    } else {
        format!(
            "{} source{}",
            metrics.sources,
            if metrics.sources == 1 { "" } else { "s" }
        )
    }
}

fn source_tree_components(uri: &str) -> Vec<String> {
    let Ok(url) = url::Url::parse(uri) else {
        return uri
            .split('/')
            .filter(|component| !component.is_empty())
            .map(str::to_owned)
            .collect();
    };
    if url.cannot_be_a_base() {
        return vec![uri.to_owned()];
    }
    let root = url[..url::Position::BeforePath].to_owned();
    let mut components = vec![root];
    components.extend(
        url.path_segments()
            .into_iter()
            .flatten()
            .filter(|component| !component.is_empty())
            .map(str::to_owned),
    );
    if let Some(last) = components.last_mut() {
        if let Some(query) = url.query() {
            last.push('?');
            last.push_str(query);
        }
        if let Some(fragment) = url.fragment() {
            last.push('#');
            last.push_str(fragment);
        }
    }
    components
}

fn render_uncompacted_source_graph(graph: &UncompactedSourceGraphSnapshot) -> String {
    if graph.nodes.is_empty() {
        return "No sources are currently observed.\n".to_owned();
    }
    let nodes = graph
        .nodes
        .iter()
        .map(|node| (node.id, node))
        .collect::<BTreeMap<_, _>>();
    let mut edges = BTreeMap::<u64, Vec<&UncompactedSourceEdgeSnapshot>>::new();
    for edge in &graph.edges {
        edges.entry(edge.derived).or_default().push(edge);
    }
    for outgoing in edges.values_mut() {
        outgoing.sort_by_key(|edge| {
            (
                &edge.projection,
                nodes.get(&edge.basis).map(|node| node.uri.as_str()),
                edge.basis,
            )
        });
    }
    let mut output = String::new();
    let mut visited = BTreeSet::new();
    for (index, root) in graph.roots.iter().enumerate() {
        if index > 0 {
            output.push('\n');
        }
        render_uncompacted_source_node(&mut output, *root, "", &nodes, &edges, &mut visited);
    }
    output
}

fn render_uncompacted_source_node(
    output: &mut String,
    id: u64,
    indent: &str,
    nodes: &BTreeMap<u64, &UncompactedSourceNodeSnapshot>,
    edges: &BTreeMap<u64, Vec<&UncompactedSourceEdgeSnapshot>>,
    visited: &mut BTreeSet<u64>,
) {
    let Some(node) = nodes.get(&id) else {
        return;
    };
    writeln!(output, "{}", uncompacted_source_node_label(node)).unwrap();
    if !visited.insert(id) {
        return;
    }
    let outgoing = edges.get(&id).map(Vec::as_slice).unwrap_or_default();
    for (index, edge) in outgoing.iter().enumerate() {
        let last = index + 1 == outgoing.len();
        let branch = if last { "└─" } else { "├─" };
        let child_indent = format!("{indent}{}", if last { "   " } else { "│  " });
        let Some(target) = nodes.get(&edge.basis) else {
            continue;
        };
        write!(
            output,
            "{indent}{branch} projection #{} {} → ",
            edge.id,
            uncompacted_projection_label(&edge.projection)
        )
        .unwrap();
        if visited.contains(&edge.basis) {
            writeln!(output, "{} ↩", uncompacted_source_node_label(target)).unwrap();
        } else {
            render_uncompacted_source_node(
                output,
                edge.basis,
                &child_indent,
                nodes,
                edges,
                visited,
            );
        }
    }
}

fn uncompacted_source_node_label(node: &UncompactedSourceNodeSnapshot) -> String {
    format!(
        "#{} {}  [{}]",
        node.id,
        node.uri,
        uncompacted_revision_label(&node.revision)
    )
}

fn uncompacted_revision_label(revision: &UncompactedSourceRevisionSnapshot) -> String {
    match revision {
        UncompactedSourceRevisionSnapshot::Content { hash } => format!("content:{hash}"),
        UncompactedSourceRevisionSnapshot::Version { namespace, value } => {
            format!("{namespace}:{value}")
        }
    }
}

fn uncompacted_projection_label(projection: &UncompactedProjectionSnapshot) -> String {
    match projection {
        UncompactedProjectionSnapshot::IdentityEqualContent { content_hash } => {
            format!("identity [equal content {content_hash}]")
        }
        UncompactedProjectionSnapshot::IdentityDeclaredByProvider { provider } => {
            format!("identity [declared by {provider}]")
        }
        UncompactedProjectionSnapshot::SourceMap {
            map_hash,
            source_index,
        } => format!("source map [{map_hash}, source {source_index}]"),
        UncompactedProjectionSnapshot::Format { formatter } => {
            format!("format [{formatter}]")
        }
        UncompactedProjectionSnapshot::Edit { edit } => format!("edit [{edit}]"),
        UncompactedProjectionSnapshot::Offset {
            line_delta,
            column_delta,
        } => format!("offset [{line_delta:+} lines, {column_delta:+} columns]"),
    }
}

fn render_compacted_source_graph(graph: &CompactedSourceGraphSnapshot) -> String {
    if graph.nodes.is_empty() {
        return "No sources are currently observed.\n".to_owned();
    }
    let mut output = String::new();
    let nodes = graph
        .nodes
        .iter()
        .map(|node| (node.id, node))
        .collect::<BTreeMap<_, _>>();
    let mut edges = BTreeMap::<u32, Vec<&CompactedSourceEdgeSnapshot>>::new();
    for edge in &graph.edges {
        edges.entry(edge.derived).or_default().push(edge);
    }
    for outgoing in edges.values_mut() {
        outgoing.sort_by_key(|edge| {
            (
                edge.kind.as_str(),
                nodes.get(&edge.basis).map(|node| node.prefix.as_str()),
                edge.basis,
            )
        });
    }
    let connected = graph
        .edges
        .iter()
        .flat_map(|edge| [edge.derived, edge.basis])
        .collect::<BTreeSet<_>>();
    let mut visited = BTreeSet::new();
    for (index, root) in graph.roots.iter().enumerate() {
        if index > 0 {
            output.push('\n');
        }
        render_source_graph_node(
            &mut output,
            *root,
            "",
            false,
            &nodes,
            &edges,
            &connected,
            &mut visited,
        );
    }
    output
}

fn render_source_graph_node(
    output: &mut String,
    id: u32,
    indent: &str,
    wildcard: bool,
    nodes: &BTreeMap<u32, &CompactedSourceNodeSnapshot>,
    edges: &BTreeMap<u32, Vec<&CompactedSourceEdgeSnapshot>>,
    connected: &BTreeSet<u32>,
    visited: &mut BTreeSet<u32>,
) {
    let Some(node) = nodes.get(&id) else {
        return;
    };
    writeln!(output, "{}", source_graph_node_label(node, wildcard)).unwrap();
    if !visited.insert(id) {
        return;
    }
    let outgoing = edges.get(&id).map(Vec::as_slice).unwrap_or_default();
    let visible_sources = (!connected.contains(&id) && node.listed_source_paths.len() > 1)
        .then_some(node.listed_source_paths.as_slice())
        .unwrap_or_default();
    let child_count = visible_sources.len() + outgoing.len();
    for (index, source) in visible_sources.iter().enumerate() {
        let last = index + 1 == child_count;
        let branch = if last { "└─" } else { "├─" };
        writeln!(output, "{indent}{branch} source {source}").unwrap();
    }
    for (edge_index, edge) in outgoing.iter().enumerate() {
        let index = visible_sources.len() + edge_index;
        let last = index + 1 == child_count;
        let branch = if last { "└─" } else { "├─" };
        let child_indent = format!("{indent}{}", if last { "   " } else { "│  " });
        let Some(target) = nodes.get(&edge.basis) else {
            continue;
        };
        write!(
            output,
            "{indent}{branch} {} → ",
            source_graph_edge_label(edge)
        )
        .unwrap();
        if visited.contains(&edge.basis) {
            writeln!(
                output,
                "{} ↩",
                source_graph_node_label(target, edge.fan_out)
            )
            .unwrap();
        } else {
            render_source_graph_node(
                output,
                edge.basis,
                &child_indent,
                edge.fan_out,
                nodes,
                edges,
                connected,
                visited,
            );
        }
    }
}

fn source_graph_node_label(node: &CompactedSourceNodeSnapshot, wildcard: bool) -> String {
    let wildcard = if wildcard {
        if node.prefix.ends_with('/') {
            "*"
        } else {
            "/*"
        }
    } else {
        ""
    };
    let snapshots = if node.snapshot_count > node.source_count {
        format!(
            ", {} snapshot{}",
            node.snapshot_count,
            if node.snapshot_count == 1 { "" } else { "s" }
        )
    } else {
        String::new()
    };
    format!(
        "#{} {}{}  [{} source{}{}]{}",
        node.id,
        node.prefix,
        wildcard,
        node.source_count,
        if node.source_count == 1 { "" } else { "s" },
        snapshots,
        if node.runtime_internal {
            " [internal]"
        } else {
            ""
        }
    )
}

fn source_graph_edge_label(edge: &CompactedSourceEdgeSnapshot) -> String {
    let rewrite = edge
        .suffix_rewrite
        .as_ref()
        .map_or_else(String::new, |rewrite| {
            format!(", {} → {}", rewrite.from, rewrite.to)
        });
    let fan_out = if edge.fan_out { ", fan-out" } else { "" };
    format!(
        "{}  [{} mapping{}{}{}]",
        edge.kind,
        edge.mapping_count,
        if edge.mapping_count == 1 { "" } else { "s" },
        fan_out,
        rewrite
    )
}

impl HumanOutput for Vec<SourceMappingSnapshot> {
    fn print_human(&self) {
        if self.is_empty() {
            println!("No mapping found.");
            return;
        }
        for mapping in self {
            println!(
                "{} / {}  {}  {}:{}:{}  [{}]",
                mapping.connection_id,
                mapping.target_id,
                mapping.direction,
                mapping.source_url,
                mapping.line,
                mapping.column,
                mapping.quality
            );
        }
    }
}

impl HumanOutput for Vec<String> {
    fn print_human(&self) {
        for item in self {
            println!("{item}");
        }
    }
}

impl HumanOutput for Vec<SourceLocation> {
    fn print_human(&self) {
        for item in self {
            println!("{}:{}:{}", item.source_url, item.line, item.column);
        }
    }
}

impl HumanOutput for u32 {
    fn print_human(&self) {
        println!("{self}");
    }
}

impl HumanOutput for HeapSnapshotResult {
    fn print_human(&self) {
        println!(
            "Heap snapshot written to {} ({} bytes) in {:.3}s: taking {:.3}s, retrieving {:.3}s.",
            self.path,
            self.bytes_written,
            self.timing.total_duration_micros() as f64 / 1_000_000.0,
            self.timing.taking_duration_micros as f64 / 1_000_000.0,
            self.timing.retrieving_duration_micros as f64 / 1_000_000.0,
        );
    }
}

impl HumanOutput for HeapCaptureResult {
    fn print_human(&self) {
        println!(
            "Captured {} ({}) in {:.3}s: taking {:.3}s, retrieving {:.3}s.",
            self.capture_id,
            compact_bytes(self.bytes_written),
            self.timing.total_duration_micros() as f64 / 1_000_000.0,
            self.timing.taking_duration_micros as f64 / 1_000_000.0,
            self.timing.retrieving_duration_micros as f64 / 1_000_000.0,
        );
    }
}

impl HumanOutput for PromiseSnapshot {
    fn print_human(&self) {
        println!("{}", promise_line(self));
    }
}

impl HumanOutput for PromiseSelectionSnapshot {
    fn print_human(&self) {
        println!(
            "{} of {} retained promise(s) selected from '{}' (graph {} in {}).",
            self.promises.len(),
            self.total_promises,
            self.capture_id,
            if self.used_cached_graph {
                "reused"
            } else {
                "parsed"
            },
            format_profile_time(self.graph_parse_duration_micros),
        );
        for promise in &self.promises {
            println!("{}", promise_line(promise));
        }
        if self.omitted_promise_count > 0 {
            println!("... {} promises omitted", self.omitted_promise_count);
        }
    }
}

fn promise_line(promise: &PromiseSnapshot) -> String {
    let settlement = promise
        .settlement
        .as_ref()
        .map_or_else(String::new, |value| {
            let preview = value.preview.as_deref().unwrap_or("<no preview>");
            let truncated = if value.truncated { "..." } else { "" };
            let reference = value
                .reference
                .as_deref()
                .map(|reference| format!(" ({reference})"))
                .unwrap_or_default();
            format!(", settlement:{preview}{truncated}{reference}")
        });
    format!(
        "{}  state:{}, classification:{}{settlement}",
        promise.reference.as_deref().unwrap_or("<no reference>"),
        promise_state_name(promise.state),
        promise_classification_name(promise.classification),
    )
}

fn promise_state_name(state: dbgjs::service_api::PromiseState) -> &'static str {
    use dbgjs::service_api::PromiseState;
    match state {
        PromiseState::Pending => "pending",
        PromiseState::Fulfilled => "fulfilled",
        PromiseState::Rejected => "rejected",
        PromiseState::Unknown => "unknown",
    }
}

fn promise_classification_name(
    classification: dbgjs::service_api::PromiseClassification,
) -> &'static str {
    match classification {
        dbgjs::service_api::PromiseClassification::Indeterminate => "indeterminate",
    }
}

impl HumanOutput for HeapNodeSelectionSnapshot {
    fn print_human(&self) {
        println!(
            "{} nodes selected from '{}' ({} nodes, {} edges; graph {} in {}).",
            self.nodes.len(),
            self.capture_id,
            self.total_nodes,
            self.total_edges,
            if self.used_cached_graph {
                "reused"
            } else {
                "parsed"
            },
            format_profile_time(self.graph_parse_duration_micros),
        );
        for node in &self.nodes {
            println!("{}", heap_node_line(node));
        }
        if self.incomplete_string_count > 0 {
            println!(
                "{} reconstructed strings were incomplete and could not be matched conclusively.",
                self.incomplete_string_count
            );
        }
    }
}

impl HumanOutput for HeapReferencesSnapshot {
    fn print_human(&self) {
        println!("{}", heap_node_line(&self.node));
        for reference in &self.references {
            println!("{}", heap_reference_line(reference));
        }
        if self.omitted_reference_count > 0 {
            println!("  ... {} references omitted", self.omitted_reference_count);
        }
    }
}

fn heap_reference_line(reference: &dbgjs::service_api::HeapReferenceSnapshot) -> String {
    let label = reference
        .name
        .as_deref()
        .map(|name| format!(" {}", escaped_heap_text(name, false)))
        .unwrap_or_else(|| format!(" [{}]", reference.name_or_index));
    format!(
        "  {}{} --{}{}--> {}{}{}{}",
        reference.source,
        heap_preview_suffix(reference.source_preview.as_deref()),
        reference.edge_type,
        label,
        reference.target,
        heap_preview_suffix(reference.target_preview.as_deref()),
        heap_reference_source(&reference.source, &reference.source_locations),
        heap_reference_source(&reference.target, &reference.target_locations),
    )
}

fn heap_preview_suffix(preview: Option<&str>) -> String {
    preview.map(|preview| format!("  {preview}")).unwrap_or_default()
}

fn heap_reference_source(reference: &str, source: &dbgjs::object_inspection::ObjectSourceSnapshot) -> String {
    let rendered = render_object_source(source);
    if rendered.is_empty() {
        String::new()
    } else {
        format!("\n    {reference}:{}", rendered.replace("\n  ", "\n      "))
    }
}

fn heap_show_lines(snapshot: &HeapReferencesSnapshot) -> Vec<String> {
    let shown = snapshot.references.len() as u64;
    let total = shown + snapshot.omitted_reference_count;
    let mut lines = vec![
        heap_node_line(&snapshot.node),
        format!("Outgoing properties/references ({shown} of {total}):"),
        "  PROPERTY/EDGE                 REFERENCE".to_owned(),
    ];
    lines.extend(snapshot.references.iter().map(|reference| {
        let label = reference
            .name
            .as_deref()
            .map(|name| escaped_heap_text(name, false))
            .unwrap_or_else(|| format!("[{}]", reference.name_or_index));
        format!(
            "  {:<28} {}{}{}",
            format!("{} {label}", reference.edge_type),
            reference.target,
            heap_preview_suffix(reference.target_preview.as_deref()),
            heap_reference_source(&reference.target, &reference.target_locations),
        )
    }));
    if snapshot.omitted_reference_count > 0 {
        lines.push(format!(
            "  ... {} references omitted; use --all to expand",
            snapshot.omitted_reference_count
        ));
    }
    lines
}

impl HumanOutput for HeapPathSnapshot {
    fn print_human(&self) {
        for line in heap_path_lines(self) {
            println!("{line}");
        }
    }
}

fn heap_path_lines(path: &HeapPathSnapshot) -> Vec<String> {
    let mut lines = vec![format!(
        "Heap path in '{}' ({} edges):",
        path.capture_id,
        path.steps.len(),
    )];
    let Some(first) = path.nodes.first() else {
        return lines;
    };
    lines.push(heap_node_line(first));
    let root_infrastructure_start = heap_root_infrastructure_start(path);
    for (step_index, (step, node)) in path.steps.iter().zip(path.nodes.iter().skip(1)).enumerate() {
        let node_index = step_index + 1;
        let prefix = root_infrastructure_start
            .filter(|start| node_index > *start)
            .map(|_| "│ ")
            .unwrap_or_default();
        lines.push(format!("{prefix}{}", heap_path_step_line(step)));
        if root_infrastructure_start == Some(node_index) {
            lines.push("┌─ V8 runtime roots (implementation details)".to_owned());
        }
        let prefix = root_infrastructure_start
            .filter(|start| node_index >= *start)
            .map(|_| "│ ")
            .unwrap_or_default();
        lines.push(format!("{prefix}{}", heap_node_line(node)));
    }
    if root_infrastructure_start.is_some() {
        lines.push("└─".to_owned());
    }
    lines
}

fn heap_path_step_line(step: &dbgjs::service_api::HeapPathStepSnapshot) -> String {
    let direction = match step.direction {
        dbgjs::service_api::HeapTraversalDirection::Outgoing => "->",
        dbgjs::service_api::HeapTraversalDirection::Incoming => "<-",
    };
    let label = step
        .name
        .as_deref()
        .map(|name| escaped_heap_text(name, false))
        .unwrap_or_else(|| format!("[{}]", step.name_or_index));
    format!("  {direction} {} {label}", step.edge_type)
}

fn heap_root_infrastructure_start(path: &HeapPathSnapshot) -> Option<usize> {
    let last = path.nodes.last()?;
    if last.node_type != "synthetic" || last.incoming_reference_count != 0 {
        return None;
    }
    let mut start = path.nodes.len() - 1;
    while start > 0
        && matches!(
            path.nodes[start - 1].node_type.as_str(),
            "native" | "synthetic"
        )
    {
        start -= 1;
    }
    (start > 0).then_some(start)
}

impl HumanOutput for HeapDominatorSnapshot {
    fn print_human(&self) {
        println!("Dominator chain for {}:", self.node.reference);
        println!("{}", heap_node_line(&self.node));
        for node in &self.chain {
            println!("  <- {}", heap_node_line(node));
        }
    }
}

impl HumanOutput for HeapAggregateSnapshot {
    fn print_human(&self) {
        let total_entry_count = self.entries.len() as u64 + self.omitted_entry_count;
        println!(
            "Heap aggregate for '{}' ({} of {} groups; largest shallow size first):",
            self.capture_id,
            self.entries.len(),
            total_entry_count,
        );
        for entry in &self.entries {
            println!(
                "{:>10}  {:>10}  {}",
                entry.count,
                compact_bytes(entry.shallow_size),
                escaped_heap_text(&entry.key, entry.key_truncated)
            );
        }
        if self.omitted_entry_count > 0 {
            println!("... {} aggregate groups omitted", self.omitted_entry_count);
        }
        if self.incomplete_string_count > 0 {
            println!(
                "... {} incomplete reconstructed strings omitted",
                self.incomplete_string_count
            );
        }
    }
}

impl HumanOutput for HeapDiffSnapshot {
    fn print_human(&self) {
        println!(
            "Heap diff '{}' -> '{}':",
            self.older_capture_id, self.newer_capture_id
        );
        for entry in &self.entries {
            println!(
                "{:+10}  {:+12} B  {}",
                entry.count_delta,
                entry.shallow_size_delta,
                escaped_heap_text(&entry.key, entry.key_truncated)
            );
        }
        if self.older_incomplete_string_count > 0 || self.newer_incomplete_string_count > 0 {
            println!(
                "... string diff is incomplete: {} older and {} newer reconstructed strings omitted",
                self.older_incomplete_string_count, self.newer_incomplete_string_count
            );
        }
    }
}

fn heap_node_line(node: &HeapNodeSnapshot) -> String {
    let value = node
        .string_value
        .as_deref()
        .map(|value| escaped_heap_text(value, node.string_truncated))
        .or_else(|| node.preview.clone())
        .unwrap_or_else(|| escaped_heap_text(&node.name, node.string_truncated));
    let retained = node
        .retained_size
        .map(|size| format!(", retained:{}", compact_bytes(size)))
        .unwrap_or_default();
    format!(
        "{}  type:{}, value:{}, shallow:{}{}, in:{}, out:{}{}",
        node.reference,
        node.node_type,
        value,
        compact_bytes(node.shallow_size),
        retained,
        node.incoming_reference_count,
        node.outgoing_reference_count,
        render_object_source(&node.source),
    )
}

fn escaped_heap_text(value: &str, truncated: bool) -> String {
    let escaped = value.escape_default().to_string();
    format!("\"{escaped}{}\"", if truncated { "..." } else { "" })
}

impl HumanOutput for Vec<ContextSummary> {
    fn print_human(&self) {
        if self.is_empty() {
            println!("No debugger contexts.");
            return;
        }
        println!("Debugger contexts:");
        for context in self {
            println!(
                "  {}  {}  rev {}  {} connection(s), {} breakpoint(s)",
                context.id,
                context.display_name,
                context.revision,
                context.connection_count,
                context.breakpoint_count
            );
        }
    }
}

impl HumanOutput for CaptureSnapshot {
    fn print_human(&self) {
        println!(
            "{}  kind={:?}  target={}/{}@{}  storage={}",
            self.name,
            self.kind,
            self.connection_id,
            self.target_id,
            self.connection_generation,
            self.storage_id
        );
    }
}

impl HumanOutput for Vec<CaptureSnapshot> {
    fn print_human(&self) {
        if self.is_empty() {
            println!("No stored captures.");
            return;
        }
        for capture in self {
            capture.print_human();
        }
    }
}

impl HumanOutput for ConnectionListOutput {
    fn print_human(&self) {
        if self.connections.is_empty() {
            println!("No connections matched in context {}.", self.context_id);
            return;
        }
        println!(
            "Connections in context {} (rev {}):",
            self.context_id, self.revision
        );
        for connection in &self.connections {
            println!(
                "{} {}  [{}]  kind={}  generation={}  targets={}",
                if connection.selected { "*" } else { " " },
                terminal_text(&connection.id),
                terminal_text(&connection_status(&connection.status)),
                connection_configuration_kind(&connection.configuration),
                connection.generation,
                connection.target_count,
            );
            println!(
                "    {}",
                terminal_text(&connection_configuration(&connection.configuration))
            );
        }
    }
}

impl HumanOutput for TargetListOutput {
    fn print_human(&self) {
        if self.targets.is_empty() {
            println!("No targets matched in context {}.", self.context_id);
            return;
        }
        println!(
            "Targets in context {} (rev {}):",
            self.context_id, self.revision
        );
        let entries_by_id = self
            .targets
            .iter()
            .enumerate()
            .map(|(index, entry)| {
                (
                    (entry.connection_id.clone(), entry.target.target_id.clone()),
                    index,
                )
            })
            .collect::<BTreeMap<_, _>>();
        let mut children = BTreeMap::<usize, Vec<usize>>::new();
        let mut roots = Vec::new();
        for (index, entry) in self.targets.iter().enumerate() {
            let parent = entry.parent_target_id.as_ref().and_then(|parent_id| {
                entries_by_id
                    .get(&(entry.connection_id.clone(), parent_id.clone()))
                    .copied()
            });
            match parent {
                Some(parent) if parent != index => children.entry(parent).or_default().push(index),
                _ => roots.push(index),
            }
        }
        let mut visited = BTreeSet::new();
        for (index, root) in roots.iter().enumerate() {
            print_target_tree(
                self,
                *root,
                "",
                index + 1 == roots.len(),
                &children,
                &mut visited,
            );
        }
        for index in 0..self.targets.len() {
            if !visited.contains(&index) {
                print_target_tree(self, index, "", true, &children, &mut visited);
            }
        }
    }
}

fn print_target_tree(
    output: &TargetListOutput,
    index: usize,
    prefix: &str,
    last: bool,
    children: &BTreeMap<usize, Vec<usize>>,
    visited: &mut BTreeSet<usize>,
) {
    if !visited.insert(index) {
        return;
    }
    let entry = &output.targets[index];
    let target = &entry.target;
    let title = if target.title.is_empty() {
        "(untitled)"
    } else {
        &target.title
    };
    let selector = target_tree_selector(entry);
    println!(
        "{prefix}{}{} {}  [{}{}]  {:?}  {}",
        if last { "└─" } else { "├─" },
        if entry.selected { "*" } else { "" },
        terminal_text(&selector),
        terminal_text(&target.target_type),
        if target.attached { "; attached" } else { "" },
        title,
        terminal_text(&target.url),
    );
    let child_prefix = format!("{prefix}{}", if last { "  " } else { "│ " });
    let child_indices = children.get(&index).map(Vec::as_slice).unwrap_or_default();
    for (child_index, child) in child_indices.iter().enumerate() {
        print_target_tree(
            output,
            *child,
            &child_prefix,
            child_index + 1 == child_indices.len(),
            children,
            visited,
        );
    }
}

fn target_tree_selector(entry: &TargetListEntry) -> String {
    dbgjs::target_selector::qualified_target_selector(
        &entry.connection_id,
        &entry.target.target_id,
        entry.connection_generation,
    )
}

impl HumanOutput for Vec<ProcessTreeSnapshot> {
    fn print_human(&self) {
        print_process_trees_human(self, ProcessTreeOutputOptions::default());
    }
}

fn print_process_trees_human(trees: &[ProcessTreeSnapshot], options: ProcessTreeOutputOptions<'_>) {
    if trees.is_empty() {
        println!(
            "No running {} process trees.",
            process_root_kind(options.root_kind)
        );
        return;
    }
    let rendered = trees
        .iter()
        .filter_map(|tree| {
            let lines = process_tree_lines(tree, options);
            (!lines.is_empty()).then_some((tree, lines))
        })
        .collect::<Vec<_>>();
    if rendered.is_empty() {
        println!(
            "No {} process tree paths matched {}.",
            process_root_kind(options.root_kind),
            options.filter.unwrap_or_default()
        );
        return;
    }
    for (tree_index, (tree, lines)) in rendered.into_iter().enumerate() {
        if tree_index != 0 {
            println!();
        }
        let mut details = vec![format!(
            "{} attachable processes",
            tree.processes
                .iter()
                .filter(|process| process.attachable)
                .count()
        )];
        if !tree.targets.is_empty() {
            details.push(format!("{} discovered targets", tree.targets.len()));
        }
        if tree.runtime_metadata_available {
            details.push("window metadata available".to_owned());
        }
        println!(
            "{} process tree {}  ({})",
            process_root_kind(tree.root_kind),
            tree.root_process_id,
            details.join("; "),
        );
        for line in lines {
            println!("{line}");
        }
        if let Some(error) = &tree.target_discovery_error {
            println!("  target discovery incomplete: {}", terminal_text(error));
        }
    }
}

#[derive(Clone)]
enum ProcessTreeLeaf<'a> {
    Process(&'a ProcessSnapshot),
    Target(&'a ProcessTargetSnapshot),
    Window {
        root_pid: u32,
        id: u32,
        title: Option<&'a str>,
    },
    Session(&'a AgentSessionSnapshot),
}

#[derive(Clone)]
struct ProcessRenderNode<'a> {
    path_segment: String,
    leaf: ProcessTreeLeaf<'a>,
    children: Vec<ProcessRenderNode<'a>>,
}

#[derive(Clone)]
struct ProcessOrder(u64);

impl Default for ProcessOrder {
    fn default() -> Self {
        Self(u64::MAX)
    }
}

impl TreeAggregate for ProcessOrder {
    fn merge(&mut self, other: &Self) {
        self.0 = self.0.min(other.0);
    }
}

struct ProcessTreeStyle {
    root_kind: ProcessRootKind,
    command_line: bool,
    stats: bool,
    colorize: bool,
}

impl BoundedTreeStyle<ProcessOrder, ProcessTreeLeaf<'_>> for ProcessTreeStyle {
    fn sort_weight(&self, aggregate: &ProcessOrder) -> u64 {
        u64::MAX.saturating_sub(aggregate.0)
    }

    fn expansion_weight(
        &self,
        node: &BoundedTree<ProcessOrder, ProcessTreeLeaf<'_>>,
        _expand_leaves: bool,
    ) -> u64 {
        node.leaf_count() as u64
    }

    fn render_node(
        &self,
        label: &str,
        node: &BoundedTree<ProcessOrder, ProcessTreeLeaf<'_>>,
        _prefix: &str,
        _expand_leaves: bool,
    ) -> String {
        match node.leaf() {
            Some(ProcessTreeLeaf::Process(process)) => {
                let label = process_label(
                    process,
                    ProcessTreeOutputOptions {
                        root_kind: self.root_kind,
                        command_line: self.command_line,
                        stats: self.stats,
                        filter: None,
                        trim_width: true,
                    },
                );
                style_process_label(label, process.attachable, self.colorize)
            }
            Some(ProcessTreeLeaf::Target(target)) => {
                let target = &target.target;
                let title = if target.title.is_empty() {
                    "(untitled)"
                } else {
                    &target.title
                };
                format!(
                    "{}  [{}{}]  {:?}  {}",
                    terminal_text(label),
                    terminal_text(&target.target_type),
                    if target.attached { "; attached" } else { "" },
                    title,
                    terminal_text(&target.url),
                )
            }
            Some(ProcessTreeLeaf::Window {
                root_pid,
                id,
                title,
            }) => format!(
                "w:{root_pid}/{id}  window{}",
                title.map(|title| format!("  {title}")).unwrap_or_default()
            ),
            Some(ProcessTreeLeaf::Session(session)) => style_session_label(
                format!(
                    "session {}  [{}{}]",
                    session.title.as_deref().unwrap_or("<untitled>"),
                    session.internal_id,
                    if session.disconnected == Some(true) {
                        ", disconnected"
                    } else {
                        ""
                    }
                ),
                self.colorize,
            ),
            None => label.to_owned(),
        }
    }

    fn render_leaf_children(
        &self,
        _prefix: &str,
        _node: &BoundedTree<ProcessOrder, ProcessTreeLeaf<'_>>,
        _budget: usize,
        _expand_leaves: bool,
    ) -> Vec<String> {
        Vec::new()
    }

    fn render_omitted(
        &self,
        hidden_items: usize,
        _hidden_leaves: usize,
        _aggregate: &ProcessOrder,
    ) -> String {
        format!("{hidden_items} process tree nodes omitted")
    }

    fn render_all_pruned(
        &self,
        child_count: usize,
        _hidden_leaves: usize,
        _aggregate: &ProcessOrder,
    ) -> String {
        format!("all {child_count} process tree nodes pruned")
    }
}

fn style_process_label(label: String, attachable: bool, colorize: bool) -> String {
    if colorize && !attachable {
        format!("\u{1b}[2m{label}\u{1b}[0m")
    } else {
        label
    }
}

fn style_session_label(label: String, colorize: bool) -> String {
    if colorize {
        format!("\u{1b}[34m{label}\u{1b}[0m")
    } else {
        label
    }
}

fn process_tree_lines(
    tree: &ProcessTreeSnapshot,
    options: ProcessTreeOutputOptions<'_>,
) -> Vec<String> {
    let Some(root) = process_render_tree(tree) else {
        return Vec::new();
    };
    let root = match options.filter {
        Some(filter) => filter_process_render_node(&root, filter),
        None => Some(root),
    };
    let Some(root) = root else {
        return Vec::new();
    };
    let mut tree = BoundedTree::default();
    let mut order = 0;
    insert_process_render_node(&mut tree, &root, &mut Vec::new(), &mut order);
    tree.render_with_options(
        &ProcessTreeStyle {
            root_kind: options.root_kind,
            command_line: options.command_line,
            stats: options.stats,
            colorize: std::io::stdout().is_terminal() && std::env::var_os("NO_COLOR").is_none(),
        },
        false,
        TreeRenderOptions::terminal(usize::MAX, options.trim_width),
    )
}

fn process_render_tree(tree: &ProcessTreeSnapshot) -> Option<ProcessRenderNode<'_>> {
    let mut children = BTreeMap::<u32, Vec<&ProcessSnapshot>>::new();
    for process in &tree.processes {
        if let Some(parent_id) = process.parent_process_id {
            children.entry(parent_id).or_default().push(process);
        }
    }
    let Some(root) = tree
        .processes
        .iter()
        .find(|process| process.process_id == tree.root_process_id)
    else {
        return None;
    };
    let targets_by_id = tree
        .targets
        .iter()
        .map(|target| (target.target.target_id.as_str(), target))
        .collect::<BTreeMap<_, _>>();
    let mut target_children = BTreeMap::<String, Vec<&ProcessTargetSnapshot>>::new();
    let mut process_targets = BTreeMap::<u32, Vec<&ProcessTargetSnapshot>>::new();
    for target in &tree.targets {
        if target.target.target_id == "$node-root" {
            continue;
        }
        let parent = target
            .target
            .parent_id
            .as_deref()
            .or(target.target.opener_id.as_deref())
            .and_then(|parent| targets_by_id.get(parent).copied());
        if let Some(parent) = parent
            && parent.target.target_id != "$node-root"
        {
            target_children
                .entry(parent.target.target_id.clone())
                .or_default()
                .push(target);
        } else {
            process_targets
                .entry(target.process_id.unwrap_or(tree.root_process_id))
                .or_default()
                .push(target);
        }
    }
    Some(process_render_node(
        root,
        tree.root_process_id,
        root.window_id,
        &children,
        &process_targets,
        &target_children,
    ))
}

fn process_render_node<'a>(
    process: &'a ProcessSnapshot,
    root_pid: u32,
    active_window: Option<u32>,
    children: &BTreeMap<u32, Vec<&'a ProcessSnapshot>>,
    process_targets: &BTreeMap<u32, Vec<&'a ProcessTargetSnapshot>>,
    target_children: &BTreeMap<String, Vec<&'a ProcessTargetSnapshot>>,
) -> ProcessRenderNode<'a> {
    let mut rendered_children = process
        .agent_sessions
        .iter()
        .map(|session| ProcessRenderNode {
            path_segment: format!(
                "session {}",
                session.title.as_deref().unwrap_or(&session.internal_id)
            ),
            leaf: ProcessTreeLeaf::Session(session),
            children: Vec::new(),
        })
        .collect::<Vec<_>>();
    let process_children = children
        .get(&process.process_id)
        .map(Vec::as_slice)
        .unwrap_or_default();
    let mut rendered_windows = BTreeSet::new();
    for child in process_children {
        match child
            .window_id
            .filter(|window_id| Some(*window_id) != active_window)
        {
            Some(window_id) if rendered_windows.insert(window_id) => {
                let title = process_children
                    .iter()
                    .filter(|candidate| candidate.window_id == Some(window_id))
                    .find_map(|candidate| candidate.window_title.as_deref());
                rendered_children.push(ProcessRenderNode {
                    path_segment: format!("window {window_id}"),
                    leaf: ProcessTreeLeaf::Window {
                        root_pid,
                        id: window_id,
                        title,
                    },
                    children: process_children
                        .iter()
                        .filter(|candidate| candidate.window_id == Some(window_id))
                        .map(|window_child| {
                            process_render_node(
                                window_child,
                                root_pid,
                                Some(window_id),
                                children,
                                process_targets,
                                target_children,
                            )
                        })
                        .collect(),
                });
            }
            Some(_) => {}
            None => rendered_children.push(process_render_node(
                child,
                root_pid,
                active_window,
                children,
                process_targets,
                target_children,
            )),
        }
    }
    rendered_children.extend(
        process_targets
            .get(&process.process_id)
            .into_iter()
            .flatten()
            .map(|target| process_target_render_node(target, target_children)),
    );
    ProcessRenderNode {
        path_segment: process_path_segment(process),
        leaf: ProcessTreeLeaf::Process(process),
        children: rendered_children,
    }
}

fn process_target_render_node<'a>(
    target: &'a ProcessTargetSnapshot,
    children: &BTreeMap<String, Vec<&'a ProcessTargetSnapshot>>,
) -> ProcessRenderNode<'a> {
    ProcessRenderNode {
        path_segment: target.target.target_id.clone(),
        leaf: ProcessTreeLeaf::Target(target),
        children: children
            .get(&target.target.target_id)
            .into_iter()
            .flatten()
            .map(|child| process_target_render_node(child, children))
            .collect(),
    }
}

fn process_path_segment(process: &ProcessSnapshot) -> String {
    let label = process.display_name.as_deref().unwrap_or(&process.name);
    format!("p:{} {label}", process.process_id)
}

fn filter_process_render_node<'a>(
    root: &ProcessRenderNode<'a>,
    filter: &str,
) -> Option<ProcessRenderNode<'a>> {
    fn filter_node<'a>(
        node: &ProcessRenderNode<'a>,
        path: &mut Vec<String>,
        filter: &str,
        ancestor_matched: bool,
    ) -> Option<ProcessRenderNode<'a>> {
        path.push(node.path_segment.to_lowercase());
        let matched = ancestor_matched || path.join("/").contains(filter);
        let result = if matched {
            Some(node.clone())
        } else {
            let children = node
                .children
                .iter()
                .filter_map(|child| filter_node(child, path, filter, false))
                .collect::<Vec<_>>();
            (!children.is_empty()).then(|| ProcessRenderNode {
                path_segment: node.path_segment.clone(),
                leaf: node.leaf.clone(),
                children,
            })
        };
        path.pop();
        result
    }

    let filter = filter.trim().to_lowercase();
    if filter.is_empty() {
        return Some(root.clone());
    }
    filter_node(root, &mut Vec::new(), &filter, false)
}

fn process_trees_json(
    trees: &[ProcessTreeSnapshot],
    options: ProcessTreeOutputOptions<'_>,
) -> Result<serde_json::Value, serde_json::Error> {
    let mut result = Vec::new();
    for tree in trees {
        let selected = match options.filter {
            Some(filter) => {
                let Some(root) = process_render_tree(tree)
                    .and_then(|root| filter_process_render_node(&root, filter))
                else {
                    continue;
                };
                let mut process_ids = BTreeSet::new();
                collect_process_ids(&root, &mut process_ids);
                Some(process_ids)
            }
            None => None,
        };
        let mut value = serde_json::to_value(tree)?;
        if let Some(processes) = value
            .get_mut("processes")
            .and_then(serde_json::Value::as_array_mut)
        {
            if let Some(selected) = &selected {
                processes.retain(|process| {
                    process
                        .get("processId")
                        .and_then(serde_json::Value::as_u64)
                        .is_some_and(|process_id| selected.contains(&(process_id as u32)))
                });
            }
            if !options.command_line {
                for process in processes.iter_mut() {
                    if let Some(process) = process.as_object_mut() {
                        process.remove("commandLine");
                    }
                }
            }
            for process in processes.iter_mut() {
                if let Some(process) = process.as_object_mut()
                    && let Some(process_id) =
                        process.get("processId").and_then(serde_json::Value::as_u64)
                {
                    process.insert(
                        "reference".to_owned(),
                        serde_json::Value::String(format!("p:{process_id}")),
                    );
                    process.insert(
                        "locator".to_owned(),
                        serde_json::Value::String(format!(
                            "{}://{}/process/{process_id}",
                            match tree.root_kind {
                                ProcessRootKind::Vscode => "vscode",
                                _ => "process-tree",
                            },
                            tree.root_process_id,
                        )),
                    );
                }
            }
        }
        if let Some(selected) = &selected
            && let Some(targets) = value
                .get_mut("targets")
                .and_then(serde_json::Value::as_array_mut)
        {
            targets.retain(|target| {
                target
                    .get("processId")
                    .and_then(serde_json::Value::as_u64)
                    .is_none_or(|process_id| selected.contains(&(process_id as u32)))
            });
        }
        result.push(value);
    }
    Ok(serde_json::Value::Array(result))
}

fn collect_process_ids(node: &ProcessRenderNode<'_>, result: &mut BTreeSet<u32>) {
    if let ProcessTreeLeaf::Process(process) = &node.leaf {
        result.insert(process.process_id);
    }
    for child in &node.children {
        collect_process_ids(child, result);
    }
}

fn insert_process_render_node<'a>(
    tree: &mut BoundedTree<ProcessOrder, ProcessTreeLeaf<'a>>,
    node: &ProcessRenderNode<'a>,
    path: &mut Vec<String>,
    order: &mut u64,
) {
    path.push(node.path_segment.clone());
    tree.insert(
        path.iter().cloned(),
        ProcessOrder(*order),
        node.leaf.clone(),
    );
    *order += 1;
    for child in &node.children {
        insert_process_render_node(tree, child, path, order);
    }
    path.pop();
}

fn process_label(process: &ProcessSnapshot, options: ProcessTreeOutputOptions<'_>) -> String {
    let label = process.display_name.as_deref().unwrap_or(&process.name);
    let stats = options.stats.then(|| {
        format!(
            "  cpu {}  memory {}",
            process
                .cpu_percent
                .map(|cpu| format!("{cpu}%"))
                .unwrap_or_else(|| "?".to_owned()),
            process
                .memory_bytes
                .map(compact_bytes)
                .unwrap_or_else(|| "?".to_owned())
        )
    });
    let command = options
        .command_line
        .then(|| process_command_summary(process))
        .flatten();
    format!(
        "p:{}  {}  [{}]{}{}",
        process.process_id,
        label,
        process_role(&process.role),
        stats.unwrap_or_default(),
        command
            .as_deref()
            .map(|command| format!("  {command}"))
            .unwrap_or_default()
    )
}

fn process_command_summary(process: &ProcessSnapshot) -> Option<String> {
    if !matches!(
        process.role,
        ProcessRole::Node
            | ProcessRole::TypeScriptServer
            | ProcessRole::TypeScriptInstaller
            | ProcessRole::LanguageServer
    ) {
        return None;
    }
    let command = strip_windows_executable(&process.command_line);
    if command.is_empty() {
        None
    } else {
        Some(command.to_owned())
    }
}

fn strip_windows_executable(command: &str) -> &str {
    let command = command.trim();
    if let Some(rest) = command.strip_prefix('"')
        && let Some(end) = rest.find('"')
    {
        return rest[end + 1..].trim();
    }
    command
        .split_once(char::is_whitespace)
        .map_or("", |(_, rest)| rest.trim())
}

impl HumanOutput for ContextSnapshot {
    fn print_human(&self) {
        println!("Context {}  rev {}", self.id, self.revision);
        println!("  Name: {}", self.display_name);

        if self.connections.is_empty() {
            println!("  Connections: none");
        } else {
            println!("  Connections:");
            for connection in &self.connections {
                println!(
                    "    {}  [{}; generation {}]",
                    connection.id,
                    connection_status(&connection.status),
                    connection.generation
                );
                println!(
                    "      Configuration: {}",
                    connection_configuration(&connection.configuration)
                );
                if connection.targets.is_empty() {
                    println!("      Targets: none");
                } else {
                    println!("      Targets:");
                    let type_counts = connection.targets.iter().fold(
                        std::collections::BTreeMap::<&str, usize>::new(),
                        |mut counts, target| {
                            *counts.entry(&target.target_type).or_default() += 1;
                            counts
                        },
                    );
                    let title_counts = connection.targets.iter().fold(
                        std::collections::BTreeMap::<&str, usize>::new(),
                        |mut counts, target| {
                            *counts.entry(&target.title).or_default() += 1;
                            counts
                        },
                    );
                    let mut target_depths = BTreeMap::<String, usize>::new();
                    for node in connection.target_forest() {
                        let target = &node.target;
                        let depth = node
                            .parent_target_id
                            .as_deref()
                            .and_then(|parent_id| target_depths.get(parent_id))
                            .map_or(0, |parent_depth| parent_depth + 1);
                        target_depths.insert(target.target_id.clone(), depth);
                        let title = if target.title.is_empty() {
                            "(untitled)"
                        } else {
                            &target.title
                        };
                        let selector = if type_counts[&target.target_type.as_str()] == 1 {
                            target.target_type.as_str()
                        } else if !target.title.is_empty()
                            && title_counts[&target.title.as_str()] == 1
                        {
                            target.title.as_str()
                        } else {
                            target.target_id.as_str()
                        };
                        println!(
                            "        {}{}  {}  {}{}",
                            "  ".repeat(depth),
                            selector,
                            title,
                            target.url,
                            if target.attached {
                                "  [a CDP client is attached]"
                            } else {
                                ""
                            }
                        );
                    }
                }
            }
        }

        if !self.breakpoints.is_empty() {
            println!("  Breakpoints:");
            for breakpoint in &self.breakpoints {
                println!(
                    "    {}  {}:{}:{}  [{}]",
                    breakpoint.id,
                    breakpoint.source_path,
                    breakpoint.line,
                    breakpoint.column,
                    breakpoint_status(&breakpoint.status)
                );
                if let Some(reason) = &breakpoint.pending_reason {
                    print_breakpoint_pending_reason(reason, "      ");
                }
                for application in &breakpoint.applications {
                    if let Some(mapping) = &application.mapping {
                        println!(
                            "      {} / {} gen {} / script {} v{} -> {}:{}:{} via {}",
                            application.connection_id,
                            application.target_id,
                            application.connection_generation,
                            application.script_id,
                            application.script_version,
                            mapping.generated_url,
                            mapping.generated_line,
                            mapping.generated_column,
                            mapping.projection.join(" -> ")
                        );
                    }
                }
            }
        }
    }
}

fn print_breakpoint_human(breakpoint: &BreakpointSnapshot) {
    println!(
        "Breakpoint {}  {}:{}:{}  [{}]",
        breakpoint.id,
        breakpoint.source_path,
        breakpoint.line,
        breakpoint.column,
        breakpoint_status(&breakpoint.status)
    );
    if let Some(reason) = &breakpoint.pending_reason {
        print_breakpoint_pending_reason(reason, "  ");
    }
    for application in &breakpoint.applications {
        if let Some(mapping) = &application.mapping {
            println!(
                "  bound on {}/{}: CDP confirmed script {} v{} at {}:{}:{} via {}",
                application.connection_id,
                application.target_id,
                application.script_id,
                application.script_version,
                application.script_url,
                application.generated_line,
                application.generated_column,
                mapping.projection.join(" -> ")
            );
        }
    }
}

fn breakpoint_status(status: &BreakpointStatus) -> String {
    match status {
        BreakpointStatus::Unconfirmed => "unconfirmed".into(),
        BreakpointStatus::Disabled => "disabled".into(),
        BreakpointStatus::Pending => "pending".into(),
        BreakpointStatus::PartiallyBound { application_count } => {
            format!("partially bound; {application_count} application(s)")
        }
        BreakpointStatus::Bound { application_count } => {
            format!("bound; {application_count} application(s)")
        }
        BreakpointStatus::Failed { message } => format!("failed: {message}"),
    }
}

fn process_role(role: &ProcessRole) -> &'static str {
    match role {
        ProcessRole::VscodeMain => "vscode-main",
        ProcessRole::ElectronMain => "electron-main",
        ProcessRole::BrowserMain => "browser-main",
        ProcessRole::Renderer => "renderer",
        ProcessRole::ExtensionHost => "extension-host",
        ProcessRole::NodeUtility => "node-utility",
        ProcessRole::Node => "node",
        ProcessRole::TypeScriptServer => "typescript-server",
        ProcessRole::TypeScriptInstaller => "typescript-installer",
        ProcessRole::LanguageServer => "language-server",
        ProcessRole::PtyHost => "pty-host",
        ProcessRole::FileWatcher => "file-watcher",
        ProcessRole::AgentHost => "agent-host",
        ProcessRole::Copilot => "copilot",
        ProcessRole::Claude => "claude",
        ProcessRole::Codex => "codex",
        ProcessRole::Agent => "agent",
        ProcessRole::Gpu => "gpu",
        ProcessRole::NetworkService => "network-service",
        ProcessRole::AudioService => "audio-service",
        ProcessRole::Crashpad => "crashpad",
        ProcessRole::Utility => "utility",
        ProcessRole::Other => "other",
    }
}

fn process_root_kind(kind: ProcessRootKind) -> &'static str {
    match kind {
        ProcessRootKind::Vscode => "VS Code",
        ProcessRootKind::Node => "Node.js",
        ProcessRootKind::Electron => "Electron",
        ProcessRootKind::Browser => "Browser",
    }
}

impl HumanOutput for TargetDebuggerSnapshot {
    fn print_human(&self) {
        print_target_human(self, &self.target_id);
    }
}

impl HumanOutput for TargetAttachmentResult {
    fn print_human(&self) {
        println!(
            "Attachment: {}",
            match self.outcome {
                TargetAttachmentOutcome::Created => "created",
                TargetAttachmentOutcome::Stolen => "stolen",
            }
        );
        print_target_human(&self.target, &self.target.target_id);
    }
}

fn print_target_human(snapshot: &TargetDebuggerSnapshot, selector: &str) {
    println!(
        "Target {}  [{}]  {}/{}  gen {}  rev {}",
        selector,
        target_phase(&snapshot.phase),
        snapshot.context_id,
        snapshot.connection_id,
        snapshot.connection_generation,
        snapshot.revision
    );
    if !snapshot.breakpoints.is_empty() {
        println!("  Breakpoints:");
        for breakpoint in &snapshot.breakpoints {
            println!(
                "    {}  {}:{}:{}  [{}]",
                breakpoint.id,
                breakpoint.source_url,
                breakpoint.line,
                breakpoint.column,
                target_breakpoint_status(&breakpoint.status)
            );
            print_target_breakpoint_explanation(breakpoint);
        }
    }
    match &snapshot.pause {
        None => println!("  Pause: none"),
        Some(pause) => {
            println!("  Pause: epoch {} ({})", pause.epoch, pause.reason);
            if let Some(source) = &pause.source {
                print_source_excerpt("Source", source);
            }
            println!("  Frames:");
            for frame in &pause.frames {
                let function_name = if frame.function_name.is_empty() {
                    "(anonymous)"
                } else {
                    &frame.function_name
                };
                let function_name = frame.breadcrumb.as_deref().unwrap_or(function_name);
                let location = match &frame.projected {
                    FrameProjectionSnapshot::Resolved { location }
                        if location.source_url.is_empty() =>
                    {
                        runtime_location(&frame.raw)
                    }
                    FrameProjectionSnapshot::Resolved { location } => {
                        format!(
                            "{}:{}:{}",
                            location.source_url, location.line, location.column
                        )
                    }
                    FrameProjectionSnapshot::Raw => runtime_location(&frame.raw),
                    FrameProjectionSnapshot::Pending => "mapping".to_owned(),
                    FrameProjectionSnapshot::Failed { .. } => runtime_location(&frame.raw),
                };
                println!("    #{} {function_name} — {location}", frame.index);
            }
            let warnings = pause
                .frames
                .iter()
                .filter_map(|frame| match &frame.projected {
                    FrameProjectionSnapshot::Failed { message } => {
                        Some((frame.index, message, runtime_location(&frame.raw)))
                    }
                    _ => None,
                });
            let mut warnings = warnings.peekable();
            if warnings.peek().is_some() {
                println!("  Warnings:");
                for (index, message, generated_location) in warnings {
                    println!(
                        "    frame #{index} source mapping failed: {message}; using generated location {generated_location}"
                    );
                }
            }
        }
    }
}

impl HumanOutput for EvaluationSnapshot {
    fn print_human(&self) {
        println!("{}", render_evaluation(self));
    }
}

impl HumanOutput for ValueSnapshot {
    fn print_human(&self) {
        if let Some(promise) = &self.promise {
            println!("Promise <{}>", promise_state_name(promise.state));
            if let Some(settlement) = &promise.settlement {
                let label = if promise.state == dbgjs::service_api::PromiseState::Rejected {
                    "reason"
                } else {
                    "value"
                };
                println!("  {label}: {}", render_value_preview(settlement));
                if let Some(reference) = &settlement.reference {
                    println!("  settlement reference: {reference}");
                }
            }
            if let Some(reference) = &self.preview.reference {
                println!("  reference: {reference}");
            }
            print!("{}", render_object_source(&self.preview.source));
            return;
        }

        println!("{}", render_value_snapshot(self));
        for property in &self.properties {
            println!(
                "  {}: {}",
                property.name,
                render_value_preview_with_reference(&property.value)
            );
        }
        if self.omitted_property_count > 0 {
            println!(
                "  ... {} properties omitted; use --max-properties to expand",
                self.omitted_property_count
            );
        } else if self.properties_truncated {
            println!("  ... additional properties omitted");
        }
    }
}

impl HumanOutput for CoverageSnapshot {
    fn print_human(&self) {
        print_coverage_human(
            self,
            CoverageOutputOptions {
                path: None,
                path_glob: None,
                all: false,
                max_lines: 300,
                trim_width: true,
            },
        );
    }
}

fn print_coverage_human(snapshot: &CoverageSnapshot, options: CoverageOutputOptions<'_>) {
    if let Some(capture_id) = &snapshot.capture_id {
        println!("Capture {capture_id}");
    }
    if snapshot.sources.is_empty() {
        println!("No executed functions captured.");
        return;
    }
    let mut files = BTreeMap::<String, Vec<CoverageEntry>>::new();
    for entry in coverage_entries(snapshot) {
        files.entry(entry.path.clone()).or_default().push(entry);
    }
    print_coverage_tree(snapshot, options, files);
}

fn print_cpu_profile_human(snapshot: &CpuProfileSnapshot, options: CpuProfileOutputOptions<'_>) {
    let sampled_micros = snapshot
        .nodes
        .iter()
        .map(|node| node.self_time_micros)
        .sum::<u64>();
    let elapsed_micros = (snapshot.end_time_micros - snapshot.start_time_micros).max(0.0) as u64;
    println!(
        "CPU profile {}: {} elapsed, {} sampled, {} samples",
        snapshot.capture_id,
        format_profile_time(elapsed_micros),
        format_profile_time(sampled_micros),
        snapshot.samples.len()
    );
    match options.view {
        CpuProfileView::Functions => {
            let mut functions = snapshot
                .functions
                .iter()
                .filter(|function| cpu_profile_matches_path(function, options.path))
                .collect::<Vec<_>>();
            functions.sort_by_key(|function| {
                std::cmp::Reverse(match options.sort {
                    CpuProfileSort::SelfTime => function.self_time_micros,
                    CpuProfileSort::TotalTime => function.total_time_micros,
                })
            });
            if functions.is_empty() {
                println!("No sampled functions matched.");
                return;
            }
            println!("Self       Total      Samples  Function");
            for function in functions
                .into_iter()
                .take(options.max_lines.saturating_sub(2))
            {
                let location = function
                    .authored_location
                    .as_ref()
                    .unwrap_or(&function.generated_location);
                let name = function
                    .breadcrumb
                    .as_deref()
                    .filter(|name| !name.is_empty())
                    .unwrap_or(&function.name);
                println!(
                    "{:<10} {:<10} {:>7}  {}  {}:{}:{}",
                    format_profile_time(function.self_time_micros),
                    format_profile_time(function.total_time_micros),
                    function.sample_count,
                    if name.is_empty() { "(anonymous)" } else { name },
                    location.source_url,
                    location.line,
                    location.column,
                );
            }
        }
        CpuProfileView::Files => {
            let mut files = BTreeMap::<String, (u64, u64, u64)>::new();
            for function in snapshot
                .functions
                .iter()
                .filter(|function| cpu_profile_matches_path(function, options.path))
            {
                let location = function
                    .authored_location
                    .as_ref()
                    .unwrap_or(&function.generated_location);
                let totals = files.entry(location.source_url.clone()).or_default();
                totals.0 = totals.0.saturating_add(function.self_time_micros);
                totals.1 = totals.1.saturating_add(function.total_time_micros);
                totals.2 = totals.2.saturating_add(function.sample_count);
            }
            let mut files = files.into_iter().collect::<Vec<_>>();
            files.sort_by_key(|(_, (self_time, total_time, _))| {
                std::cmp::Reverse(match options.sort {
                    CpuProfileSort::SelfTime => *self_time,
                    CpuProfileSort::TotalTime => *total_time,
                })
            });
            if files.is_empty() {
                println!("No sampled files matched.");
                return;
            }
            println!("Self       Total      Samples  File");
            for (path, (self_time, total_time, samples)) in
                files.into_iter().take(options.max_lines.saturating_sub(2))
            {
                println!(
                    "{:<10} {:<10} {:>7}  {}",
                    format_profile_time(self_time),
                    format_profile_time(total_time),
                    samples,
                    path,
                );
            }
        }
    }
}

fn cpu_profile_matches_path(function: &CpuProfileFunctionSnapshot, prefix: Option<&str>) -> bool {
    let Some(prefix) = prefix else {
        return true;
    };
    let location = function
        .authored_location
        .as_ref()
        .unwrap_or(&function.generated_location);
    normalize_source_path(&location.source_url).starts_with(&normalize_source_path(prefix))
}

fn format_profile_time(micros: u64) -> String {
    if micros < 1_000 {
        format!("{micros}us")
    } else if micros < 1_000_000 {
        format!("{:.2}ms", micros as f64 / 1_000.0)
    } else {
        format!("{:.2}s", micros as f64 / 1_000_000.0)
    }
}

#[derive(Clone, Copy, Default)]
struct HeapClassMetrics {
    classes: u64,
    instances: u64,
    shallow_size: u64,
}

impl TreeAggregate for HeapClassMetrics {
    fn merge(&mut self, other: &Self) {
        self.classes = self.classes.saturating_add(other.classes);
        self.instances = self.instances.saturating_add(other.instances);
        self.shallow_size = self.shallow_size.saturating_add(other.shallow_size);
    }
}

struct HeapClassTreeStyle {
    instances: bool,
}

impl BoundedTreeStyle<HeapClassMetrics, Vec<HeapClassSnapshotEntry>> for HeapClassTreeStyle {
    fn sort_weight(&self, aggregate: &HeapClassMetrics) -> u64 {
        aggregate.instances
    }

    fn expansion_weight(
        &self,
        node: &BoundedTree<HeapClassMetrics, Vec<HeapClassSnapshotEntry>>,
        _expand_leaves: bool,
    ) -> u64 {
        if node.children().is_empty() && node.leaf().is_none() {
            0
        } else {
            (node.aggregate().instances.max(1) as f64).sqrt().ceil() as u64
        }
    }

    fn render_node(
        &self,
        label: &str,
        node: &BoundedTree<HeapClassMetrics, Vec<HeapClassSnapshotEntry>>,
        _prefix: &str,
        _expand_leaves: bool,
    ) -> String {
        let suffix = format!(
            "{} instances, {}",
            node.aggregate().instances,
            compact_bytes(node.aggregate().shallow_size)
        );
        if node.leaf().is_some() {
            format!("{label}  {suffix}")
        } else {
            format!(
                "{label}/  [{} classes in {} files, {suffix}]",
                node.aggregate().classes,
                node.leaf_count()
            )
        }
    }

    fn render_leaf_children(
        &self,
        prefix: &str,
        node: &BoundedTree<HeapClassMetrics, Vec<HeapClassSnapshotEntry>>,
        budget: usize,
        _expand_leaves: bool,
    ) -> Vec<String> {
        let Some(classes) = node.leaf() else {
            return Vec::new();
        };
        let mut classes = classes.iter().collect::<Vec<_>>();
        classes.sort_by_key(|class| std::cmp::Reverse(class.instance_count));
        let mut output = Vec::new();
        let mut rendered_classes = 0;
        for (index, class) in classes.iter().enumerate() {
            if output.len() >= budget {
                break;
            }
            if budget != usize::MAX
                && output.len().saturating_add(1) >= budget
                && index + 1 < classes.len()
            {
                break;
            }
            let last_class = index + 1 == classes.len();
            let branch = if last_class { "└─" } else { "├─" };
            let inline = if !self.instances && class.instance_count <= 3 {
                let aliases = class
                    .instances
                    .iter()
                    .map(|instance| format!("{} id {}", instance.alias, instance.heap_object_id))
                    .collect::<Vec<_>>()
                    .join(", ");
                (!aliases.is_empty())
                    .then(|| format!("  [{aliases}]"))
                    .unwrap_or_default()
            } else {
                String::new()
            };
            output.push(format!(
                "{prefix}{branch} {}  {} instances, {}{}",
                heap_class_label(class),
                class.instance_count,
                compact_bytes(class.shallow_size),
                inline
            ));
            rendered_classes += 1;
            if self.instances {
                let instance_budget = if index + 1 < classes.len() {
                    budget.saturating_sub(1)
                } else {
                    budget
                };
                let instance_prefix = format!("{prefix}{}", if last_class { "   " } else { "│  " });
                for (instance_index, instance) in class.instances.iter().enumerate() {
                    if output.len() >= instance_budget {
                        break;
                    }
                    let last_instance = instance_index + 1 == class.instances.len()
                        && class.omitted_instance_count == 0;
                    output.push(format!(
                        "{instance_prefix}{} {}  id {}  {}",
                        if last_instance { "└─" } else { "├─" },
                        instance.alias,
                        instance.heap_object_id,
                        compact_bytes(instance.shallow_size)
                    ));
                }
                if class.omitted_instance_count > 0 && output.len() < instance_budget {
                    output.push(format!(
                        "{instance_prefix}└─ … {} instances omitted",
                        class.omitted_instance_count
                    ));
                }
            }
        }
        if rendered_classes < classes.len() && output.len() < budget {
            output.push(format!(
                "{prefix}└─ … {} classes omitted",
                classes.len().saturating_sub(rendered_classes)
            ));
        }
        output.truncate(budget);
        output
    }

    fn render_omitted(
        &self,
        hidden_items: usize,
        hidden_leaves: usize,
        aggregate: &HeapClassMetrics,
    ) -> String {
        format!(
            "[{hidden_items} items, {} classes in {hidden_leaves} files, {} instances, {}]",
            aggregate.classes,
            aggregate.instances,
            compact_bytes(aggregate.shallow_size)
        )
    }

    fn render_all_pruned(
        &self,
        child_count: usize,
        hidden_leaves: usize,
        aggregate: &HeapClassMetrics,
    ) -> String {
        self.render_omitted(child_count, hidden_leaves, aggregate)
    }
}

fn print_heap_classes_human(snapshot: &HeapClassSnapshot, options: HeapClassOutputOptions) {
    for line in render_heap_classes_human(snapshot, options) {
        println!("{line}");
    }
}

fn render_heap_classes_human(
    snapshot: &HeapClassSnapshot,
    options: HeapClassOutputOptions,
) -> Vec<String> {
    let maximum_lines = if options.all {
        usize::MAX
    } else {
        options.max_lines
    };
    let mut output = vec![format!(
        "{} classes, {} instances, {} shallow size",
        snapshot.classes.len(),
        snapshot.total_instances,
        compact_bytes(snapshot.total_shallow_size)
    )];
    if output.len() >= maximum_lines {
        return output;
    }
    let analysis_duration = snapshot
        .analysis
        .parse_duration_micros
        .saturating_add(snapshot.analysis.projection_duration_micros);
    let analysis = format!(
        "{:.3}s (parse {:.3}s, projection {:.3}s; mapping {}; {} constructor groups{})",
        analysis_duration as f64 / 1_000_000.0,
        snapshot.analysis.parse_duration_micros as f64 / 1_000_000.0,
        snapshot.analysis.projection_duration_micros as f64 / 1_000_000.0,
        heap_mapping_status_label(&snapshot.analysis.mapping_status),
        snapshot.analysis.constructor_group_count,
        if snapshot.analysis.used_cached_groups {
            ", cached"
        } else {
            ""
        }
    );
    output.push(match &snapshot.analysis.snapshot_timing {
        Some(timing) => format!(
            "Total {:.3}s: snapshot {:.3}s (taking {:.3}s, retrieving {:.3}s), analysis {analysis}",
            timing
                .total_duration_micros()
                .saturating_add(analysis_duration) as f64
                / 1_000_000.0,
            timing.total_duration_micros() as f64 / 1_000_000.0,
            timing.taking_duration_micros as f64 / 1_000_000.0,
            timing.retrieving_duration_micros as f64 / 1_000_000.0,
        ),
        None => format!("Analysis {analysis}"),
    });
    if snapshot.classes.is_empty() {
        return output;
    }
    for diagnostic in &snapshot.analysis.script_mappings {
        if diagnostic.status != dbgjs::service_api::HeapMappingStatus::Mapped
            && output.len() < maximum_lines.saturating_sub(1) {
            output.push(format!("Mapping script:{} ({}): {}{}",
                diagnostic.script_id, diagnostic.url, heap_mapping_status_label(&diagnostic.status),
                diagnostic.diagnostic.as_ref().map(|reason| format!(" — {reason}")).unwrap_or_default()));
        }
    }
    if options.sort_by_instances {
        render_heap_classes_ranked(snapshot, &options, maximum_lines, &mut output);
        return output;
    }
    let mut files = BTreeMap::<String, Vec<HeapClassSnapshotEntry>>::new();
    for class in &snapshot.classes {
        files
            .entry(normalize_source_path(&class.source_url))
            .or_default()
            .push(class.clone());
    }
    let mut tree = BoundedTree::default();
    for (path, classes) in files {
        let metrics = classes
            .iter()
            .fold(HeapClassMetrics::default(), |mut metrics, class| {
                metrics.classes = metrics.classes.saturating_add(1);
                metrics.instances = metrics.instances.saturating_add(class.instance_count);
                metrics.shallow_size = metrics.shallow_size.saturating_add(class.shallow_size);
                metrics
            });
        tree.insert(
            path.split('/')
                .filter(|component| !component.is_empty())
                .map(str::to_owned),
            metrics,
            classes,
        );
    }
    let budget = if options.all {
        usize::MAX
    } else {
        maximum_lines.saturating_sub(output.len())
    };
    let style = HeapClassTreeStyle {
        instances: options.instances,
    };
    output.extend(tree.render_with_options(
        &style,
        true,
        TreeRenderOptions::terminal(budget, options.trim_width),
    ));
    if maximum_lines != usize::MAX {
        output.truncate(maximum_lines);
    }
    output
}

fn heap_mapping_status_label(status: &dbgjs::service_api::HeapMappingStatus) -> &'static str {
    use dbgjs::service_api::HeapMappingStatus;
    match status {
        HeapMappingStatus::NotAttempted => "not attempted (metadata unavailable)",
        HeapMappingStatus::NoMapSupplied => "no map supplied",
        HeapMappingStatus::MapLoadingFailed => "map loading failed",
        HeapMappingStatus::Mapped => "mapped (captured metadata)",
    }
}

fn heap_class_label(class: &HeapClassSnapshotEntry) -> String {
    let mut labels = Vec::new();
    if let Some(frame) = &class.provenance.frame_id { labels.push(format!("frame:{frame}")); }
    if let Some(context) = class.provenance.execution_context_id { labels.push(format!("context:{context}")); }
    if labels.is_empty() { class.name.clone() } else { format!("{} [{}]", class.name, labels.join(", ")) }
}

fn render_heap_classes_ranked(
    snapshot: &HeapClassSnapshot,
    options: &HeapClassOutputOptions,
    maximum_lines: usize,
    output: &mut Vec<String>,
) {
    let mut classes = snapshot.classes.iter().collect::<Vec<_>>();
    classes.sort_by(|left, right| {
        right
            .instance_count
            .cmp(&left.instance_count)
            .then_with(|| left.name.cmp(&right.name))
            .then_with(|| left.source_url.cmp(&right.source_url))
    });
    let mut rendered_classes = 0;
    for (index, class) in classes.iter().enumerate() {
        let remaining_classes = classes.len() - index;
        if output.len() >= maximum_lines {
            break;
        }
        if maximum_lines != usize::MAX
            && remaining_classes > 1
            && output.len().saturating_add(1) >= maximum_lines
        {
            break;
        }
        output.push(format!(
            "{:>4}. {}  {} instances, {}  {}:{}:{}",
            index + 1,
            heap_class_label(class),
            class.instance_count,
            compact_bytes(class.shallow_size),
            normalize_source_path(&class.source_url),
            class.location.line,
            class.location.column
        ));
        rendered_classes += 1;
        if options.instances {
            let detail_limit = if maximum_lines == usize::MAX {
                usize::MAX
            } else {
                maximum_lines.saturating_sub(usize::from(remaining_classes > 1))
            };
            for instance in &class.instances {
                if output.len() >= detail_limit {
                    break;
                }
                output.push(format!(
                    "      {}  id {}  {}",
                    instance.alias,
                    instance.heap_object_id,
                    compact_bytes(instance.shallow_size)
                ));
            }
            if class.omitted_instance_count > 0 && output.len() < detail_limit {
                output.push(format!(
                    "      ... {} instances omitted",
                    class.omitted_instance_count
                ));
            }
        }
    }
    if rendered_classes < classes.len() && output.len() < maximum_lines {
        let omitted = &classes[rendered_classes..];
        output.push(format!(
            "... {} classes omitted ({} instances, {})",
            omitted.len(),
            omitted
                .iter()
                .map(|class| class.instance_count)
                .sum::<u64>(),
            compact_bytes(omitted.iter().map(|class| class.shallow_size).sum::<u64>())
        ));
    }
    if maximum_lines != usize::MAX {
        output.truncate(maximum_lines);
    }
}

fn compact_bytes(bytes: u64) -> String {
    const UNITS: &[(&str, u64)] = &[
        ("GiB", 1024 * 1024 * 1024),
        ("MiB", 1024 * 1024),
        ("KiB", 1024),
    ];
    for (unit, size) in UNITS {
        if bytes >= *size {
            return format!("{:.1} {unit}", bytes as f64 / *size as f64);
        }
    }
    format!("{bytes} B")
}

fn print_coverage_tree(
    snapshot: &CoverageSnapshot,
    options: CoverageOutputOptions<'_>,
    files: BTreeMap<String, Vec<CoverageEntry>>,
) {
    let mut root = BoundedTree::default();
    for (path, entries) in files {
        let mut entries = aggregate_coverage_entries(entries);
        entries.sort_by_key(|entry| std::cmp::Reverse(entry.metrics.hit_lines));
        let metrics = effective_file_metrics(&entries);
        root.insert(
            path.split('/')
                .filter(|component| !component.is_empty())
                .map(str::to_owned),
            metrics,
            entries,
        );
    }

    println!(
        "{} RL (run lines), {} HL (hit lines)",
        root.aggregate().run_lines,
        root.aggregate().hit_lines
    );
    if let Some(analysis) = &snapshot.analysis {
        println!(
            "Analysis {:.1}s; source-map cache: {} hit, {} miss, {} bypass",
            analysis.duration_micros as f64 / 1_000_000.0,
            analysis.source_map_cache_hits,
            analysis.source_map_cache_misses,
            analysis.source_map_cache_bypasses
        );
    }
    let symbols = options.path.is_some() || options.path_glob.is_some() || options.all;
    let budget = if options.all {
        usize::MAX
    } else {
        options
            .max_lines
            .saturating_sub(
                1 + usize::from(snapshot.analysis.is_some())
                    + usize::from(snapshot.capture_id.is_some()),
            )
    };
    for line in root.render_with_options(
        &CoverageTreeStyle,
        symbols,
        TreeRenderOptions::terminal(budget, options.trim_width),
    ) {
        println!("{line}");
    }
}

fn effective_file_metrics(entries: &[CoverageEntry]) -> CoverageMetrics {
    let hit_lines = entries
        .iter()
        .flat_map(|entry| entry.line_counts.keys().copied())
        .collect::<BTreeSet<_>>();
    if hit_lines.is_empty() {
        CoverageMetrics {
            hit_lines: entries.iter().map(|entry| entry.metrics.hit_lines).sum(),
            run_lines: entries.iter().map(|entry| entry.metrics.run_lines).sum(),
        }
    } else {
        CoverageMetrics {
            hit_lines: hit_lines.len() as u64,
            run_lines: entries.iter().map(|entry| entry.metrics.run_lines).sum(),
        }
    }
}

#[derive(Clone, Copy, Default)]
struct CoverageMetrics {
    hit_lines: u64,
    run_lines: u64,
}

impl CoverageMetrics {
    fn add(&mut self, other: Self) {
        self.hit_lines = self.hit_lines.saturating_add(other.hit_lines);
        self.run_lines = self.run_lines.saturating_add(other.run_lines);
    }

    fn compact(self) -> String {
        format!("{} HL, {} RL", self.hit_lines, self.run_lines)
    }
}

impl TreeAggregate for CoverageMetrics {
    fn merge(&mut self, other: &Self) {
        self.add(*other);
    }
}

#[derive(Clone)]
struct CoverageEntry {
    path: String,
    function: String,
    line_counts: BTreeMap<u32, u64>,
    metrics: CoverageMetrics,
    generated_location: Option<String>,
}

fn aggregate_coverage_entries(entries: Vec<CoverageEntry>) -> Vec<CoverageEntry> {
    let mut symbols = BTreeMap::<String, BTreeMap<u32, u64>>::new();
    let mut unknown = BTreeMap::<String, CoverageMetrics>::new();
    let mut paths = BTreeMap::<String, String>::new();
    let mut generated_locations = BTreeMap::<String, String>::new();
    for entry in entries {
        paths
            .entry(entry.function.clone())
            .or_insert(entry.path.clone());
        if let Some(location) = entry.generated_location {
            generated_locations
                .entry(entry.function.clone())
                .or_insert(location);
        }
        if entry.line_counts.is_empty() {
            unknown
                .entry(entry.function)
                .or_default()
                .add(entry.metrics);
        } else {
            let lines = symbols.entry(entry.function).or_default();
            for (line, count) in entry.line_counts {
                lines
                    .entry(line)
                    .and_modify(|current| *current = (*current).max(count))
                    .or_insert(count);
            }
        }
    }
    paths
        .into_iter()
        .map(|(function, path)| {
            let line_counts = symbols.remove(&function).unwrap_or_default();
            let metrics = if line_counts.is_empty() {
                unknown.remove(&function).unwrap_or(CoverageMetrics {
                    hit_lines: 1,
                    run_lines: 1,
                })
            } else {
                CoverageMetrics {
                    hit_lines: line_counts.len() as u64,
                    run_lines: line_counts.values().sum(),
                }
            };
            let generated_location = generated_locations.remove(&function);
            CoverageEntry {
                path,
                function,
                line_counts,
                metrics,
                generated_location,
            }
        })
        .collect()
}

fn coverage_entries(snapshot: &CoverageSnapshot) -> Vec<CoverageEntry> {
    let mut entries = Vec::new();
    for source in &snapshot.sources {
        for function in &source.functions {
            if function.name == "(anonymous)" {
                continue;
            }
            if function.effective_ranges.is_empty()
                && !function.ranges.iter().any(|range| range.count > 0)
            {
                continue;
            }
            let projected_ranges = if function.effective_ranges.is_empty() {
                &function.ranges
            } else {
                &function.effective_ranges
            };
            let mut ranges = projected_ranges.iter().collect::<Vec<_>>();
            ranges.sort_by_key(|range| {
                std::cmp::Reverse(range.end_offset.saturating_sub(range.start_offset))
            });
            let mut lines_by_path = BTreeMap::<String, BTreeMap<u32, u64>>::new();
            let mut unmapped_run_lines = 0_u64;
            for range in ranges {
                match (&range.authored_start, &range.authored_end) {
                    (Some(start), Some(end)) if start.source_url == end.source_url => {
                        let lines = lines_by_path
                            .entry(normalize_source_path(&start.source_url))
                            .or_default();
                        for line in start.line..=end.line.max(start.line) {
                            lines
                                .entry(line)
                                .and_modify(|count| *count = (*count).max(range.count))
                                .or_insert(range.count);
                        }
                    }
                    _ if range.count > 0 => {
                        unmapped_run_lines = unmapped_run_lines.saturating_add(range.count)
                    }
                    _ => {}
                }
            }
            let function_name = function
                .breadcrumb
                .clone()
                .unwrap_or_else(|| function.name.clone());
            let generated_location = (function.breadcrumb.is_none()
                && looks_minified_identifier(&function.name))
            .then(|| format_generated_location(source, function));
            let mut mapped = false;
            for (path, line_counts) in lines_by_path {
                if line_counts.is_empty() {
                    continue;
                }
                mapped = true;
                let metrics = CoverageMetrics {
                    hit_lines: line_counts.len() as u64,
                    run_lines: line_counts.values().sum(),
                };
                entries.push(CoverageEntry {
                    path,
                    function: function_name.clone(),
                    line_counts,
                    metrics,
                    generated_location: generated_location.clone(),
                });
            }
            if !mapped && unmapped_run_lines > 0 {
                entries.push(CoverageEntry {
                    path: source.generated_url.clone(),
                    function: function_name,
                    line_counts: BTreeMap::new(),
                    metrics: CoverageMetrics {
                        hit_lines: 1,
                        run_lines: unmapped_run_lines,
                    },
                    generated_location,
                });
            }
        }
    }
    entries
}

fn format_generated_location(
    source: &dbgjs::service_api::CoverageSourceSnapshot,
    function: &dbgjs::service_api::CoverageFunctionSnapshot,
) -> String {
    match &function.generated_location {
        Some(location) => {
            let source = location
                .source_url
                .rsplit('/')
                .next()
                .unwrap_or(&location.source_url)
                .split('?')
                .next()
                .unwrap_or(&location.source_url);
            format!("{}:{}:{}", source, location.line, location.column)
        }
        None => {
            let source = source
                .generated_url
                .rsplit('/')
                .next()
                .unwrap_or(&source.generated_url)
                .split('?')
                .next()
                .unwrap_or(&source.generated_url);
            format!("{source}@offset {}", function.root_start_offset)
        }
    }
}

fn looks_minified_identifier(name: &str) -> bool {
    let name = name
        .strip_prefix("get ")
        .or_else(|| name.strip_prefix("set "))
        .unwrap_or(name);
    if name.contains('.') || name.contains(' ') {
        return false;
    }
    let length = name.chars().count();
    length <= 3
        || (length <= 4
            && (name.chars().any(|character| character.is_ascii_digit())
                || name
                    .chars()
                    .filter(|character| character.is_ascii_uppercase())
                    .count()
                    >= 2))
}

fn normalize_source_path(path: &str) -> String {
    path.trim_start_matches("../")
        .trim_start_matches("./")
        .to_owned()
}

struct CoverageTreeStyle;

impl BoundedTreeStyle<CoverageMetrics, Vec<CoverageEntry>> for CoverageTreeStyle {
    fn sort_weight(&self, aggregate: &CoverageMetrics) -> u64 {
        aggregate.hit_lines
    }

    fn expansion_weight(
        &self,
        node: &BoundedTree<CoverageMetrics, Vec<CoverageEntry>>,
        symbols: bool,
    ) -> u64 {
        if !symbols
            && node
                .leaf()
                .is_some_and(|entries| single_class_summary(entries).is_some())
        {
            return 0;
        }
        let has_class = node
            .leaf()
            .is_some_and(|entries| entries.iter().any(has_resolved_class));
        if node.children().is_empty() && !(symbols && node.leaf().is_some()) && !has_class {
            return 0;
        }
        (node.aggregate().hit_lines.max(1) as f64).sqrt().ceil() as u64
    }

    fn render_node(
        &self,
        label: &str,
        node: &BoundedTree<CoverageMetrics, Vec<CoverageEntry>>,
        prefix: &str,
        expand_leaves: bool,
    ) -> String {
        match node.leaf() {
            None => format!(
                "{label}/  [{} files, {}]",
                node.leaf_count(),
                node.aggregate().compact()
            ),
            Some(entries) => {
                let collapsed_class = (!expand_leaves)
                    .then(|| single_class_summary(entries))
                    .flatten();
                let label = collapsed_class.as_ref().map_or_else(
                    || label.to_owned(),
                    |class| format!("{label}/{}", class.name),
                );
                let collapsed_suffix = collapsed_class.as_ref().map_or_else(String::new, |class| {
                    let inline = inline_method_summary(class, prefix, 120);
                    if inline.is_empty() {
                        "  [methods pruned]".to_owned()
                    } else {
                        inline
                    }
                });
                format!(
                    "{label}  {}{}{}",
                    node.aggregate().compact(),
                    inline_symbol_summary(node, &label, prefix, 120),
                    collapsed_suffix
                )
            }
        }
    }

    fn render_leaf_children(
        &self,
        prefix: &str,
        node: &BoundedTree<CoverageMetrics, Vec<CoverageEntry>>,
        budget: usize,
        expand_leaves: bool,
    ) -> Vec<String> {
        node.leaf().map_or_else(Vec::new, |entries| {
            if !expand_leaves && single_class_summary(entries).is_some() {
                Vec::new()
            } else {
                render_symbol_groups(prefix, entries, budget, expand_leaves)
            }
        })
    }

    fn render_omitted(
        &self,
        hidden_items: usize,
        hidden_files: usize,
        metrics: &CoverageMetrics,
    ) -> String {
        format!(
            "[{hidden_items} items, {hidden_files} files, {}]",
            metrics.compact()
        )
    }

    fn render_all_pruned(
        &self,
        child_count: usize,
        hidden_files: usize,
        metrics: &CoverageMetrics,
    ) -> String {
        format!(
            "[all {child_count} children pruned, {hidden_files} files, {}]",
            metrics.compact()
        )
    }
}

fn inline_symbol_summary(
    tree: &BoundedTree<CoverageMetrics, Vec<CoverageEntry>>,
    label: &str,
    prefix: &str,
    maximum: usize,
) -> String {
    let resolved = tree
        .leaf()
        .into_iter()
        .flatten()
        .filter(|entry| entry.generated_location.is_none() && !has_resolved_class(entry))
        .cloned()
        .collect::<Vec<_>>();
    let summaries = symbol_summaries(&resolved);
    if summaries.is_empty() {
        return String::new();
    }
    let rendered = summaries
        .iter()
        .map(|summary| format!("{} {}", summary.name, summary.metrics.compact()))
        .collect::<Vec<_>>()
        .join(", ");
    let suffix = format!(" [{rendered}]");
    let base = prefix.chars().count()
        + label.chars().count()
        + tree.aggregate().compact().chars().count()
        + 4;
    (base + suffix.chars().count() <= maximum)
        .then_some(suffix)
        .unwrap_or_default()
}

struct SymbolSummary<'a> {
    name: String,
    metrics: CoverageMetrics,
    entries: Vec<&'a CoverageEntry>,
    is_class: bool,
}

fn symbol_summaries(entries: &[CoverageEntry]) -> Vec<SymbolSummary<'_>> {
    let mut groups = BTreeMap::<String, Vec<&CoverageEntry>>::new();
    for entry in entries {
        let name = entry
            .function
            .split_once('.')
            .filter(|(class, method)| {
                !class.is_empty()
                    && !method.is_empty()
                    && class.starts_with(|character: char| character.is_ascii_uppercase())
            })
            .map_or_else(|| entry.function.clone(), |(class, _)| class.to_owned());
        groups.entry(name).or_default().push(entry);
    }

    let mut summaries = groups
        .into_iter()
        .map(|(name, entries)| SymbolSummary {
            metrics: effective_file_metrics(&entries.iter().copied().cloned().collect::<Vec<_>>()),
            is_class: entries.iter().any(|entry| has_resolved_class(entry)),
            name,
            entries,
        })
        .collect::<Vec<_>>();
    summaries.sort_by_key(|summary| std::cmp::Reverse(summary.metrics.hit_lines));
    summaries
}

fn single_class_summary(entries: &[CoverageEntry]) -> Option<SymbolSummary<'_>> {
    let mut summaries = symbol_summaries(entries);
    (summaries.len() == 1 && summaries[0].is_class).then(|| summaries.remove(0))
}

fn render_symbol_groups(
    prefix: &str,
    entries: &[CoverageEntry],
    budget: usize,
    expand_methods: bool,
) -> Vec<String> {
    let summaries = symbol_summaries(entries)
        .into_iter()
        .filter(|summary| expand_methods || summary.is_class)
        .collect::<Vec<_>>();
    let mut output = Vec::new();
    for (index, summary) in summaries.iter().enumerate() {
        if output.len() >= budget {
            break;
        }
        let last = index + 1 == summaries.len();
        let branch = if last { "└─" } else { "├─" };
        output.push(format!(
            "{prefix}{branch} {}  {}{}",
            summary.name,
            summary.metrics.compact(),
            if !expand_methods && summary.is_class {
                inline_method_summary(summary, prefix, 120)
            } else if summary.entries.len() == 1 {
                summary.entries[0]
                    .generated_location
                    .as_ref()
                    .map_or_else(String::new, |location| format!("  [generated {location}]"))
            } else {
                String::new()
            }
        ));
        if expand_methods && summary.entries.len() > 1 {
            let child_prefix = format!("{prefix}{}", if last { "   " } else { "│  " });
            for (method_index, entry) in summary.entries.iter().enumerate() {
                if output.len() >= budget {
                    break;
                }
                let method = entry
                    .function
                    .split_once('.')
                    .map_or(entry.function.as_str(), |(_, method)| method);
                let method_last = method_index + 1 == summary.entries.len();
                output.push(format!(
                    "{child_prefix}{} {method}  {}{}",
                    if method_last { "└─" } else { "├─" },
                    entry.metrics.compact(),
                    entry
                        .generated_location
                        .as_ref()
                        .map_or_else(String::new, |location| format!("  [generated {location}]"))
                ));
            }
        }
    }
    output
}

fn has_resolved_class(entry: &CoverageEntry) -> bool {
    entry
        .function
        .split_once('.')
        .is_some_and(|(class, method)| {
            !class.is_empty()
                && !method.is_empty()
                && class.starts_with(|character: char| character.is_ascii_uppercase())
        })
}

fn inline_method_summary(summary: &SymbolSummary<'_>, prefix: &str, maximum: usize) -> String {
    let rendered = summary
        .entries
        .iter()
        .map(|entry| {
            let method = entry
                .function
                .split_once('.')
                .map_or(entry.function.as_str(), |(_, method)| method);
            format!("{method} {}", entry.metrics.compact())
        })
        .collect::<Vec<_>>()
        .join(", ");
    let suffix = format!(" [{rendered}]");
    let base = prefix.chars().count()
        + summary.name.chars().count()
        + summary.metrics.compact().chars().count()
        + 4;
    (base + suffix.chars().count() <= maximum)
        .then_some(suffix)
        .unwrap_or_default()
}

fn print_source_excerpt(label: &str, source: &SourceExcerpt) {
    match &source.breadcrumb {
        Some(breadcrumb) => println!("  {label}: {} — {breadcrumb}", source.source_url),
        None => println!("  {label}: {}", source.source_url),
    }
    let width = source
        .lines
        .last()
        .map_or(1, |line| line.line.to_string().len());
    for line in &source.lines {
        let marker = if line.line == source.current_line {
            ">"
        } else {
            " "
        };
        println!("    {marker} {:>width$} | {}", line.line, line.text);
        if line.line == source.current_line {
            println!(
                "      {:width$} | {}{}",
                "",
                " ".repeat(source.highlight_start.saturating_sub(1) as usize),
                "^".repeat(source.highlight_length as usize)
            );
        }
    }
}

fn runtime_location(location: &dbgjs::service_api::SourceLocation) -> String {
    if location.source_url.is_empty() {
        format!(
            "runtime (anonymous script):{}:{}",
            location.line, location.column
        )
    } else {
        format!(
            "runtime {}:{}:{}",
            location.source_url, location.line, location.column
        )
    }
}

fn render_evaluation(evaluation: &EvaluationSnapshot) -> String {
    render_value_preview(&evaluation.preview)
}

fn eval_truncation_guidance(value: &ValueSnapshot, full: bool) -> Option<&'static str> {
    value.preview.truncated.then_some(if full {
        "Preview remains incomplete in --full mode because this value cannot be safely represented in full. Evaluate JSON.stringify(value) to render a JSON-serializable value."
    } else {
        "Preview truncated; rerun target eval with --full or --max-preview-length <n>. For objects, evaluate JSON.stringify(value) to request JSON serialization."
    })
}

fn render_value_snapshot(value: &ValueSnapshot) -> String {
    value
        .class_name
        .as_deref()
        .filter(|_| value.preview.preview.is_none())
        .map_or_else(
            || render_value_preview_with_reference(&value.preview),
            |class_name| {
                let reference = value
                    .preview
                    .reference
                    .as_deref()
                    .map(|reference| format!(" ({reference})"))
                    .unwrap_or_default();
                format!("{class_name}{reference}{}", render_object_source(&value.preview.source))
            },
        )
}

fn render_value_preview_with_reference(
    value: &dbgjs::service_api::ValuePreviewSnapshot,
) -> String {
    let preview = render_value_preview(value);
    let reference = value
        .reference
        .as_deref()
        .map(|reference| format!(" ({reference})"))
        .unwrap_or_default();
    format!("{preview}{reference}")
}

fn render_value_preview(value: &dbgjs::service_api::ValuePreviewSnapshot) -> String {
    let preview = value.preview.as_deref().unwrap_or(&value.kind);
    let truncated = if value.truncated { "..." } else { "" };
    format!("{}{truncated}{}", terminal_text(preview), render_object_source(&value.source))
}

fn render_object_source(source: &dbgjs::object_inspection::ObjectSourceSnapshot) -> String {
    let mut output = String::new();
    if source.has_conflicting_locations() {
        output.push_str("\n  source conflict: location evidence disagrees; all positions retained");
    }
    for location in &source.locations {
        if location.origin == "live"
            && let Some(snapshot) = source.locations.iter().find(|snapshot| {
                snapshot.origin == "heapSnapshot"
                    && snapshot.kind == location.kind
                    && snapshot.script_id == location.script_id
                    && snapshot.position.generated == location.position.generated
                    && snapshot.position.resolved == location.position.resolved
                    && snapshot.position.mapping == location.position.mapping
            })
        {
            if location.position.diagnostic != snapshot.position.diagnostic
                && let Some(diagnostic) = &location.position.diagnostic
            {
                output.push_str(&format!("\n  source note: {}", terminal_text(diagnostic)));
            }
            continue;
        }
        let position = &location.position.resolved;
        output.push_str(&format!(
            "\n  source [{}; {}; {}]: {}:{}:{}",
            location.origin, location.kind, location.position.mapping,
            source_path_text(&position.source_url), position.line, position.column,
        ));
        if let Some(breadcrumb) = &location.position.breadcrumb {
            output.push_str(&format!(" -- {}", terminal_text(breadcrumb)));
        }
        if let Some(diagnostic) = &location.position.diagnostic {
            output.push_str(&format!("\n  source note: {}", terminal_text(diagnostic)));
        }
    }
    for diagnostic in &source.diagnostics {
        output.push_str(&format!("\n  source note: {}", terminal_text(diagnostic)));
    }
    output
}

fn source_path_text(value: &str) -> String {
    value.chars().flat_map(|character| {
        if character.is_control() {
            character.escape_default().collect::<Vec<_>>()
        } else {
            vec![character]
        }
    }).collect()
}

fn terminal_text(value: &str) -> String {
    value.escape_default().to_string()
}

fn connection_configuration(configuration: &ConnectionConfiguration) -> String {
    match configuration {
        ConnectionConfiguration::DirectCdp { endpoint } => {
            format!("direct CDP at {endpoint}")
        }
        ConnectionConfiguration::NodeInspector { endpoint } => {
            format!("Node inspector at {endpoint}")
        }
        ConnectionConfiguration::Process { process_id } => {
            format!("process {process_id}")
        }
        ConnectionConfiguration::ProcessTree { root_pid } => {
            format!("process tree rooted at PID {root_pid}")
        }
        ConnectionConfiguration::ScopedProcessTree {
            root_pid,
            target_id,
        } => {
            format!("process tree rooted at PID {root_pid}, scoped to {target_id}")
        }
        ConnectionConfiguration::Playwright {
            url,
            playwright_package: _,
            channel,
            headless,
            ignore_https_errors,
        } => format!(
            "Playwright {}{}{} opening {url}",
            playwright_channel(channel),
            if *headless { " headless" } else { " headed" },
            if *ignore_https_errors {
                " (ignoring HTTPS errors)"
            } else {
                ""
            }
        ),
        ConnectionConfiguration::Chrome {
            url,
            executable,
            headless,
            ..
        } => format!(
            "Chrome at {executable}{} opening {url}",
            if *headless { " headless" } else { " headed" },
        ),
        ConnectionConfiguration::Node {
            program,
            runtime_executable,
            ..
        } => format!("Node.js at {runtime_executable} running {program}"),
        ConnectionConfiguration::Stdio {
            command, topology, ..
        } => format!(
            "CDP over stdio from {command} ({})",
            match topology {
                dbgjs::service_api::CdpStdioTopology::Browser => "browser",
                dbgjs::service_api::CdpStdioTopology::Target => "target",
            }
        ),
    }
}

fn connection_configuration_kind(configuration: &ConnectionConfiguration) -> &'static str {
    match configuration {
        ConnectionConfiguration::DirectCdp { .. } => "direct-cdp",
        ConnectionConfiguration::NodeInspector { .. } => "node-inspector",
        ConnectionConfiguration::Process { .. } => "process",
        ConnectionConfiguration::ProcessTree { .. } => "process-tree",
        ConnectionConfiguration::ScopedProcessTree { .. } => "scoped-process-tree",
        ConnectionConfiguration::Playwright { .. } => "playwright",
        ConnectionConfiguration::Chrome { .. } => "chrome",
        ConnectionConfiguration::Node { .. } => "node",
        ConnectionConfiguration::Stdio { .. } => "stdio",
    }
}

fn playwright_channel(channel: &PlaywrightChannel) -> &'static str {
    match channel {
        PlaywrightChannel::Bundled => "bundled Chromium",
        PlaywrightChannel::Chrome => "Chrome",
        PlaywrightChannel::ChromeBeta => "Chrome Beta",
        PlaywrightChannel::ChromeDev => "Chrome Dev",
        PlaywrightChannel::ChromeCanary => "Chrome Canary",
        PlaywrightChannel::Msedge => "Microsoft Edge",
        PlaywrightChannel::MsedgeBeta => "Microsoft Edge Beta",
        PlaywrightChannel::MsedgeDev => "Microsoft Edge Dev",
        PlaywrightChannel::MsedgeCanary => "Microsoft Edge Canary",
    }
}

fn connection_status(status: &ConnectionStatus) -> String {
    match status {
        ConnectionStatus::Disconnected => "disconnected".to_owned(),
        ConnectionStatus::Connecting => "connecting".to_owned(),
        ConnectionStatus::Disconnecting => "disconnecting".to_owned(),
        ConnectionStatus::Connected {
            product,
            protocol_version,
        } => format!("connected to {product}; CDP {protocol_version}"),
        ConnectionStatus::Failed { message } => format!("failed: {message}"),
    }
}

fn target_phase(phase: &TargetDebuggerPhase) -> String {
    match phase {
        TargetDebuggerPhase::Running => "running".to_owned(),
        TargetDebuggerPhase::Paused { epoch } => format!("paused at epoch {epoch}"),
        TargetDebuggerPhase::Resuming { epoch } => format!("resuming epoch {epoch}"),
        TargetDebuggerPhase::Failed { message } => format!("failed: {message}"),
    }
}

fn target_breakpoint_status(status: &TargetBreakpointStatus) -> String {
    match status {
        TargetBreakpointStatus::WaitingForScript => "waiting for script".to_owned(),
        TargetBreakpointStatus::SourceNotFound { .. } => "source not found".to_owned(),
        TargetBreakpointStatus::AmbiguousSource { candidates, .. } => {
            format!("ambiguous source; {} candidates", candidates.len())
        }
        TargetBreakpointStatus::Unmapped { .. } => "source found but location unmapped".to_owned(),
        TargetBreakpointStatus::Applicable { mapping_count } => {
            format!("applicable; {mapping_count} mapping(s)")
        }
        TargetBreakpointStatus::Installing { application_count } => {
            format!("installing; {application_count} application(s)")
        }
        TargetBreakpointStatus::Installed { binding_count } => {
            let suffix = if *binding_count == 1 { "" } else { "s" };
            format!("installed; {binding_count} binding{suffix}")
        }
        TargetBreakpointStatus::Failed { message } => format!("failed: {message}"),
    }
}

fn print_breakpoint_pending_reason(reason: &BreakpointPendingReason, indent: &str) {
    match reason {
        BreakpointPendingReason::WaitingForTarget => {
            println!("{indent}waiting for an eligible target")
        }
        BreakpointPendingReason::WaitingForScript => {
            println!("{indent}waiting for target scripts and source maps")
        }
        BreakpointPendingReason::SourceNotFound { diagnostics } => {
            println!("{indent}source not found in loaded scripts");
            for diagnostic in diagnostics {
                println!("{indent}- {diagnostic}");
            }
        }
        BreakpointPendingReason::AmbiguousSource {
            candidates,
            omitted_candidate_count,
        } => {
            println!("{indent}source is ambiguous; qualify one candidate:");
            for candidate in candidates {
                println!(
                    "{indent}- {} [{}; {}]",
                    candidate.source_url, candidate.content_hash, candidate.provenance
                );
            }
            if *omitted_candidate_count > 0 {
                println!("{indent}- … and {omitted_candidate_count} more");
            }
        }
        BreakpointPendingReason::Unmapped { diagnostics } => {
            println!("{indent}source matched, but the requested location is unmapped");
            for diagnostic in diagnostics {
                println!("{indent}- {diagnostic}");
            }
        }
        BreakpointPendingReason::Applicable => {
            println!("{indent}mapped and ready for physical application")
        }
        BreakpointPendingReason::Installing => {
            println!("{indent}mapped; physical breakpoint installation is in progress")
        }
        BreakpointPendingReason::Failed { message } => println!("{indent}failed: {message}"),
    }
}

fn print_target_breakpoint_explanation(
    breakpoint: &dbgjs::service_api::TargetBreakpointSnapshot,
) {
    match &breakpoint.status {
        TargetBreakpointStatus::WaitingForScript => {
            println!(
                "      waiting for a loaded script that resolves '{}'",
                breakpoint.source_url
            );
        }
        TargetBreakpointStatus::SourceNotFound { diagnostics }
        | TargetBreakpointStatus::Unmapped { diagnostics } => {
            for diagnostic in diagnostics {
                println!("      {diagnostic}");
            }
        }
        TargetBreakpointStatus::AmbiguousSource {
            candidates,
            omitted_candidate_count,
        } => {
            for candidate in candidates {
                println!(
                    "      candidate {} [{}; {}]",
                    candidate.source_url, candidate.content_hash, candidate.provenance
                );
            }
            if *omitted_candidate_count > 0 {
                println!("      … and {omitted_candidate_count} more");
            }
        }
        TargetBreakpointStatus::Applicable { .. }
        | TargetBreakpointStatus::Installing { .. }
        | TargetBreakpointStatus::Installed { .. }
        | TargetBreakpointStatus::Failed { .. } => {}
    }
    for application in &breakpoint.applications {
        if let Some(mapping) = &application.mapping {
            println!(
                "      script {} v{} -> {}:{}:{} via {}",
                application.script_id,
                application.script_version,
                mapping.generated_url,
                mapping.generated_line,
                mapping.generated_column,
                mapping.projection.join(" -> ")
            );
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{
        BoundedTree, CoverageEntry, CoverageMetrics, CoverageTreeStyle, HeapClassOutputOptions,
        ProcessTreeOutputOptions, SourceTreeOutputOptions, TargetListEntry,
        aggregate_coverage_entries, coverage_entries, effective_file_metrics,
        eval_truncation_guidance, heap_node_line, heap_path_lines, heap_reference_line, heap_show_lines, looks_minified_identifier,
        page_logs, process_tree_lines, process_trees_json, render_compacted_source_graph,
        render_evaluation,
        render_heap_classes_human, render_source_search, render_uncompacted_source_graph,
        render_value_snapshot,
        source_search_incomplete_message, source_tree_lines, style_process_label,
        style_session_label, target_tree_selector, terminal_text,
    };
    use dbgjs::service_api::{
        AgentSessionSnapshot, CompactedSourceEdgeSnapshot, CompactedSourceGraphSnapshot,
        CompactedSourceNodeSnapshot, ConsoleMessageSnapshot, CoverageFunctionSnapshot,
        CoverageRangeSnapshot, CoverageSnapshot, CoverageSourceSnapshot, EvaluationSnapshot,
        HeapClassAnalysisSnapshot, HeapClassSnapshot, HeapClassSnapshotEntry, HeapEdgePolicy,
        HeapInstanceSnapshot, HeapNodeSnapshot, HeapPathSnapshot, HeapPathStepSnapshot,
        HeapReferenceDirection, HeapReferenceSnapshot, HeapReferencesSnapshot, HeapSnapshotTiming,
        HeapTraversalDirection, ProcessRole, ProcessRootKind, ProcessSnapshot,
        ProcessTargetSnapshot, ProcessTreeSnapshot, SourceLocation, SourceMatchSnapshot,
        SourceSearchSkip,
        SourceSearchSnapshot, SourceSuffixRewriteSnapshot, SourceTreeKind, SourceTreeSnapshot,
        TargetSnapshot, UncompactedProjectionSnapshot,
        UncompactedSourceEdgeSnapshot, UncompactedSourceGraphSnapshot,
        UncompactedSourceNodeSnapshot, UncompactedSourceRevisionSnapshot, ValuePreviewSnapshot,
        ValueSelector, ValueSnapshot,
    };
    use std::collections::BTreeMap;

    #[test]
    fn terminal_text_escapes_control_sequences() {
        assert_eq!(
            terminal_text("\u{1b}]8;;https://evil.test\u{7}label"),
            "\\u{1b}]8;;https://evil.test\\u{7}label"
        );
    }

    #[test]
    fn object_source_output_keeps_complete_resolvable_paths_and_conflicting_evidence() {
        let path = format!("file:///workspace/{}/provider.ts", "long-directory/".repeat(30));
        let generated = SourceLocation { source_url: "file:///workspace/app.min.js".into(), line: 1, column: 89 };
        let position = dbgjs::source_location::ResolvedSourcePosition {
            generated: generated.clone(),
            resolved: SourceLocation { source_url: path.clone(), line: 42, column: 7 },
            breadcrumb: Some("Provider.provideModels".into()),
            mapping: "authored".into(),
            diagnostic: None,
        };
        let heap = dbgjs::object_inspection::ObjectLocationSnapshot {
            origin: "heapSnapshot".into(), kind: "function".into(), script_id: "7".into(),
            position,
        };
        let mut live = heap.clone();
        live.origin = "live".into();
        live.position.generated.column += 1;
        let source = dbgjs::object_inspection::ObjectSourceSnapshot {
            locations: vec![heap, live], diagnostics: vec![],
        };
        let rendered = super::render_object_source(&source);
        assert_eq!(rendered.matches(&path).count(), 2);
        assert!(rendered.contains(":42:7"));
        assert!(rendered.contains("source conflict"));
        assert!(rendered.contains("Provider.provideModels"));
        assert!(!rendered.contains("..."));
        assert_eq!(super::source_path_text("file:///src/\u{e9}.ts"), "file:///src/\u{e9}.ts");
        assert!(!super::source_path_text("file:///src/\u{1b}.ts").contains('\u{1b}'));
    }

    #[test]
    fn object_source_output_prefers_snapshot_for_matching_live_locations() {
        use dbgjs::object_inspection::{ObjectLocationSnapshot, ObjectSourceSnapshot};
        let heap = ObjectLocationSnapshot {
            origin: "heapSnapshot".into(),
            kind: "constructor".into(),
            script_id: "7".into(),
            position: dbgjs::source_location::ResolvedSourcePosition {
                generated: SourceLocation {
                    source_url: "file:///workspace/app.min.js".into(), line: 1, column: 89,
                },
                resolved: SourceLocation {
                    source_url: "file:///workspace/provider.ts".into(), line: 6, column: 3,
                },
                breadcrumb: Some("Provider.constructor".into()),
                mapping: "authored".into(),
                diagnostic: None,
            },
        };
        let mut live = heap.clone();
        live.origin = "live".into();
        for locations in [vec![heap.clone(), live.clone()], vec![live.clone(), heap.clone()]] {
            let source = ObjectSourceSnapshot { locations, diagnostics: vec![] };
            let rendered = super::render_object_source(&source);
            assert_eq!(rendered.matches("source [").count(), 1);
            assert!(rendered.contains("[heapSnapshot; constructor; authored]"));
            assert!(!rendered.contains("[live;"));
            assert!(!rendered.contains("source conflict"));
            assert_eq!(serde_json::to_value(&source).unwrap()["locations"].as_array().unwrap().len(), 2);
        }
        let mut source = ObjectSourceSnapshot { locations: vec![live.clone()], diagnostics: vec![] };
        assert!(super::render_object_source(&source).contains("[live; constructor; authored]"));
        source.locations.push(heap);
        source.locations[0].position.diagnostic = Some("live mapping note".into());
        let rendered = super::render_object_source(&source);
        assert_eq!(rendered.matches("source [").count(), 1);
        assert!(rendered.contains("live mapping note"));
        source.locations[0].position.resolved.line += 1;
        let rendered = super::render_object_source(&source);
        assert_eq!(rendered.matches("source [").count(), 2);
        assert!(rendered.contains("source conflict"));
        source.locations[0] = live;
        source.locations[0].kind = "function".into();
        assert_eq!(super::render_object_source(&source).matches("source [").count(), 2);
    }

    #[test]
    fn evaluation_and_value_snapshot_share_bounded_preview_rendering() {
        let preview = ValuePreviewSnapshot {
            kind: "string".to_owned(),
            preview: Some("bounded".to_owned()),
            truncated: true,
            reference: None,
            source: Default::default(),
        };
        let evaluation = EvaluationSnapshot {
            expression: "value".to_owned(),
            kind: "string".to_owned(),
            value: Some(serde_json::json!("unbounded legacy value")),
            unserializable_value: None,
            description: None,
            object_id: None,
            preview: preview.clone(),
        };
        let value = ValueSnapshot {
            selector: ValueSelector::Expression {
                expression: "value".to_owned(),
                allow_side_effects: false,
            },
            subtype: None,
            class_name: None,
            preview,
            properties: Vec::new(),
            omitted_property_count: 0,
            properties_truncated: false,
            promise: None,
        };

        assert_eq!(render_evaluation(&evaluation), "bounded...");
        assert_eq!(
            render_evaluation(&evaluation),
            render_value_snapshot(&value)
        );
    }

    #[test]
    fn evaluation_truncation_guidance_distinguishes_preview_limits_from_full_mode() {
        let value = ValueSnapshot {
            selector: ValueSelector::Expression {
                expression: "value".to_owned(),
                allow_side_effects: true,
            },
            subtype: None,
            class_name: None,
            preview: ValuePreviewSnapshot {
                kind: "symbol".to_owned(),
                preview: None,
                truncated: true,
                reference: None,
                source: Default::default(),
            },
            properties: Vec::new(),
            omitted_property_count: 0,
            properties_truncated: false,
            promise: None,
        };

        assert!(eval_truncation_guidance(&value, false).unwrap().contains("--full"));
        assert!(
            eval_truncation_guidance(&value, true)
                .unwrap()
                .contains("cannot be safely represented")
        );
        assert_eq!(
            serde_json::to_value(&value).unwrap()["preview"],
            serde_json::json!({
                "kind": "symbol",
                "preview": null,
                "truncated": true,
                "reference": null,
                "source": { "locations": [], "diagnostics": [] }
            })
        );
    }

    fn source_search_match(path: &str, line: u32, column: u32, text: &str) -> SourceMatchSnapshot {
        SourceMatchSnapshot {
            path: path.to_owned(),
            content_hash: "hash".to_owned(),
            kind: "authored".to_owned(),
            provenance: "source map".to_owned(),
            connection_id: Some("connection".to_owned()),
            target_id: Some("target".to_owned()),
            line,
            column,
            match_length: 3,
            text: text.to_owned(),
            before_context: Vec::new(),
            after_context: Vec::new(),
        }
    }

    fn source_search_snapshot(matches: Vec<SourceMatchSnapshot>) -> SourceSearchSnapshot {
        SourceSearchSnapshot {
            matches,
            omitted_matches: 0,
            searched_sources: 1,
            searched_contents: 1,
            skipped_sources: 0,
            skipped: Vec::new(),
        }
    }

    #[test]
    fn source_search_renders_context_with_one_path_header_and_aligned_line_numbers() {
        let mut item = source_search_match("webpack:///src/app.ts", 99, 3, "  hit();");
        item.before_context = vec!["function run() {".to_owned()];
        item.after_context = vec!["}".to_owned(), String::new()];

        assert_eq!(
            render_source_search(&source_search_snapshot(vec![item])),
            concat!(
                "webpack:///src/app.ts:99:3\n",
                "     98 | function run() {\n",
                "  >  99 |   hit();\n",
                "    100 | }\n",
                "    101 | \n",
                "\n",
                "1 source(s) searched, 0 skipped\n",
            )
        );
    }

    #[test]
    fn source_search_renders_without_context() {
        let item = source_search_match("file:///src/app.ts", 1, 8, "return hit();");
        assert_eq!(
            render_source_search(&source_search_snapshot(vec![item])),
            concat!(
                "file:///src/app.ts:1:8\n",
                "  > 1 | return hit();\n",
                "\n",
                "1 source(s) searched, 0 skipped\n",
            )
        );
    }

    #[test]
    fn source_search_keeps_distinct_occurrences_and_files_with_overlapping_context() {
        let mut first = source_search_match("first.ts", 1, 1, "hit(hit());");
        first.after_context = vec!["done();".to_owned()];
        let second = SourceMatchSnapshot {
            column: 5,
            ..first.clone()
        };
        let mut third = source_search_match("second.ts", 2, 1, "hit();");
        third.before_context = vec!["start();".to_owned()];
        let snapshot = SourceSearchSnapshot {
            searched_sources: 2,
            searched_contents: 2,
            ..source_search_snapshot(vec![first, second, third])
        };
        assert_eq!(
            render_source_search(&snapshot),
            concat!(
                "first.ts:1:1\n",
                "  > 1 | hit(hit());\n",
                "    2 | done();\n",
                "\n",
                "first.ts:1:5\n",
                "  > 1 | hit(hit());\n",
                "    2 | done();\n",
                "\n",
                "second.ts:2:1\n",
                "    1 | start();\n",
                "  > 2 | hit();\n",
                "\n",
                "2 source(s) searched, 0 skipped\n",
            )
        );
    }

    #[test]
    fn source_search_preserves_omitted_counts_and_source_map_diagnostics() {
        let snapshot = SourceSearchSnapshot {
            omitted_matches: 4,
            skipped_sources: 1,
            skipped: vec![SourceSearchSkip {
                path: "broken.js.map".to_owned(),
                kind: "sourceMap".to_owned(),
                connection_id: Some("connection".to_owned()),
                target_id: Some("target".to_owned()),
                reason: "invalid source map JSON".to_owned(),
            }],
            ..source_search_snapshot(vec![source_search_match("app.ts", 1, 1, "hit();")])
        };
        assert_eq!(
            render_source_search(&snapshot),
            concat!(
                "app.ts:1:1\n",
                "  > 1 | hit();\n",
                "\n",
                "... 4 additional matches omitted; increase --max-results\n",
                "1 source(s) searched, 1 skipped\n",
                "Skipped broken.js.map (sourceMap): invalid source map JSON\n",
            )
        );
        assert_eq!(
            render_source_search(&source_search_snapshot(Vec::new())),
            "1 source(s) searched, 0 skipped\n",
        );
    }

    #[test]
    fn fully_skipped_source_search_is_reported_as_incomplete() {
        let snapshot = SourceSearchSnapshot {
            matches: Vec::new(),
            omitted_matches: 0,
            searched_sources: 0,
            searched_contents: 0,
            skipped_sources: 1,
            skipped: vec![SourceSearchSkip {
                path: "missing.js".to_owned(),
                kind: "runtime".to_owned(),
                connection_id: Some("connection".to_owned()),
                target_id: Some("target".to_owned()),
                reason: "script was collected".to_owned(),
            }],
        };

        assert_eq!(
            render_source_search(&snapshot),
            concat!(
                "Search incomplete: no sources were searched because all 1 candidate source(s) were skipped.\n",
                "0 source(s) searched, 1 skipped\n",
                "Skipped missing.js (runtime): script was collected\n",
            )
        );
        assert_eq!(
            source_search_incomplete_message(&snapshot).as_deref(),
            Some(
                "Search incomplete: no sources were searched because all 1 candidate source(s) were skipped."
            )
        );
        assert_eq!(
            source_search_incomplete_message(&SourceSearchSnapshot {
                searched_sources: 1,
                ..snapshot
            }),
            None
        );
    }

    #[test]
    fn source_graph_renders_a_spanning_forest_with_references() {
        let graph = CompactedSourceGraphSnapshot {
            roots: vec![1, 3],
            nodes: vec![
                CompactedSourceNodeSnapshot {
                    id: 1,
                    prefix: "file:///workspace/out/".into(),
                    source_count: 2,
                    snapshot_count: 2,
                    listed_source_paths: vec![
                        "file:///workspace/out/a.js".into(),
                        "file:///workspace/out/b.js".into(),
                    ],
                    runtime_internal: false,
                },
                CompactedSourceNodeSnapshot {
                    id: 2,
                    prefix: "file:///workspace/src/".into(),
                    source_count: 2,
                    snapshot_count: 2,
                    listed_source_paths: vec![
                        "file:///workspace/src/a.ts".into(),
                        "file:///workspace/src/b.ts".into(),
                    ],
                    runtime_internal: false,
                },
                CompactedSourceNodeSnapshot {
                    id: 3,
                    prefix: "node:internal/modules/".into(),
                    source_count: 1,
                    snapshot_count: 1,
                    listed_source_paths: vec!["node:internal/modules/cjs/loader".into()],
                    runtime_internal: true,
                },
            ],
            edges: vec![
                CompactedSourceEdgeSnapshot {
                    derived: 1,
                    basis: 2,
                    kind: "source map".into(),
                    mapping_count: 2,
                    fan_out: false,
                    suffix_rewrite: Some(SourceSuffixRewriteSnapshot {
                        from: ".js".into(),
                        to: ".ts".into(),
                    }),
                },
                CompactedSourceEdgeSnapshot {
                    derived: 3,
                    basis: 2,
                    kind: "identity".into(),
                    mapping_count: 1,
                    fan_out: false,
                    suffix_rewrite: None,
                },
            ],
        };

        assert_eq!(
            render_compacted_source_graph(&graph),
            "\
#1 file:///workspace/out/  [2 sources]
└─ source map  [2 mappings, .js → .ts] → #2 file:///workspace/src/  [2 sources]

#3 node:internal/modules/  [1 source] [internal]
└─ identity  [1 mapping] → #2 file:///workspace/src/  [2 sources] ↩
"
        );
    }

    #[test]
    fn source_graph_marks_fan_out_subtrees() {
        let graph = CompactedSourceGraphSnapshot {
            roots: vec![1],
            nodes: vec![
                CompactedSourceNodeSnapshot {
                    id: 1,
                    prefix: "https://example.test/bundle.js".into(),
                    source_count: 1,
                    snapshot_count: 1,
                    listed_source_paths: vec!["https://example.test/bundle.js".into()],
                    runtime_internal: false,
                },
                CompactedSourceNodeSnapshot {
                    id: 2,
                    prefix: "source://resolved/~up/src/".into(),
                    source_count: 2,
                    snapshot_count: 2,
                    listed_source_paths: vec![
                        "source://resolved/~up/src/a.ts".into(),
                        "source://resolved/~up/src/b.ts".into(),
                    ],
                    runtime_internal: false,
                },
            ],
            edges: vec![CompactedSourceEdgeSnapshot {
                derived: 1,
                basis: 2,
                kind: "source map".into(),
                mapping_count: 2,
                fan_out: true,
                suffix_rewrite: None,
            }],
        };

        assert_eq!(
            render_compacted_source_graph(&graph),
            "\
#1 https://example.test/bundle.js  [1 source]
└─ source map  [2 mappings, fan-out] → #2 source://resolved/~up/src/*  [2 sources]
"
        );
    }

    #[test]
    fn source_graph_lists_members_of_small_isolated_groups() {
        let graph = CompactedSourceGraphSnapshot {
            roots: vec![1, 2],
            nodes: vec![
                CompactedSourceNodeSnapshot {
                    id: 1,
                    prefix: "https://example.test/node_modules/".into(),
                    source_count: 3,
                    snapshot_count: 3,
                    listed_source_paths: vec!["a.js".into(), "b.js".into(), "c.js".into()],
                    runtime_internal: false,
                },
                CompactedSourceNodeSnapshot {
                    id: 2,
                    prefix: "https://example.test/service.js".into(),
                    source_count: 1,
                    snapshot_count: 5,
                    listed_source_paths: vec!["https://example.test/service.js".into()],
                    runtime_internal: false,
                },
            ],
            edges: Vec::new(),
        };

        assert_eq!(
            render_compacted_source_graph(&graph),
            "\
#1 https://example.test/node_modules/  [3 sources]
├─ source a.js
├─ source b.js
└─ source c.js

#2 https://example.test/service.js  [1 source, 5 snapshots]
"
        );
    }

    #[test]
    fn uncompacted_source_graph_renders_snapshots_and_concrete_projections() {
        let graph = UncompactedSourceGraphSnapshot {
            roots: vec![1, 3],
            nodes: vec![
                UncompactedSourceNodeSnapshot {
                    id: 1,
                    uri: "https://example.test/out/app.js".into(),
                    revision: UncompactedSourceRevisionSnapshot::Content {
                        hash: "1111111111111111".into(),
                    },
                },
                UncompactedSourceNodeSnapshot {
                    id: 2,
                    uri: "file:///workspace/src/app.ts".into(),
                    revision: UncompactedSourceRevisionSnapshot::Content {
                        hash: "2222222222222222".into(),
                    },
                },
                UncompactedSourceNodeSnapshot {
                    id: 3,
                    uri: "node:internal/modules/cjs/loader".into(),
                    revision: UncompactedSourceRevisionSnapshot::Version {
                        namespace: "cdp-script".into(),
                        value: "runtime-1".into(),
                    },
                },
            ],
            edges: vec![
                UncompactedSourceEdgeSnapshot {
                    id: 1,
                    derived: 1,
                    basis: 2,
                    projection: UncompactedProjectionSnapshot::SourceMap {
                        map_hash: "aaaaaaaaaaaaaaaa".into(),
                        source_index: 0,
                    },
                },
                UncompactedSourceEdgeSnapshot {
                    id: 2,
                    derived: 3,
                    basis: 2,
                    projection: UncompactedProjectionSnapshot::IdentityDeclaredByProvider {
                        provider: "node".into(),
                    },
                },
            ],
        };

        assert_eq!(
            render_uncompacted_source_graph(&graph),
            "\
#1 https://example.test/out/app.js  [content:1111111111111111]
└─ projection #1 source map [aaaaaaaaaaaaaaaa, source 0] → #2 file:///workspace/src/app.ts  [content:2222222222222222]

#3 node:internal/modules/cjs/loader  [cdp-script:runtime-1]
└─ projection #2 identity [declared by node] → #2 file:///workspace/src/app.ts  [content:2222222222222222] ↩
"
        );
    }

    #[test]
    fn source_tree_groups_uri_paths_and_reports_multiple_revisions() {
        let revision = |value: &str| UncompactedSourceRevisionSnapshot::Version {
            namespace: "test".into(),
            value: value.into(),
        };
        let snapshot = SourceTreeSnapshot {
            kind: SourceTreeKind::Loaded,
            sources: vec![
                UncompactedSourceNodeSnapshot {
                    id: 1,
                    uri: "https://example.test/src/a.ts".into(),
                    revision: revision("1"),
                },
                UncompactedSourceNodeSnapshot {
                    id: 2,
                    uri: "https://example.test/src/a.ts".into(),
                    revision: revision("2"),
                },
                UncompactedSourceNodeSnapshot {
                    id: 3,
                    uri: "https://example.test/src/nested/b.ts".into(),
                    revision: revision("1"),
                },
                UncompactedSourceNodeSnapshot {
                    id: 4,
                    uri: "source://runtime/anonymous/4".into(),
                    revision: revision("1"),
                },
            ],
        };

        let lines = source_tree_lines(
            &snapshot,
            SourceTreeOutputOptions {
                all: true,
                max_lines: 1,
                trim_width: true,
            },
        );
        assert_eq!(
            lines,
            [
                "├─ https://example.test/src/  [2 sources, 3 snapshots]",
                "│  ├─ a.ts  [2 snapshots]",
                "│  └─ nested/b.ts",
                "└─ source://runtime/anonymous/4",
            ]
        );
    }

    #[test]
    fn source_tree_honors_line_budget_unless_all_is_requested() {
        let snapshot = SourceTreeSnapshot {
            kind: SourceTreeKind::Resolved,
            sources: (0..20)
                .map(|id| UncompactedSourceNodeSnapshot {
                    id,
                    uri: format!("file:///workspace/src/module-{id}/index.ts"),
                    revision: UncompactedSourceRevisionSnapshot::Version {
                        namespace: "test".into(),
                        value: id.to_string(),
                    },
                })
                .collect(),
        };
        let pruned = source_tree_lines(
            &snapshot,
            SourceTreeOutputOptions {
                all: false,
                max_lines: 5,
                trim_width: false,
            },
        );
        let complete = source_tree_lines(
            &snapshot,
            SourceTreeOutputOptions {
                all: true,
                max_lines: 5,
                trim_width: false,
            },
        );

        assert!(pruned.len() <= 5);
        assert!(pruned.iter().any(|line| line.contains("pruned")));
        assert!(complete.len() > 5);
        assert!(complete.iter().all(|line| !line.contains("pruned")));
    }

    #[test]
    fn process_tree_uses_virtual_window_nodes() {
        let tree = ProcessTreeSnapshot {
            root_process_id: 1,
            root_kind: ProcessRootKind::Vscode,
            runtime_metadata_available: true,
            targets: Vec::new(),
            targets_observed: false,
            target_discovery_error: None,
            processes: vec![
                process(1, None, "Code.exe", ProcessRole::VscodeMain, None, None),
                process(
                    2,
                    Some(1),
                    "renderer",
                    ProcessRole::Renderer,
                    Some(3),
                    Some("project"),
                ),
                process(
                    3,
                    Some(1),
                    "extension-host",
                    ProcessRole::ExtensionHost,
                    Some(3),
                    Some("project"),
                ),
                process(
                    4,
                    Some(3),
                    "server",
                    ProcessRole::Node,
                    Some(3),
                    Some("project"),
                ),
                process(5, Some(1), "agent-host", ProcessRole::AgentHost, None, None),
            ],
        };

        assert_eq!(
            process_tree_lines(&tree, ProcessTreeOutputOptions::default()),
            vec![
                "└─ p:1  Code.exe  [vscode-main]",
                "   ├─ w:1/3  window  project",
                "   │  ├─ p:2  renderer  [renderer]",
                "   │  └─ p:3  extension-host  [extension-host]",
                "   │     └─ p:4  server  [node]",
                "   └─ p:5  agent-host  [agent-host]",
            ]
        );
        let filtered = process_tree_lines(
            &tree,
            ProcessTreeOutputOptions {
                filter: Some("window 3"),
                ..ProcessTreeOutputOptions::default()
            },
        );
        assert!(filtered.iter().any(|line| line.contains("w:1/3")));
        assert!(filtered.iter().all(|line| !line.contains("agent-host")));

        let json = process_trees_json(
            std::slice::from_ref(&tree),
            ProcessTreeOutputOptions {
                command_line: false,
                filter: Some("window 3"),
                ..ProcessTreeOutputOptions::default()
            },
        )
        .unwrap();
        let processes = json[0]["processes"].as_array().unwrap();
        assert_eq!(processes.len(), 4);
        assert!(
            processes
                .iter()
                .all(|process| process.get("commandLine").is_none())
        );
        assert!(
            processes
                .iter()
                .all(|process| process["processId"] != serde_json::json!(5))
        );
    }

    #[test]
    fn process_tree_nests_discovered_targets_under_their_os_process() {
        let target = |process_id, target_id: &str, parent_id: Option<&str>, target_type: &str| {
            ProcessTargetSnapshot {
                process_id: Some(process_id),
                target: TargetSnapshot {
                    target_id: target_id.to_owned(),
                    target_type: target_type.to_owned(),
                    title: target_id.to_owned(),
                    url: String::new(),
                    attached: false,
                    parent_id: parent_id.map(str::to_owned),
                    opener_id: None,
                    browser_context_id: None,
                    subtype: None,
                },
            }
        };
        let tree = ProcessTreeSnapshot {
            root_process_id: 1,
            root_kind: ProcessRootKind::Electron,
            runtime_metadata_available: false,
            processes: vec![
                process(
                    1,
                    None,
                    "electron.exe",
                    ProcessRole::ElectronMain,
                    None,
                    None,
                ),
                process(2, Some(1), "renderer", ProcessRole::Renderer, None, None),
            ],
            targets: vec![
                target(1, "$node-root", None, "node"),
                target(2, "renderer-3", Some("$node-root"), "page"),
                target(2, "renderer-3/target/iframe", Some("renderer-3"), "iframe"),
            ],
            targets_observed: true,
            target_discovery_error: None,
        };

        assert_eq!(
            process_tree_lines(&tree, ProcessTreeOutputOptions::default()),
            vec![
                "└─ p:1  electron.exe  [electron-main]",
                "   └─ p:2  renderer  [renderer]",
                "      └─ renderer-3  [page]  \"renderer-3\"  ",
                "         └─ renderer-3/target/iframe  [iframe]  \"renderer-3/target/iframe\"  ",
            ]
        );
    }

    #[test]
    fn target_tree_uses_round_trippable_qualified_selectors() {
        let entry = |target_id: &str| TargetListEntry {
            connection_id: "tree".to_owned(),
            connection_generation: 1,
            selected: false,
            parent_target_id: None,
            target: TargetSnapshot {
                target_id: target_id.to_owned(),
                target_type: "page".to_owned(),
                title: String::new(),
                url: String::new(),
                attached: false,
                parent_id: None,
                opener_id: None,
                browser_context_id: None,
                subtype: None,
            },
        };
        let root = entry("$node-root:tree");
        let browser = entry("cdp-browser-1");
        let renderer = entry("renderer-1");
        let renderer_iframe = entry("renderer-1/target/iframe");
        let page = entry("cdp-browser-1/target/page");
        let page_iframe = entry("cdp-browser-1/target/iframe");

        assert_eq!(target_tree_selector(&root), "tree/$node-root:tree@1");
        assert_eq!(
            target_tree_selector(&browser),
            "tree/cdp-browser-1@1"
        );
        assert_eq!(
            target_tree_selector(&renderer_iframe),
            "tree/renderer-1/target/iframe@1"
        );
        assert_eq!(
            target_tree_selector(&page_iframe),
            "tree/cdp-browser-1/target/iframe@1"
        );
        for entry in [
            &root, &browser, &renderer, &renderer_iframe, &page, &page_iframe,
        ] {
            assert_eq!(
                dbgjs::target_selector::match_target_selector(
                    &entry.target,
                    &entry.connection_id,
                    entry.connection_generation,
                    &target_tree_selector(entry),
                ),
                Some(dbgjs::target_selector::TargetSelectorMatch::Qualified),
            );
        }
    }

    #[test]
    fn process_tree_renders_multiplexed_agent_sessions_as_virtual_children() {
        let mut copilot = process(3, Some(2), "copilot", ProcessRole::Copilot, None, None);
        copilot.agent_sessions = vec![AgentSessionSnapshot {
            internal_id: "internal-1".to_owned(),
            chat_uri: Some("copilotcli:/chat-1".to_owned()),
            title: Some("Add extension launch config".to_owned()),
            working_directories: vec!["file:///d%3A/dev/dbgjs".to_owned()],
            disconnected: Some(false),
        }];
        let tree = ProcessTreeSnapshot {
            root_process_id: 1,
            root_kind: ProcessRootKind::Vscode,
            runtime_metadata_available: true,
            targets: Vec::new(),
            targets_observed: false,
            target_discovery_error: None,
            processes: vec![
                process(1, None, "Code.exe", ProcessRole::VscodeMain, None, None),
                process(2, Some(1), "agent-host", ProcessRole::AgentHost, None, None),
                copilot,
                process(4, Some(3), "cmd.exe", ProcessRole::Other, None, None),
            ],
        };
        assert_eq!(
            process_tree_lines(&tree, ProcessTreeOutputOptions::default()),
            vec![
                "└─ p:1  Code.exe  [vscode-main]",
                "   └─ p:2  agent-host  [agent-host]",
                "      └─ p:3  copilot  [copilot]",
                "         ├─ session Add extension launch config  [internal-1]",
                "         └─ p:4  cmd.exe  [other]",
            ]
        );
    }

    #[test]
    fn non_attachable_processes_dim_the_terminal_foreground_without_changing_the_background() {
        let styled = style_process_label("native process".to_owned(), false, true);
        assert_eq!(styled, "\u{1b}[2mnative process\u{1b}[0m");
        assert!(!styled.contains("[4"));
    }

    #[test]
    fn agent_sessions_use_blue_foreground_without_changing_the_background() {
        let styled = style_session_label("session Heap analysis".to_owned(), true);
        assert_eq!(styled, "\u{1b}[34msession Heap analysis\u{1b}[0m");
        assert!(!styled.contains("[4"));
    }

    #[test]
    fn heap_paths_group_v8_root_infrastructure_and_label_reference_counts() {
        let path = HeapPathSnapshot {
            capture_id: ".".to_owned(),
            from: ".#100".to_owned(),
            to: ".#1".to_owned(),
            cost: 4,
            nodes: vec![
                heap_node(".#100", "object", "Object", 1, 2),
                heap_node(".#90", "object", "Window [JSGlobalObject]", 3, 735),
                heap_node(".#20", "native", "system / NativeContext", 6955, 269),
                heap_node(".#3", "synthetic", "(GC roots)", 1, 30),
                heap_node(".#1", "synthetic", "", 0, 1),
            ],
            steps: vec![
                heap_step(".#100", ".#90", "owner"),
                heap_step(".#90", ".#20", "global_object"),
                heap_step(".#20", ".#3", "context"),
                heap_step(".#3", ".#1", "1"),
            ],
        };

        assert_eq!(
            heap_path_lines(&path),
            vec![
                "Heap path in '.' (4 edges):",
                ".#100  type:object, value:\"Object\", shallow:0 B, in:1, out:2",
                "  <- internal \"owner\"",
                ".#90  type:object, value:\"Window [JSGlobalObject]\", shallow:0 B, in:3, out:735",
                "  <- internal \"global_object\"",
                "┌─ V8 runtime roots (implementation details)",
                "│ .#20  type:native, value:\"system / NativeContext\", shallow:0 B, in:6955, out:269",
                "│   <- internal \"context\"",
                "│ .#3  type:synthetic, value:\"(GC roots)\", shallow:0 B, in:1, out:30",
                "│   <- internal \"1\"",
                "│ .#1  type:synthetic, value:\"\", shallow:0 B, in:0, out:1",
                "└─",
            ]
        );
    }

    #[test]
    fn heap_show_renders_a_bounded_property_reference_table() {
        let snapshot = HeapReferencesSnapshot {
            capture_id: ".".to_owned(),
            node: heap_node(".#10", "object", "Object", 1, 4),
            direction: HeapReferenceDirection::Outgoing,
            edge_policy: HeapEdgePolicy::All,
            references: vec![
                HeapReferenceSnapshot {
                    edge_index: 1,
                    edge_type: "property".to_owned(),
                    name: Some("title".to_owned()),
                    name_or_index: 0,
                    source: ".#10".to_owned(),
                    target: ".#11".to_owned(),
                    source_preview: None,
                    target_preview: Some("\"hello\\nworld\"".to_owned()),
                    source_locations: Default::default(),
                    target_locations: Default::default(),
                },
                HeapReferenceSnapshot {
                    edge_index: 2,
                    edge_type: "element".to_owned(),
                    name: None,
                    name_or_index: 3,
                    source: ".#10".to_owned(),
                    target: ".#12".to_owned(),
                    source_preview: None,
                    target_preview: Some("Array [0: 42]".to_owned()),
                    source_locations: Default::default(),
                    target_locations: Default::default(),
                },
            ],
            omitted_reference_count: 2,
        };

        assert_eq!(
            heap_show_lines(&snapshot),
            vec![
                ".#10  type:object, value:\"Object\", shallow:0 B, in:1, out:4",
                "Outgoing properties/references (2 of 4):",
                "  PROPERTY/EDGE                 REFERENCE",
                "  property \"title\"             .#11  \"hello\\nworld\"",
                "  element [3]                  .#12  Array [0: 42]",
                "  ... 2 references omitted; use --all to expand",
            ]
        );
    }

    #[test]
    fn heap_previews_preserve_endpoint_references() {
        let mut node = heap_node("capture#10", "object", "Widget", 1, 1);
        node.preview = Some("Widget {\"title\": \"hello\"}".to_owned());
        assert_eq!(
            heap_node_line(&node),
            "capture#10  type:object, value:Widget {\"title\": \"hello\"}, shallow:0 B, in:1, out:1"
        );
        let reference = HeapReferenceSnapshot {
            edge_index: 1,
            edge_type: "property".to_owned(),
            name: Some("title".to_owned()),
            name_or_index: 0,
            source: "capture#10".to_owned(),
            target: "capture#11".to_owned(),
            source_preview: node.preview,
            target_preview: Some("\"hello\"".to_owned()),
            source_locations: Default::default(),
            target_locations: Default::default(),
        };
        assert_eq!(
            heap_reference_line(&reference),
            "  capture#10  Widget {\"title\": \"hello\"} --property \"title\"--> capture#11  \"hello\""
        );
    }

    #[test]
    fn heap_previews_default_when_deserializing_older_snapshots() {
        let mut json = serde_json::to_value(heap_node(".#10", "object", "Object", 0, 0)).unwrap();
        json.as_object_mut().unwrap().remove("preview");
        let node: HeapNodeSnapshot = serde_json::from_value(json).unwrap();
        assert_eq!(node.preview, None);
        let reference: HeapReferenceSnapshot = serde_json::from_value(serde_json::json!({
            "edgeIndex": 1, "edgeType": "property", "name": "title", "nameOrIndex": 0,
            "source": ".#10", "target": ".#11"
        })).unwrap();
        assert_eq!(reference.source_preview, None);
        assert_eq!(reference.target_preview, None);
        assert_eq!(heap_reference_line(&reference), "  .#10 --property \"title\"--> .#11");
    }

    #[test]
    fn heap_preview_does_not_replace_explicit_selected_string_value() {
        let mut node = heap_node(".#10", "string", "abcdefghijklmnopqrstuvw", 0, 0);
        node.string_value = Some(node.name.clone());
        node.preview = Some("\"abcdefghijklmnopqrst...\"".to_owned());
        assert!(heap_node_line(&node).contains("value:\"abcdefghijklmnopqrstuvw\""));
    }

    fn heap_node(
        reference: &str,
        node_type: &str,
        name: &str,
        incoming_reference_count: u64,
        outgoing_reference_count: u64,
    ) -> HeapNodeSnapshot {
        HeapNodeSnapshot {
            reference: reference.to_owned(),
            node_index: 0,
            node_type: node_type.to_owned(),
            heap_object_id: reference.trim_start_matches(".#").to_owned(),
            name: name.to_owned(),
            string_value: None,
            string_truncated: false,
            preview: None,
            shallow_size: 0,
            outgoing_reference_count,
            incoming_reference_count,
            locations: Vec::new(),
            source: Default::default(),
            immediate_dominator: None,
            retained_size: None,
        }
    }

    fn heap_step(from: &str, to: &str, name: &str) -> HeapPathStepSnapshot {
        HeapPathStepSnapshot {
            from: from.to_owned(),
            to: to.to_owned(),
            edge_index: 0,
            edge_type: "internal".to_owned(),
            name: Some(name.to_owned()),
            name_or_index: 0,
            direction: HeapTraversalDirection::Incoming,
        }
    }

    fn process(
        process_id: u32,
        parent_process_id: Option<u32>,
        name: &str,
        role: ProcessRole,
        window_id: Option<u32>,
        window_title: Option<&str>,
    ) -> ProcessSnapshot {
        ProcessSnapshot {
            process_id,
            parent_process_id,
            attachable: true,
            debug_target_id: Some(format!("process-{process_id}-test")),
            name: name.to_owned(),
            command_line: String::new(),
            creation_date: String::new(),
            role,
            display_name: None,
            window_id,
            window_title: window_title.map(str::to_owned),
            cpu_percent: None,
            memory_bytes: None,
            agent_sessions: Vec::new(),
        }
    }

    fn heap_class(name: &str, count: u64) -> HeapClassSnapshotEntry {
        let retained = count.min(20);
        HeapClassSnapshotEntry {
            name: name.to_owned(),
            script_id: "1".into(),
            provenance: Default::default(),
            source_url: "src/model.ts".to_owned(),
            location: SourceLocation {
                source_url: "src/model.ts".to_owned(),
                line: 1,
                column: 1,
            },
            generated_name: name.to_owned(),
            instance_count: count,
            shallow_size: count * 8,
            instances: (1..=retained)
                .map(|index| HeapInstanceSnapshot {
                    alias: format!("{name}@{index}"),
                    heap_object_id: index.to_string(),
                    shallow_size: 8,
                })
                .collect(),
            omitted_instance_count: count.saturating_sub(retained),
        }
    }

    fn heap_snapshot(classes: Vec<HeapClassSnapshotEntry>) -> HeapClassSnapshot {
        HeapClassSnapshot {
            capture_id: ".".to_owned(),
            total_instances: classes.iter().map(|class| class.instance_count).sum(),
            total_shallow_size: classes.iter().map(|class| class.shallow_size).sum(),
            classes,
            analysis: HeapClassAnalysisSnapshot {
                snapshot_timing: Some(HeapSnapshotTiming {
                    taking_duration_micros: 2_000_000,
                    retrieving_duration_micros: 3_000_000,
                }),
                parse_duration_micros: 0,
                projection_duration_micros: 0,
                source_map_hydration_duration_micros: 0,
                constructor_group_count: 0,
                used_cached_groups: false,
                mapping_status: Default::default(),
                script_mappings: Vec::new(),
            },
        }
    }

    #[test]
    fn heap_class_labels_include_owning_frame_and_context() {
        let mut class = heap_class("Original", 1);
        class.provenance.frame_id = Some("webview-child".into());
        class.provenance.execution_context_id = Some(23);
        assert_eq!(super::heap_class_label(&class), "Original [frame:webview-child, context:23]");
        let lines = render_heap_classes_human(&heap_snapshot(vec![class]), HeapClassOutputOptions {
            all: true, max_lines: 300, instances: false, sort_by_instances: true, trim_width: false,
        });
        assert!(lines.iter().any(|line| line.contains("frame:webview-child")));
        assert!(lines.iter().any(|line| line.contains("mapping not attempted")));
        assert!(!lines.iter().any(|line| line.contains("source-map hydration 0.000s")));
    }

    #[test]
    fn heap_classes_inline_small_instance_sets() {
        let lines = render_heap_classes_human(
            &heap_snapshot(vec![heap_class("PieceTreeModel", 2)]),
            HeapClassOutputOptions {
                all: false,
                max_lines: 300,
                instances: false,
                sort_by_instances: false,
                trim_width: true,
            },
        );
        assert!(lines.iter().any(|line| {
            line.contains("PieceTreeModel@1 id 1") && line.contains("PieceTreeModel@2 id 2")
        }));
        assert!(
            lines[1].contains("Total 5.000s: snapshot 5.000s (taking 2.000s, retrieving 3.000s)")
        );
    }

    #[test]
    fn heap_classes_bound_output_and_report_pruned_classes() {
        let classes = (0..40)
            .map(|index| heap_class(&format!("Class{index}"), 100 - index))
            .collect();
        let lines = render_heap_classes_human(
            &heap_snapshot(classes),
            HeapClassOutputOptions {
                all: false,
                max_lines: 8,
                instances: true,
                sort_by_instances: false,
                trim_width: true,
            },
        );
        assert!(lines.len() <= 8, "{lines:#?}");
        assert!(
            lines.iter().any(|line| line.contains("classes omitted")),
            "{lines:#?}"
        );
    }

    #[test]
    fn heap_classes_apply_max_lines_to_headers() {
        let lines = render_heap_classes_human(
            &heap_snapshot(vec![heap_class("PieceTreeModel", 2)]),
            HeapClassOutputOptions {
                all: false,
                max_lines: 1,
                instances: false,
                sort_by_instances: false,
                trim_width: true,
            },
        );
        assert_eq!(lines.len(), 1);
    }

    #[test]
    fn heap_classes_rank_by_instances_across_source_files() {
        let mut least = heap_class("Least", 2);
        least.source_url = "src/z.ts".to_owned();
        least.location.source_url = least.source_url.clone();
        let mut most = heap_class("Most", 20);
        most.source_url = "src/a.ts".to_owned();
        most.location.source_url = most.source_url.clone();
        let lines = render_heap_classes_human(
            &heap_snapshot(vec![least, most]),
            HeapClassOutputOptions {
                all: false,
                max_lines: 10,
                instances: false,
                sort_by_instances: true,
                trim_width: true,
            },
        );
        assert!(lines[2].contains("1. Most  20 instances"), "{lines:#?}");
        assert!(lines[3].contains("2. Least  2 instances"), "{lines:#?}");
    }

    #[test]
    fn identifies_short_mangled_names_without_flagging_readable_symbols() {
        for name in ["Bbi", "OXe", "cDe", "j0e", "fS"] {
            assert!(looks_minified_identifier(name), "{name}");
        }

        for name in [
            "next",
            "rbTreeBase",
            "get isVisible",
            "CursorsController.type",
        ] {
            assert!(!looks_minified_identifier(name), "{name}");
        }
    }

    #[test]
    fn default_tree_collapses_a_file_with_one_class() {
        let metrics = CoverageMetrics {
            hit_lines: 12,
            run_lines: 20,
        };
        let entries = vec![
            CoverageEntry {
                path: "workingCopyBackupTracker.ts".to_owned(),
                function: "WorkingCopyBackupTracker.backup".to_owned(),
                line_counts: BTreeMap::from([(1, 1)]),
                metrics,
                generated_location: None,
            },
            CoverageEntry {
                path: "workingCopyBackupTracker.ts".to_owned(),
                function: "WorkingCopyBackupTracker.schedule".to_owned(),
                line_counts: BTreeMap::from([(2, 1)]),
                metrics,
                generated_location: None,
            },
        ];
        let mut tree = BoundedTree::default();
        tree.insert(["workingCopyBackupTracker.ts".to_owned()], metrics, entries);
        assert!(
            tree.render(&CoverageTreeStyle, false, 10)[0]
                .contains("workingCopyBackupTracker.ts/WorkingCopyBackupTracker")
        );
    }

    #[test]
    fn nested_zero_count_ranges_remove_parent_lines() {
        let location = |line| SourceLocation {
            source_url: "src/example.ts".to_owned(),
            line,
            column: 1,
        };
        let snapshot = CoverageSnapshot {
            capture_id: None,
            timestamp_micros: 0,
            analysis: None,
            sources: vec![CoverageSourceSnapshot {
                script_id: "1".to_owned(),
                generated_url: "bundle.js".to_owned(),
                associated_authored_source: None,
                functions: vec![CoverageFunctionSnapshot {
                    name: "example".to_owned(),
                    block_coverage: true,
                    root_start_offset: 0,
                    root_end_offset: 100,
                    ranges: vec![
                        CoverageRangeSnapshot {
                            start_offset: 0,
                            end_offset: 100,
                            count: 1,
                            authored_start: Some(location(1)),
                            authored_end: Some(location(100)),
                        },
                        CoverageRangeSnapshot {
                            start_offset: 49,
                            end_offset: 60,
                            count: 0,
                            authored_start: Some(location(50)),
                            authored_end: Some(location(60)),
                        },
                    ],
                    effective_ranges: vec![
                        CoverageRangeSnapshot {
                            start_offset: 0,
                            end_offset: 49,
                            count: 1,
                            authored_start: Some(location(1)),
                            authored_end: Some(location(49)),
                        },
                        CoverageRangeSnapshot {
                            start_offset: 60,
                            end_offset: 100,
                            count: 1,
                            authored_start: Some(location(61)),
                            authored_end: Some(location(100)),
                        },
                    ],
                    authored_location: Some(location(1)),
                    breadcrumb: Some("example".to_owned()),
                    generated_location: None,
                }],
            }],
        };
        let entries = coverage_entries(&snapshot);
        assert_eq!(entries[0].metrics.hit_lines, 89);
        assert_eq!(entries[0].metrics.run_lines, 89);
        assert!(!entries[0].line_counts.contains_key(&50));
        assert!(entries[0].line_counts.contains_key(&49));
        assert!(entries[0].line_counts.contains_key(&61));
    }

    #[test]
    fn aggregation_preserves_noncontiguous_line_sets() {
        let entries = aggregate_coverage_entries(vec![CoverageEntry {
            path: "src/example.ts".to_owned(),
            function: "example".to_owned(),
            line_counts: BTreeMap::from([(1, 3), (100, 3)]),
            metrics: CoverageMetrics {
                hit_lines: 2,
                run_lines: 6,
            },
            generated_location: None,
        }]);
        assert_eq!(effective_file_metrics(&entries).hit_lines, 2);
        assert_eq!(effective_file_metrics(&entries).run_lines, 6);
        assert_eq!(entries[0].metrics.hit_lines, 2);
        assert_eq!(entries[0].metrics.run_lines, 6);
    }

    #[test]
    fn run_lines_weight_hit_lines_by_effective_count() {
        let location = |line| SourceLocation {
            source_url: "src/example.ts".to_owned(),
            line,
            column: 1,
        };
        let snapshot = CoverageSnapshot {
            capture_id: None,
            timestamp_micros: 0,
            analysis: None,
            sources: vec![CoverageSourceSnapshot {
                script_id: "1".to_owned(),
                generated_url: "bundle.js".to_owned(),
                associated_authored_source: None,
                functions: vec![CoverageFunctionSnapshot {
                    name: "example".to_owned(),
                    block_coverage: true,
                    root_start_offset: 0,
                    root_end_offset: 10,
                    ranges: Vec::new(),
                    effective_ranges: vec![CoverageRangeSnapshot {
                        start_offset: 0,
                        end_offset: 10,
                        count: 3,
                        authored_start: Some(location(1)),
                        authored_end: Some(location(10)),
                    }],
                    authored_location: Some(location(1)),
                    breadcrumb: Some("Example.run".to_owned()),
                    generated_location: None,
                }],
            }],
        };
        let entries = coverage_entries(&snapshot);
        assert_eq!(entries[0].metrics.hit_lines, 10);
        assert_eq!(entries[0].metrics.run_lines, 30);
    }

    #[test]
    fn log_empty_reports_capture_status_not_absence_of_errors() {
        use dbgjs::service_api::{LogCaptureSnapshot, LogCaptureStatus, TargetLogSnapshot};
        let mut snapshot = TargetLogSnapshot {
            context_id: "context".into(),
            connection_id: "browser".into(),
            target_id: "renderer/target/frame".into(),
            connection_generation: 2,
            messages: vec![],
            capture: LogCaptureSnapshot {
                status: LogCaptureStatus::Active,
                capture_id: Some("capture-1".into()),
                session_id: Some("session-1".into()),
                started_at_unix_ms: Some(1234),
                collected_events: vec!["Runtime.consoleAPICalled".into()],
                evicted_count: Some(0),
                dropped_count: None,
            },
        };
        let text = super::log_coverage_human(&snapshot, true, 0);
        assert!(text.contains("Log capture: active"));
        assert!(text.contains("Started (Unix ms): 1234"));
        assert!(text.contains("No captured entries to display; this does not mean no errors occurred."));
        assert!(text.contains("evicted: 0; dropped before retention: unknown"));
        assert!(text.contains("Browser diagnostics (Log.entryAdded)"));
        assert!(text.contains("network failures are not collected"));
        let json = serde_json::to_value(&snapshot).unwrap();
        assert_eq!(json["capture"]["status"], "active");
        assert!(json["capture"]["droppedCount"].is_null());
        assert_eq!(page_logs(&[], 0, 20), (0, vec![]));
        assert_eq!(page_logs(&[], 5, 20), (0, vec![]));
        let text = super::log_coverage_human(&snapshot, true, 5);
        assert!(text.contains("after the cursor"));
        snapshot.capture = LogCaptureSnapshot {
            status: LogCaptureStatus::Inactive,
            ..Default::default()
        };
        let text = super::log_coverage_human(&snapshot, true, 0);
        assert!(text.contains("inactive (target is not attached)"));
        assert!(text.contains("Started (Unix ms): unknown"));
        assert!(text.contains("evicted: unknown"));
    }

    #[test]
    fn log_paging_counts_entries_evicted_before_the_retained_window() {
        let logs = (50..=149)
            .map(|index| ConsoleMessageSnapshot {
                index,
                values: vec![index.to_string()],
                params: None,
            })
            .collect::<Vec<_>>();
        let (skipped, displayed) = page_logs(&logs, 1, 20);
        assert_eq!(skipped, 128);
        assert_eq!(displayed.first().unwrap().index, 130);
        assert_eq!(displayed.last().unwrap().index, 149);
        assert_eq!(page_logs(&logs, 149, 20), (0, vec![]));
        assert_eq!(page_logs(&logs, 200, 20), (0, vec![]));
        let (skipped, displayed) = page_logs(&logs, 140, 20);
        assert_eq!(skipped, 0);
        assert_eq!(displayed.len(), 9);
    }
}
