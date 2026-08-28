use std::collections::BTreeMap;
use std::fmt::Write as _;
use std::io::{self, Write};
use std::time::Duration;

use cdp_client::service_api::{
    BreakpointSnapshot, BreakpointStatus, ConnectionConfiguration, ConnectionSnapshot,
    ConnectionStatus, ContextSnapshot, DebuggerServiceApiClient, FrameProjectionSnapshot,
    TargetBreakpointStatus, TargetDebuggerPhase, TargetDebuggerSnapshot, TargetSnapshot,
};
use futures_util::FutureExt;
use futures_util::future::{LocalBoxFuture, select_all};

pub struct ContextView {
    pub snapshot: ContextSnapshot,
    pub debuggers: BTreeMap<(String, String), TargetDebuggerSnapshot>,
}

pub async fn run(
    client: &DebuggerServiceApiClient,
    context_id: Option<&str>,
    all_contexts: bool,
    interactive: bool,
) -> Result<(), io::Error> {
    let mut previous = String::new();
    loop {
        let contexts = capture(client, context_id, all_contexts).await?;
        let screen = render(&contexts, all_contexts);
        if !interactive {
            print!("{screen}");
            return Ok(());
        }
        if screen != previous {
            print!("\x1b[2J\x1b[H{screen}");
            io::stdout().flush()?;
            previous = screen;
        }
        wait_for_change(client, &contexts).await;
    }
}

async fn capture(
    client: &DebuggerServiceApiClient,
    context_id: Option<&str>,
    all_contexts: bool,
) -> Result<Vec<ContextView>, io::Error> {
    let context_ids = if all_contexts {
        client
            .list_contexts(None)
            .await
            .map_err(rpc_error)?
            .into_iter()
            .map(|context| context.id)
            .collect::<Vec<_>>()
    } else {
        vec![
            context_id
                .ok_or_else(|| io::Error::other("daemon view requires a context"))?
                .to_owned(),
        ]
    };

    let mut contexts = Vec::with_capacity(context_ids.len());
    for context_id in context_ids {
        let snapshot = match client.get_context(context_id.clone()).await {
            Ok(snapshot) => snapshot,
            Err(_) if all_contexts => continue,
            Err(error) => return Err(rpc_error(error)),
        };
        let mut debuggers = BTreeMap::new();
        for target in &snapshot.target_forest {
            let debugger = client
                .get_target(
                    snapshot.id.clone(),
                    target.connection_id.clone(),
                    target.target.target_id.clone(),
                )
                .await;
            if let Ok(debugger) = debugger
                && debugger.connection_generation == target.connection_generation
            {
                debuggers.insert(
                    (
                        target.connection_id.clone(),
                        target.target.target_id.clone(),
                    ),
                    debugger,
                );
            }
        }
        contexts.push(ContextView {
            snapshot,
            debuggers,
        });
    }
    contexts.sort_by(|left, right| left.snapshot.id.cmp(&right.snapshot.id));
    Ok(contexts)
}

async fn wait_for_change(client: &DebuggerServiceApiClient, contexts: &[ContextView]) {
    let mut waits = Vec::<LocalBoxFuture<'_, ()>>::new();
    for context in contexts {
        let context_id = context.snapshot.id.clone();
        let revision = context.snapshot.revision;
        waits.push(
            async move {
                let _ = client
                    .observe_context(
                        context_id,
                        cdp_client::service_api::ObservationCursor::After { revision },
                        30_000,
                    )
                    .await;
            }
            .boxed_local(),
        );
        for debugger in context.debuggers.values() {
            let context_id = debugger.context_id.clone();
            let connection_id = debugger.connection_id.clone();
            let target_id = debugger.target_id.clone();
            let revision = debugger.revision;
            waits.push(
                async move {
                    let _ = client
                        .observe_target(context_id, connection_id, target_id, revision, 30_000)
                        .await;
                }
                .boxed_local(),
            );
        }
    }
    waits.push(
        async {
            tokio::time::sleep(Duration::from_secs(1)).await;
        }
        .boxed_local(),
    );
    let _ = select_all(waits).await;
}

