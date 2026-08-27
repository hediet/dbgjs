use cdp_client::service_api::{
    AgentSessionSnapshot, BreakpointStatus, CompactedSourceEdgeSnapshot,
    CompactedSourceGraphSnapshot, CompactedSourceNodeSnapshot, ConnectionConfiguration,
    ConnectionStatus, ConsoleMessageSnapshot, ContextSnapshot, ContextSummary, CoverageSnapshot,
    CpuProfileFunctionSnapshot, CpuProfileSnapshot, EvaluationSnapshot, FrameProjectionSnapshot,
    HeapAggregateSnapshot, HeapCaptureResult, HeapClassSnapshot, HeapClassSnapshotEntry,
    HeapDiffSnapshot, HeapDominatorSnapshot, HeapNodeSelectionSnapshot, HeapNodeSnapshot,
    HeapPathSnapshot, HeapReferencesSnapshot, HeapSnapshotProgress, HeapSnapshotResult,
    ObservationResult, PlaywrightChannel, ProcessRole, ProcessSnapshot, ProcessTreeSnapshot,
    ServiceInfo, SourceContentSnapshot, SourceExcerpt, SourceGraphViewSnapshot, SourceLocation,
    SourceMappingSnapshot, SourceSearchSnapshot, SourceSnapshotInfo, TargetBreakpointStatus,
    TargetDebuggerPhase, TargetDebuggerSnapshot, UncompactedProjectionSnapshot,
    UncompactedSourceEdgeSnapshot, UncompactedSourceGraphSnapshot, UncompactedSourceNodeSnapshot,
    UncompactedSourceRevisionSnapshot,
};
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
    pub command_line: bool,
    pub stats: bool,
    pub filter: Option<&'a str>,
    pub trim_width: bool,
}

