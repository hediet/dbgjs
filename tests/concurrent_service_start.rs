#![cfg(windows)]

use std::collections::BTreeSet;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};
use std::sync::mpsc;
use std::thread;
use std::time::Duration;
use std::time::{SystemTime, UNIX_EPOCH};

#[test]
fn cold_context_creation_closes_captured_output_while_service_stays_alive() {
    let cli = PathBuf::from(env!("CARGO_BIN_EXE_dbgjs"));
    let service = PathBuf::from(env!("CARGO_BIN_EXE_dbgjs-service"));
    let directory = unique_test_directory("dbgjs-cold-piped-output");
    fs::create_dir(&directory).unwrap();
    let state_file = directory.join("service.json");
    let cleanup = ServiceCleanup {
        cli: cli.clone(),
        service: service.clone(),
        state_file: state_file.clone(),
        directory,
    };
    let (sender, receiver) = mpsc::channel();
    let cli_thread = cli.clone();
    let service_thread = service.clone();
    let state_file_thread = state_file.clone();
    thread::spawn(move || {
        let output = Command::new(cli_thread)
            .args(["--json", "context", "create", ":cold-piped-output"])
            .env("DBGJS_SERVICE_EXE", service_thread)
            .env("DBGJS_SERVICE_STATE", state_file_thread)
            .output()
            .unwrap();
        let _ = sender.send(output);
    });

    let output = match receiver.recv_timeout(Duration::from_secs(5)) {
        Ok(output) => output,
        Err(error) => {
            terminate_isolated_service(&state_file);
            panic!("cold CLI did not close its captured output: {error}");
        }
    };
    let created = successful_json(&output);
    let status = successful_json(
        &Command::new(&cli)
            .args(["--json", "service", "status"])
            .env("DBGJS_SERVICE_EXE", &service)
            .env("DBGJS_SERVICE_STATE", &state_file)
            .output()
            .unwrap(),
    );
    assert_eq!(created["agentInstanceId"], status["agentInstanceId"]);
    assert_ne!(status["processId"].as_u64(), Some(0));

    drop(cleanup);
}

#[test]
fn concurrent_cold_context_creation_starts_one_service() {
    let cli = PathBuf::from(env!("CARGO_BIN_EXE_dbgjs"));
    let service = PathBuf::from(env!("CARGO_BIN_EXE_dbgjs-service"));
    let directory = unique_test_directory("dbgjs-concurrent-start");
    fs::create_dir(&directory).unwrap();
    let state_file = directory.join("service.json");
    let cleanup = ServiceCleanup {
        cli: cli.clone(),
        service: service.clone(),
        state_file: state_file.clone(),
        directory,
    };

    let children = (0..6)
        .map(|index| {
            let stdout = cleanup.directory.join(format!("context-{index}.stdout"));
            let stderr = cleanup.directory.join(format!("context-{index}.stderr"));
            let child = Command::new(&cli)
                .args([
                    "--json",
                    "context",
                    "create",
                    &format!(":concurrent-{index}"),
                ])
                .env("DBGJS_SERVICE_EXE", &service)
                .env("DBGJS_SERVICE_STATE", &state_file)
                .stdout(Stdio::from(fs::File::create(&stdout).unwrap()))
                .stderr(Stdio::from(fs::File::create(&stderr).unwrap()))
                .spawn()
                .unwrap();
            (child, stdout, stderr)
        })
        .collect::<Vec<_>>();
    let outputs = children
        .into_iter()
        .map(|(mut child, stdout, stderr)| Output {
            status: child.wait().unwrap(),
            stdout: fs::read(stdout).unwrap(),
            stderr: fs::read(stderr).unwrap(),
        })
        .collect::<Vec<_>>();

    let instances = outputs
        .iter()
        .map(successful_json)
        .map(|value| value["agentInstanceId"].as_str().unwrap().to_owned())
        .collect::<BTreeSet<_>>();
    assert_eq!(instances.len(), 1, "all clients must reach one service");

    let status = successful_json(
        &Command::new(&cli)
            .args(["--json", "service", "status"])
            .env("DBGJS_SERVICE_EXE", &service)
            .env("DBGJS_SERVICE_STATE", &state_file)
            .output()
            .unwrap(),
    );
    let endpoint: serde_json::Value =
        serde_json::from_slice(&fs::read(&state_file).unwrap()).unwrap();
    assert_eq!(status["processId"], endpoint["processId"]);

    drop(cleanup);
}

fn unique_test_directory(prefix: &str) -> PathBuf {
    std::env::temp_dir().join(format!(
        "{prefix}-{}-{}",
        std::process::id(),
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ))
}

fn terminate_isolated_service(state_file: &Path) {
    let endpoint: serde_json::Value =
        serde_json::from_slice(&fs::read(state_file).unwrap()).unwrap();
    let process_id = endpoint["processId"].as_u64().unwrap().to_string();
    let status = Command::new("taskkill")
        .args(["/PID", &process_id, "/F"])
        .status()
        .unwrap();
    assert!(status.success(), "failed to terminate isolated service");
}

fn successful_json(output: &Output) -> serde_json::Value {
    assert!(
        output.status.success(),
        "CLI failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    serde_json::from_slice(&output.stdout).unwrap()
}

struct ServiceCleanup {
    cli: PathBuf,
    service: PathBuf,
    state_file: PathBuf,
    directory: PathBuf,
}

impl Drop for ServiceCleanup {
    fn drop(&mut self) {
        let _ = Command::new(&self.cli)
            .args(["service", "stop"])
            .env("DBGJS_SERVICE_EXE", &self.service)
            .env("DBGJS_SERVICE_STATE", &self.state_file)
            .output();
        remove_file_if_present(&self.state_file);
        remove_file_if_present(&self.state_file.with_extension("contexts.json"));
        remove_file_if_present(&self.state_file.with_extension("startup.lock"));
        remove_file_if_present(&self.state_file.with_extension("startup-error.txt"));
        let _ = fs::remove_dir_all(&self.directory);
    }
}

fn remove_file_if_present(path: &Path) {
    match fs::remove_file(path) {
        Ok(()) => {}
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(error) => panic!("failed to remove {}: {error}", path.display()),
    }
}
