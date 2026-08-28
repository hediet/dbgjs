use std::fs::{self, File};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Command, ExitStatus, Stdio};
use std::thread;
use std::time::{Duration, Instant};

use serde_json::Value;

use cdp_client::local_rpc::{
    LocalTransportEndpoint, persistent_state_file, read_endpoint, startup_error_file,
};

#[test]
fn cli_resolves_cwd_contexts_with_binding_precedence_and_ranked_listing() {
    let root = std::env::temp_dir().join(format!(
        "jsdbg-context-resolution-{}-{}",
        std::process::id(),
        unique_suffix()
    ));
    let child = root.join("packages").join("ui");
    fs::create_dir_all(&child).unwrap();
    let state_file = root.join("service.json");
    let cli = PathBuf::from(env!("CARGO_BIN_EXE_jsdbg"));
    let service = PathBuf::from(env!("CARGO_BIN_EXE_jsdbg-service"));
    let cleanup = ServiceCleanup::new(cli.clone(), service.clone(), state_file.clone());

    let root_context = run_json_in(
        &cli,
        &service,
        &state_file,
        &root,
        &["context", "create", ".", "Root", "--set"],
    );
    let root_id = root.to_string_lossy().to_lowercase();
    assert_eq!(root_context["id"], root_id.as_ref());

    let child_context = run_json_in(
        &cli,
        &service,
        &state_file,
        &child,
        &["context", "create", ".", "UI"],
    );
    let child_id = child.to_string_lossy().to_lowercase();
    assert_eq!(child_context["id"], child_id.as_ref());

    let inherited = run_json_in(&cli, &service, &state_file, &child, &["context", "show"]);
    assert_eq!(inherited["id"], root_id.as_ref());
    let explicit = run_json_in(
        &cli,
        &service,
        &state_file,
        &child,
        &["context", "show", "--context", "."],
    );
    assert_eq!(explicit["id"], child_id.as_ref());

    let listed = run_json_in(&cli, &service, &state_file, &child, &["context", "list"]);
    assert_eq!(listed[0]["id"], child_id.as_ref());
    assert_eq!(listed[0]["kind"], "path");
    assert_eq!(listed[0]["pathDistance"], 0);
    assert_eq!(listed[1]["id"], root_id.as_ref());
    assert_eq!(listed[1]["pathAncestor"], true);

    run_json_in(
        &cli,
        &service,
        &state_file,
        &root,
        &["context", "delete", "--context", "."],
    );
    let stale = run_in(&cli, &service, &state_file, &child, &["context", "show"]);
    assert!(!stale.0.success());
    assert!(String::from_utf8_lossy(&stale.2).contains("stale context binding"));

    run_json_in(&cli, &service, &state_file, &child, &["service", "stop"]);
    wait_until_removed(&state_file);
    cleanup_persistent_state(&state_file);
    let _ = fs::remove_dir_all(root);
    cleanup.disarm();
}

