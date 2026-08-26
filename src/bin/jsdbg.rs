use std::collections::BTreeMap;
use std::env;
use std::fs;
use std::io;
use std::io::Write;
use std::path::Path;

use atomic_write_file::AtomicWriteFile;
use base64::Engine;
use cdp_client::context_identity::{
    ContextIdentity, ContextKind, normalize_absolute_path, path_and_parents,
    resolve_context_expression,
};
use cdp_client::local_rpc::{connect_existing, default_state_file, ensure_service};
use cdp_client::service_api::{
    BreakpointSpec, ConnectionConfiguration, ContextSummary, CpuProfileSnapshot,
    DebuggerServiceApiClient, EvaluationSnapshot, HeapAggregateBy, HeapCaptureResult,
    HeapEdgePolicy, HeapNodeSelector, HeapPathCost, HeapPathDirection, HeapPathOptions,
    HeapReferenceDirection, HeapSnapshotProgress, LogpointSpec, MutationOptions, ObservationCursor,
    ObservationResult, PlaywrightChannel, ProcessRole, SourceDisplayOptions, SourceSearchOptions,
    StepKind, TargetDebuggerPhase, TargetDebuggerSnapshot, TargetWaitPredicate,
};
use serde::{Deserialize, Serialize};

#[path = "jsdbg/bounded_tree.rs"]
mod bounded_tree;
#[path = "jsdbg/output.rs"]
mod output;