impl Default for ProcessTreeOutputOptions<'_> {
    fn default() -> Self {
        Self {
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

impl OutputFormat {
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
                    } else if matches!(breakpoint.status, TargetBreakpointStatus::Pending) {
                        println!(
                            "Breakpoint {} is still pending: no loaded script resolved '{}'.",
                            breakpoint.id, breakpoint.source_url
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
    ) -> Result<(), serde_json::Error> {
        let filtered = options.path.map(|path| filter_coverage_path(value, path));
        let value = filtered.as_ref().unwrap_or(value);
        match self {
            Self::Human => print_coverage_human(value, options),
            Self::Json => println!("{}", serde_json::to_string_pretty(value)?),
        }
        Ok(())
    }

    pub fn print_logs(
        &self,
        logs: &[ConsoleMessageSnapshot],
        after: u64,
        limit: usize,
    ) -> Result<u64, serde_json::Error> {
        let (skipped, displayed) = page_logs(logs, after, limit);
        match self {
            Self::Human => {
                if skipped > 0 {
                    println!("[...skipped {skipped} entries...]");
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
                    "messages": displayed,
                }))?
            ),
        }
        Ok(logs
            .last()
            .map_or(after, |message| message.index.max(after)))
    }

    pub fn print_coverage_stopped(&self) -> Result<(), serde_json::Error> {
        match self {
            Self::Human => println!("Coverage recording stopped. Captured ."),
            Self::Json => println!(
                "{}",
                serde_json::to_string_pretty(&serde_json::json!({ "captureId": "." }))?
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

impl HumanOutput for SourceSearchSnapshot {
    fn print_human(&self) {
        for item in &self.matches {
            let first_context_line = item.line.saturating_sub(item.before_context.len() as u32);
            for (index, line) in item.before_context.iter().enumerate() {
                println!(
                    "{}-{}-{}",
                    item.path,
                    first_context_line + index as u32,
                    line
                );
            }
            println!("{}:{}:{}:{}", item.path, item.line, item.column, item.text);
            for (index, line) in item.after_context.iter().enumerate() {
                println!("{}-{}-{}", item.path, item.line + index as u32 + 1, line);
            }
            if !item.before_context.is_empty() || !item.after_context.is_empty() {
                println!("--");
            }
        }
        if self.omitted_matches > 0 {
            println!(
                "... {} additional matches omitted; increase --max-results",
                self.omitted_matches
            );
        }
        println!(
            "{} source(s) searched, {} skipped",
            self.searched_sources, self.skipped_sources
        );
    }
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
    }
}

impl HumanOutput for HeapReferencesSnapshot {
    fn print_human(&self) {
        println!("{}", heap_node_line(&self.node));
        for reference in &self.references {
            let label = reference
                .name
                .as_deref()
                .map(|name| format!(" {}", escaped_heap_text(name, false)))
                .unwrap_or_else(|| format!(" [{}]", reference.name_or_index));
            println!(
                "  {} --{}{}--> {}",
                reference.source, reference.edge_type, label, reference.target
            );
        }
        if self.omitted_reference_count > 0 {
            println!("  ... {} references omitted", self.omitted_reference_count);
        }
    }
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

fn heap_path_step_line(step: &cdp_client::service_api::HeapPathStepSnapshot) -> String {
    let direction = match step.direction {
        cdp_client::service_api::HeapTraversalDirection::Outgoing => "->",
        cdp_client::service_api::HeapTraversalDirection::Incoming => "<-",
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
    }
}

fn heap_node_line(node: &HeapNodeSnapshot) -> String {
    let value = node
        .string_value
        .as_deref()
        .map(|value| escaped_heap_text(value, node.string_truncated))
        .unwrap_or_else(|| escaped_heap_text(&node.name, node.string_truncated));
    let retained = node
        .retained_size
        .map(|size| format!(", retained:{}", compact_bytes(size)))
        .unwrap_or_default();
    format!(
        "{}  type:{}, value:{}, shallow:{}{}, in:{}, out:{}",
        node.reference,
        node.node_type,
        value,
        compact_bytes(node.shallow_size),
        retained,
        node.incoming_reference_count,
        node.outgoing_reference_count,
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

impl HumanOutput for Vec<ProcessTreeSnapshot> {
    fn print_human(&self) {
        print_process_trees_human(self, ProcessTreeOutputOptions::default());
    }
}

fn print_process_trees_human(trees: &[ProcessTreeSnapshot], options: ProcessTreeOutputOptions<'_>) {
    if trees.is_empty() {
        println!("No running VS Code process trees.");
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
            "No VS Code process tree paths matched {}.",
            options.filter.unwrap_or_default()
        );
        return;
    }
    for (tree_index, (tree, lines)) in rendered.into_iter().enumerate() {
        if tree_index != 0 {
            println!();
        }
        println!(
            "VS Code process tree {}  ({} attachable targets{})",
            tree.root_process_id,
            tree.processes
                .iter()
                .filter(|process| process.attachable)
                .count(),
            if tree.runtime_metadata_available {
                "; window metadata available"
            } else {
                ""
            }
        );
        for line in lines {
            println!("{line}");
        }
    }
}

#[derive(Clone)]
enum ProcessTreeLeaf<'a> {
    Process(&'a ProcessSnapshot),
    Window { id: u32, title: Option<&'a str> },
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
                        command_line: self.command_line,
                        stats: self.stats,
                        filter: None,
                        trim_width: true,
                    },
                );
                style_process_label(label, process.attachable, self.colorize)
            }
            Some(ProcessTreeLeaf::Window { id, title }) => format!(
                "window {id}{}",
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
    Some(process_render_node(root, root.window_id, &children))
}

fn process_render_node<'a>(
    process: &'a ProcessSnapshot,
    active_window: Option<u32>,
    children: &BTreeMap<u32, Vec<&'a ProcessSnapshot>>,
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
                        id: window_id,
                        title,
                    },
                    children: process_children
                        .iter()
                        .filter(|candidate| candidate.window_id == Some(window_id))
                        .map(|window_child| {
                            process_render_node(window_child, Some(window_id), children)
                        })
                        .collect(),
                });
            }
            Some(_) => {}
            None => rendered_children.push(process_render_node(child, active_window, children)),
        }
    }
    ProcessRenderNode {
        path_segment: process_path_segment(process),
        leaf: ProcessTreeLeaf::Process(process),
        children: rendered_children,
    }
}

