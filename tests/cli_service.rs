use std::fs::{self, File};
use std::io::{BufRead, BufReader, Write};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, ExitStatus, Stdio};
use std::sync::{Arc, Mutex, mpsc};
use std::thread;
use std::time::{Duration, Instant};

use serde_json::Value;

use cdp_client::local_rpc::{
    LocalTransportEndpoint, persistent_state_file, read_endpoint, startup_error_file,
};

#[test]
fn electron_bridge_recovers_from_failed_initialization_and_enforces_ownership() {
    let output = Command::new("node")
        .arg(
            PathBuf::from(env!("CARGO_MANIFEST_DIR"))
                .join("tests")
                .join("electron_bridge_ownership.mjs"),
        )
        .output()
        .unwrap();
    assert_success(
        &["node", "tests/electron_bridge_ownership.mjs"],
        output.status,
        &output.stdout,
        &output.stderr,
    );
    let transcript = String::from_utf8(output.stdout).unwrap();
    assert_eq!(
        transcript,
        include_str!("transcripts/electron-attachment-ownership.txt")
    );
}

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
    #[cfg(unix)]
    let root_id = fs::canonicalize(&root)
        .unwrap()
        .to_string_lossy()
        .to_lowercase();
    #[cfg(not(unix))]
    let root_id = root.to_string_lossy().to_lowercase();
    assert_eq!(root_context["id"], root_id.as_ref());

    let child_context = run_json_in(
        &cli,
        &service,
        &state_file,
        &child,
        &["context", "create", ".", "UI"],
    );
    #[cfg(unix)]
    let child_id = fs::canonicalize(&child)
        .unwrap()
        .to_string_lossy()
        .to_lowercase();
    #[cfg(not(unix))]
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
fn cli_connects_to_a_target_over_mcp_style_stdio() {
    let root = std::env::temp_dir().join(format!(
        "jsdbg-stdio-{}-{}",
        std::process::id(),
        unique_suffix()
    ));
    fs::create_dir_all(&root).unwrap();
    let state_file = root.join("service.json");
    let cli = PathBuf::from(env!("CARGO_BIN_EXE_jsdbg"));
    let service = PathBuf::from(env!("CARGO_BIN_EXE_jsdbg-service"));
    let cleanup = ServiceCleanup::new(cli.clone(), service.clone(), state_file.clone());

    run_json_in(
        &cli,
        &service,
        &state_file,
        &root,
        &["context", "create", ":stdio", "Stdio", "--set"],
    );
    let connected = run_json_in(
        &cli,
        &service,
        &state_file,
        &root,
        &[
            "connection",
            "add",
            "--stdio",
            "--connection",
            "adapter",
            "--connect",
            "--",
            "node",
            "--input-type=module",
            "--eval",
            "process.stdin.resume(); process.stdin.on('end', () => process.exit(0));",
        ],
    );

    let connection = &connected["connections"][0];
    assert_eq!(connection["configuration"]["kind"], "stdio");
    assert_eq!(connection["configuration"]["command"], "node");
    assert_eq!(connection["configuration"]["topology"], "target");
    assert_eq!(connection["status"]["kind"], "connected");
    assert_eq!(connection["targets"][0]["targetType"], "runtime");
    assert_eq!(connection["targets"][0]["url"], "stdio:node");

    run_json_in(
        &cli,
        &service,
        &state_file,
        &root,
        &["connection", "disconnect", "--connection", "adapter"],
    );
    run_json_in(&cli, &service, &state_file, &root, &["service", "stop"]);
    wait_until_removed(&state_file);
    cleanup_persistent_state(&state_file);
    let _ = fs::remove_dir_all(root);
    cleanup.disarm();
}

/// A minimal CDP-over-stdio target: acknowledges every request with an empty result (enough
/// for the debugger driver's attach handshake), answers `Runtime.evaluate` for real, and emits
/// one raw `Runtime.consoleAPICalled` event right after its first response so relay tests can
/// verify raw event mirroring alongside request/response forwarding.
const FAKE_CDP_TARGET_SCRIPT: &str = r#"
let buffer = '';
process.stdin.setEncoding('utf8');
process.stdin.on('data', (chunk) => {
  buffer += chunk;
  let index;
  while ((index = buffer.indexOf('\n')) >= 0) {
    const line = buffer.slice(0, index);
    buffer = buffer.slice(index + 1);
    if (!line.trim()) continue;
    const message = JSON.parse(line);
    if (message.id === undefined) continue;
    let result = {};
    let emitEvent = false;
    if (message.method === 'Runtime.evaluate') {
      const value = eval(String(message.params.expression));
      result = { result: { type: typeof value, value } };
      // Only fire once the relay client's own probe arrives, well after the driver's
      // attach handshake (Runtime.enable/Debugger.enable/...) - a raw event emitted during
      // attach itself would be broadcast before any relay client has subscribed to it.
      emitEvent = true;
    } else if (message.method === 'Debugger.enable') {
      result = { debuggerId: 'fake-debugger-id' };
    }
    process.stdout.write(JSON.stringify({ id: message.id, result }) + '\n');
    if (emitEvent) {
      process.stdout.write(JSON.stringify({
        method: 'Runtime.consoleAPICalled',
        params: {
          type: 'log',
          args: [{ type: 'string', value: 'hello-from-fake-target' }],
          executionContextId: 1,
          timestamp: Date.now(),
        },
      }) + '\n');
    }
  }
});
process.stdin.on('end', () => process.exit(0));
"#;