use output::{
    CoverageOutputOptions, CpuProfileOutputOptions, CpuProfileSort, CpuProfileView,
    HeapClassOutputOptions, OutputFormat, ProcessTreeOutputOptions,
};

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
    let mut scope_options = extract_scope_options(&mut arguments)?;
    let state_file = default_state_file();
    let selection_file = state_file.with_extension("selection.json");
    let cwd = env::current_dir()?;
    let normalized_cwd = normalize_absolute_path(&cwd)?;
    if !is_context_create(&arguments)
        && let Some(expression) = scope_options.context.take()
    {
        scope_options.context = Some(resolve_context_expression(&expression, &cwd)?.id);
    } else if command_requires_context(&arguments) {
        let client = ensure_service(&state_file).await?;
        scope_options.context =
            Some(resolve_implicit_context(&client, &selection_file, &normalized_cwd).await?);
    }
    if !is_context_create(&arguments)
        && let Some(context) = scope_options.context.as_deref()
    {
        activate_selection_scope(&selection_file, &normalized_cwd, context)?;
    }
    match arguments.as_slice() {
        [set, context] if set == "set" && matches!(context.as_str(), "context" | "workspace") => {
            let context_id = required_option("--context", scope_options.context.as_ref())?;
            let client = ensure_service(&state_file).await?;
            rpc(client.get_context(context_id.clone()).await)?;
            select_context(&selection_file, &context_id)?;
            println!("Context: {context_id}");
        }
        [set, target] if set == "set" && target == "target" => {
            let selector = required_option("--target", scope_options.target.as_ref())?;
            let client = ensure_service(&state_file).await?;
            let selection = load_selection(&selection_file)?;
            let scope = resolve_scope(&client, &selection, &scope_options).await?;
            let snapshot = rpc(client
                .get_target(
                    scope.context.clone(),
                    scope.connection.clone(),
                    scope.target.clone(),
                )
                .await)?;
            select_scope(&selection_file, &scope, &snapshot)?;
            println!("Target: {selector}");
        }
        [target, show] if target == "target" && show == "show" => {
            let client = ensure_service(&state_file).await?;
            let selection = load_selection(&selection_file)?;
            let scope = resolve_scope(&client, &selection, &scope_options).await?;
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
            let scope = resolve_scope(&client, &selection, &scope_options).await?;
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
        [target, step, kind, options @ ..]
            if target == "target"
                && step == "step"
                && matches!(kind.as_str(), "into" | "over" | "out") =>
        {
            let client = ensure_service(&state_file).await?;
            let selection = load_selection(&selection_file)?;
            let scope = resolve_scope(&client, &selection, &scope_options).await?;
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
        [target, resume, options @ ..]
            if target == "target"
                && resume == "resume"
                && options
                    .first()
                    .is_none_or(|argument| argument.starts_with("--")) =>
        {
            let client = ensure_service(&state_file).await?;
            let selection = load_selection(&selection_file)?;
            let scope = resolve_scope(&client, &selection, &scope_options).await?;
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
            let scope = resolve_scope(&client, &selection, &scope_options).await?;
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
            let scope =
                resolve_scope(&client, &load_selection(&selection_file)?, &scope_options).await?;
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
            let scope =
                resolve_scope(&client, &load_selection(&selection_file)?, &scope_options).await?;
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
            let scope = resolve_scope(&client, &selection, &scope_options).await?;
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
            let scope =
                resolve_scope(&client, &load_selection(&selection_file)?, &scope_options).await?;
            rpc(client
                .key_target(scope.context, scope.connection, scope.target, chord.clone())
                .await)?;
            println!("Pressed {chord}");
        }
        [target, type_text, text] if target == "target" && type_text == "type" => {
            let client = ensure_service(&state_file).await?;
            let scope =
                resolve_scope(&client, &load_selection(&selection_file)?, &scope_options).await?;
            rpc(client
                .type_target(scope.context, scope.connection, scope.target, text.clone())
                .await)?;
            println!("Typed {text:?}");
        }
        [screenshot, capture, options @ ..]
            if screenshot == "screenshot" && capture == "capture" =>
        {
            let options = parse_screenshot_capture_options(options)?;
            let client = ensure_service(&state_file).await?;
            let scope =
                resolve_scope(&client, &load_selection(&selection_file)?, &scope_options).await?;
            let snapshot = rpc(client
                .capture_screenshot(scope.context, scope.connection, scope.target)
                .await)?;
            let bytes = base64::engine::general_purpose::STANDARD
                .decode(&snapshot.data_base64)
                .map_err(|error| {
                    io::Error::new(
                        io::ErrorKind::InvalidData,
                        format!("debug target returned invalid screenshot data: {error}"),
                    )
                })?;
            let (width, height) = png_dimensions(&bytes)?;
            let requested_path = options.output.unwrap_or_else(default_screenshot_path);
            let path = write_binary_file(&requested_path, &bytes)?;
            output.print_screenshot_captured(
                &path,
                bytes.len(),
                width,
                height,
                &snapshot.media_type,
            )?;
        }
        [coverage, start] if coverage == "coverage" && start == "start" => {
            let client = ensure_service(&state_file).await?;
            let scope =
                resolve_scope(&client, &load_selection(&selection_file)?, &scope_options).await?;
            rpc(client
                .start_coverage(scope.context, scope.connection, scope.target)
                .await)?;
            println!("Coverage recording started.");
        }
        [coverage, take]
            if coverage == "coverage" && matches!(take.as_str(), "take" | "capture") =>
        {
            let client = ensure_service(&state_file).await?;
            let scope =
                resolve_scope(&client, &load_selection(&selection_file)?, &scope_options).await?;
            output.print(&rpc(client
                .take_coverage(scope.context, scope.connection, scope.target, None, None)
                .await)?)?;
        }
        [coverage, capture, id, capture_id]
            if coverage == "coverage" && capture == "capture" && id == "--id" =>
        {
            let client = ensure_service(&state_file).await?;
            let scope =
                resolve_scope(&client, &load_selection(&selection_file)?, &scope_options).await?;
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
            let scope =
                resolve_scope(&client, &load_selection(&selection_file)?, &scope_options).await?;
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
                    trim_width: true,
                },
            )?;
        }
        [coverage, stop] if coverage == "coverage" && stop == "stop" => {
            let client = ensure_service(&state_file).await?;
            let scope =
                resolve_scope(&client, &load_selection(&selection_file)?, &scope_options).await?;
            rpc(client
                .finish_coverage(scope.context, scope.connection, scope.target, None)
                .await)?;
            output.print_coverage_stopped()?;
        }
        [coverage, stop, exclude, capture_id]
            if coverage == "coverage" && stop == "stop" && exclude == "--exclude" =>
        {
            let client = ensure_service(&state_file).await?;
            let scope =
                resolve_scope(&client, &load_selection(&selection_file)?, &scope_options).await?;
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
            let scope =
                resolve_scope(&client, &load_selection(&selection_file)?, &scope_options).await?;
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
                    trim_width: options.trim_width,
                },
            )?;
        }
        [profile, start, options @ ..] if profile == "profile" && start == "start" => {
            let sampling_interval_micros = parse_cpu_profile_start_options(options)?;
            let client = ensure_service(&state_file).await?;
            let scope =
                resolve_scope(&client, &load_selection(&selection_file)?, &scope_options).await?;
            rpc(client
                .start_cpu_profile(
                    scope.context,
                    scope.connection,
                    scope.target,
                    sampling_interval_micros,
                )
                .await)?;
            output.print_cpu_profile_started(sampling_interval_micros)?;
        }
        [profile, stop, options @ ..] if profile == "profile" && stop == "stop" => {
            let capture_id = parse_cpu_profile_stop_options(options)?;
            let client = ensure_service(&state_file).await?;
            let scope =
                resolve_scope(&client, &load_selection(&selection_file)?, &scope_options).await?;
            let profile = rpc(client
                .stop_cpu_profile(scope.context, scope.connection, scope.target, capture_id)
                .await)?;
            output.print_cpu_profile_stopped(&profile)?;
        }
        [profile, show, options @ ..] if profile == "profile" && show == "show" => {
            let options = parse_cpu_profile_show_options(options)?;
            let client = ensure_service(&state_file).await?;
            let scope =
                resolve_scope(&client, &load_selection(&selection_file)?, &scope_options).await?;
            let profile = rpc(client
                .get_cpu_profile(
                    scope.context,
                    scope.connection,
                    scope.target,
                    options.capture_id,
                    options.path.clone(),
                    options.no_cache,
                    true,
                )
                .await)?;
            output.print_cpu_profile(
                &profile,
                CpuProfileOutputOptions {
                    path: options.path.as_deref(),
                    view: options.view,
                    sort: options.sort,
                    max_lines: options.max_lines,
                },
            )?;
        }
        [profile, export, options @ ..] if profile == "profile" && export == "export" => {
            let options = parse_cpu_profile_export_options(options)?;
            let destination = absolute_path(Path::new(&options.output))?;
            let client = ensure_service(&state_file).await?;
            let scope =
                resolve_scope(&client, &load_selection(&selection_file)?, &scope_options).await?;
            let profile = rpc(client
                .get_cpu_profile(
                    scope.context,
                    scope.connection,
                    scope.target,
                    options.capture_id,
                    None,
                    false,
                    false,
                )
                .await)?;
            let serialized = serde_json::to_vec_pretty(&cpu_profile_export(&profile))?;
            tokio::fs::write(&destination, serialized).await?;
            output.print_cpu_profile_exported(&destination)?;
        }
        [heap, capture, options @ ..] if heap == "heap" && capture == "capture" => {
            let options = parse_heap_capture_options(options)?;
            let client = ensure_service(&state_file).await?;
            let scope =
                resolve_scope(&client, &load_selection(&selection_file)?, &scope_options).await?;
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
            let scope =
                resolve_scope(&client, &load_selection(&selection_file)?, &scope_options).await?;
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
                    trim_width: options.trim_width,
                },
            )?;
        }
        [heap, select, options @ ..] if heap == "heap" && select == "select" => {
            let options = parse_heap_select_options(options)?;
            let client = ensure_service(&state_file).await?;
            let scope =
                resolve_scope(&client, &load_selection(&selection_file)?, &scope_options).await?;
            let selection = rpc(client
                .select_heap_nodes(
                    scope.context,
                    scope.connection,
                    scope.target,
                    options.capture_id,
                    options.selector,
                    options.max_string_length,
                    options.include_dominators,
                )
                .await)?;
            output.print(&selection)?;
        }
        [heap, strings, options @ ..] if heap == "heap" && strings == "strings" => {
            let options = parse_heap_string_options(options)?;
            let client = ensure_service(&state_file).await?;
            let scope =
                resolve_scope(&client, &load_selection(&selection_file)?, &scope_options).await?;
            let selection = rpc(client
                .select_heap_nodes(
                    scope.context,
                    scope.connection,
                    scope.target,
                    options.capture_id,
                    options.selector,
                    options.max_string_length,
                    false,
                )
                .await)?;
            output.print(&selection)?;
        }
        [heap, show, reference, options @ ..] if heap == "heap" && show == "show" => {
            let (capture_id, heap_object_id) = split_heap_reference_cli(reference)?;
            let max_string_length = parse_heap_string_display_options(options)?;
            let client = ensure_service(&state_file).await?;
            let scope =
                resolve_scope(&client, &load_selection(&selection_file)?, &scope_options).await?;
            let selection = rpc(client
                .select_heap_nodes(
                    scope.context,
                    scope.connection,
                    scope.target,
                    capture_id,
                    HeapNodeSelector {
                        heap_object_id: Some(heap_object_id),
                        limit: Some(1),
                        ..HeapNodeSelector::default()
                    },
                    max_string_length,
                    true,
                )
                .await)?;
            if selection.nodes.is_empty() {
                return Err(io::Error::new(
                    io::ErrorKind::NotFound,
                    format!("heap reference '{reference}' does not exist"),
                )
                .into());
            }
            output.print(&selection)?;
        }
        [heap, refs, reference, options @ ..] if heap == "heap" && refs == "refs" => {
            let options = parse_heap_reference_options(options)?;
            let client = ensure_service(&state_file).await?;
            let scope =
                resolve_scope(&client, &load_selection(&selection_file)?, &scope_options).await?;
            let references = rpc(client
                .get_heap_references(
                    scope.context,
                    scope.connection,
                    scope.target,
                    reference.clone(),
                    options.direction,
                    options.edge_policy,
                    options.limit,
                    options.max_string_length,
                )
                .await)?;
            output.print(&references)?;
        }
        [heap, path, from, to, options @ ..] if heap == "heap" && path == "path" => {
            let options = parse_heap_path_options(options)?;
            let client = ensure_service(&state_file).await?;
            let scope =
                resolve_scope(&client, &load_selection(&selection_file)?, &scope_options).await?;
            let path = rpc(client
                .get_heap_path(
                    scope.context,
                    scope.connection,
                    scope.target,
                    from.clone(),
                    to.clone(),
                    options.path,
                    options.max_string_length,
                )
                .await)?;
            match path {
                Some(path) => output.print(&path)?,
                None => {
                    return Err(io::Error::new(
                        io::ErrorKind::NotFound,
                        format!("no heap path exists from '{from}' to '{to}'"),
                    )
                    .into());
                }
            }
        }
        [heap, root_path, reference, options @ ..]
            if heap == "heap" && root_path == "root-path" =>
        {
            let options = parse_heap_path_options(options)?;
            let (capture_id, _) = split_heap_reference_cli(reference)?;
            let client = ensure_service(&state_file).await?;
            let scope =
                resolve_scope(&client, &load_selection(&selection_file)?, &scope_options).await?;
            let root = rpc(client
                .select_heap_nodes(
                    scope.context.clone(),
                    scope.connection.clone(),
                    scope.target.clone(),
                    capture_id,
                    HeapNodeSelector {
                        limit: Some(1),
                        ..HeapNodeSelector::default()
                    },
                    options.max_string_length,
                    false,
                )
                .await)?
            .nodes
            .into_iter()
            .next()
            .ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, "heap graph is empty"))?;
            let path = rpc(client
                .get_heap_path(
                    scope.context,
                    scope.connection,
                    scope.target,
                    root.reference,
                    reference.clone(),
                    options.path,
                    options.max_string_length,
                )
                .await)?;
            match path {
                Some(path) => output.print(&path)?,
                None => {
                    return Err(io::Error::new(
                        io::ErrorKind::NotFound,
                        format!("no root path exists for '{reference}'"),
                    )
                    .into());
                }
            }
        }
        [heap, retainer_path, reference, options @ ..]
            if heap == "heap" && retainer_path == "retainer-path" =>
        {
            let mut options = parse_heap_path_options(options)?;
            options.path.direction = HeapPathDirection::Incoming;
            let (capture_id, _) = split_heap_reference_cli(reference)?;
            let client = ensure_service(&state_file).await?;
            let scope =
                resolve_scope(&client, &load_selection(&selection_file)?, &scope_options).await?;
            let root = rpc(client
                .select_heap_nodes(
                    scope.context.clone(),
                    scope.connection.clone(),
                    scope.target.clone(),
                    capture_id,
                    HeapNodeSelector {
                        limit: Some(1),
                        ..HeapNodeSelector::default()
                    },
                    options.max_string_length,
                    false,
                )
                .await)?
            .nodes
            .into_iter()
            .next()
            .ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, "heap graph is empty"))?;
            let path = rpc(client
                .get_heap_path(
                    scope.context,
                    scope.connection,
                    scope.target,
                    reference.clone(),
                    root.reference,
                    options.path,
                    options.max_string_length,
                )
                .await)?;
            match path {
                Some(path) => output.print(&path)?,
                None => {
                    return Err(io::Error::new(
                        io::ErrorKind::NotFound,
                        format!("no retainer path exists for '{reference}'"),
                    )
                    .into());
                }
            }
        }
        [heap, dominators, reference, options @ ..]
            if heap == "heap" && dominators == "dominators" =>
        {
            let max_string_length = parse_heap_string_display_options(options)?;
            let client = ensure_service(&state_file).await?;
            let scope =
                resolve_scope(&client, &load_selection(&selection_file)?, &scope_options).await?;
            let chain = rpc(client
                .get_heap_dominator_chain(
                    scope.context,
                    scope.connection,
                    scope.target,
                    reference.clone(),
                    max_string_length,
                )
                .await)?;
            output.print(&chain)?;
        }
        [heap, aggregate, options @ ..] if heap == "heap" && aggregate == "aggregate" => {
            let options = parse_heap_aggregate_options(options)?;
            let client = ensure_service(&state_file).await?;
            let scope =
                resolve_scope(&client, &load_selection(&selection_file)?, &scope_options).await?;
            let aggregate = rpc(client
                .aggregate_heap_snapshot(
                    scope.context,
                    scope.connection,
                    scope.target,
                    options.capture_id,
                    options.by,
                    options.limit,
                    options.max_string_length,
                )
                .await)?;
            output.print(&aggregate)?;
        }
        [heap, diff, older, newer, options @ ..] if heap == "heap" && diff == "diff" => {
            let options = parse_heap_diff_options(options)?;
            let client = ensure_service(&state_file).await?;
            let scope =
                resolve_scope(&client, &load_selection(&selection_file)?, &scope_options).await?;
            let diff = rpc(client
                .diff_heap_snapshots(
                    scope.context,
                    scope.connection,
                    scope.target,
                    older.clone(),
                    newer.clone(),
                    options.by,
                    options.limit,
                    options.max_string_length,
                )
                .await)?;
            output.print(&diff)?;
        }
        [heap, snapshot, path, options @ ..] if heap == "heap" && snapshot == "snapshot" => {
            let options = parse_heap_snapshot_options(options)?;
            let destination = absolute_path(Path::new(path))?;
            let client = ensure_service(&state_file).await?;
            let scope =
                resolve_scope(&client, &load_selection(&selection_file)?, &scope_options).await?;
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
        [target, watch, expression] if target == "target" && watch == "watch" => {
            let client = ensure_service(&state_file).await?;
            let mut selection = load_selection(&selection_file)?;
            let scope = resolve_scope(&client, &selection, &scope_options).await?;
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
        [process, list, options @ ..] if process == "process" && list == "list" => {
            let options = parse_process_list_options(options)?;
            let trees =
                cdp_client::process_discovery::discover_vscode_process_trees(options.stats).await?;
            output.print_process_trees(
                &trees,
                ProcessTreeOutputOptions {
                    command_line: options.command_line,
                    stats: options.stats,
                    filter: options.filter.as_deref(),
                    trim_width: options.trim_width,
                },
            )?;
        }
        [process, attach, arguments @ ..] if process == "process" && attach == "attach" => {
            let options = parse_process_attach_options(arguments)?;
            let context_id =
                selected_or_explicit_context(&selection_file, scope_options.context.clone())?;
            let process_id = options.process_id;
            let renderer = cdp_client::process_discovery::discover_vscode_process_trees(false)
                .await?
                .into_iter()
                .find_map(|tree| {
                    tree.processes
                        .into_iter()
                        .find(|process| {
                            process.process_id == process_id
                                && process.role == ProcessRole::Renderer
                        })
                        .map(|process| (tree.root_process_id, process.debug_target_id))
                });
            let (connection_id, configuration, target_id) =
                if let Some((root_process_id, Some(target_id))) = renderer {
                    (
                        format!("process-tree-{root_process_id}"),
                        ConnectionConfiguration::ProcessTree {
                            root_pid: root_process_id,
                        },
                        target_id,
                    )
                } else {
                    (
                        format!("process-{process_id}"),
                        ConnectionConfiguration::Process { process_id },
                        "$node-root".to_owned(),
                    )
                };
            let client = ensure_service(&state_file).await?;
            rpc(client
                .put_connection(context_id.clone(), connection_id.clone(), configuration)
                .await)?;
            rpc(client
                .connect_connection(context_id.clone(), connection_id.clone())
                .await)?;
            if target_id != "$node-root" {
                let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
                loop {
                    let context = rpc(client.get_context(context_id.clone()).await)?;
                    if context.target_forest.iter().any(|node| {
                        node.connection_id == connection_id && node.target.target_id == target_id
                    }) {
                        break;
                    }
                    if std::time::Instant::now() >= deadline {
                        return Err(format!(
                            "renderer target {target_id} was not published within 10 seconds"
                        )
                        .into());
                    }
                    tokio::time::sleep(std::time::Duration::from_millis(100)).await;
                }
            }
            let snapshot = rpc(client
                .attach_target(context_id.clone(), connection_id.clone(), target_id.clone())
                .await)?;
            if options.set_default {
                select_scope(
                    &selection_file,
                    &ResolvedScope {
                        context: context_id.clone(),
                        connection: connection_id,
                        target: target_id.clone(),
                    },
                    &snapshot,
                )?;
            }
            output.print_target(&snapshot, &target_id)?;
        }
        [context, list] if context == "context" && list == "list" => {
            let client = ensure_service(&state_file).await?;
            output.print(&rpc(client
                .list_contexts(Some(normalized_cwd.clone()))
                .await)?)?;
        }
        [context, create, options @ ..] if context == "context" && create == "create" => {
            let (expression, display_name, set_default) =
                parse_context_create_options(scope_options.context.as_ref(), options)?;
            let ContextIdentity {
                id: context_id,
                kind,
            } = resolve_context_expression(&expression, &cwd)?;
            let client = ensure_service(&state_file).await?;
            let snapshot = rpc(client
                .put_context(context_id.clone(), kind, display_name)
                .await)?;
            if set_default {
                select_context(&selection_file, &context_id)?;
            }
            output.print(&snapshot)?;
        }
        [context, show] if context == "context" && show == "show" => {
            let context_id =
                selected_or_explicit_context(&selection_file, scope_options.context.clone())?;
            let client = ensure_service(&state_file).await?;
            output.print(&rpc(client.get_context(context_id).await)?)?;
        }
        [context, delete, options @ ..] if context == "context" && delete == "delete" => {
            let context_id =
                selected_or_explicit_context(&selection_file, scope_options.context.clone())?;
            let client = ensure_service(&state_file).await?;
            output.print(&rpc(client
                .delete_context(context_id, parse_mutation_options(options)?)
                .await)?)?;
        }
        [state, get] if state == "state" && get == "get" => {
            let context_id =
                selected_or_explicit_context(&selection_file, scope_options.context.clone())?;
            let client = ensure_service(&state_file).await?;
            output.print(&rpc(client.get_context(context_id).await)?)?;
        }
        [state, watch, options @ ..] if state == "state" && watch == "watch" => {
            let context_id =
                selected_or_explicit_context(&selection_file, scope_options.context.clone())?;
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
        [events, after, revision] if events == "events" && after == "--after-revision" => {
            let context_id =
                selected_or_explicit_context(&selection_file, scope_options.context.clone())?;
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
        [connection, add, process, process_id, connect_now]
            if connection == "connection"
                && add == "add"
                && process == "--process"
                && connect_now == "--connect" =>
        {
            let context_id =
                selected_or_explicit_context(&selection_file, scope_options.context.clone())?;
            let connection_id = required_option("--connection", scope_options.connection.as_ref())?;
            add_connection(
                &context_id,
                connection_id,
                ConnectionConfiguration::Process {
                    process_id: parse_u32("process ID", process_id)?,
                },
                true,
                &state_file,
                &selection_file,
                false,
                output,
            )
            .await?;
        }
        [connection, add, process_tree, root_pid, connect_now]
            if connection == "connection"
                && add == "add"
                && process_tree == "--process-tree"
                && connect_now == "--connect" =>
        {
            let context_id =
                selected_or_explicit_context(&selection_file, scope_options.context.clone())?;
            let connection_id = required_option("--connection", scope_options.connection.as_ref())?;
            add_connection(
                &context_id,
                connection_id,
                ConnectionConfiguration::ProcessTree {
                    root_pid: parse_u32("root process ID", root_pid)?,
                },
                true,
                &state_file,
                &selection_file,
                false,
                output,
            )
            .await?;
        }
        [connection, add, node_inspector, endpoint, connect_now]
            if connection == "connection"
                && add == "add"
                && node_inspector == "--node-inspector"
                && connect_now == "--connect" =>
        {
            let context_id =
                selected_or_explicit_context(&selection_file, scope_options.context.clone())?;
            let connection_id = required_option("--connection", scope_options.connection.as_ref())?;
            add_connection(
                &context_id,
                connection_id,
                ConnectionConfiguration::NodeInspector {
                    endpoint: endpoint.clone(),
                },
                true,
                &state_file,
                &selection_file,
                false,
                output,
            )
            .await?;
        }
        [connection, add, endpoint, connect_now]
            if connection == "connection" && add == "add" && connect_now == "--connect" =>
        {
            let context_id =
                selected_or_explicit_context(&selection_file, scope_options.context.clone())?;
            let connection_id = required_option("--connection", scope_options.connection.as_ref())?;
            add_connection(
                &context_id,
                connection_id,
                ConnectionConfiguration::DirectCdp {
                    endpoint: endpoint.clone(),
                },
                true,
                &state_file,
                &selection_file,
                false,
                output,
            )
            .await?;
        }
        [connection, add, endpoint] if connection == "connection" && add == "add" => {
            let context_id =
                selected_or_explicit_context(&selection_file, scope_options.context.clone())?;
            let connection_id = required_option("--connection", scope_options.connection.as_ref())?;
            add_connection(
                &context_id,
                connection_id,
                ConnectionConfiguration::DirectCdp {
                    endpoint: endpoint.clone(),
                },
                false,
                &state_file,
                &selection_file,
                false,
                output,
            )
            .await?;
        }
        [connection, add, playwright, url, options @ ..]
            if connection == "connection" && add == "add" && playwright == "--playwright" =>
        {
            let context_id =
                selected_or_explicit_context(&selection_file, scope_options.context.clone())?;
            let connection_id = required_option("--connection", scope_options.connection.as_ref())?;
            let options = parse_playwright_options(options)?;
            add_connection(
                &context_id,
                connection_id,
                ConnectionConfiguration::Playwright {
                    url: url.clone(),
                    playwright_package: None,
                    channel: options.channel,
                    headless: options.headless,
                    ignore_https_errors: options.ignore_https_errors,
                },
                options.connect,
                &state_file,
                &selection_file,
                options.set_default,
                output,
            )
            .await?;
        }
        [connection, add, chrome, url, options @ ..]
            if connection == "connection" && add == "add" && chrome == "--chrome" =>
        {
            let context_id =
                selected_or_explicit_context(&selection_file, scope_options.context.clone())?;
            let connection_id = required_option("--connection", scope_options.connection.as_ref())?;
            let options = parse_chrome_options(options)?;
            add_connection(
                &context_id,
                connection_id,
                ConnectionConfiguration::Chrome {
                    url: url.clone(),
                    executable: options.executable,
                    headless: options.headless,
                    user_data_dir: options.user_data_dir,
                    args: options.args,
                },
                options.connect,
                &state_file,
                &selection_file,
                options.set_default,
                output,
            )
            .await?;
        }
        [connection, connect] if connection == "connection" && connect == "connect" => {
            let (context_id, connection_id) =
                selected_or_explicit_connection(&selection_file, &scope_options)?;
            let client = ensure_service(&state_file).await?;
            output.print(&rpc(client
                .connect_connection(context_id, connection_id)
                .await)?)?;
        }
        [connection, disconnect] if connection == "connection" && disconnect == "disconnect" => {
            let (context_id, connection_id) =
                selected_or_explicit_connection(&selection_file, &scope_options)?;
            let client = ensure_service(&state_file).await?;
            output.print(&rpc(client
                .disconnect_connection(context_id, connection_id)
                .await)?)?;
        }
        [connection, delete, options @ ..] if connection == "connection" && delete == "delete" => {
            let (context_id, connection_id) =
                selected_or_explicit_connection(&selection_file, &scope_options)?;
            let client = ensure_service(&state_file).await?;
            output.print(&rpc(client
                .delete_connection(context_id, connection_id, parse_mutation_options(options)?)
                .await)?)?;
        }
        [breakpoint, set, breakpoint_id, source_path, line]
            if breakpoint == "breakpoint" && set == "set" =>
        {
            put_selected_breakpoint(
                breakpoint_id,
                source_path,
                line,
                "1",
                &selection_file,
                &state_file,
                &scope_options,
                output,
            )
            .await?;
        }
        [
            breakpoint,
            set,
            breakpoint_id,
            source_path,
            line,
            column_option,
            column,
        ] if breakpoint == "breakpoint" && set == "set" && column_option == "--column" => {
            put_selected_breakpoint(
                breakpoint_id,
                source_path,
                line,
                column,
                &selection_file,
                &state_file,
                &scope_options,
                output,
            )
            .await?;
        }
        [
            breakpoint,
            configure,
            breakpoint_id,
            source_path,
            line,
            column,
            options @ ..,
        ] if breakpoint == "breakpoint" && configure == "configure" => {
            let context_id =
                selected_or_explicit_context(&selection_file, scope_options.context.clone())?;
            let (specification, mutation) =
                parse_breakpoint_spec(source_path, line, column, options)?;
            let client = ensure_service(&state_file).await?;
            output.print(&rpc(client
                .put_breakpoint_spec(context_id, breakpoint_id.clone(), specification, mutation)
                .await)?)?;
        }
        [breakpoint, delete, breakpoint_id, options @ ..]
            if breakpoint == "breakpoint" && delete == "delete" =>
        {
            let context_id =
                selected_or_explicit_context(&selection_file, scope_options.context.clone())?;
            let client = ensure_service(&state_file).await?;
            output.print(&rpc(client
                .delete_breakpoint(
                    context_id,
                    breakpoint_id.clone(),
                    parse_mutation_options(options)?,
                )
                .await)?)?;
        }
        [source, list, options @ ..] if source == "source" && list == "list" => {
            let context_id =
                selected_or_explicit_context(&selection_file, scope_options.context.clone())?;
            let client = ensure_service(&state_file).await?;
            output.print(&rpc(client
                .list_sources(context_id, parse_source_list_options(options)?)
                .await)?)?;
        }
        [source, resolve, arguments @ ..] if source == "source" && resolve == "resolve" => {
            let path = parse_source_path_arguments(arguments, "source resolve")?;
            let client = ensure_service(&state_file).await?;
            let context_id =
                selected_or_explicit_context(&selection_file, scope_options.context.clone())?;
            output.print(&rpc(client.list_sources(context_id, Some(path)).await)?)?;
        }
        [source, endpoints, arguments @ ..] if source == "source" && endpoints == "endpoints" => {
            let path = parse_source_path_arguments(arguments, "source endpoints")?;
            let client = ensure_service(&state_file).await?;
            let context_id =
                selected_or_explicit_context(&selection_file, scope_options.context.clone())?;
            output.print(&rpc(client.list_sources(context_id, Some(path)).await)?)?;
        }
        [source, show, arguments @ ..] if source == "source" && show == "show" => {
            let (path, options) = parse_source_show_options(arguments)?;
            let client = ensure_service(&state_file).await?;
            let context_id =
                selected_or_explicit_context(&selection_file, scope_options.context.clone())?;
            output.print(&rpc(client.show_source(context_id, path, options).await)?)?;
        }
        [source, grep, arguments @ ..] if source == "source" && grep == "grep" => {
            let options = parse_source_grep_options(arguments)?;
            let client = ensure_service(&state_file).await?;
            let context_id =
                selected_or_explicit_context(&selection_file, scope_options.context.clone())?;
            output.print(&rpc(client.grep_sources(context_id, options).await)?)?;
        }
        [source, explain, arguments @ ..] if source == "source" && explain == "explain" => {
            let path = parse_source_path_arguments(arguments, "source explain")?;
            let client = ensure_service(&state_file).await?;
            let context_id =
                selected_or_explicit_context(&selection_file, scope_options.context.clone())?;
            output.print(&rpc(client.explain_source(context_id, path).await)?)?;
        }
        [source, graph] if source == "source" && graph == "graph" => {
            let client = ensure_service(&state_file).await?;
            let context_id =
                selected_or_explicit_context(&selection_file, scope_options.context.clone())?;
            output.print(&rpc(client.show_source_graph(context_id).await)?)?;
        }
        [source, map, show] if source == "source" && map == "map" && show == "show" => {
            let client = ensure_service(&state_file).await?;
            let context_id =
                selected_or_explicit_context(&selection_file, scope_options.context.clone())?;
            output.print(&rpc(client.show_source_graph(context_id).await)?)?;
        }
        [source, map, arguments @ ..] if source == "source" && map == "map" => {
            let (path, line, column) = parse_source_map_arguments(arguments)?;
            let client = ensure_service(&state_file).await?;
            let context_id =
                selected_or_explicit_context(&selection_file, scope_options.context.clone())?;
            output.print(&rpc(client
                .map_source(context_id, path, line, column)
                .await)?)?;
        }
        [source, cache, evict] if source == "source" && cache == "cache" && evict == "evict" => {
            let context_id =
                selected_or_explicit_context(&selection_file, scope_options.context.clone())?;
            let client = ensure_service(&state_file).await?;
            output.print(&rpc(client.evict_source_caches(context_id).await)?)?;
        }
        [source, export, destination] if source == "source" && export == "export" => {
            let context_id =
                selected_or_explicit_context(&selection_file, scope_options.context.clone())?;
            let client = ensure_service(&state_file).await?;
            output.print(&rpc(client
                .export_sources(context_id, destination.clone())
                .await)?)?;
        }
        [target, attach, options @ ..] if target == "target" && attach == "attach" => {
            let set_default = parse_set_option(options)?;
            let client = ensure_service(&state_file).await?;
            let scope =
                resolve_scope(&client, &load_selection(&selection_file)?, &scope_options).await?;
            let snapshot = rpc(client
                .attach_target(
                    scope.context.clone(),
                    scope.connection.clone(),
                    scope.target.clone(),
                )
                .await)?;
            if set_default {
                select_scope(&selection_file, &scope, &snapshot)?;
            }
            output.print_target(&snapshot, &scope.target)?;
        }
        [target, wait, installed, breakpoint_id]
            if target == "target" && wait == "wait" && installed == "breakpoint-installed" =>
        {
            let client = ensure_service(&state_file).await?;
            let scope =
                resolve_scope(&client, &load_selection(&selection_file)?, &scope_options).await?;
            wait_target(
                &scope.context,
                &scope.connection,
                &scope.target,
                TargetWaitPredicate::BreakpointInstalled {
                    breakpoint_id: breakpoint_id.clone(),
                },
                "30000",
                &state_file,
                output,
            )
            .await?;
        }
        [target, wait, installed, breakpoint_id, timeout_ms]
            if target == "target" && wait == "wait" && installed == "breakpoint-installed" =>
        {
            let client = ensure_service(&state_file).await?;
            let scope =
                resolve_scope(&client, &load_selection(&selection_file)?, &scope_options).await?;
            wait_target(
                &scope.context,
                &scope.connection,
                &scope.target,
                TargetWaitPredicate::BreakpointInstalled {
                    breakpoint_id: breakpoint_id.clone(),
                },
                timeout_ms,
                &state_file,
                output,
            )
            .await?;
        }
        [target, wait, paused, after_epoch]
            if target == "target" && wait == "wait" && paused == "paused" =>
        {
            let client = ensure_service(&state_file).await?;
            let scope =
                resolve_scope(&client, &load_selection(&selection_file)?, &scope_options).await?;
            wait_target(
                &scope.context,
                &scope.connection,
                &scope.target,
                TargetWaitPredicate::Paused {
                    after_epoch: parse_u64("pause epoch", after_epoch)?,
                },
                "30000",
                &state_file,
                output,
            )
            .await?;
        }
        [target, wait, paused, after_epoch, timeout_ms]
            if target == "target" && wait == "wait" && paused == "paused" =>
        {
            let client = ensure_service(&state_file).await?;
            let scope =
                resolve_scope(&client, &load_selection(&selection_file)?, &scope_options).await?;
            wait_target(
                &scope.context,
                &scope.connection,
                &scope.target,
                TargetWaitPredicate::Paused {
                    after_epoch: parse_u64("pause epoch", after_epoch)?,
                },
                timeout_ms,
                &state_file,
                output,
            )
            .await?;
        }
        [target, wait, running] if target == "target" && wait == "wait" && running == "running" => {
            let client = ensure_service(&state_file).await?;
            let scope =
                resolve_scope(&client, &load_selection(&selection_file)?, &scope_options).await?;
            wait_target(
                &scope.context,
                &scope.connection,
                &scope.target,
                TargetWaitPredicate::Running,
                "30000",
                &state_file,
                output,
            )
            .await?;
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
    #[serde(default, alias = "workspace")]
    context: Option<String>,
    #[serde(default)]
    connection: Option<String>,
    target: Option<String>,
    #[serde(default)]
    watches: Vec<String>,
    #[serde(default)]
    log_cursor: u64,
    #[serde(default)]
    log_scope: Option<String>,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
struct SelectionStore {
    schema_version: u32,
    #[serde(default)]
    cwd_bindings: BTreeMap<String, String>,
    #[serde(default)]
    active_scopes: BTreeMap<String, String>,
    #[serde(default)]
    scopes: BTreeMap<String, CliSelection>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct LegacyCliSelection {
    #[serde(default, alias = "workspace")]
    context: Option<String>,
    #[serde(default)]
    connection: Option<String>,
    target: Option<String>,
    #[serde(default)]
    watches: Vec<String>,
    #[serde(default)]
    log_cursor: u64,
    #[serde(default)]
    log_scope: Option<String>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct ResolvedScope {
    context: String,
    connection: String,
    target: String,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
struct ScopeOptionKind {
    context: bool,
    connection: bool,
    target: bool,
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
struct ScopeOptions {
    context: Option<String>,
    connection: Option<String>,
    target: Option<String>,
}

fn required_option<'a>(name: &str, value: Option<&'a String>) -> Result<&'a String, io::Error> {
    value.ok_or_else(|| {
        io::Error::new(
            io::ErrorKind::InvalidInput,
            format!("{name} is required for this command"),
        )
    })
}

fn scope_option_kind(arguments: &[String]) -> ScopeOptionKind {
    let command = arguments.first().map(String::as_str);
    let operation = arguments.get(1).map(String::as_str);
    match (command, operation) {
        (Some("target" | "coverage" | "profile" | "heap" | "screenshot" | "log" | "watch"), _)
        | (Some("breakpoint"), Some("set"))
        | (Some("set"), Some("target")) => ScopeOptionKind {
            context: true,
            connection: true,
            target: true,
        },
        (Some("connection"), _) => ScopeOptionKind {
            context: true,
            connection: true,
            target: false,
        },
        (Some("context" | "state" | "events" | "source"), _)
        | (Some("breakpoint"), _)
        | (Some("process"), Some("attach"))
        | (Some("set"), Some("context" | "workspace")) => ScopeOptionKind {
            context: true,
            connection: false,
            target: false,
        },
        _ => ScopeOptionKind::default(),
    }
}

fn is_context_create(arguments: &[String]) -> bool {
    matches!(
        arguments,
        [context, create, ..] if context == "context" && create == "create"
    )
}

fn command_requires_context(arguments: &[String]) -> bool {
    let kind = scope_option_kind(arguments);
    if !kind.context {
        return false;
    }
    !matches!(
        arguments,
        [context, operation, ..]
            if context == "context" && matches!(operation.as_str(), "create" | "list")
    ) && !matches!(
        arguments,
        [set, context] if set == "set" && matches!(context.as_str(), "context" | "workspace")
    )
}

fn extract_scope_options(arguments: &mut Vec<String>) -> Result<ScopeOptions, io::Error> {
    let kind = scope_option_kind(arguments);
    let mut options = ScopeOptions::default();
    let mut index = 0;
    while index < arguments.len() {
        if arguments[index] == "--" {
            arguments.remove(index);
            break;
        }
        let slot = match arguments[index].as_str() {
            "--context" if kind.context => Some((&mut options.context, "--context")),
            "--connection" if kind.connection => Some((&mut options.connection, "--connection")),
            "--target" if kind.target => Some((&mut options.target, "--target")),
            _ => None,
        };
        let Some((slot, option)) = slot else {
            index += 1;
            continue;
        };
        if slot.is_some() {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                format!("{option} may only be specified once"),
            ));
        }
        if index + 1 >= arguments.len() {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                format!("{option} requires a value"),
            ));
        }
        *slot = Some(arguments.remove(index + 1));
        arguments.remove(index);
    }
    Ok(options)
}

fn parse_set_option(options: &[String]) -> Result<bool, io::Error> {
    match options {
        [] => Ok(false),
        [option] if option == "--set" => Ok(true),
        [option, ..] => Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            format!("unknown option '{option}'"),
        )),
    }
}

fn parse_context_create_options(
    option_expression: Option<&String>,
    options: &[String],
) -> Result<(String, Option<String>, bool), io::Error> {
    let mut expression = option_expression.cloned();
    let mut display_name = None;
    let mut set_default = false;
    for option in options {
        if option == "--set" {
            if set_default {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidInput,
                    "--set may only be specified once",
                ));
            }
            set_default = true;
        } else if option.starts_with("--") {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                format!("unknown context create option '{option}'"),
            ));
        } else if expression.is_none() {
            expression = Some(option.clone());
        } else if display_name.replace(option.clone()).is_some() {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "context create accepts one expression and at most one display name",
            ));
        }
    }
    Ok((
        expression.ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::InvalidInput,
                "context create requires a path or :<id> expression",
            )
        })?,
        display_name,
        set_default,
    ))
}

