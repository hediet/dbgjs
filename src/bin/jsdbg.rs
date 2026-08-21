use std::env;
use std::fs;
use std::io;
use std::io::Write;
use std::path::Path;

use atomic_write_file::AtomicWriteFile;
use cdp_client::local_rpc::{connect_existing, default_state_file, ensure_service};
use cdp_client::service_api::{
    ConnectionConfiguration, DebuggerServiceApiClient, EvaluationSnapshot, PlaywrightChannel,
    StepKind, TargetDebuggerPhase, TargetDebuggerSnapshot, TargetWaitPredicate,
};
use serde::{Deserialize, Serialize};

#[path = "jsdbg/output.rs"]
mod output;

use output::OutputFormat;

#[tokio::main]
async fn main() {
    if let Err(error) = run().await {
        eprintln!("jsdbg: {error}");
        std::process::exit(1);
    }
}

async fn run() -> Result<(), Box<dyn std::error::Error>> {
    let mut arguments = env::args().skip(1).collect::<Vec<_>>();
    let output = OutputFormat::from_arguments(&mut arguments);
    let state_file = default_state_file();
    let selection_file = state_file.with_extension("selection.json");
    match arguments.as_slice() {
        [set, workspace, context_id] if set == "set" && workspace == "workspace" => {
            let client = ensure_service(&state_file).await?;
            rpc(client.get_context(context_id.clone()).await)?;
            let mut selection = load_selection(&selection_file)?;
            if selection.workspace.as_deref() != Some(context_id) {
                selection.target = None;
                selection.watches.clear();
            }
            selection.workspace = Some(context_id.clone());
            write_selection(&selection_file, &selection)?;
            println!("Workspace: {context_id}");
        }
        [set, target, selector] if set == "set" && target == "target" => {
            let client = ensure_service(&state_file).await?;
            let mut selection = load_selection(&selection_file)?;
            selection.target = Some(selector.clone());
            let scope = resolve_scope(&client, &selection).await?;
            rpc(client
                .get_target(scope.context, scope.connection, selector.clone())
                .await)?;
            write_selection(&selection_file, &selection)?;
            println!("Target: {selector}");
        }
        [target, show] if target == "target" && show == "show" => {
            let client = ensure_service(&state_file).await?;
            let selection = load_selection(&selection_file)?;
            let scope = resolve_scope(&client, &selection).await?;
            let snapshot = rpc(client
                .get_target(
                    scope.context.clone(),
                    scope.connection.clone(),
                    scope.target.clone(),
                )
                .await)?;
            print_target_with_watches(&output, &client, &selection, &scope, &snapshot).await?;
        }
        [target, step, kind, options @ ..] if target == "target" && step == "step" => {
            let client = ensure_service(&state_file).await?;
            let selection = load_selection(&selection_file)?;
            let scope = resolve_scope(&client, &selection).await?;
            let pause_epoch = resolve_pause_epoch(
                &client,
                &scope.context,
                &scope.connection,
                &scope.target,
                options,
            )
            .await?;
            let snapshot = rpc(client
                .step_target(
                    scope.context.clone(),
                    scope.connection.clone(),
                    scope.target.clone(),
                    pause_epoch,
                    parse_step_kind(kind)?,
                )
                .await)?;
            print_target_with_watches(&output, &client, &selection, &scope, &snapshot).await?;
        }
        [target, resume, options @ ..] if target == "target" && resume == "resume" => {
            let client = ensure_service(&state_file).await?;
            let selection = load_selection(&selection_file)?;
            let scope = resolve_scope(&client, &selection).await?;
            let pause_epoch = resolve_pause_epoch(
                &client,
                &scope.context,
                &scope.connection,
                &scope.target,
                options,
            )
            .await?;
            let snapshot = rpc(client
                .resume_target(
                    scope.context.clone(),
                    scope.connection.clone(),
                    scope.target.clone(),
                    pause_epoch,
                )
                .await)?;
            print_target_with_watches(&output, &client, &selection, &scope, &snapshot).await?;
        }
        [target, eval, expression] if target == "target" && eval == "eval" => {
            let client = ensure_service(&state_file).await?;
            let selection = load_selection(&selection_file)?;
            let scope = resolve_scope(&client, &selection).await?;
            let snapshot = rpc(client
                .get_target(
                    scope.context.clone(),
                    scope.connection.clone(),
                    scope.target.clone(),
                )
                .await)?;
            output.print(&rpc(client
                .evaluate_target(
                    scope.context,
                    scope.connection,
                    scope.target,
                    pause_epoch(&snapshot),
                    0,
                    expression.clone(),
                )
                .await)?)?;
        }
        [target, click, selector] if target == "target" && click == "click" => {
            let client = ensure_service(&state_file).await?;
            let selection = load_selection(&selection_file)?;
            let scope = resolve_scope(&client, &selection).await?;
            rpc(client
                .click_target(
                    scope.context,
                    scope.connection,
                    scope.target,
                    selector.clone(),
                )
                .await)?;
            println!("Clicked {selector}");
        }
        [target, key, chord] if target == "target" && key == "key" => {
            let client = ensure_service(&state_file).await?;
            let scope = resolve_scope(&client, &load_selection(&selection_file)?).await?;
            rpc(client
                .key_target(scope.context, scope.connection, scope.target, chord.clone())
                .await)?;
            println!("Pressed {chord}");
        }
        [target, type_text, text] if target == "target" && type_text == "type" => {
            let client = ensure_service(&state_file).await?;
            let scope = resolve_scope(&client, &load_selection(&selection_file)?).await?;
            rpc(client
                .type_target(scope.context, scope.connection, scope.target, text.clone())
                .await)?;
            println!("Typed {text:?}");
        }
        [
            target,
            click,
            context_id,
            connection_id,
            target_id,
            selector,
        ] if target == "target" && click == "click" => {
            let client = ensure_service(&state_file).await?;
            rpc(client
                .click_target(
                    context_id.clone(),
                    connection_id.clone(),
                    target_id.clone(),
                    selector.clone(),
                )
                .await)?;
            println!("Clicked {selector}");
        }
        [coverage, start] if coverage == "coverage" && start == "start" => {
            let client = ensure_service(&state_file).await?;
            let scope = resolve_scope(&client, &load_selection(&selection_file)?).await?;
            rpc(client
                .start_coverage(scope.context, scope.connection, scope.target)
                .await)?;
            println!("Coverage recording started.");
        }
        [coverage, take]
            if coverage == "coverage" && matches!(take.as_str(), "take" | "capture") =>
        {
            let client = ensure_service(&state_file).await?;
            let scope = resolve_scope(&client, &load_selection(&selection_file)?).await?;
            output.print(&rpc(client
                .take_coverage(scope.context, scope.connection, scope.target, None, None)
                .await)?)?;
        }
        [coverage, capture, id, capture_id]
            if coverage == "coverage" && capture == "capture" && id == "--id" =>
        {
            let client = ensure_service(&state_file).await?;
            let scope = resolve_scope(&client, &load_selection(&selection_file)?).await?;
            let snapshot = rpc(client
                .take_coverage(
                    scope.context,
                    scope.connection,
                    scope.target,
                    Some(capture_id.clone()),
                    None,
                )
                .await)?;
            output.print_coverage_capture(&snapshot, capture_id)?;
        }
        [coverage, capture, exclude, capture_id]
            if coverage == "coverage" && capture == "capture" && exclude == "--exclude" =>
        {
            let client = ensure_service(&state_file).await?;
            let scope = resolve_scope(&client, &load_selection(&selection_file)?).await?;
            output.print(&rpc(client
                .take_coverage(
                    scope.context,
                    scope.connection,
                    scope.target,
                    None,
                    Some(capture_id.clone()),
                )
                .await)?)?;
        }
        [coverage, stop] if coverage == "coverage" && stop == "stop" => {
            let client = ensure_service(&state_file).await?;
            let scope = resolve_scope(&client, &load_selection(&selection_file)?).await?;
            output.print(&rpc(client
                .stop_coverage(scope.context, scope.connection, scope.target, None)
                .await)?)?;
        }
        [coverage, stop, exclude, capture_id]
            if coverage == "coverage" && stop == "stop" && exclude == "--exclude" =>
        {
            let client = ensure_service(&state_file).await?;
            let scope = resolve_scope(&client, &load_selection(&selection_file)?).await?;
            output.print(&rpc(client
                .stop_coverage(
                    scope.context,
                    scope.connection,
                    scope.target,
                    Some(capture_id.clone()),
                )
                .await)?)?;
        }
        [coverage, show] if coverage == "coverage" && show == "show" => {
            let client = ensure_service(&state_file).await?;
            let scope = resolve_scope(&client, &load_selection(&selection_file)?).await?;
            output.print(&rpc(client
                .get_coverage(
                    scope.context,
                    scope.connection,
                    scope.target,
                    ".".to_owned(),
                )
                .await)?)?;
        }
        [coverage, show, capture_id] if coverage == "coverage" && show == "show" => {
            let client = ensure_service(&state_file).await?;
            let scope = resolve_scope(&client, &load_selection(&selection_file)?).await?;
            output.print(&rpc(client
                .get_coverage(
                    scope.context,
                    scope.connection,
                    scope.target,
                    capture_id.clone(),
                )
                .await)?)?;
        }
        [coverage, operation, context_id, connection_id, target_id]
            if coverage == "coverage"
                && matches!(operation.as_str(), "start" | "take" | "capture" | "stop") =>
        {
            let client = ensure_service(&state_file).await?;
            match operation.as_str() {
                "start" => {
                    rpc(client
                        .start_coverage(
                            context_id.clone(),
                            connection_id.clone(),
                            target_id.clone(),
                        )
                        .await)?;
                    println!("Coverage recording started.");
                }
                "take" | "capture" => output.print(&rpc(client
                    .take_coverage(
                        context_id.clone(),
                        connection_id.clone(),
                        target_id.clone(),
                        None,
                        None,
                    )
                    .await)?)?,
                "stop" => output.print(&rpc(client
                    .stop_coverage(
                        context_id.clone(),
                        connection_id.clone(),
                        target_id.clone(),
                        None,
                    )
                    .await)?)?,
                _ => unreachable!(),
            }
        }
        [target, watch, expression] if target == "target" && watch == "watch" => {
            let client = ensure_service(&state_file).await?;
            let mut selection = load_selection(&selection_file)?;
            let scope = resolve_scope(&client, &selection).await?;
            if !selection.watches.contains(expression) {
                selection.watches.push(expression.clone());
                write_selection(&selection_file, &selection)?;
            }
            let snapshot = rpc(client
                .get_target(
                    scope.context.clone(),
                    scope.connection.clone(),
                    scope.target.clone(),
                )
                .await)?;
            print_target_with_watches(&output, &client, &selection, &scope, &snapshot).await?;
        }
        [service, status] if service == "service" && status == "status" => {
            let client = connect_existing(&state_file).await?;
            output.print(&rpc(client.service_info().await)?)?;
        }
        [service, stop] if service == "service" && stop == "stop" => {
            let client = connect_existing(&state_file).await?;
            output.print(&rpc(client.shutdown().await)?)?;
        }
        [context, list] if context == "context" && list == "list" => {
            let client = ensure_service(&state_file).await?;
            output.print(&rpc(client.list_contexts().await)?)?;
        }
        [context, create, context_id] if context == "context" && create == "create" => {
            let client = ensure_service(&state_file).await?;
            output.print(&rpc(client.put_context(context_id.clone(), None).await)?)?;
        }
        [context, create, context_id, display_name]
            if context == "context" && create == "create" =>
        {
            let client = ensure_service(&state_file).await?;
            output.print(&rpc(client
                .put_context(context_id.clone(), Some(display_name.clone()))
                .await)?)?;
        }
        [context, show, context_id] if context == "context" && show == "show" => {
            let client = ensure_service(&state_file).await?;
            output.print(&rpc(client.get_context(context_id.clone()).await)?)?;
        }
        [
            connection,
            add,
            context_id,
            connection_id,
            endpoint,
            connect_now,
        ] if connection == "connection" && add == "add" && connect_now == "--connect" => {
            add_connection(
                context_id,
                connection_id,
                ConnectionConfiguration::DirectCdp {
                    endpoint: endpoint.clone(),
                },
                true,
                &state_file,
                output,
            )
            .await?;
        }
        [connection, add, context_id, connection_id, endpoint]
            if connection == "connection" && add == "add" =>
        {
            add_connection(
                context_id,
                connection_id,
                ConnectionConfiguration::DirectCdp {
                    endpoint: endpoint.clone(),
                },
                false,
                &state_file,
                output,
            )
            .await?;
        }
        [
            connection,
            add,
            context_id,
            connection_id,
            playwright,
            url,
            options @ ..,
        ] if connection == "connection" && add == "add" && playwright == "--playwright" => {
            let options = parse_playwright_options(options)?;
            add_connection(
                context_id,
                connection_id,
                ConnectionConfiguration::Playwright {
                    url: url.clone(),
                    channel: options.channel,
                    headless: options.headless,
                    ignore_https_errors: options.ignore_https_errors,
                },
                options.connect,
                &state_file,
                output,
            )
            .await?;
        }
        [connection, connect, context_id, connection_id]
            if connection == "connection" && connect == "connect" =>
        {
            let client = ensure_service(&state_file).await?;
            output.print(&rpc(client
                .connect_connection(context_id.clone(), connection_id.clone())
                .await)?)?;
        }
        [connection, disconnect, context_id, connection_id]
            if connection == "connection" && disconnect == "disconnect" =>
        {
            let client = ensure_service(&state_file).await?;
            output.print(&rpc(client
                .disconnect_connection(context_id.clone(), connection_id.clone())
                .await)?)?;
        }
        [
            breakpoint,
            set,
            context_id,
            breakpoint_id,
            source_path,
            line,
        ] if breakpoint == "breakpoint" && set == "set" => {
            put_breakpoint(
                context_id,
                breakpoint_id,
                source_path,
                line,
                "1",
                &state_file,
                output,
            )
            .await?;
        }
        [
            breakpoint,
            set,
            context_id,
            breakpoint_id,
            source_path,
            line,
            column,
        ] if breakpoint == "breakpoint" && set == "set" => {
            put_breakpoint(
                context_id,
                breakpoint_id,
                source_path,
                line,
                column,
                &state_file,
                output,
            )
            .await?;
        }
        [target, attach, context_id, connection_id, target_id]
            if target == "target" && attach == "attach" =>
        {
            let client = ensure_service(&state_file).await?;
            let snapshot = rpc(client
                .attach_target(context_id.clone(), connection_id.clone(), target_id.clone())
                .await)?;
            output.print_target(&snapshot, target_id)?;
        }
        [target, show, context_id, connection_id, target_id]
            if target == "target" && show == "show" =>
        {
            let client = ensure_service(&state_file).await?;
            let snapshot = rpc(client
                .get_target(context_id.clone(), connection_id.clone(), target_id.clone())
                .await)?;
            output.print_target(&snapshot, target_id)?;
        }
        [
            target,
            wait,
            context_id,
            connection_id,
            target_id,
            installed,
            breakpoint_id,
        ] if target == "target" && wait == "wait" && installed == "breakpoint-installed" => {
            wait_target(
                context_id,
                connection_id,
                target_id,
                TargetWaitPredicate::BreakpointInstalled {
                    breakpoint_id: breakpoint_id.clone(),
                },
                "30000",
                &state_file,
                output,
            )
            .await?;
        }
        [
            target,
            wait,
            context_id,
            connection_id,
            target_id,
            installed,
            breakpoint_id,
            timeout_ms,
        ] if target == "target" && wait == "wait" && installed == "breakpoint-installed" => {
            wait_target(
                context_id,
                connection_id,
                target_id,
                TargetWaitPredicate::BreakpointInstalled {
                    breakpoint_id: breakpoint_id.clone(),
                },
                timeout_ms,
                &state_file,
                output,
            )
            .await?;
        }
        [
            target,
            wait,
            context_id,
            connection_id,
            target_id,
            paused,
            after_epoch,
        ] if target == "target" && wait == "wait" && paused == "paused" => {
            wait_target(
                context_id,
                connection_id,
                target_id,
                TargetWaitPredicate::Paused {
                    after_epoch: parse_u64("pause epoch", after_epoch)?,
                },
                "30000",
                &state_file,
                output,
            )
            .await?;
        }
        [
            target,
            wait,
            context_id,
            connection_id,
            target_id,
            paused,
            after_epoch,
            timeout_ms,
        ] if target == "target" && wait == "wait" && paused == "paused" => {
            wait_target(
                context_id,
                connection_id,
                target_id,
                TargetWaitPredicate::Paused {
                    after_epoch: parse_u64("pause epoch", after_epoch)?,
                },
                timeout_ms,
                &state_file,
                output,
            )
            .await?;
        }
        [target, wait, context_id, connection_id, target_id, running]
            if target == "target" && wait == "wait" && running == "running" =>
        {
            wait_target(
                context_id,
                connection_id,
                target_id,
                TargetWaitPredicate::Running,
                "30000",
                &state_file,
                output,
            )
            .await?;
        }
        [
            target,
            resume,
            context_id,
            connection_id,
            target_id,
            options @ ..,
        ] if target == "target" && resume == "resume" => {
            let client = ensure_service(&state_file).await?;
            let pause_epoch =
                resolve_pause_epoch(&client, context_id, connection_id, target_id, options).await?;
            let snapshot = rpc(client
                .resume_target(
                    context_id.clone(),
                    connection_id.clone(),
                    target_id.clone(),
                    pause_epoch,
                )
                .await)?;
            output.print_target(&snapshot, target_id)?;
        }
        [
            target,
            step,
            context_id,
            connection_id,
            target_id,
            kind,
            options @ ..,
        ] if target == "target" && step == "step" => {
            let client = ensure_service(&state_file).await?;
            let pause_epoch =
                resolve_pause_epoch(&client, context_id, connection_id, target_id, options).await?;
            let snapshot = rpc(client
                .step_target(
                    context_id.clone(),
                    connection_id.clone(),
                    target_id.clone(),
                    pause_epoch,
                    parse_step_kind(kind)?,
                )
                .await)?;
            output.print_target(&snapshot, target_id)?;
        }
        [
            target,
            operation,
            context_id,
            connection_id,
            target_id,
            expression,
        ] if target == "target" && operation == "eval" => {
            let client = ensure_service(&state_file).await?;
            let snapshot = rpc(client
                .get_target(context_id.clone(), connection_id.clone(), target_id.clone())
                .await)?;
            output.print(&rpc(client
                .evaluate_target(
                    context_id.clone(),
                    connection_id.clone(),
                    target_id.clone(),
                    pause_epoch(&snapshot),
                    0,
                    expression.clone(),
                )
                .await)?)?;
        }
        [
            target,
            logpoint,
            context_id,
            connection_id,
            target_id,
            logpoint_id,
            source_url,
            line,
            column,
            expression,
        ] if target == "target" && logpoint == "logpoint" => {
            let client = ensure_service(&state_file).await?;
            let snapshot = rpc(client
                .set_logpoint(
                    context_id.clone(),
                    connection_id.clone(),
                    target_id.clone(),
                    logpoint_id.clone(),
                    source_url.clone(),
                    line.parse()?,
                    column.parse()?,
                    expression.clone(),
                )
                .await)?;
            output.print_target(&snapshot, target_id)?;
        }
        _ => {
            return Err(usage().into());
        }
    }
    Ok(())
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
struct CliSelection {
    workspace: Option<String>,
    target: Option<String>,
    #[serde(default)]
    watches: Vec<String>,
}

struct ResolvedScope {
    context: String,
    connection: String,
    target: String,
}

fn load_selection(path: &Path) -> Result<CliSelection, Box<dyn std::error::Error>> {
    match fs::read(path) {
        Ok(bytes) => Ok(serde_json::from_slice(&bytes)?),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(CliSelection::default()),
        Err(error) => Err(error.into()),
    }
}

fn write_selection(
    path: &Path,
    selection: &CliSelection,
) -> Result<(), Box<dyn std::error::Error>> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    let mut file = AtomicWriteFile::open(path)?;
    file.write_all(&serde_json::to_vec_pretty(selection)?)?;
    file.commit()?;
    Ok(())
}

