use std::env;
use std::fs;
use std::io;
use std::io::Write;
use std::path::Path;

use atomic_write_file::AtomicWriteFile;
use cdp_client::local_rpc::{connect_existing, default_state_file, ensure_service};
use cdp_client::service_api::{
    BreakpointSpec, ConnectionConfiguration, DebuggerServiceApiClient, EvaluationSnapshot,
    HeapCaptureResult, HeapSnapshotProgress, LogpointSpec, MutationOptions, ObservationCursor,
    ObservationResult, PlaywrightChannel, StepKind, TargetDebuggerPhase, TargetDebuggerSnapshot,
    TargetWaitPredicate,
};
use serde::{Deserialize, Serialize};

#[path = "jsdbg/bounded_tree.rs"]
mod bounded_tree;
#[path = "jsdbg/output.rs"]
mod output;

use output::{CoverageOutputOptions, HeapClassOutputOptions, OutputFormat};

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
                selection.log_cursor = 0;
                selection.log_scope = None;
            }
            selection.workspace = Some(context_id.clone());
            write_selection(&selection_file, &selection)?;
            println!("Workspace: {context_id}");
        }
        [set, target, selector] if set == "set" && target == "target" => {
            let client = ensure_service(&state_file).await?;
            let mut selection = load_selection(&selection_file)?;
            if selection.target.as_deref() != Some(selector) {
                selection.log_cursor = 0;
                selection.log_scope = None;
            }
            selection.target = Some(selector.clone());
            let scope = resolve_scope(&client, &selection).await?;
            let snapshot = rpc(client
                .get_target(
                    scope.context.clone(),
                    scope.connection.clone(),
                    selector.clone(),
                )
                .await)?;
            selection.log_cursor = snapshot.logs.last().map_or(0, |message| message.index);
            selection.log_scope = Some(log_scope(&scope, &snapshot));
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
        [log, options @ ..] if log == "log" => {
            let client = ensure_service(&state_file).await?;
            let mut selection = load_selection(&selection_file)?;
            let scope = resolve_scope(&client, &selection).await?;
            let snapshot = rpc(client
                .get_target(
                    scope.context.clone(),
                    scope.connection.clone(),
                    scope.target.clone(),
                )
                .await)?;
            let current_scope = log_scope(&scope, &snapshot);
            let persisted_cursor = if selection.log_scope.as_deref() == Some(current_scope.as_str())
            {
                selection.log_cursor
            } else {
                0
            };
            let (after, limit, explicit_after) = parse_log_options(options, persisted_cursor)?;
            let next = output.print_logs(&snapshot.logs, after, limit)?;
            if !explicit_after {
                selection.log_cursor = next;
                selection.log_scope = Some(current_scope);
                write_selection(&selection_file, &selection)?;
            }
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
        [target, logpoint, id, source, line, column, expression]
            if target == "target" && logpoint == "logpoint" =>
        {
            let client = ensure_service(&state_file).await?;
            let scope = resolve_scope(&client, &load_selection(&selection_file)?).await?;
            let snapshot = rpc(client
                .set_logpoints(
                    scope.context,
                    scope.connection,
                    scope.target.clone(),
                    vec![parse_logpoint_spec(id, source, line, column, expression)?],
                )
                .await)?;
            output.print_target_with_breakpoint_sources(
                &snapshot,
                &scope.target,
                &[format!("log:{id}")],
            )?;
        }
        [target, logpoints, specifications @ ..]
            if target == "target" && logpoints == "logpoints" =>
        {
            let logpoints = parse_logpoint_specs(specifications)?;
            let ids = logpoints
                .iter()
                .map(|logpoint| format!("log:{}", logpoint.id))
                .collect::<Vec<_>>();
            let client = ensure_service(&state_file).await?;
            let scope = resolve_scope(&client, &load_selection(&selection_file)?).await?;
            let snapshot = rpc(client
                .set_logpoints(
                    scope.context,
                    scope.connection,
                    scope.target.clone(),
                    logpoints,
                )
                .await)?;
            output.print_target_with_breakpoint_sources(&snapshot, &scope.target, &ids)?;
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
            output.print_coverage(
                &rpc(client
                    .take_coverage(
                        scope.context,
                        scope.connection,
                        scope.target,
                        None,
                        Some(capture_id.clone()),
                    )
                    .await)?,
                CoverageOutputOptions {
                    path: None,
                    all: false,
                    max_lines: 300,
                },
            )?;
        }
        [coverage, stop] if coverage == "coverage" && stop == "stop" => {
            let client = ensure_service(&state_file).await?;
            let scope = resolve_scope(&client, &load_selection(&selection_file)?).await?;
            rpc(client
                .finish_coverage(scope.context, scope.connection, scope.target, None)
                .await)?;
            output.print_coverage_stopped()?;
        }
        [coverage, stop, exclude, capture_id]
            if coverage == "coverage" && stop == "stop" && exclude == "--exclude" =>
        {
            let client = ensure_service(&state_file).await?;
            let scope = resolve_scope(&client, &load_selection(&selection_file)?).await?;
            rpc(client
                .finish_coverage(
                    scope.context,
                    scope.connection,
                    scope.target,
                    Some(capture_id.clone()),
                )
                .await)?;
            output.print_coverage_stopped()?;
        }
        [coverage, show, options @ ..] if coverage == "coverage" && show == "show" => {
            let options = parse_coverage_show_options(options)?;
            let client = ensure_service(&state_file).await?;
            let scope = resolve_scope(&client, &load_selection(&selection_file)?).await?;
            output.print_coverage(
                &rpc(client
                    .get_coverage(
                        scope.context,
                        scope.connection,
                        scope.target,
                        options.capture_id,
                        options.path.clone(),
                        options.no_cache,
                    )
                    .await)?,
                CoverageOutputOptions {
                    path: options.path.as_deref(),
                    all: options.all,
                    max_lines: options.max_lines,
                },
            )?;
        }
        [heap, capture, options @ ..] if heap == "heap" && capture == "capture" => {
            let options = parse_heap_capture_options(options)?;
            let client = ensure_service(&state_file).await?;
            let scope = resolve_scope(&client, &load_selection(&selection_file)?).await?;
            let operation = client.capture_heap_snapshot(
                scope.context.clone(),
                scope.connection.clone(),
                scope.target.clone(),
                options.capture_id,
                options.capture_numeric_value,
                options.expose_internals,
            );
            tokio::pin!(operation);
            let result = wait_for_heap_capture(&output, &client, &scope, &mut operation).await?;
            output.print(&result)?;
        }
        [heap, classes, options @ ..] if heap == "heap" && classes == "classes" => {
            let options = parse_heap_class_options(options)?;
            let client = ensure_service(&state_file).await?;
            let scope = resolve_scope(&client, &load_selection(&selection_file)?).await?;
            if options.capture {
                let operation = client.capture_heap_snapshot(
                    scope.context.clone(),
                    scope.connection.clone(),
                    scope.target.clone(),
                    Some(options.capture_id.clone()),
                    false,
                    false,
                );
                tokio::pin!(operation);
                wait_for_heap_capture(&output, &client, &scope, &mut operation).await?;
            }
            let classes = rpc(client
                .get_heap_classes(
                    scope.context,
                    scope.connection,
                    scope.target,
                    options.capture_id,
                    options.filter,
                    options.no_cache,
                )
                .await)?;
            output.print_heap_classes(
                &classes,
                HeapClassOutputOptions {
                    all: options.all,
                    max_lines: options.max_lines,
                    instances: options.instances,
                    sort_by_instances: options.sort_by_instances,
                },
            )?;
        }
        [heap, snapshot, path, options @ ..] if heap == "heap" && snapshot == "snapshot" => {
            let options = parse_heap_snapshot_options(options)?;
            let destination = absolute_path(Path::new(path))?;
            let client = ensure_service(&state_file).await?;
            let scope = resolve_scope(&client, &load_selection(&selection_file)?).await?;
            let operation = client.take_heap_snapshot(
                scope.context.clone(),
                scope.connection.clone(),
                scope.target.clone(),
                destination.to_string_lossy().into_owned(),
                options.capture_numeric_value,
                options.expose_internals,
            );
            tokio::pin!(operation);
            let mut last_progress = None::<HeapSnapshotProgress>;
            let result = loop {
                tokio::select! {
                    result = &mut operation => break rpc(result)?,
                    _ = tokio::time::sleep(std::time::Duration::from_millis(100)) => {
                        let progress = rpc(client
                            .get_heap_snapshot_progress(
                                scope.context.clone(),
                                scope.connection.clone(),
                                scope.target.clone(),
                            )
                            .await)?;
                        if let Some(progress) = progress
                            && last_progress.as_ref() != Some(&progress)
                        {
                            output.print_heap_snapshot_progress(&progress)?;
                            last_progress = Some(progress);
                        }
                    }
                }
            };
            let progress = rpc(client
                .get_heap_snapshot_progress(
                    scope.context.clone(),
                    scope.connection.clone(),
                    scope.target.clone(),
                )
                .await)?;
            if let Some(progress) = progress
                && last_progress.as_ref() != Some(&progress)
            {
                output.print_heap_snapshot_progress(&progress)?;
            }
            output.print(&result)?;
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
                "stop" => {
                    rpc(client
                        .finish_coverage(
                            context_id.clone(),
                            connection_id.clone(),
                            target_id.clone(),
                            None,
                        )
                        .await)?;
                    output.print_coverage_stopped()?;
                }
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
        [context, delete, context_id, options @ ..]
            if context == "context" && delete == "delete" =>
        {
            let client = ensure_service(&state_file).await?;
            output.print(&rpc(client
                .delete_context(context_id.clone(), parse_mutation_options(options)?)
                .await)?)?;
        }
        [state, get, context_id] if state == "state" && get == "get" => {
            let client = ensure_service(&state_file).await?;
            output.print(&rpc(client.get_context(context_id.clone()).await)?)?;
        }
        [state, watch, context_id, options @ ..] if state == "state" && watch == "watch" => {
            let client = ensure_service(&state_file).await?;
            let mut cursor = parse_observation_cursor(options)?;
            loop {
                match rpc(client
                    .observe_context(context_id.clone(), cursor.clone(), 30_000)
                    .await)?
                {
                    ObservationResult::Items { items } => {
                        for item in items {
                            cursor = ObservationCursor::After {
                                revision: item.snapshot.revision,
                            };
                            println!("{}", serde_json::to_string(&item)?);
                        }
                    }
                    gap @ ObservationResult::HistoryGap { .. } => {
                        println!("{}", serde_json::to_string(&gap)?);
                        return Err(io::Error::other("context observation history gap").into());
                    }
                }
            }
        }
        [events, context_id, after, revision]
            if events == "events" && after == "--after-revision" =>
        {
            let client = ensure_service(&state_file).await?;
            output.print(&rpc(client
                .observe_context(
                    context_id.clone(),
                    ObservationCursor::After {
                        revision: parse_u64("revision", revision)?,
                    },
                    0,
                )
                .await)?)?;
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
        [connection, delete, context_id, connection_id, options @ ..]
            if connection == "connection" && delete == "delete" =>
        {
            let client = ensure_service(&state_file).await?;
            output.print(&rpc(client
                .delete_connection(
                    context_id.clone(),
                    connection_id.clone(),
                    parse_mutation_options(options)?,
                )
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
            configure,
            context_id,
            breakpoint_id,
            source_path,
            line,
            column,
            options @ ..,
        ] if breakpoint == "breakpoint" && configure == "configure" => {
            let (specification, mutation) =
                parse_breakpoint_spec(source_path, line, column, options)?;
            let client = ensure_service(&state_file).await?;
            output.print(&rpc(client
                .put_breakpoint_spec(
                    context_id.clone(),
                    breakpoint_id.clone(),
                    specification,
                    mutation,
                )
                .await)?)?;
        }
        [breakpoint, delete, context_id, breakpoint_id, options @ ..]
            if breakpoint == "breakpoint" && delete == "delete" =>
        {
            let client = ensure_service(&state_file).await?;
            output.print(&rpc(client
                .delete_breakpoint(
                    context_id.clone(),
                    breakpoint_id.clone(),
                    parse_mutation_options(options)?,
                )
                .await)?)?;
        }
        [source, list, context_id] if source == "source" && list == "list" => {
            let client = ensure_service(&state_file).await?;
            output.print(&rpc(client.list_sources(context_id.clone(), None).await)?)?;
        }
        [source, resolve, context_id, path] if source == "source" && resolve == "resolve" => {
            let client = ensure_service(&state_file).await?;
            output.print(&rpc(client
                .list_sources(context_id.clone(), Some(path.clone()))
                .await)?)?;
        }
        [source, endpoints, context_id, path] if source == "source" && endpoints == "endpoints" => {
            let client = ensure_service(&state_file).await?;
            output.print(&rpc(client
                .list_sources(context_id.clone(), Some(path.clone()))
                .await)?)?;
        }
        [source, show, context_id, path] if source == "source" && show == "show" => {
            let client = ensure_service(&state_file).await?;
            output.print(&rpc(client
                .show_source(context_id.clone(), path.clone())
                .await)?)?;
        }
        [source, grep, context_id, pattern] if source == "source" && grep == "grep" => {
            let client = ensure_service(&state_file).await?;
            output.print(&rpc(client
                .grep_sources(context_id.clone(), pattern.clone())
                .await)?)?;
        }
        [source, map, context_id, path, line, column] if source == "source" && map == "map" => {
            let client = ensure_service(&state_file).await?;
            output.print(&rpc(client
                .map_source(
                    context_id.clone(),
                    path.clone(),
                    parse_u64("line", line)?.try_into().map_err(|_| {
                        io::Error::new(io::ErrorKind::InvalidInput, "line exceeds u32")
                    })?,
                    parse_u64("column", column)?.try_into().map_err(|_| {
                        io::Error::new(io::ErrorKind::InvalidInput, "column exceeds u32")
                    })?,
                )
                .await)?)?;
        }
        [source, cache, evict, context_id]
            if source == "source" && cache == "cache" && evict == "evict" =>
        {
            let client = ensure_service(&state_file).await?;
            output.print(&rpc(client.evict_source_caches(context_id.clone()).await)?)?;
        }
        [source, export, context_id, destination] if source == "source" && export == "export" => {
            let client = ensure_service(&state_file).await?;
            output.print(&rpc(client
                .export_sources(context_id.clone(), destination.clone())
                .await)?)?;
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
            output.print_target_with_breakpoint_sources(
                &snapshot,
                target_id,
                &[format!("log:{logpoint_id}")],
            )?;
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
    #[serde(default)]
    log_cursor: u64,
    #[serde(default)]
    log_scope: Option<String>,
}

struct ResolvedScope {
    context: String,
    connection: String,
    target: String,
}

fn log_scope(scope: &ResolvedScope, snapshot: &TargetDebuggerSnapshot) -> String {
    format!(
        "{}\0{}\0{}\0{}",
        scope.context, scope.connection, scope.target, snapshot.connection_generation
    )
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

fn parse_log_options(
    options: &[String],
    default_after: u64,
) -> Result<(u64, usize, bool), io::Error> {
    let mut after = default_after;
    let mut limit = 20_usize;
    let mut explicit_after = false;
    let mut index = 0;
    while index < options.len() {
        match options[index].as_str() {
            "--after" => {
                index += 1;
                after = options
                    .get(index)
                    .ok_or_else(|| {
                        io::Error::new(io::ErrorKind::InvalidInput, "--after requires a cursor")
                    })?
                    .parse()
                    .map_err(|error| {
                        io::Error::new(
                            io::ErrorKind::InvalidInput,
                            format!("invalid log cursor: {error}"),
                        )
                    })?;
                explicit_after = true;
            }
            "--limit" => {
                index += 1;
                limit = options
                    .get(index)
                    .ok_or_else(|| {
                        io::Error::new(io::ErrorKind::InvalidInput, "--limit requires a value")
                    })?
                    .parse::<usize>()
                    .map_err(|error| {
                        io::Error::new(
                            io::ErrorKind::InvalidInput,
                            format!("invalid log limit: {error}"),
                        )
                    })?
                    .clamp(1, 100);
            }
            option => {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidInput,
                    format!("unknown log option '{option}'"),
                ));
            }
        }
        index += 1;
    }
    Ok((after, limit, explicit_after))
}

fn parse_logpoint_specs(values: &[String]) -> Result<Vec<LogpointSpec>, io::Error> {
    const FIELDS: usize = 5;
    if values.is_empty() || !values.len().is_multiple_of(FIELDS) {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "logpoints require repeated groups: <id> <source> <line> <column> <expression>",
        ));
    }
    values
        .chunks_exact(FIELDS)
        .map(|fields| {
            parse_logpoint_spec(&fields[0], &fields[1], &fields[2], &fields[3], &fields[4])
        })
        .collect()
}

fn parse_logpoint_spec(
    id: &str,
    source_url: &str,
    line: &str,
    column: &str,
    expression: &str,
) -> Result<LogpointSpec, io::Error> {
    Ok(LogpointSpec {
        id: id.to_owned(),
        source_url: source_url.to_owned(),
        line: line.parse().map_err(|error| {
            io::Error::new(
                io::ErrorKind::InvalidInput,
                format!("invalid logpoint line: {error}"),
            )
        })?,
        column: column.parse().map_err(|error| {
            io::Error::new(
                io::ErrorKind::InvalidInput,
                format!("invalid logpoint column: {error}"),
            )
        })?,
        expression: expression.to_owned(),
    })
}

struct CoverageShowOptions {
    capture_id: String,
    path: Option<String>,
    all: bool,
    max_lines: usize,
    no_cache: bool,
}

struct HeapSnapshotOptions {
    capture_numeric_value: bool,
    expose_internals: bool,
}

struct HeapCaptureOptions {
    capture_id: Option<String>,
    capture_numeric_value: bool,
    expose_internals: bool,
}

struct HeapClassOptions {
    capture_id: String,
    capture: bool,
    filter: Option<String>,
    all: bool,
    max_lines: usize,
    instances: bool,
    sort_by_instances: bool,
    no_cache: bool,
}

fn parse_heap_capture_options(values: &[String]) -> Result<HeapCaptureOptions, io::Error> {
    let mut capture_id = None;
    let mut capture_numeric_value = false;
    let mut expose_internals = false;
    let mut index = 0;
    while index < values.len() {
        match values[index].as_str() {
            "--id" => {
                index += 1;
                capture_id = Some(
                    values
                        .get(index)
                        .ok_or_else(|| {
                            io::Error::new(io::ErrorKind::InvalidInput, "--id requires a name")
                        })?
                        .clone(),
                );
            }
            "--capture-numeric-value" => capture_numeric_value = true,
            "--expose-internals" => expose_internals = true,
            option => {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidInput,
                    format!("unknown heap capture option '{option}'"),
                ));
            }
        }
        index += 1;
    }
    Ok(HeapCaptureOptions {
        capture_id,
        capture_numeric_value,
        expose_internals,
    })
}

fn parse_heap_class_options(values: &[String]) -> Result<HeapClassOptions, io::Error> {
    let mut capture_id = None;
    let mut capture = false;
    let mut filter = None;
    let mut all = false;
    let mut max_lines = 300_usize;
    let mut instances = false;
    let mut sort_by_instances = false;
    let mut no_cache = false;
    let mut index = 0;
    while index < values.len() {
        match values[index].as_str() {
            "--capture" | "--create-snapshot" => capture = true,
            "--filter" => {
                index += 1;
                filter = Some(
                    values
                        .get(index)
                        .ok_or_else(|| {
                            io::Error::new(
                                io::ErrorKind::InvalidInput,
                                "--filter requires a regular expression",
                            )
                        })?
                        .clone(),
                );
            }
            "--all" => all = true,
            "--instances" => instances = true,
            "--sort-by-instances" => sort_by_instances = true,
            "--no-cache" => no_cache = true,
            "--max-lines" => {
                index += 1;
                max_lines = values
                    .get(index)
                    .ok_or_else(|| {
                        io::Error::new(
                            io::ErrorKind::InvalidInput,
                            "--max-lines requires a positive integer",
                        )
                    })?
                    .parse()
                    .map_err(|error| {
                        io::Error::new(
                            io::ErrorKind::InvalidInput,
                            format!("invalid --max-lines value: {error}"),
                        )
                    })?;
                if max_lines == 0 {
                    return Err(io::Error::new(
                        io::ErrorKind::InvalidInput,
                        "--max-lines must be positive",
                    ));
                }
            }
            option if option.starts_with("--") => {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidInput,
                    format!("unknown heap classes option '{option}'"),
                ));
            }
            value if capture_id.is_none() => capture_id = Some(value.to_owned()),
            value => {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidInput,
                    format!("unexpected heap classes argument '{value}'"),
                ));
            }
        }
        index += 1;
    }
    Ok(HeapClassOptions {
        capture_id: capture_id.unwrap_or_else(|| ".".to_owned()),
        capture,
        filter,
        all,
        max_lines,
        instances,
        sort_by_instances,
        no_cache,
    })
}