fn log_scope(scope: &ResolvedScope, snapshot: &TargetDebuggerSnapshot) -> String {
    format!(
        "{}\0{}\0{}\0{}",
        scope.context, scope.connection, scope.target, snapshot.connection_generation
    )
}

fn load_selection(path: &Path) -> Result<CliSelection, Box<dyn std::error::Error>> {
    let cwd = normalized_cwd()?;
    let store = load_selection_store(path, &cwd)?;
    let context = store
        .active_scopes
        .get(&cwd)
        .or_else(|| nearest_binding(&store, &cwd).map(|(_, context)| context));
    let mut selection = context
        .and_then(|context| store.scopes.get(&scope_key(&cwd, context)))
        .cloned()
        .unwrap_or_default();
    selection.context = context.cloned();
    Ok(selection)
}

fn write_selection(
    path: &Path,
    selection: &CliSelection,
) -> Result<(), Box<dyn std::error::Error>> {
    let cwd = normalized_cwd()?;
    let mut store = load_selection_store(path, &cwd)?;
    if let Some(context) = selection.context.as_ref() {
        store.active_scopes.insert(cwd.clone(), context.clone());
        store
            .scopes
            .insert(scope_key(&cwd, context), selection.clone());
    }
    write_selection_store(path, &store)
}