async fn resolve_scope(
    client: &DebuggerServiceApiClient,
    selection: &CliSelection,
) -> Result<ResolvedScope, Box<dyn std::error::Error>> {
    let context = match &selection.workspace {
        Some(context) => context.clone(),
        None => {
            let contexts = rpc(client.list_contexts().await)?;
            match contexts.as_slice() {
                [context] => context.id.clone(),
                [] => return Err("no debugger workspace exists; run `jsdbg set workspace`".into()),
                _ => {
                    return Err(
                        "multiple workspaces exist; run `jsdbg set workspace <context>`".into(),
                    );
                }
            }
        }
    };
    let snapshot = rpc(client.get_context(context.clone()).await)?;
    let connected = snapshot
        .connections
        .iter()
        .filter(|connection| {
            matches!(
                connection.status,
                cdp_client::service_api::ConnectionStatus::Connected { .. }
            )
        })
        .collect::<Vec<_>>();
    let connection = match connected.as_slice() {
        [connection] => connection.id.clone(),
        [] => return Err(format!("workspace '{context}' has no connected connection").into()),
        _ => {
            return Err(format!(
                "workspace '{context}' has multiple connected connections; use an explicit command"
            )
            .into());
        }
    };
    let connection_snapshot = connected[0];
    let target = match &selection.target {
        Some(target) => target.clone(),
        None => match connection_snapshot.targets.as_slice() {
            [target] => target.target_type.clone(),
            [] => return Err("the connection has no targets".into()),
            _ => {
                return Err(
                    "the connection has multiple targets; run `jsdbg set target <selector>`".into(),
                );
            }
        },
    };
    Ok(ResolvedScope {
        context,
        connection,
        target,
    })
}

