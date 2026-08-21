use cdp_client::service_api::{
    BreakpointStatus, ConnectionConfiguration, ConnectionStatus, ContextSnapshot, ContextSummary,
    FrameProjectionSnapshot, PlaywrightChannel, ServiceInfo, TargetBreakpointStatus,
    TargetDebuggerPhase, TargetDebuggerSnapshot, TargetScriptStatus,
};
use serde::Serialize;

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
        println!("  Protocol: {}", self.protocol_version);
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
                    for target in &connection.targets {
                        let title = if target.title.is_empty() {
                            "(untitled)"
                        } else {
                            &target.title
                        };
                        println!(
                            "        {}  {}  {}  {}{}",
                            target.target_id,
                            target.target_type,
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
        println!("Target {}  [{}]", self.target_id, target_phase(&self.phase));
        println!("  Context: {}", self.context_id);
        println!("  Connection: {}", self.connection_id);
        println!("  Generation: {}", self.connection_generation);
        println!("  Revision: {}", self.revision);

        let named_scripts = self
            .scripts
            .iter()
            .filter(|script| !script.url.is_empty())
            .collect::<Vec<_>>();
        let anonymous_count = self.scripts.len() - named_scripts.len();
        if named_scripts.is_empty() && anonymous_count == 0 {
            println!("  Scripts: none observed");
        } else {
            println!("  Scripts:");
            for script in named_scripts {
                println!("    {}  [{}]", script.url, script_status(&script.status));
                if let Some(source_map_url) = &script.source_map_url {
                    println!("      Source map: {source_map_url}");
                }
                if let TargetScriptStatus::Resolved { authored_sources } = &script.status {
                    for source in authored_sources {
                        println!("      Authored: {source}");
                    }
                }
            }
            if anonymous_count > 0 {
                println!("    {anonymous_count} anonymous runtime script(s)");
            }
        }

        if !self.breakpoints.is_empty() {
            println!("  Breakpoints:");
            for breakpoint in &self.breakpoints {
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

        match &self.pause {
            None => println!("  Pause: none"),
            Some(pause) => {
                println!("  Pause: epoch {} ({})", pause.epoch, pause.reason);
                println!("  Frames:");
                for frame in &pause.frames {
                    let function_name = if frame.function_name.is_empty() {
                        "(anonymous)"
                    } else {
                        &frame.function_name
                    };
                    println!("    #{} {function_name}", frame.index);
                    match &frame.projected {
                        FrameProjectionSnapshot::Resolved { location }
                            if location.source_url.is_empty() =>
                        {
                            println!("      Authored: unavailable")
                        }
                        FrameProjectionSnapshot::Resolved { location } => println!(
                            "      Authored: {}:{}:{}",
                            location.source_url, location.line, location.column
                        ),
                        FrameProjectionSnapshot::Raw => println!("      Authored: not mapped"),
                        FrameProjectionSnapshot::Pending => println!("      Authored: mapping"),
                        FrameProjectionSnapshot::Failed { message } => {
                            println!("      Authored: mapping failed ({message})")
                        }
                    }
                    let generated_source = if frame.raw.source_url.is_empty() {
                        "(anonymous script)"
                    } else {
                        &frame.raw.source_url
                    };
                    println!(
                        "      Generated: {}:{}:{}",
                        generated_source, frame.raw.line, frame.raw.column
                    );
                }
            }
        }
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
        } => format!(
            "Playwright {}{} opening {url}",
            playwright_channel(channel),
            if *headless { " headless" } else { " headed" }
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

fn script_status(status: &TargetScriptStatus) -> String {
    match status {
        TargetScriptStatus::Unresolved => "unresolved".to_owned(),
        TargetScriptStatus::Pending => "loading".to_owned(),
        TargetScriptStatus::Resolved { .. } => "source map resolved".to_owned(),
        TargetScriptStatus::Failed { message } => format!("failed: {message}"),
    }
}
