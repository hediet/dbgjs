use std::collections::BTreeMap;
use std::env;
use std::fs;
use std::io;
use std::io::IsTerminal;
use std::io::Read;
use std::io::Write;
use std::path::Path;
use std::time::Duration;

use atomic_write_file::AtomicWriteFile;
use base64::Engine;
use dbgjs::context_identity::{
    ContextIdentity, ContextKind, normalize_absolute_path, path_and_parents,
    resolve_context_expression, synthetic_node_target_id,
};
use dbgjs::coverage_filter::CoveragePathFilter;
use dbgjs::local_rpc::{connect_existing, default_state_file, ensure_service};
use dbgjs::playwright_proxy::{CLEANUP_RESERVE, OPERATION_TIMEOUT as PLAYWRIGHT_EXECUTION_TIMEOUT};
use dbgjs::promise_debugging::{
    DEFAULT_PROMISE_LIMIT, DEFAULT_PROMISE_PREVIEW_LENGTH, DEFAULT_VALUE_PREVIEW_LENGTH,
};
use dbgjs::service_api::{
    BreakpointSpec, CaptureKind, CdpStdioTopology, ConnectionConfiguration, ConnectionStatus,
    ContextSnapshot, ContextSummary, CpuProfileSnapshot, DbgServiceClient, EvaluationSnapshot,
    HeapAggregateBy, HeapEdgePolicy, HeapNodeSelector, HeapPathCost, HeapPathDirection,
    HeapPathOptions, HeapReferenceDirection, HeapSnapshotProgress, LogpointSpec, MutationOptions,
    ObservationCursor, ObservationResult, PlaywrightChannel, ProcessRole, ProcessRootKind,
    ProcessTreeSnapshot, PromiseState, ResourceGraphSnapshot, SourceDisplayOptions,
    SourceFormattingMode, SourceSearchOptions, SourceTreeKind, SourceViewPreference, StepKind,
    TargetAttachOptions, TargetBreakpointStatus, TargetDebuggerPhase, TargetDebuggerSnapshot,
    TargetScriptStatus, TargetWaitPredicate, ValueInspectionOptions, ValueSelector,
};
use serde::{Deserialize, Serialize};
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWriteExt};
use tokio::process::Command as TokioCommand;
use tokio::sync::mpsc;

#[path = "dbgjs/bounded_tree.rs"]
mod bounded_tree;
#[path = "dbgjs/daemon_view.rs"]
mod daemon_view;
#[path = "dbgjs/output.rs"]
mod output;

use output::{
    ConnectionListEntry, ConnectionListOutput, CoverageOutputOptions, CpuProfileOutputOptions,
    CpuProfileSort, CpuProfileView, HeapClassOutputOptions, OutputFormat, ProcessTreeOutputOptions,
    SourceTreeOutputOptions, TargetListEntry, TargetListOutput,
};

const DEFAULT_VALUE_PROPERTY_LIMIT: u32 = 20;
const PLAYWRIGHT_PROGRAM_LIMIT: usize = 1024 * 1024;
const PLAYWRIGHT_OUTPUT_LIMIT: usize = 1024 * 1024 + 4096;
const PLAYWRIGHT_ERROR_LIMIT: usize = 64 * 1024;
const PLAYWRIGHT_PAGE_HELPER: &str = include_str!("../providers/playwright_page.mjs");
const COVERAGE_HINT_DELAY: Duration = Duration::from_secs(20);

fn main() {
    let thread = std::thread::Builder::new()
        .name("dbgjs-main".to_owned())
        .stack_size(16 * 1024 * 1024)
        .spawn(|| {
            let runtime = tokio::runtime::Builder::new_multi_thread()
                .enable_all()
                .build()
                .expect("failed to create the dbgjs runtime");
            let hint = coverage_delay_hint(&env::args().skip(1).collect::<Vec<_>>());
            let result = runtime.block_on(async {
                match hint {
                    Some(hint) => {
                        with_delayed_hint(run(), COVERAGE_HINT_DELAY, || eprintln!("{hint}")).await
                    }
                    None => run().await,
                }
            });
            if let Err(error) = result {
                eprintln!("dbgjs: {error}");
                std::process::exit(1);
            }
        })
        .expect("failed to start the dbgjs main thread");
    if thread.join().is_err() {
        eprintln!("dbgjs: main thread panicked");
        std::process::exit(1);
    }
}

fn coverage_delay_hint(arguments: &[String]) -> Option<&'static str> {
    if !arguments.windows(2).any(|pair| {
        pair[0] == "coverage" && matches!(pair[1].as_str(), "capture" | "take" | "show" | "stop")
    }) {
        return None;
    }
    Some(if arguments.iter().any(|argument| argument == "--raw") {
        "dbgjs: Still waiting after 20s. Raw coverage already skips source-map lookup and enrichment; the current command is continuing."
    } else {
        "dbgjs: Still waiting after 20s. For a collection-only lower bound, use `dbgjs coverage capture --raw` with the same target scope. It skips source-map lookup and symbol enrichment. The current command is continuing."
    })
}

async fn with_delayed_hint<F: std::future::Future>(
    operation: F,
    delay: Duration,
    hint: impl FnOnce(),
) -> F::Output {
    tokio::pin!(operation);
    tokio::select! {
        biased;
        result = &mut operation => result,
        _ = tokio::time::sleep(delay) => {
            hint();
            operation.await
        }
    }
}