pub fn render(contexts: &[ContextView], all_contexts: bool) -> String {
    let mut output = String::new();
    let scope = if all_contexts {
        "all contexts".to_owned()
    } else {
        contexts
            .first()
            .map(|context| format!("context {}", context.snapshot.id))
            .unwrap_or_else(|| "current context".to_owned())
    };
    writeln!(output, "jsdbg daemon view — {scope}").unwrap();
    writeln!(
        output,
        "breakpoints per target: i=installed p=pending f=failed"
    )
    .unwrap();

    if contexts.is_empty() {
        writeln!(output).unwrap();
        writeln!(output, "No debugger contexts.").unwrap();
        return output;
    }

    for (context_index, context) in contexts.iter().enumerate() {
        if context_index != 0 {
            writeln!(output).unwrap();
        }
        writeln!(
            output,
            "Context {} {:?} rev={} connections={} breakpoints={}",
            context.snapshot.id,
            inline(&context.snapshot.display_name, 60),
            context.snapshot.revision,
            context.snapshot.connections.len(),
            context.snapshot.breakpoints.len(),
        )
        .unwrap();

        if context.snapshot.connections.is_empty() {
            writeln!(output, "  (no connections)").unwrap();
            continue;
        }

        let mut connections = context.snapshot.connections.iter().collect::<Vec<_>>();
        connections.sort_by(|left, right| left.id.cmp(&right.id));
        for connection in connections {
            render_connection(&mut output, context, connection);
        }
    }
    output
}

fn render_connection(output: &mut String, context: &ContextView, connection: &ConnectionSnapshot) {
    writeln!(
        output,
        "  Connection {} {} gen={} targets={} kind={}",
        connection.id,
        connection_status(&connection.status),
        connection.generation,
        connection.targets.len(),
        connection_kind(&connection.configuration),
    )
    .unwrap();

    let mut targets = connection.targets.iter().collect::<Vec<_>>();
    targets.sort_by(|left, right| left.target_id.cmp(&right.target_id));
    for target in targets {
        let debugger = context
            .debuggers
            .get(&(connection.id.clone(), target.target_id.clone()));
        render_target(
            output,
            target,
            &context.snapshot.breakpoints,
            connection.generation,
            &connection.id,
            debugger,
        );
    }
}

fn render_target(
    output: &mut String,
    target: &TargetSnapshot,
    desired_breakpoints: &[BreakpointSnapshot],
    generation: u64,
    connection_id: &str,
    debugger: Option<&TargetDebuggerSnapshot>,
) {
    let title = if target.title.is_empty() {
        "(untitled)".to_owned()
    } else {
        inline(&target.title, 60)
    };
    let (phase, location, lifecycle) = debugger.map(debugger_state).unwrap_or_else(|| {
        (
            "unobserved".to_owned(),
            String::new(),
            if target.attached {
                "externally-attached".to_owned()
            } else {
                "observed".to_owned()
            },
        )
    });
    let attachment = if debugger.is_some() {
        "attached"
    } else if target.attached {
        "external"
    } else {
        "detached"
    };
    let (installed, pending, failed) = debugger
        .map(breakpoint_counts)
        .unwrap_or_else(|| desired_breakpoint_counts(desired_breakpoints, target));
    let url = if target.url.is_empty() {
        String::new()
    } else {
        format!(" url={:?}", inline(&target.url, 80))
    };
    writeln!(
        output,
        "    target={connection_id}/{} [{}] {:?} {}{} debugger={} bp=i{installed}/p{pending}/f{failed} gen={generation} lifecycle={lifecycle}{url}",
        target.target_id,
        target.target_type,
        title,
        phase,
        location,
        attachment,
    )
    .unwrap();
}

fn debugger_state(debugger: &TargetDebuggerSnapshot) -> (String, String, String) {
    match &debugger.phase {
        TargetDebuggerPhase::Running => {
            ("running".to_owned(), String::new(), "debugging".to_owned())
        }
        TargetDebuggerPhase::Paused { epoch } => (
            format!("paused(epoch={epoch})"),
            pause_location(debugger),
            "debugging".to_owned(),
        ),
        TargetDebuggerPhase::Resuming { epoch } => (
            format!("resuming(epoch={epoch})"),
            pause_location(debugger),
            "resuming".to_owned(),
        ),
        TargetDebuggerPhase::Failed { message } => (
            "failed".to_owned(),
            String::new(),
            format!("failed({:?})", inline(message, 80)),
        ),
    }
}

fn pause_location(debugger: &TargetDebuggerSnapshot) -> String {
    let Some(frame) = debugger
        .pause
        .as_ref()
        .and_then(|pause| pause.frames.first())
    else {
        return String::new();
    };
    let generated = source_location(&frame.raw);
    match &frame.projected {
        FrameProjectionSnapshot::Resolved { location } => {
            format!(" at {} <- {generated}", source_location(location))
        }
        FrameProjectionSnapshot::Raw
        | FrameProjectionSnapshot::Pending
        | FrameProjectionSnapshot::Failed { .. } => format!(" at {generated}"),
    }
}

fn source_location(location: &cdp_client::service_api::SourceLocation) -> String {
    format!(
        "{}:{}:{}",
        inline(&location.source_url, 80),
        location.line,
        location.column
    )
}