fn write_selection_store(
    path: &Path,
    store: &SelectionStore,
) -> Result<(), Box<dyn std::error::Error>> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    let mut file = AtomicWriteFile::open(path)?;
    file.write_all(&serde_json::to_vec_pretty(store)?)?;
    file.commit()?;
    Ok(())
}

fn activate_selection_scope(
    path: &Path,
    cwd: &str,
    context: &str,
) -> Result<(), Box<dyn std::error::Error>> {
    let mut store = load_selection_store(path, cwd)?;
    store
        .active_scopes
        .insert(cwd.to_owned(), context.to_owned());
    store
        .scopes
        .entry(scope_key(cwd, context))
        .or_insert_with(|| CliSelection {
            context: Some(context.to_owned()),
            ..CliSelection::default()
        });
    write_selection_store(path, &store)
}

fn load_selection_store(
    path: &Path,
    cwd: &str,
) -> Result<SelectionStore, Box<dyn std::error::Error>> {
    let bytes = match fs::read(path) {
        Ok(bytes) => bytes,
        Err(error) if error.kind() == io::ErrorKind::NotFound => {
            return Ok(SelectionStore {
                schema_version: 2,
                ..SelectionStore::default()
            });
        }
        Err(error) => return Err(error.into()),
    };
    let value: serde_json::Value = serde_json::from_slice(&bytes)?;
    if value
        .get("schemaVersion")
        .and_then(serde_json::Value::as_u64)
        == Some(2)
    {
        return Ok(serde_json::from_value(value)?);
    }

    let legacy: LegacyCliSelection = serde_json::from_value(value)?;
    let mut store = SelectionStore {
        schema_version: 2,
        ..SelectionStore::default()
    };
    if let Some(context) = legacy.context {
        let selection = CliSelection {
            context: Some(context.clone()),
            connection: legacy.connection,
            target: legacy.target,
            watches: legacy.watches,
            log_cursor: legacy.log_cursor,
            log_scope: legacy.log_scope,
        };
        store.cwd_bindings.insert(cwd.to_owned(), context.clone());
        store.active_scopes.insert(cwd.to_owned(), context.clone());
        store.scopes.insert(scope_key(cwd, &context), selection);
    }
    write_selection_store(path, &store)?;
    Ok(store)
}

fn normalized_cwd() -> Result<String, Box<dyn std::error::Error>> {
    Ok(normalize_absolute_path(&env::current_dir()?)?)
}

fn scope_key(cwd: &str, context: &str) -> String {
    format!("{cwd}\0{context}")
}

fn nearest_binding<'a>(store: &'a SelectionStore, cwd: &str) -> Option<(&'a str, &'a String)> {
    path_and_parents(cwd)
        .ok()?
        .into_iter()
        .find_map(|directory| {
            store
                .cwd_bindings
                .get_key_value(&directory)
                .map(|(binding, context)| (binding.as_str(), context))
        })
}

#[derive(Debug, PartialEq, Eq)]
struct ScreenshotCaptureOptions {
    output: Option<std::path::PathBuf>,
}

#[derive(Debug, PartialEq, Eq)]
struct ProcessAttachOptions {
    process_id: u32,
    set_default: bool,
}

fn parse_context_option(arguments: &[String]) -> Result<Option<String>, io::Error> {
    match arguments {
        [] => Ok(None),
        [flag, context_id] if flag == "--context" => Ok(Some(context_id.clone())),
        [flag] if flag == "--context" => Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "--context requires a value",
        )),
        _ => Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "expected at most --context <id>",
        )),
    }
}

fn selected_or_explicit_context(
    selection_file: &Path,
    explicit_context: Option<String>,
) -> Result<String, Box<dyn std::error::Error>> {
    explicit_context
        .or(load_selection(selection_file)?.context)
        .ok_or_else(|| io::Error::other("no context is selected; use --context <id>").into())
}

async fn resolve_implicit_context(
    client: &DebuggerServiceApiClient,
    selection_file: &Path,
    cwd: &str,
) -> Result<String, Box<dyn std::error::Error>> {
    let store = load_selection_store(selection_file, cwd)?;
    let contexts = rpc(client.list_contexts(Some(cwd.to_owned())).await)?;
    select_implicit_context(&store, cwd, &contexts)
}

fn select_implicit_context(
    store: &SelectionStore,
    cwd: &str,
    contexts: &[ContextSummary],
) -> Result<String, Box<dyn std::error::Error>> {
    if let Some((binding, context)) = nearest_binding(&store, cwd) {
        if contexts.iter().any(|candidate| candidate.id == *context) {
            return Ok(context.clone());
        }
        return Err(io::Error::other(format!(
            "stale context binding at '{binding}' refers to missing context '{context}'; replace it with 'jsdbg set context --context <expression>' from that directory"
        ))
        .into());
    }
    contexts
        .iter()
        .find(|context| {
            context.kind == ContextKind::Path && context.path_ancestor == Some(true)
        })
        .map(|context| context.id.clone())
        .ok_or_else(|| {
            io::Error::other(
                "no context applies to the current directory; use --context <path|:id>, create a path context, or set a cwd binding",
            )
            .into()
        })
}

fn selected_or_explicit_connection(
    selection_file: &Path,
    options: &ScopeOptions,
) -> Result<(String, String), Box<dyn std::error::Error>> {
    let selection = load_selection(selection_file)?;
    let context = options
        .context
        .clone()
        .or(selection.context.clone())
        .ok_or_else(|| io::Error::other("no context is selected; use --context <id>"))?;
    let use_selection = selection.context.as_deref() == Some(context.as_str());
    let connection = options
        .connection
        .clone()
        .or_else(|| use_selection.then_some(selection.connection).flatten())
        .ok_or_else(|| {
            io::Error::other("no connection is selected; use --connection <id> or --set")
        })?;
    Ok((context, connection))
}

fn parse_process_attach_options(arguments: &[String]) -> Result<ProcessAttachOptions, io::Error> {
    let mut process_id = None;
    let mut set_default = false;
    let mut index = 0;
    while index < arguments.len() {
        match arguments[index].as_str() {
            "--set" => {
                if set_default {
                    return Err(io::Error::new(
                        io::ErrorKind::InvalidInput,
                        "--set may only be specified once",
                    ));
                }
                set_default = true;
                index += 1;
            }
            argument if argument.starts_with("--") => {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidInput,
                    format!("unknown process attach option '{argument}'"),
                ));
            }
            argument => {
                if process_id.is_some() {
                    return Err(io::Error::new(
                        io::ErrorKind::InvalidInput,
                        "process attach accepts exactly one process ID",
                    ));
                }
                process_id = Some(parse_u32("process ID", argument)?);
                index += 1;
            }
        }
    }
    Ok(ProcessAttachOptions {
        process_id: process_id.ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::InvalidInput,
                "process attach requires a process ID",
            )
        })?,
        set_default,
    })
}

fn parse_screenshot_capture_options(
    arguments: &[String],
) -> Result<ScreenshotCaptureOptions, io::Error> {
    let mut output = None;
    let mut index = 0;
    while index < arguments.len() {
        match arguments[index].as_str() {
            "--output" => {
                if output.is_some() {
                    return Err(io::Error::new(
                        io::ErrorKind::InvalidInput,
                        "--output may only be specified once",
                    ));
                }
                let value = arguments.get(index + 1).ok_or_else(|| {
                    io::Error::new(io::ErrorKind::InvalidInput, "--output requires a value")
                })?;
                output = Some(value.into());
                index += 2;
            }
            argument => {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidInput,
                    format!("unknown screenshot capture option '{argument}'"),
                ));
            }
        }
    }
    Ok(ScreenshotCaptureOptions { output })
}

fn default_screenshot_path() -> std::path::PathBuf {
    let timestamp = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis();
    std::env::temp_dir()
        .join("jsdbg-screenshots")
        .join(format!("screenshot-{}-{timestamp}.png", std::process::id()))
}

fn write_binary_file(path: &Path, bytes: &[u8]) -> Result<std::path::PathBuf, io::Error> {
    if let Some(parent) = path
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty())
    {
        fs::create_dir_all(parent)?;
    }
    let mut file = AtomicWriteFile::open(path)?;
    file.write_all(bytes)?;
    file.commit()?;
    fs::canonicalize(path)
}

fn png_dimensions(bytes: &[u8]) -> Result<(u32, u32), io::Error> {
    const PNG_SIGNATURE: &[u8; 8] = b"\x89PNG\r\n\x1a\n";
    if bytes.len() < 24 || &bytes[..8] != PNG_SIGNATURE || &bytes[12..16] != b"IHDR" {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "debug target returned an invalid PNG screenshot",
        ));
    }
    let width = u32::from_be_bytes(bytes[16..20].try_into().unwrap());
    let height = u32::from_be_bytes(bytes[20..24].try_into().unwrap());
    if width == 0 || height == 0 {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "debug target returned an empty PNG screenshot",
        ));
    }
    Ok((width, height))
}

fn select_context(path: &Path, context_id: &str) -> Result<(), Box<dyn std::error::Error>> {
    let cwd = normalized_cwd()?;
    let mut store = load_selection_store(path, &cwd)?;
    store
        .cwd_bindings
        .insert(cwd.clone(), context_id.to_owned());
    store
        .active_scopes
        .insert(cwd.clone(), context_id.to_owned());
    store
        .scopes
        .entry(scope_key(&cwd, context_id))
        .or_insert_with(|| CliSelection {
            context: Some(context_id.to_owned()),
            ..CliSelection::default()
        });
    write_selection_store(path, &store)
}

fn select_scope(
    path: &Path,
    scope: &ResolvedScope,
    snapshot: &TargetDebuggerSnapshot,
) -> Result<(), Box<dyn std::error::Error>> {
    let mut selection = load_selection(path)?;
    apply_scope_selection(
        &mut selection,
        scope,
        snapshot.logs.last().map_or(0, |message| message.index),
        log_scope(scope, snapshot),
    );
    write_selection(path, &selection)
}

fn apply_scope_selection(
    selection: &mut CliSelection,
    scope: &ResolvedScope,
    log_cursor: u64,
    log_scope: String,
) {
    if selection.context.as_deref() != Some(scope.context.as_str()) {
        selection.watches.clear();
    }
    selection.context = Some(scope.context.clone());
    selection.connection = Some(scope.connection.clone());
    selection.target = Some(scope.target.clone());
    selection.log_cursor = log_cursor;
    selection.log_scope = Some(log_scope);
}

async fn resolve_scope(
    client: &DebuggerServiceApiClient,
    selection: &CliSelection,
    options: &ScopeOptions,
) -> Result<ResolvedScope, Box<dyn std::error::Error>> {
    let context = match options.context.as_ref().or(selection.context.as_ref()) {
        Some(context) => context.clone(),
        None => {
            return Err(
                "no context applies to the current directory; use --context <path|:id>".into(),
            );
        }
    };
    let snapshot = rpc(client.get_context(context.clone()).await)?;
    Ok(resolve_target_scope(
        context, &snapshot, selection, options,
    )?)
}

fn resolve_target_scope(
    context: String,
    snapshot: &cdp_client::service_api::ContextSnapshot,
    selection: &CliSelection,
    options: &ScopeOptions,
) -> Result<ResolvedScope, io::Error> {
    let use_selection = selection.context.as_deref() == Some(context.as_str());
    let selected_connection = use_selection
        .then(|| selection.connection.as_ref())
        .flatten();
    let selected_target = use_selection.then(|| selection.target.as_ref()).flatten();
    let requested_connection = options.connection.as_ref().or(selected_connection);
    let requested_target = options.target.as_ref().or(selected_target);
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
    if connected.is_empty() {
        return Err(io::Error::other(format!(
            "context '{context}' has no connected connection"
        )));
    }

    let candidate_connections = match requested_connection {
        Some(connection_id) => vec![
            connected
                .iter()
                .copied()
                .find(|connection| connection.id == *connection_id)
                .ok_or_else(|| {
                    io::Error::new(
                        io::ErrorKind::NotFound,
                        format!(
                            "connected connection '{connection_id}' does not exist in context '{context}'"
                        ),
                    )
                })?,
        ],
        None => connected,
    };
    let candidates = candidate_connections
        .iter()
        .flat_map(|connection| {
            connection.targets.iter().filter_map(|target| {
                let matches = requested_target.is_none_or(|selector| {
                    target.target_id == *selector
                        || target.target_type == *selector
                        || target.title == *selector
                        || target.url == *selector
                });
                matches.then_some((connection.id.as_str(), target.target_id.as_str()))
            })
        })
        .collect::<Vec<_>>();
    let (connection, target) = match candidates.as_slice() {
        [(connection, target)] => ((*connection).to_owned(), (*target).to_owned()),
        [] => {
            let selector = requested_target.map_or("<unspecified>", String::as_str);
            return Err(io::Error::new(
                io::ErrorKind::NotFound,
                format!(
                    "target selector '{selector}' did not match a target in context '{context}'"
                ),
            ));
        }
        _ => {
            let selector = requested_target.map_or("<unspecified>", String::as_str);
            let connections = candidates
                .iter()
                .map(|(connection, _)| *connection)
                .collect::<std::collections::BTreeSet<_>>();
            let hint = if connections.len() > 1 {
                " use --connection <id> to disambiguate"
            } else {
                " use --target <selector> to select one"
            };
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                format!(
                    "target selector '{selector}' is ambiguous across {} targets in context '{context}';{hint}",
                    candidates.len()
                ),
            ));
        }
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
                    object_id: None,
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
    trim_width: bool,
}

