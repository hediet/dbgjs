use cdp_client::service_api::{
    BreakpointStatus, ConnectionConfiguration, ConnectionStatus, ContextSnapshot, ContextSummary,
    CoverageSnapshot, EvaluationSnapshot, FrameProjectionSnapshot, PlaywrightChannel, ServiceInfo,
    TargetBreakpointStatus, TargetDebuggerPhase, TargetDebuggerSnapshot,
};
use serde::Serialize;
use std::collections::BTreeMap;

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
    if !snapshot.logs.is_empty() {
        println!("Logs:");
        for message in &snapshot.logs {
            println!("  {}", message.values.join(" "));
        }
    }

    match &snapshot.pause {
        None => println!("  Pause: none"),
        Some(pause) => {
            println!("  Pause: epoch {} ({})", pause.epoch, pause.reason);
            if let Some(source) = &pause.source {
                match &source.breadcrumb {
                    Some(breadcrumb) => {
                        println!("  Source: {} — {}", source.source_url, breadcrumb)
                    }
                    None => println!("  Source: {}", source.source_url),
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
        if self.sources.is_empty() {
            println!("No executed functions captured.");
            return;
        }
        let mut files = BTreeMap::<String, Vec<CoverageEntry>>::new();
        for entry in coverage_entries(self) {
            files.entry(entry.path.clone()).or_default().push(entry);
        }
        let mut files = files.into_iter().collect::<Vec<_>>();
        files.sort_by_key(|(_, entries)| {
            std::cmp::Reverse(entries.iter().map(|entry| entry.weighted_loc).sum::<u64>())
        });
        let omitted_files = files.len().saturating_sub(20);
        let omitted_ranges = files
            .iter()
            .take(20)
            .map(|(_, entries)| entries.len().saturating_sub(5))
            .sum::<usize>();
        let mut root = CoverageTree::default();
        for (path, mut entries) in files.into_iter().take(20) {
            entries.sort_by_key(|entry| std::cmp::Reverse(entry.weighted_loc));
            let total = entries.iter().map(|entry| entry.weighted_loc).sum();
            root.insert_file(path, total, entries.into_iter().take(5).collect());
        }
        root.print();
        if omitted_ranges > 0 {
            println!("… {omitted_ranges} additional hit ranges omitted from displayed files");
        }
        if omitted_files > 0 {
            println!("… {omitted_files} additional files omitted");
        }
    }
}

struct CoverageEntry {
    path: String,
    function: String,
    start_line: u32,
    start_column: u32,
    end_line: u32,
    end_column: u32,
    count: u64,
    weighted_loc: u64,
}

fn coverage_entries(snapshot: &CoverageSnapshot) -> Vec<CoverageEntry> {
    snapshot
        .sources
        .iter()
        .flat_map(|source| {
            source.functions.iter().flat_map(move |function| {
                function.ranges.iter().filter_map(move |range| {
                    if range.count == 0 || function.name == "(anonymous)" {
                        return None;
                    }
                    let (path, start_line, start_column, end_line, end_column) =
                        match (&range.authored_start, &range.authored_end) {
                            (Some(start), Some(end)) if start.source_url == end.source_url => (
                                normalize_source_path(&start.source_url),
                                start.line,
                                start.column,
                                end.line,
                                end.column,
                            ),
                            _ => (
                                source.generated_url.clone(),
                                0,
                                range.start_offset,
                                0,
                                range.end_offset,
                            ),
                        };
                    let line_span = if start_line > 0 && end_line >= start_line {
                        u64::from(end_line - start_line + 1)
                    } else {
                        1
                    };
                    Some(CoverageEntry {
                        path,
                        function: function
                            .breadcrumb
                            .clone()
                            .unwrap_or_else(|| function.name.clone()),
                        start_line,
                        start_column,
                        end_line,
                        end_column,
                        count: range.count,
                        weighted_loc: line_span.saturating_mul(range.count),
                    })
                })
            })
        })
        .collect()
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
    weighted_loc: u64,
}

impl CoverageTree {
    fn insert_file(&mut self, path: String, weighted_loc: u64, ranges: Vec<CoverageEntry>) {
        let components = path
            .split('/')
            .filter(|component| !component.is_empty())
            .collect::<Vec<_>>();
        let mut node = self;
        node.weighted_loc = node.weighted_loc.saturating_add(weighted_loc);
        for component in components {
            node = node.children.entry(component.to_owned()).or_default();
            node.weighted_loc = node.weighted_loc.saturating_add(weighted_loc);
        }
        node.ranges = ranges;
    }

    fn weighted_loc(&self) -> u64 {
        self.weighted_loc
    }

    fn print(&self) {
        self.print_children("");
    }

    fn print_children(&self, prefix: &str) {
        let len = self.children.len();
        for (index, (name, child)) in self.children.iter().enumerate() {
            let last = index + 1 == len;
            let branch = if last { "└─" } else { "├─" };
            println!(
                "{prefix}{branch} {name}  {} weighted LoC",
                child.weighted_loc()
            );
            let child_prefix = format!("{prefix}{}", if last { "   " } else { "│  " });
            child.print_children(&child_prefix);
            for entry in &child.ranges {
                let location = if entry.start_line > 0 {
                    format!(
                        "{}:{}–{}:{}",
                        entry.start_line, entry.start_column, entry.end_line, entry.end_column
                    )
                } else {
                    format!("+{}–+{}", entry.start_column, entry.end_column)
                };
                println!(
                    "{child_prefix}└─ {}  {}  x{}  {} weighted LoC",
                    entry.function, location, entry.count, entry.weighted_loc
                );
            }
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