fn breakpoint_counts(debugger: &TargetDebuggerSnapshot) -> (usize, usize, usize) {
    debugger
        .breakpoints
        .iter()
        .fold(
            (0, 0, 0),
            |(installed, pending, failed), breakpoint| match breakpoint.status {
                TargetBreakpointStatus::Installed { .. } => (installed + 1, pending, failed),
                TargetBreakpointStatus::Failed { .. } => (installed, pending, failed + 1),
                TargetBreakpointStatus::WaitingForScript
                | TargetBreakpointStatus::SourceNotFound { .. }
                | TargetBreakpointStatus::AmbiguousSource { .. }
                | TargetBreakpointStatus::Unmapped { .. }
                | TargetBreakpointStatus::Applicable { .. }
                | TargetBreakpointStatus::Installing { .. } => (installed, pending + 1, failed),
            },
        )
}

fn desired_breakpoint_counts(
    breakpoints: &[BreakpointSnapshot],
    target: &TargetSnapshot,
) -> (usize, usize, usize) {
    breakpoints
        .iter()
        .filter(|breakpoint| {
            breakpoint.enabled
                && breakpoint
                    .target_selector
                    .as_deref()
                    .is_none_or(|selector| {
                        selector == target.target_id
                            || selector == target.target_type
                            || selector == target.title
                            || selector == target.url
                    })
        })
        .fold(
            (0, 0, 0),
            |(installed, pending, failed), breakpoint| match breakpoint.status {
                BreakpointStatus::Disabled => (installed, pending, failed),
                BreakpointStatus::Failed { .. } => (installed, pending, failed + 1),
                BreakpointStatus::Unconfirmed
                | BreakpointStatus::Pending
                | BreakpointStatus::PartiallyBound { .. }
                | BreakpointStatus::Bound { .. } => (installed, pending + 1, failed),
            },
        )
}

fn connection_status(status: &ConnectionStatus) -> String {
    match status {
        ConnectionStatus::Disconnected => "disconnected".to_owned(),
        ConnectionStatus::Connecting => "connecting".to_owned(),
        ConnectionStatus::Disconnecting => "disconnecting".to_owned(),
        ConnectionStatus::Connected { .. } => "connected".to_owned(),
        ConnectionStatus::Failed { message } => format!("failed({:?})", inline(message, 80)),
    }
}

fn connection_kind(configuration: &ConnectionConfiguration) -> &'static str {
    match configuration {
        ConnectionConfiguration::DirectCdp { .. } => "direct-cdp",
        ConnectionConfiguration::NodeInspector { .. } => "node-inspector",
        ConnectionConfiguration::Process { .. } => "process",
        ConnectionConfiguration::ProcessTree { .. } => "process-tree",
        ConnectionConfiguration::Playwright { .. } => "playwright",
        ConnectionConfiguration::Chrome { .. } => "chrome",
        ConnectionConfiguration::Node { .. } => "node",
    }
}

fn inline(value: &str, max_chars: usize) -> String {
    let value = value.split_whitespace().collect::<Vec<_>>().join(" ");
    let mut chars = value.chars();
    let prefix = chars.by_ref().take(max_chars).collect::<String>();
    if chars.next().is_some() {
        format!("{prefix}…")
    } else {
        prefix
    }
}