struct CpuProfileShowOptions {
    capture_id: String,
    path: Option<String>,
    view: CpuProfileView,
    sort: CpuProfileSort,
    max_lines: usize,
    no_cache: bool,
}

struct CpuProfileExportOptions {
    capture_id: String,
    output: String,
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

fn parse_source_list_options(values: &[String]) -> Result<Option<String>, io::Error> {
    let mut path = None;
    let mut index = 0;
    while index < values.len() {
        match values[index].as_str() {
            "--path" => {
                index += 1;
                path = Some(required_source_option(values, index, "--path")?.to_owned());
            }
            option if option.starts_with("--") => {
                return Err(invalid_option("source list", option));
            }
            value => return Err(unexpected_argument("source list", value)),
        }
        index += 1;
    }
    Ok(path)
}

fn parse_source_path_arguments(values: &[String], command: &str) -> Result<String, io::Error> {
    match values {
        [path] => Ok(path.clone()),
        _ => Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            format!("{command} requires <path>"),
        )),
    }
}

fn parse_source_show_options(
    values: &[String],
) -> Result<(String, SourceDisplayOptions), io::Error> {
    let mut positional = Vec::new();
    let mut line = None;
    let mut context_lines = 20_u32;
    let mut index = 0;
    while index < values.len() {
        match values[index].as_str() {
            "--line" => {
                index += 1;
                line = Some(parse_positive_u32(
                    "--line",
                    required_source_option(values, index, "--line")?,
                )?);
            }
            "--context-lines" => {
                index += 1;
                context_lines = parse_u32_value(
                    "--context-lines",
                    required_source_option(values, index, "--context-lines")?,
                )?;
            }
            option if option.starts_with("--") => {
                return Err(invalid_option("source show", option));
            }
            value => positional.push(value.to_owned()),
        }
        index += 1;
    }
    let path = parse_source_path_arguments(&positional, "source show")?;
    Ok((
        path,
        SourceDisplayOptions {
            line,
            context_lines,
        },
    ))
}

fn parse_source_grep_options(values: &[String]) -> Result<SourceSearchOptions, io::Error> {
    let mut positional = Vec::new();
    let mut path = None;
    let mut regex = false;
    let mut case_sensitive = true;
    let mut max_results = 200_u32;
    let mut context_lines = 0_u32;
    let mut index = 0;
    while index < values.len() {
        match values[index].as_str() {
            "--path" => {
                index += 1;
                path = Some(required_source_option(values, index, "--path")?.to_owned());
            }
            "--regex" => regex = true,
            "--ignore-case" => case_sensitive = false,
            "--max-results" => {
                index += 1;
                max_results = parse_positive_u32(
                    "--max-results",
                    required_source_option(values, index, "--max-results")?,
                )?;
            }
            "--context-lines" => {
                index += 1;
                context_lines = parse_u32_value(
                    "--context-lines",
                    required_source_option(values, index, "--context-lines")?,
                )?;
            }
            option if option.starts_with("--") => {
                return Err(invalid_option("source grep", option));
            }
            value => positional.push(value.to_owned()),
        }
        index += 1;
    }
    let pattern = match positional.as_slice() {
        [pattern] => pattern.clone(),
        _ => {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "source grep requires <pattern>",
            ));
        }
    };
    Ok(SourceSearchOptions {
        pattern,
        path,
        regex,
        case_sensitive,
        max_results,
        context_lines,
    })
}

fn parse_source_map_arguments(values: &[String]) -> Result<(String, u32, u32), io::Error> {
    let (path, line, column) = match values {
        [path, line, column] => (path, line, column),
        _ => {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "source map requires <path> <line> <column>",
            ));
        }
    };
    Ok((
        path.clone(),
        parse_positive_u32("line", line)?,
        parse_positive_u32("column", column)?,
    ))
}

fn required_source_option<'a>(
    values: &'a [String],
    index: usize,
    option: &str,
) -> Result<&'a str, io::Error> {
    values.get(index).map(String::as_str).ok_or_else(|| {
        io::Error::new(
            io::ErrorKind::InvalidInput,
            format!("{option} requires a value"),
        )
    })
}

fn parse_positive_u32(name: &str, value: &str) -> Result<u32, io::Error> {
    let parsed = parse_u32_value(name, value)?;
    if parsed == 0 {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            format!("{name} must be positive"),
        ));
    }
    Ok(parsed)
}

fn parse_u32_value(name: &str, value: &str) -> Result<u32, io::Error> {
    value.parse().map_err(|error| {
        io::Error::new(
            io::ErrorKind::InvalidInput,
            format!("invalid {name} value: {error}"),
        )
    })
}

fn invalid_option(command: &str, option: &str) -> io::Error {
    io::Error::new(
        io::ErrorKind::InvalidInput,
        format!("unknown {command} option '{option}'"),
    )
}

fn unexpected_argument(command: &str, value: &str) -> io::Error {
    io::Error::new(
        io::ErrorKind::InvalidInput,
        format!("unexpected {command} argument '{value}'"),
    )
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
    trim_width: bool,
}

const DEFAULT_HEAP_STRING_LENGTH: u32 = 160;

struct HeapSelectOptions {
    capture_id: String,
    selector: HeapNodeSelector,
    max_string_length: Option<u32>,
    include_dominators: bool,
}

struct HeapReferenceOptions {
    direction: HeapReferenceDirection,
    edge_policy: HeapEdgePolicy,
    limit: u32,
    max_string_length: Option<u32>,
}

struct HeapPathCliOptions {
    path: HeapPathOptions,
    max_string_length: Option<u32>,
}

struct HeapAggregateOptions {
    capture_id: String,
    by: HeapAggregateBy,
    limit: u32,
    max_string_length: Option<u32>,
}

struct HeapDiffOptions {
    by: HeapAggregateBy,
    limit: u32,
    max_string_length: Option<u32>,
}

fn parse_heap_select_options(values: &[String]) -> Result<HeapSelectOptions, io::Error> {
    let mut capture_id = None;
    let mut selector = HeapNodeSelector::default();
    let mut max_string_length = Some(DEFAULT_HEAP_STRING_LENGTH);
    let mut include_dominators = false;
    let mut index = 0;
    while index < values.len() {
        let option = values[index].as_str();
        let target = match option {
            "--id" => Some(&mut selector.heap_object_id),
            "--type" => Some(&mut selector.node_type),
            "--name" => Some(&mut selector.name),
            "--name-regex" => Some(&mut selector.name_regex),
            "--string-grep" => Some(&mut selector.string_contains),
            "--string-regex" => Some(&mut selector.string_regex),
            _ => None,
        };
        if let Some(target) = target {
            index += 1;
            *target = Some(
                values
                    .get(index)
                    .ok_or_else(|| {
                        io::Error::new(
                            io::ErrorKind::InvalidInput,
                            format!("{option} requires a value"),
                        )
                    })?
                    .clone(),
            );
        } else {
            match option {
                "--min-size" => {
                    index += 1;
                    selector.min_shallow_size =
                        Some(parse_u64_option(values, index, "--min-size")?);
                }
                "--max-size" => {
                    index += 1;
                    selector.max_shallow_size =
                        Some(parse_u64_option(values, index, "--max-size")?);
                }
                "--limit" => {
                    index += 1;
                    selector.limit = Some(parse_u32_option(values, index, "--limit")?);
                }
                "--dominators" => include_dominators = true,
                "--full-strings" => max_string_length = None,
                "--max-string-length" => {
                    index += 1;
                    max_string_length =
                        Some(parse_u32_option(values, index, "--max-string-length")?);
                }
                value if value.starts_with("--") => {
                    return Err(io::Error::new(
                        io::ErrorKind::InvalidInput,
                        format!("unknown heap select option '{value}'"),
                    ));
                }
                value if capture_id.is_none() => capture_id = Some(value.to_owned()),
                value => {
                    return Err(io::Error::new(
                        io::ErrorKind::InvalidInput,
                        format!("unexpected heap select argument '{value}'"),
                    ));
                }
            }
        }
        index += 1;
    }
    Ok(HeapSelectOptions {
        capture_id: capture_id.unwrap_or_else(|| ".".to_owned()),
        selector,
        max_string_length,
        include_dominators,
    })
}

fn parse_heap_string_options(values: &[String]) -> Result<HeapSelectOptions, io::Error> {
    let mut capture_id = ".".to_owned();
    let mut string_contains = None;
    let mut string_regex = None;
    let mut limit = 100;
    let mut max_string_length = Some(DEFAULT_HEAP_STRING_LENGTH);
    let mut index = 0;
    while index < values.len() {
        match values[index].as_str() {
            "--capture" => {
                index += 1;
                capture_id = values
                    .get(index)
                    .ok_or_else(|| {
                        io::Error::new(io::ErrorKind::InvalidInput, "--capture requires a name")
                    })?
                    .clone();
            }
            option @ ("--grep" | "--regex") => {
                index += 1;
                let value = values
                    .get(index)
                    .ok_or_else(|| {
                        io::Error::new(
                            io::ErrorKind::InvalidInput,
                            format!("{option} requires a pattern"),
                        )
                    })?
                    .clone();
                if option == "--grep" {
                    string_contains = Some(value);
                } else {
                    string_regex = Some(value);
                }
            }
            "--limit" => {
                index += 1;
                limit = parse_u32_option(values, index, "--limit")?;
            }
            "--full-strings" => max_string_length = None,
            "--max-string-length" => {
                index += 1;
                max_string_length = Some(parse_u32_option(values, index, "--max-string-length")?);
            }
            option => {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidInput,
                    format!("unknown heap strings option '{option}'"),
                ));
            }
        }
        index += 1;
    }
    if string_contains.is_some() == string_regex.is_some() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "heap strings requires exactly one of --grep <text> or --regex <regex>",
        ));
    }
    Ok(HeapSelectOptions {
        capture_id,
        selector: HeapNodeSelector {
            string_contains,
            string_regex,
            limit: Some(limit),
            ..HeapNodeSelector::default()
        },
        max_string_length,
        include_dominators: false,
    })
}

fn parse_heap_reference_options(values: &[String]) -> Result<HeapReferenceOptions, io::Error> {
    let mut options = HeapReferenceOptions {
        direction: HeapReferenceDirection::Outgoing,
        edge_policy: HeapEdgePolicy::Strong,
        limit: 100,
        max_string_length: Some(DEFAULT_HEAP_STRING_LENGTH),
    };
    let mut index = 0;
    while index < values.len() {
        match values[index].as_str() {
            "--incoming" => options.direction = HeapReferenceDirection::Incoming,
            "--outgoing" => options.direction = HeapReferenceDirection::Outgoing,
            "--both" => options.direction = HeapReferenceDirection::Both,
            "--all-edges" => options.edge_policy = HeapEdgePolicy::All,
            "--limit" => {
                index += 1;
                options.limit = parse_u32_option(values, index, "--limit")?;
            }
            "--full-strings" => options.max_string_length = None,
            "--max-string-length" => {
                index += 1;
                options.max_string_length =
                    Some(parse_u32_option(values, index, "--max-string-length")?);
            }
            option => {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidInput,
                    format!("unknown heap refs option '{option}'"),
                ));
            }
        }
        index += 1;
    }
    Ok(options)
}

fn parse_heap_path_options(values: &[String]) -> Result<HeapPathCliOptions, io::Error> {
    let mut options = HeapPathCliOptions {
        path: HeapPathOptions::default(),
        max_string_length: Some(DEFAULT_HEAP_STRING_LENGTH),
    };
    let mut index = 0;
    while index < values.len() {
        match values[index].as_str() {
            "--direction" => {
                index += 1;
                options.path.direction = match values.get(index).map(String::as_str) {
                    Some("outgoing") => HeapPathDirection::Outgoing,
                    Some("incoming") => HeapPathDirection::Incoming,
                    Some("either") => HeapPathDirection::Either,
                    Some(value) => {
                        return Err(io::Error::new(
                            io::ErrorKind::InvalidInput,
                            format!("unknown heap path direction '{value}'"),
                        ));
                    }
                    None => {
                        return Err(io::Error::new(
                            io::ErrorKind::InvalidInput,
                            "--direction requires outgoing, incoming, or either",
                        ));
                    }
                };
            }
            "--all-edges" => options.path.edge_policy = HeapEdgePolicy::All,
            "--readable" => options.path.cost = HeapPathCost::Readable,
            "--full-strings" => options.max_string_length = None,
            "--max-string-length" => {
                index += 1;
                options.max_string_length =
                    Some(parse_u32_option(values, index, "--max-string-length")?);
            }
            option => {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidInput,
                    format!("unknown heap path option '{option}'"),
                ));
            }
        }
        index += 1;
    }
    Ok(options)
}

fn parse_heap_aggregate_options(values: &[String]) -> Result<HeapAggregateOptions, io::Error> {
    let mut capture_id = None;
    let mut by = HeapAggregateBy::NodeType;
    let mut limit = 100;
    let mut max_string_length = Some(DEFAULT_HEAP_STRING_LENGTH);
    let mut index = 0;
    while index < values.len() {
        match values[index].as_str() {
            "--by" => {
                index += 1;
                by = parse_heap_aggregate_by(values.get(index))?;
            }
            "--limit" => {
                index += 1;
                limit = parse_u32_option(values, index, "--limit")?;
            }
            "--full-strings" => max_string_length = None,
            "--max-string-length" => {
                index += 1;
                max_string_length = Some(parse_u32_option(values, index, "--max-string-length")?);
            }
            option if option.starts_with("--") => {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidInput,
                    format!("unknown heap aggregate option '{option}'"),
                ));
            }
            value if capture_id.is_none() => capture_id = Some(value.to_owned()),
            value => {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidInput,
                    format!("unexpected heap aggregate argument '{value}'"),
                ));
            }
        }
        index += 1;
    }
    Ok(HeapAggregateOptions {
        capture_id: capture_id.unwrap_or_else(|| ".".to_owned()),
        by,
        limit,
        max_string_length,
    })
}