fn parse_heap_snapshot_options(values: &[String]) -> Result<HeapSnapshotOptions, io::Error> {
    let mut options = HeapSnapshotOptions {
        capture_numeric_value: false,
        expose_internals: false,
    };
    for value in values {
        match value.as_str() {
            "--capture-numeric-value" => options.capture_numeric_value = true,
            "--expose-internals" => options.expose_internals = true,
            option => {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidInput,
                    format!("unknown heap snapshot option '{option}'"),
                ));
            }
        }
    }
    Ok(options)
}

fn absolute_path(path: &Path) -> Result<std::path::PathBuf, io::Error> {
    if path.is_absolute() {
        Ok(path.to_owned())
    } else {
        Ok(env::current_dir()?.join(path))
    }
}

async fn wait_for_heap_capture<F>(
    output: &OutputFormat,
    client: &DebuggerServiceApiClient,
    scope: &ResolvedScope,
    operation: &mut std::pin::Pin<&mut F>,
) -> Result<HeapCaptureResult, Box<dyn std::error::Error>>
where
    F: std::future::Future<Output = Result<HeapCaptureResult, hubrpc::prelude::JsonRpcError>>,
{
    let mut last_progress = None::<HeapSnapshotProgress>;
    let result = loop {
        tokio::select! {
            result = operation.as_mut() => break rpc(result)?,
            _ = tokio::time::sleep(std::time::Duration::from_millis(100)) => {
                let progress = rpc(client
                    .get_heap_snapshot_progress(
                        scope.context.clone(),
                        scope.connection.clone(),
                        scope.target.clone(),
                    )
                    .await)?;
                if let Some(progress) = progress
                    && last_progress.as_ref() != Some(&progress)
                {
                    output.print_heap_snapshot_progress(&progress)?;
                    last_progress = Some(progress);
                }
            }
        }
    };
    let progress = rpc(client
        .get_heap_snapshot_progress(
            scope.context.clone(),
            scope.connection.clone(),
            scope.target.clone(),
        )
        .await)?;
    if let Some(progress) = progress
        && last_progress.as_ref() != Some(&progress)
    {
        output.print_heap_snapshot_progress(&progress)?;
    }
    Ok(result)
}