async fn run() -> Result<(), Box<dyn std::error::Error>> {
    let mut arguments = env::args().skip(1).collect::<Vec<_>>();
    let output = OutputFormat::from_arguments(&mut arguments);
    if matches!(
        arguments.as_slice(),
        [argument] if matches!(argument.as_str(), "--version" | "-V" | "version")
    ) {
        let commit = env!("DBGJS_BUILD_GIT_COMMIT");
        let dirty = env!("DBGJS_BUILD_GIT_DIRTY").parse::<bool>().ok();
        if output.is_json() {
            println!(
                "{}",
                serde_json::to_string_pretty(&serde_json::json!({
                    "version": env!("CARGO_PKG_VERSION"),
                    "gitCommit": (commit != "unknown").then_some(commit),
                    "gitDirty": dirty,
                }))?
            );
        } else {
            let suffix = match dirty {
                Some(true) => ", dirty",
                Some(false) => "",
                None => ", dirty status unknown",
            };
            println!(
                "dbgjs {} (commit {commit}{suffix})",
                env!("CARGO_PKG_VERSION")
            );
        }
        return Ok(());
    }
    if matches!(
        arguments.as_slice(),
        [argument] if matches!(argument.as_str(), "--help" | "-h" | "help")
    ) {
        println!("{}", usage());
        return Ok(());
    }
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
        [daemon, view, options @ ..] if daemon == "daemon" && view == "view" => {
            if output.is_json() {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidInput,
                    "daemon view is a human-readable terminal view; omit --json",
                )
                .into());
            }
            let all_contexts = parse_daemon_view_options(options)?;
            if all_contexts && scope_options.context.is_some() {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidInput,
                    "--context and --all-contexts are mutually exclusive",
                )
                .into());
            }
            let client = ensure_service(&state_file).await?;
            daemon_view::run(
                &client,
                scope_options.context.as_deref(),
                all_contexts,
                io::stdout().is_terminal(),
            )
            .await?;
        }
        [set, context] if set == "set" && matches!(context.as_str(), "context" | "workspace") => {
            let context_id = required_option("--context", scope_options.context.as_ref())?;
            let client = ensure_service(&state_file).await?;
            rpc(client.contexts.get_context(context_id.clone()).await)?;
            select_context(&selection_file, &context_id)?;
            println!("Context: {context_id}");
        }
        [set, target] if set == "set" && target == "target" => {
            let selector = required_option("--target", scope_options.target.as_ref())?;
            let client = ensure_service(&state_file).await?;
            let selection = load_selection(&selection_file)?;
            let scope = resolve_scope(&client, &selection, &scope_options).await?;
            let snapshot = rpc(client.targets.get_target(scope.target_ref()).await)?;
            select_scope(&selection_file, &scope, &snapshot)?;
            println!("Target: {selector}");
        }
        [target, show] if target == "target" && show == "show" => {
            let client = ensure_service(&state_file).await?;
            let selection = load_selection(&selection_file)?;
            let scope = resolve_scope(&client, &selection, &scope_options).await?;
            let snapshot = rpc(client.targets.get_target(scope.target_ref()).await)?;
            print_target_with_watches(&output, &client, &selection, &scope, &snapshot).await?;
        }
        [target, graph] if target == "target" && graph == "graph" => {
            let context_id =
                selected_or_explicit_context(&selection_file, scope_options.context.clone())?;
            let client = ensure_service(&state_file).await?;
            output.print(&rpc(client.contexts.get_resource_graph(context_id).await)?)?;
        }
        [log, options @ ..] if log == "log" => {
            let client = ensure_service(&state_file).await?;
            let mut selection = load_selection(&selection_file)?;
            let scope = resolve_scope(&client, &selection, &scope_options).await?;
            let snapshot = rpc(client.targets.get_logs(scope.target_ref()).await)?;
            let current_scope = log_scope(
                &scope,
                &snapshot.target_id,
                snapshot.connection_generation,
                &snapshot.capture,
            );
            let persisted_cursor = if selection.log_scope.as_deref() == Some(current_scope.as_str())
            {
                selection.log_cursor
            } else {
                0
            };
            let (after, limit, explicit_after) = parse_log_options(options, persisted_cursor)?;
            let next = output.print_logs(&snapshot, after, limit)?;
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
                .targets
                .step_target(scope.target_ref(), pause_epoch, parse_step_kind(kind)?)
                .await)?;
            print_target_with_watches(&output, &client, &selection, &scope, &snapshot).await?;
        }
        [target, release] if target == "target" && release == "release" => {
            let client = ensure_service(&state_file).await?;
            let selection = load_selection(&selection_file)?;
            let scope = resolve_scope(&client, &selection, &scope_options).await?;
            let snapshot = rpc(client.targets.release_target(scope.target_ref()).await)?;
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
                .targets
                .resume_target(scope.target_ref(), pause_epoch)
                .await)?;
            print_target_with_watches(&output, &client, &selection, &scope, &snapshot).await?;
        }
        [target, eval, arguments @ ..] if target == "target" && eval == "eval" => {
            let options = parse_eval_options(arguments, io::stdin())?;
            let client = ensure_service(&state_file).await?;
            let selection = load_selection(&selection_file)?;
            let scope = resolve_scope(&client, &selection, &scope_options).await?;
            let snapshot = rpc(client.targets.get_target(scope.target_ref()).await)?;
            let value = rpc(client
                .targets
                .inspect_value(
                    scope.target_ref(),
                    pause_epoch(&snapshot),
                    ValueSelector::Expression {
                        expression: options.expression,
                        allow_side_effects: true,
                    },
                    ValueInspectionOptions {
                        max_preview_length: options.max_preview_length,
                        max_properties: DEFAULT_VALUE_PROPERTY_LIMIT,
                        retain_references: false,
                    },
                )
                .await)?;
            output.print_eval(&value, options.full)?;
        }
        [playwright, arguments @ ..] if playwright == "playwright" => {
            let program = read_playwright_program(arguments, io::stdin())?;
            let deadline = tokio::time::Instant::now() + PLAYWRIGHT_EXECUTION_TIMEOUT;
            let client =
                tokio::time::timeout_at(deadline - CLEANUP_RESERVE, ensure_service(&state_file))
                    .await
                    .map_err(|_| {
                        io::Error::new(
                            io::ErrorKind::TimedOut,
                            "Playwright starting exceeded its deadline",
                        )
                    })??;
            let scope = tokio::time::timeout_at(
                deadline - CLEANUP_RESERVE,
                resolve_scope(&client, &load_selection(&selection_file)?, &scope_options),
            )
            .await
            .map_err(|_| {
                io::Error::new(
                    io::ErrorKind::TimedOut,
                    "Playwright selecting a page exceeded its deadline",
                )
            })??;
            let context = rpc(tokio::time::timeout_at(
                deadline - CLEANUP_RESERVE,
                client.contexts.get_context(scope.context.clone()),
            )
            .await
            .map_err(|_| {
                io::Error::new(
                    io::ErrorKind::TimedOut,
                    "Playwright selecting a page exceeded its deadline",
                )
            })?)?;
            let generation = context
                .connections
                .iter()
                .find(|connection| connection.id == scope.connection)
                .ok_or_else(|| {
                    io::Error::new(
                        io::ErrorKind::NotFound,
                        format!("connection '{}' no longer exists", scope.connection),
                    )
                })?
                .generation;
            let proxy = tokio::time::timeout_at(
                deadline - CLEANUP_RESERVE,
                client
                    .relay
                    .open_playwright_proxy(scope.target_ref(), generation),
            )
            .await
            .map_err(|_| {
                io::Error::new(
                    io::ErrorKind::TimedOut,
                    "Playwright proxy opening exceeded its deadline",
                )
            })?;
            let proxy = rpc(proxy)?;
            let result =
                run_playwright_program(&proxy.websocket_url, &program, &scope.target, deadline)
                    .await;
            let cleanup_deadline = std::cmp::min(
                deadline,
                tokio::time::Instant::now() + Duration::from_secs(2),
            );
            let cleanup = tokio::time::timeout_at(
                cleanup_deadline,
                client.relay.close_playwright_proxy(proxy.id),
            )
            .await;
            let cleanup_error = match cleanup {
                Ok(Ok(_)) => None,
                Ok(Err(error)) => Some(format!("Playwright proxy cleanup failed: {error}")),
                Err(_) => Some("Playwright proxy cleanup exceeded its deadline".to_owned()),
            };
            if let Some(value) = playwright_result_with_cleanup(result, cleanup_error)? {
                println!("{}", serde_json::to_string_pretty(&value)?);
            }
        }
        [target, cdp, method, options @ ..] if target == "target" && cdp == "cdp" => {
            let options = parse_raw_cdp_options(options)?;
            let client = ensure_service(&state_file).await?;
            let scope =
                resolve_scope(&client, &load_selection(&selection_file)?, &scope_options).await?;
            let result = if let Some(session_id) = options.session_id {
                rpc(client
                    .cdp
                    .raw_cdp_session_request(
                        scope.target_ref(),
                        session_id,
                        method.clone(),
                        options.params,
                        options.validate,
                    )
                    .await)?
            } else {
                rpc(client
                    .cdp
                    .raw_cdp_request(
                        scope.target_ref(),
                        method.clone(),
                        options.params,
                        options.validate,
                    )
                    .await)?
            };
            println!("{}", serde_json::to_string_pretty(&result)?);
        }
        [target, relay, options @ ..] if target == "target" && relay == "relay" => {
            parse_relay_options(options)?;
            let client = ensure_service(&state_file).await?;
            let scope =
                resolve_scope(&client, &load_selection(&selection_file)?, &scope_options).await?;
            let relay = rpc(client.relay.open_target_relay(scope.target_ref()).await)?;
            let result = run_relay_stdio(&relay.websocket_url).await;
            let _ = client.relay.close_relay(relay.id).await;
            result?;
        }
        [value, arguments @ ..] if value == "value" => {
            let options = parse_value_options(arguments)?;
            let client = ensure_service(&state_file).await?;
            let scope =
                resolve_scope(&client, &load_selection(&selection_file)?, &scope_options).await?;
            let snapshot = rpc(client.targets.get_target(scope.target_ref()).await)?;
            let value = rpc(client
                .targets
                .inspect_value(
                    scope.target_ref(),
                    pause_epoch(&snapshot),
                    options.selector,
                    ValueInspectionOptions {
                        max_preview_length: options.max_preview_length,
                        max_properties: options.max_properties,
                        retain_references: true,
                    },
                )
                .await)?;
            output.print(&value)?;
        }
        [target, logpoint, delete, id]
            if target == "target" && logpoint == "logpoint" && delete == "delete" =>
        {
            let client = ensure_service(&state_file).await?;
            let scope =
                resolve_scope(&client, &load_selection(&selection_file)?, &scope_options).await?;
            let result = rpc(client
                .targets
                .remove_logpoint(scope.target_ref(), id.clone())
                .await)?;
            output.print(&result)?;
        }
        [target, logpoint, id, source, line, column, expression, options @ ..]
            if target == "target" && logpoint == "logpoint" =>
        {
            let install = parse_logpoint_install_options(options)?;
            let client = ensure_service(&state_file).await?;
            let scope =
                resolve_scope(&client, &load_selection(&selection_file)?, &scope_options).await?;
            let mut snapshot = rpc(client
                .targets
                .set_logpoints(
                    scope.target_ref(),
                    vec![parse_logpoint_spec(id, source, line, column, expression)?],
                )
                .await)?;
            if let Some(timeout_ms) = install {
                snapshot = require_logpoints_installed(
                    &client, &scope, snapshot, &[format!("log:{id}")], timeout_ms,
                ).await?;
            }
            output.print_target_with_breakpoint_sources(
                &snapshot,
                &scope.target,
                &[format!("log:{id}")],
            )?;
        }
        [target, logpoints, specifications @ ..]
            if target == "target" && logpoints == "logpoints" =>
        {
            let (specifications, install) = split_logpoint_install_options(specifications)?;
            let logpoints = parse_logpoint_specs(specifications)?;
            let ids = logpoints
                .iter()
                .map(|logpoint| format!("log:{}", logpoint.id))
                .collect::<Vec<_>>();
            let client = ensure_service(&state_file).await?;
            let scope =
                resolve_scope(&client, &load_selection(&selection_file)?, &scope_options).await?;
            let mut snapshot = rpc(client
                .targets
                .set_logpoints(scope.target_ref(), logpoints)
                .await)?;
            if let Some(timeout_ms) = install {
                snapshot = require_logpoints_installed(
                    &client, &scope, snapshot, &ids, timeout_ms,
                ).await?;
            }
            output.print_target_with_breakpoint_sources(&snapshot, &scope.target, &ids)?;
        }
        [target, click, selector] if target == "target" && click == "click" => {
            let client = ensure_service(&state_file).await?;
            let selection = load_selection(&selection_file)?;
            let scope = resolve_scope(&client, &selection, &scope_options).await?;
            rpc(client
                .browser
                .click_target(scope.target_ref(), selector.clone())
                .await)?;
            println!("Clicked {selector}");
        }
        [target, type_text, text] if target == "target" && type_text == "type" => {
            let client = ensure_service(&state_file).await?;
            let scope =
                resolve_scope(&client, &load_selection(&selection_file)?, &scope_options).await?;
            rpc(client
                .browser
                .type_target(scope.target_ref(), text.clone())
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
            let snapshot = rpc(client.browser.capture_screenshot(scope.target_ref()).await)?;
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
            rpc(client.coverage.start_coverage(scope.target_ref()).await)?;
            println!("Coverage recording started.");
        }
        [coverage, capture, options @ ..]
            if coverage == "coverage" && matches!(capture.as_str(), "capture" | "take") =>
        {
            let options = parse_coverage_capture_options(options)?;
            warn_deprecated_coverage_path(options.deprecated_path);
            let client = ensure_service(&state_file).await?;
            let scope =
                resolve_scope(&client, &load_selection(&selection_file)?, &scope_options).await?;
            let snapshot = rpc(client
                .coverage
                .take_coverage(
                    scope.target_ref(),
                    options.capture_id.clone(),
                    Some(options.raw),
                )
                .await)?;
            let capture_id = snapshot.capture_id.as_deref().ok_or_else(|| {
                io::Error::new(
                    io::ErrorKind::InvalidData,
                    "service returned a coverage capture without its durable captureId",
                )
            })?;
            if !options.render_requested {
                output.print_coverage_capture(&snapshot, capture_id)?;
            } else {
                output.print_coverage(
                    &snapshot,
                    CoverageOutputOptions {
                        path: options.path.as_deref(),
                        path_glob: options.path_glob.as_deref(),
                        all: options.all,
                        max_lines: options.max_lines,
                        trim_width: options.trim_width,
                    },
                )?;
            }
        }
        [coverage, stop, options @ ..] if coverage == "coverage" && stop == "stop" => {
            let options = parse_coverage_stop_options(options)?;
            let client = ensure_service(&state_file).await?;
            let scope =
                resolve_scope(&client, &load_selection(&selection_file)?, &scope_options).await?;
            let snapshot = rpc(client
                .coverage
                .stop_coverage(scope.target_ref(), options.capture_id)
                .await)?;
            let capture_id = snapshot.capture_id.as_deref().ok_or_else(|| {
                io::Error::new(
                    io::ErrorKind::InvalidData,
                    "service returned stopped coverage without its durable captureId",
                )
            })?;
            output.print_coverage_stopped(capture_id)?;
        }
        [coverage, show, options @ ..] if coverage == "coverage" && show == "show" => {
            let options = parse_coverage_show_options(options)?;
            warn_deprecated_coverage_path(options.deprecated_path);
            let client = ensure_service(&state_file).await?;
            let context =
                selected_or_explicit_context(&selection_file, scope_options.context.clone())?;
            show_stored_coverage(&client, &output, context, &scope_options, options).await?;
        }
        [profile, start, options @ ..] if profile == "profile" && start == "start" => {
            let sampling_interval_micros = parse_cpu_profile_start_options(options)?;
            let client = ensure_service(&state_file).await?;
            let scope =
                resolve_scope(&client, &load_selection(&selection_file)?, &scope_options).await?;
            rpc(client
                .cpu
                .start_cpu_profile(scope.target_ref(), sampling_interval_micros)
                .await)?;
            output.print_cpu_profile_started(sampling_interval_micros)?;
        }
        [profile, stop, options @ ..] if profile == "profile" && stop == "stop" => {
            let capture_id = parse_cpu_profile_stop_options(options)?;
            let client = ensure_service(&state_file).await?;
            let scope =
                resolve_scope(&client, &load_selection(&selection_file)?, &scope_options).await?;
            let profile = rpc(client
                .cpu
                .stop_cpu_profile(scope.target_ref(), capture_id)
                .await)?;
            output.print_cpu_profile_stopped(&profile)?;
        }
        [profile, show, options @ ..] if profile == "profile" && show == "show" => {
            let options = parse_cpu_profile_show_options(options)?;
            let client = ensure_service(&state_file).await?;
            let context =
                selected_or_explicit_context(&selection_file, scope_options.context.clone())?;
            show_stored_cpu_profile(&client, &output, context, &scope_options, options).await?;
        }
        [profile, export, options @ ..] if profile == "profile" && export == "export" => {
            let options = parse_cpu_profile_export_options(options)?;
            let destination = absolute_path(Path::new(&options.output))?;
            let client = ensure_service(&state_file).await?;
            let context =
                selected_or_explicit_context(&selection_file, scope_options.context.clone())?;
            let profile = rpc(client
                .captures
                .get_stored_cpu_profile(
                    context,
                    options.capture_id,
                    None,
                    scope_options.target.clone(),
                    scope_options.connection.clone(),
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
            let call = rpc(client
                .heap
                .capture_heap_snapshot(
                    scope.target_ref(),
                    options.capture_id,
                    options.capture_numeric_value,
                    options.expose_internals,
                )
                .await)?;
            let (result, _, progress, _) = call.into_parts();
            let result = wait_for_heap_stream(&output, result, progress).await?;
            output.print(&result)?;
        }
        [heap, classes, options @ ..] if heap == "heap" && classes == "classes" => {
            let mut options = parse_heap_class_options(options)?;
            let mut capture_id = options.capture_id.clone();
            let client = ensure_service(&state_file).await?;
            let selection = load_selection(&selection_file)?;
            if options.capture {
                let scope = resolve_scope(&client, &selection, &scope_options).await?;
                let call = rpc(client
                    .heap
                    .capture_heap_snapshot(
                        scope.target_ref(),
                        (capture_id != ".").then(|| capture_id.clone()),
                        false,
                        false,
                    )
                    .await)?;
                let (result, _, progress, _) = call.into_parts();
                capture_id = wait_for_heap_stream(&output, result, progress)
                    .await?
                    .capture_id;
            }
            let context =
                selected_or_explicit_context(&selection_file, scope_options.context.clone())?;
            options.capture_id = capture_id;
            show_stored_heap_classes(&client, &output, context, &scope_options, options).await?;
        }
        [heap, supply, capture, script, hash, map] if heap == "heap" && supply == "supply-map" => {
            let map_path = absolute_path(Path::new(map))?;
            let source_map_url = url::Url::from_file_path(&map_path)
                .map_err(|_| {
                    io::Error::new(io::ErrorKind::InvalidInput, "invalid source map path")
                })?
                .to_string();
            let source_map = tokio::fs::read_to_string(map_path).await?;
            let client = ensure_service(&state_file).await?;
            let context =
                selected_or_explicit_context(&selection_file, scope_options.context.clone())?;
            rpc(client
                .captures
                .supply_stored_heap_source_map(
                    context,
                    capture.clone(),
                    dbgjs::service_api::HeapSourceMapSupply {
                        script_id: script.clone(),
                        script_hash: hash.clone(),
                        source_map_url,
                        source_map,
                    },
                )
                .await)?;
            output.print_heap_map_supplied(capture, script)?;
        }
        [capture, list] if capture == "capture" && list == "list" => {
            let client = ensure_service(&state_file).await?;
            let context =
                selected_or_explicit_context(&selection_file, scope_options.context.clone())?;
            output.print(&rpc(client.captures.list_captures(context).await)?)?;
        }
        [capture, show, name] if capture == "capture" && show == "show" => {
            let client = ensure_service(&state_file).await?;
            let context =
                selected_or_explicit_context(&selection_file, scope_options.context.clone())?;
            let capture = rpc(client
                .captures
                .get_capture(context.clone(), name.clone())
                .await)?;
            if output.is_json() {
                output.print(&capture)?;
            } else {
                // Resolve relative history once across all kinds, then read the exact stored name.
                match capture.kind {
                    CaptureKind::Coverage => {
                        show_stored_coverage(
                            &client,
                            &output,
                            context,
                            &scope_options,
                            CoverageShowOptions {
                                capture_id: capture.name,
                                ..parse_coverage_show_options(&[])?
                            },
                        )
                        .await?;
                    }
                    CaptureKind::CpuProfile => {
                        show_stored_cpu_profile(
                            &client,
                            &output,
                            context,
                            &scope_options,
                            CpuProfileShowOptions {
                                capture_id: capture.name,
                                ..parse_cpu_profile_show_options(&[])?
                            },
                        )
                        .await?;
                    }
                    CaptureKind::HeapSnapshot => {
                        show_stored_heap_classes(
                            &client,
                            &output,
                            context,
                            &scope_options,
                            HeapClassOptions {
                                capture_id: capture.name,
                                ..parse_heap_class_options(&[])?
                            },
                        )
                        .await?;
                    }
                }
            }
        }
        [capture, delete, name] if capture == "capture" && delete == "delete" => {
            let client = ensure_service(&state_file).await?;
            let context =
                selected_or_explicit_context(&selection_file, scope_options.context.clone())?;
            output.print(&rpc(client
                .captures
                .delete_capture(context, name.clone())
                .await)?)?;
        }
        [heap, select, options @ ..] if heap == "heap" && select == "select" => {
            let options = parse_heap_select_options(options)?;
            let client = ensure_service(&state_file).await?;
            let selection = load_selection(&selection_file)?;
            let scope = resolve_offline_scope(&client, &selection, &scope_options).await?;
            let selection = rpc(client
                .heap
                .select_heap_nodes(
                    scope.target_ref(),
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
            let selection = load_selection(&selection_file)?;
            let scope = resolve_offline_scope(&client, &selection, &scope_options).await?;
            let selection = rpc(client
                .heap
                .select_heap_nodes(
                    scope.target_ref(),
                    options.capture_id,
                    options.selector,
                    options.max_string_length,
                    false,
                )
                .await)?;
            output.print(&selection)?;
        }
        [promise, list, options @ ..] if promise == "promise" && list == "list" => {
            let options = parse_promise_list_options(options)?;
            let client = ensure_service(&state_file).await?;
            let scope =
                resolve_scope(&client, &load_selection(&selection_file)?, &scope_options).await?;
            let promises = rpc(client
                .heap
                .select_promises(
                    scope.target_ref(),
                    options.capture_id,
                    options.state,
                    options.limit,
                    options.max_preview_length,
                )
                .await)?;
            output.print(&promises)?;
        }
        [heap, show, reference, options @ ..] if heap == "heap" && show == "show" => {
            let options = parse_heap_show_options(options)?;
            let client = ensure_service(&state_file).await?;
            let selection = load_selection(&selection_file)?;
            let scope = resolve_offline_scope(&client, &selection, &scope_options).await?;
            let properties = rpc(client
                .heap
                .get_heap_references(
                    scope.target_ref(),
                    reference.clone(),
                    HeapReferenceDirection::Outgoing,
                    HeapEdgePolicy::All,
                    options.limit,
                    options.max_string_length,
                )
                .await)?;
            output.print_heap_show(&properties)?;
        }
        [heap, refs, reference, options @ ..] if heap == "heap" && refs == "refs" => {
            let options = parse_heap_reference_options(options)?;
            let client = ensure_service(&state_file).await?;
            let selection = load_selection(&selection_file)?;
            let scope = resolve_offline_scope(&client, &selection, &scope_options).await?;
            let references = rpc(client
                .heap
                .get_heap_references(
                    scope.target_ref(),
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
            let selection = load_selection(&selection_file)?;
            let scope = resolve_offline_scope(&client, &selection, &scope_options).await?;
            let path = rpc(client
                .heap
                .get_heap_path(
                    scope.target_ref(),
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
            let selection = load_selection(&selection_file)?;
            let scope = resolve_offline_scope(&client, &selection, &scope_options).await?;
            let root = rpc(client
                .heap
                .select_heap_nodes(
                    scope.target_ref(),
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
                .heap
                .get_heap_path(
                    scope.target_ref(),
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
            let selection = load_selection(&selection_file)?;
            let scope = resolve_offline_scope(&client, &selection, &scope_options).await?;
            let root = rpc(client
                .heap
                .select_heap_nodes(
                    scope.target_ref(),
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
                .heap
                .get_heap_path(
                    scope.target_ref(),
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
            let selection = load_selection(&selection_file)?;
            let scope = resolve_offline_scope(&client, &selection, &scope_options).await?;
            let chain = rpc(client
                .heap
                .get_heap_dominator_chain(scope.target_ref(), reference.clone(), max_string_length)
                .await)?;
            output.print(&chain)?;
        }
        [heap, aggregate, options @ ..] if heap == "heap" && aggregate == "aggregate" => {
            let options = parse_heap_aggregate_options(options)?;
            let client = ensure_service(&state_file).await?;
            let selection = load_selection(&selection_file)?;
            let scope = resolve_offline_scope(&client, &selection, &scope_options).await?;
            let aggregate = rpc(client
                .heap
                .aggregate_heap_snapshot(
                    scope.target_ref(),
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
            let selection = load_selection(&selection_file)?;
            let scope = resolve_offline_scope(&client, &selection, &scope_options).await?;
            let diff = rpc(client
                .heap
                .diff_heap_snapshots(
                    scope.target_ref(),
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
            let call = rpc(client
                .heap
                .take_heap_snapshot(
                    scope.target_ref(),
                    destination.to_string_lossy().into_owned(),
                    options.capture_numeric_value,
                    options.expose_internals,
                )
                .await)?;
            let (result, _, progress, _) = call.into_parts();
            let result = wait_for_heap_stream(&output, result, progress).await?;
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
            let snapshot = rpc(client.targets.get_target(scope.target_ref()).await)?;
            print_target_with_watches(&output, &client, &selection, &scope, &snapshot).await?;
        }
        [service, status] if service == "service" && status == "status" => {
            let client = connect_existing(&state_file).await?;
            output.print(&rpc(client.service.service_info().await)?)?;
        }
        [service, stop] if service == "service" && stop == "stop" => {
            let client = connect_existing(&state_file).await?;
            output.print(&rpc(client.service.shutdown().await)?)?;
        }
        [process, list, options @ ..] if process == "process" && list == "list" => {
            let options = parse_process_list_options(options)?;
            let mut trees =
                dbgjs::process_discovery::discover_process_trees(options.root_kind, options.stats)
                    .await?;
            if options.full {
                dbgjs::process_discovery::populate_process_tree_targets(&mut trees).await;
                if let Ok(client) = connect_existing(&state_file).await
                    && let Ok(contexts) = client.contexts.list_contexts(None).await
                {
                    let mut snapshots = Vec::new();
                    for context in contexts {
                        if let Ok(snapshot) = client.contexts.get_context(context.id).await {
                            snapshots.push(snapshot);
                        }
                    }
                    output::project_process_tree_target_attachments(&mut trees, &snapshots);
                }
            }
            output.print_process_trees(
                &trees,
                ProcessTreeOutputOptions {
                    root_kind: options.root_kind,
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
            let discovered = if matches!(
                options.locator,
                ProcessAttachLocator::VscodeProcess { .. }
                    | ProcessAttachLocator::VscodeWindow { .. }
            ) {
                dbgjs::process_discovery::discover_vscode_process_trees(false).await?
            } else {
                dbgjs::process_discovery::discover_recognized_process_trees().await?
            };
            let (connection_id, configuration, target) =
                process_attach_destination(options.locator, &discovered)?;
            let client = ensure_service(&state_file).await?;
            let context = rpc(client.contexts.get_context(context_id.clone()).await)?;
            let existing = context
                .connections
                .iter()
                .find(|connection| connection.id == connection_id);
            let connected = existing.is_some_and(|connection| {
                matches!(
                    connection.status,
                    ConnectionStatus::Connected { .. }
                        | ConnectionStatus::Connecting
                        | ConnectionStatus::Disconnecting
                )
            });
            if connected
                && existing.is_some_and(|connection| connection.configuration != configuration)
            {
                if !options.force {
                    return Err(io::Error::other(format!(
                        "target ownership conflict: connection '{connection_id}' is active with another process; retry with --force to replace it"
                    ))
                    .into());
                }
                rpc(client
                    .contexts
                    .disconnect_connection(dbgjs::service_api::ConnectionRef {
                        context_id: context_id.clone(),
                        connection_id: connection_id.clone(),
                    })
                    .await)?;
            }
            if !connected
                || existing.is_some_and(|connection| connection.configuration != configuration)
            {
                rpc(client
                    .contexts
                    .put_connection(
                        dbgjs::service_api::ConnectionRef {
                            context_id: context_id.clone(),
                            connection_id: connection_id.clone(),
                        },
                        configuration,
                    )
                    .await)?;
                rpc(client
                    .contexts
                    .connect_connection(dbgjs::service_api::ConnectionRef {
                        context_id: context_id.clone(),
                        connection_id: connection_id.clone(),
                    })
                    .await)?;
            }
            let target_id = match target {
                ProcessAttachTarget::Renderer(selector) => {
                    resolve_renderer_target_id(&client, &context_id, &connection_id, selector)
                        .await?
                }
                ProcessAttachTarget::Target(target_id) if target_id == "$node-root" => {
                    synthetic_node_target_id(&connection_id)
                }
                ProcessAttachTarget::Target(target_id) => target_id,
            };
            if target_id != synthetic_node_target_id(&connection_id) {
                let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
                loop {
                    let context = rpc(client.contexts.get_context(context_id.clone()).await)?;
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
            let result = rpc(client
                .targets
                .attach_target(
                    dbgjs::service_api::TargetRef {
                        connection: dbgjs::service_api::ConnectionRef {
                            context_id: context_id.clone(),
                            connection_id: connection_id.clone(),
                        },
                        target_id: target_id.clone(),
                    },
                    TargetAttachOptions {
                        force: options.force,
                        expected_connection_generation: None,
                    },
                )
                .await)?;
            if options.set_default {
                select_scope(
                    &selection_file,
                    &ResolvedScope {
                        context: context_id.clone(),
                        connection: connection_id,
                        target: target_id.clone(),
                    },
                    &result.target,
                )?;
            }
            output.print(&result)?;
        }
        [context, list] if context == "context" && list == "list" => {
            let client = ensure_service(&state_file).await?;
            output.print(&rpc(client
                .contexts
                .list_contexts(Some(normalized_cwd.clone()))
                .await)?)?;
        }
        [connection, list, options @ ..] if connection == "connection" && list == "list" => {
            let options = parse_connection_list_options(options)?;
            let context_id =
                selected_or_explicit_context(&selection_file, scope_options.context.clone())?;
            let client = ensure_service(&state_file).await?;
            let snapshot = rpc(client.contexts.get_context(context_id).await)?;
            let selection = load_selection(&selection_file)?;
            output.print(&connection_list_output(
                &snapshot,
                &selection,
                scope_options.connection.as_deref(),
                &options,
            ))?;
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
                .contexts
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
            output.print(&rpc(client.contexts.get_context(context_id).await)?)?;
        }
        [context, delete, options @ ..] if context == "context" && delete == "delete" => {
            let mutation = parse_mutation_options(options)?;
            let context_id =
                selected_or_explicit_context(&selection_file, scope_options.context.clone())?;
            let client = ensure_service(&state_file).await?;
            let deleted = rpc(client
                .contexts
                .delete_context(context_id.clone(), mutation)
                .await)?;
            output.print_context_deleted(&context_id, deleted)?;
        }
        [context, relay, options @ ..] if context == "context" && relay == "relay" => {
            parse_relay_options(options)?;
            let context_id =
                selected_or_explicit_context(&selection_file, scope_options.context.clone())?;
            let client = ensure_service(&state_file).await?;
            let relay = rpc(client.relay.open_context_relay(context_id).await)?;
            let result = run_relay_stdio(&relay.websocket_url).await;
            let _ = client.relay.close_relay(relay.id).await;
            result?;
        }
        [state, get] if state == "state" && get == "get" => {
            let context_id =
                selected_or_explicit_context(&selection_file, scope_options.context.clone())?;
            let client = ensure_service(&state_file).await?;
            output.print(&rpc(client.contexts.get_context(context_id).await)?)?;
        }
        [state, watch, options @ ..] if state == "state" && watch == "watch" => {
            let context_id =
                selected_or_explicit_context(&selection_file, scope_options.context.clone())?;
            let client = ensure_service(&state_file).await?;
            let mut cursor = parse_observation_cursor(options)?;
            loop {
                match rpc(client
                    .contexts
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
                .contexts
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
        [connection, add, node, program, options @ ..]
            if connection == "connection" && add == "add" && node == "--node" =>
        {
            let context_id =
                selected_or_explicit_context(&selection_file, scope_options.context.clone())?;
            let connection_id = required_option("--connection", scope_options.connection.as_ref())?;
            let options = parse_node_options(options)?;
            add_connection(
                &context_id,
                connection_id,
                ConnectionConfiguration::Node {
                    program: program.clone(),
                    args: options.args,
                    cwd: options.cwd,
                    runtime_executable: options.runtime_executable,
                    runtime_args: options.runtime_args,
                    env: options.env,
                },
                options.connect,
                &state_file,
                &selection_file,
                options.set_default,
                output,
            )
            .await?;
        }
        [connection, add, stdio, options @ ..]
            if connection == "connection" && add == "add" && stdio == "--stdio" =>
        {
            let context_id =
                selected_or_explicit_context(&selection_file, scope_options.context.clone())?;
            let connection_id = required_option("--connection", scope_options.connection.as_ref())?;
            let options = parse_stdio_options(options)?;
            add_connection(
                &context_id,
                connection_id,
                ConnectionConfiguration::Stdio {
                    command: options.command,
                    args: options.args,
                    cwd: options.cwd,
                    env: options.env,
                    topology: options.topology,
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
                .contexts
                .connect_connection(dbgjs::service_api::ConnectionRef {
                    context_id: context_id,
                    connection_id: connection_id,
                })
                .await)?)?;
        }
        [connection, disconnect] if connection == "connection" && disconnect == "disconnect" => {
            let (context_id, connection_id) =
                selected_or_explicit_connection(&selection_file, &scope_options)?;
            let client = ensure_service(&state_file).await?;
            output.print(&rpc(client
                .contexts
                .disconnect_connection(dbgjs::service_api::ConnectionRef {
                    context_id: context_id,
                    connection_id: connection_id,
                })
                .await)?)?;
        }
        [connection, pause_future, mode]
            if connection == "connection" && pause_future == "pause-future" =>
        {
            let enabled = match mode.as_str() {
                "on" => true,
                "off" => false,
                _ => {
                    return Err(io::Error::new(
                        io::ErrorKind::InvalidInput,
                        "connection pause-future requires 'on' or 'off'",
                    )
                    .into());
                }
            };
            let (context_id, connection_id) =
                selected_or_explicit_connection(&selection_file, &scope_options)?;
            let client = ensure_service(&state_file).await?;
            let enabled = rpc(client
                .contexts
                .set_pause_future_children(
                    dbgjs::service_api::ConnectionRef {
                        context_id: context_id,
                        connection_id: connection_id,
                    },
                    enabled,
                )
                .await)?;
            if output.is_json() {
                println!("{}", serde_json::to_string_pretty(&enabled)?);
            } else {
                println!(
                    "Pause future child targets: {}",
                    if enabled { "on" } else { "off" }
                );
            }
        }
        [connection, delete, options @ ..] if connection == "connection" && delete == "delete" => {
            let (context_id, connection_id) =
                selected_or_explicit_connection(&selection_file, &scope_options)?;
            let client = ensure_service(&state_file).await?;
            output.print(&rpc(client
                .contexts
                .delete_connection(
                    dbgjs::service_api::ConnectionRef {
                        context_id: context_id,
                        connection_id: connection_id,
                    },
                    parse_mutation_options(options)?,
                )
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
            let context = rpc(client
                .contexts
                .put_breakpoint_spec(context_id, breakpoint_id.clone(), specification, mutation)
                .await)?;
            print_breakpoint_result(&client, &context, breakpoint_id, output).await?;
        }
        [breakpoint, delete, breakpoint_id, options @ ..]
            if breakpoint == "breakpoint" && delete == "delete" =>
        {
            let context_id =
                selected_or_explicit_context(&selection_file, scope_options.context.clone())?;
            let client = ensure_service(&state_file).await?;
            output.print(&rpc(client
                .contexts
                .delete_breakpoint(
                    context_id,
                    breakpoint_id.clone(),
                    parse_mutation_options(options)?,
                )
                .await)?)?;
        }
        [source, formatting, get]
            if source == "source" && formatting == "formatting" && get == "get" =>
        {
            let context_id =
                selected_or_explicit_context(&selection_file, scope_options.context.clone())?;
            let client = ensure_service(&state_file).await?;
            let context = rpc(client.contexts.get_context(context_id).await)?;
            output.print(&context.source_formatting)?;
        }
        [source, formatting, set, mode]
            if source == "source" && formatting == "formatting" && set == "set" =>
        {
            let context_id =
                selected_or_explicit_context(&selection_file, scope_options.context.clone())?;
            let client = ensure_service(&state_file).await?;
            output.print(&rpc(client
                .sources
                .set_source_formatting(context_id, parse_source_formatting_mode(mode)?)
                .await)?)?;
        }
        [source, formatting, rule, list]
            if source == "source"
                && formatting == "formatting"
                && rule == "rule"
                && list == "list" =>
        {
            let context_id =
                selected_or_explicit_context(&selection_file, scope_options.context.clone())?;
            let client = ensure_service(&state_file).await?;
            let context = rpc(client.contexts.get_context(context_id).await)?;
            output.print(&context.source_formatting)?;
        }
        [source, formatting, rule, add, arguments @ ..]
            if source == "source"
                && formatting == "formatting"
                && rule == "rule"
                && add == "add" =>
        {
            let (mode, target_pattern, url_pattern) = parse_source_formatting_rule(arguments)?;
            let context_id =
                selected_or_explicit_context(&selection_file, scope_options.context.clone())?;
            let client = ensure_service(&state_file).await?;
            output.print(&rpc(client
                .sources
                .add_source_formatting_rule(context_id, mode, target_pattern, url_pattern)
                .await)?)?;
        }
        [source, formatting, rule, remove, rule_id]
            if source == "source"
                && formatting == "formatting"
                && rule == "rule"
                && remove == "remove" =>
        {
            let context_id =
                selected_or_explicit_context(&selection_file, scope_options.context.clone())?;
            let client = ensure_service(&state_file).await?;
            output.print(&rpc(client
                .sources
                .delete_source_formatting_rule(context_id, rule_id.clone())
                .await)?)?;
        }
        [source, list, options @ ..] if source == "source" && list == "list" => {
            let context_id =
                selected_or_explicit_context(&selection_file, scope_options.context.clone())?;
            let client = ensure_service(&state_file).await?;
            output.print(&rpc(client
                .sources
                .list_sources(context_id, parse_source_list_options(options)?)
                .await)?)?;
        }
        [source, tree, arguments @ ..] if source == "source" && tree == "tree" => {
            let (kind, options) = parse_source_tree_options(arguments)?;
            let context_id =
                selected_or_explicit_context(&selection_file, scope_options.context.clone())?;
            let client = ensure_service(&state_file).await?;
            let tree = rpc(client.sources.show_source_tree(context_id, kind).await)?;
            output.print_source_tree(&tree, options)?;
        }
        [source, resolve, arguments @ ..] if source == "source" && resolve == "resolve" => {
            let path = parse_source_path_arguments(arguments, "source resolve")?;
            let client = ensure_service(&state_file).await?;
            let context_id =
                selected_or_explicit_context(&selection_file, scope_options.context.clone())?;
            output.print(&rpc(client
                .sources
                .resolve_sources(context_id, path)
                .await)?)?;
        }
        [source, endpoints, arguments @ ..] if source == "source" && endpoints == "endpoints" => {
            let path = parse_source_path_arguments(arguments, "source endpoints")?;
            let client = ensure_service(&state_file).await?;
            let context_id =
                selected_or_explicit_context(&selection_file, scope_options.context.clone())?;
            output.print(&rpc(client
                .sources
                .list_sources(context_id, Some(path))
                .await)?)?;
        }
        [source, show, arguments @ ..] if source == "source" && show == "show" => {
            let (path, options) = parse_source_show_options(arguments)?;
            let client = ensure_service(&state_file).await?;
            let context_id =
                selected_or_explicit_context(&selection_file, scope_options.context.clone())?;
            output.print(&rpc(client
                .sources
                .show_source(context_id, path, options)
                .await)?)?;
        }
        [source, grep, arguments @ ..] if source == "source" && grep == "grep" => {
            let (options, max_output_bytes, max_line_bytes, verbose_diagnostics) =
                parse_source_grep_cli_options(arguments)?;
            let client = ensure_service(&state_file).await?;
            let context_id =
                selected_or_explicit_context(&selection_file, scope_options.context.clone())?;
            let snapshot = rpc(client
                .sources
                .grep_sources(context_id, options.clone())
                .await)?;
            output.print_source_search(
                &snapshot, max_output_bytes, max_line_bytes, verbose_diagnostics,
                options.path.as_deref(),
            )?;
        }
        [source, explain, arguments @ ..] if source == "source" && explain == "explain" => {
            let path = parse_source_path_arguments(arguments, "source explain")?;
            let client = ensure_service(&state_file).await?;
            let context_id =
                selected_or_explicit_context(&selection_file, scope_options.context.clone())?;
            output.print(&rpc(client.sources.explain_source(context_id, path).await)?)?;
        }
        [source, graph] if source == "source" && graph == "graph" => {
            let client = ensure_service(&state_file).await?;
            let context_id =
                selected_or_explicit_context(&selection_file, scope_options.context.clone())?;
            output.print(&rpc(client.sources.show_source_graph(context_id).await)?)?;
        }
        [source, graph, uncompacted]
            if source == "source" && graph == "graph" && uncompacted == "--uncompacted" =>
        {
            let client = ensure_service(&state_file).await?;
            let context_id =
                selected_or_explicit_context(&selection_file, scope_options.context.clone())?;
            output.print(&rpc(client
                .sources
                .show_uncompacted_source_graph(context_id)
                .await)?)?;
        }
        [source, map, show] if source == "source" && map == "map" && show == "show" => {
            let client = ensure_service(&state_file).await?;
            let context_id =
                selected_or_explicit_context(&selection_file, scope_options.context.clone())?;
            output.print(&rpc(client.sources.show_source_graph(context_id).await)?)?;
        }
        [source, map, arguments @ ..] if source == "source" && map == "map" => {
            let (path, line, column) = parse_source_map_arguments(arguments)?;
            let client = ensure_service(&state_file).await?;
            let context_id =
                selected_or_explicit_context(&selection_file, scope_options.context.clone())?;
            output.print(&rpc(client
                .sources
                .map_source(context_id, path, line, column)
                .await)?)?;
        }
        [source, cache, evict] if source == "source" && cache == "cache" && evict == "evict" => {
            let context_id =
                selected_or_explicit_context(&selection_file, scope_options.context.clone())?;
            let client = ensure_service(&state_file).await?;
            output.print(&rpc(client.sources.evict_source_caches(context_id).await)?)?;
        }
        [source, export, destination] if source == "source" && export == "export" => {
            let context_id =
                selected_or_explicit_context(&selection_file, scope_options.context.clone())?;
            let client = ensure_service(&state_file).await?;
            output.print(&rpc(client
                .sources
                .export_sources(context_id, destination.clone())
                .await)?)?;
        }
        [target, list, options @ ..] if target == "target" && list == "list" => {
            let options = parse_target_list_options(options)?;
            let context_id =
                selected_or_explicit_context(&selection_file, scope_options.context.clone())?;
            let client = ensure_service(&state_file).await?;
            let snapshot = rpc(client.contexts.get_context(context_id).await)?;
            let selection = load_selection(&selection_file)?;
            output.print(&target_list_output(
                &snapshot,
                &selection,
                &scope_options,
                &options,
            )?)?;
        }
        [target, attach, options @ ..] if target == "target" && attach == "attach" => {
            let options = parse_attach_options(options)?;
            let client = ensure_service(&state_file).await?;
            let scope =
                resolve_scope(&client, &load_selection(&selection_file)?, &scope_options).await?;
            let result = rpc(client
                .targets
                .attach_target(
                    scope.target_ref(),
                    TargetAttachOptions {
                        force: options.force,
                        expected_connection_generation: None,
                    },
                )
                .await)?;
            if options.set_default {
                select_scope(&selection_file, &scope, &result.target)?;
            }
            output.print(&result)?;
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

impl ResolvedScope {
    fn target_ref(&self) -> dbgjs::service_api::TargetRef {
        dbgjs::service_api::TargetRef {
            connection: dbgjs::service_api::ConnectionRef {
                context_id: self.context.clone(),
                connection_id: self.connection.clone(),
            },
            target_id: self.target.clone(),
        }
    }
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

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum ConnectionStatusFilter {
    Disconnected,
    Connecting,
    Disconnecting,
    Connected,
    Failed,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum ConnectionKindFilter {
    DirectCdp,
    NodeInspector,
    Process,
    ProcessTree,
    Playwright,
    Chrome,
    Node,
    Stdio,
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
struct ConnectionListOptions {
    status: Option<ConnectionStatusFilter>,
    kind: Option<ConnectionKindFilter>,
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
struct TargetListOptions {
    target_type: Option<String>,
    title: Option<String>,
    url: Option<String>,
    attached: Option<bool>,
}

fn parse_connection_list_options(arguments: &[String]) -> Result<ConnectionListOptions, io::Error> {
    let mut options = ConnectionListOptions::default();
    let mut index = 0;
    while index < arguments.len() {
        match arguments[index].as_str() {
            "--status" => {
                let value = arguments.get(index + 1).ok_or_else(|| {
                    io::Error::new(io::ErrorKind::InvalidInput, "--status requires a value")
                })?;
                if options.status.is_some() {
                    return Err(io::Error::new(
                        io::ErrorKind::InvalidInput,
                        "--status may only be specified once",
                    ));
                }
                options.status = Some(match value.as_str() {
                    "disconnected" => ConnectionStatusFilter::Disconnected,
                    "connecting" => ConnectionStatusFilter::Connecting,
                    "disconnecting" => ConnectionStatusFilter::Disconnecting,
                    "connected" => ConnectionStatusFilter::Connected,
                    "failed" => ConnectionStatusFilter::Failed,
                    _ => {
                        return Err(io::Error::new(
                            io::ErrorKind::InvalidInput,
                            format!("unknown connection status '{value}'"),
                        ));
                    }
                });
                index += 2;
            }
            "--kind" => {
                let value = arguments.get(index + 1).ok_or_else(|| {
                    io::Error::new(io::ErrorKind::InvalidInput, "--kind requires a value")
                })?;
                if options.kind.is_some() {
                    return Err(io::Error::new(
                        io::ErrorKind::InvalidInput,
                        "--kind may only be specified once",
                    ));
                }
                options.kind = Some(match value.as_str() {
                    "direct-cdp" => ConnectionKindFilter::DirectCdp,
                    "node-inspector" => ConnectionKindFilter::NodeInspector,
                    "process" => ConnectionKindFilter::Process,
                    "process-tree" => ConnectionKindFilter::ProcessTree,
                    "playwright" => ConnectionKindFilter::Playwright,
                    "chrome" => ConnectionKindFilter::Chrome,
                    "node" => ConnectionKindFilter::Node,
                    "stdio" => ConnectionKindFilter::Stdio,
                    _ => {
                        return Err(io::Error::new(
                            io::ErrorKind::InvalidInput,
                            format!("unknown connection kind '{value}'"),
                        ));
                    }
                });
                index += 2;
            }
            argument => {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidInput,
                    format!("unknown connection list option '{argument}'"),
                ));
            }
        }
    }
    Ok(options)
}

fn parse_target_list_options(arguments: &[String]) -> Result<TargetListOptions, io::Error> {
    let mut options = TargetListOptions::default();
    let mut index = 0;
    while index < arguments.len() {
        match arguments[index].as_str() {
            "--type" | "--title" | "--url" => {
                let flag = arguments[index].as_str();
                let value = arguments.get(index + 1).ok_or_else(|| {
                    io::Error::new(
                        io::ErrorKind::InvalidInput,
                        format!("{flag} requires a value"),
                    )
                })?;
                let slot = match flag {
                    "--type" => &mut options.target_type,
                    "--title" => &mut options.title,
                    "--url" => &mut options.url,
                    _ => unreachable!(),
                };
                if slot.replace(value.clone()).is_some() {
                    return Err(io::Error::new(
                        io::ErrorKind::InvalidInput,
                        format!("{flag} may only be specified once"),
                    ));
                }
                index += 2;
            }
            "--attached" | "--unattached" => {
                if options.attached.is_some() {
                    return Err(io::Error::new(
                        io::ErrorKind::InvalidInput,
                        "--attached and --unattached are mutually exclusive",
                    ));
                }
                options.attached = Some(arguments[index] == "--attached");
                index += 1;
            }
            argument => {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidInput,
                    format!("unknown target list option '{argument}'"),
                ));
            }
        }
    }
    Ok(options)
}

fn connection_list_output(
    snapshot: &ContextSnapshot,
    selection: &CliSelection,
    connection_filter: Option<&str>,
    options: &ConnectionListOptions,
) -> ConnectionListOutput {
    let selection_applies = selection.context.as_deref() == Some(snapshot.id.as_str());
    let connections = snapshot
        .connections
        .iter()
        .filter(|connection| {
            connection_filter.is_none_or(|id| connection.id == id)
                && options
                    .status
                    .is_none_or(|status| connection_status_matches(&connection.status, status))
                && options.kind.is_none_or(|kind| {
                    connection_configuration_matches(&connection.configuration, kind)
                })
        })
        .map(|connection| ConnectionListEntry {
            id: connection.id.clone(),
            selected: selection_applies
                && selection.connection.as_deref() == Some(connection.id.as_str()),
            configuration: connection.configuration.clone(),
            generation: connection.generation,
            status: connection.status.clone(),
            target_count: connection.targets.len(),
        })
        .collect();
    ConnectionListOutput {
        agent_instance_id: snapshot.agent_instance_id.clone(),
        context_id: snapshot.id.clone(),
        revision: snapshot.revision,
        connections,
    }
}

fn target_list_output(
    snapshot: &ContextSnapshot,
    selection: &CliSelection,
    scope: &ScopeOptions,
    options: &TargetListOptions,
) -> Result<TargetListOutput, io::Error> {
    let selection_applies = selection.context.as_deref() == Some(snapshot.id.as_str());
    let candidates = snapshot
        .target_forest
        .iter()
        .filter(|node| {
            scope
                .connection
                .as_deref()
                .is_none_or(|connection| node.connection_id == connection)
        })
        .collect::<Vec<_>>();
    let targets = dbgjs::target_selector::select_target_matches(
        &candidates,
        snapshot
            .connections
            .iter()
            .map(|connection| (connection.id.as_str(), connection.generation)),
        scope.target.as_deref(),
        |node| dbgjs::target_selector::TargetSelectorCandidate {
            target: &node.target,
            connection_id: &node.connection_id,
            generation: node.connection_generation,
        },
    )
    .map_err(io::Error::other)?
    .into_iter()
    .filter(|node| {
        options
            .target_type
            .as_deref()
            .is_none_or(|target_type| node.target.target_type.eq_ignore_ascii_case(target_type))
            && options
                .title
                .as_deref()
                .is_none_or(|title| contains_case_insensitive(&node.target.title, title))
            && options
                .url
                .as_deref()
                .is_none_or(|url| contains_case_insensitive(&node.target.url, url))
            && options
                .attached
                .is_none_or(|attached| node.target.attached == attached)
    })
    .map(|node| TargetListEntry {
        connection_id: node.connection_id.clone(),
        connection_generation: node.connection_generation,
        selected: selection_applies
            && selection.connection.as_deref() == Some(node.connection_id.as_str())
            && selection.target.as_deref().is_some_and(|selector| {
                dbgjs::target_selector::match_target_selector(
                    &node.target,
                    &node.connection_id,
                    node.connection_generation,
                    selector,
                )
                .is_some_and(|rank| rank >= dbgjs::target_selector::TargetSelectorMatch::Canonical)
            }),
        parent_target_id: node.parent_target_id.clone(),
        attachment: node.attachment,
        target: node.target.clone(),
    })
    .collect();
    Ok(TargetListOutput {
        agent_instance_id: snapshot.agent_instance_id.clone(),
        context_id: snapshot.id.clone(),
        revision: snapshot.revision,
        targets,
    })
}

fn connection_status_matches(status: &ConnectionStatus, filter: ConnectionStatusFilter) -> bool {
    matches!(
        (status, filter),
        (
            ConnectionStatus::Disconnected,
            ConnectionStatusFilter::Disconnected
        ) | (
            ConnectionStatus::Connecting,
            ConnectionStatusFilter::Connecting
        ) | (
            ConnectionStatus::Disconnecting,
            ConnectionStatusFilter::Disconnecting
        ) | (
            ConnectionStatus::Connected { .. },
            ConnectionStatusFilter::Connected
        ) | (
            ConnectionStatus::Failed { .. },
            ConnectionStatusFilter::Failed
        )
    )
}

fn connection_configuration_matches(
    configuration: &ConnectionConfiguration,
    filter: ConnectionKindFilter,
) -> bool {
    matches!(
        (configuration, filter),
        (
            ConnectionConfiguration::DirectCdp { .. },
            ConnectionKindFilter::DirectCdp
        ) | (
            ConnectionConfiguration::NodeInspector { .. },
            ConnectionKindFilter::NodeInspector
        ) | (
            ConnectionConfiguration::Process { .. },
            ConnectionKindFilter::Process
        ) | (
            ConnectionConfiguration::ProcessTree { .. }
                | ConnectionConfiguration::ScopedProcessTree { .. },
            ConnectionKindFilter::ProcessTree
        ) | (
            ConnectionConfiguration::Playwright { .. },
            ConnectionKindFilter::Playwright
        ) | (
            ConnectionConfiguration::Chrome { .. },
            ConnectionKindFilter::Chrome
        ) | (
            ConnectionConfiguration::Node { .. },
            ConnectionKindFilter::Node
        ) | (
            ConnectionConfiguration::Stdio { .. },
            ConnectionKindFilter::Stdio
        )
    )
}

fn contains_case_insensitive(value: &str, needle: &str) -> bool {
    value.to_lowercase().contains(&needle.to_lowercase())
}

fn required_option<'a>(name: &str, value: Option<&'a String>) -> Result<&'a String, io::Error> {
    value.ok_or_else(|| {
        io::Error::new(
            io::ErrorKind::InvalidInput,
            format!("{name} is required for this command"),
        )
    })
}

fn parse_process_attach_locator(value: &str) -> Result<ProcessAttachLocator, io::Error> {
    if let Some(value) = value.strip_prefix("process-tree://") {
        let (root_pid, path) = value.split_once('/').ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::InvalidInput,
                "process-tree locators use process-tree://<root-pid>/process/<pid>",
            )
        })?;
        let (kind, process_id) = path.split_once('/').ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::InvalidInput,
                "process-tree locators use process-tree://<root-pid>/process/<pid>",
            )
        })?;
        if kind != "process" || process_id.contains('/') {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "process-tree locators use process-tree://<root-pid>/process/<pid>",
            ));
        }
        return Ok(ProcessAttachLocator::ProcessTreeProcess {
            root_pid: parse_u32("process tree root ID", root_pid)?,
            process_id: parse_u32("process ID", process_id)?,
        });
    }
    if let Some(value) = value.strip_prefix("vscode://") {
        let (root_pid, path) = value.split_once('/').ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::InvalidInput,
                "VS Code locators use vscode://<root-pid>/process/<pid> or vscode://<root-pid>/window/<window-id>",
            )
        })?;
        let root_pid = parse_u32("VS Code root process ID", root_pid)?;
        let (kind, id) = path.split_once('/').ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::InvalidInput,
                "VS Code locators use vscode://<root-pid>/process/<pid> or vscode://<root-pid>/window/<window-id>",
            )
        })?;
        return match kind {
            "process" if !id.contains('/') => Ok(ProcessAttachLocator::VscodeProcess {
                root_pid,
                process_id: parse_u32("process ID", id)?,
            }),
            "window" if !id.contains('/') => Ok(ProcessAttachLocator::VscodeWindow {
                root_pid,
                window_id: parse_u32("VS Code window ID", id)?,
            }),
            _ => Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "VS Code locators use vscode://<root-pid>/process/<pid> or vscode://<root-pid>/window/<window-id>",
            )),
        };
    }
    if let Some(value) = value.strip_prefix("w:") {
        let (root_pid, window_id) = value.split_once('/').ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::InvalidInput,
                "window references use w:<root-pid>/<window-id>",
            )
        })?;
        return Ok(ProcessAttachLocator::VscodeWindow {
            root_pid: parse_u32("VS Code root process ID", root_pid)?,
            window_id: parse_u32("VS Code window ID", window_id)?,
        });
    }
    Ok(ProcessAttachLocator::Process(parse_u32(
        "process ID",
        value.strip_prefix("p:").unwrap_or(value),
    )?))
}

fn scope_option_kind(arguments: &[String]) -> ScopeOptionKind {
    let command = arguments.first().map(String::as_str);
    let operation = arguments.get(1).map(String::as_str);
    match (command, operation) {
        (
            Some(
                "target" | "playwright" | "value" | "coverage" | "profile" | "promise" | "heap"
                | "screenshot" | "log" | "watch",
            ),
            _,
        )
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
        (Some("context" | "state" | "events" | "source" | "capture"), _)
        | (Some("breakpoint"), _)
        | (Some("daemon"), Some("view"))
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
        [daemon, view, options @ ..]
            if daemon == "daemon"
                && view == "view"
                && options.iter().any(|option| option == "--all-contexts")
    ) && !matches!(
        arguments,
        [set, context] if set == "set" && matches!(context.as_str(), "context" | "workspace")
    )
}

fn parse_daemon_view_options(arguments: &[String]) -> Result<bool, io::Error> {
    match arguments {
        [] => Ok(false),
        [all] if all == "--all-contexts" => Ok(true),
        [option, ..] => Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            format!("unknown or repeated daemon view option '{option}'"),
        )),
    }
}

fn extract_scope_options(arguments: &mut Vec<String>) -> Result<ScopeOptions, io::Error> {
    let kind = scope_option_kind(arguments);
    let preserve_delimiter = matches!(
        arguments.as_slice(),
        [connection, add, stdio, ..]
            if connection == "connection" && add == "add" && stdio == "--stdio"
    ) || matches!(
        arguments.as_slice(),
        [target, eval, ..] if target == "target" && eval == "eval"
    );
    let mut options = ScopeOptions::default();
    let mut index = 0;
    while index < arguments.len() {
        if arguments[index] == "--" {
            if !preserve_delimiter {
                arguments.remove(index);
            }
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

fn log_scope(
    scope: &ResolvedScope,
    target_id: &str,
    connection_generation: u64,
    capture: &dbgjs::service_api::LogCaptureSnapshot,
) -> String {
    format!(
        "{}\0{}\0{}\0{}\0{}",
        scope.context,
        scope.connection,
        target_id,
        connection_generation,
        capture.capture_id.as_deref().unwrap_or("")
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

#[derive(Debug, PartialEq)]
struct RawCdpOptions {
    params: serde_json::Value,
    validate: bool,
    session_id: Option<String>,
}

fn parse_raw_cdp_options(arguments: &[String]) -> Result<RawCdpOptions, io::Error> {
    let mut params = serde_json::json!({});
    let mut has_params = false;
    let mut validate = true;
    let mut session_id = None;
    let mut index = 0;
    while index < arguments.len() {
        match arguments[index].as_str() {
            "--params" => {
                if has_params {
                    return Err(io::Error::new(
                        io::ErrorKind::InvalidInput,
                        "--params may only be specified once",
                    ));
                }
                let value = arguments.get(index + 1).ok_or_else(|| {
                    io::Error::new(io::ErrorKind::InvalidInput, "--params requires JSON")
                })?;
                params = serde_json::from_str(value).map_err(|error| {
                    io::Error::new(
                        io::ErrorKind::InvalidInput,
                        format!("invalid --params JSON: {error}"),
                    )
                })?;
                has_params = true;
                index += 2;
            }
            "--no-validation" => {
                validate = false;
                index += 1;
            }
            "--session-id" => {
                if session_id.is_some() {
                    return Err(io::Error::new(
                        io::ErrorKind::InvalidInput,
                        "--session-id may only be specified once",
                    ));
                }
                session_id = Some(arguments.get(index + 1).cloned().ok_or_else(|| {
                    io::Error::new(
                        io::ErrorKind::InvalidInput,
                        "--session-id requires a flattened CDP session ID",
                    )
                })?);
                index += 2;
            }
            argument => {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidInput,
                    format!("unknown target cdp option '{argument}'"),
                ));
            }
        }
    }
    Ok(RawCdpOptions {
        params,
        validate,
        session_id,
    })
}

/// `dbgjs context relay --stdio` and `dbgjs target relay --stdio` currently support only the
/// stdio transport, so `--stdio` is a required literal rather than an optional flag.
fn parse_relay_options(arguments: &[String]) -> Result<(), io::Error> {
    match arguments {
        [stdio] if stdio == "--stdio" => Ok(()),
        _ => Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "relay requires exactly one option: --stdio",
        )),
    }
}

#[derive(Debug, PartialEq, Eq)]
struct ProcessAttachOptions {
    locator: ProcessAttachLocator,
    set_default: bool,
    force: bool,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum ProcessAttachLocator {
    Process(u32),
    ProcessTreeProcess { root_pid: u32, process_id: u32 },
    VscodeProcess { root_pid: u32, process_id: u32 },
    VscodeWindow { root_pid: u32, window_id: u32 },
}

#[derive(Debug, Default, PartialEq, Eq)]
struct AttachOptions {
    set_default: bool,
    force: bool,
}

fn parse_attach_options(arguments: &[String]) -> Result<AttachOptions, io::Error> {
    let mut options = AttachOptions::default();
    for argument in arguments {
        match argument.as_str() {
            "--set" if !options.set_default => options.set_default = true,
            "--force" if !options.force => options.force = true,
            "--set" => {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidInput,
                    "--set may only be specified once",
                ));
            }
            "--force" => {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidInput,
                    "--force may only be specified once",
                ));
            }
            option => {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidInput,
                    format!("unknown target attach option '{option}'"),
                ));
            }
        }
    }
    Ok(options)
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
    client: &DbgServiceClient,
    selection_file: &Path,
    cwd: &str,
) -> Result<String, Box<dyn std::error::Error>> {
    let store = load_selection_store(selection_file, cwd)?;
    let contexts = rpc(client.contexts.list_contexts(Some(cwd.to_owned())).await)?;
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
            "stale context binding at '{binding}' refers to missing context '{context}'; replace it with 'dbgjs set context --context <expression>' from that directory"
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
    let mut locator = None;
    let mut set_default = false;
    let mut force = false;
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
            "--force" => {
                if force {
                    return Err(io::Error::new(
                        io::ErrorKind::InvalidInput,
                        "--force may only be specified once",
                    ));
                }
                force = true;
                index += 1;
            }
            argument if argument.starts_with("--") => {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidInput,
                    format!("unknown process attach option '{argument}'"),
                ));
            }
            argument => {
                if locator.is_some() {
                    return Err(io::Error::new(
                        io::ErrorKind::InvalidInput,
                        "process attach accepts exactly one process reference",
                    ));
                }
                locator = Some(parse_process_attach_locator(argument)?);
                index += 1;
            }
        }
    }
    Ok(ProcessAttachOptions {
        locator: locator.ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::InvalidInput,
                "process attach requires a process reference",
            )
        })?,
        set_default,
        force,
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
        .join("dbgjs-screenshots")
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
        log_scope(
            scope,
            &snapshot.target_id,
            snapshot.connection_generation,
            &snapshot.log_capture,
        ),
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
    client: &DbgServiceClient,
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
    let snapshot = rpc(client.contexts.get_context(context.clone()).await)?;
    if options.connection.is_some() {
        return Ok(resolve_target_scope(
            context, &snapshot, selection, options,
        )?);
    }
    let use_selection = selection.context.as_deref() == Some(context.as_str());
    if let Some(selector) = options
        .target
        .as_ref()
        .or_else(|| use_selection.then(|| selection.target.as_ref()).flatten())
    {
        let target = rpc(client
            .targets
            .resolve_target(context.clone(), selector.clone())
            .await)?;
        return Ok(ResolvedScope {
            context,
            target: dbgjs::target_selector::resolved_target_selector(
                &target.connection_id,
                &target.target_id,
                target.connection_generation,
                Some(selector),
            ),
            connection: target.connection_id,
        });
    }
    Ok(resolve_target_scope(
        context, &snapshot, selection, options,
    )?)
}

