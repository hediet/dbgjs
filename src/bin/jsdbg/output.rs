use cdp_client::service_api::{
    BreakpointStatus, ConnectionConfiguration, ConnectionStatus, ConsoleMessageSnapshot,
    ContextSnapshot, ContextSummary, CoverageSnapshot, EvaluationSnapshot, FrameProjectionSnapshot,
    PlaywrightChannel, ServiceInfo, SourceExcerpt, TargetBreakpointStatus, TargetDebuggerPhase,
    TargetDebuggerSnapshot,
};
use serde::Serialize;
use std::collections::{BTreeMap, BTreeSet};

#[derive(Clone, Copy)]
pub enum OutputFormat {
    Human,
    Json,
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
        path: Option<&str>,
    ) -> Result<(), serde_json::Error> {
        let filtered = path.map(|path| filter_coverage_path(value, path));
        let value = filtered.as_ref().unwrap_or(value);
        match self {
            Self::Human => print_coverage_human(value, path.is_some()),
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
        print_coverage_human(self, false);
    }
}

fn print_coverage_human(snapshot: &CoverageSnapshot, detailed: bool) {
    if snapshot.sources.is_empty() {
        println!("No executed functions captured.");
        return;
    }
    let mut files = BTreeMap::<String, Vec<CoverageEntry>>::new();
    for entry in coverage_entries(snapshot) {
        files.entry(entry.path.clone()).or_default().push(entry);
    }

    let mut root = CoverageTree::default();
    for (path, entries) in files {
        let mut entries = aggregate_coverage_entries(entries);
        entries.sort_by_key(|entry| std::cmp::Reverse(entry.line_span));
        let total = effective_file_hit_loc(&entries);
        root.insert_file(path, total, entries);
    }

    root.print(detailed);
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

fn effective_file_hit_loc(entries: &[CoverageEntry]) -> u64 {
    let lines = entries
        .iter()
        .flat_map(|entry| entry.lines.iter().copied())
        .collect::<BTreeSet<_>>();
    if lines.is_empty() {
        entries.iter().map(|entry| entry.line_span).sum()
    } else {
        lines.len() as u64
    }
}

struct CoverageEntry {
    path: String,
    function: String,
    lines: BTreeSet<u32>,
    line_span: u64,
    generated_location: Option<String>,
}

fn aggregate_coverage_entries(entries: Vec<CoverageEntry>) -> Vec<CoverageEntry> {
    let mut symbols = BTreeMap::<String, BTreeSet<u32>>::new();
    let mut unknown = BTreeMap::<String, u64>::new();
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
        if entry.lines.is_empty() {
            *unknown.entry(entry.function).or_default() += entry.line_span;
        } else {
            symbols
                .entry(entry.function)
                .or_default()
                .extend(entry.lines);
        }
    }
    paths
        .into_iter()
        .map(|(function, path)| {
            let lines = symbols.remove(&function).unwrap_or_default();
            let line_span = if lines.is_empty() {
                unknown.remove(&function).unwrap_or(1)
            } else {
                lines.len() as u64
            };
            let generated_location = generated_locations.remove(&function);
            CoverageEntry {
                path,
                function,
                lines,
                line_span,
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
            let projected_ranges = if function.effective_ranges.is_empty() {
                &function.ranges
            } else {
                &function.effective_ranges
            };
            let mut ranges = projected_ranges.iter().collect::<Vec<_>>();
            ranges.sort_by_key(|range| {
                std::cmp::Reverse(range.end_offset.saturating_sub(range.start_offset))
            });
            let mut lines_by_path = BTreeMap::<String, BTreeSet<u32>>::new();
            let mut has_unmapped_hit = false;
            for range in ranges {
                match (&range.authored_start, &range.authored_end) {
                    (Some(start), Some(end)) if start.source_url == end.source_url => {
                        let lines = lines_by_path
                            .entry(normalize_source_path(&start.source_url))
                            .or_default();
                        for line in start.line..=end.line.max(start.line) {
                            if range.count > 0 {
                                lines.insert(line);
                            } else {
                                lines.remove(&line);
                            }
                        }
                    }
                    _ if range.count > 0 => has_unmapped_hit = true,
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
            for (path, lines) in lines_by_path {
                if lines.is_empty() {
                    continue;
                }
                mapped = true;
                entries.push(CoverageEntry {
                    path,
                    function: function_name.clone(),
                    line_span: lines.len() as u64,
                    lines,
                    generated_location: generated_location.clone(),
                });
            }
            if !mapped && has_unmapped_hit {
                entries.push(CoverageEntry {
                    path: source.generated_url.clone(),
                    function: function_name,
                    lines: BTreeSet::new(),
                    line_span: 1,
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

#[derive(Default)]
struct CoverageTree {
    children: BTreeMap<String, CoverageTree>,
    ranges: Vec<CoverageEntry>,
    hit_loc: u64,
    file_count: usize,
}

impl CoverageTree {
    fn insert_file(&mut self, path: String, hit_loc: u64, ranges: Vec<CoverageEntry>) {
        let components = path
            .split('/')
            .filter(|component| !component.is_empty())
            .collect::<Vec<_>>();
        let mut node = self;
        node.hit_loc = node.hit_loc.saturating_add(hit_loc);
        node.file_count += 1;
        for component in components {
            node = node.children.entry(component.to_owned()).or_default();
            node.hit_loc = node.hit_loc.saturating_add(hit_loc);
            node.file_count += 1;
        }
        node.ranges = ranges;
    }

    fn hit_loc(&self) -> u64 {
        self.hit_loc
    }

    fn print(&self, detailed: bool) {
        self.print_children("", detailed, 0);
    }

    fn print_children(&self, prefix: &str, detailed: bool, depth: usize) {
        const CHILD_LIMIT: usize = 5;
        const SYMBOL_LIMIT: usize = 3;
        const MAX_DEPTH: usize = 4;

        if !detailed && depth >= MAX_DEPTH {
            return;
        }

        let mut children = self.children.iter().collect::<Vec<_>>();
        children.sort_by_key(|(_, child)| std::cmp::Reverse(child.hit_loc));
        let depth_limit = CHILD_LIMIT.saturating_sub(depth / 2).max(3);
        let visible = if detailed {
            children.len()
        } else {
            children.len().min(depth_limit)
        };
        let hidden_items = children.len().saturating_sub(visible);
        let hidden_files = children[visible..]
            .iter()
            .map(|(_, child)| child.file_count)
            .sum::<usize>();
        let hidden_loc = children[visible..]
            .iter()
            .map(|(_, child)| child.hit_loc)
            .sum::<u64>();
        let output_len = visible + usize::from(hidden_items > 0);
        for (index, (name, child)) in children.into_iter().take(visible).enumerate() {
            let last = index + 1 == output_len;
            let branch = if last { "└─" } else { "├─" };
            let (label, child) = collapse_tree_label(name, child);
            if child.ranges.is_empty() {
                println!(
                    "{prefix}{branch} {label}/  [{} files, {} hit LoC]",
                    child.file_count,
                    child.hit_loc()
                );
            } else {
                println!("{prefix}{branch} {label}  {} hit LoC", child.hit_loc());
            }
            let child_prefix = format!("{prefix}{}", if last { "   " } else { "│  " });
            child.print_children(&child_prefix, detailed, depth + 1);
            let range_limit = if detailed {
                child.ranges.len()
            } else {
                child.ranges.len().min(SYMBOL_LIMIT)
            };
            for (range_index, entry) in child.ranges.iter().take(range_limit).enumerate() {
                let has_aggregate = range_limit < child.ranges.len();
                let range_last = range_index + 1 == range_limit && !has_aggregate;
                let range_branch = if range_last { "└─" } else { "├─" };
                println!(
                    "{child_prefix}{range_branch} {}  {} hit LoC{}",
                    entry.function,
                    entry.line_span,
                    entry
                        .generated_location
                        .as_ref()
                        .map_or_else(String::new, |location| format!("  [generated {location}]"))
                );
            }
            if range_limit < child.ranges.len() {
                let omitted = &child.ranges[range_limit..];
                println!(
                    "{child_prefix}└─ … [{} symbols, {} hit LoC]",
                    omitted.len(),
                    omitted.iter().map(|entry| entry.line_span).sum::<u64>()
                );
            }
        }
        if hidden_items > 0 {
            println!(
                "{prefix}└─ … [{hidden_items} items, {hidden_files} files, {hidden_loc} hit LoC]",
            );
        }
    }
}

fn collapse_tree_label<'a>(name: &str, mut node: &'a CoverageTree) -> (String, &'a CoverageTree) {
    let mut label = name.to_owned();
    while node.ranges.is_empty() && node.children.len() == 1 {
        let (child_name, child) = node.children.first_key_value().unwrap();
        label.push('/');
        label.push_str(child_name);
        node = child;
    }
    (label, node)
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
        CoverageEntry, aggregate_coverage_entries, coverage_entries, effective_file_hit_loc,
        looks_minified_identifier, page_logs,
    };
    use cdp_client::service_api::{
        ConsoleMessageSnapshot, CoverageFunctionSnapshot, CoverageRangeSnapshot, CoverageSnapshot,
        CoverageSourceSnapshot, SourceLocation,
    };
    use std::collections::BTreeSet;

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
    fn nested_zero_count_ranges_remove_parent_lines() {
        let location = |line| SourceLocation {
            source_url: "src/example.ts".to_owned(),
            line,
            column: 1,
        };
        let snapshot = CoverageSnapshot {
            timestamp_micros: 0,
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
        assert_eq!(entries[0].line_span, 89);
        assert!(!entries[0].lines.contains(&50));
        assert!(entries[0].lines.contains(&49));
        assert!(entries[0].lines.contains(&61));
    }

    #[test]
    fn aggregation_preserves_noncontiguous_line_sets() {
        let entries = aggregate_coverage_entries(vec![CoverageEntry {
            path: "src/example.ts".to_owned(),
            function: "example".to_owned(),
            lines: BTreeSet::from([1, 100]),
            line_span: 2,
            generated_location: None,
        }]);
        assert_eq!(effective_file_hit_loc(&entries), 2);
        assert_eq!(entries[0].line_span, 2);
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