async fn print_target_with_watches(
    output: &OutputFormat,
    client: &DebuggerServiceApiClient,
    selection: &CliSelection,
    scope: &ResolvedScope,
    snapshot: &TargetDebuggerSnapshot,
) -> Result<(), Box<dyn std::error::Error>> {
    let evaluations = evaluate_watches(client, selection, scope, snapshot).await?;
    output.print_target_with_watches(snapshot, &scope.target, &evaluations)?;
    Ok(())
}

async fn evaluate_watches(
    client: &DebuggerServiceApiClient,
    selection: &CliSelection,
    scope: &ResolvedScope,
    snapshot: &TargetDebuggerSnapshot,
) -> Result<Vec<EvaluationSnapshot>, Box<dyn std::error::Error>> {
    let Some(epoch) = pause_epoch(snapshot) else {
        return Ok(Vec::new());
    };
    let mut evaluations = Vec::new();
    for expression in &selection.watches {
        evaluations.push(
            match client
                .evaluate_target(
                    scope.context.clone(),
                    scope.connection.clone(),
                    scope.target.clone(),
                    Some(epoch),
                    0,
                    expression.clone(),
                )
                .await
            {
                Ok(evaluation) => evaluation,
                Err(_) => EvaluationSnapshot {
                    expression: expression.clone(),
                    kind: "error".to_owned(),
                    value: None,
                    unserializable_value: None,
                    description: Some("unavailable in this frame".to_owned()),
                },
            },
        );
    }
    Ok(evaluations)
}