#[test]
fn cli_spawns_service_and_manages_shared_context_state() {
    let state_file = std::env::temp_dir().join(format!(
        "jsdbg-cli-service-{}-{}.json",
        std::process::id(),
        unique_suffix()
    ));
    let cli = PathBuf::from(env!("CARGO_BIN_EXE_jsdbg"));
    let service = PathBuf::from(env!("CARGO_BIN_EXE_jsdbg-service"));
    let cleanup = ServiceCleanup::new(cli.clone(), service.clone(), state_file.clone());

    let created = run_json(
        &cli,
        &service,
        &state_file,
        &["context", "create", "--context", ":shop", "Shop"],
    );
    assert_eq!(created["id"], "shop");
    assert_eq!(created["revision"], 1);
    let endpoint = read_endpoint(&state_file).unwrap();
    #[cfg(windows)]
    assert!(matches!(
        endpoint.transport,
        LocalTransportEndpoint::NamedPipe { .. }
    ));
    #[cfg(unix)]
    assert!(matches!(
        endpoint.transport,
        LocalTransportEndpoint::UnixSocket { .. }
    ));

    run_json(
        &cli,
        &service,
        &state_file,
        &[
            "connection",
            "add",
            "ws://127.0.0.1:9229",
            "--context",
            ":shop",
            "--connection",
            "server",
        ],
    );
    run_json(
        &cli,
        &service,
        &state_file,
        &[
            "connection",
            "add",
            "ws://127.0.0.1:9222",
            "--context",
            ":shop",
            "--connection",
            "browser",
        ],
    );
    run_json(
        &cli,
        &service,
        &state_file,
        &[
            "breakpoint",
            "set",
            "shared-validation",
            "file:///workspace/shared/validation.ts",
            "41",
            "--column",
            "1",
            "--context",
            ":shop",
        ],
    );

    let snapshot = run_json(
        &cli,
        &service,
        &state_file,
        &["context", "show", "--context", ":shop"],
    );
    assert_eq!(snapshot["revision"], 4);
    assert_eq!(
        snapshot["connections"]
            .as_array()
            .unwrap()
            .iter()
            .map(|connection| connection["id"].as_str().unwrap())
            .collect::<Vec<_>>(),
        vec!["browser", "server"]
    );
    assert_eq!(
        snapshot["breakpoints"][0]["sourcePath"],
        "file:///workspace/shared/validation.ts"
    );
    assert_eq!(snapshot["breakpoints"][0]["status"], "pending");

    let stopped = run_json(&cli, &service, &state_file, &["service", "stop"]);
    assert_eq!(stopped, Value::Bool(true));
    wait_until_removed(&state_file);
    cleanup_persistent_state(&state_file);
    cleanup.disarm();
}

#[test]
fn service_ensure_starts_the_shared_daemon() {
    let state_file = std::env::temp_dir().join(format!(
        "jsdbg-service-ensure-{}-{}.json",
        std::process::id(),
        unique_suffix()
    ));
    let cli = PathBuf::from(env!("CARGO_BIN_EXE_jsdbg"));
    let service = PathBuf::from(env!("CARGO_BIN_EXE_jsdbg-service"));
    let cleanup = ServiceCleanup::new(cli.clone(), service.clone(), state_file.clone());

    let status = Command::new(&service)
        .args(["--ensure", "--state-file"])
        .arg(&state_file)
        .status()
        .unwrap();
    assert!(
        status.success(),
        "jsdbg-service --ensure failed with {status}"
    );
    assert!(read_endpoint(&state_file).is_ok());

    run_json(&cli, &service, &state_file, &["service", "stop"]);
    wait_until_removed(&state_file);
    cleanup_persistent_state(&state_file);
    cleanup.disarm();
}