fn rpc_error(error: hubrpc::prelude::JsonRpcError) -> io::Error {
    io::Error::other(format!("{error:?}"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use cdp_client::service_api::{
        BreakpointSnapshot, ContextSnapshot, FrameSnapshot, PauseSnapshot, ScopeSnapshot,
        SourceLocation, TargetBreakpointSnapshot, TargetNodeSnapshot,
    };

    #[test]
    fn renders_daemon_state_deterministically_with_authored_pause_location() {
        let target = TargetSnapshot {
            target_id: "page-1".to_owned(),
            target_type: "page".to_owned(),
            title: "Checkout\npage".to_owned(),
            url: "https://example.test/checkout".to_owned(),
            attached: true,
            parent_id: None,
            opener_id: None,
            browser_context_id: None,
            subtype: None,
        };
        let context = ContextSnapshot {
            agent_instance_id: "agent-ignored".to_owned(),
            id: "shop".to_owned(),
            display_name: "Shop".to_owned(),
            revision: 12,
            connections: vec![ConnectionSnapshot {
                id: "browser".to_owned(),
                configuration: ConnectionConfiguration::DirectCdp {
                    endpoint: "ws://ignored".to_owned(),
                },
                generation: 7,
                status: ConnectionStatus::Connected {
                    product: "Chrome".to_owned(),
                    protocol_version: "1.3".to_owned(),
                },
                targets: vec![target.clone()],
            }],
            target_forest: vec![TargetNodeSnapshot {
                connection_id: "browser".to_owned(),
                connection_generation: 7,
                target,
                parent_target_id: None,
            }],
            breakpoints: vec![],
        };
        let debugger = TargetDebuggerSnapshot {
            context_id: "shop".to_owned(),
            connection_id: "browser".to_owned(),
            target_id: "page-1".to_owned(),
            connection_generation: 7,
            revision: 19,
            phase: TargetDebuggerPhase::Paused { epoch: 3 },
            scripts: vec![],
            breakpoints: vec![
                target_breakpoint(
                    "installed",
                    TargetBreakpointStatus::Installed { binding_count: 1 },
                ),
                target_breakpoint("pending", TargetBreakpointStatus::WaitingForScript),
                target_breakpoint(
                    "failed",
                    TargetBreakpointStatus::Failed {
                        message: "bad map".to_owned(),
                    },
                ),
            ],
            logs: vec![],
            pause: Some(PauseSnapshot {
                epoch: 3,
                reason: "other".to_owned(),
                frames: vec![FrameSnapshot {
                    index: 0,
                    function_name: "submit".to_owned(),
                    raw: SourceLocation {
                        source_url: "https://example.test/app.js".to_owned(),
                        line: 42,
                        column: 7,
                    },
                    projected: FrameProjectionSnapshot::Resolved {
                        location: SourceLocation {
                            source_url: "file:///workspace/src/checkout.ts".to_owned(),
                            line: 8,
                            column: 3,
                        },
                    },
                    scopes: Vec::<ScopeSnapshot>::new(),
                    breadcrumb: None,
                }],
                source: None,
            }),
        };
        let actual = render(
            &[ContextView {
                snapshot: context,
                debuggers: BTreeMap::from([(
                    ("browser".to_owned(), "page-1".to_owned()),
                    debugger,
                )]),
            }],
            false,
        );
        assert_eq!(
            actual,
            concat!(
                "jsdbg daemon view — context shop\n",
                "breakpoints per target: i=installed p=pending f=failed\n",
                "Context shop \"Shop\" rev=12 connections=1 breakpoints=0\n",
                "  Connection browser connected gen=7 targets=1 kind=direct-cdp\n",
                "    target=browser/page-1 [page] \"Checkout page\" paused(epoch=3) at file:///workspace/src/checkout.ts:8:3 <- https://example.test/app.js:42:7 debugger=attached bp=i1/p1/f1 gen=7 lifecycle=debugging url=\"https://example.test/checkout\"\n",
            )
        );
    }

    #[test]
    fn renders_all_contexts_and_unattached_target_lifecycle() {
        let target = TargetSnapshot {
            target_id: "worker-1".to_owned(),
            target_type: "worker".to_owned(),
            title: String::new(),
            url: String::new(),
            attached: false,
            parent_id: None,
            opener_id: None,
            browser_context_id: None,
            subtype: None,
        };
        let context = ContextSnapshot {
            agent_instance_id: "ignored".to_owned(),
            id: "workers".to_owned(),
            display_name: "Workers".to_owned(),
            revision: 4,
            connections: vec![ConnectionSnapshot {
                id: "runtime".to_owned(),
                configuration: ConnectionConfiguration::NodeInspector {
                    endpoint: "ws://ignored".to_owned(),
                },
                generation: 2,
                status: ConnectionStatus::Connecting,
                targets: vec![target.clone()],
            }],
            target_forest: vec![],
            breakpoints: vec![BreakpointSnapshot {
                id: "load".to_owned(),
                source_path: "worker.ts".to_owned(),
                line: 1,
                column: 1,
                status: BreakpointStatus::Pending,
                enabled: true,
                condition: None,
                target_selector: Some("worker".to_owned()),
                pending_reason: None,
                targets: vec![],
                applications: vec![],
            }],
        };
        assert_eq!(
            render(
                &[ContextView {
                    snapshot: context,
                    debuggers: BTreeMap::new(),
                }],
                true,
            ),
            concat!(
                "jsdbg daemon view — all contexts\n",
                "breakpoints per target: i=installed p=pending f=failed\n",
                "Context workers \"Workers\" rev=4 connections=1 breakpoints=1\n",
                "  Connection runtime connecting gen=2 targets=1 kind=node-inspector\n",
                "    target=runtime/worker-1 [worker] \"(untitled)\" unobserved debugger=detached bp=i0/p1/f0 gen=2 lifecycle=observed\n",
            )
        );
    }

    fn target_breakpoint(id: &str, status: TargetBreakpointStatus) -> TargetBreakpointSnapshot {
        TargetBreakpointSnapshot {
            id: id.to_owned(),
            source_url: "app.ts".to_owned(),
            line: 1,
            column: 1,
            status,
            source: None,
            assessments: vec![],
            applications: vec![],
        }
    }
}