fn pause_epoch(snapshot: &TargetDebuggerSnapshot) -> Option<u64> {
    match snapshot.phase {
        TargetDebuggerPhase::Paused { epoch } => Some(epoch),
        _ => None,
    }
}

async fn resolve_pause_epoch(
    client: &cdp_client::service_api::DebuggerServiceApiClient,
    context_id: &str,
    connection_id: &str,
    target_id: &str,
    options: &[String],
) -> Result<u64, Box<dyn std::error::Error>> {
    match options {
        [] => {
            let snapshot = rpc(client
                .get_target(
                    context_id.to_owned(),
                    connection_id.to_owned(),
                    target_id.to_owned(),
                )
                .await)?;
            Ok(current_pause_epoch(&snapshot)?)
        }
        [epoch, value] if epoch == "--epoch" => Ok(parse_u64("pause epoch", value)?),
        _ => Err("expected no options or --epoch <epoch>".into()),
    }
}

fn current_pause_epoch(
    snapshot: &cdp_client::service_api::TargetDebuggerSnapshot,
) -> Result<u64, io::Error> {
    match snapshot.phase {
        TargetDebuggerPhase::Paused { epoch } => Ok(epoch),
        _ => Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "target is not currently paused",
        )),
    }
}

fn parse_step_kind(value: &str) -> Result<StepKind, io::Error> {
    match value {
        "into" => Ok(StepKind::Into),
        "over" => Ok(StepKind::Over),
        "out" => Ok(StepKind::Out),
        _ => Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "step kind must be into, over, or out",
        )),
    }
}