async fn resolve_offline_scope(
    client: &DbgServiceClient,
    selection: &CliSelection,
    options: &ScopeOptions,
) -> Result<ResolvedScope, Box<dyn std::error::Error>> {
    if let (Some(context), Some(connection), Some(target)) = (
        options.context.clone(),
        options.connection.clone(),
        options.target.clone(),
    ) {
        return Ok(ResolvedScope {
            context,
            connection,
            target,
        });
    }
    if options.context.is_none()
        && options.connection.is_none()
        && options.target.is_none()
        && let (Some(context), Some(connection), Some(target)) = (
            selection.context.clone(),
            selection.connection.clone(),
            selection.target.clone(),
        )
    {
        return Ok(ResolvedScope {
            context,
            connection,
            target,
        });
    }
    resolve_scope(client, selection, options).await
}

fn resolve_target_scope(
    context: String,
    snapshot: &dbgjs::service_api::ContextSnapshot,
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
                dbgjs::service_api::ConnectionStatus::Connected { .. }
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
            connection
                .targets
                .iter()
                .map(|target| (*connection, target))
        })
        .collect::<Vec<_>>();
    let candidates = dbgjs::target_selector::select_target_matches(
        &candidates,
        snapshot
            .connections
            .iter()
            .map(|connection| (connection.id.as_str(), connection.generation)),
        requested_target.map(String::as_str),
        |(connection, target)| dbgjs::target_selector::TargetSelectorCandidate {
            target,
            connection_id: &connection.id,
            generation: connection.generation,
        },
    )
    .map_err(io::Error::other)?;
    let (connection, target) = match candidates.as_slice() {
        [(connection, target)] => (
            connection.id.clone(),
            dbgjs::target_selector::resolved_target_selector(
                &connection.id,
                &target.target_id,
                connection.generation,
                requested_target.map(String::as_str),
            ),
        ),
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
                .map(|(connection, _)| connection.id.as_str())
                .collect::<std::collections::BTreeSet<_>>();
            let hint = if connections.len() > 1 {
                " use --connection <id> to disambiguate"
            } else {
                " use --target <selector> to select one"
            };
            let details = candidates
                .iter()
                .map(|(connection, target)| {
                    let qualified = dbgjs::target_selector::qualified_target_selector(
                        &connection.id,
                        &target.target_id,
                        connection.generation,
                    );
                    format!(
                        "\n  {qualified}  type={}  title={:?}  url={}",
                        target.target_type, target.title, target.url
                    )
                })
                .collect::<String>();
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                format!(
                    "target selector '{selector}' is ambiguous across {} targets in context '{context}';{hint}. Candidates:{details}",
                    candidates.len(),
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
    client: &DbgServiceClient,
    selection: &CliSelection,
    scope: &ResolvedScope,
    snapshot: &TargetDebuggerSnapshot,
) -> Result<(), Box<dyn std::error::Error>> {
    let evaluations = evaluate_watches(client, selection, scope, snapshot).await?;
    output.print_target_with_watches(snapshot, &scope.target, &evaluations)?;
    Ok(())
}

async fn evaluate_watches(
    client: &DbgServiceClient,
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
                .targets
                .evaluate_target(scope.target_ref(), Some(epoch), 0, expression.clone())
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
                    preview: dbgjs::service_api::ValuePreviewSnapshot {
                        kind: "error".to_owned(),
                        preview: Some("unavailable in this frame".to_owned()),
                        truncated: false,
                        reference: None,
                        source: Default::default(),
                    },
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
    client: &dbgjs::service_api::DbgServiceClient,
    context_id: &str,
    connection_id: &str,
    target_id: &str,
    options: &[String],
) -> Result<u64, Box<dyn std::error::Error>> {
    match options {
        [] => {
            let snapshot = rpc(client
                .targets
                .get_target(dbgjs::service_api::TargetRef {
                    connection: dbgjs::service_api::ConnectionRef {
                        context_id: context_id.to_owned(),
                        connection_id: connection_id.to_owned(),
                    },
                    target_id: target_id.to_owned(),
                })
                .await)?;
            Ok(current_pause_epoch(&snapshot)?)
        }
        [epoch, value] if epoch == "--epoch" => Ok(parse_u64("pause epoch", value)?),
        _ => Err("expected no options or --epoch <epoch>".into()),
    }
}

fn current_pause_epoch(
    snapshot: &dbgjs::service_api::TargetDebuggerSnapshot,
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

fn split_logpoint_install_options(values: &[String]) -> Result<(&[String], Option<u64>), io::Error> {
    let split = values.iter().enumerate()
        .position(|(index, value)| index % 5 == 0 && value.starts_with("--"))
        .unwrap_or(values.len());
    Ok((&values[..split], parse_logpoint_install_options(&values[split..])?))
}

fn parse_logpoint_install_options(options: &[String]) -> Result<Option<u64>, io::Error> {
    let mut required = false;
    let mut timeout = None;
    let mut index = 0;
    while index < options.len() {
        match options[index].as_str() {
            "--require-installed" if !required => {
                required = true;
                index += 1;
            }
            "--timeout-ms" if timeout.is_none() => {
                let value = options.get(index + 1).ok_or_else(|| io::Error::new(
                    io::ErrorKind::InvalidInput, "--timeout-ms requires milliseconds"
                ))?;
                let ms = value.parse::<u64>().map_err(|_| io::Error::new(
                    io::ErrorKind::InvalidInput, "--timeout-ms must be a positive integer"
                ))?;
                if ms == 0 || ms > 300_000 {
                    return Err(io::Error::new(io::ErrorKind::InvalidInput,
                        "--timeout-ms must be between 1 and 300000"));
                }
                timeout = Some(ms);
                index += 2;
            }
            value => return Err(io::Error::new(
                io::ErrorKind::InvalidInput, format!("unknown or repeated logpoint option '{value}'")
            )),
        }
    }
    if timeout.is_some() && !required {
        return Err(io::Error::new(io::ErrorKind::InvalidInput,
            "--timeout-ms requires --require-installed"));
    }
    Ok(required.then_some(timeout.unwrap_or(30_000)))
}

async fn require_logpoints_installed(
    client: &DbgServiceClient,
    scope: &ResolvedScope,
    mut snapshot: TargetDebuggerSnapshot,
    ids: &[String],
    timeout_ms: u64,
) -> Result<TargetDebuggerSnapshot, Box<dyn std::error::Error>> {
    let deadline = tokio::time::Instant::now() + std::time::Duration::from_millis(timeout_ms);
    for id in ids {
        let status = snapshot.breakpoints.iter()
            .find(|breakpoint| breakpoint.id == *id)
            .map(|breakpoint| &breakpoint.status);
        match status {
            Some(TargetBreakpointStatus::Installed { .. }) => continue,
            Some(TargetBreakpointStatus::Failed { .. }) => {
                return Err(io::Error::other(format!(
                    "logpoint {id} was accepted but not installed: {:?}",
                    status.unwrap()
                )).into());
            }
            _ => {}
        }
        let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
        if remaining.is_zero() {
            return Err(io::Error::new(io::ErrorKind::TimedOut,
                format!("logpoint {id} was accepted but not installed within {timeout_ms} ms")).into());
        }
        snapshot = rpc(client.targets.wait_target(
            scope.target_ref(),
            TargetWaitPredicate::BreakpointInstalled { breakpoint_id: id.clone() },
            remaining.as_millis().min(u64::MAX as u128) as u64,
        ).await)?;
    }
    Ok(snapshot)
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
    exclude_capture_id: Option<String>,
    path: Option<String>,
    path_glob: Option<String>,
    deprecated_path: bool,
    all: bool,
    max_lines: usize,
    trim_width: bool,
}

struct CoverageCaptureOptions {
    capture_id: Option<String>,
    raw: bool,
    path: Option<String>,
    path_glob: Option<String>,
    deprecated_path: bool,
    all: bool,
    max_lines: usize,
    trim_width: bool,
    render_requested: bool,
}

struct CoverageStopOptions {
    capture_id: Option<String>,
}

struct CpuProfileShowOptions {
    capture_id: String,
    path: Option<String>,
    view: CpuProfileView,
    sort: CpuProfileSort,
    max_lines: usize,
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

fn parse_source_tree_options(
    values: &[String],
) -> Result<(SourceTreeKind, SourceTreeOutputOptions), io::Error> {
    let mut kind = None;
    let mut all = false;
    let mut max_lines = 300_usize;
    let mut trim_width = true;
    let mut index = 0;
    while index < values.len() {
        match values[index].as_str() {
            "loaded" if kind.is_none() => kind = Some(SourceTreeKind::Loaded),
            "source-mapped" if kind.is_none() => kind = Some(SourceTreeKind::SourceMapped),
            "formatted" if kind.is_none() => kind = Some(SourceTreeKind::Formatted),
            "resolved" if kind.is_none() => kind = Some(SourceTreeKind::Resolved),
            "--all" => all = true,
            "--no-trim" => trim_width = false,
            "--max-lines" => {
                index += 1;
                max_lines = required_source_option(values, index, "--max-lines")?
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
                return Err(invalid_option("source tree", option));
            }
            value => return Err(unexpected_argument("source tree", value)),
        }
        index += 1;
    }
    let kind = kind.ok_or_else(|| {
        io::Error::new(
            io::ErrorKind::InvalidInput,
            "source tree requires <loaded|source-mapped|formatted|resolved>",
        )
    })?;
    Ok((
        kind,
        SourceTreeOutputOptions {
            all,
            max_lines,
            trim_width,
        },
    ))
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
    let mut view = SourceViewPreference::Policy;
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
            "--view" => {
                index += 1;
                view = parse_source_view(required_source_option(values, index, "--view")?)?;
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
            view,
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
    let mut timeout_ms = Some(30_000);
    let mut view = SourceViewPreference::Policy;
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
            "--timeout-ms" => {
                index += 1;
                timeout_ms = Some(u64::from(parse_positive_u32(
                    "--timeout-ms",
                    required_source_option(values, index, "--timeout-ms")?,
                )?));
            }
            "--view" => {
                index += 1;
                view = parse_source_view(required_source_option(values, index, "--view")?)?;
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
        timeout_ms,
        view,
    })
}

fn parse_source_grep_cli_options(
    values: &[String],
) -> Result<(SourceSearchOptions, usize, usize, bool), io::Error> {
    let mut search_values = Vec::new();
    let mut max_output_bytes = 8192;
    let mut max_line_bytes = 256;
    let mut verbose_diagnostics = false;
    let mut index = 0;
    while index < values.len() {
        match values[index].as_str() {
            "--max-output-bytes" | "--max-line-bytes" => {
                let flag = values[index].as_str();
                index += 1;
                let size = parse_positive_u32(
                    flag, required_source_option(values, index, flag)?,
                )? as usize;
                if (flag == "--max-output-bytes" && size < 1024)
                    || (flag == "--max-line-bytes" && size < 64) {
                    return Err(io::Error::new(io::ErrorKind::InvalidInput, format!("{flag} is too small")));
                }
                if flag == "--max-output-bytes" { max_output_bytes = size; }
                else { max_line_bytes = size; }
            }
            "--verbose-diagnostics" => verbose_diagnostics = true,
            _ => search_values.push(values[index].clone()),
        }
        index += 1;
    }
    Ok((parse_source_grep_options(&search_values)?, max_output_bytes, max_line_bytes, verbose_diagnostics))
}

fn parse_source_formatting_mode(value: &str) -> Result<SourceFormattingMode, io::Error> {
    match value {
        "off" => Ok(SourceFormattingMode::Off),
        "auto" => Ok(SourceFormattingMode::Auto),
        "on" => Ok(SourceFormattingMode::On),
        _ => Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "formatting mode must be one of: off, auto, on",
        )),
    }
}

fn parse_source_view(value: &str) -> Result<SourceViewPreference, io::Error> {
    match value {
        "original" => Ok(SourceViewPreference::Original),
        "formatted" => Ok(SourceViewPreference::Formatted),
        _ => Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "--view must be one of: original, formatted",
        )),
    }
}