#[test]
fn cli_log_reports_empty_capture_retention_and_reconnect() {
    let root = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("artifacts")
        .join(format!("log-coverage-{}-{}", std::process::id(), unique_suffix()));
    fs::create_dir_all(&root).unwrap();
    let state_file = root.join("service.json");
    let cli = PathBuf::from(env!("CARGO_BIN_EXE_jsdbg"));
    let service = PathBuf::from(env!("CARGO_BIN_EXE_jsdbg-service"));
    let cleanup = ServiceCleanup::new(cli.clone(), service.clone(), state_file.clone());
    let run = |arguments: &[&str]| run_json_in(&cli, &service, &state_file, &root, arguments);
    run(&["context", "create", ":log-coverage", "Logs", "--set"]);
    let connected = run(&[
        "connection", "add", "--stdio", "--connection", "adapter", "--connect",
        "--", "node", "--input-type=module", "--eval", FAKE_CDP_TARGET_SCRIPT,
    ]);
    let target = connected["connections"][0]["targets"][0]["targetId"].as_str().unwrap();
    let scoped = |arguments: &[&str]| {
        let mut args = arguments.to_vec();
        args.extend(["--connection", "adapter", "--target", target]);
        run(&args)
    };
    let inactive = scoped(&["log"]);
    assert_eq!(inactive["capture"]["status"], "inactive");
    assert_eq!(inactive["messages"], serde_json::json!([]));
    assert!(inactive["capture"]["startedAtUnixMs"].is_null());
    assert!(inactive["capture"]["evictedCount"].is_null());

    let attached = scoped(&["target", "attach"]);
    let empty = scoped(&["log"]);
    assert_eq!(empty["capture"], attached["target"]["logCapture"]);
    assert_eq!(empty["capture"]["status"], "active");
    assert_eq!(empty["capture"]["collectedEvents"], serde_json::json!(["Runtime.consoleAPICalled"]));
    assert!(empty["capture"]["startedAtUnixMs"].is_u64());
    assert!(empty["capture"]["droppedCount"].is_null());
    assert_eq!(empty["messages"], serde_json::json!([]));
    let human = run_human_in(
        &cli, &service, &state_file, &root,
        &["log", "--connection", "adapter", "--target", target],
    );
    assert_success(&["log"], human.0, &human.1, &human.2);
    assert!(String::from_utf8(human.1).unwrap().contains("this does not mean no errors occurred"));

    scoped(&[
        "target", "cdp", "Runtime.evaluate", "--params",
        r#"{"expression":"42"}"#,
    ]);
    let deadline = Instant::now() + Duration::from_secs(5);
    let message = loop {
        let snapshot = scoped(&["log", "--after", "0"]);
        if !snapshot["messages"].as_array().unwrap().is_empty() {
            break snapshot;
        }
        assert!(Instant::now() < deadline, "console event was not captured");
        thread::sleep(Duration::from_millis(20));
    };
    assert_eq!(message["messages"][0]["index"], 1);
    assert_eq!(message["messages"][0]["params"]["executionContextId"], 1);
    assert_eq!(message["messages"][0]["values"][0], "hello-from-fake-target");
    assert_eq!(message["nextCursor"], 1);
    assert_eq!(scoped(&["log"])["messages"].as_array().unwrap().len(), 1);
    assert_eq!(scoped(&["log"])["messages"], serde_json::json!([]));

    run(&["connection", "disconnect", "--connection", "adapter"]);
    run(&["connection", "connect", "--connection", "adapter"]);
    scoped(&["target", "attach"]);
    let fresh = scoped(&["log"]);
    assert_eq!(fresh["capture"]["status"], "active");
    assert_ne!(fresh["capture"]["captureId"], empty["capture"]["captureId"]);
    assert_ne!(fresh["connectionGeneration"], empty["connectionGeneration"]);
    assert_eq!(fresh["nextCursor"], 0);
    assert_eq!(fresh["messages"], serde_json::json!([]));
    run(&["service", "stop"]);
    wait_until_removed(&state_file);
    cleanup_persistent_state(&state_file);
    fs::remove_dir_all(&root).unwrap();
    cleanup.disarm();
}