async fn put_breakpoint(
    context_id: &str,
    breakpoint_id: &str,
    source_path: &str,
    line: &str,
    column: &str,
    state_file: &std::path::Path,
    output: OutputFormat,
) -> Result<(), Box<dyn std::error::Error>> {
    let line = line.parse::<u32>()?;
    let column = column.parse::<u32>()?;
    let client = ensure_service(state_file).await?;
    let context = rpc(client
        .put_breakpoint(
            context_id.to_owned(),
            breakpoint_id.to_owned(),
            source_path.to_owned(),
            line,
            column,
        )
        .await)?;
    let targets = context
        .connections
        .iter()
        .filter(|connection| {
            matches!(
                connection.status,
                cdp_client::service_api::ConnectionStatus::Connected { .. }
            )
        })
        .flat_map(|connection| {
            connection
                .targets
                .iter()
                .map(move |target| (connection.id.clone(), target))
        })
        .collect::<Vec<_>>();
    if let [(connection, target)] = targets.as_slice() {
        match client
            .get_target(
                context_id.to_owned(),
                connection.clone(),
                target.target_id.clone(),
            )
            .await
        {
            Ok(snapshot) => output.print_target(&snapshot, &target.target_type)?,
            Err(_) => output.print(&context)?,
        }
    } else {
        output.print(&context)?;
    }
    Ok(())
}

