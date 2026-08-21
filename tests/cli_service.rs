use std::fs::{self, File};
use std::path::{Path, PathBuf};
use std::process::{Command, ExitStatus, Stdio};
use std::thread;
use std::time::{Duration, Instant};

use serde_json::Value;

use cdp_client::local_rpc::{
    LocalTransportEndpoint, persistent_state_file, read_endpoint, startup_error_file,
};

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
        &["context", "create", "shop", "Shop"],
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
        &["connection", "add", "shop", "server", "ws://127.0.0.1:9229"],
    );
    run_json(
        &cli,
        &service,
        &state_file,
        &[
            "connection",
            "add",
            "shop",
            "browser",
            "ws://127.0.0.1:9222",
        ],
    );
    run_json(
        &cli,
        &service,
        &state_file,
        &[
            "breakpoint",
            "set",
            "shop",
            "shared-validation",
            "file:///workspace/shared/validation.ts",
            "41",
            "1",
        ],
    );

    let snapshot = run_json(&cli, &service, &state_file, &["context", "show", "shop"]);
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
    assert_eq!(snapshot["breakpoints"][0]["status"], "unconfirmed");

    let stopped = run_json(&cli, &service, &state_file, &["service", "stop"]);
    assert_eq!(stopped, Value::Bool(true));
    wait_until_removed(&state_file);
    cleanup_persistent_state(&state_file);
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
        &["context", "create", "shop", "Shop"],
    );
    let first_instance = created["agentInstanceId"].as_str().unwrap().to_owned();
    run_json(
        &cli,
        &service,
        &state_file,
        &["connection", "add", "shop", "server", "ws://127.0.0.1:9229"],
    );
    run_json(
        &cli,
        &service,
        &state_file,
        &[
            "breakpoint",
            "set",
            "shop",
            "shared-validation",
            "file:///workspace/shared/validation.ts",
            "41",
            "1",
        ],
    );
    run_json(&cli, &service, &state_file, &["service", "stop"]);
    wait_until_removed(&state_file);

    let restored = run_json(&cli, &service, &state_file, &["context", "show", "shop"]);
    assert_ne!(restored["agentInstanceId"], first_instance);
    assert_eq!(restored["displayName"], "Shop");
    assert_eq!(restored["revision"], 3);
    assert_eq!(restored["connections"][0]["id"], "server");
    assert_eq!(restored["connections"][0]["status"]["kind"], "disconnected");
    assert_eq!(
        restored["breakpoints"][0]["sourcePath"],
        "file:///workspace/shared/validation.ts"
    );

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
        &["context", "create", "live-browser"],
    );
    run_json(
        &cli,
        &service,
        &state_file,
        &["connection", "add", "live-browser", "browser", &endpoint],
    );
    let connected = run_json(
        &cli,
        &service,
        &state_file,
        &["connection", "connect", "live-browser", "browser"],
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

    let disconnected = run_json(
        &cli,
        &service,
        &state_file,
        &["connection", "disconnect", "live-browser", "browser"],
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
        &["connection", "connect", "live-browser", "browser"],
    );
    assert_eq!(reconnected["connections"][0]["status"]["kind"], "connected");
    assert_eq!(reconnected["connections"][0]["generation"], 2);

    run_json(&cli, &service, &state_file, &["service", "stop"]);
    wait_until_removed(&state_file);
    cleanup_persistent_state(&state_file);
    cleanup.disarm();
}

fn run_json(cli: &Path, service: &Path, state_file: &Path, arguments: &[&str]) -> Value {
    let suffix = unique_suffix();
    let stdout_path = state_file.with_extension(format!("{suffix}.stdout"));
    let stderr_path = state_file.with_extension(format!("{suffix}.stderr"));
    let status = Command::new(cli)
        .arg("--json")
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
    assert_success(arguments, status, &stdout, &stderr);
    serde_json::from_slice(&stdout).unwrap_or_else(|error| {
        panic!(
            "invalid JSON for {arguments:?}: {error}\nstdout: {}\nstderr: {}",
            String::from_utf8_lossy(&stdout),
            String::from_utf8_lossy(&stderr)
        )
    })
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