fn parse_source_formatting_rule(
    values: &[String],
) -> Result<(SourceFormattingMode, Option<String>, Option<String>), io::Error> {
    let mut mode = None;
    let mut target_pattern = None;
    let mut url_pattern = None;
    let mut index = 0;
    while index < values.len() {
        match values[index].as_str() {
            "--mode" => {
                index += 1;
                mode = Some(parse_source_formatting_mode(required_source_option(
                    values, index, "--mode",
                )?)?);
            }
            "--target" => {
                index += 1;
                target_pattern =
                    Some(required_source_option(values, index, "--target")?.to_owned());
            }
            "--url" => {
                index += 1;
                url_pattern = Some(required_source_option(values, index, "--url")?.to_owned());
            }
            option if option.starts_with("--") => {
                return Err(invalid_option("source formatting rule add", option));
            }
            value => return Err(unexpected_argument("source formatting rule add", value)),
        }
        index += 1;
    }
    let mode = mode.ok_or_else(|| {
        io::Error::new(
            io::ErrorKind::InvalidInput,
            "source formatting rule add requires --mode <off|auto|on>",
        )
    })?;
    if target_pattern.is_none() && url_pattern.is_none() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "source formatting rule add requires --target, --url, or both",
        ));
    }
    Ok((mode, target_pattern, url_pattern))
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
    trim_width: bool,
}

const DEFAULT_HEAP_STRING_LENGTH: u32 = 160;
const DEFAULT_HEAP_SHOW_REFERENCE_LIMIT: u32 = 20;

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

struct HeapShowOptions {
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

fn parse_heap_show_options(values: &[String]) -> Result<HeapShowOptions, io::Error> {
    let mut options = HeapShowOptions {
        limit: DEFAULT_HEAP_SHOW_REFERENCE_LIMIT,
        max_string_length: Some(DEFAULT_HEAP_STRING_LENGTH),
    };
    let mut index = 0;
    while index < values.len() {
        match values[index].as_str() {
            "--all" => options.limit = u32::MAX,
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
                    format!("unknown heap show option '{option}'"),
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

struct ValueOptions {
    selector: ValueSelector,
    max_preview_length: u32,
    max_properties: u32,
}

fn parse_value_options(arguments: &[String]) -> Result<ValueOptions, io::Error> {
    let mut expression = None;
    let mut object_id = None;
    let mut allow_side_effects = false;
    let mut max_preview_length = DEFAULT_PROMISE_PREVIEW_LENGTH;
    let mut max_properties = DEFAULT_VALUE_PROPERTY_LIMIT;
    let mut index = 0;
    while index < arguments.len() {
        match arguments[index].as_str() {
            "--object-id" => {
                index += 1;
                object_id = Some(
                    arguments
                        .get(index)
                        .ok_or_else(|| {
                            io::Error::new(
                                io::ErrorKind::InvalidInput,
                                "--object-id requires a value",
                            )
                        })?
                        .clone(),
                );
            }
            "--allow-side-effects" => allow_side_effects = true,
            "--max-preview-length" => {
                index += 1;
                max_preview_length = parse_u32_option(arguments, index, "--max-preview-length")?;
            }
            "--max-properties" => {
                index += 1;
                max_properties = parse_u32_option(arguments, index, "--max-properties")?;
            }
            argument if argument.starts_with("--") => {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidInput,
                    format!("unknown value option '{argument}'"),
                ));
            }
            argument if expression.is_none() => expression = Some(argument.to_owned()),
            _ => {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidInput,
                    "value accepts exactly one JavaScript expression",
                ));
            }
        }
        index += 1;
    }
    let selector = match (expression, object_id) {
        (Some(expression), None) => ValueSelector::Expression {
            expression,
            allow_side_effects,
        },
        (None, Some(object_id)) if !allow_side_effects => ValueSelector::RemoteObject { object_id },
        (None, Some(_)) => {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "--allow-side-effects only applies to JavaScript expressions",
            ));
        }
        (Some(_), Some(_)) => {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "provide either a JavaScript expression or --object-id, not both",
            ));
        }
        (None, None) => {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "value requires a JavaScript expression or --object-id",
            ));
        }
    };
    Ok(ValueOptions {
        selector,
        max_preview_length,
        max_properties,
    })
}

struct PromiseListOptions {
    capture_id: String,
    state: Option<PromiseState>,
    limit: u32,
    max_preview_length: u32,
}

fn parse_promise_list_options(arguments: &[String]) -> Result<PromiseListOptions, io::Error> {
    let mut capture_id = ".".to_owned();
    let mut state = None;
    let mut limit = DEFAULT_PROMISE_LIMIT;
    let mut max_preview_length = DEFAULT_PROMISE_PREVIEW_LENGTH;
    let mut index = 0;
    if arguments
        .first()
        .is_some_and(|argument| !argument.starts_with("--"))
    {
        capture_id = arguments[0].clone();
        index = 1;
    }
    while index < arguments.len() {
        match arguments[index].as_str() {
            "--state" => {
                index += 1;
                state = Some(match arguments.get(index).map(String::as_str) {
                    Some("pending") => PromiseState::Pending,
                    Some("fulfilled") => PromiseState::Fulfilled,
                    Some("rejected") => PromiseState::Rejected,
                    Some("unknown") => PromiseState::Unknown,
                    _ => {
                        return Err(io::Error::new(
                            io::ErrorKind::InvalidInput,
                            "--state requires pending, fulfilled, rejected, or unknown",
                        ));
                    }
                });
            }
            "--limit" => {
                index += 1;
                limit = parse_u32_option(arguments, index, "--limit")?;
            }
            "--max-preview-length" => {
                index += 1;
                max_preview_length = parse_u32_option(arguments, index, "--max-preview-length")?;
            }
            option => {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidInput,
                    format!("unknown promise option '{option}'"),
                ));
            }
        }
        index += 1;
    }
    Ok(PromiseListOptions {
        capture_id,
        state,
        limit,
        max_preview_length,
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
            "--no-cache" => {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidInput,
                    "--no-cache is not supported for captured heaps; capture a new snapshot to refresh mapping metadata",
                ));
            }
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

async fn wait_for_heap_stream<T>(
    output: &OutputFormat,
    mut result: linkrpc::prelude::CallResult<T, dbgjs::service_api::HeapProfilerError>,
    mut progress: linkrpc::prelude::StreamReceiver<HeapSnapshotProgress>,
) -> Result<T, Box<dyn std::error::Error>> {
    let mut last_progress = None::<HeapSnapshotProgress>;
    let mut final_result = None;
    loop {
        tokio::select! {
            result = &mut result, if final_result.is_none() => {
                final_result = Some(result);
            }
            update = progress.recv() => {
                let Some(update) = update else {
                    break;
                };
                if last_progress.as_ref() != Some(&update)
                {
                    output.print_heap_snapshot_progress(&update)?;
                    last_progress = Some(update);
                }
            }
        }
    }
    let final_result = match final_result {
        Some(result) => result,
        None => result.await,
    };
    Ok(rpc(final_result)?)
}

async fn show_stored_coverage(
    client: &DbgServiceClient,
    output: &OutputFormat,
    context: String,
    scope: &ScopeOptions,
    options: CoverageShowOptions,
) -> Result<(), Box<dyn std::error::Error>> {
    let snapshot = rpc(client
        .captures
        .get_stored_coverage(
            context,
            options.capture_id,
            None,
            scope.target.clone(),
            scope.connection.clone(),
            None,
            options.exclude_capture_id,
        )
        .await)?;
    output.print_coverage(
        &snapshot,
        CoverageOutputOptions {
            path: options.path.as_deref(),
            path_glob: options.path_glob.as_deref(),
            all: options.all,
            max_lines: options.max_lines,
            trim_width: options.trim_width,
        },
    )?;
    Ok(())
}