async fn add_connection(
    context_id: &str,
    connection_id: &str,
    configuration: ConnectionConfiguration,
    connect_now: bool,
    state_file: &std::path::Path,
    output: OutputFormat,
) -> Result<(), Box<dyn std::error::Error>> {
    let client = ensure_service(state_file).await?;
    let configured = rpc(client
        .put_connection(
            context_id.to_owned(),
            connection_id.to_owned(),
            configuration,
        )
        .await)?;
    if connect_now {
        output.print(&rpc(client
            .connect_connection(context_id.to_owned(), connection_id.to_owned())
            .await)?)?;
    } else {
        output.print(&configured)?;
    }
    Ok(())
}

struct PlaywrightOptions {
    channel: PlaywrightChannel,
    headless: bool,
    connect: bool,
    ignore_https_errors: bool,
}

fn parse_playwright_options(options: &[String]) -> Result<PlaywrightOptions, io::Error> {
    let mut parsed = PlaywrightOptions {
        channel: PlaywrightChannel::Bundled,
        headless: true,
        connect: false,
        ignore_https_errors: false,
    };
    let mut index = 0;
    while index < options.len() {
        match options[index].as_str() {
            "--connect" => parsed.connect = true,
            "--headed" => parsed.headless = false,
            "--ignore-https-errors" => parsed.ignore_https_errors = true,
            "--channel" => {
                index += 1;
                let channel = options.get(index).ok_or_else(|| {
                    io::Error::new(io::ErrorKind::InvalidInput, "--channel requires a value")
                })?;
                parsed.channel = parse_playwright_channel(channel)?;
            }
            option => {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidInput,
                    format!("unknown Playwright connection option '{option}'"),
                ));
            }
        }
        index += 1;
    }
    Ok(parsed)
}