#[test]
fn cli_manages_lifecycle_concurrency_and_sources() {
    let state_file = std::env::temp_dir().join(format!(
        "jsdbg-cli-management-{}-{}.json",
        std::process::id(),
        unique_suffix()
    ));
    let source_file = state_file.with_extension("source.ts");
    fs::write(
        &source_file,
        "export const validationValue = 42;\nconsole.log(validationValue);\nvoid validationValue;\n",
    )
    .unwrap();
    let cli = PathBuf::from(env!("CARGO_BIN_EXE_jsdbg"));
    let service = PathBuf::from(env!("CARGO_BIN_EXE_jsdbg-service"));
    let cleanup = ServiceCleanup::new(cli.clone(), service.clone(), state_file.clone());

    run_json(
        &cli,
        &service,
        &state_file,
        &["context", "create", "--context", "managed"],
    );
    let source = source_file.to_string_lossy();
    let configured = run_json(
        &cli,
        &service,
        &state_file,
        &[
            "breakpoint",
            "configure",
            "conditional",
            &source,
            "1",
            "1",
            "--context",
            "managed",
            "--disabled",
            "--condition",
            "validationValue > 0",
            "--expected-revision",
            "1",
            "--request-id",
            "configure-1",
        ],
    );
    assert_eq!(configured["breakpoints"][0]["status"], "disabled");
    assert_eq!(
        configured["breakpoints"][0]["condition"],
        "validationValue > 0"
    );
    let observed = run_json(
        &cli,
        &service,
        &state_file,
        &["events", "--after-revision", "1", "--context", "managed"],
    );
    let observed_items = observed["items"].as_array().unwrap();
    assert_eq!(observed_items.len(), 1);
    assert_eq!(observed_items[0]["snapshot"]["revision"], 2);
    assert_eq!(observed_items[0]["events"][0]["kind"], "breakpoint.updated");

    let repeated = run_json(
        &cli,
        &service,
        &state_file,
        &[
            "breakpoint",
            "configure",
            "conditional",
            &source,
            "1",
            "1",
            "--context",
            "managed",
            "--request-id",
            "configure-1",
        ],
    );
    assert_eq!(repeated["revision"], configured["revision"]);

    let matches = run_json(
        &cli,
        &service,
        &state_file,
        &["source", "grep", "validationValue", "--context", "managed"],
    );
    assert_eq!(matches["matches"].as_array().unwrap().len(), 3);
    assert_eq!(matches["omittedMatches"], 0);
    assert_eq!(matches["searchedSources"], 1);
    let bounded_matches = run_json(
        &cli,
        &service,
        &state_file,
        &[
            "source",
            "grep",
            "validationValue",
            "--max-results",
            "1",
            "--context",
            "managed",
        ],
    );
    assert_eq!(bounded_matches["matches"].as_array().unwrap().len(), 1);
    assert_eq!(bounded_matches["omittedMatches"], 2);
    let source_graph = run_json(
        &cli,
        &service,
        &state_file,
        &["source", "map", "show", "--context", "managed"],
    );
    assert_eq!(source_graph["roots"], serde_json::json!([]));
    assert_eq!(source_graph["nodes"], serde_json::json!([]));
    assert_eq!(source_graph["edges"], serde_json::json!([]));

    let configured_revision = configured["revision"].as_u64().unwrap().to_string();
    let deleted = run_json(
        &cli,
        &service,
        &state_file,
        &[
            "breakpoint",
            "delete",
            "conditional",
            "--context",
            "managed",
            "--expected-revision",
            &configured_revision,
        ],
    );
    assert!(deleted["breakpoints"].as_array().unwrap().is_empty());
    let deleted_revision = deleted["revision"].as_u64().unwrap().to_string();
    run_json(
        &cli,
        &service,
        &state_file,
        &[
            "context",
            "delete",
            "--context",
            "managed",
            "--expected-revision",
            &deleted_revision,
            "--request-id",
            "delete-managed",
        ],
    );
    let repeated_delete = run_json(
        &cli,
        &service,
        &state_file,
        &[
            "context",
            "delete",
            "--context",
            "managed",
            "--request-id",
            "delete-managed",
        ],
    );
    assert_eq!(repeated_delete, Value::Bool(true));

    run_json(&cli, &service, &state_file, &["service", "stop"]);
    wait_until_removed(&state_file);
    cleanup_persistent_state(&state_file);
    let _ = fs::remove_file(source_file);
    cleanup.disarm();
}

