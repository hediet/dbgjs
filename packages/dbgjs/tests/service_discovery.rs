use std::fs;
use std::path::Path;
use std::process::{Command, Output};
use std::time::Duration;

use dbgjs::api::service_api::service_description;
use dbgjs::connection::transport::local_rpc::read_endpoint;
use serde_json::Value;

fn command(root: &Path, executable: &str, args: &[&str]) -> Output {
    Command::new(executable)
        .args(args)
        .env_remove("LOCALAPPDATA")
        .env_remove("DBGJS_SERVICE_STATE")
        .env_remove("DBGJS_SERVICE_EXE")
        .env("XDG_RUNTIME_DIR", root)
        .output()
        .unwrap()
}

#[test]
fn listing_is_read_only_and_custom_overrides_are_exact() {
    let directory = tempfile::tempdir().unwrap();
    let root = directory.path();
    let service = env!("CARGO_BIN_EXE_dbgjs-service");
    let cli = env!("CARGO_BIN_EXE_dbgjs");
    let listed = command(root, cli, &["--json", "service", "list"]);
    assert!(listed.status.success(), "{}", String::from_utf8_lossy(&listed.stderr));
    assert_eq!(
        serde_json::from_slice::<Value>(&listed.stdout).unwrap(),
        serde_json::json!([])
    );
    assert!(!root.join("dbgjs").exists());
    let state = root.join("custom.json");
    let mismatch = command(
        root,
        service,
        &[
            "--ensure",
            "--state-file",
            state.to_str().unwrap(),
            "--expected-contract",
            "wrong",
        ],
    );
    assert!(!mismatch.status.success());
    assert!(String::from_utf8_lossy(&mismatch.stderr).contains("client expects wrong"));
    assert!(!state.exists());
}

#[test]
fn default_namespace_agrees_with_generated_contract_and_keeps_state_location() {
    let directory = tempfile::tempdir().unwrap();
    let root = directory.path();
    let service = env!("CARGO_BIN_EXE_dbgjs-service");
    let cli = env!("CARGO_BIN_EXE_dbgjs");
    let description = service_description();
    let fingerprint = command(root, service, &["--contract-fingerprint"]);
    assert!(fingerprint.status.success());
    assert_eq!(
        String::from_utf8(fingerprint.stdout).unwrap().trim(),
        description.contract_fingerprint
    );
    let generated = include_str!("../../vscode-extension/src/generated/interfaces.ts");
    assert!(generated.contains(&format!(
        "export const serviceContractFingerprint = \"{}\";",
        description.contract_fingerprint
    )));
    let mut process = Command::new(service)
        .env_remove("LOCALAPPDATA")
        .env_remove("DBGJS_SERVICE_STATE")
        .env("XDG_RUNTIME_DIR", root)
        .spawn()
        .unwrap();
    let state = root
        .join("dbgjs")
        .join("services")
        .join(&description.contract_fingerprint)
        .join("service.json");
    for _ in 0..500 {
        if state.exists() {
            break;
        }
        if let Some(status) = process.try_wait().unwrap() {
            panic!("service exited: {status}");
        }
        std::thread::sleep(Duration::from_millis(10));
    }
    if !state.exists() {
        process.kill().unwrap();
        process.wait().unwrap();
        panic!("service did not publish its endpoint");
    }
    let result = || {
        let listed = command(root, cli, &["--json", "service", "list"]);
        assert!(listed.status.success(), "{}", String::from_utf8_lossy(&listed.stderr));
        let services: Value = serde_json::from_slice(&listed.stdout).unwrap();
        assert_eq!(services.as_array().unwrap().len(), 1);
        assert_eq!(services[0]["status"], "running");
        assert_eq!(services[0]["compatible"], true);
        let endpoint = read_endpoint(&state).unwrap();
        assert!(!String::from_utf8_lossy(&listed.stdout).contains(&endpoint.token));
        let ensure = command(root, service, &["--ensure", "--refuse-incompatible"]);
        assert!(ensure.status.success(), "{:?}", ensure);
        assert_eq!(read_endpoint(&state).unwrap().process_id, process.id());
        let context = command(
            root,
            cli,
            &[
                "--json",
                "context",
                "create",
                ":namespace-test",
                "Namespace",
            ],
        );
        assert!(context.status.success(), "{:?}", context);
        let stop = command(root, cli, &["--json", "service", "stop"]);
        assert!(stop.status.success(), "{:?}", stop);
    };
    // Always reap the child, including when an assertion fails.
    let result = std::panic::catch_unwind(result);
    if result.is_ok() {
        for _ in 0..500 {
            if process.try_wait().unwrap().is_some() {
                break;
            }
            std::thread::sleep(Duration::from_millis(10));
        }
    }
    if process.try_wait().unwrap().is_none() {
        process.kill().unwrap();
    }
    process.wait().unwrap();
    if let Err(error) = result {
        std::panic::resume_unwind(error);
    }
    assert!(root.join("dbgjs").join("service.contexts.json").exists());
    assert!(!state.exists());
}

#[test]
fn discovery_reports_invalid_and_stale_records_without_credentials() {
    let directory = tempfile::tempdir().unwrap();
    let root = directory.path();
    let registry = root.join("dbgjs").join("services").join("invalid");
    fs::create_dir_all(&registry).unwrap();
    fs::write(registry.join("service.json"), "invalid json").unwrap();
    let cli = env!("CARGO_BIN_EXE_dbgjs");
    let listed = command(root, cli, &["--json", "service", "list"]);
    assert!(listed.status.success(), "{}", String::from_utf8_lossy(&listed.stderr));
    let services: Value = serde_json::from_slice(&listed.stdout).unwrap();
    assert_eq!(services[0]["status"], "invalid");
    let service = env!("CARGO_BIN_EXE_dbgjs-service");
    let ensure = command(root, service, &["--ensure", "--refuse-incompatible"]);
    assert!(!ensure.status.success());
    assert!(
        String::from_utf8_lossy(&ensure.stderr).contains("Cannot determine available services")
    );

    #[cfg(unix)]
    {
        use dbgjs::connection::transport::local_rpc::{
            LocalServiceEndpoint, LocalTransportEndpoint,
        };
        let stale = root.join("stale.json");
        let endpoint = LocalServiceEndpoint {
            process_id: 42,
            transport: LocalTransportEndpoint::UnixSocket {
                path: root.join("missing.sock"),
            },
            token: "do-not-print-this-token".into(),
            description: None,
        };
        fs::write(&stale, serde_json::to_vec(&endpoint).unwrap()).unwrap();
        let listed = command(
            root,
            service,
            &["--list", "--state-file", stale.to_str().unwrap()],
        );
        assert!(listed.status.success(), "{:?}", listed);
        assert!(!String::from_utf8_lossy(&listed.stdout).contains(&endpoint.token));
        let services: Value = serde_json::from_slice(&listed.stdout).unwrap();
        assert!(
            services
                .as_array()
                .unwrap()
                .iter()
                .any(|service| service["status"] == "stale")
        );
    }
}