fn parse_playwright_channel(value: &str) -> Result<PlaywrightChannel, io::Error> {
    match value {
        "bundled" => Ok(PlaywrightChannel::Bundled),
        "chrome" => Ok(PlaywrightChannel::Chrome),
        "chrome-beta" => Ok(PlaywrightChannel::ChromeBeta),
        "chrome-dev" => Ok(PlaywrightChannel::ChromeDev),
        "chrome-canary" => Ok(PlaywrightChannel::ChromeCanary),
        "msedge" => Ok(PlaywrightChannel::Msedge),
        "msedge-beta" => Ok(PlaywrightChannel::MsedgeBeta),
        "msedge-dev" => Ok(PlaywrightChannel::MsedgeDev),
        "msedge-canary" => Ok(PlaywrightChannel::MsedgeCanary),
        _ => Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            format!("unsupported Playwright channel '{value}'"),
        )),
    }
}

async fn wait_target(
    context_id: &str,
    connection_id: &str,
    target_id: &str,
    predicate: TargetWaitPredicate,
    timeout_ms: &str,
    state_file: &std::path::Path,
    output: OutputFormat,
) -> Result<(), Box<dyn std::error::Error>> {
    let timeout_ms = parse_u64("timeout", timeout_ms)?;
    let client = ensure_service(state_file).await?;
    let snapshot = rpc(client
        .wait_target(
            context_id.to_owned(),
            connection_id.to_owned(),
            target_id.to_owned(),
            predicate,
            timeout_ms,
        )
        .await)?;
    output.print_target(&snapshot, target_id)?;
    Ok(())
}