async fn show_stored_cpu_profile(
    client: &DbgServiceClient,
    output: &OutputFormat,
    context: String,
    scope: &ScopeOptions,
    options: CpuProfileShowOptions,
) -> Result<(), Box<dyn std::error::Error>> {
    let profile = rpc(client
        .captures
        .get_stored_cpu_profile(
            context,
            options.capture_id,
            options.path.clone(),
            scope.target.clone(),
            scope.connection.clone(),
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
    Ok(())
}

async fn show_stored_heap_classes(
    client: &DbgServiceClient,
    output: &OutputFormat,
    context: String,
    scope: &ScopeOptions,
    options: HeapClassOptions,
) -> Result<(), Box<dyn std::error::Error>> {
    let classes = rpc(client
        .captures
        .get_stored_heap_classes(
            context,
            options.capture_id,
            options.filter,
            scope.target.clone(),
            scope.connection.clone(),
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
    Ok(())
}

fn parse_coverage_show_options(values: &[String]) -> Result<CoverageShowOptions, io::Error> {
    let mut capture_id = None;
    let mut exclude_capture_id = None;
    let mut path = None;
    let mut path_glob = None;
    let mut deprecated_path = false;
    let mut all = false;
    let mut max_lines = 300_usize;
    let mut trim_width = true;
    let mut index = 0;
    while index < values.len() {
        match values[index].as_str() {
            "--exclude" => {
                index += 1;
                let value = values
                    .get(index)
                    .filter(|value| !value.is_empty() && !value.starts_with("--"))
                    .ok_or_else(|| {
                        io::Error::new(
                            io::ErrorKind::InvalidInput,
                            "--exclude requires a baseline capture selector",
                        )
                    })?;
                if exclude_capture_id.replace(value.clone()).is_some() {
                    return Err(io::Error::new(
                        io::ErrorKind::InvalidInput,
                        "--exclude may only be specified once",
                    ));
                }
            }
            "--path" | "--path-prefix" | "--path-glob" => {
                let option = values[index].as_str();
                index += 1;
                let value = values
                    .get(index)
                    .filter(|value| !value.is_empty() && !value.starts_with("--"))
                    .ok_or_else(|| {
                        io::Error::new(
                            io::ErrorKind::InvalidInput,
                            format!(
                                "{option} requires a source URL {}",
                                if option == "--path-glob" {
                                    "glob"
                                } else {
                                    "prefix"
                                }
                            ),
                        )
                    })?;
                if path.is_some() || path_glob.is_some() {
                    return Err(io::Error::new(
                        io::ErrorKind::InvalidInput,
                        "specify only one of --path-prefix or --path-glob (--path is a deprecated prefix alias)",
                    ));
                }
                if option == "--path-glob" {
                    path_glob = Some(value.clone());
                } else {
                    path = Some(value.clone());
                    deprecated_path = option == "--path";
                }
            }

            "--all" => all = true,
            "--no-cache" => {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidInput,
                    "--no-cache is not supported for stored coverage: views read current available local source maps without a persistent capture cache",
                ));
            }
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
    CoveragePathFilter::new(path.as_deref(), path_glob.as_deref())
        .map_err(|error| io::Error::new(io::ErrorKind::InvalidInput, error))?;
    Ok(CoverageShowOptions {
        capture_id: capture_id.unwrap_or_else(|| ".".to_owned()),
        exclude_capture_id,
        path,
        path_glob,
        deprecated_path,
        all,
        max_lines,
        trim_width,
    })
}

fn parse_coverage_capture_options(values: &[String]) -> Result<CoverageCaptureOptions, io::Error> {
    let mut capture_id = None;
    let mut raw = false;
    let mut render_values = Vec::with_capacity(values.len());
    let mut index = 0;
    while index < values.len() {
        if values[index] == "--exclude" {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "--exclude is only supported by coverage show <capture> --exclude <baseline>",
            ));
        } else if values[index] == "--id" {
            let option = values[index].as_str();
            let value = values.get(index + 1).ok_or_else(|| {
                io::Error::new(
                    io::ErrorKind::InvalidInput,
                    format!("{option} requires a capture name"),
                )
            })?;
            if value.starts_with("--") || value.is_empty() {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidInput,
                    format!("{option} requires a capture name"),
                ));
            }
            if capture_id.replace(value.clone()).is_some() {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidInput,
                    format!("{option} may only be specified once"),
                ));
            }
            index += 2;
        } else if values[index] == "--raw" {
            raw = true;
            index += 1;
        } else {
            render_values.push(values[index].clone());
            index += 1;
        }
    }
    let render_requested = !render_values.is_empty();
    let mut show_values = Vec::with_capacity(render_values.len() + 1);
    show_values.push(".".to_owned());
    show_values.extend(render_values);
    let options = parse_coverage_show_options(&show_values)?;
    Ok(CoverageCaptureOptions {
        capture_id,
        raw,
        path: options.path,
        path_glob: options.path_glob,
        deprecated_path: options.deprecated_path,
        all: options.all,
        max_lines: options.max_lines,
        trim_width: options.trim_width,
        render_requested,
    })
}

fn parse_coverage_stop_options(values: &[String]) -> Result<CoverageStopOptions, io::Error> {
    let mut options = CoverageStopOptions { capture_id: None };
    let mut arguments = values.iter();
    while let Some(option) = arguments.next() {
        let destination = match option.as_str() {
            "--id" => &mut options.capture_id,
            "--exclude" => {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidInput,
                    "--exclude is only supported by coverage show <capture> --exclude <baseline>",
                ));
            }
            _ => {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidInput,
                    format!("unknown coverage stop option '{option}'; expected --id"),
                ));
            }
        };
        let value = arguments
            .next()
            .filter(|value| !value.is_empty() && !value.starts_with("--"))
            .ok_or_else(|| {
                io::Error::new(
                    io::ErrorKind::InvalidInput,
                    format!("{option} requires a capture ID"),
                )
            })?;
        if destination.replace(value.clone()).is_some() {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                format!("{option} may only be specified once"),
            ));
        }
    }
    Ok(options)
}

fn warn_deprecated_coverage_path(deprecated: bool) {
    if deprecated {
        eprintln!(
            "Warning: coverage --path is a deprecated prefix alias; use --path-prefix, or --path-glob '**/issue/**' to match a directory anywhere in a source URL."
        );
    }
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
            "--no-cache" => {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidInput,
                    "--no-cache is not supported for stored CPU profiles: views read current available local source maps without a persistent capture cache",
                ));
            }
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
    let current = rpc(client.contexts.get_context(context_id.clone()).await)?;
    let explicit_target_scope =
        scope_options.connection.is_some() || scope_options.target.is_some();
    let scope = match resolve_target_scope(context_id.clone(), &current, &selection, scope_options)
    {
        Ok(scope) => Some(scope),
        Err(_) if !explicit_target_scope => None,
        Err(error) => return Err(error.into()),
    };
    let context = rpc(client
        .contexts
        .put_breakpoint(
            context_id,
            breakpoint_id.to_owned(),
            source_path.to_owned(),
            line,
            column,
        )
        .await)?;
    if let Some(scope) = scope {
        let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(30);
        let snapshot = loop {
            let snapshot = rpc(client.targets.get_target(scope.target_ref()).await)?;
            let settled = snapshot
                .breakpoints
                .iter()
                .find(|breakpoint| breakpoint.id == breakpoint_id)
                .is_some_and(|breakpoint| match breakpoint.status {
                    TargetBreakpointStatus::WaitingForScript => snapshot
                        .scripts
                        .iter()
                        .all(|script| !matches!(script.status, TargetScriptStatus::Pending)),
                    TargetBreakpointStatus::SourceNotFound { .. }
                    | TargetBreakpointStatus::AmbiguousSource { .. }
                    | TargetBreakpointStatus::Unmapped { .. }
                    | TargetBreakpointStatus::Installed { .. }
                    | TargetBreakpointStatus::Failed { .. } => true,
                    TargetBreakpointStatus::Applicable { .. }
                    | TargetBreakpointStatus::Installing { .. } => false,
                });
            if settled || tokio::time::Instant::now() >= deadline {
                break snapshot;
            }
            tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        };
        let _ = snapshot;
    }
    let context = rpc(client.contexts.get_context(context.id.clone()).await)?;
    print_breakpoint_result(&client, &context, breakpoint_id, output).await?;
    Ok(())
}

async fn print_breakpoint_result(
    client: &DbgServiceClient,
    context: &ContextSnapshot,
    breakpoint_id: &str,
    output: OutputFormat,
) -> Result<(), Box<dyn std::error::Error>> {
    let breakpoint = context
        .breakpoints
        .iter()
        .find(|breakpoint| breakpoint.id == breakpoint_id)
        .ok_or_else(|| io::Error::other("updated context omitted the requested breakpoint"))?;
    let mut sources = Vec::new();
    for application in &breakpoint.applications {
        let target = rpc(client
            .targets
            .get_target(dbgjs::service_api::TargetRef {
                connection: dbgjs::service_api::ConnectionRef {
                    context_id: context.id.clone(),
                    connection_id: application.connection_id.clone(),
                },
                target_id: application.target_id.clone(),
            })
            .await)?;
        if let Some(source) = target
            .breakpoints
            .iter()
            .find(|candidate| candidate.id == breakpoint_id)
            .and_then(|candidate| candidate.source.clone())
            && !sources.contains(&source)
        {
            sources.push(source);
        }
    }
    output.print_breakpoint(context, breakpoint_id, &sources)?;
    Ok(())
}

#[derive(Debug, PartialEq, Eq)]
enum ProcessAttachTarget {
    Target(String),
    Renderer(RendererAttachSelector),
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum RendererAttachSelector {
    Process(u32),
    Window { root_pid: u32, window_id: u32 },
}

impl std::fmt::Display for RendererAttachSelector {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Process(process_id) => write!(f, "renderer process {process_id}"),
            Self::Window {
                root_pid,
                window_id,
            } => {
                write!(f, "VS Code window w:{root_pid}/{window_id}")
            }
        }
    }
}

fn process_attach_destination(
    locator: ProcessAttachLocator,
    discovered: &[ProcessTreeSnapshot],
) -> io::Result<(String, ConnectionConfiguration, ProcessAttachTarget)> {
    let (process_id, root_pid) = match locator {
        ProcessAttachLocator::VscodeWindow {
            root_pid,
            window_id,
        } => {
            if !discovered
                .iter()
                .any(|tree| tree.root_process_id == root_pid)
            {
                return Err(io::Error::new(
                    io::ErrorKind::NotFound,
                    format!("VS Code window w:{root_pid}/{window_id} is no longer available"),
                ));
            }
            return Ok((
                format!("process-tree-{root_pid}"),
                ConnectionConfiguration::ProcessTree { root_pid },
                ProcessAttachTarget::Renderer(RendererAttachSelector::Window {
                    root_pid,
                    window_id,
                }),
            ));
        }
        ProcessAttachLocator::Process(process_id) => (process_id, None),
        ProcessAttachLocator::ProcessTreeProcess {
            root_pid,
            process_id,
        }
        | ProcessAttachLocator::VscodeProcess {
            root_pid,
            process_id,
        } => {
            if !discovered.iter().any(|tree| {
                tree.root_process_id == root_pid
                    && tree
                        .processes
                        .iter()
                        .any(|process| process.process_id == process_id)
            }) {
                let scheme = if matches!(locator, ProcessAttachLocator::VscodeProcess { .. }) {
                    "vscode"
                } else {
                    "process-tree"
                };
                return Err(io::Error::new(
                    io::ErrorKind::NotFound,
                    format!(
                        "process {scheme}://{root_pid}/process/{process_id} is no longer available"
                    ),
                ));
            }
            (process_id, Some(root_pid))
        }
    };
    let process_tree_target = discovered
        .iter()
        .filter(|tree| root_pid.is_none_or(|root| tree.root_process_id == root))
        .find_map(|tree| {
            tree.processes
                .iter()
                .find(|process| {
                    process.process_id == process_id
                        && process.attachable
                        // A terminal-launched Node app can be a descendant of the IDE
                        // without belonging to its debugger-managed process tree.
                        && (root_pid.is_some() || process.role != ProcessRole::Node)
                })
                .map(|process| (tree.root_process_id, process))
        });
    if let Some((root_pid, process)) = process_tree_target
        && let Some(target_id) = &process.debug_target_id
    {
        Ok((
            format!("process-tree-{root_pid}"),
            ConnectionConfiguration::ProcessTree { root_pid },
            if process.role == ProcessRole::Renderer {
                ProcessAttachTarget::Renderer(RendererAttachSelector::Process(process_id))
            } else {
                ProcessAttachTarget::Target(target_id.clone())
            },
        ))
    } else {
        Ok((
            format!("process-{process_id}"),
            ConnectionConfiguration::Process { process_id },
            ProcessAttachTarget::Target("$node-root".to_owned()),
        ))
    }
}

fn select_renderer_target(
    graph: &ResourceGraphSnapshot,
    context_id: &str,
    connection_id: &str,
    selector: RendererAttachSelector,
) -> Result<Option<String>, String> {
    let (attribute, value) = match selector {
        RendererAttachSelector::Process(process_id) => ("processId", process_id),
        RendererAttachSelector::Window { window_id, .. } => ("primaryWindowId", window_id),
    };
    let candidates = graph
        .resources
        .iter()
        .filter(|resource| {
            resource
                .attributes
                .get("connectionId")
                .and_then(serde_json::Value::as_str)
                == Some(connection_id)
                && resource
                    .attributes
                    .get("subtype")
                    .and_then(serde_json::Value::as_str)
                    == Some("electron-renderer")
                && resource
                    .attributes
                    .get(attribute)
                    .and_then(serde_json::Value::as_u64)
                    == Some(u64::from(value))
        })
        .filter_map(|resource| Some((resource.attributes.get("targetId")?.as_str()?, resource)))
        .collect::<BTreeMap<_, _>>();
    match candidates.len() {
        0 => Ok(None),
        1 => Ok(candidates.keys().next().map(|id| (*id).to_owned())),
        _ => Err(format!(
            "{selector} maps to multiple Electron webContents targets. Attach one with:\n{}",
            candidates.into_iter().map(|(target_id, resource)| {
                let text = |key: &str| {
                    resource.attributes.get(key).and_then(serde_json::Value::as_str).unwrap_or("")
                };
                format!(
                    "  title: {:?}\n  URL: {:?}\n  dbgjs target attach --context \":{context_id}\" --target \"{connection_id}/{target_id}\"",
                    text("title"), text("url"),
                )
            }).collect::<Vec<_>>().join("\n")
        )),
    }
}

async fn resolve_renderer_target_id(
    client: &DbgServiceClient,
    context_id: &str,
    connection_id: &str,
    selector: RendererAttachSelector,
) -> Result<String, Box<dyn std::error::Error>> {
    wait_for_renderer_target_id(
        context_id,
        connection_id,
        selector,
        Duration::from_secs(10),
        async || {
            Ok(rpc(client
                .contexts
                .get_resource_graph(context_id.to_owned())
                .await)?)
        },
    )
    .await
}

async fn wait_for_renderer_target_id(
    context_id: &str,
    connection_id: &str,
    selector: RendererAttachSelector,
    wait: Duration,
    mut get_graph: impl AsyncFnMut() -> Result<ResourceGraphSnapshot, Box<dyn std::error::Error>>,
) -> Result<String, Box<dyn std::error::Error>> {
    tokio::time::timeout(wait, async {
        loop {
            let graph = get_graph().await?;
            if let Some(target_id) =
                select_renderer_target(&graph, context_id, connection_id, selector)?
            {
                return Ok(target_id);
            }
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
    })
    .await
    .unwrap_or_else(|_| Err(format!(
        "{selector} has no live Electron webContents matching its identity after {}s; discovery may be incomplete or the target may have closed. Re-run process list --full and retry.",
        wait.as_secs_f64(),
    ).into()))
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
    let wait_for_initial_process_tree = matches!(
        &configuration,
        ConnectionConfiguration::ProcessTree { .. }
            | ConnectionConfiguration::ScopedProcessTree { .. }
    );
    let client = ensure_service(state_file).await?;
    let configured = rpc(client
        .contexts
        .put_connection(
            dbgjs::service_api::ConnectionRef {
                context_id: context_id.to_owned(),
                connection_id: connection_id.to_owned(),
            },
            configuration,
        )
        .await)?;
    if connect_now {
        let mut connected = rpc(client
            .contexts
            .connect_connection(dbgjs::service_api::ConnectionRef {
                context_id: context_id.to_owned(),
                connection_id: connection_id.to_owned(),
            })
            .await)?;
        if wait_for_initial_process_tree {
            let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
            let mut last_target_ids = Vec::new();
            let mut stable_since = tokio::time::Instant::now();
            loop {
                let target_ids = connected
                    .connections
                    .iter()
                    .find(|connection| connection.id == connection_id)
                    .map(|connection| {
                        connection
                            .targets
                            .iter()
                            .map(|target| target.target_id.clone())
                            .collect::<Vec<_>>()
                    })
                    .unwrap_or_default();
                if target_ids != last_target_ids {
                    last_target_ids = target_ids;
                    stable_since = tokio::time::Instant::now();
                }
                if last_target_ids.len() > 1 && stable_since.elapsed() >= Duration::from_millis(500)
                {
                    break;
                }
                if tokio::time::Instant::now() >= deadline {
                    break;
                }
                tokio::time::sleep(Duration::from_millis(100)).await;
                connected = rpc(client.contexts.get_context(context_id.to_owned()).await)?;
            }
        }
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
                        connected = rpc(client.contexts.get_context(context_id.to_owned()).await)?;
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
                .targets
                .get_target(dbgjs::service_api::TargetRef {
                    connection: dbgjs::service_api::ConnectionRef {
                        context_id: context_id.to_owned(),
                        connection_id: connection_id.to_owned(),
                    },
                    target_id: target_id.clone(),
                })
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

struct NodeOptions {
    cwd: String,
    runtime_executable: String,
    args: Vec<String>,
    runtime_args: Vec<String>,
    env: BTreeMap<String, String>,
    connect: bool,
    set_default: bool,
}

struct StdioOptions {
    command: String,
    args: Vec<String>,
    cwd: String,
    env: BTreeMap<String, String>,
    topology: CdpStdioTopology,
    connect: bool,
    set_default: bool,
}

fn parse_stdio_options(options: &[String]) -> Result<StdioOptions, io::Error> {
    let delimiter = options
        .iter()
        .position(|option| option == "--")
        .ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::InvalidInput,
                "stdio connections require '-- <command> [args...]'",
            )
        })?;
    let command = options.get(delimiter + 1).ok_or_else(|| {
        io::Error::new(
            io::ErrorKind::InvalidInput,
            "stdio connections require a command after '--'",
        )
    })?;
    let mut parsed = StdioOptions {
        command: command.clone(),
        args: options[delimiter + 2..].to_vec(),
        cwd: env::current_dir()?.to_string_lossy().into_owned(),
        env: BTreeMap::new(),
        topology: CdpStdioTopology::Target,
        connect: false,
        set_default: false,
    };
    let mut index = 0;
    while index < delimiter {
        match options[index].as_str() {
            "--connect" => parsed.connect = true,
            "--set" => parsed.set_default = true,
            option @ ("--cwd" | "--env" | "--topology") => {
                index += 1;
                let value = options
                    .get(index)
                    .filter(|_| index < delimiter)
                    .ok_or_else(|| {
                        io::Error::new(
                            io::ErrorKind::InvalidInput,
                            format!("{option} requires a value before '--'"),
                        )
                    })?;
                match option {
                    "--cwd" => parsed.cwd = value.clone(),
                    "--env" => {
                        let (name, value) = value.split_once('=').ok_or_else(|| {
                            io::Error::new(io::ErrorKind::InvalidInput, "--env requires NAME=VALUE")
                        })?;
                        if name.is_empty() {
                            return Err(io::Error::new(
                                io::ErrorKind::InvalidInput,
                                "--env requires a non-empty name",
                            ));
                        }
                        parsed.env.insert(name.to_owned(), value.to_owned());
                    }
                    "--topology" => {
                        parsed.topology = match value.as_str() {
                            "browser" => CdpStdioTopology::Browser,
                            "target" => CdpStdioTopology::Target,
                            _ => {
                                return Err(io::Error::new(
                                    io::ErrorKind::InvalidInput,
                                    format!(
                                        "unsupported stdio topology '{value}'; expected browser or target"
                                    ),
                                ));
                            }
                        };
                    }
                    _ => unreachable!(),
                }
            }
            option => {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidInput,
                    format!("unknown stdio connection option '{option}'"),
                ));
            }
        }
        index += 1;
    }
    Ok(parsed)
}

fn parse_node_options(options: &[String]) -> Result<NodeOptions, io::Error> {
    let mut parsed = NodeOptions {
        cwd: env::current_dir()?.to_string_lossy().into_owned(),
        runtime_executable: env::var("DBGJS_NODE").unwrap_or_else(|_| "node".to_owned()),
        args: Vec::new(),
        runtime_args: Vec::new(),
        env: BTreeMap::new(),
        connect: false,
        set_default: false,
    };
    let mut index = 0;
    while index < options.len() {
        match options[index].as_str() {
            "--connect" => parsed.connect = true,
            "--set" => parsed.set_default = true,
            option @ ("--cwd" | "--runtime-executable" | "--arg" | "--runtime-arg" | "--env") => {
                index += 1;
                let value = options.get(index).ok_or_else(|| {
                    io::Error::new(
                        io::ErrorKind::InvalidInput,
                        format!("{option} requires a value"),
                    )
                })?;
                match option {
                    "--cwd" => parsed.cwd = value.clone(),
                    "--runtime-executable" => parsed.runtime_executable = value.clone(),
                    "--arg" => parsed.args.push(value.clone()),
                    "--runtime-arg" => parsed.runtime_args.push(value.clone()),
                    "--env" => {
                        let (name, value) = value.split_once('=').ok_or_else(|| {
                            io::Error::new(io::ErrorKind::InvalidInput, "--env requires NAME=VALUE")
                        })?;
                        if name.is_empty() {
                            return Err(io::Error::new(
                                io::ErrorKind::InvalidInput,
                                "--env requires a non-empty name",
                            ));
                        }
                        parsed.env.insert(name.to_owned(), value.to_owned());
                    }
                    _ => unreachable!(),
                }
            }
            option => {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidInput,
                    format!("unknown Node.js connection option '{option}'"),
                ));
            }
        }
        index += 1;
    }
    Ok(parsed)
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
        .targets
        .wait_target(
            dbgjs::service_api::TargetRef {
                connection: dbgjs::service_api::ConnectionRef {
                    context_id: context_id.to_owned(),
                    connection_id: connection_id.to_owned(),
                },
                target_id: target_id.to_owned(),
            },
            predicate,
            timeout_ms,
        )
        .await)?;
    output.print_target(&snapshot, target_id)?;
    Ok(())
}

struct ProcessListOptions {
    root_kind: ProcessRootKind,
    full: bool,
    command_line: bool,
    stats: bool,
    filter: Option<String>,
    trim_width: bool,
}

fn parse_process_list_options(arguments: &[String]) -> Result<ProcessListOptions, io::Error> {
    let mut result = ProcessListOptions {
        root_kind: ProcessRootKind::Vscode,
        full: false,
        command_line: true,
        stats: false,
        filter: None,
        trim_width: true,
    };
    let mut root_kind = None;
    let mut index = 0;
    while index < arguments.len() {
        match arguments[index].as_str() {
            "--vscode" if root_kind.is_none() => root_kind = Some(ProcessRootKind::Vscode),
            "--vscode" => {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidInput,
                    "--vscode conflicts with an existing process root selector",
                ));
            }
            "--root" => {
                if root_kind.is_some() {
                    return Err(io::Error::new(
                        io::ErrorKind::InvalidInput,
                        "--root may only be specified once and conflicts with --vscode",
                    ));
                }
                index += 1;
                root_kind = Some(parse_process_root_kind(arguments.get(index).ok_or_else(
                    || {
                        io::Error::new(
                            io::ErrorKind::InvalidInput,
                            "--root requires vscode, node, electron, or browser",
                        )
                    },
                )?)?);
            }
            "--full" if !result.full => result.full = true,
            "--full" => {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidInput,
                    "--full may only be specified once",
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
    result.root_kind = root_kind.ok_or_else(|| {
        io::Error::new(
            io::ErrorKind::InvalidInput,
            "process list requires --root vscode|node|electron|browser",
        )
    })?;
    Ok(result)
}

fn parse_process_root_kind(value: &str) -> Result<ProcessRootKind, io::Error> {
    match value {
        "vscode" => Ok(ProcessRootKind::Vscode),
        "node" => Ok(ProcessRootKind::Node),
        "electron" => Ok(ProcessRootKind::Electron),
        "browser" => Ok(ProcessRootKind::Browser),
        _ => Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            format!(
                "unknown process root kind '{value}'; expected vscode, node, electron, or browser"
            ),
        )),
    }
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

struct EvalOptions {
    expression: String,
    max_preview_length: u32,
    full: bool,
}

fn parse_eval_options(arguments: &[String], stdin: impl Read) -> Result<EvalOptions, io::Error> {
    let mut expression = Vec::new();
    let mut max_preview_length = None;
    let mut full = false;
    let mut index = 0;
    while index < arguments.len() {
        match arguments[index].as_str() {
            "--full" => full = true,
            "--max-preview-length" => {
                index += 1;
                max_preview_length =
                    Some(parse_u32_option(arguments, index, "--max-preview-length")?);
            }
            "--" => {
                expression.extend_from_slice(&arguments[index + 1..]);
                break;
            }
            _ => expression.push(arguments[index].clone()),
        }
        index += 1;
    }
    if full && max_preview_length.is_some() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "--full and --max-preview-length cannot be combined",
        ));
    }
    Ok(EvalOptions {
        expression: read_eval_expression(&expression, stdin)?,
        max_preview_length: if full {
            u32::MAX
        } else {
            max_preview_length.unwrap_or(DEFAULT_VALUE_PREVIEW_LENGTH)
        },
        full,
    })
}