#[test]
fn context_intent_survives_service_restart() {
    let state_file = std::env::temp_dir().join(format!(
        "jsdbg-cli-restart-{}-{}.json",
        std::process::id(),
        unique_suffix()
    ));
    let cli = PathBuf::from(env!("CARGO_BIN_EXE_jsdbg"));
    let service = PathBuf::from(env!("CARGO_BIN_EXE_jsdbg-service"));
    let cleanup = ServiceCleanup::new(cli.clone(), service.clone(), state_file.clone());

    let created = run_json(
        &cli,
        &service,
        &state_file,
        &["context", "create", "--context", "shop", "Shop"],
    );
    let first_instance = created["agentInstanceId"].as_str().unwrap().to_owned();
    run_json(
        &cli,
        &service,
        &state_file,
        &[
            "connection",
            "add",
            "ws://127.0.0.1:9229",
            "--context",
            "shop",
            "--connection",
            "server",
        ],
    );
    run_json(
        &cli,
        &service,
        &state_file,
        &[
            "breakpoint",
            "set",
            "shared-validation",
            "file:///workspace/shared/validation.ts",
            "41",
            "--column",
            "1",
            "--context",
            "shop",
        ],
    );
    run_json(&cli, &service, &state_file, &["service", "stop"]);
    wait_until_removed(&state_file);

    let restored = run_json(
        &cli,
        &service,
        &state_file,
        &["context", "show", "--context", "shop"],
    );
    assert_ne!(restored["agentInstanceId"], first_instance);
    assert_eq!(restored["displayName"], "Shop");
    assert_eq!(restored["revision"], 3);
    assert_eq!(restored["connections"][0]["id"], "server");
    assert_eq!(restored["connections"][0]["status"]["kind"], "disconnected");
    assert_eq!(
        restored["breakpoints"][0]["sourcePath"],
        "file:///workspace/shared/validation.ts"
    );
    let history_gap = run_json(
        &cli,
        &service,
        &state_file,
        &["events", "--after-revision", "0", "--context", "shop"],
    );
    assert_eq!(history_gap["kind"], "historyGap");
    assert_eq!(history_gap["oldest_available_revision"], 3);
    assert_eq!(history_gap["current"]["revision"], 3);

    run_json(&cli, &service, &state_file, &["service", "stop"]);
    wait_until_removed(&state_file);
    cleanup_persistent_state(&state_file);
    cleanup.disarm();
}

#[test]
fn corrupt_persistence_reports_an_actionable_startup_error() {
    let state_file = std::env::temp_dir().join(format!(
        "jsdbg-cli-corrupt-{}-{}.json",
        std::process::id(),
        unique_suffix()
    ));
    let cli = PathBuf::from(env!("CARGO_BIN_EXE_jsdbg"));
    let service = PathBuf::from(env!("CARGO_BIN_EXE_jsdbg-service"));
    fs::write(persistent_state_file(&state_file), b"{not-json").unwrap();

    let output = Command::new(&cli)
        .args(["context", "list"])
        .env("JSDBG_SERVICE_EXE", &service)
        .env("JSDBG_SERVICE_STATE", &state_file)
        .output()
        .unwrap();

    assert!(!output.status.success());
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("service failed during startup"),
        "unexpected stderr: {stderr}"
    );
    assert!(
        startup_error_file(&state_file).exists(),
        "service should preserve its startup diagnostic"
    );
    assert!(
        !state_file.exists(),
        "a failed startup must not publish an endpoint"
    );
    cleanup_persistent_state(&state_file);
    let _ = fs::remove_file(state_file.with_extension("startup.lock"));
}