fn parse_heap_diff_options(values: &[String]) -> Result<HeapDiffOptions, io::Error> {
    let mut by = HeapAggregateBy::NodeType;
    let mut limit = 100;
    let mut max_string_length = Some(DEFAULT_HEAP_STRING_LENGTH);
    let mut index = 0;
    while index < values.len() {
        match values[index].as_str() {
            "--by" => {
                index += 1;
                by = parse_heap_aggregate_by(values.get(index))?;
            }
            "--limit" => {
                index += 1;
                limit = parse_u32_option(values, index, "--limit")?;
            }
            "--full-strings" => max_string_length = None,
            "--max-string-length" => {
                index += 1;
                max_string_length = Some(parse_u32_option(values, index, "--max-string-length")?);
            }
            option => {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidInput,
                    format!("unknown heap diff option '{option}'"),
                ));
            }
        }
        index += 1;
    }
    Ok(HeapDiffOptions {
        by,
        limit,
        max_string_length,
    })
}

fn parse_heap_aggregate_by(value: Option<&String>) -> Result<HeapAggregateBy, io::Error> {
    match value.map(String::as_str) {
        Some("type") => Ok(HeapAggregateBy::NodeType),
        Some("name") => Ok(HeapAggregateBy::Name),
        Some("string") => Ok(HeapAggregateBy::StringValue),
        Some(value) => Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            format!("unknown heap aggregate key '{value}'"),
        )),
        None => Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "--by requires type, name, or string",
        )),
    }
}

fn parse_heap_string_display_options(values: &[String]) -> Result<Option<u32>, io::Error> {
    let mut max_string_length = Some(DEFAULT_HEAP_STRING_LENGTH);
    let mut index = 0;
    while index < values.len() {
        match values[index].as_str() {
            "--full-strings" => max_string_length = None,
            "--max-string-length" => {
                index += 1;
                max_string_length = Some(parse_u32_option(values, index, "--max-string-length")?);
            }
            option => {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidInput,
                    format!("unknown heap display option '{option}'"),
                ));
            }
        }
        index += 1;
    }
    Ok(max_string_length)
}

fn parse_u32_option(values: &[String], index: usize, option: &str) -> Result<u32, io::Error> {
    values
        .get(index)
        .ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::InvalidInput,
                format!("{option} requires an unsigned integer"),
            )
        })?
        .parse()
        .map_err(|error| {
            io::Error::new(
                io::ErrorKind::InvalidInput,
                format!("invalid {option} value: {error}"),
            )
        })
}

fn parse_u64_option(values: &[String], index: usize, option: &str) -> Result<u64, io::Error> {
    values
        .get(index)
        .ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::InvalidInput,
                format!("{option} requires an unsigned integer"),
            )
        })?
        .parse()
        .map_err(|error| {
            io::Error::new(
                io::ErrorKind::InvalidInput,
                format!("invalid {option} value: {error}"),
            )
        })
}

fn split_heap_reference_cli(reference: &str) -> Result<(String, String), io::Error> {
    let (capture_id, heap_object_id) = reference.rsplit_once('#').ok_or_else(|| {
        io::Error::new(
            io::ErrorKind::InvalidInput,
            format!("heap reference '{reference}' must use <capture>#<heap-object-id>"),
        )
    })?;
    if capture_id.is_empty() || heap_object_id.parse::<u64>().is_err() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            format!("invalid heap reference '{reference}'"),
        ));
    }
    Ok((capture_id.to_owned(), heap_object_id.to_owned()))
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
    let mut trim_width = true;
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
            "--no-trim" => trim_width = false,
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
        trim_width,
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
    let mut trim_width = true;
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
            "--no-trim" => trim_width = false,
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
        trim_width,
    })
}

fn parse_cpu_profile_start_options(values: &[String]) -> Result<Option<u64>, io::Error> {
    match values {
        [] => Ok(None),
        [option, value] if option == "--sampling-interval" => {
            parse_cpu_profile_sampling_interval(value).map(Some)
        }
        [option] if option == "--sampling-interval" => Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "--sampling-interval requires a duration",
        )),
        [option, ..] => Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            format!("unknown profile start option '{option}'"),
        )),
    }
}

fn parse_cpu_profile_sampling_interval(value: &str) -> Result<u64, io::Error> {
    let value = value.trim().to_ascii_lowercase();
    let (number, multiplier, unit) = if let Some(number) = value.strip_suffix("us") {
        (number, 1.0, "us")
    } else if let Some(number) = value.strip_suffix("ms") {
        (number, 1_000.0, "ms")
    } else if let Some(number) = value.strip_suffix('s') {
        (number, 1_000_000.0, "s")
    } else {
        (value.as_str(), 1_000.0, "ms")
    };
    let number = number.parse::<f64>().map_err(|error| {
        io::Error::new(
            io::ErrorKind::InvalidInput,
            format!("invalid sampling interval '{value}': {error}"),
        )
    })?;
    let micros = number * multiplier;
    if !micros.is_finite() || micros <= 0.0 {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "sampling interval must be positive",
        ));
    }
    if micros.fract() != 0.0 {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            format!("sampling interval must resolve to a whole number of microseconds ({unit})"),
        ));
    }
    if micros > i32::MAX as f64 {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "sampling interval exceeds the runtime maximum of 2147483647us",
        ));
    }
    Ok(micros as u64)
}

fn parse_cpu_profile_stop_options(values: &[String]) -> Result<Option<String>, io::Error> {
    match values {
        [] => Ok(None),
        [option, capture_id] if option == "--id" && !capture_id.is_empty() => {
            Ok(Some(capture_id.clone()))
        }
        [option] if option == "--id" => Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "--id requires a name",
        )),
        [option, ..] => Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            format!("unknown profile stop option '{option}'"),
        )),
    }
}

fn parse_cpu_profile_show_options(values: &[String]) -> Result<CpuProfileShowOptions, io::Error> {
    let mut capture_id = None;
    let mut path = None;
    let mut view = CpuProfileView::Functions;
    let mut sort = CpuProfileSort::SelfTime;
    let mut max_lines = 80_usize;
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
            "--view" => {
                index += 1;
                view = match values.get(index).map(String::as_str) {
                    Some("functions") => CpuProfileView::Functions,
                    Some("files") => CpuProfileView::Files,
                    Some(value) => {
                        return Err(io::Error::new(
                            io::ErrorKind::InvalidInput,
                            format!("unsupported profile view '{value}'"),
                        ));
                    }
                    None => {
                        return Err(io::Error::new(
                            io::ErrorKind::InvalidInput,
                            "--view requires 'functions' or 'files'",
                        ));
                    }
                };
            }
            "--sort" => {
                index += 1;
                sort = match values.get(index).map(String::as_str) {
                    Some("self") => CpuProfileSort::SelfTime,
                    Some("total") => CpuProfileSort::TotalTime,
                    Some(value) => {
                        return Err(io::Error::new(
                            io::ErrorKind::InvalidInput,
                            format!("unsupported profile sort '{value}'"),
                        ));
                    }
                    None => {
                        return Err(io::Error::new(
                            io::ErrorKind::InvalidInput,
                            "--sort requires 'self' or 'total'",
                        ));
                    }
                };
            }
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
                if max_lines < 3 {
                    return Err(io::Error::new(
                        io::ErrorKind::InvalidInput,
                        "--max-lines must be at least 3",
                    ));
                }
            }
            "--no-cache" => no_cache = true,
            option if option.starts_with("--") => {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidInput,
                    format!("unknown profile show option '{option}'"),
                ));
            }
            value if capture_id.is_none() => capture_id = Some(value.to_owned()),
            value => {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidInput,
                    format!("unexpected profile show argument '{value}'"),
                ));
            }
        }
        index += 1;
    }
    Ok(CpuProfileShowOptions {
        capture_id: capture_id.unwrap_or_else(|| ".".to_owned()),
        path,
        view,
        sort,
        max_lines,
        no_cache,
    })
}

fn parse_cpu_profile_export_options(
    values: &[String],
) -> Result<CpuProfileExportOptions, io::Error> {
    let mut capture_id = None;
    let mut output = None;
    let mut index = 0;
    while index < values.len() {
        match values[index].as_str() {
            "--output" => {
                index += 1;
                output = Some(
                    values
                        .get(index)
                        .ok_or_else(|| {
                            io::Error::new(io::ErrorKind::InvalidInput, "--output requires a path")
                        })?
                        .clone(),
                );
            }
            option if option.starts_with("--") => {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidInput,
                    format!("unknown profile export option '{option}'"),
                ));
            }
            value if capture_id.is_none() => capture_id = Some(value.to_owned()),
            value => {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidInput,
                    format!("unexpected profile export argument '{value}'"),
                ));
            }
        }
        index += 1;
    }
    Ok(CpuProfileExportOptions {
        capture_id: capture_id.unwrap_or_else(|| ".".to_owned()),
        output: output.ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::InvalidInput,
                "profile export requires --output <path>",
            )
        })?,
    })
}

fn cpu_profile_export(profile: &CpuProfileSnapshot) -> serde_json::Value {
    let nodes = profile
        .nodes
        .iter()
        .map(|node| {
            let mut value = serde_json::Map::new();
            value.insert("id".to_owned(), serde_json::json!(node.id));
            value.insert(
                "callFrame".to_owned(),
                serde_json::json!({
                    "functionName": node.call_frame.function_name,
                    "scriptId": node.call_frame.script_id,
                    "url": node.call_frame.url,
                    "lineNumber": node.call_frame.line_number,
                    "columnNumber": node.call_frame.column_number,
                }),
            );
            if let Some(hit_count) = node.hit_count {
                value.insert("hitCount".to_owned(), serde_json::json!(hit_count));
            }
            if !node.children.is_empty() {
                value.insert("children".to_owned(), serde_json::json!(node.children));
            }
            if let Some(deopt_reason) = &node.deopt_reason {
                value.insert("deoptReason".to_owned(), serde_json::json!(deopt_reason));
            }
            if !node.position_ticks.is_empty() {
                value.insert(
                    "positionTicks".to_owned(),
                    serde_json::json!(node.position_ticks),
                );
            }
            serde_json::Value::Object(value)
        })
        .collect::<Vec<_>>();
    serde_json::json!({
        "nodes": nodes,
        "startTime": profile.start_time_micros,
        "endTime": profile.end_time_micros,
        "samples": profile.samples,
        "timeDeltas": profile.time_deltas_micros,
    })
}

async fn put_selected_breakpoint(
    breakpoint_id: &str,
    source_path: &str,
    line: &str,
    column: &str,
    selection_file: &Path,
    state_file: &Path,
    scope_options: &ScopeOptions,
    output: OutputFormat,
) -> Result<(), Box<dyn std::error::Error>> {
    let line = line.parse::<u32>()?;
    let column = column.parse::<u32>()?;
    let client = ensure_service(state_file).await?;
    let selection = load_selection(selection_file)?;
    let context_id = scope_options
        .context
        .clone()
        .or(selection.context.clone())
        .ok_or_else(|| io::Error::other("no context is selected; use --context <id>"))?;
    let current = rpc(client.get_context(context_id.clone()).await)?;
    let explicit_target_scope =
        scope_options.connection.is_some() || scope_options.target.is_some();
    let scope = match resolve_target_scope(context_id.clone(), &current, &selection, scope_options)
    {
        Ok(scope) => Some(scope),
        Err(_) if !explicit_target_scope => None,
        Err(error) => return Err(error.into()),
    };
    let context = rpc(client
        .put_breakpoint(
            context_id,
            breakpoint_id.to_owned(),
            source_path.to_owned(),
            line,
            column,
        )
        .await)?;
    if let Some(scope) = scope {
        let snapshot = rpc(client
            .wait_target(
                scope.context,
                scope.connection,
                scope.target.clone(),
                TargetWaitPredicate::BreakpointInstalled {
                    breakpoint_id: breakpoint_id.to_owned(),
                },
                30_000,
            )
            .await)?;
        output.print_target_with_breakpoint_sources(
            &snapshot,
            &scope.target,
            &[breakpoint_id.to_owned()],
        )?;
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
    selection_file: &std::path::Path,
    set_default: bool,
    output: OutputFormat,
) -> Result<(), Box<dyn std::error::Error>> {
    if set_default && !connect_now {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "--set requires --connect so a target can be selected",
        )
        .into());
    }
    let client = ensure_service(state_file).await?;
    let configured = rpc(client
        .put_connection(
            context_id.to_owned(),
            connection_id.to_owned(),
            configuration,
        )
        .await)?;
    if connect_now {
        let mut connected = rpc(client
            .connect_connection(context_id.to_owned(), connection_id.to_owned())
            .await)?;
        if set_default {
            let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(30);
            let target_id = loop {
                let connection = connected
                    .connections
                    .iter()
                    .find(|connection| connection.id == connection_id)
                    .ok_or_else(|| {
                        io::Error::new(
                            io::ErrorKind::NotFound,
                            format!("connected context has no connection '{connection_id}'"),
                        )
                    })?;
                let attached = connection
                    .targets
                    .iter()
                    .filter(|target| target.attached)
                    .collect::<Vec<_>>();
                match attached.as_slice() {
                    [target] => break target.target_id.clone(),
                    [] if tokio::time::Instant::now() < deadline => {
                        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
                        connected = rpc(client.get_context(context_id.to_owned()).await)?;
                    }
                    [] => {
                        return Err(io::Error::new(
                            io::ErrorKind::TimedOut,
                            format!(
                                "connection '{connection_id}' did not attach a target within 30 seconds"
                            ),
                        )
                        .into());
                    }
                    targets => {
                        return Err(io::Error::other(format!(
                            "connection '{connection_id}' has {} attached targets; select one explicitly",
                            targets.len()
                        ))
                        .into());
                    }
                }
            };
            let snapshot = rpc(client
                .get_target(
                    context_id.to_owned(),
                    connection_id.to_owned(),
                    target_id.clone(),
                )
                .await)?;
            select_scope(
                selection_file,
                &ResolvedScope {
                    context: context_id.to_owned(),
                    connection: connection_id.to_owned(),
                    target: target_id,
                },
                &snapshot,
            )?;
        }
        output.print(&connected)?;
    } else {
        output.print(&configured)?;
    }
    Ok(())
}

struct PlaywrightOptions {
    channel: PlaywrightChannel,
    headless: bool,
    connect: bool,
    set_default: bool,
    ignore_https_errors: bool,
}