fn parse_coverage_show_options(values: &[String]) -> Result<CoverageShowOptions, io::Error> {
    let mut capture_id = None;
    let mut path = None;
    let mut all = false;
    let mut max_lines = 300_usize;
    let mut no_cache = false;
    let mut index = 0;
    while index < values.len() {
        match values[index].as_str() {
            "--path" => {
                index += 1;
                path = Some(
                    values
                        .get(index)
                        .ok_or_else(|| {
                            io::Error::new(
                                io::ErrorKind::InvalidInput,
                                "--path requires a source prefix",
                            )
                        })?
                        .clone(),
                );
            }
            "--all" => all = true,
            "--no-cache" => no_cache = true,
            "--max-lines" => {
                index += 1;
                max_lines = values
                    .get(index)
                    .ok_or_else(|| {
                        io::Error::new(
                            io::ErrorKind::InvalidInput,
                            "--max-lines requires a positive integer",
                        )
                    })?
                    .parse()
                    .map_err(|error| {
                        io::Error::new(
                            io::ErrorKind::InvalidInput,
                            format!("invalid --max-lines value: {error}"),
                        )
                    })?;
                if max_lines == 0 {
                    return Err(io::Error::new(
                        io::ErrorKind::InvalidInput,
                        "--max-lines must be positive",
                    ));
                }
            }
            option if option.starts_with("--") => {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidInput,
                    format!("unknown coverage show option '{option}'"),
                ));
            }
            value if capture_id.is_none() => capture_id = Some(value.to_owned()),
            value => {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidInput,
                    format!("unexpected coverage show argument '{value}'"),
                ));
            }
        }
        index += 1;
    }
    Ok(CoverageShowOptions {
        capture_id: capture_id.unwrap_or_else(|| ".".to_owned()),
        path,
        all,
        max_lines,
        no_cache,
    })
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
            Ok(snapshot) => output.print_target_with_breakpoint_sources(
                &snapshot,
                &target.target_type,
                &[breakpoint_id.to_owned()],
            )?,
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