fn process_path_segment(process: &ProcessSnapshot) -> String {
    let label = process.display_name.as_deref().unwrap_or(&process.name);
    format!("{} {label}", process.process_id)
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
                for process in processes {
                    if let Some(process) = process.as_object_mut() {
                        process.remove("commandLine");
                    }
                }
            }
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
        "{}  {}  [{}]{}{}{}",
        process.process_id,
        label,
        process_role(&process.role),
        process
            .debug_target_id
            .as_deref()
            .map(|target_id| format!("  target {target_id}"))
            .unwrap_or_default(),
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
            }

            fn breakpoint_status(status: &BreakpointStatus) -> String {
                match status {
                    BreakpointStatus::Unconfirmed => "unconfirmed".into(),
                    BreakpointStatus::Disabled => "disabled".into(),
                    BreakpointStatus::Pending => "pending".into(),
                    BreakpointStatus::PartiallyBound { application_count } => {
                        format!("partially-bound:{application_count}")
                    }
                    BreakpointStatus::Bound { application_count } => {
                        format!("bound:{application_count}")
                    }
                    BreakpointStatus::Failed { message } => format!("failed:{message}"),
                }
            }
        }
    }
}

fn process_role(role: &ProcessRole) -> &'static str {
    match role {
        ProcessRole::VscodeMain => "vscode-main",
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

impl HumanOutput for TargetDebuggerSnapshot {
    fn print_human(&self) {
        print_target_human(self, &self.target_id);
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
    if let Some(reused) = snapshot.attachment_reused {
        println!(
            "  Attachment: {}",
            if reused {
                "reused existing debugger session"
            } else {
                "created new debugger session"
            }
        );
    }

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
            if matches!(breakpoint.status, TargetBreakpointStatus::Pending) {
                println!(
                    "      waiting for a loaded script that resolves '{}'",
                    breakpoint.source_url
                );
            }
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

impl HumanOutput for CoverageSnapshot {
    fn print_human(&self) {
        print_coverage_human(
            self,
            CoverageOutputOptions {
                path: None,
                all: false,
                max_lines: 300,
                trim_width: true,
            },
        );
    }
}

fn print_coverage_human(snapshot: &CoverageSnapshot, options: CoverageOutputOptions<'_>) {
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
    let sampled_micros = snapshot.time_deltas_micros.iter().copied().sum::<u64>();
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
                class.name,
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
        "{:.3}s (parse {:.3}s, projection {:.3}s, source-map hydration {:.3}s; {} constructor groups{})",
        analysis_duration as f64 / 1_000_000.0,
        snapshot.analysis.parse_duration_micros as f64 / 1_000_000.0,
        snapshot.analysis.projection_duration_micros as f64 / 1_000_000.0,
        snapshot.analysis.source_map_hydration_duration_micros as f64 / 1_000_000.0,
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
            class.name,
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
    let symbols = options.path.is_some() || options.all;
    let budget = if options.all {
        usize::MAX
    } else {
        options
            .max_lines
            .saturating_sub(1 + usize::from(snapshot.analysis.is_some()))
    };
    for line in root.render_with_options(
        &CoverageTreeStyle,
        symbols,
        TreeRenderOptions::terminal(budget, options.trim_width),
    ) {
        println!("{line}");
    }
}

fn filter_coverage_path(snapshot: &CoverageSnapshot, prefix: &str) -> CoverageSnapshot {
    let prefix = normalize_source_path(prefix);
    let mut filtered = snapshot.clone();
    for source in &mut filtered.sources {
        let generated_url = normalize_source_path(&source.generated_url);
        for function in &mut source.functions {
            let matches = |range: &cdp_client::service_api::CoverageRangeSnapshot| {
                range.authored_start.as_ref().is_some_and(|location| {
                    normalize_source_path(&location.source_url).starts_with(&prefix)
                }) || (range.authored_start.is_none() && generated_url.starts_with(&prefix))
            };
            function.ranges.retain(matches);
            function.effective_ranges.retain(matches);
        }
        source.functions.retain(|function| {
            !function.ranges.is_empty() || !function.effective_ranges.is_empty()
        });
    }
    filtered
        .sources
        .retain(|source| !source.functions.is_empty());
    filtered
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
    source: &cdp_client::service_api::CoverageSourceSnapshot,
    function: &cdp_client::service_api::CoverageFunctionSnapshot,
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

fn runtime_location(location: &cdp_client::service_api::SourceLocation) -> String {
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
    evaluation
        .value
        .as_ref()
        .map(format_value)
        .or_else(|| evaluation.unserializable_value.clone())
        .or_else(|| evaluation.description.clone())
        .unwrap_or_else(|| evaluation.kind.clone())
}

fn format_value(value: &serde_json::Value) -> String {
    match value {
        serde_json::Value::String(value) => format!("{value:?}"),
        _ => value.to_string(),
    }
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
        TargetBreakpointStatus::Pending => "pending".to_owned(),
        TargetBreakpointStatus::Installed { binding_count } => {
            let suffix = if *binding_count == 1 { "" } else { "s" };
            format!("installed; {binding_count} binding{suffix}")
        }
        TargetBreakpointStatus::Failed { message } => format!("failed: {message}"),
    }
}

#[cfg(test)]
mod tests {
    use super::{
        BoundedTree, CoverageEntry, CoverageMetrics, CoverageTreeStyle, HeapClassOutputOptions,
        ProcessTreeOutputOptions, aggregate_coverage_entries, coverage_entries,
        effective_file_metrics, heap_path_lines, looks_minified_identifier, page_logs,
        process_tree_lines, process_trees_json, render_compacted_source_graph,
        render_heap_classes_human, render_uncompacted_source_graph, style_process_label,
        style_session_label,
    };
    use cdp_client::service_api::{
        AgentSessionSnapshot, CompactedSourceEdgeSnapshot, CompactedSourceGraphSnapshot,
        CompactedSourceNodeSnapshot, ConsoleMessageSnapshot, CoverageFunctionSnapshot,
        CoverageRangeSnapshot, CoverageSnapshot, CoverageSourceSnapshot, HeapClassAnalysisSnapshot,
        HeapClassSnapshot, HeapClassSnapshotEntry, HeapInstanceSnapshot, HeapNodeSnapshot,
        HeapPathSnapshot, HeapPathStepSnapshot, HeapSnapshotTiming, HeapTraversalDirection,
        ProcessRole, ProcessSnapshot, ProcessTreeSnapshot, SourceLocation,
        SourceSuffixRewriteSnapshot, UncompactedProjectionSnapshot, UncompactedSourceEdgeSnapshot,
        UncompactedSourceGraphSnapshot, UncompactedSourceNodeSnapshot,
        UncompactedSourceRevisionSnapshot,
    };
    use std::collections::BTreeMap;

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
    fn process_tree_uses_virtual_window_nodes() {
        let tree = ProcessTreeSnapshot {
            root_process_id: 1,
            runtime_metadata_available: true,
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
                "└─ 1  Code.exe  [vscode-main]",
                "   ├─ window 3  project",
                "   │  ├─ 2  renderer  [renderer]",
                "   │  └─ 3  extension-host  [extension-host]",
                "   │     └─ 4  server  [node]",
                "   └─ 5  agent-host  [agent-host]",
            ]
        );
        let filtered = process_tree_lines(
            &tree,
            ProcessTreeOutputOptions {
                filter: Some("window 3"),
                ..ProcessTreeOutputOptions::default()
            },
        );
        assert!(filtered.iter().any(|line| line.contains("window 3")));
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
    fn process_tree_renders_multiplexed_agent_sessions_as_virtual_children() {
        let mut copilot = process(3, Some(2), "copilot", ProcessRole::Copilot, None, None);
        copilot.agent_sessions = vec![AgentSessionSnapshot {
            internal_id: "internal-1".to_owned(),
            chat_uri: Some("copilotcli:/chat-1".to_owned()),
            title: Some("Add extension launch config".to_owned()),
            working_directories: vec!["file:///d%3A/dev/hediet/cdp-client".to_owned()],
            disconnected: Some(false),
        }];
        let tree = ProcessTreeSnapshot {
            root_process_id: 1,
            runtime_metadata_available: true,
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
                "└─ 1  Code.exe  [vscode-main]",
                "   └─ 2  agent-host  [agent-host]",
                "      └─ 3  copilot  [copilot]",
                "         ├─ session Add extension launch config  [internal-1]",
                "         └─ 4  cmd.exe  [other]",
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
            shallow_size: 0,
            outgoing_reference_count,
            incoming_reference_count,
            locations: Vec::new(),
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
            },
        }
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
    fn log_paging_counts_entries_evicted_before_the_retained_window() {
        let logs = (50..=149)
            .map(|index| ConsoleMessageSnapshot {
                index,
                values: vec![index.to_string()],
            })
            .collect::<Vec<_>>();
        let (skipped, displayed) = page_logs(&logs, 1, 20);
        assert_eq!(skipped, 128);
        assert_eq!(displayed.first().unwrap().index, 130);
        assert_eq!(displayed.last().unwrap().index, 149);
    }
}