fn parse_playwright_options(options: &[String]) -> Result<PlaywrightOptions, io::Error> {
    let mut parsed = PlaywrightOptions {
        channel: PlaywrightChannel::Bundled,
        headless: true,
        connect: false,
        set_default: false,
        ignore_https_errors: false,
    };
    let mut index = 0;
    while index < options.len() {
        match options[index].as_str() {
            "--connect" => parsed.connect = true,
            "--set" => parsed.set_default = true,
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

struct ChromeOptions {
    executable: String,
    headless: bool,
    connect: bool,
    set_default: bool,
    user_data_dir: Option<String>,
    args: Vec<String>,
}

fn parse_chrome_options(options: &[String]) -> Result<ChromeOptions, io::Error> {
    let mut executable = None;
    let mut parsed = ChromeOptions {
        executable: String::new(),
        headless: true,
        connect: false,
        set_default: false,
        user_data_dir: None,
        args: Vec::new(),
    };
    let mut index = 0;
    while index < options.len() {
        match options[index].as_str() {
            "--connect" => parsed.connect = true,
            "--set" => parsed.set_default = true,
            "--headed" => parsed.headless = false,
            "--executable" => {
                index += 1;
                executable = Some(
                    options
                        .get(index)
                        .ok_or_else(|| {
                            io::Error::new(
                                io::ErrorKind::InvalidInput,
                                "--executable requires a value",
                            )
                        })?
                        .clone(),
                );
            }
            "--user-data-dir" => {
                index += 1;
                parsed.user_data_dir = Some(
                    options
                        .get(index)
                        .ok_or_else(|| {
                            io::Error::new(
                                io::ErrorKind::InvalidInput,
                                "--user-data-dir requires a value",
                            )
                        })?
                        .clone(),
                );
            }
            "--arg" => {
                index += 1;
                parsed.args.push(
                    options
                        .get(index)
                        .ok_or_else(|| {
                            io::Error::new(io::ErrorKind::InvalidInput, "--arg requires a value")
                        })?
                        .clone(),
                );
            }
            option => {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidInput,
                    format!("unknown Chrome connection option '{option}'"),
                ));
            }
        }
        index += 1;
    }
    parsed.executable = executable.ok_or_else(|| {
        io::Error::new(
            io::ErrorKind::InvalidInput,
            "--chrome requires --executable <path>",
        )
    })?;
    Ok(parsed)
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

struct ProcessListOptions {
    command_line: bool,
    stats: bool,
    filter: Option<String>,
    trim_width: bool,
}

fn parse_process_list_options(arguments: &[String]) -> Result<ProcessListOptions, io::Error> {
    let mut result = ProcessListOptions {
        command_line: true,
        stats: false,
        filter: None,
        trim_width: true,
    };
    let mut vscode = false;
    let mut index = 0;
    while index < arguments.len() {
        match arguments[index].as_str() {
            "--vscode" if !vscode => vscode = true,
            "--vscode" => {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidInput,
                    "--vscode may only be specified once",
                ));
            }
            "--no-cmd-line" => result.command_line = false,
            "--stats" => result.stats = true,
            "--no-trim" => result.trim_width = false,
            "--filter" => {
                if result.filter.is_some() {
                    return Err(io::Error::new(
                        io::ErrorKind::InvalidInput,
                        "--filter may only be specified once",
                    ));
                }
                index += 1;
                result.filter = Some(arguments.get(index).cloned().ok_or_else(|| {
                    io::Error::new(io::ErrorKind::InvalidInput, "--filter requires a tree path")
                })?);
            }
            argument => {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidInput,
                    format!("unknown process list option: {argument}"),
                ));
            }
        }
        index += 1;
    }
    if !vscode {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "process list requires --vscode",
        ));
    }
    Ok(result)
}

fn parse_u64(name: &str, value: &str) -> Result<u64, io::Error> {
    value.parse().map_err(|error| {
        io::Error::new(
            io::ErrorKind::InvalidInput,
            format!("invalid {name} '{value}': {error}"),
        )
    })
}

fn parse_u32(name: &str, value: &str) -> Result<u32, io::Error> {
    parse_u64(name, value)?
        .try_into()
        .map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, format!("{name} is too large")))
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
  jsdbg process list --vscode [--no-cmd-line] [--stats] [--filter <tree-path>] [--no-trim]
  jsdbg process attach <process-id> [--context <id>] [--set]
  jsdbg context list
  jsdbg context create <path|:id> [display-name] [--set]
  jsdbg context show [--context <path|:id>]
  jsdbg context delete [--context <path|:id>] [--expected-revision <revision>] [--request-id <id>]
  jsdbg state get [--context <id>]
  jsdbg state watch [--context <id>] [--after-revision <revision>]
  jsdbg events --after-revision <revision> [--context <id>]
  jsdbg set context --context <id>
  jsdbg set target --target <selector> [--context <id>] [--connection <id>]
  jsdbg connection add <ws-endpoint> --connection <id> [--context <id>] [--connect]
  jsdbg connection add --node-inspector <ws-endpoint> --connection <id> [--context <id>] --connect
  jsdbg connection add --process <process-id> --connection <id> [--context <id>] --connect
  jsdbg connection add --process-tree <root-pid> --connection <id> [--context <id>] --connect
  jsdbg connection add --playwright <url> --connection <id> [--context <id>] [--channel <channel>] [--headed] [--ignore-https-errors] [--connect] [--set]
  jsdbg connection add --chrome <url> --connection <id> [--context <id>] --executable <path> [--headed] [--user-data-dir <path>] [--arg <value>]... [--connect] [--set]
  jsdbg connection connect|disconnect [--context <id>] [--connection <id>]
  jsdbg connection delete [--context <id>] [--connection <id>] [--expected-revision <revision>] [--request-id <id>]
  jsdbg breakpoint set <breakpoint-id> <source-url> <line> [--column <column>] [--context <id>]
  jsdbg breakpoint configure <breakpoint-id> <source-url> <line> <column> [--context <id>] [--disabled] [--condition <expression>] [--target <target>] [--expected-revision <revision>] [--request-id <id>]
  jsdbg breakpoint delete <breakpoint-id> [--context <id>] [--expected-revision <revision>] [--request-id <id>]
  jsdbg source list [--path <substring>] [--context <id>]
  jsdbg source resolve|endpoints|explain <path> [--context <id>]
  jsdbg source graph|map show [--context <id>]
  jsdbg source show <path> [--line <line>] [--context-lines <lines>] [--context <id>]
  jsdbg source grep <pattern> [--path <substring>] [--regex] [--ignore-case] [--max-results <count>] [--context-lines <lines>] [--context <id>]
  jsdbg source map <path> <line> <column> [--context <id>]
  jsdbg source cache evict [--context <id>]
  jsdbg source export <destination> [--context <id>]
  jsdbg target show [target scope]
  jsdbg target attach [target scope] [--set]
  jsdbg target wait breakpoint-installed <breakpoint-id> [timeout-ms] [target scope]
  jsdbg target wait paused <after-epoch> [timeout-ms] [target scope]
  jsdbg target wait running [target scope]
  jsdbg target resume [--epoch <epoch>] [target scope]
  jsdbg target step into|over|out [--epoch <epoch>] [target scope]
  jsdbg target eval|watch <expression> [target scope]
  jsdbg target logpoint <id> <source> <line> <column> <expression> [target scope]
  jsdbg target logpoints (<id> <source> <line> <column> <expression>)+ [target scope]
  jsdbg log [--after <cursor>] [--limit <count>] [target scope]
  jsdbg target click <css-selector> [target scope]
  jsdbg target key <ctrl+n|ctrl+k,ctrl+m|ctrl+k,n|enter|accept|arrowup> [target scope]
  jsdbg target type <text> [target scope]
  jsdbg screenshot capture [--output <path>] [target scope]
  jsdbg coverage start [target scope]
  jsdbg coverage capture [--id <name>] [target scope]
  jsdbg coverage stop [--exclude <name>] [target scope]
  jsdbg coverage show [<name>] [--path <source-prefix>] [--max-lines <count>] [--all] [--no-cache] [--no-trim] [target scope]
  jsdbg profile start [--sampling-interval <duration>] [target scope]
  jsdbg profile stop [--id <name>] [target scope]
  jsdbg profile show [<name>] [--view <functions|files>] [--sort <self|total>] [--path <source-prefix>] [--max-lines <count>] [--no-cache] [target scope]
  jsdbg profile export [<name>] --output <path> [target scope]
  jsdbg heap capture [--id <name>] [--capture-numeric-value] [--expose-internals] [target scope]
  jsdbg heap classes [<name>] [--capture] [--filter <regex>] [--sort-by-instances] [--instances] [--max-lines <count>] [--all] [--no-cache] [--no-trim]
  jsdbg heap select [<capture>] [--id <heap-object-id>] [--type <kind>] [--name <text>|--name-regex <regex>] [--string-grep <text>|--string-regex <regex>] [--min-size <bytes>] [--max-size <bytes>] [--limit <count>] [--dominators] [--full-strings]
  jsdbg heap strings (--grep <text>|--regex <regex>) [--capture <name>] [--limit <count>] [--full-strings]
  jsdbg heap show <capture#heap-object-id> [--full-strings]
  jsdbg heap refs <capture#heap-object-id> [--incoming|--outgoing|--both] [--all-edges] [--limit <count>]
  jsdbg heap path <from-ref> <to-ref> [--direction <outgoing|incoming|either>] [--all-edges] [--readable]
  jsdbg heap root-path|retainer-path|dominators <capture#heap-object-id>
  jsdbg heap aggregate [<capture>] [--by <type|name|string>] [--limit <count>] [--full-strings]
  jsdbg heap diff <older-capture> <newer-capture> [--by <type|name|string>] [--limit <count>] [--full-strings]
  jsdbg heap snapshot <path> [--capture-numeric-value] [--expose-internals] [target scope]

target scope:
  [--context <id>] [--target <selector>] [--connection <id>]
  Accepted by target, log, screenshot, coverage, profile, and heap commands.
  --connection is only needed when the target selector is ambiguous."
}

#[cfg(test)]
mod tests {
    use super::{
        CliSelection, ResolvedScope, ScopeOptions, SelectionStore, activate_selection_scope,
        apply_scope_selection, extract_scope_options, load_selection_store, parse_chrome_options,
        parse_context_create_options, parse_context_option, parse_coverage_show_options,
        parse_cpu_profile_sampling_interval, parse_cpu_profile_start_options,
        parse_heap_capture_options, parse_heap_class_options, parse_heap_path_options,
        parse_heap_select_options, parse_heap_string_options, parse_process_attach_options,
        parse_process_list_options, parse_screenshot_capture_options, parse_source_grep_options,
        parse_source_map_arguments, parse_source_show_options, png_dimensions,
        resolve_target_scope, select_implicit_context, split_heap_reference_cli,
    };
    use cdp_client::context_identity::ContextKind;
    use cdp_client::service_api::{
        ConnectionConfiguration, ConnectionSnapshot, ConnectionStatus, ContextSnapshot,
        ContextSummary, HeapEdgePolicy, HeapPathCost, HeapPathDirection, TargetSnapshot,
    };
    use std::fs;

    fn arguments(values: &[&str]) -> Vec<String> {
        values.iter().map(|value| (*value).to_owned()).collect()
    }

    #[test]
    fn parses_screenshot_capture_output() {
        let options =
            parse_screenshot_capture_options(&arguments(&["--output", "renderer.png"])).unwrap();
        assert_eq!(
            options.output,
            Some(std::path::PathBuf::from("renderer.png"))
        );
        assert_eq!(parse_screenshot_capture_options(&[]).unwrap().output, None);
    }

    #[test]
    fn parses_selected_and_explicit_context_options() {
        assert_eq!(parse_context_option(&[]).unwrap(), None);
        assert_eq!(
            parse_context_option(&arguments(&["--context", "renderer"])).unwrap(),
            Some("renderer".to_owned())
        );
        assert!(parse_context_option(&arguments(&["renderer"])).is_err());
    }

    #[test]
    fn parses_process_attach_scope_as_an_option() {
        let mut args = arguments(&[
            "process",
            "attach",
            "15388",
            "--context",
            "linkrpc-ext-host",
            "--set",
        ]);
        let scope = extract_scope_options(&mut args).unwrap();
        let options = parse_process_attach_options(&args[2..]).unwrap();
        assert_eq!(options.process_id, 15388);
        assert_eq!(scope.context.as_deref(), Some("linkrpc-ext-host"));
        assert!(options.set_default);
        assert!(parse_process_attach_options(&arguments(&["linkrpc-ext-host", "15388"])).is_err());
    }

    #[test]
    fn extracts_target_scope_flags_without_positional_scope() {
        let mut args = arguments(&[
            "target",
            "show",
            "--target",
            "$node-root",
            "--context",
            "brave",
            "--connection",
            "process-42",
        ]);
        let scope = extract_scope_options(&mut args).unwrap();
        assert_eq!(args, arguments(&["target", "show"]));
        assert_eq!(
            scope,
            ScopeOptions {
                context: Some("brave".to_owned()),
                connection: Some("process-42".to_owned()),
                target: Some("$node-root".to_owned()),
            }
        );

        let mut positional = arguments(&["target", "show", "brave", "process-42", "$node-root"]);
        assert_eq!(
            extract_scope_options(&mut positional).unwrap(),
            ScopeOptions::default()
        );
        assert_eq!(positional.len(), 5);
    }

    #[test]
    fn target_scope_infers_connection_and_reports_ambiguity() {
        let snapshot =
            context_snapshot(&[("process-1", &["$node-root"]), ("renderer", &["page-1"])]);
        let scope = resolve_target_scope(
            "ctx".to_owned(),
            &snapshot,
            &CliSelection::default(),
            &ScopeOptions {
                target: Some("page-1".to_owned()),
                ..ScopeOptions::default()
            },
        )
        .unwrap();
        assert_eq!(
            scope,
            ResolvedScope {
                context: "ctx".to_owned(),
                connection: "renderer".to_owned(),
                target: "page-1".to_owned(),
            }
        );

        let duplicate = context_snapshot(&[
            ("process-1", &["$node-root"]),
            ("process-2", &["$node-root"]),
        ]);
        let options = ScopeOptions {
            target: Some("$node-root".to_owned()),
            ..ScopeOptions::default()
        };
        let error = resolve_target_scope(
            "ctx".to_owned(),
            &duplicate,
            &CliSelection::default(),
            &options,
        )
        .unwrap_err();
        assert!(error.to_string().contains("--connection <id>"));

        let scope = resolve_target_scope(
            "ctx".to_owned(),
            &duplicate,
            &CliSelection::default(),
            &ScopeOptions {
                connection: Some("process-2".to_owned()),
                ..options
            },
        )
        .unwrap();
        assert_eq!(scope.connection, "process-2");
    }