fn parse_mutation_options(arguments: &[String]) -> Result<MutationOptions, io::Error> {
    let mut options = MutationOptions::default();
    let mut index = 0;
    while index < arguments.len() {
        match arguments[index].as_str() {
            "--expected-revision" => {
                let value = arguments.get(index + 1).ok_or_else(|| {
                    io::Error::new(
                        io::ErrorKind::InvalidInput,
                        "--expected-revision requires a value",
                    )
                })?;
                options.expected_revision = Some(parse_u64("revision", value)?);
                index += 2;
            }
            "--request-id" => {
                let value = arguments.get(index + 1).ok_or_else(|| {
                    io::Error::new(io::ErrorKind::InvalidInput, "--request-id requires a value")
                })?;
                options.request_id = Some(value.clone());
                index += 2;
            }
            argument => {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidInput,
                    format!("unknown mutation option '{argument}'"),
                ));
            }
        }
    }
    Ok(options)
}

fn parse_observation_cursor(arguments: &[String]) -> Result<ObservationCursor, io::Error> {
    match arguments {
        [] => Ok(ObservationCursor::Current),
        [after, revision] if after == "--after-revision" => Ok(ObservationCursor::After {
            revision: parse_u64("revision", revision)?,
        }),
        _ => Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "state watch accepts only --after-revision <revision>",
        )),
    }
}

