use cdp_client::service_api::{
    BreakpointStatus, ConnectionConfiguration, ConnectionStatus, ConsoleMessageSnapshot,
    ContextSnapshot, ContextSummary, CoverageSnapshot, EvaluationSnapshot, FrameProjectionSnapshot,
    HeapCaptureResult, HeapClassSnapshot, HeapClassSnapshotEntry, HeapSnapshotProgress,
    HeapSnapshotResult, PlaywrightChannel, ServiceInfo, SourceExcerpt, TargetBreakpointStatus,
    TargetDebuggerPhase, TargetDebuggerSnapshot,
};
use serde::Serialize;
use std::collections::{BTreeMap, BTreeSet};

use super::bounded_tree::{BoundedTree, BoundedTreeStyle, TreeAggregate};

#[derive(Clone, Copy)]
pub enum OutputFormat {
    Human,
    Json,
}

pub struct CoverageOutputOptions<'a> {
    pub path: Option<&'a str>,
    pub all: bool,
    pub max_lines: usize,
}

pub struct HeapClassOutputOptions {
    pub all: bool,
    pub max_lines: usize,
    pub instances: bool,
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

impl HumanOutput for HeapSnapshotResult {
    fn print_human(&self) {
        println!(
            "Heap snapshot written to {} ({} bytes).",
            self.path, self.bytes_written
        );
    }
}

impl HumanOutput for HeapCaptureResult {
    fn print_human(&self) {
        println!(
            "Captured {} ({}).",
            self.capture_id,
            compact_bytes(self.bytes_written)
        );
    }
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
                    for target in &connection.targets {
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
                            "        {}  {}  {}{}",
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
                    match breakpoint.status {
                        BreakpointStatus::Unconfirmed => "unconfirmed",
                    }
                );
            }
        }
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
                    FrameProjectionSnapshot::Failed { message } => {
                        format!("mapping failed ({message})")
                    }
                };
                println!("    #{} {function_name} — {location}", frame.index);
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
    output.push(format!(
        "Analysis {:.3}s: parse {:.3}s, projection {:.3}s (source-map hydration {:.3}s; {} constructor groups{})",
        (snapshot.analysis.parse_duration_micros + snapshot.analysis.projection_duration_micros)
            as f64
            / 1_000_000.0,
        snapshot.analysis.parse_duration_micros as f64 / 1_000_000.0,
        snapshot.analysis.projection_duration_micros as f64 / 1_000_000.0,
        snapshot.analysis.source_map_hydration_duration_micros as f64 / 1_000_000.0,
        snapshot.analysis.constructor_group_count,
        if snapshot.analysis.used_cached_groups {
            ", cached"
        } else {
            ""
        }
    ));
    if snapshot.classes.is_empty() {
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
    output.extend(tree.render(&style, true, budget));
    if maximum_lines != usize::MAX {
        output.truncate(maximum_lines);
    }
    output
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
    for line in root.render(&CoverageTreeStyle, symbols, budget) {
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
        ConnectionConfiguration::Playwright {
            url,
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
        aggregate_coverage_entries, coverage_entries, effective_file_metrics,
        looks_minified_identifier, page_logs, render_heap_classes_human,
    };
    use cdp_client::service_api::{
        ConsoleMessageSnapshot, CoverageFunctionSnapshot, CoverageRangeSnapshot, CoverageSnapshot,
        CoverageSourceSnapshot, HeapClassAnalysisSnapshot, HeapClassSnapshot,
        HeapClassSnapshotEntry, HeapInstanceSnapshot, SourceLocation,
    };
    use std::collections::BTreeMap;

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
            },
        );
        assert!(lines.iter().any(|line| {
            line.contains("PieceTreeModel@1 id 1") && line.contains("PieceTreeModel@2 id 2")
        }));
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
            },
        );
        assert_eq!(lines.len(), 1);
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