    #[test]
    fn setting_scope_persists_the_resolved_identity() {
        let scope = ResolvedScope {
            context: "ctx".to_owned(),
            connection: "process-42".to_owned(),
            target: "$node-root".to_owned(),
        };
        let mut selection = CliSelection {
            context: Some("old".to_owned()),
            watches: vec!["value".to_owned()],
            ..CliSelection::default()
        };
        apply_scope_selection(&mut selection, &scope, 17, "scope-key".to_owned());
        assert_eq!(selection.context.as_deref(), Some("ctx"));
        assert_eq!(selection.connection.as_deref(), Some("process-42"));
        assert_eq!(selection.target.as_deref(), Some("$node-root"));
        assert_eq!(selection.log_cursor, 17);
        assert_eq!(selection.log_scope.as_deref(), Some("scope-key"));
        assert!(selection.watches.is_empty());

        let legacy: CliSelection =
            serde_json::from_str(r#"{"workspace":"ctx","target":"$node-root"}"#).unwrap();
        assert_eq!(legacy.context.as_deref(), Some("ctx"));
    }

    fn context_snapshot(connections: &[(&str, &[&str])]) -> ContextSnapshot {
        ContextSnapshot {
            agent_instance_id: "agent".to_owned(),
            id: "ctx".to_owned(),
            display_name: "Context".to_owned(),
            revision: 1,
            connections: connections
                .iter()
                .map(|(connection_id, target_ids)| ConnectionSnapshot {
                    id: (*connection_id).to_owned(),
                    configuration: ConnectionConfiguration::DirectCdp {
                        endpoint: String::new(),
                    },
                    generation: 1,
                    status: ConnectionStatus::Connected {
                        product: String::new(),
                        protocol_version: String::new(),
                    },
                    targets: target_ids
                        .iter()
                        .map(|target_id| TargetSnapshot {
                            target_id: (*target_id).to_owned(),
                            target_type: "node".to_owned(),
                            title: (*target_id).to_owned(),
                            url: String::new(),
                            attached: true,
                            parent_id: None,
                            opener_id: None,
                            browser_context_id: None,
                            subtype: None,
                        })
                        .collect(),
                })
                .collect(),
            target_forest: Vec::new(),
            breakpoints: Vec::new(),
        }
    }

    #[test]
    fn rejects_invalid_screenshot_capture_options() {
        assert!(
            parse_screenshot_capture_options(&arguments(&[
                "--output", "one.png", "--output", "two.png"
            ]))
            .is_err()
        );
        assert!(parse_screenshot_capture_options(&arguments(&["--full-page"])).is_err());
    }

    #[test]
    fn reads_png_dimensions() {
        let mut png = b"\x89PNG\r\n\x1a\n\0\0\0\rIHDR".to_vec();
        png.extend_from_slice(&1920u32.to_be_bytes());
        png.extend_from_slice(&1080u32.to_be_bytes());
        assert_eq!(png_dimensions(&png).unwrap(), (1920, 1080));
        assert!(png_dimensions(b"not a png").is_err());
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
    fn parses_process_tree_output_options() {
        let options = parse_process_list_options(&arguments(&[
            "--vscode",
            "--no-cmd-line",
            "--stats",
            "--filter",
            "window 3",
            "--no-trim",
        ]))
        .unwrap();
        assert!(!options.command_line);
        assert!(options.stats);
        assert_eq!(options.filter.as_deref(), Some("window 3"));
        assert!(!options.trim_width);
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
            "--no-trim",
        ]))
        .unwrap();
        assert_eq!(options.capture_id, "startup");
        assert!(options.capture);
        assert_eq!(options.filter.as_deref(), Some(".*PieceTree.*"));
        assert!(options.sort_by_instances);
        assert!(options.instances);
        assert_eq!(options.max_lines, 42);
        assert!(options.no_cache);
        assert!(!options.trim_width);
    }

    #[test]
    fn parses_composable_heap_selector_options() {
        let options = parse_heap_select_options(&arguments(&[
            "startup",
            "--type",
            "string",
            "--string-regex",
            "session.*title",
            "--min-size",
            "16",
            "--limit",
            "25",
            "--dominators",
            "--full-strings",
        ]))
        .unwrap();
        assert_eq!(options.capture_id, "startup");
        assert_eq!(options.selector.node_type.as_deref(), Some("string"));
        assert_eq!(
            options.selector.string_regex.as_deref(),
            Some("session.*title")
        );
        assert_eq!(options.selector.min_shallow_size, Some(16));
        assert_eq!(options.selector.limit, Some(25));
        assert!(options.include_dominators);
        assert_eq!(options.max_string_length, None);
    }

    #[test]
    fn parses_heap_string_search_and_path_policies() {
        let strings = parse_heap_string_options(&arguments(&[
            "--capture",
            "live",
            "--regex",
            "Add extension launch config",
            "--limit",
            "7",
        ]))
        .unwrap();
        assert_eq!(strings.capture_id, "live");
        assert_eq!(
            strings.selector.string_regex.as_deref(),
            Some("Add extension launch config")
        );
        assert_eq!(strings.selector.limit, Some(7));

        let grep = parse_heap_string_options(&arguments(&["--grep", "extension launch"])).unwrap();
        assert_eq!(
            grep.selector.string_contains.as_deref(),
            Some("extension launch")
        );
        assert!(parse_heap_string_options(&arguments(&["extension launch"])).is_err());
        assert!(
            parse_heap_string_options(&arguments(&["--grep", "extension", "--regex", "launch"]))
                .is_err()
        );

        let path = parse_heap_path_options(&arguments(&[
            "--direction",
            "either",
            "--all-edges",
            "--readable",
        ]))
        .unwrap();
        assert_eq!(path.path.direction, HeapPathDirection::Either);
        assert_eq!(path.path.edge_policy, HeapEdgePolicy::All);
        assert_eq!(path.path.cost, HeapPathCost::Readable);
        assert_eq!(
            split_heap_reference_cli("live#123").unwrap(),
            ("live".to_owned(), "123".to_owned())
        );
    }

    #[test]
    fn parses_coverage_no_trim_option() {
        let options = parse_coverage_show_options(&arguments(&["--no-trim"])).unwrap();
        assert!(!options.trim_width);
    }

    #[test]
    fn parses_native_chrome_connection_options() {
        let options = parse_chrome_options(&arguments(&[
            "--executable",
            "C:\\Program Files\\Google\\Chrome\\Application\\chrome.exe",
            "--headed",
            "--user-data-dir",
            "C:\\tmp\\jsdbg-chrome",
            "--arg",
            "--disable-extensions",
            "--arg",
            "--window-size=1200,800",
            "--connect",
            "--set",
        ]))
        .unwrap();
        assert_eq!(
            options.executable,
            "C:\\Program Files\\Google\\Chrome\\Application\\chrome.exe"
        );
        assert!(!options.headless);
        assert!(options.connect);
        assert!(options.set_default);
        assert_eq!(
            options.user_data_dir.as_deref(),
            Some("C:\\tmp\\jsdbg-chrome")
        );
        assert_eq!(
            options.args,
            ["--disable-extensions", "--window-size=1200,800"]
        );
    }

    #[test]
    fn parses_context_creation_selection_policy_independently_of_the_name() {
        assert_eq!(
            parse_context_create_options(
                Some(&":heap".to_owned()),
                &arguments(&["Heap analysis", "--set"])
            )
            .unwrap(),
            (":heap".to_owned(), Some("Heap analysis".to_owned()), true)
        );
        assert_eq!(
            parse_context_create_options(None, &arguments(&[".", "--set"])).unwrap(),
            (".".to_owned(), None, true)
        );
    }

    #[test]
    fn implicit_context_resolution_prioritizes_bindings_and_reports_stale_ones() {
        let cwd = "/work/shop/packages/ui";
        let mut store = SelectionStore {
            schema_version: 2,
            ..SelectionStore::default()
        };
        store
            .cwd_bindings
            .insert("/work/shop".into(), "incident".into());
        let contexts = vec![
            context_summary(
                "/work/shop/packages/ui",
                ContextKind::Path,
                Some(0),
                Some(true),
            ),
            context_summary("incident", ContextKind::Named, None, None),
        ];
        assert_eq!(
            select_implicit_context(&store, cwd, &contexts).unwrap(),
            "incident"
        );

        store
            .cwd_bindings
            .insert("/work/shop/packages".into(), "missing".into());
        let error = select_implicit_context(&store, cwd, &contexts)
            .unwrap_err()
            .to_string();
        assert!(error.contains("stale context binding at '/work/shop/packages'"));
        assert!(error.contains("missing context 'missing'"));
    }

    #[test]
    fn implicit_context_resolution_has_no_sole_named_context_fallback() {
        let store = SelectionStore {
            schema_version: 2,
            ..SelectionStore::default()
        };
        let contexts = vec![context_summary("only", ContextKind::Named, None, None)];
        assert!(
            select_implicit_context(&store, "/work/unrelated", &contexts)
                .unwrap_err()
                .to_string()
                .contains("no context applies")
        );
    }

    #[test]
    fn legacy_global_selection_migrates_to_the_current_cwd_only() {
        let path = std::env::temp_dir().join(format!(
            "jsdbg-selection-migration-{}-{}.json",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        fs::write(
            &path,
            br#"{"workspace":"legacy","connection":"server","target":"main","watches":["x"]}"#,
        )
        .unwrap();
        let store = load_selection_store(&path, "/work/shop").unwrap();
        assert_eq!(
            store.cwd_bindings.get("/work/shop").map(String::as_str),
            Some("legacy")
        );
        assert!(!store.cwd_bindings.contains_key("/work/other"));
        assert_eq!(store.schema_version, 2);
        let migrated: serde_json::Value =
            serde_json::from_slice(&fs::read(&path).unwrap()).unwrap();
        assert_eq!(migrated["schemaVersion"], 2);
        let _ = fs::remove_file(path);
    }

    #[test]
    fn activating_an_explicit_context_preserves_separate_view_state() {
        let path = std::env::temp_dir().join(format!(
            "jsdbg-selection-scope-{}-{}.json",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let cwd = "/work/shop";
        let mut store = SelectionStore {
            schema_version: 2,
            ..SelectionStore::default()
        };
        store.active_scopes.insert(cwd.into(), "first".into());
        store.scopes.insert(
            super::scope_key(cwd, "first"),
            CliSelection {
                context: Some("first".into()),
                watches: vec!["firstWatch".into()],
                ..CliSelection::default()
            },
        );
        super::write_selection_store(&path, &store).unwrap();

        activate_selection_scope(&path, cwd, "second").unwrap();
        let updated = load_selection_store(&path, cwd).unwrap();
        assert_eq!(
            updated.active_scopes.get(cwd).map(String::as_str),
            Some("second")
        );
        assert_eq!(
            updated.scopes[&super::scope_key(cwd, "first")].watches,
            ["firstWatch"]
        );
        assert_eq!(
            updated.scopes[&super::scope_key(cwd, "second")]
                .context
                .as_deref(),
            Some("second")
        );
        let _ = fs::remove_file(path);
    }

    fn context_summary(
        id: &str,
        kind: ContextKind,
        path_distance: Option<u32>,
        path_ancestor: Option<bool>,
    ) -> ContextSummary {
        ContextSummary {
            agent_instance_id: "agent".into(),
            id: id.into(),
            kind,
            path_distance,
            path_ancestor,
            display_name: id.into(),
            revision: 1,
            connection_count: 0,
            breakpoint_count: 0,
        }
    }

    #[test]
    fn parses_cpu_profile_sampling_intervals() {
        assert_eq!(parse_cpu_profile_sampling_interval("1").unwrap(), 1_000);
        assert_eq!(parse_cpu_profile_sampling_interval("1ms").unwrap(), 1_000);
        assert_eq!(parse_cpu_profile_sampling_interval("500us").unwrap(), 500);
        assert_eq!(parse_cpu_profile_sampling_interval("0.5ms").unwrap(), 500);
        assert_eq!(
            parse_cpu_profile_sampling_interval("1s").unwrap(),
            1_000_000
        );
        assert!(parse_cpu_profile_sampling_interval("0").is_err());
        assert!(parse_cpu_profile_sampling_interval("0.1us").is_err());
        assert!(parse_cpu_profile_sampling_interval("-1ms").is_err());
    }

    #[test]
    fn omitted_cpu_profile_sampling_interval_uses_the_runtime_default() {
        assert_eq!(parse_cpu_profile_start_options(&[]).unwrap(), None);
        assert_eq!(
            parse_cpu_profile_start_options(&arguments(&["--sampling-interval", "1ms"])).unwrap(),
            Some(1_000)
        );
    }

    #[test]
    fn heap_classes_default_to_coverage_aligned_capture_and_limit() {
        let options = parse_heap_class_options(&[]).unwrap();
        assert_eq!(options.capture_id, ".");
        assert_eq!(options.max_lines, 300);
        assert!(!options.capture);
    }

    #[test]
    fn parses_source_grep_options() {
        let options = parse_source_grep_options(&arguments(&[
            "trim.*Whitespace",
            "--regex",
            "--ignore-case",
            "--path",
            "src/vs/editor",
            "--max-results",
            "25",
            "--context-lines",
            "2",
        ]))
        .unwrap();
        assert_eq!(options.pattern, "trim.*Whitespace");
        assert_eq!(options.path.as_deref(), Some("src/vs/editor"));
        assert!(options.regex);
        assert!(!options.case_sensitive);
        assert_eq!(options.max_results, 25);
        assert_eq!(options.context_lines, 2);
    }

    #[test]
    fn parses_source_show_and_map() {
        let (path, options) = parse_source_show_options(&arguments(&[
            "src/model.ts",
            "--line",
            "1352",
            "--context-lines",
            "12",
        ]))
        .unwrap();
        assert_eq!(path, "src/model.ts");
        assert_eq!(options.line, Some(1352));
        assert_eq!(options.context_lines, 12);

        let (path, line, column) =
            parse_source_map_arguments(&arguments(&["src/model.ts", "1352", "3"])).unwrap();
        assert_eq!(path, "src/model.ts");
        assert_eq!((line, column), (1352, 3));
    }
}