fn parse_u64(name: &str, value: &str) -> Result<u64, io::Error> {
    value.parse().map_err(|error| {
        io::Error::new(
            io::ErrorKind::InvalidInput,
            format!("invalid {name} '{value}': {error}"),
        )
    })
}

fn rpc<T>(result: Result<T, hubrpc::prelude::JsonRpcError>) -> Result<T, io::Error> {
    result.map_err(|error| io::Error::other(format!("{error:?}")))
}

fn usage() -> &'static str {
    "usage: jsdbg [--json] <command>

commands:
  jsdbg service status|stop
  jsdbg context list
  jsdbg context create <context-id> [display-name]
  jsdbg context show <context-id>
  jsdbg set workspace <context-id>
  jsdbg set target <selector>
  jsdbg connection add <context-id> <connection-id> <ws-endpoint> [--connect]
  jsdbg connection add <context-id> <connection-id> --playwright <url> [--channel <channel>] [--headed] [--ignore-https-errors] [--connect]
  jsdbg connection connect|disconnect <context-id> <connection-id>
  jsdbg breakpoint set <context-id> <breakpoint-id> <source-url> <line> [column]
  jsdbg target show
  jsdbg target attach|show <context-id> <connection-id> <target>
  jsdbg target wait <context-id> <connection-id> <target> breakpoint-installed <breakpoint-id> [timeout-ms]
  jsdbg target wait <context-id> <connection-id> <target> paused <after-epoch> [timeout-ms]
  jsdbg target wait <context-id> <connection-id> <target> running
  jsdbg target resume [--epoch <epoch>]
  jsdbg target step into|over|out [--epoch <epoch>]
  jsdbg target eval|watch <expression>
  jsdbg target click <css-selector>
  jsdbg target key <chord>
  jsdbg target type <text>
  jsdbg target click <context-id> <connection-id> <target> <css-selector>
  jsdbg coverage start
  jsdbg coverage capture [--id <name>]
  jsdbg coverage stop [--exclude <name>]
  jsdbg coverage show [<name>]
  jsdbg coverage start|capture|stop <context-id> <connection-id> <target>
  jsdbg target resume <context-id> <connection-id> <target> [--epoch <epoch>]
  jsdbg target step <context-id> <connection-id> <target> into|over|out [--epoch <epoch>]
  jsdbg target eval <context-id> <connection-id> <target> <expression>
  jsdbg target logpoint <context-id> <connection-id> <target> <id> <source> <line> <column> <expression>"
}