fn parse_breakpoint_spec(
    source_path: &str,
    line: &str,
    column: &str,
    arguments: &[String],
) -> Result<(BreakpointSpec, MutationOptions), io::Error> {
    let mut specification = BreakpointSpec {
        source_path: source_path.to_owned(),
        line: parse_u64("line", line)?
            .try_into()
            .map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, "line exceeds u32"))?,
        column: parse_u64("column", column)?
            .try_into()
            .map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, "column exceeds u32"))?,
        enabled: true,
        condition: None,
        target_selector: None,
    };
    let mut mutation_arguments = Vec::new();
    let mut index = 0;
    while index < arguments.len() {
        match arguments[index].as_str() {
            "--disabled" => {
                specification.enabled = false;
                index += 1;
            }
            "--condition" => {
                specification.condition = Some(
                    arguments
                        .get(index + 1)
                        .ok_or_else(|| {
                            io::Error::new(
                                io::ErrorKind::InvalidInput,
                                "--condition requires a value",
                            )
                        })?
                        .clone(),
                );
                index += 2;
            }
            "--target" => {
                specification.target_selector = Some(
                    arguments
                        .get(index + 1)
                        .ok_or_else(|| {
                            io::Error::new(io::ErrorKind::InvalidInput, "--target requires a value")
                        })?
                        .clone(),
                );
                index += 2;
            }
            "--expected-revision" | "--request-id" => {
                mutation_arguments.push(arguments[index].clone());
                mutation_arguments.push(
                    arguments
                        .get(index + 1)
                        .ok_or_else(|| {
                            io::Error::new(
                                io::ErrorKind::InvalidInput,
                                format!("{} requires a value", arguments[index]),
                            )
                        })?
                        .clone(),
                );
                index += 2;
            }
            argument => {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidInput,
                    format!("unknown breakpoint option '{argument}'"),
                ));
            }
        }
    }
    Ok((specification, parse_mutation_options(&mutation_arguments)?))
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
  jsdbg context delete <context-id> [--expected-revision <revision>] [--request-id <id>]
  jsdbg state get <context-id>
  jsdbg state watch <context-id> [--after-revision <revision>]
  jsdbg events <context-id> --after-revision <revision>
  jsdbg set workspace <context-id>
  jsdbg set target <selector>
  jsdbg connection add <context-id> <connection-id> <ws-endpoint> [--connect]
  jsdbg connection add <context-id> <connection-id> --playwright <url> [--channel <channel>] [--headed] [--ignore-https-errors] [--connect]
  jsdbg connection connect|disconnect <context-id> <connection-id>
  jsdbg connection delete <context-id> <connection-id> [--expected-revision <revision>] [--request-id <id>]
  jsdbg breakpoint set <context-id> <breakpoint-id> <source-url> <line> [column]
  jsdbg breakpoint configure <context-id> <breakpoint-id> <source-url> <line> <column> [--disabled] [--condition <expression>] [--target <target>] [--expected-revision <revision>] [--request-id <id>]
  jsdbg breakpoint delete <context-id> <breakpoint-id> [--expected-revision <revision>] [--request-id <id>]
  jsdbg source list <context-id>
  jsdbg source resolve|endpoints <context-id> <path>
  jsdbg source show|grep <context-id> <path-or-pattern>
  jsdbg source map <context-id> <generated-path> <line> <column>
  jsdbg source cache evict <context-id>
  jsdbg source export <context-id> <destination>
  jsdbg target show
  jsdbg target attach|show <context-id> <connection-id> <target>
  jsdbg target wait <context-id> <connection-id> <target> breakpoint-installed <breakpoint-id> [timeout-ms]
  jsdbg target wait <context-id> <connection-id> <target> paused <after-epoch> [timeout-ms]
  jsdbg target wait <context-id> <connection-id> <target> running
  jsdbg target resume [--epoch <epoch>]
  jsdbg target step into|over|out [--epoch <epoch>]
  jsdbg target eval|watch <expression>
  jsdbg target logpoint <id> <source> <line> <column> <expression>
  jsdbg target logpoints (<id> <source> <line> <column> <expression>)+
  jsdbg log [--after <cursor>] [--limit <count>]
  jsdbg target click <css-selector>
  jsdbg target key <ctrl+n|ctrl+k,ctrl+m|enter|accept|arrowup>
  jsdbg target type <text>
  jsdbg target click <context-id> <connection-id> <target> <css-selector>
  jsdbg coverage start
  jsdbg coverage capture [--id <name>]
  jsdbg coverage stop [--exclude <name>]
  jsdbg coverage show [<name>] [--path <source-prefix>] [--max-lines <count>] [--all] [--no-cache]
  jsdbg coverage start|capture|stop <context-id> <connection-id> <target>
  jsdbg heap capture [--id <name>] [--capture-numeric-value] [--expose-internals]
  jsdbg heap classes [<name>] [--capture] [--filter <regex>] [--sort-by-instances] [--instances] [--max-lines <count>] [--all] [--no-cache]
  jsdbg heap snapshot <path> [--capture-numeric-value] [--expose-internals]
  jsdbg target resume <context-id> <connection-id> <target> [--epoch <epoch>]
  jsdbg target step <context-id> <connection-id> <target> into|over|out [--epoch <epoch>]
  jsdbg target eval <context-id> <connection-id> <target> <expression>
  jsdbg target logpoint <context-id> <connection-id> <target> <id> <source> <line> <column> <expression>"
}

