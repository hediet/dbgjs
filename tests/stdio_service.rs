use std::process::Stdio;
use std::time::Duration;

use dbgjs::service_api;
use serde_json::{Value, json};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader, Lines};
use tokio::process::{Child, ChildStdout, Command};
use tokio::time::timeout;

#[tokio::test]
async fn stdio_reflects_every_service_contract_and_exits_on_eof_without_persistent_state() {
    let directory = tempfile::tempdir().unwrap();
    let state_file = directory.path().join("existing-service.json");
    std::fs::write(&state_file, "do not read or overwrite").unwrap();
    let (mut child, mut output) = spawn(directory.path(), &state_file);
    let listing = request(&mut child, &mut output, "hubrpc.directory::list", json!({})).await;
    let items = listing["items"].as_array().unwrap();
    let mut methods = std::collections::BTreeSet::new();
    for definition in service_api::interfaces() {
        assert!(
            items
                .iter()
                .any(|item| item["interfaceId"] == definition.id())
        );
        let reflected = request(
            &mut child,
            &mut output,
            "hubrpc.schemas::get",
            json!({ "interfaceId": definition.id(), "hash": definition.schema_hash() }),
        )
        .await;
        let schema: linkrpc::prelude::LinkRpcInterfaceSchema =
            serde_json::from_value(reflected["schema"].clone()).unwrap();
        assert_eq!(schema, definition.to_schema());
        for method in schema.methods.keys() {
            assert!(
                methods.insert(method.clone()),
                "duplicate daemon method {method}"
            );
        }
    }
    assert_eq!(methods.len(), 85);
    assert!(methods.contains("remove_logpoint"));
    assert!(
        !items
            .iter()
            .any(|item| item["interfaceId"].as_str().unwrap().starts_with("cdp."))
    );

    assert_eq!(
        request(
            &mut child,
            &mut output,
            "dev.dbgjs.context::list_contexts",
            json!({ "cwd": null }),
        )
        .await,
        json!([])
    );
    request(
        &mut child,
        &mut output,
        "dev.dbgjs.context::put_context",
        json!({ "contextId": "isolated", "kind": "named", "displayName": null }),
    )
    .await;
    drop(child.stdin.take());
    let status = timeout(Duration::from_secs(10), child.wait())
        .await
        .unwrap()
        .unwrap();
    assert!(status.success());
    assert_eq!(
        std::fs::read_to_string(&state_file).unwrap(),
        "do not read or overwrite"
    );
    assert_eq!(std::fs::read_dir(directory.path()).unwrap().count(), 1);
}

#[tokio::test]
async fn stdio_returns_a_structured_application_error_response() {
    let directory = tempfile::tempdir().unwrap();
    let (mut child, mut output) = spawn(directory.path(), &directory.path().join("unused.json"));
    let response = request_response(
        &mut child,
        &mut output,
        "dev.dbgjs.capture::list_captures",
        json!({ "contextId": "missing-context" }),
    )
    .await;
    assert_eq!(response, json!({
        "jsonrpc": "2.0",
        "id": 1,
        "error": {
            "code": 1,
            "message": "context 'missing-context' does not exist",
            "data": {
                "type": "ContextNotFound",
                "data": { "context_id": "missing-context" }
            }
        }
    }));
    drop(child.stdin.take());
    assert!(timeout(Duration::from_secs(10), child.wait()).await.unwrap().unwrap().success());
}

#[tokio::test]
async fn stdio_shutdown_exits_even_while_parent_keeps_stdin_open() {
    let directory = tempfile::tempdir().unwrap();
    let (mut child, mut output) = spawn(directory.path(), &directory.path().join("unused.json"));
    assert_eq!(
        request(
            &mut child,
            &mut output,
            "dev.dbgjs.cdp-debugger::shutdown",
            json!({}),
        )
        .await,
        json!(true)
    );
    let status = timeout(Duration::from_secs(10), child.wait())
        .await
        .unwrap()
        .unwrap();
    assert!(status.success());
    assert_eq!(std::fs::read_dir(directory.path()).unwrap().count(), 0);
}

#[tokio::test]
async fn stdio_rejects_shared_daemon_options() {
    for arguments in [
        vec!["--stdio", "--ensure"],
        vec!["--stdio", "--state-file", "must-not-be-created.json"],
        vec!["--stdio", "--stdio"],
    ] {
        let result = Command::new(env!("CARGO_BIN_EXE_dbgjs-service"))
            .args(arguments)
            .output()
            .await
            .unwrap();
        assert!(!result.status.success());
        assert!(result.stdout.is_empty());
        assert!(
            String::from_utf8(result.stderr)
                .unwrap()
                .contains("--stdio")
        );
    }
}

fn spawn(
    directory: &std::path::Path,
    state_file: &std::path::Path,
) -> (Child, Lines<BufReader<ChildStdout>>) {
    let mut child = Command::new(env!("CARGO_BIN_EXE_dbgjs-service"))
        .arg("--stdio")
        .env("DBGJS_SERVICE_STATE", state_file)
        .env("TMPDIR", directory)
        .env("TMP", directory)
        .env("TEMP", directory)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::inherit())
        .kill_on_drop(true)
        .spawn()
        .unwrap();
    let output = BufReader::new(child.stdout.take().unwrap()).lines();
    (child, output)
}

async fn request(
    child: &mut Child,
    output: &mut Lines<BufReader<ChildStdout>>,
    method: &str,
    params: Value,
) -> Value {
    let response = request_response(child, output, method, params).await;
    assert!(response.get("error").is_none(), "{response}");
    response.get("result").expect("RPC result").clone()
}

async fn request_response(
    child: &mut Child,
    output: &mut Lines<BufReader<ChildStdout>>,
    method: &str,
    params: Value,
) -> Value {
    let frame = json!({ "jsonrpc": "2.0", "id": 1, "method": method, "params": params });
    let input = child.stdin.as_mut().unwrap();
    input
        .write_all(format!("{frame}\n").as_bytes())
        .await
        .unwrap();
    input.flush().await.unwrap();
    let line = timeout(Duration::from_secs(10), output.next_line())
        .await
        .unwrap()
        .unwrap()
        .expect("daemon responds");
    let response: Value = serde_json::from_str(&line).unwrap();
    assert_eq!(response["id"], 1);
    response
}