#[test]
fn cli_printed_nested_selectors_round_trip_across_operations_and_reconnect() {
    let root = std::env::current_dir().unwrap().join("target")
        .join(format!("selector-roundtrip-{}", unique_suffix()));
    fs::create_dir_all(&root).unwrap();
    let state_file = root.join("service.json");
    let cli = PathBuf::from(env!("CARGO_BIN_EXE_jsdbg"));
    let service = PathBuf::from(env!("CARGO_BIN_EXE_jsdbg-service"));
    let cleanup = ServiceCleanup::new(cli.clone(), service.clone(), state_file.clone());
    let fixture = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("tests").join("fixtures").join("selector_browser.mjs");
    let run = |args: &[&str]| run_json_in(&cli, &service, &state_file, &root, args);
    run(&["context", "create", ":selectors", "--set"]);
    for connection in ["browser", "second"] {
        let connected = run(&[
            "connection", "add", "--stdio", "--topology", "browser",
            "--connection", connection, "--connect", "--", "node", fixture.to_str().unwrap(),
        ]);
        let connection = connected["connections"].as_array().unwrap().iter()
            .find(|item| item["id"] == connection).unwrap();
        assert_eq!(connection["targets"].as_array().unwrap().len(), 3, "{connected}");
    }
    let (status, stdout, stderr) = run_human_in(
        &cli, &service, &state_file, &root, &["target", "list"],
    );
    assert_success(&["target", "list"], status, &stdout, &stderr);
    let printed = String::from_utf8(stdout).unwrap();
    let selector = printed.lines()
        .filter_map(|line| line.split_once("  ["))
        .filter_map(|(identity, _)| identity.split_whitespace().last())
        .find(|word| word.starts_with("browser/browser/renderer/target/frame@"))
        .unwrap_or_else(|| panic!("listing should print a copyable nested target identity: {printed}"));
    assert_eq!(selector, "browser/browser/renderer/target/frame@1");
    let target_id = "browser/renderer/target/frame";
    for connection_scope in [false, true] {
        for selector in [selector, selector.strip_suffix("@1").unwrap()] {
            let scope = if connection_scope {
                vec!["--target", selector, "--connection", "browser"]
            } else {
                vec!["--target", selector]
            };
            let command = |args: &[&str]| {
                let mut args = args.to_vec();
                args.extend_from_slice(&scope);
                run(&args)
            };
            let listed = command(&["target", "list"]);
            assert_eq!(listed["targets"].as_array().unwrap().len(), 1, "{listed}");
            assert_eq!(listed["targets"][0]["targetId"], target_id, "{listed}");
            let attached = command(&["target", "attach", "--force"]);
            assert_eq!(attached["target"]["targetId"], target_id, "{attached}");
            let shown = command(&["target", "show"]);
            assert_eq!(shown["target"]["targetId"], target_id, "{shown}");
            let evaluated = command(&["target", "eval", "identity"]);
            assert!(evaluated["preview"]["preview"].as_str().unwrap().contains(target_id), "{evaluated}");
            let raw = command(&["target", "cdp", "Runtime.evaluate", "--params", r#"{"expression":"identity"}"#]);
            assert_eq!(raw["result"]["value"], target_id, "{raw}");
            let logs = command(&["log", "--after", "0"]);
            assert!(logs.to_string().contains(target_id), "{logs}");
        }
    }
    for selector in ["Duplicate title", "renderer/target/frame"] {
        let (status, _, stderr) = run_in(
            &cli, &service, &state_file, &root,
            &["target", "show", "--target", selector],
        );
        assert!(!status.success());
        assert!(String::from_utf8_lossy(&stderr).contains("ambiguous"));
    }
    run(&["connection", "disconnect", "--connection", "browser"]);
    run(&["connection", "connect", "--connection", "browser"]);
    for operation in [
        vec!["target", "list"],
        vec!["target", "show"],
        vec!["target", "attach"],
        vec!["target", "eval", "identity"],
        vec!["log"],
        vec!["target", "cdp", "Runtime.evaluate", "--params", r#"{"expression":"identity"}"#],
    ] {
        for explicit_connection in [false, true] {
            for (selector, expected_error) in [
                (selector, "stale connection generation"),
                ("browser/undiscovered/frame@2", "discovery may be incomplete"),
            ] {
                let mut args = operation.clone();
                args.extend(["--target", selector]);
                if explicit_connection {
                    args.extend(["--connection", "browser"]);
                }
                let (status, _, stderr) = run_in(&cli, &service, &state_file, &root, &args);
                assert!(!status.success(), "unresolved selector unexpectedly accepted: {args:?}");
                let error = String::from_utf8_lossy(&stderr);
                assert!(error.contains(expected_error), "{args:?}: {error}");
            }
        }
    }
    let fresh = selector.replace("@1", "@2");
    run(&["target", "attach", "--target", &fresh]);
    for selector in [fresh.as_str(), "browser/browser/renderer/target/frame"] {
        let shown = run(&["target", "show", "--target", selector]);
        assert_eq!(shown["target"]["targetId"], target_id, "{shown}");
    }
    run(&["service", "stop"]);
    wait_until_removed(&state_file);
    cleanup_persistent_state(&state_file);
    let _ = fs::remove_dir_all(root);
    cleanup.disarm();
}

#[test]
fn target_relay_forwards_cdp_and_enforces_exclusive_context_ownership() {
    let root = std::env::temp_dir().join(format!(
        "jsdbg-target-relay-{}-{}",
        std::process::id(),
        unique_suffix()
    ));
    fs::create_dir_all(&root).unwrap();
    let state_file = root.join("service.json");
    let cli = PathBuf::from(env!("CARGO_BIN_EXE_jsdbg"));
    let service = PathBuf::from(env!("CARGO_BIN_EXE_jsdbg-service"));
    let cleanup = ServiceCleanup::new(cli.clone(), service.clone(), state_file.clone());

    run_json_in(
        &cli,
        &service,
        &state_file,
        &root,
        &["context", "create", ":target-relay", "TargetRelay", "--set"],
    );
    run_json_in(
        &cli,
        &service,
        &state_file,
        &root,
        &[
            "connection",
            "add",
            "--stdio",
            "--connection",
            "adapter",
            "--connect",
            "--",
            "node",
            "--input-type=module",
            "--eval",
            FAKE_CDP_TARGET_SCRIPT,
        ],
    );

    let mut relay = spawn_stdio(
        &cli,
        &service,
        &state_file,
        &root,
        &[
            "target",
            "relay",
            "--stdio",
            "--context",
            ":target-relay",
            "--connection",
            "adapter",
        ],
    );

    relay.send(&serde_json::json!({
        "id": 1,
        "method": "Runtime.evaluate",
        "params": { "expression": "6 * 7" },
    }));

    let mut saw_response = false;
    let mut saw_event = false;
    let deadline = Instant::now() + Duration::from_secs(10);
    while (!saw_response || !saw_event) && Instant::now() < deadline {
        let message = relay.recv_line(Duration::from_secs(10));
        if message.get("id").and_then(Value::as_i64) == Some(1) {
            assert_eq!(message["result"]["result"]["value"], 42);
            saw_response = true;
        } else if message["method"] == "Runtime.consoleAPICalled" {
            assert_eq!(
                message["params"]["args"][0]["value"],
                "hello-from-fake-target"
            );
            saw_event = true;
        }
    }
    assert!(
        saw_response,
        "relay must forward the Runtime.evaluate response"
    );
    assert!(saw_event, "relay must mirror the raw console event");

    // While the relay owns the context exclusively, an ordinary local command against the
    // same already-attached target must fail clearly rather than silently racing the relay.
    let (status, _stdout, stderr) = run_in(
        &cli,
        &service,
        &state_file,
        &root,
        &[
            "target",
            "cdp",
            "Runtime.evaluate",
            "--params",
            r#"{"expression":"1 + 1"}"#,
            "--context",
            ":target-relay",
            "--connection",
            "adapter",
        ],
    );
    assert!(!status.success(), "raw cdp must fail while relayed");
    assert!(
        String::from_utf8_lossy(&stderr).contains("exclusively owned by an active relay"),
        "stderr: {}",
        String::from_utf8_lossy(&stderr)
    );

    // Ending the relay client (closing its stdin, which closes the loopback WebSocket) must
    // restore ordinary local access without restarting the underlying stdio connection or
    // disturbing the attachment the relay itself created.
    relay.shutdown();

    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        let (status, stdout, stderr) = run_in(
            &cli,
            &service,
            &state_file,
            &root,
            &[
                "target",
                "cdp",
                "Runtime.evaluate",
                "--params",
                r#"{"expression":"1 + 1"}"#,
                "--context",
                ":target-relay",
                "--connection",
                "adapter",
            ],
        );
        if status.success() {
            let value: Value = serde_json::from_slice(&stdout).unwrap();
            assert_eq!(value["result"]["value"], 2);
            break;
        }
        assert!(
            Instant::now() < deadline,
            "relay never released the context; stderr: {}",
            String::from_utf8_lossy(&stderr)
        );
        thread::sleep(Duration::from_millis(100));
    }

    run_json_in(
        &cli,
        &service,
        &state_file,
        &root,
        &["connection", "disconnect", "--connection", "adapter"],
    );
    run_json_in(&cli, &service, &state_file, &root, &["service", "stop"]);
    wait_until_removed(&state_file);
    cleanup_persistent_state(&state_file);
    let _ = fs::remove_dir_all(root);
    cleanup.disarm();
}

#[test]
fn context_relay_exposes_virtual_browser_root_and_enforces_exclusivity() {
    let root = std::env::temp_dir().join(format!(
        "jsdbg-context-relay-{}-{}",
        std::process::id(),
        unique_suffix()
    ));
    fs::create_dir_all(&root).unwrap();
    let state_file = root.join("service.json");
    let cli = PathBuf::from(env!("CARGO_BIN_EXE_jsdbg"));
    let service = PathBuf::from(env!("CARGO_BIN_EXE_jsdbg-service"));
    let cleanup = ServiceCleanup::new(cli.clone(), service.clone(), state_file.clone());

    run_json_in(
        &cli,
        &service,
        &state_file,
        &root,
        &[
            "context",
            "create",
            ":context-relay",
            "ContextRelay",
            "--set",
        ],
    );
    let connected = run_json_in(
        &cli,
        &service,
        &state_file,
        &root,
        &[
            "connection",
            "add",
            "--stdio",
            "--connection",
            "adapter",
            "--connect",
            "--",
            "node",
            "--input-type=module",
            "--eval",
            FAKE_CDP_TARGET_SCRIPT,
        ],
    );
    let canonical_target_id = connected["connections"][0]["targets"][0]["targetId"]
        .as_str()
        .unwrap()
        .to_owned();

    let mut relay = spawn_stdio(
        &cli,
        &service,
        &state_file,
        &root,
        &["context", "relay", "--stdio", "--context", ":context-relay"],
    );

    relay.send(&serde_json::json!({ "id": 1, "method": "Browser.getVersion" }));
    let version = relay.recv_line(Duration::from_secs(10));
    assert_eq!(version["id"], 1);
    assert!(version["result"]["protocolVersion"].is_string());

    relay.send(&serde_json::json!({ "id": 2, "method": "Target.getTargets" }));
    let targets = relay.recv_line(Duration::from_secs(10));
    assert_eq!(targets["id"], 2);
    let target_infos = targets["result"]["targetInfos"].as_array().unwrap();
    assert_eq!(target_infos.len(), 1);
    assert_eq!(target_infos[0]["targetId"], canonical_target_id);

    relay.send(&serde_json::json!({
        "id": 3,
        "method": "Target.attachToTarget",
        "params": { "targetId": canonical_target_id, "flatten": true },
    }));

    // Explicit attachment returns its session directly. `Target.attachedToTarget` is reserved for
    // auto-attach discovery; emitting it here would make browser clients register the page twice.
    let attached = relay.recv_line(Duration::from_secs(10));
    assert_eq!(attached["id"], 3);
    let session_id = attached["result"]["sessionId"]
        .as_str()
        .expect("Target.attachToTarget must return a sessionId")
        .to_owned();

    // Session-scoped messages carry the relay's own sessionId and forward opaquely to the
    // attached target, with the external request id preserved on the response. The response
    // and the raw event this evaluation triggers travel independent async paths (an actor
    // round trip vs. a direct broadcast forward), so their relative arrival order is not
    // guaranteed - exactly like real CDP, where clients correlate responses by id rather than
    // by position relative to unrelated events.
    relay.send(&serde_json::json!({
        "sessionId": session_id,
        "id": 4,
        "method": "Runtime.evaluate",
        "params": { "expression": "2 + 2" },
    }));
    let evaluated = loop {
        let message = relay.recv_line(Duration::from_secs(10));
        if message["id"] == 4 {
            break message;
        }
    };
    assert_eq!(
        evaluated["result"]["result"]["value"], 4,
        "unexpected evaluate response: {evaluated}"
    );
    assert_eq!(evaluated["sessionId"], session_id);
    assert_eq!(evaluated["id"], 4);

    // Exclusive relay ownership applies context-wide, even to a target the relay has not
    // itself explicitly attached through the ordinary local attach path.
    let (status, _stdout, stderr) = run_in(
        &cli,
        &service,
        &state_file,
        &root,
        &[
            "target",
            "cdp",
            "Runtime.evaluate",
            "--params",
            r#"{"expression":"1 + 1"}"#,
            "--context",
            ":context-relay",
            "--connection",
            "adapter",
        ],
    );
    assert!(!status.success(), "raw cdp must fail while context-relayed");
    assert!(
        String::from_utf8_lossy(&stderr).contains("exclusively owned by an active relay"),
        "stderr: {}",
        String::from_utf8_lossy(&stderr)
    );

    relay.shutdown();

    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        let (status, stdout, stderr) = run_in(
            &cli,
            &service,
            &state_file,
            &root,
            &[
                "target",
                "cdp",
                "Runtime.evaluate",
                "--params",
                r#"{"expression":"1 + 1"}"#,
                "--context",
                ":context-relay",
                "--connection",
                "adapter",
            ],
        );
        if status.success() {
            let value: Value = serde_json::from_slice(&stdout).unwrap();
            assert_eq!(value["result"]["value"], 2);
            break;
        }
        assert!(
            Instant::now() < deadline,
            "context relay never released the context; stderr: {}",
            String::from_utf8_lossy(&stderr)
        );
        thread::sleep(Duration::from_millis(100));
    }

    run_json_in(
        &cli,
        &service,
        &state_file,
        &root,
        &["connection", "disconnect", "--connection", "adapter"],
    );
    run_json_in(&cli, &service, &state_file, &root, &["service", "stop"]);
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
        &["context", "create", "--context", ":shop", "Shop", "--set"],
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

    let current_view = run_human_in(
        &cli,
        &service,
        &state_file,
        &std::env::current_dir().unwrap(),
        &["daemon", "view"],
    );
    assert_success(
        &["daemon", "view"],
        current_view.0,
        &current_view.1,
        &current_view.2,
    );
    let current_view = String::from_utf8(current_view.1).unwrap();
    assert!(current_view.contains("jsdbg daemon view — context shop"));
    assert!(current_view.contains("Connection browser disconnected"));
    assert!(current_view.contains("Connection server disconnected"));

    let all_view = run_human_in(
        &cli,
        &service,
        &state_file,
        &std::env::current_dir().unwrap(),
        &["daemon", "view", "--all-contexts"],
    );
    assert_success(
        &["daemon", "view", "--all-contexts"],
        all_view.0,
        &all_view.1,
        &all_view.2,
    );
    assert!(
        String::from_utf8(all_view.1)
            .unwrap()
            .contains("jsdbg daemon view — all contexts")
    );

    let conflicting_scope = run_human_in(
        &cli,
        &service,
        &state_file,
        &std::env::current_dir().unwrap(),
        &["daemon", "view", "--context", ":shop", "--all-contexts"],
    );
    assert!(!conflicting_scope.0.success());
    assert!(
        String::from_utf8_lossy(&conflicting_scope.2)
            .contains("--context and --all-contexts are mutually exclusive"),
        "unexpected stderr: {}",
        String::from_utf8_lossy(&conflicting_scope.2)
    );

    let stopped = run_json(&cli, &service, &state_file, &["service", "stop"]);
    assert_eq!(stopped, Value::Bool(true));
    wait_until_removed(&state_file);
    cleanup_persistent_state(&state_file);
    cleanup.disarm();
}

#[test]
fn cli_persists_ordered_source_formatting_rules() {
    let state_file = std::env::temp_dir().join(format!(
        "jsdbg-source-formatting-{}-{}.json",
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
        &["context", "create", ":formatting", "Formatting", "--set"],
    );

    let mut transcript = String::new();
    let commands: &[(&[&str], &str)] = &[
        (
            &["source", "formatting", "get"],
            "jsdbg source formatting get",
        ),
        (
            &["source", "formatting", "set", "auto"],
            "jsdbg source formatting set auto",
        ),
        (
            &[
                "source",
                "formatting",
                "rule",
                "add",
                "--mode",
                "off",
                "--url",
                "**/vendor/**",
            ],
            "jsdbg source formatting rule add --mode off --url '**/vendor/**'",
        ),
        (
            &[
                "source",
                "formatting",
                "rule",
                "add",
                "--mode",
                "on",
                "--target",
                "page-*",
                "--url",
                "**/*.min.js",
            ],
            "jsdbg source formatting rule add --mode on --target 'page-*' --url '**/*.min.js'",
        ),
        (
            &["source", "formatting", "rule", "remove", "fmt-1"],
            "jsdbg source formatting rule remove fmt-1",
        ),
    ];
    for (arguments, rendered) in commands {
        transcript.push_str("$ ");
        transcript.push_str(rendered);
        transcript.push('\n');
        transcript.push_str(&run_human(&cli, &service, &state_file, arguments));
    }
    assert_eq!(
        transcript,
        include_str!("transcripts/source-formatting.txt")
    );

    run_json(&cli, &service, &state_file, &["service", "stop"]);
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
    assert_eq!(configured["status"], "disabled");
    assert_eq!(configured["condition"], "validationValue > 0");
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
    assert_eq!(repeated, configured);
    let repeated_context = run_json(
        &cli,
        &service,
        &state_file,
        &["context", "show", "--context", "managed"],
    );
    assert_eq!(repeated_context["revision"], 2);

    let matches = run_json(
        &cli,
        &service,
        &state_file,
        &["source", "grep", "validationValue", "--context", "managed"],
    );
    assert_eq!(matches["matches"].as_array().unwrap().len(), 3);
    assert_eq!(matches["omittedMatches"], 0);
    assert_eq!(matches["searchedSources"], 1);
    assert_eq!(matches["searchedContents"], 1);
    assert_eq!(matches["matches"][0]["kind"], "intent");
    assert_eq!(matches["matches"][0]["provenance"], "local file");
    assert_eq!(matches["matches"][0]["matchLength"], 15);
    assert_eq!(
        matches["matches"][0]["contentHash"].as_str().unwrap().len(),
        64
    );
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
    let transcript = run_human(
        &cli,
        &service,
        &state_file,
        &[
            "source",
            "grep",
            "VALIDATIONVALUE",
            "--ignore-case",
            "--path",
            source_file.file_name().unwrap().to_str().unwrap(),
            "--max-results",
            "1",
            "--context-lines",
            "1",
            "--timeout-ms",
            "5000",
            "--context",
            "managed",
        ],
    );
    assert_eq!(
        transcript,
        format!(
            "{source}:1:14:export const validationValue = 42;\n\
             {source}-2-console.log(validationValue);\n\
             --\n\
             ... 2 additional matches omitted; increase --max-results\n\
             1 source(s) searched, 0 skipped\n"
        )
    );
    let source_graph = run_json(
        &cli,
        &service,
        &state_file,
        &["source", "map", "show", "--context", "managed"],
    );
    assert_eq!(source_graph["roots"], serde_json::json!([]));
    assert_eq!(source_graph["nodes"], serde_json::json!([]));
    assert_eq!(source_graph["edges"], serde_json::json!([]));

    let configured_revision = repeated_context["revision"].as_u64().unwrap().to_string();
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
fn cli_resolves_canonical_target_and_queries_capture_offline() {
    let Ok(mut node) = Command::new("node")
        .args([
            "-e",
            "const vm=require('node:vm');vm.runInThisContext(\"globalThis.lateSource=function(){return 'lateSourceNeedle';};\",{filename:'https://fixtures.test/late.min.js'});vm.runInThisContext(\"globalThis.formatFirst=function(){return 'formattedFirstNeedle';};\",{filename:'https://fixtures.test/format-first.min.js'});const inspector=require('node:inspector');inspector.open(0,'127.0.0.1',false);console.log(inspector.url());setInterval(()=>{},1000)",
        ])
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
    else {
        return;
    };
    let endpoint = {
        let mut line = String::new();
        BufReader::new(node.stdout.take().unwrap())
            .read_line(&mut line)
            .unwrap();
        line.trim().to_owned()
    };
    assert!(endpoint.starts_with("ws://"), "{endpoint}");
    let _node = ChildCleanup(node);
    let Ok(mut second_node) = Command::new("node")
        .args([
            "-e",
            "const inspector=require('node:inspector');inspector.open(0,'127.0.0.1',false);console.log(inspector.url());setInterval(()=>{},1000)",
        ])
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
    else {
        return;
    };
    let second_endpoint = {
        let mut line = String::new();
        BufReader::new(second_node.stdout.take().unwrap())
            .read_line(&mut line)
            .unwrap();
        line.trim().to_owned()
    };
    assert!(second_endpoint.starts_with("ws://"), "{second_endpoint}");
    let _second_node = ChildCleanup(second_node);

    let suffix = unique_suffix();
    let context_id = format!("identity-e2e-{suffix}");
    let context = format!(":{context_id}");
    let state_file = std::env::current_dir()
        .unwrap()
        .join("target")
        .join(format!("jsdbg-identity-e2e-{suffix}.json"));
    let cli = PathBuf::from(env!("CARGO_BIN_EXE_jsdbg"));
    let service = PathBuf::from(env!("CARGO_BIN_EXE_jsdbg-service"));
    let cleanup = ServiceCleanup::new(cli.clone(), service.clone(), state_file.clone());

    run_json(
        &cli,
        &service,
        &state_file,
        &["context", "create", "--context", &context],
    );
    let connected = run_json(
        &cli,
        &service,
        &state_file,
        &[
            "connection",
            "add",
            "--node-inspector",
            &endpoint,
            "--connection",
            "runtime-a",
            "--context",
            &context,
            "--connect",
        ],
    );
    assert_eq!(connected["connections"][0]["generation"], 1);
    assert_eq!(
        connected["connections"][0]["targets"][0]["targetId"],
        "$node-root:runtime-a"
    );
    run_json(
        &cli,
        &service,
        &state_file,
        &[
            "target",
            "attach",
            "--context",
            &context,
            "--target",
            "$node-root:runtime-a",
        ],
    );
    let connected = run_json(
        &cli,
        &service,
        &state_file,
        &[
            "connection",
            "add",
            "--node-inspector",
            &second_endpoint,
            "--connection",
            "runtime-b",
            "--context",
            &context,
            "--connect",
        ],
    );
    let targets = connected["targetForest"].as_array().unwrap();
    assert!(targets.iter().any(|target| {
        target["connectionId"] == "runtime-a"
            && target["target"]["targetId"] == "$node-root:runtime-a"
    }));
    assert!(targets.iter().any(|target| {
        target["connectionId"] == "runtime-b"
            && target["target"]["targetId"] == "$node-root:runtime-b"
    }));
    run_json(
        &cli,
        &service,
        &state_file,
        &[
            "target",
            "attach",
            "--context",
            &context,
            "--target",
            "$node-root:runtime-b",
        ],
    );
    let ambiguous = run_in(
        &cli,
        &service,
        &state_file,
        &std::env::current_dir().unwrap(),
        &["target", "show", "--context", &context, "--target", "node"],
    );
    assert!(!ambiguous.0.success());
    let ambiguity = String::from_utf8_lossy(&ambiguous.2);
    assert!(
        ambiguity.contains("runtime-a/$node-root:runtime-a@1"),
        "{ambiguity}"
    );
    assert!(
        ambiguity.contains("runtime-b/$node-root:runtime-b@1"),
        "{ambiguity}"
    );

    let sources = run_json(
        &cli, &service, &state_file,
        &["source", "list", "--path", "late.min.js", "--context", &context],
    );
    assert!(sources.as_array().is_some_and(|sources| !sources.is_empty()), "{sources}");
    let searched = run_json(
        &cli, &service, &state_file,
        &["source", "grep", "lateSourceNeedle", "--path", "late.min.js", "--context", &context],
    );
    assert!(searched["searchedSources"].as_u64().unwrap() > 0, "{searched}");
    assert_eq!(searched["skippedSources"], 0);
    assert!(!searched["matches"].as_array().unwrap().is_empty());

    run_json(
        &cli, &service, &state_file,
        &["source", "formatting", "set", "on", "--context", &context],
    );
    let formatted = run_json(
        &cli, &service, &state_file,
        &["source", "show", "https://fixtures.test/format-first.min.js", "--view", "formatted", "--context", &context],
    );
    assert_eq!(formatted["path"], "https://fixtures.test/format-first.min.js?formatted");
    assert!(formatted["content"].as_str().unwrap().contains("formattedFirstNeedle"));
    assert!(formatted["totalLines"].as_u64().unwrap() > 1);
    let searched = run_json(
        &cli, &service, &state_file,
        &["source", "grep", "formattedFirstNeedle", "--path", "format-first.min.js", "--view", "formatted", "--context", &context],
    );
    assert!(searched["matches"].as_array().unwrap().iter().any(|matched| {
        matched["path"] == "https://fixtures.test/format-first.min.js?formatted"
    }), "{searched}");

    let evaluated = run_json(
        &cli,
        &service,
        &state_file,
        &[
            "target",
            "eval",
            "6 * 7",
            "--context",
            &context,
            "--target",
            "$node-root:runtime-a",
        ],
    );
    assert_eq!(evaluated["preview"]["preview"], "42");
    for selector in ["runtime-a/$node-root:runtime-a", "runtime-a/$node-root:runtime-a@1"] {
        run_json(
            &cli, &service, &state_file,
            &["target", "show", "--context", &context, "--target", selector],
        );
        let attach = run_in(
            &cli, &service, &state_file, &std::env::current_dir().unwrap(),
            &["target", "attach", "--context", &context, "--target", selector],
        );
        assert!(!attach.0.success());
        assert!(String::from_utf8_lossy(&attach.2).contains("target ownership conflict"));
        let result = run_json(
            &cli, &service, &state_file,
            &["target", "eval", "6 * 7", "--context", &context, "--target", selector],
        );
        assert_eq!(result["preview"]["preview"], "42");
    }
    for expression in ["'x'.repeat(4096)", "JSON.stringify({text:'x'.repeat(4096)})"] {
        let full = run_json(
            &cli, &service, &state_file,
            &["target", "eval", expression, "--full", "--context", &context, "--target", "runtime-a/$node-root:runtime-a@1"],
        );
        let expected = if expression.starts_with("JSON") {
            serde_json::json!({"text": "x".repeat(4096)}).to_string()
        } else {
            "x".repeat(4096)
        };
        assert_eq!(full["preview"]["preview"], expected);
        assert_eq!(full["preview"]["truncated"], false);
        assert!(!contains_reference(&full));
        let bounded = run_json(
            &cli, &service, &state_file,
            &["target", "eval", expression, "--max-preview-length", "200", "--context", &context, "--target", "runtime-a/$node-root:runtime-a"],
        );
        assert_eq!(bounded["preview"]["preview"], &expected[..200]);
        assert_eq!(bounded["preview"]["truncated"], true);
    }
    let truncated = run_human_in(
        &cli, &service, &state_file, &std::env::current_dir().unwrap(),
        &["target", "eval", "'x'.repeat(4096)", "--context", &context, "--target", "runtime-a/$node-root:runtime-a"],
    );
    assert!(truncated.0.success());
    assert!(String::from_utf8_lossy(&truncated.1).contains("--full"));
    run_json(
        &cli,
        &service,
        &state_file,
        &[
            "profile",
            "start",
            "--context",
            &context,
            "--target",
            "$node-root:runtime-a",
        ],
    );
    run_json(
        &cli,
        &service,
        &state_file,
        &[
            "target",
            "eval",
            "(()=>{const end=Date.now()+50;let value=0;while(Date.now()<end){value++}return value})()",
            "--context",
            &context,
            "--target",
            "$node-root:runtime-a",
        ],
    );
    run_json(
        &cli,
        &service,
        &state_file,
        &[
            "profile",
            "stop",
            "--id",
            "offline-profile",
            "--context",
            &context,
            "--target",
            "$node-root:runtime-a",
        ],
    );
    let coverage_started = run_human_in(
        &cli,
        &service,
        &state_file,
        &std::env::current_dir().unwrap(),
        &[
            "coverage",
            "start",
            "--context",
            &context,
            "--target",
            "$node-root:runtime-a",
        ],
    );
    assert_success(
        &["coverage", "start"],
        coverage_started.0,
        &coverage_started.1,
        &coverage_started.2,
    );
    let duplicate = run_in(
        &cli,
        &service,
        &state_file,
        &std::env::current_dir().unwrap(),
        &[
            "coverage",
            "capture",
            "--id",
            "offline-profile",
            "--context",
            &context,
            "--target",
            "$node-root:runtime-a",
        ],
    );
    assert!(!duplicate.0.success());
    assert!(
        String::from_utf8_lossy(&duplicate.2).contains("capture 'offline-profile' already exists")
    );
    let coverage_stopped = run_human_in(
        &cli,
        &service,
        &state_file,
        &std::env::current_dir().unwrap(),
        &[
            "coverage",
            "stop",
            "--context",
            &context,
            "--target",
            "$node-root:runtime-a",
        ],
    );
    assert_success(
        &["coverage", "stop"],
        coverage_stopped.0,
        &coverage_stopped.1,
        &coverage_stopped.2,
    );
    run_json(
        &cli,
        &service,
        &state_file,
        &[
            "heap",
            "capture",
            "--id",
            "offline-heap",
            "--context",
            &context,
            "--target",
            "$node-root:runtime-b",
        ],
    );
    let capture_directory = persistent_state_file(&state_file).with_extension("captures");
    let heap_files = heap_capture_files(&capture_directory);
    assert_eq!(heap_files.len(), 1, "{heap_files:?}");
    let payload_files = capture_payload_files(&capture_directory);
    assert_eq!(payload_files.len(), 3, "{payload_files:?}");
    assert!(payload_files.iter().any(|path| {
        path.file_name()
            .unwrap()
            .to_string_lossy()
            .ends_with(".cpuprofile.json")
    }));
    assert!(payload_files.iter().any(|path| {
        path.file_name()
            .unwrap()
            .to_string_lossy()
            .ends_with(".coverage.json")
    }));
    let persisted: serde_json::Value =
        serde_json::from_slice(&fs::read(persistent_state_file(&state_file)).unwrap()).unwrap();
    assert_eq!(persisted["schemaVersion"], 5);
    assert!(
        persisted["captures"]
            .as_array()
            .unwrap()
            .iter()
            .all(|capture| {
                capture["payload"]["path"].is_string()
                    && capture["payload"]["sha256"].is_string()
                    && capture["payload"]["byteLen"].is_number()
                    && capture["payload"].get("kind").is_none()
            })
    );
    run_json(
        &cli,
        &service,
        &state_file,
        &[
            "connection",
            "disconnect",
            "--context",
            &context,
            "--connection",
            "runtime-a",
        ],
    );
    run_json(
        &cli,
        &service,
        &state_file,
        &[
            "connection",
            "disconnect",
            "--context",
            &context,
            "--connection",
            "runtime-b",
        ],
    );
    run_json(&cli, &service, &state_file, &["service", "stop"]);
    wait_until_removed(&state_file);

    let captures = run_json(
        &cli,
        &service,
        &state_file,
        &["capture", "list", "--context", &context],
    );
    let profile = captures
        .as_array()
        .unwrap()
        .iter()
        .find(|capture| capture["name"] == "offline-profile")
        .unwrap();
    assert_eq!(profile["kind"], "cpuProfile");
    assert_eq!(profile["targetId"], "$node-root:runtime-a");
    assert_eq!(profile["connectionId"], "runtime-a");
    assert_eq!(profile["connectionGeneration"], 1);
    assert_eq!(profile["contextId"], context_id);
    assert!(
        profile["storageId"]
            .as_str()
            .is_some_and(|storage_id| !storage_id.is_empty())
    );
    let shown = run_json(
        &cli,
        &service,
        &state_file,
        &["capture", "show", "offline-profile", "--context", &context],
    );
    assert_eq!(shown, *profile);
    let offline_profile = run_json(
        &cli,
        &service,
        &state_file,
        &[
            "profile",
            "show",
            "offline-profile",
            "--max-lines",
            "3",
            "--context",
            &context,
        ],
    );
    assert!(
        !offline_profile["functions"].as_array().unwrap().is_empty(),
        "{offline_profile}"
    );
    assert!(offline_profile["analysis"].is_object(), "{offline_profile}");
    let heap = captures
        .as_array()
        .unwrap()
        .iter()
        .find(|capture| capture["name"] == "offline-heap")
        .unwrap();
    assert_eq!(heap["targetId"], "$node-root:runtime-b");
    assert_eq!(heap["connectionId"], "runtime-b");
    run_json(
        &cli,
        &service,
        &state_file,
        &["capture", "delete", "offline-heap", "--context", &context],
    );
    assert!(!heap_files[0].exists());

    let transcript = "\
$ jsdbg target show --context <context> --target node
error: target selector 'node' is ambiguous: runtime-a/<node-a>@1, runtime-b/<node-b>@1
$ jsdbg target eval '6 * 7' --context <context> --target <canonical-id>
42
$ jsdbg profile stop --id offline-profile --context <context> --target <canonical-id>
capture registered context-wide; payload externalized from service state
$ jsdbg coverage capture --id offline-profile --context <context> --target <canonical-id>
error: capture 'offline-profile' already exists in context
$ jsdbg heap capture --id offline-heap --context <context> --target <node-b>
capture reserved before storage
$ jsdbg connection disconnect --context <context> --connection runtime-a
$ jsdbg connection disconnect --context <context> --connection runtime-b
$ jsdbg service stop
$ jsdbg capture show offline-profile --context <context>
kind=cpuProfile owner=runtime-a/<node-a>@1
$ jsdbg profile show offline-profile --context <context>
offline query succeeded
$ jsdbg capture delete offline-heap --context <context>
catalog persisted, then immutable heap storage removed
";
    print!("{transcript}");
    assert_eq!(
        transcript,
        include_str!("transcripts/context-global-identities.txt")
    );

    run_json(&cli, &service, &state_file, &["service", "stop"]);
    wait_until_removed(&state_file);
    cleanup_persistent_state(&state_file);
    let _ = fs::remove_dir_all(persistent_state_file(&state_file).with_extension("captures"));
    cleanup.disarm();
}

#[test]
fn cli_service_connects_to_live_cdp() {
    let Ok(endpoint) = std::env::var("CDP_WS_ENDPOINT") else {
        return;
    };
    let state_file = std::env::current_dir()
        .unwrap()
        .join("target")
        .join(format!(
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
    let resolved_context_id = connected["id"].as_str().unwrap().to_owned();

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
    assert!(bounded_eval["properties"].as_array().unwrap().len() <= 20);
    assert_eq!(bounded_eval["propertiesTruncated"], true);
    assert!(!contains_reference(&bounded_eval));

    let pending_logpoint = run_json(
        &cli,
        &service,
        &state_file,
        &[
            "target",
            "logpoint",
            "view-pending",
            "file:///workspace/not-loaded.ts",
            "1",
            "1",
            "'view'",
            "--context",
            "live-browser",
            "--connection",
            "browser",
            "--target",
            &target_id,
        ],
    );
    assert_eq!(
        pending_logpoint["breakpoints"][0]["status"]["kind"],
        "waitingForScript"
    );
    let daemon_view = run_human_in(
        &cli,
        &service,
        &state_file,
        &std::env::current_dir().unwrap(),
        &["daemon", "view", "--context", "live-browser"],
    );
    assert_success(
        &["daemon", "view", "--context", "live-browser"],
        daemon_view.0,
        &daemon_view.1,
        &daemon_view.2,
    );
    let daemon_view = normalize_daemon_view(
        &String::from_utf8(daemon_view.1).unwrap(),
        &resolved_context_id,
        &target_id,
    );
    let daemon_transcript = format!("$ jsdbg daemon view --context live-browser\n{daemon_view}");
    print!("{daemon_transcript}");
    assert_eq!(
        daemon_transcript,
        include_str!("transcripts/daemon-view.txt")
    );

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
    assert!(
        long_property["value"]["preview"]
            .as_str()
            .unwrap()
            .chars()
            .count()
            <= 120
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
    let collision = run_in(
        &cli,
        &service,
        &state_file,
        &std::env::current_dir().unwrap(),
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
    assert!(!collision.0.success());
    assert!(
        String::from_utf8_lossy(&collision.2).contains("canonical target ID"),
        "{}",
        String::from_utf8_lossy(&collision.2)
    );

    let deleted = run_json(
        &cli,
        &service,
        &state_file,
        &["context", "delete", "--context", "live-browser"],
    );
    assert_eq!(deleted, Value::Bool(true));

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

fn normalize_daemon_view(rendering: &str, context_id: &str, target_id: &str) -> String {
    let mut rendering = rendering.to_owned();
    for context_rendering in [
        escaped_prefix(context_id, context_id.chars().count()),
        escaped_prefix(context_id, 80),
        escaped_prefix(context_id, 60),
    ] {
        rendering = rendering.replace(&context_rendering, "live-browser");
    }
    rendering
        .replace(target_id, "<target-id>")
        .lines()
        .filter_map(|line| {
            if line.trim_start().starts_with("target=") && !line.contains("<target-id>") {
                return None;
            }
            let mut line = line.to_owned();
            if line.trim_start().starts_with("Connection ")
                && let Some(start) = line.find("targets=")
            {
                let value_start = start + "targets=".len();
                let value_end = line[value_start..]
                    .find(char::is_whitespace)
                    .map_or(line.len(), |offset| value_start + offset);
                line.replace_range(value_start..value_end, "<target-count>");
            }
            if line.contains("target=browser/<target-id>")
                && let Some(start) = line.find(" url=")
            {
                line.replace_range(start.., " url=\"<debuggee-url>\"");
            }
            let Some(start) = line.find(" rev=") else {
                return Some(line);
            };
            let value_start = start + " rev=".len();
            let value_end = line[value_start..]
                .find(char::is_whitespace)
                .map_or(line.len(), |offset| value_start + offset);
            Some(format!(
                "{}<revision>{}",
                &line[..value_start],
                &line[value_end..]
            ))
        })
        .collect::<Vec<_>>()
        .join("\n")
        + "\n"
}

fn escaped_prefix(value: &str, max_chars: usize) -> String {
    let mut chars = value.chars();
    let prefix = chars.by_ref().take(max_chars).collect::<String>();
    let truncated = if chars.next().is_some() {
        format!("{prefix}…")
    } else {
        prefix
    };
    truncated.chars().flat_map(char::escape_default).collect()
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

/// A long-lived interactive CLI subprocess (e.g. `target relay --stdio`), for tests that must
/// exchange multiple NDJSON messages with it instead of running it to completion.
struct StdioProcess {
    child: Child,
    stdin: Option<std::process::ChildStdin>,
    lines: mpsc::Receiver<String>,
    stderr: Arc<Mutex<String>>,
}

impl StdioProcess {
    fn send(&mut self, value: &Value) {
        let mut line = serde_json::to_string(value).unwrap();
        line.push('\n');
        self.stdin
            .as_mut()
            .expect("stdio process stdin is still open")
            .write_all(line.as_bytes())
            .unwrap();
    }

    fn recv_line(&mut self, timeout: Duration) -> Value {
        let line = self.lines.recv_timeout(timeout).unwrap_or_else(|_| {
            panic!(
                "timed out waiting for a relay message; stderr so far: {}",
                self.stderr.lock().unwrap()
            )
        });
        serde_json::from_str(&line)
            .unwrap_or_else(|error| panic!("invalid JSON line {line:?}: {error}"))
    }

    /// Closes stdin (which ends the relay's stdio bridge and its loopback WebSocket) and waits
    /// for the process to exit.
    fn shutdown(mut self) {
        drop(self.stdin.take());
        let _ = self.child.wait();
    }
}

fn spawn_stdio(
    cli: &Path,
    service: &Path,
    state_file: &Path,
    cwd: &Path,
    arguments: &[&str],
) -> StdioProcess {
    let mut child = Command::new(cli)
        .current_dir(cwd)
        .args(arguments)
        .env("JSDBG_SERVICE_EXE", service)
        .env("JSDBG_SERVICE_STATE", state_file)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    let stdin = child.stdin.take();
    let stdout = child.stdout.take().unwrap();
    let stderr_pipe = child.stderr.take().unwrap();

    let (sender, receiver) = mpsc::channel();
    thread::spawn(move || {
        let mut reader = BufReader::new(stdout);
        let mut line = String::new();
        loop {
            line.clear();
            match reader.read_line(&mut line) {
                Ok(0) | Err(_) => break,
                Ok(_) if sender.send(line.trim_end().to_owned()).is_err() => break,
                Ok(_) => {}
            }
        }
    });

    let stderr_buffer = Arc::new(Mutex::new(String::new()));
    let stderr_writer = stderr_buffer.clone();
    thread::spawn(move || {
        let mut reader = BufReader::new(stderr_pipe);
        let mut line = String::new();
        loop {
            line.clear();
            match reader.read_line(&mut line) {
                Ok(0) | Err(_) => break,
                Ok(_) => stderr_writer.lock().unwrap().push_str(&line),
            }
        }
    });

    StdioProcess {
        child,
        stdin,
        lines: receiver,
        stderr: stderr_buffer,
    }
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

fn heap_capture_files(root: &Path) -> Vec<PathBuf> {
    capture_payload_files(root)
        .into_iter()
        .filter(|path| {
            path.extension()
                .is_some_and(|extension| extension == "heapsnapshot")
        })
        .collect()
}

fn capture_payload_files(root: &Path) -> Vec<PathBuf> {
    let mut files = Vec::new();
    let mut pending = vec![root.to_owned()];
    while let Some(directory) = pending.pop() {
        let Ok(entries) = fs::read_dir(directory) else {
            continue;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            if path.is_dir() {
                pending.push(path);
            } else if !path
                .file_name()
                .unwrap()
                .to_string_lossy()
                .ends_with(".partial")
            {
                files.push(path);
            }
        }
    }
    files
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

struct ChildCleanup(std::process::Child);

impl Drop for ChildCleanup {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}