#[cfg(test)]
mod tests {
    use super::{parse_heap_capture_options, parse_heap_class_options};

    fn arguments(values: &[&str]) -> Vec<String> {
        values.iter().map(|value| (*value).to_owned()).collect()
    }

    #[test]
    fn parses_managed_heap_capture_options() {
        let options = parse_heap_capture_options(&arguments(&[
            "--id",
            "startup",
            "--capture-numeric-value",
            "--expose-internals",
        ]))
        .unwrap();
        assert_eq!(options.capture_id.as_deref(), Some("startup"));
        assert!(options.capture_numeric_value);
        assert!(options.expose_internals);
    }

    #[test]
    fn parses_heap_class_capture_and_output_options() {
        let options = parse_heap_class_options(&arguments(&[
            "startup",
            "--create-snapshot",
            "--filter",
            ".*PieceTree.*",
            "--sort-by-instances",
            "--instances",
            "--max-lines",
            "42",
            "--no-cache",
        ]))
        .unwrap();
        assert_eq!(options.capture_id, "startup");
        assert!(options.capture);
        assert_eq!(options.filter.as_deref(), Some(".*PieceTree.*"));
        assert!(options.sort_by_instances);
        assert!(options.instances);
        assert_eq!(options.max_lines, 42);
        assert!(options.no_cache);
    }

    #[test]
    fn heap_classes_default_to_coverage_aligned_capture_and_limit() {
        let options = parse_heap_class_options(&[]).unwrap();
        assert_eq!(options.capture_id, ".");
        assert_eq!(options.max_lines, 300);
        assert!(!options.capture);
    }
}