fn read_eval_expression(arguments: &[String], mut stdin: impl Read) -> Result<String, io::Error> {
    match arguments {
        [expression] if expression != "-" => Ok(expression.clone()),
        [stdin_marker] if stdin_marker == "-" => {
            let mut expression = String::new();
            stdin.read_to_string(&mut expression)?;
            if expression.trim().is_empty() {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidInput,
                    "target eval received an empty expression on stdin",
                ));
            }
            Ok(expression)
        }
        [] => Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "target eval requires <expression> or '-' to read the expression from stdin",
        )),
        _ => Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "target eval accepts exactly one expression; use '-' alone to read from stdin",
        )),
    }
}

fn read_playwright_program(
    arguments: &[String],
    mut stdin: impl Read,
) -> Result<String, io::Error> {
    let program = match arguments {
        [stdin_marker] if stdin_marker == "-" => {
            let mut bytes = Vec::new();
            std::io::Read::take(&mut stdin, (PLAYWRIGHT_PROGRAM_LIMIT + 1) as u64)
                .read_to_end(&mut bytes)?;
            if bytes.len() > PLAYWRIGHT_PROGRAM_LIMIT {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidInput,
                    format!("Playwright program exceeds the {PLAYWRIGHT_PROGRAM_LIMIT}-byte limit"),
                ));
            }
            String::from_utf8(bytes).map_err(|_| {
                io::Error::new(
                    io::ErrorKind::InvalidInput,
                    "Playwright program on stdin must be UTF-8",
                )
            })?
        }
        [program] => {
            if program.len() > PLAYWRIGHT_PROGRAM_LIMIT {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidInput,
                    format!("Playwright program exceeds the {PLAYWRIGHT_PROGRAM_LIMIT}-byte limit"),
                ));
            }
            program.clone()
        }
        _ => {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "playwright requires <program> or '-' for stdin",
            ));
        }
    };
    if program.trim().is_empty() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "playwright received an empty program",
        ));
    }
    Ok(program)
}