#[test]
fn cli_service_connects_to_live_cdp() {
    let Ok(endpoint) = std::env::var("CDP_WS_ENDPOINT") else {
        return;
    };
    let state_file = std::env::temp_dir().join(format!(
        "jsdbg-cli-live-{}-{}.json",
        std::process::id(),
        unique_suffix()
    ));
    let cli = PathBuf::from(env!("CARGO_BIN_EXE_jsdbg"));
    let service = PathBuf::from(env!("CARGO_BIN_EXE_jsdbg-service"));
    let cleanup = ServiceCleanup::new(cli.clone(), service.clone(), state_file.clone());

    run_json(
        &cli,
        &service,
        &state_file,
        &["context", "create", "--context", "live-browser"],
    );
    run_json(
        &cli,
        &service,
        &state_file,
        &[
            "connection",
            "add",
            &endpoint,
            "--context",
            "live-browser",
            "--connection",
            "browser",
        ],
    );
    let connected = run_json(
        &cli,
        &service,
        &state_file,
        &[
            "connection",
            "connect",
            "--context",
            "live-browser",
            "--connection",
            "browser",
        ],
    );

    assert_eq!(connected["connections"][0]["status"]["kind"], "connected");
    assert!(
        connected["connections"][0]["status"]["product"]
            .as_str()
            .is_some_and(|product| !product.is_empty())
    );
    assert!(
        connected["connections"][0]["status"]["protocolVersion"]
            .as_str()
            .is_some_and(|version| !version.is_empty())
    );
    assert!(
        connected["connections"][0]["targets"]
            .as_array()
            .is_some_and(|targets| targets.iter().any(|target| target["targetType"] == "page"))
    );
    let target_id = connected["connections"][0]["targets"]
        .as_array()
        .unwrap()
        .iter()
        .find(|target| target["targetType"] == "page")
        .unwrap()["targetId"]
        .as_str()
        .unwrap()
        .to_owned();

    let positional_eval = run_json(
        &cli,
        &service,
        &state_file,
        &[
            "target",
            "eval",
            "6 * 7",
            "--context",
            "live-browser",
            "--connection",
            "browser",
            "--target",
            &target_id,
        ],
    );
    assert_eq!(positional_eval["preview"]["preview"], "42");

    let stdin_eval = run_json_with_stdin(
        &cli,
        &service,
        &state_file,
        &[
            "target",
            "eval",
            "-",
            "--context",
            "live-browser",
            "--connection",
            "browser",
            "--target",
            &target_id,
        ],
        b"40\n  + 2\n",
    );
    assert_eq!(stdin_eval["preview"]["preview"], "42");

    let bounded_eval = run_json(
        &cli,
        &service,
        &state_file,
        &[
            "target",
            "eval",
            "Object.fromEntries(Array.from({ length: 50 }, (_, index) => [`p${index}`, index]))",
            "--context",
            "live-browser",
            "--connection",
            "browser",
            "--target",
            &target_id,
        ],
    );
    assert_eq!(bounded_eval["properties"].as_array().unwrap().len(), 20);
    assert_eq!(bounded_eval["omittedPropertyCount"], 30);
    assert!(!contains_reference(&bounded_eval));

    let ambiguous = run_in(
        &cli,
        &service,
        &state_file,
        &std::env::current_dir().unwrap(),
        &[
            "target",
            "eval",
            "-",
            "6 * 7",
            "--context",
            "live-browser",
            "--connection",
            "browser",
            "--target",
            &target_id,
        ],
    );
    assert!(!ambiguous.0.success());
    assert!(String::from_utf8_lossy(&ambiguous.2).contains("use '-' alone to read from stdin"));

    let long_value = "x".repeat(140);
    let setup_expression = format!(
        "globalThis.__jsdbgConsistentValue = {{ short: 'ok', long: '{long_value}', nested: {{ answer: 42 }} }}"
    );
    let eval_json = run_json(
        &cli,
        &service,
        &state_file,
        &[
            "target",
            "eval",
            &setup_expression,
            "--context",
            "live-browser",
            "--connection",
            "browser",
            "--target",
            &target_id,
        ],
    );
    let value_json = run_json(
        &cli,
        &service,
        &state_file,
        &[
            "value",
            "globalThis.__jsdbgConsistentValue",
            "--context",
            "live-browser",
            "--connection",
            "browser",
            "--target",
            &target_id,
        ],
    );
    assert_eq!(
        value_presentation(&eval_json),
        value_presentation(&redact_references(value_json.clone()))
    );
    assert!(!contains_reference(&eval_json));
    let long_property = eval_json["properties"]
        .as_array()
        .unwrap()
        .iter()
        .find(|property| property["name"] == "long")
        .unwrap();
    assert_eq!(
        long_property["value"]["preview"]
            .as_str()
            .unwrap()
            .chars()
            .count(),
        120
    );
    assert_eq!(long_property["value"]["truncated"], true);

    let eval_human = run_human_in(
        &cli,
        &service,
        &state_file,
        &std::env::current_dir().unwrap(),
        &[
            "target",
            "eval",
            "globalThis.__jsdbgConsistentValue",
            "--context",
            "live-browser",
            "--connection",
            "browser",
            "--target",
            &target_id,
        ],
    );
    assert_success(
        &["target", "eval", "globalThis.__jsdbgConsistentValue"],
        eval_human.0,
        &eval_human.1,
        &eval_human.2,
    );
    let value_human = run_human_in(
        &cli,
        &service,
        &state_file,
        &std::env::current_dir().unwrap(),
        &[
            "value",
            "globalThis.__jsdbgConsistentValue",
            "--context",
            "live-browser",
            "--connection",
            "browser",
            "--target",
            &target_id,
        ],
    );
    assert_success(
        &["value", "globalThis.__jsdbgConsistentValue"],
        value_human.0,
        &value_human.1,
        &value_human.2,
    );
    let eval_rendering = normalize_value_rendering(&String::from_utf8(eval_human.1).unwrap());
    let value_rendering = normalize_value_rendering(&String::from_utf8(value_human.1).unwrap());
    assert_eq!(eval_rendering, value_rendering);
    let transcript = format!(
        "$ jsdbg target eval globalThis.__jsdbgConsistentValue\n{eval_rendering}\
         $ jsdbg value globalThis.__jsdbgConsistentValue\n{value_rendering}\
         equivalent bounded rendering: yes\n\
         target eval raw references exposed: no\n"
    );
    print!("{transcript}");
    assert_eq!(
        transcript,
        include_str!("transcripts/consistent-values.txt")
    );

    let connections = run_json(
        &cli,
        &service,
        &state_file,
        &[
            "connection",
            "list",
            "--context",
            "live-browser",
            "--status",
            "connected",
            "--kind",
            "direct-cdp",
        ],
    );
    assert_eq!(connections["contextId"], connected["id"]);
    assert!(connections["revision"].as_u64().unwrap() >= connected["revision"].as_u64().unwrap());
    assert_eq!(connections["connections"][0]["id"], "browser");
    assert!(
        connections["connections"][0]["targetCount"]
            .as_u64()
            .is_some_and(|count| count >= 1)
    );
    assert_eq!(connections["connections"][0]["status"]["kind"], "connected");

    let targets = run_json(
        &cli,
        &service,
        &state_file,
        &[
            "target",
            "list",
            "--context",
            "live-browser",
            "--connection",
            "browser",
            "--type",
            "page",
        ],
    );
    let page = targets["targets"]
        .as_array()
        .unwrap()
        .iter()
        .find(|target| target["targetType"] == "page")
        .expect("target discovery should include a page");
    assert_eq!(targets["contextId"], connected["id"]);
    assert_eq!(page["connectionId"], "browser");
    assert!(page["targetId"].as_str().is_some_and(|id| !id.is_empty()));
    assert!(page["url"].as_str().is_some());

    let connections_human = run_human(
        &cli,
        &service,
        &state_file,
        &[
            "connection",
            "list",
            "--context",
            "live-browser",
            "--connection",
            "browser",
        ],
    );
    assert!(connections_human.contains("Connections in context"));
    assert!(connections_human.contains("kind=direct-cdp"));
    let targets_human = run_human(
        &cli,
        &service,
        &state_file,
        &[
            "target",
            "list",
            "--context",
            "live-browser",
            "--connection",
            "browser",
            "--type",
            "page",
        ],
    );
    assert!(targets_human.contains("Targets in context"));
    assert!(targets_human.contains("[page"));

    let disconnected = run_json(
        &cli,
        &service,
        &state_file,
        &[
            "connection",
            "disconnect",
            "--context",
            "live-browser",
            "--connection",
            "browser",
        ],
    );
    assert_eq!(
        disconnected["connections"][0]["status"]["kind"],
        "disconnected"
    );
    assert_eq!(
        disconnected["connections"][0]["targets"]
            .as_array()
            .unwrap()
            .len(),
        0
    );

    let reconnected = run_json(
        &cli,
        &service,
        &state_file,
        &[
            "connection",
            "connect",
            "--context",
            "live-browser",
            "--connection",
            "browser",
        ],
    );
    assert_eq!(reconnected["connections"][0]["status"]["kind"], "connected");
    assert_eq!(reconnected["connections"][0]["generation"], 2);
    run_json(
        &cli,
        &service,
        &state_file,
        &[
            "connection",
            "add",
            &endpoint,
            "--context",
            "live-browser",
            "--connection",
            "observer",
            "--connect",
        ],
    );

    let refused_delete = run_in(
        &cli,
        &service,
        &state_file,
        &std::env::current_dir().unwrap(),
        &["context", "delete", "--context", "live-browser"],
    );
    assert!(!refused_delete.0.success());
    let guidance = String::from_utf8_lossy(&refused_delete.2);
    assert!(guidance.contains("disconnect them before deleting"));
    assert!(guidance.contains("jsdbg connection disconnect"));
    assert!(guidance.contains(r#"--connection "browser""#));
    assert!(guidance.contains(r#"--connection "observer""#));
    assert!(guidance.contains("jsdbg context delete"));
    assert!(guidance.contains("--disconnect-connections"));

    let preserved = run_json(
        &cli,
        &service,
        &state_file,
        &["context", "show", "--context", "live-browser"],
    );
    assert!(
        preserved["connections"]
            .as_array()
            .unwrap()
            .iter()
            .all(|connection| connection["status"]["kind"] == "connected")
    );

    let cascaded_delete = run_json(
        &cli,
        &service,
        &state_file,
        &[
            "context",
            "delete",
            "--context",
            "live-browser",
            "--disconnect-connections",
        ],
    );
    assert_eq!(cascaded_delete, Value::Bool(true));

    run_json(&cli, &service, &state_file, &["service", "stop"]);
    wait_until_removed(&state_file);
    cleanup_persistent_state(&state_file);
    cleanup.disarm();
}

fn value_presentation(value: &Value) -> Value {
    serde_json::json!({
        "subtype": value["subtype"],
        "className": value["className"],
        "preview": value["preview"],
        "properties": value["properties"],
        "promise": value["promise"],
    })
}

fn redact_references(mut value: Value) -> Value {
    match &mut value {
        Value::Array(values) => {
            for value in values {
                *value = redact_references(value.take());
            }
        }
        Value::Object(fields) => {
            if fields.contains_key("reference") {
                fields.insert("reference".to_owned(), Value::Null);
            }
            for value in fields.values_mut() {
                *value = redact_references(value.take());
            }
        }
        _ => {}
    }
    value
}

fn contains_reference(value: &Value) -> bool {
    match value {
        Value::Array(values) => values.iter().any(contains_reference),
        Value::Object(fields) => {
            fields
                .get("reference")
                .is_some_and(|reference| !reference.is_null())
                || fields.values().any(contains_reference)
        }
        _ => false,
    }
}

fn normalize_value_rendering(rendering: &str) -> String {
    let mut result = String::new();
    for line in rendering.lines() {
        let line = line
            .rsplit_once(" (")
            .filter(|(_, suffix)| suffix.ends_with(')'))
            .map_or(line, |(value, _)| value);
        if line.starts_with("  long: ") {
            result.push_str("  long: <120 x characters>...\n");
        } else {
            result.push_str(line);
            result.push('\n');
        }
    }
    result
}

fn run_json(cli: &Path, service: &Path, state_file: &Path, arguments: &[&str]) -> Value {
    run_json_in(
        cli,
        service,
        state_file,
        &std::env::current_dir().unwrap(),
        arguments,
    )
}

fn run_json_in(
    cli: &Path,
    service: &Path,
    state_file: &Path,
    cwd: &Path,
    arguments: &[&str],
) -> Value {
    let (status, stdout, stderr) = run_in(cli, service, state_file, cwd, arguments);
    assert_success(arguments, status, &stdout, &stderr);
    serde_json::from_slice(&stdout).unwrap_or_else(|error| {
        panic!(
            "invalid JSON for {arguments:?}: {error}\nstdout: {}\nstderr: {}",
            String::from_utf8_lossy(&stdout),
            String::from_utf8_lossy(&stderr)
        )
    })
}

fn run_json_with_stdin(
    cli: &Path,
    service: &Path,
    state_file: &Path,
    arguments: &[&str],
    stdin: &[u8],
) -> Value {
    let mut child = Command::new(cli)
        .arg("--json")
        .args(arguments)
        .env("JSDBG_SERVICE_EXE", service)
        .env("JSDBG_SERVICE_STATE", state_file)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    child.stdin.take().unwrap().write_all(stdin).unwrap();
    let output = child.wait_with_output().unwrap();
    assert_success(arguments, output.status, &output.stdout, &output.stderr);
    serde_json::from_slice(&output.stdout).unwrap()
}

fn run_in(
    cli: &Path,
    service: &Path,
    state_file: &Path,
    cwd: &Path,
    arguments: &[&str],
) -> (ExitStatus, Vec<u8>, Vec<u8>) {
    run_in_with_format(cli, service, state_file, cwd, arguments, true)
}

fn run_human(cli: &Path, service: &Path, state_file: &Path, arguments: &[&str]) -> String {
    let (status, stdout, stderr) = run_in_with_format(
        cli,
        service,
        state_file,
        &std::env::current_dir().unwrap(),
        arguments,
        false,
    );
    assert_success(arguments, status, &stdout, &stderr);
    String::from_utf8(stdout).expect("human CLI output should be UTF-8")
}

fn run_human_in(
    cli: &Path,
    service: &Path,
    state_file: &Path,
    cwd: &Path,
    arguments: &[&str],
) -> (ExitStatus, Vec<u8>, Vec<u8>) {
    run_in_with_format(cli, service, state_file, cwd, arguments, false)
}

fn run_in_with_format(
    cli: &Path,
    service: &Path,
    state_file: &Path,
    cwd: &Path,
    arguments: &[&str],
    json: bool,
) -> (ExitStatus, Vec<u8>, Vec<u8>) {
    let suffix = unique_suffix();
    let stdout_path = state_file.with_extension(format!("{suffix}.stdout"));
    let stderr_path = state_file.with_extension(format!("{suffix}.stderr"));
    let mut command = Command::new(cli);
    command.current_dir(cwd);
    if json {
        command.arg("--json");
    }
    let status = command
        .args(arguments)
        .env("JSDBG_SERVICE_EXE", service)
        .env("JSDBG_SERVICE_STATE", state_file)
        .stdout(Stdio::from(File::create(&stdout_path).unwrap()))
        .stderr(Stdio::from(File::create(&stderr_path).unwrap()))
        .status()
        .unwrap();
    let stdout = fs::read(&stdout_path).unwrap();
    let stderr = fs::read(&stderr_path).unwrap();
    let _ = fs::remove_file(stdout_path);
    let _ = fs::remove_file(stderr_path);
    (status, stdout, stderr)
}

fn assert_success(arguments: &[&str], status: ExitStatus, stdout: &[u8], stderr: &[u8]) {
    assert!(
        status.success(),
        "command {arguments:?} failed with {}\nstdout: {}\nstderr: {}",
        status,
        String::from_utf8_lossy(stdout),
        String::from_utf8_lossy(stderr)
    );
}

fn wait_until_removed(path: &Path) {
    let deadline = Instant::now() + Duration::from_secs(5);
    while path.exists() && Instant::now() < deadline {
        thread::sleep(Duration::from_millis(20));
    }
    assert!(!path.exists(), "service state file was not removed");
    let _ = fs::remove_file(path.with_extension("startup.lock"));
}

fn cleanup_persistent_state(path: &Path) {
    let _ = fs::remove_file(persistent_state_file(path));
    let _ = fs::remove_file(startup_error_file(path));
}

fn unique_suffix() -> String {
    format!(
        "{}",
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    )
}

struct ServiceCleanup {
    cli: PathBuf,
    service: PathBuf,
    state_file: PathBuf,
    armed: std::cell::Cell<bool>,
}

impl ServiceCleanup {
    fn new(cli: PathBuf, service: PathBuf, state_file: PathBuf) -> Self {
        Self {
            cli,
            service,
            state_file,
            armed: std::cell::Cell::new(true),
        }
    }

    fn disarm(&self) {
        self.armed.set(false);
    }
}

impl Drop for ServiceCleanup {
    fn drop(&mut self) {
        if !self.armed.get() || !self.state_file.exists() {
            return;
        }
        let _ = Command::new(&self.cli)
            .args(["service", "stop"])
            .env("JSDBG_SERVICE_EXE", &self.service)
            .env("JSDBG_SERVICE_STATE", &self.state_file)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status();
    }
}