/// Bridges the current process's stdin/stdout (compact newline-delimited CDP JSON, matching
/// `stdio_transport`'s framing) to the relay's authenticated loopback WebSocket. Both transports
/// already speak the same `CdpEnvelope` wire format, so this is pure message pass-through: dbgjs
/// does not interpret CDP itself here, it only relays bytes between the two connections.
async fn run_relay_stdio(websocket_url: &str) -> Result<(), Box<dyn std::error::Error>> {
    use dbgjs::cdp_transport::ManagedCdpTransport;
    use dbgjs::stdio_transport::CdpStdioTransport;
    use dbgjs::websocket_transport::CdpWebSocketTransport;
    use linkrpc::prelude::MessageTransport;

    let websocket = CdpWebSocketTransport::connect(websocket_url).await?;
    let stdio = CdpStdioTransport::new(
        tokio::io::BufReader::new(tokio::io::stdin()),
        tokio::io::stdout(),
    );
    loop {
        tokio::select! {
            incoming = websocket.recv() => {
                match incoming {
                    Some(envelope) => stdio.send(envelope).await?,
                    None => break,
                }
            }
            outgoing = stdio.recv() => {
                match outgoing {
                    Some(envelope) => websocket.send(envelope).await?,
                    None => break,
                }
            }
        }
    }
    websocket.close().await;
    stdio.close().await;
    Ok(())
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct PlaywrightProgramResult {
    ok: bool,
    #[serde(default)]
    has_value: bool,
    #[serde(default)]
    value: serde_json::Value,
    error: Option<String>,
}

fn playwright_result_with_cleanup<T>(
    result: Result<T, Box<dyn std::error::Error>>,
    cleanup_error: Option<String>,
) -> Result<T, Box<dyn std::error::Error>> {
    match (result, cleanup_error) {
        (Err(primary), Some(cleanup)) => {
            Err(io::Error::other(format!("{primary}; additionally, {cleanup}")).into())
        }
        (Ok(_), Some(cleanup)) => Err(io::Error::other(cleanup).into()),
        (result, None) => result,
    }
}

async fn run_playwright_program(
    endpoint: &str,
    program: &str,
    target_id: &str,
    deadline: tokio::time::Instant,
) -> Result<Option<serde_json::Value>, Box<dyn std::error::Error>> {
    let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
    let helper_budget = remaining
        .saturating_sub(CLEANUP_RESERVE)
        .min(PLAYWRIGHT_EXECUTION_TIMEOUT.saturating_sub(CLEANUP_RESERVE));
    if helper_budget.is_zero() {
        return Err(io::Error::new(
            io::ErrorKind::TimedOut,
            "Playwright starting exceeded its deadline",
        )
        .into());
    }
    let playwright_package = dbgjs::connection_provider::find_playwright_package()?;
    let node = env::var_os("DBGJS_NODE").unwrap_or_else(|| "node".into());
    let mut child = TokioCommand::new(&node)
        .arg("--input-type=module")
        .arg("--eval")
        .arg(PLAYWRIGHT_PAGE_HELPER)
        .env("DBGJS_PLAYWRIGHT_ENDPOINT", endpoint)
        .env("DBGJS_PLAYWRIGHT_PACKAGE", playwright_package)
        .env("DBGJS_PLAYWRIGHT_PROGRESS", "1")
        .env(
            "DBGJS_PLAYWRIGHT_TIMEOUT_MS",
            helper_budget.as_millis().to_string(),
        )
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .kill_on_drop(true)
        .spawn()
        .map_err(|error| {
            io::Error::new(
                error.kind(),
                format!("failed to start Playwright with {node:?}: {error}"),
            )
        })?;
    let mut child_stdin = child
        .stdin
        .take()
        .ok_or_else(|| io::Error::other("Playwright child has no stdin"))?;
    let program = program.as_bytes().to_vec();
    let writer = tokio::spawn(async move {
        child_stdin.write_all(&program).await?;
        child_stdin.shutdown().await
    });
    let stdout = child
        .stdout
        .take()
        .ok_or_else(|| io::Error::other("Playwright child has no stdout"))?;
    let stderr = child
        .stderr
        .take()
        .ok_or_else(|| io::Error::other("Playwright child has no stderr"))?;
    let (overflow_sender, mut overflow_receiver) = mpsc::channel(1);
    let stdout_reader = tokio::spawn(read_bounded(
        stdout,
        PLAYWRIGHT_OUTPUT_LIMIT,
        overflow_sender.clone(),
    ));
    let stderr_reader = tokio::spawn(read_bounded(
        stderr,
        PLAYWRIGHT_ERROR_LIMIT,
        overflow_sender,
    ));

    enum Completion {
        Exited(Result<std::process::ExitStatus, io::Error>),
        TimedOut,
        OutputExceeded,
    }
    let started = tokio::time::Instant::now();
    let completion = {
        let wait = child.wait();
        tokio::pin!(wait);
        tokio::select! {
            status = &mut wait => Completion::Exited(status),
            _ = tokio::time::sleep_until(deadline - Duration::from_secs(2)) => Completion::TimedOut,
            Some(_) = overflow_receiver.recv() => Completion::OutputExceeded,
        }
    };
    let status = match completion {
        Completion::Exited(status) => status?,
        Completion::TimedOut => {
            let _ = child.start_kill();
            let _ = tokio::time::timeout(Duration::from_secs(1), child.wait()).await;
            writer.abort();
            stdout_reader.abort();
            let stderr = match tokio::time::timeout(Duration::from_secs(1), stderr_reader).await {
                Ok(Ok(Ok((stderr, _)))) => stderr,
                _ => Vec::new(),
            };
            let (phase, _, _) = playwright_progress(&stderr);
            return Err(io::Error::new(
                io::ErrorKind::TimedOut,
                format!(
                    "Playwright {phase} exceeded the {}-second limit (elapsed {} ms, target {})",
                    PLAYWRIGHT_EXECUTION_TIMEOUT.as_secs(),
                    started.elapsed().as_millis(),
                    target_id,
                ),
            )
            .into());
        }
        Completion::OutputExceeded => {
            let _ = child.start_kill();
            let _ = tokio::time::timeout(Duration::from_secs(1), child.wait()).await;
            writer.abort();
            stdout_reader.abort();
            stderr_reader.abort();
            return Err(
                io::Error::other("Playwright program output exceeded its size limit").into(),
            );
        }
    };
    let write_result = writer.await.map_err(io::Error::other)?;
    let stdout = stdout_reader.await.map_err(io::Error::other)??.0;
    let stderr = stderr_reader.await.map_err(io::Error::other)??.0;
    let (_, _, stderr) = playwright_progress(&stderr);
    let result = parse_playwright_output(status, &stdout, &stderr).map_err(|error| {
        io::Error::new(
            error.kind(),
            error.to_string().replace(endpoint, "<playwright-endpoint>"),
        )
    })?;
    write_result?;
    if !stderr.is_empty() {
        eprint!("{}", String::from_utf8_lossy(&stderr));
    }
    Ok(result)
}

fn playwright_progress(stderr: &[u8]) -> (&'static str, u64, Vec<u8>) {
    const PREFIX: &str = "DBGJS_PLAYWRIGHT_PROGRESS:";
    let mut phase = "starting";
    let mut elapsed_ms = 0;
    let mut output = Vec::with_capacity(stderr.len());
    for line in stderr.split_inclusive(|byte| *byte == b'\n') {
        let text = std::str::from_utf8(line).unwrap_or("");
        let progress = text
            .strip_prefix(PREFIX)
            .and_then(|value| serde_json::from_str::<serde_json::Value>(value.trim_end()).ok());
        if let Some(progress) = progress {
            if let (Some(next), Some(elapsed)) =
                (progress["phase"].as_str(), progress["elapsedMs"].as_u64())
            {
                if matches!(
                    next,
                    "starting" | "connecting" | "initializing" | "executing" | "closing"
                ) {
                    phase = match next {
                        "starting" => "starting",
                        "connecting" => "connecting",
                        "initializing" => "initializing",
                        "executing" => "executing",
                        _ => "closing",
                    };
                    elapsed_ms = elapsed;
                    continue;
                }
            }
        }
        output.extend_from_slice(line);
    }
    (phase, elapsed_ms, output)
}

fn parse_playwright_output(
    status: std::process::ExitStatus,
    stdout: &[u8],
    stderr: &[u8],
) -> Result<Option<serde_json::Value>, io::Error> {
    if !status.success() {
        let reported_error = serde_json::from_slice::<PlaywrightProgramResult>(trim_ascii(stdout))
            .ok()
            .and_then(|result| result.error);
        return Err(io::Error::other(format!(
            "Playwright exited with {status}{}; stderr: {}",
            reported_error.map_or_else(String::new, |error| format!(": {error}")),
            String::from_utf8_lossy(stderr)
        )));
    }
    let result: PlaywrightProgramResult =
        serde_json::from_slice(trim_ascii(stdout)).map_err(|error| {
            io::Error::other(format!(
                "Playwright returned an invalid result: {error}; stderr: {}",
                String::from_utf8_lossy(stderr)
            ))
        })?;
    if !result.ok {
        return Err(io::Error::other(
            result
                .error
                .unwrap_or_else(|| "Playwright program failed".to_owned()),
        ));
    }
    Ok(result.has_value.then_some(result.value))
}

async fn read_bounded<R>(
    mut reader: R,
    limit: usize,
    overflow: mpsc::Sender<()>,
) -> Result<(Vec<u8>, bool), io::Error>
where
    R: AsyncRead + Unpin,
{
    let mut collected = Vec::new();
    let mut buffer = [0_u8; 8192];
    let mut exceeded = false;
    loop {
        let count = reader.read(&mut buffer).await?;
        if count == 0 {
            return Ok((collected, exceeded));
        }
        let remaining = limit.saturating_sub(collected.len());
        collected.extend_from_slice(&buffer[..count.min(remaining)]);
        if count > remaining && !exceeded {
            exceeded = true;
            let _ = overflow.try_send(());
        }
    }
}

fn trim_ascii(bytes: &[u8]) -> &[u8] {
    let start = bytes
        .iter()
        .position(|byte| !byte.is_ascii_whitespace())
        .unwrap_or(bytes.len());
    let end = bytes
        .iter()
        .rposition(|byte| !byte.is_ascii_whitespace())
        .map_or(start, |index| index + 1);
    &bytes[start..end]
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

fn rpc<T, E: std::fmt::Display>(result: Result<T, E>) -> Result<T, io::Error> {
    result.map_err(|error| io::Error::other(error.to_string()))
}

fn usage() -> &'static str {
    "usage: dbgjs [--json] <command>

commands:
  dbgjs --version | -V | version
    reports the binary's build version, source commit, and tracked-worktree dirty status; supports --json
  dbgjs daemon view [--context <id> | --all-contexts]
  dbgjs service status|stop
  dbgjs process list --root <vscode|node|electron|browser> [--full] [--no-cmd-line] [--stats] [--filter <tree-path>] [--no-trim]
  dbgjs process list --vscode [--full] [--no-cmd-line] [--stats] [--filter <tree-path>] [--no-trim]
  dbgjs process attach <process-reference> [--context <id>] [--set] [--force]
  dbgjs context list
  dbgjs context create <path|:id> [display-name] [--set]
  dbgjs context show [--context <path|:id>]
  dbgjs context delete [--context <path|:id>] [--expected-revision <revision>] [--request-id <id>]
  dbgjs context relay --stdio [--context <id>]
  dbgjs state get [--context <id>]
  dbgjs state watch [--context <id>] [--after-revision <revision>]
  dbgjs events --after-revision <revision> [--context <id>]
  dbgjs set context --context <id>
  dbgjs set target --target <selector> [--context <id>] [--connection <id>]
  dbgjs connection list [--status <status>] [--kind <kind>] [--context <id>] [--connection <id>]
  dbgjs connection add <ws-endpoint> --connection <id> [--context <id>] [--connect]
  dbgjs connection add --node-inspector <ws-endpoint> --connection <id> [--context <id>] --connect
  dbgjs connection add --node <program> --connection <id> [--context <id>] [--cwd <path>] [--runtime-executable <path>] [--runtime-arg <value>]... [--arg <value>]... [--env <name=value>]... [--connect] [--set]
  dbgjs connection add --stdio --connection <id> [--context <id>] [--cwd <path>] [--env <name=value>]... [--topology target|browser] [--connect] [--set] -- <command> [args...]
  dbgjs connection add --process <process-id> --connection <id> [--context <id>] --connect
  dbgjs connection add --process-tree <root-pid> --connection <id> [--context <id>] --connect
  dbgjs connection add --playwright <url> --connection <id> [--context <id>] [--channel <channel>] [--headed] [--ignore-https-errors] [--connect] [--set]
  dbgjs connection add --chrome <url> --connection <id> [--context <id>] --executable <path> [--headed] [--user-data-dir <path>] [--arg <value>]... [--connect] [--set]
  dbgjs connection connect|disconnect [--context <id>] [--connection <id>]
  dbgjs connection pause-future on|off [--context <id>] [--connection <id>]
  dbgjs connection delete [--context <id>] [--connection <id>] [--expected-revision <revision>] [--request-id <id>]
  dbgjs breakpoint set <breakpoint-id> <source-url> <line> [--column <column>] [--context <id>]
  dbgjs breakpoint configure <breakpoint-id> <source-url> <line> <column> [--context <id>] [--disabled] [--condition <expression>] [--target <target>] [--expected-revision <revision>] [--request-id <id>]
  dbgjs breakpoint delete <breakpoint-id> [--context <id>] [--expected-revision <revision>] [--request-id <id>]
  dbgjs source formatting get|set <off|auto|on> [--context <id>]
  dbgjs source formatting rule list [--context <id>]
  dbgjs source formatting rule add --mode <off|auto|on> [--target <glob>] [--url <glob>] [--context <id>]
  dbgjs source formatting rule remove <rule-id> [--context <id>]
  dbgjs source list [--path <substring>] [--context <id>]
  dbgjs source resolve|endpoints|explain <path> [--context <id>]
  dbgjs source tree <loaded|source-mapped|formatted|resolved> [--max-lines <count>] [--all] [--no-trim] [--context <id>]
  dbgjs source graph [--uncompacted] [--context <id>]
  dbgjs source show <path> [--line <line>] [--context-lines <lines>] [--view <original|formatted>] [--context <id>]
  dbgjs source grep <pattern> [--path <substring>] [--regex] [--ignore-case] [--max-results <count>] [--context-lines <lines>] [--timeout-ms <ms>] [--max-output-bytes <bytes>] [--max-line-bytes <bytes>] [--verbose-diagnostics] [--view <original|formatted>] [--context <id>]
  dbgjs source map <path> <line> <column> [--context <id>]
  dbgjs source cache evict [--context <id>]
  dbgjs source export <destination> [--context <id>]
  dbgjs target list [--type <type>] [--title <substring>] [--url <substring>] [--attached|--unattached] [target scope]
  dbgjs target graph [--context <id>]
  dbgjs target show [target scope]
  dbgjs target attach [target scope] [--set] [--force]
  dbgjs target release [target scope]
  dbgjs target wait breakpoint-installed <breakpoint-id> [timeout-ms] [target scope]
  dbgjs target wait paused <after-epoch> [timeout-ms] [target scope]
  dbgjs target wait running [target scope]
  dbgjs target resume [--epoch <epoch>] [target scope]
  dbgjs target step into|over|out [--epoch <epoch>] [target scope]
  dbgjs target eval <expression|-> [--full | --max-preview-length <n>] [target scope]
    '-' reads the expression from stdin; --full preserves complete strings, not recursive object serialization
  dbgjs playwright <program|-> [target scope]
    exposes the selected target as `page`; use return for results; console logs go to stderr
    '-' reads the program from stdin
  dbgjs target watch <expression> [target scope]
  dbgjs target cdp <method> [--params <json>] [--session-id <id>] [--no-validation] [target scope]
  dbgjs target relay --stdio [target scope]
  dbgjs value <expression> [--allow-side-effects] [--max-preview-length <count>] [--max-properties <count>] [target scope]
  dbgjs value --object-id <remote-object-id> [--max-preview-length <count>] [--max-properties <count>] [target scope]
  dbgjs target logpoint <id> <source> <line> <column> <expression> [--require-installed [--timeout-ms <ms>]] [target scope]
  dbgjs target logpoint delete <id> [target scope]
  dbgjs target logpoints (<id> <source> <line> <column> <expression>)+ [--require-installed [--timeout-ms <ms>]] [target scope]
    default accepts configuration even when pending or unresolved; --require-installed waits up to 30s (max 300s) and exits nonzero unless every live binding is installed
  dbgjs log [--after <cursor>] [--limit <count>] [target scope]
    reports target-local console capture coverage, not browser/network diagnostics; does not attach
  dbgjs target click <css-selector> [target scope]
  dbgjs target type <text> [target scope]
  dbgjs screenshot capture [--output <path>] [target scope]
  dbgjs coverage start [target scope]
  dbgjs coverage capture [--id <name>] [--raw] [--path-prefix <prefix> | --path-glob <glob>] [--max-lines <count>] [--all] [--no-trim] [target scope]
    --raw collects counts and runtime offsets without source-map lookup or symbol enrichment
  dbgjs coverage stop [--id <name>] [target scope]
  dbgjs coverage show [<selector>] [--exclude <baseline>] [--path-prefix <prefix> | --path-glob <glob>] [--max-lines <count>] [--all] [--no-trim] [target scope]
    filters match normalized source URLs, including authored ranges inside bundles
    --path is a deprecated prefix alias, not a substring match; example glob: '**/issue/**'
    captures have immutable IDs; . and .1 select latest by kind across the context, .2 the previous
    explicit --target/--connection filters narrow history before selection; stored reads ignore live target selection
  dbgjs profile start [--sampling-interval <duration>] [target scope]
  dbgjs profile stop [--id <name>] [target scope]
  dbgjs profile show [<name>] [--view <functions|files>] [--sort <self|total>] [--path <source-prefix>] [--max-lines <count>] [--context <id>]
  dbgjs profile export [<name>] --output <path> [--context <id>]
  dbgjs heap capture [--id <name>] [--capture-numeric-value] [--expose-internals] [target scope]
  dbgjs capture list [--context <id>]
  dbgjs capture show <name> [--context <id>]
    renders the kind's default bounded view: coverage tree, CPU profile, or heap classes
    --json returns catalog metadata; use coverage show, profile show, or heap classes for view options and JSON data
  dbgjs capture delete <name> [--context <id>]
  dbgjs promise list [<capture>] [--state <pending|fulfilled|rejected|unknown>] [--limit <count>] [--max-preview-length <count>] [target scope]
  dbgjs heap classes [<name>] [--capture] [--filter <regex>] [--sort-by-instances] [--instances] [--max-lines <count>] [--all] [--no-cache] [--no-trim]
  dbgjs heap supply-map <capture> <script-id> <captured-script-hash> <map-file>
  dbgjs heap select [<capture>] [--id <heap-object-id>] [--type <kind>] [--name <text>|--name-regex <regex>] [--string-grep <text>|--string-regex <regex>] [--min-size <bytes>] [--max-size <bytes>] [--limit <count>] [--dominators] [--full-strings]
  dbgjs heap strings (--grep <text>|--regex <regex>) [--capture <name>] [--limit <count>] [--full-strings]
  dbgjs heap show <capture#heap-object-id> [--limit <count>|--all] [--full-strings]
  dbgjs heap refs <capture#heap-object-id> [--incoming|--outgoing|--both] [--all-edges] [--limit <count>]
  dbgjs heap path <from-ref> <to-ref> [--direction <outgoing|incoming|either>] [--all-edges] [--readable]
  dbgjs heap root-path|retainer-path|dominators <capture#heap-object-id>
  dbgjs heap aggregate [<capture>] [--by <type|name|string>] [--limit <count>] [--full-strings]
  dbgjs heap diff <older-capture> <newer-capture> [--by <type|name|string>] [--limit <count>] [--full-strings]
  dbgjs heap snapshot <path> [--capture-numeric-value] [--expose-internals] [target scope]

target scope:
  [--context <id>] [--target <selector>] [--connection <id>]
  Accepted by target, page, value, log, screenshot, coverage, profile, promise, and heap commands.
  Exact canonical target IDs resolve context-wide without --connection.

target cdp validates params against the generated CDP schema by default.
Use --no-validation for vendor or newer protocol methods.
Use --session-id to address a flattened session returned by Target.attachToTarget.

context relay exposes every target in a context as one virtual browser-root CDP
endpoint; target relay exposes exactly one target as a direct CDP root. Both
speak compact newline-delimited JSON on stdin/stdout. Opening a relay takes
exclusive ownership of its context: local target debugging commands fail
until the relay process exits or the context is deleted."
}

#[cfg(test)]
mod tests {
    use super::{
        AttachOptions, CliSelection, ConnectionKindFilter, ConnectionStatusFilter,
        DEFAULT_HEAP_SHOW_REFERENCE_LIMIT, DEFAULT_HEAP_STRING_LENGTH,
        DEFAULT_VALUE_PROPERTY_LIMIT, ProcessAttachLocator, ResolvedScope, ScopeOptions,
        SelectionStore, TargetListOptions, activate_selection_scope, apply_scope_selection,
        connection_list_output, extract_scope_options, load_selection_store, parse_attach_options,
        parse_chrome_options, parse_connection_list_options, parse_context_create_options,
        parse_context_option, parse_coverage_capture_options, parse_coverage_show_options,
        parse_cpu_profile_sampling_interval, parse_cpu_profile_start_options, parse_eval_options,
        parse_heap_capture_options, parse_heap_class_options, parse_heap_path_options,
        parse_heap_select_options, parse_heap_show_options, parse_heap_string_options,
        parse_mutation_options, parse_node_options, parse_process_attach_options,
        parse_process_list_options, parse_promise_list_options, parse_raw_cdp_options,
        parse_screenshot_capture_options, parse_source_formatting_rule, parse_source_grep_options,
        parse_source_grep_cli_options,
        parse_source_map_arguments, parse_source_show_options, parse_source_tree_options,
        parse_source_view, parse_stdio_options, parse_target_list_options, parse_value_options,
        png_dimensions, read_eval_expression, read_playwright_program, resolve_target_scope,
        select_implicit_context, split_heap_reference_cli, target_list_output,
    };
    use super::{
        ProcessAttachTarget, RendererAttachSelector, process_attach_destination,
        select_renderer_target, wait_for_renderer_target_id,
    };
    use dbgjs::context_identity::ContextKind;
    use dbgjs::service_api::{
        CdpStdioTopology, ConnectionConfiguration, ConnectionSnapshot, ConnectionStatus,
        ContextSnapshot, ContextSummary, HeapEdgePolicy, HeapPathCost, HeapPathDirection,
        ProcessRootKind, ProcessTreeSnapshot, PromiseState, ResourceGraphSnapshot,
        ResourceSnapshot, SourceFormattingMode, SourceViewPreference, TargetSnapshot,
        ValueSelector,
    };
    use std::fs;
    use std::time::Duration;

    fn arguments(values: &[&str]) -> Vec<String> {
        values.iter().map(|value| (*value).to_owned()).collect()
    }

    #[test]
    fn node_pid_attachment_does_not_attach_its_enclosing_ide() {
        let tree: ProcessTreeSnapshot = serde_json::from_value(serde_json::json!({
            "rootProcessId": 100,
            "processes": [{
                "processId": 200, "name": "node.exe", "commandLine": "node app.mjs",
                "creationDate": "", "role": "node", "attachable": true,
                "debugTargetId": "process-200-instance"
            }, {
                "processId": 300, "name": "Code.exe", "commandLine": "--type=renderer",
                "creationDate": "", "role": "renderer", "attachable": true,
                "debugTargetId": "process-300-instance"
            }],
            "runtimeMetadataAvailable": false
        }))
        .unwrap();
        assert_eq!(
            process_attach_destination(
                ProcessAttachLocator::Process(200),
                std::slice::from_ref(&tree),
            )
            .unwrap(),
            (
                "process-200".to_owned(),
                ConnectionConfiguration::Process { process_id: 200 },
                ProcessAttachTarget::Target("$node-root".to_owned()),
            )
        );
        assert_eq!(
            process_attach_destination(
                ProcessAttachLocator::Process(300),
                std::slice::from_ref(&tree),
            )
            .unwrap(),
            (
                "process-tree-100".to_owned(),
                ConnectionConfiguration::ProcessTree { root_pid: 100 },
                ProcessAttachTarget::Renderer(RendererAttachSelector::Process(300)),
            )
        );
        assert_eq!(
            process_attach_destination(
                ProcessAttachLocator::ProcessTreeProcess {
                    root_pid: 100,
                    process_id: 200,
                },
                &[tree],
            )
            .unwrap(),
            (
                "process-tree-100".to_owned(),
                ConnectionConfiguration::ProcessTree { root_pid: 100 },
                ProcessAttachTarget::Target("process-200-instance".to_owned()),
            )
        );
    }

    fn renderer_resource(id: &str, process_id: u32, window_id: Option<u32>) -> ResourceSnapshot {
        ResourceSnapshot {
            id: format!("electron-web-contents:100/{id}"),
            kinds: vec!["page".to_owned()],
            label: None,
            attributes: serde_json::from_value(serde_json::json!({
                "connectionId": "process-tree-100",
                "targetId": id,
                "processId": process_id,
                "primaryWindowId": window_id,
                "subtype": "electron-renderer",
                "title": format!("Title {id}"),
                "url": format!("file:///{id}.html"),
            }))
            .unwrap(),
            contributors: Vec::new(),
            capabilities: Vec::new(),
            frontiers: Vec::new(),
        }
    }

    #[test]
    fn window_attachment_preserves_identity_despite_stale_or_multiple_renderer_pids() {
        for processes in [
            serde_json::json!([]),
            serde_json::json!([
                {"processId": 11, "name": "oopif", "commandLine": "", "creationDate": "",
                 "role": "renderer", "windowId": 7, "attachable": true, "debugTargetId": "old-oopif"},
                {"processId": 22, "name": "main", "commandLine": "", "creationDate": "",
                 "role": "renderer", "windowId": 7, "attachable": true, "debugTargetId": "old-main"}
            ]),
        ] {
            let tree: ProcessTreeSnapshot = serde_json::from_value(serde_json::json!({
                "rootProcessId": 100,
                "processes": processes,
                "runtimeMetadataAvailable": false
            }))
            .unwrap();
            assert_eq!(
                process_attach_destination(
                    ProcessAttachLocator::VscodeWindow {
                        root_pid: 100,
                        window_id: 7
                    },
                    &[tree]
                )
                .unwrap(),
                (
                    "process-tree-100".to_owned(),
                    ConnectionConfiguration::ProcessTree { root_pid: 100 },
                    ProcessAttachTarget::Renderer(RendererAttachSelector::Window {
                        root_pid: 100,
                        window_id: 7
                    }),
                )
            );
        }
    }

    #[test]
    fn window_attachment_selects_primary_web_contents_not_shared_pid_or_nested_target() {
        let mut graph = ResourceGraphSnapshot {
            revision: 1,
            resources: vec![
                renderer_resource("renderer-1", 22, Some(8)),
                renderer_resource("renderer-2", 22, Some(7)),
                renderer_resource("renderer-3", 22, None),
            ],
            relations: Vec::new(),
        };
        let window = RendererAttachSelector::Window {
            root_pid: 100,
            window_id: 7,
        };
        assert_eq!(
            select_renderer_target(&graph, "ctx", "process-tree-100", window),
            Ok(Some("renderer-2".to_owned()))
        );
        assert_eq!(
            select_renderer_target(&graph, "ctx", "other-connection", window),
            Ok(None)
        );
        graph.resources.remove(1);
        assert_eq!(
            select_renderer_target(&graph, "ctx", "process-tree-100", window),
            Ok(None)
        );
        graph
            .resources
            .push(renderer_resource("renderer-4", 33, Some(7)));
        assert_eq!(
            select_renderer_target(&graph, "ctx", "process-tree-100", window),
            Ok(Some("renderer-4".to_owned()))
        );
        assert_eq!(
            select_renderer_target(
                &graph,
                "ctx",
                "process-tree-100",
                RendererAttachSelector::Process(11)
            ),
            Ok(None)
        );
    }

    #[test]
    fn renderer_attachment_ambiguity_is_strict_and_actionable_for_pids_and_windows() {
        let graph = ResourceGraphSnapshot {
            revision: 1,
            resources: vec![
                renderer_resource("renderer-1", 22, Some(7)),
                renderer_resource("renderer-2", 22, Some(7)),
            ],
            relations: Vec::new(),
        };
        for selector in [
            RendererAttachSelector::Process(22),
            RendererAttachSelector::Window {
                root_pid: 100,
                window_id: 7,
            },
        ] {
            let error =
                select_renderer_target(&graph, "ctx", "process-tree-100", selector).unwrap_err();
            assert!(error.contains(&format!("{selector} maps to multiple")));
            for id in ["renderer-1", "renderer-2"] {
                assert!(error.contains(&format!("title: \"Title {id}\"")));
                assert!(error.contains(&format!("URL: \"file:///{id}.html\"")));
                assert!(error.contains(&format!(
                    "dbgjs target attach --context \":ctx\" --target \"process-tree-100/{id}\""
                )));
            }
        }
    }

    #[tokio::test]
    async fn renderer_attachment_waits_for_incomplete_discovery() {
        let mut attempts = 0;
        let target = wait_for_renderer_target_id(
            "ctx",
            "process-tree-100",
            RendererAttachSelector::Window {
                root_pid: 100,
                window_id: 7,
            },
            Duration::from_secs(2),
            async || {
                attempts += 1;
                Ok(ResourceGraphSnapshot {
                    revision: attempts,
                    resources: if attempts == 1 {
                        Vec::new()
                    } else {
                        vec![renderer_resource("renderer-2", 22, Some(7))]
                    },
                    relations: Vec::new(),
                })
            },
        )
        .await
        .unwrap();
        assert_eq!(target, "renderer-2");
        assert_eq!(attempts, 2);
    }

    #[tokio::test]
    async fn renderer_attachment_bounds_incomplete_and_stalled_discovery() {
        for stalled in [false, true] {
            let result = tokio::time::timeout(
                Duration::from_secs(1),
                wait_for_renderer_target_id(
                    "ctx",
                    "process-tree-100",
                    RendererAttachSelector::Window {
                        root_pid: 100,
                        window_id: 7,
                    },
                    Duration::from_millis(10),
                    async || {
                        if stalled {
                            std::future::pending::<()>().await;
                        }
                        Ok(ResourceGraphSnapshot {
                            revision: 1,
                            resources: Vec::new(),
                            relations: Vec::new(),
                        })
                    },
                ),
            )
            .await
            .expect("resolution must remain bounded");
            let error = result.unwrap_err().to_string();
            assert!(error.contains("w:100/7 has no live Electron webContents"));
            assert!(error.contains("after 0.01s; discovery may be incomplete"));
        }
    }

    #[test]
    fn reads_target_eval_expression_from_argument_or_stdin_without_ambiguity() {
        assert_eq!(
            read_eval_expression(&arguments(&["answer + 1"]), "".as_bytes()).unwrap(),
            "answer + 1"
        );
        assert_eq!(
            read_eval_expression(&arguments(&["-"]), "answer\n  + 1\n".as_bytes()).unwrap(),
            "answer\n  + 1\n"
        );
        assert!(read_eval_expression(&arguments(&["-"]), " \n".as_bytes()).is_err());
        assert!(
            read_eval_expression(&arguments(&["answer", "-"]), "".as_bytes())
                .err()
                .unwrap()
                .to_string()
                .contains("'-' alone")
        );
    }

    #[test]
    fn target_eval_options_preserve_full_and_bounded_strings() {
        let full = parse_eval_options(
            &arguments(&["--full", "-"]),
            "JSON.stringify(value)".as_bytes(),
        )
        .unwrap();
        assert_eq!(full.expression, "JSON.stringify(value)");
        assert_eq!(full.max_preview_length, u32::MAX);
        assert!(full.full);
        let bounded = parse_eval_options(
            &arguments(&["answer", "--max-preview-length", "2000"]),
            "".as_bytes(),
        )
        .unwrap();
        assert_eq!(bounded.expression, "answer");
        assert_eq!(bounded.max_preview_length, 2000);
        assert!(!bounded.full);
        let default = parse_eval_options(&arguments(&["-1"]), "".as_bytes()).unwrap();
        assert_eq!(default.max_preview_length, 120);
        assert_eq!(default.expression, "-1");
        assert!(!default.full);
        assert_eq!(
            parse_eval_options(&arguments(&["--counter"]), "".as_bytes())
                .unwrap()
                .expression,
            "--counter",
        );
        assert_eq!(
            parse_eval_options(&arguments(&["--", "--counter"]), "".as_bytes())
                .unwrap()
                .expression,
            "--counter",
        );
        let mut command = arguments(&["target", "eval", "--context", ":test", "--", "--full"]);
        let scope = extract_scope_options(&mut command).unwrap();
        assert_eq!(scope.context.as_deref(), Some(":test"));
        assert_eq!(
            parse_eval_options(&command[2..], "".as_bytes())
                .unwrap()
                .expression,
            "--full",
        );
        for invalid in [
            vec!["--full", "--max-preview-length", "10", "answer"],
            vec!["answer", "--max-preview-length"],
            vec!["answer", "--max-preview-length", "-1"],
            vec!["answer", "--unknown"],
            vec!["--full"],
        ] {
            assert!(
                parse_eval_options(&arguments(&invalid), "".as_bytes()).is_err(),
                "{invalid:?}"
            );
        }
    }

    #[test]
    fn reads_playwright_program_from_argument_or_stdin() {
        assert_eq!(
            read_playwright_program(&arguments(&["return await page.title()"]), "".as_bytes())
                .unwrap(),
            "return await page.title()"
        );
        assert_eq!(
            read_playwright_program(&arguments(&["-"]), &b"await page.mouse.wheel(0, 800)"[..])
                .unwrap(),
            "await page.mouse.wheel(0, 800)"
        );
        assert!(read_playwright_program(&arguments(&[]), "".as_bytes()).is_err());
        assert!(read_playwright_program(&arguments(&["-"]), " \n".as_bytes()).is_err());

        let mut scoped = arguments(&[
            "playwright",
            "await page.keyboard.press('ArrowRight')",
            "--context",
            "ctx",
            "--connection",
            "browser",
            "--target",
            "page",
        ]);
        let scope = extract_scope_options(&mut scoped).unwrap();
        assert_eq!(
            scoped,
            arguments(&["playwright", "await page.keyboard.press('ArrowRight')"])
        );
        assert_eq!(scope.context.as_deref(), Some("ctx"));
        assert_eq!(scope.connection.as_deref(), Some("browser"));
        assert_eq!(scope.target.as_deref(), Some("page"));
    }

    #[test]
    fn context_delete_uses_standard_mutation_options() {
        let options = parse_mutation_options(&arguments(&[
            "--expected-revision",
            "7",
            "--request-id",
            "cleanup",
        ]))
        .unwrap();
        assert_eq!(options.expected_revision, Some(7));
        assert_eq!(options.request_id.as_deref(), Some("cleanup"));
        assert!(parse_mutation_options(&arguments(&["--disconnect-connections"])).is_err());
    }

    #[test]
    fn parses_raw_cdp_params_and_validation_bypass() {
        let defaults = parse_raw_cdp_options(&[]).unwrap();
        assert_eq!(defaults.params, serde_json::json!({}));
        assert!(defaults.validate);
        assert_eq!(defaults.session_id, None);

        let options = parse_raw_cdp_options(&arguments(&[
            "--params",
            r#"{"expression":"globalThis.location.href"}"#,
            "--no-validation",
            "--session-id",
            "nested-session",
        ]))
        .unwrap();
        assert_eq!(
            options.params,
            serde_json::json!({ "expression": "globalThis.location.href" })
        );
        assert!(!options.validate);
        assert_eq!(options.session_id.as_deref(), Some("nested-session"));
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
    fn parses_bounded_promise_options() {
        let options = parse_promise_list_options(&arguments(&[
            "capture",
            "--state",
            "rejected",
            "--limit",
            "8",
            "--max-preview-length",
            "32",
        ]))
        .unwrap();
        assert_eq!(options.capture_id, "capture");
        assert_eq!(options.state, Some(PromiseState::Rejected));
        assert_eq!(options.limit, 8);
        assert_eq!(options.max_preview_length, 32);
    }

    #[test]
    fn parses_value_selectors_and_side_effect_policy() {
        let mut scoped = arguments(&[
            "value",
            "user.promise",
            "--context",
            "app",
            "--connection",
            "node",
            "--target",
            "$node-root",
        ]);
        let scope = extract_scope_options(&mut scoped).unwrap();
        assert_eq!(scoped, arguments(&["value", "user.promise"]));
        assert_eq!(
            scope,
            ScopeOptions {
                context: Some("app".to_owned()),
                connection: Some("node".to_owned()),
                target: Some("$node-root".to_owned()),
            }
        );

        let options = parse_value_options(&arguments(&["user.promise"])).unwrap();
        assert_eq!(
            options.selector,
            ValueSelector::Expression {
                expression: "user.promise".to_owned(),
                allow_side_effects: false,
            }
        );
        assert_eq!(options.max_preview_length, 120);
        assert_eq!(options.max_properties, DEFAULT_VALUE_PROPERTY_LIMIT);

        let options = parse_value_options(&arguments(&[
            "refresh()",
            "--allow-side-effects",
            "--max-preview-length",
            "40",
            "--max-properties",
            "7",
        ]))
        .unwrap();
        assert_eq!(
            options.selector,
            ValueSelector::Expression {
                expression: "refresh()".to_owned(),
                allow_side_effects: true,
            }
        );
        assert_eq!(options.max_preview_length, 40);
        assert_eq!(options.max_properties, 7);

        let options = parse_value_options(&arguments(&["--object-id", "{\"id\":1}"])).unwrap();
        assert_eq!(
            options.selector,
            ValueSelector::RemoteObject {
                object_id: "{\"id\":1}".to_owned(),
            }
        );
        assert!(
            parse_value_options(&arguments(&[
                "--object-id",
                "{\"id\":1}",
                "--allow-side-effects"
            ]))
            .is_err()
        );
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
    fn parses_source_tree_kind_and_output_bounds() {
        let (kind, options) =
            parse_source_tree_options(&arguments(&["resolved", "--max-lines", "42", "--no-trim"]))
                .unwrap();
        assert_eq!(kind, dbgjs::service_api::SourceTreeKind::Resolved);
        assert_eq!(options.max_lines, 42);
        assert!(!options.all);
        assert!(!options.trim_width);

        let (kind, options) = parse_source_tree_options(&arguments(&["loaded", "--all"])).unwrap();
        assert_eq!(kind, dbgjs::service_api::SourceTreeKind::Loaded);
        assert!(options.all);
        assert!(options.trim_width);

        let (kind, _) = parse_source_tree_options(&arguments(&["source-mapped"])).unwrap();
        assert_eq!(kind, dbgjs::service_api::SourceTreeKind::SourceMapped);
        let (kind, _) = parse_source_tree_options(&arguments(&["formatted"])).unwrap();
        assert_eq!(kind, dbgjs::service_api::SourceTreeKind::Formatted);

        assert!(parse_source_tree_options(&arguments(&["loaded", "--max-lines", "0"])).is_err());
        assert!(parse_source_tree_options(&arguments(&["unknown"])).is_err());
        assert!(parse_source_tree_options(&[]).is_err());
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
            "--force",
        ]);
        let scope = extract_scope_options(&mut args).unwrap();
        let options = parse_process_attach_options(&args[2..]).unwrap();
        assert_eq!(options.locator, ProcessAttachLocator::Process(15388));
        assert_eq!(scope.context.as_deref(), Some("linkrpc-ext-host"));
        assert!(options.set_default);
        assert!(options.force);
        assert!(parse_process_attach_options(&arguments(&["linkrpc-ext-host", "15388"])).is_err());
        assert_eq!(
            parse_process_attach_options(&arguments(&["p:15388"]))
                .unwrap()
                .locator,
            ProcessAttachLocator::Process(15388)
        );
        assert_eq!(
            parse_process_attach_options(&arguments(&["w:100/7"]))
                .unwrap()
                .locator,
            ProcessAttachLocator::VscodeWindow {
                root_pid: 100,
                window_id: 7,
            }
        );
        assert_eq!(
            parse_process_attach_options(&arguments(&["vscode://100/process/15388"]))
                .unwrap()
                .locator,
            ProcessAttachLocator::VscodeProcess {
                root_pid: 100,
                process_id: 15388,
            }
        );
        assert_eq!(
            parse_process_attach_options(&arguments(&["process-tree://100/process/15388"]))
                .unwrap()
                .locator,
            ProcessAttachLocator::ProcessTreeProcess {
                root_pid: 100,
                process_id: 15388,
            }
        );
        assert_eq!(
            parse_process_attach_options(&arguments(&["vscode://100/window/7"]))
                .unwrap()
                .locator,
            ProcessAttachLocator::VscodeWindow {
                root_pid: 100,
                window_id: 7,
            }
        );
    }

    #[test]
    fn parses_target_attach_ownership_options() {
        assert_eq!(
            parse_attach_options(&arguments(&["--set", "--force"])).unwrap(),
            AttachOptions {
                set_default: true,
                force: true,
            }
        );
        assert!(parse_attach_options(&arguments(&["--force", "--force"])).is_err());
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
    fn parses_discovery_filters_and_scope_independently() {
        let connection = parse_connection_list_options(&arguments(&[
            "--status",
            "connected",
            "--kind",
            "direct-cdp",
        ]))
        .unwrap();
        assert_eq!(connection.status, Some(ConnectionStatusFilter::Connected));
        assert_eq!(connection.kind, Some(ConnectionKindFilter::DirectCdp));

        let mut target_arguments = arguments(&[
            "target",
            "list",
            "--context",
            "ctx",
            "--connection",
            "browser",
            "--target",
            "page",
            "--type",
            "page",
            "--title",
            "checkout",
            "--url",
            "example.test",
            "--attached",
        ]);
        let scope = extract_scope_options(&mut target_arguments).unwrap();
        let target = parse_target_list_options(&target_arguments[2..]).unwrap();
        assert_eq!(scope.context.as_deref(), Some("ctx"));
        assert_eq!(scope.connection.as_deref(), Some("browser"));
        assert_eq!(scope.target.as_deref(), Some("page"));
        assert_eq!(target.target_type.as_deref(), Some("page"));
        assert_eq!(target.title.as_deref(), Some("checkout"));
        assert_eq!(target.url.as_deref(), Some("example.test"));
        assert_eq!(target.attached, Some(true));
        assert!(parse_target_list_options(&arguments(&["--attached", "--unattached"])).is_err());
        assert!(parse_connection_list_options(&arguments(&["--status", "unknown"])).is_err());
    }

    #[test]
    fn discovery_views_filter_snapshots_and_mark_exact_selection() {
        let mut snapshot =
            context_snapshot(&[("browser", &["page-1", "worker-1"]), ("node", &["root"])]);
        snapshot.connections[0].targets[0].target_type = "page".to_owned();
        snapshot.connections[0].targets[0].title = "Checkout".to_owned();
        snapshot.connections[0].targets[0].url = "https://example.test/cart".to_owned();
        snapshot.connections[0].targets[1].target_type = "service_worker".to_owned();
        snapshot.connections[0].targets[1].attached = false;
        snapshot.target_forest = snapshot
            .connections
            .iter()
            .flat_map(ConnectionSnapshot::target_forest)
            .collect();
        snapshot.target_forest[0].attachment = dbgjs::service_api::TargetAttachmentState::Debugger;
        let selection = CliSelection {
            context: Some("ctx".to_owned()),
            connection: Some("browser".to_owned()),
            target: Some("page-1".to_owned()),
            ..CliSelection::default()
        };

        let connections = connection_list_output(
            &snapshot,
            &selection,
            Some("browser"),
            &parse_connection_list_options(&arguments(&["--status", "connected"])).unwrap(),
        );
        assert_eq!(connections.connections.len(), 1);
        assert!(connections.connections[0].selected);
        assert_eq!(connections.connections[0].target_count, 2);

        let targets = target_list_output(
            &snapshot,
            &selection,
            &ScopeOptions {
                connection: Some("browser".to_owned()),
                ..ScopeOptions::default()
            },
            &TargetListOptions {
                target_type: Some("PAGE".to_owned()),
                title: Some("check".to_owned()),
                url: Some("EXAMPLE.TEST".to_owned()),
                attached: Some(true),
            },
        )
        .unwrap();
        assert_eq!(targets.targets.len(), 1);
        assert_eq!(targets.targets[0].target.target_id, "page-1");
        assert!(targets.targets[0].selected);
        assert_eq!(
            serde_json::to_value(&targets.targets[0]).unwrap()["attachment"],
            "debugger",
            "a managed debugger must be distinct from CDP's targetInfo.attached"
        );
        let mut native = snapshot.clone();
        native.target_forest[0].attachment = dbgjs::service_api::TargetAttachmentState::CdpClient;
        let discovered = target_list_output(
            &native,
            &selection,
            &ScopeOptions::default(),
            &TargetListOptions::default(),
        )
        .unwrap();
        let native = discovered.targets.iter().find(|entry| entry.target.target_id == "page-1").unwrap();
        assert_eq!(serde_json::to_value(native).unwrap()["attachment"], "cdpClient");
        let entry = &targets.targets[0];
        let qualified = dbgjs::target_selector::qualified_target_selector(
            &entry.connection_id,
            &entry.target.target_id,
            entry.connection_generation,
        );
        let filtered = target_list_output(
            &snapshot,
            &selection,
            &ScopeOptions {
                target: Some(qualified),
                ..ScopeOptions::default()
            },
            &TargetListOptions::default(),
        )
        .unwrap();
        assert_eq!(filtered.targets.len(), 1);
        assert_eq!(filtered.targets[0].target.target_id, "page-1");
    }

    #[test]
    fn target_list_identity_wins_over_friendly_matches() {
        let mut snapshot = context_snapshot(&[
            ("browser", &["renderer/target/frame", "other"]),
            ("other-browser", &["renderer/target/frame"]),
        ]);
        let selector = format!(
            "browser/renderer/target/frame@{}",
            snapshot.connections[0].generation,
        );
        snapshot.connections[0].targets[1].title = selector.clone();
        snapshot.target_forest = snapshot
            .connections
            .iter()
            .flat_map(ConnectionSnapshot::target_forest)
            .collect();
        let listed = target_list_output(
            &snapshot,
            &CliSelection::default(),
            &ScopeOptions {
                target: Some(selector),
                ..ScopeOptions::default()
            },
            &TargetListOptions::default(),
        )
        .unwrap();
        assert_eq!(listed.targets.len(), 1);
        assert_eq!(listed.targets[0].target.target_id, "renderer/target/frame");
    }

    #[test]
    fn target_list_and_scope_reject_stale_identity_instead_of_matching_title() {
        let mut snapshot = context_snapshot(&[("browser", &["renderer/target/frame", "decoy"])]);
        let generation = snapshot.connections[0].generation;
        let stale = format!("browser/renderer/target/frame@{}", generation + 1);
        snapshot.connections[0].targets[1].title = stale.clone();
        snapshot.target_forest = snapshot
            .connections
            .iter()
            .flat_map(ConnectionSnapshot::target_forest)
            .collect();
        let options = ScopeOptions {
            target: Some(stale),
            ..ScopeOptions::default()
        };
        let error = target_list_output(
            &snapshot,
            &CliSelection::default(),
            &options,
            &TargetListOptions::default(),
        )
        .err()
        .unwrap();
        assert!(
            error.to_string().contains("stale connection generation"),
            "{error}"
        );
        let error = resolve_target_scope(
            "ctx".to_owned(),
            &snapshot,
            &CliSelection::default(),
            &options,
        )
        .unwrap_err();
        assert!(
            error.to_string().contains("stale connection generation"),
            "{error}"
        );
    }

    #[test]
    fn target_scope_resolves_qualified_nested_ids() {
        let snapshot = context_snapshot(&[
            ("process-a", &["renderer-11/target/iframe"]),
            ("process-b", &["renderer-11/target/iframe"]),
        ]);
        let generation = snapshot
            .connections
            .iter()
            .find(|connection| connection.id == "process-b")
            .unwrap()
            .generation;
        for selector in [
            "process-b/renderer-11/target/iframe".to_owned(),
            format!("process-b/renderer-11/target/iframe@{generation}"),
        ] {
            let expected = selector.clone();
            let scope = resolve_target_scope(
                "ctx".to_owned(),
                &snapshot,
                &CliSelection::default(),
                &ScopeOptions {
                    target: Some(selector),
                    ..ScopeOptions::default()
                },
            )
            .unwrap();
            assert_eq!(scope.connection, "process-b");
            assert_eq!(scope.target, expected);
        }
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
    fn log_cursor_scope_changes_on_reconnect_and_reattachment() {
        use dbgjs::service_api::LogCaptureSnapshot;
        let scope = ResolvedScope {
            context: "ctx".into(),
            connection: "browser".into(),
            target: "selected-alias".into(),
        };
        let capture = LogCaptureSnapshot {
            capture_id: Some("first".into()),
            ..Default::default()
        };
        let first = super::log_scope(&scope, "frame", 1, &capture);
        assert_eq!(first, super::log_scope(&scope, "frame", 1, &capture));
        assert_ne!(first, super::log_scope(&scope, "frame", 2, &capture));
        assert_ne!(first, super::log_scope(&scope, "other-frame", 1, &capture));
        let second = LogCaptureSnapshot {
            capture_id: Some("second".into()),
            ..capture
        };
        assert_ne!(first, super::log_scope(&scope, "frame", 1, &second));
        assert_eq!(super::parse_log_options(&[], 17).unwrap(), (17, 20, false));
        assert_eq!(
            super::parse_log_options(&["--after".into(), "0".into()], 17).unwrap(),
            (0, 20, true)
        );
    }

    #[test]
    fn logpoint_options_do_not_consume_expression_starting_with_dashes() {
        let fields = [
            "probe", "file:///app.js", "1", "1", "--counter",
            "--require-installed", "--timeout-ms", "300",
        ].map(str::to_owned);
        let (specs, wait) = super::split_logpoint_install_options(&fields).unwrap();
        assert_eq!(specs.len(), 5);
        assert_eq!(specs[4], "--counter");
        assert_eq!(wait, Some(300));
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
            resource_revision: 1,
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
            source_formatting: Default::default(),
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
        assert_eq!(options.root_kind, ProcessRootKind::Vscode);
        assert!(!options.full);
        assert!(!options.command_line);
        assert!(options.stats);
        assert_eq!(options.filter.as_deref(), Some("window 3"));
        assert!(!options.trim_width);

        let options =
            parse_process_list_options(&arguments(&["--root", "browser", "--full"])).unwrap();
        assert_eq!(options.root_kind, ProcessRootKind::Browser);
        assert!(options.full);
        assert!(parse_process_list_options(&arguments(&["--root", "unknown"])).is_err());
        assert!(parse_process_list_options(&arguments(&["--vscode", "--root", "node"])).is_err());
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
            "--no-trim",
        ]))
        .unwrap();
        assert_eq!(options.capture_id, "startup");
        assert!(options.capture);
        assert_eq!(options.filter.as_deref(), Some(".*PieceTree.*"));
        assert!(options.sort_by_instances);
        assert!(options.instances);
        assert_eq!(options.max_lines, 42);
        assert!(!options.trim_width);
        assert!(
            parse_heap_class_options(&arguments(&["--no-cache"]))
                .err()
                .unwrap()
                .to_string()
                .contains("not supported for captured heaps")
        );
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
    fn bounds_heap_show_references_unless_explicitly_expanded() {
        let defaults = parse_heap_show_options(&arguments(&[])).unwrap();
        assert_eq!(defaults.limit, DEFAULT_HEAP_SHOW_REFERENCE_LIMIT);
        assert_eq!(defaults.max_string_length, Some(DEFAULT_HEAP_STRING_LENGTH));

        let limited =
            parse_heap_show_options(&arguments(&["--limit", "7", "--max-string-length", "40"]))
                .unwrap();
        assert_eq!(limited.limit, 7);
        assert_eq!(limited.max_string_length, Some(40));

        let expanded = parse_heap_show_options(&arguments(&["--all", "--full-strings"])).unwrap();
        assert_eq!(expanded.limit, u32::MAX);
        assert_eq!(expanded.max_string_length, None);
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
    fn coverage_path_matching_is_explicit() {
        let prefix = parse_coverage_show_options(&arguments(&[
            ".2",
            "--path-prefix",
            "https://example.test/src/",
        ]))
        .unwrap();
        assert_eq!(prefix.capture_id, ".2");
        assert_eq!(prefix.path.as_deref(), Some("https://example.test/src/"));
        assert!(!prefix.deprecated_path);

        let glob =
            parse_coverage_show_options(&arguments(&["--path-glob", "**/issue/**"])).unwrap();
        assert_eq!(glob.path_glob.as_deref(), Some("**/issue/**"));
        assert!(glob.path.is_none());
        assert!(
            parse_coverage_show_options(&arguments(&["--path", "src/"]))
                .unwrap()
                .deprecated_path
        );
        for arguments in [
            vec!["--path-prefix"],
            vec!["--path-glob", "--all"],
            vec!["--path-glob", "["],
            vec!["--path-prefix", "src/", "--path-glob", "**"],
            vec!["--path", "src/", "--path-prefix", "other/"],
            vec!["--no-cache"],
        ] {
            let arguments = arguments.into_iter().map(str::to_owned).collect::<Vec<_>>();
            assert!(
                parse_coverage_show_options(&arguments).is_err(),
                "{arguments:?}"
            );
        }
    }

    #[test]
    fn parses_named_coverage_stop_without_exclusion() {
        let options =
            super::parse_coverage_stop_options(&arguments(&["--id", "after-click"])).unwrap();
        assert_eq!(options.capture_id.as_deref(), Some("after-click"));
        assert!(
            super::parse_coverage_stop_options(&[])
                .unwrap()
                .capture_id
                .is_none()
        );
        for values in [
            vec!["--id"],
            vec!["--id", "--exclude", "before"],
            vec!["--id", "one", "--id", "two"],
            vec!["--exclude", "before"],
            vec!["--exclude", "one", "--exclude", "two"],
            vec!["--raw"],
        ] {
            assert!(super::parse_coverage_stop_options(&arguments(&values)).is_err());
        }
    }

    #[test]
    fn parses_coverage_show_exclusion_with_rendering_options() {
        let options = parse_coverage_show_options(&arguments(&[
            ".1",
            "--exclude",
            ".2",
            "--path-glob",
            "**/src/**",
            "--all",
        ]))
        .unwrap();
        assert_eq!(options.capture_id, ".1");
        assert_eq!(options.exclude_capture_id.as_deref(), Some(".2"));
        assert_eq!(options.path_glob.as_deref(), Some("**/src/**"));
        assert!(options.all);
        let latest = parse_coverage_show_options(&arguments(&["--exclude", "baseline"])).unwrap();
        assert_eq!(latest.capture_id, ".");
        for values in [
            vec!["--exclude"],
            vec!["--exclude", ""],
            vec!["--exclude", "--all"],
            vec!["--exclude", "one", "--exclude", "two"],
        ] {
            assert!(parse_coverage_show_options(&arguments(&values)).is_err());
        }
    }

    #[test]
    fn playwright_failed_process_reports_exit_and_stderr_before_json_errors() {
        #[cfg(unix)]
        use std::os::unix::process::ExitStatusExt;
        #[cfg(windows)]
        use std::os::windows::process::ExitStatusExt;
        let status = std::process::ExitStatus::from_raw(256);
        let error = super::parse_playwright_output(status, b"", b"Error: missing browserContextId")
            .unwrap_err()
            .to_string();
        assert!(error.contains("Playwright exited with"), "{error}");
        assert!(error.contains("missing browserContextId"), "{error}");
        assert!(!error.contains("EOF"), "{error}");
    }

    #[test]
    fn playwright_success_requires_valid_result_and_preserves_return_value() {
        #[cfg(unix)]
        use std::os::unix::process::ExitStatusExt;
        #[cfg(windows)]
        use std::os::windows::process::ExitStatusExt;
        let status = std::process::ExitStatus::from_raw(0);
        let value = super::parse_playwright_output(
            status,
            br#"{"ok":true,"hasValue":true,"value":"page title"}"#,
            b"diagnostic",
        )
        .unwrap();
        assert_eq!(value, Some(serde_json::json!("page title")));
        assert!(
            super::parse_playwright_output(status, b"not json", b"")
                .unwrap_err()
                .to_string()
                .contains("invalid result")
        );
        assert!(super::parse_playwright_output(
            status, br#"{"ok":false,"error":"script failed"}"#, b"",
        ).unwrap_err().to_string().contains("script failed"));
    }

    #[test]
    fn playwright_progress_does_not_pollute_program_console_or_disclose_values() {
        let (phase, elapsed, console) = super::playwright_progress(
            b"DBGJS_PLAYWRIGHT_PROGRESS:{\"phase\":\"initializing\",\"elapsedMs\":12}\n\
              user output\n\
              DBGJS_PLAYWRIGHT_PROGRESS:{\"phase\":\"executing\",\"elapsedMs\":20}\n",
        );
        assert_eq!(phase, "executing");
        assert_eq!(elapsed, 20);
        assert_eq!(console, b"user output\n");
    }

    #[test]
    fn playwright_cleanup_preserves_the_program_failure() {
        let failure = super::playwright_result_with_cleanup::<()>(
            Err(std::io::Error::other("program failed").into()),
            Some("proxy failed".into()),
        )
        .unwrap_err()
        .to_string();
        assert!(failure.contains("program failed"), "{failure}");
        assert!(failure.contains("proxy failed"), "{failure}");
    }

    #[test]
    fn parses_coverage_capture_rendering_options() {
        let options = parse_coverage_capture_options(&arguments(&[
            "--id",
            "baseline",
            "--path",
            "src/vs",
            "--max-lines",
            "25",
            "--no-trim",
        ]))
        .unwrap();
        assert_eq!(options.capture_id.as_deref(), Some("baseline"));
        assert_eq!(options.path.as_deref(), Some("src/vs"));
        assert_eq!(options.max_lines, 25);
        assert!(!options.trim_width);
        assert!(parse_coverage_capture_options(&arguments(&["--no-cache"])).is_err());
        assert!(parse_coverage_capture_options(&arguments(&["--id"])).is_err());
        assert!(
            parse_coverage_capture_options(&arguments(&["--id", "one", "--id", "two"])).is_err()
        );
    }

    #[test]
    fn parses_raw_coverage_independently_of_storage_and_rendering() {
        let options = parse_coverage_capture_options(&arguments(&[
            "--raw",
            "--id",
            "sample",
            "--max-lines",
            "5",
        ]))
        .unwrap();
        assert!(options.raw);
        assert_eq!(options.capture_id.as_deref(), Some("sample"));
        assert_eq!(options.max_lines, 5);
        assert!(!parse_coverage_capture_options(&[]).unwrap().raw);
        assert!(parse_coverage_capture_options(&arguments(&["--id", "--raw"])).is_err());
        assert!(parse_coverage_capture_options(&arguments(&["--exclude"])).is_err());
        assert!(parse_coverage_capture_options(&arguments(&["--exclude", "baseline"])).is_err());
        assert!(
            parse_coverage_capture_options(&arguments(&["--exclude", "a", "--exclude", "b"]))
                .is_err()
        );
    }

    #[test]
    fn stored_profile_rejects_unsupported_cache_bypass() {
        let error = super::parse_cpu_profile_show_options(&arguments(&["--no-cache"]))
            .err()
            .unwrap();
        assert!(
            error
                .to_string()
                .contains("not supported for stored CPU profiles")
        );
    }

    #[tokio::test]
    async fn coverage_hint_waits_without_restarting_or_cancelling_the_operation() {
        let (sender, receiver) = tokio::sync::oneshot::channel();
        let result = super::with_delayed_hint(receiver, std::time::Duration::ZERO, || {
            sender.send(42).unwrap()
        })
        .await;
        assert_eq!(result.unwrap(), 42);
        let result = super::with_delayed_hint(
            std::future::ready(Err::<(), _>("original failure")),
            std::time::Duration::ZERO,
            || panic!("a completed operation must not print a hint"),
        )
        .await;
        assert_eq!(result, Err("original failure"));
        let hint =
            super::coverage_delay_hint(&arguments(&["--json", "coverage", "capture"])).unwrap();
        assert!(hint.contains("coverage capture --raw"));
        assert!(
            super::coverage_delay_hint(&arguments(&["coverage", "capture", "--raw"]))
                .unwrap()
                .contains("already skips")
        );
        assert!(super::coverage_delay_hint(&arguments(&["source", "show"])).is_none());
    }

    #[test]
    fn parses_native_chrome_connection_options() {
        let options = parse_chrome_options(&arguments(&[
            "--executable",
            "C:\\Program Files\\Google\\Chrome\\Application\\chrome.exe",
            "--headed",
            "--user-data-dir",
            "C:\\tmp\\dbgjs-chrome",
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
            Some("C:\\tmp\\dbgjs-chrome")
        );
        assert_eq!(
            options.args,
            ["--disable-extensions", "--window-size=1200,800"]
        );
    }

    #[test]
    fn parses_node_connection_options() {
        let options = parse_node_options(&arguments(&[
            "--cwd",
            "/workspace/app",
            "--runtime-executable",
            "/usr/bin/node",
            "--runtime-arg",
            "--enable-source-maps",
            "--arg",
            "worker",
            "--env",
            "NODE_ENV=test",
            "--connect",
            "--set",
        ]))
        .unwrap();
        assert_eq!(options.cwd, "/workspace/app");
        assert_eq!(options.runtime_executable, "/usr/bin/node");
        assert_eq!(options.runtime_args, ["--enable-source-maps"]);
        assert_eq!(options.args, ["worker"]);
        assert_eq!(
            options.env.get("NODE_ENV").map(String::as_str),
            Some("test")
        );
        assert!(options.connect);
        assert!(options.set_default);
    }

    #[test]
    fn parses_stdio_connection_options_and_preserves_command_arguments() {
        let options = parse_stdio_options(&arguments(&[
            "--cwd",
            "/workspace/adapter",
            "--env",
            "TOKEN=test",
            "--topology",
            "browser",
            "--connect",
            "--set",
            "--",
            "./my-cdp-adapter",
            "--foobar",
            "--connection",
            "adapter-owned-value",
        ]))
        .unwrap();
        assert_eq!(options.command, "./my-cdp-adapter");
        assert_eq!(
            options.args,
            ["--foobar", "--connection", "adapter-owned-value"]
        );
        assert_eq!(options.cwd, "/workspace/adapter");
        assert_eq!(options.env.get("TOKEN").map(String::as_str), Some("test"));
        assert_eq!(options.topology, CdpStdioTopology::Browser);
        assert!(options.connect);
        assert!(options.set_default);
    }

    #[test]
    fn scope_extraction_stops_at_stdio_command_delimiter() {
        let mut values = arguments(&[
            "connection",
            "add",
            "--stdio",
            "--connection",
            "runtime",
            "--",
            "./adapter",
            "--connection",
            "adapter-value",
        ]);
        let scope = extract_scope_options(&mut values).unwrap();
        assert_eq!(scope.connection.as_deref(), Some("runtime"));
        assert_eq!(
            values,
            arguments(&[
                "connection",
                "add",
                "--stdio",
                "--",
                "./adapter",
                "--connection",
                "adapter-value",
            ])
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
            "dbgjs-selection-migration-{}-{}.json",
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
            "dbgjs-selection-scope-{}-{}.json",
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
    fn cpu_profile_export_preserves_signed_deltas_and_sample_order() {
        let profile: super::CpuProfileSnapshot = serde_json::from_value(serde_json::json!({
            "captureId": "typing-cpu",
            "samplingIntervalMicros": 1000,
            "startTimeMicros": 10000,
            "endTimeMicros": 11000,
            "nodes": [],
            "samples": [2, 3, 2],
            "timeDeltasMicros": [100, -28, 10]
        }))
        .unwrap();
        let exported = super::cpu_profile_export(&profile);
        assert_eq!(exported["samples"], serde_json::json!([2, 3, 2]));
        assert_eq!(exported["timeDeltas"], serde_json::json!([100, -28, 10]));
        assert_eq!(exported["startTime"], 10000.0);
        assert_eq!(exported["endTime"], 11000.0);
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
            "--timeout-ms",
            "1500",
            "--view",
            "formatted",
        ]))
        .unwrap();
        assert_eq!(options.pattern, "trim.*Whitespace");
        assert_eq!(options.path.as_deref(), Some("src/vs/editor"));
        assert!(options.regex);
        assert!(!options.case_sensitive);
        assert_eq!(options.max_results, 25);
        assert_eq!(options.context_lines, 2);
        assert_eq!(options.timeout_ms, Some(1500));
        assert_eq!(options.view, SourceViewPreference::Formatted);
        let (options, budget, line, verbose) = parse_source_grep_cli_options(&arguments(&[
            "needle", "--max-output-bytes", "2048", "--max-line-bytes", "128",
            "--verbose-diagnostics", "--path", "needle.js",
        ])).unwrap();
        assert_eq!(options.path.as_deref(), Some("needle.js"));
        assert_eq!(options.timeout_ms, Some(30_000));
        assert_eq!((budget, line, verbose), (2048, 128, true));
        assert!(parse_source_grep_cli_options(&arguments(&[
            "needle", "--max-output-bytes", "50",
        ])).is_err());
    }

    #[test]
    fn parses_source_show_and_map() {
        let (path, options) = parse_source_show_options(&arguments(&[
            "src/model.ts",
            "--line",
            "1352",
            "--context-lines",
            "12",
            "--view",
            "original",
        ]))
        .unwrap();
        assert_eq!(path, "src/model.ts");
        assert_eq!(options.line, Some(1352));
        assert_eq!(options.context_lines, 12);
        assert_eq!(options.view, SourceViewPreference::Original);

        let (path, line, column) =
            parse_source_map_arguments(&arguments(&["src/model.ts", "1352", "3"])).unwrap();
        assert_eq!(path, "src/model.ts");
        assert_eq!((line, column), (1352, 3));
    }

    #[test]
    fn parses_source_formatting_rules() {
        let (mode, target, url) = parse_source_formatting_rule(&arguments(&[
            "--mode",
            "auto",
            "--target",
            "page-*",
            "--url",
            "**/*.min.js",
        ]))
        .unwrap();
        assert_eq!(mode, SourceFormattingMode::Auto);
        assert_eq!(target.as_deref(), Some("page-*"));
        assert_eq!(url.as_deref(), Some("**/*.min.js"));
        assert!(parse_source_formatting_rule(&arguments(&["--mode", "on"])).is_err());
        assert!(parse_source_view("policy").is_err());
    }
}
