use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, ExitStatus, Output, Stdio};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::thread;
use std::time::{Duration, Instant};

use serde_json::Value;

static NEXT: AtomicUsize = AtomicUsize::new(0);

struct Fixture {
    root: PathBuf,
    state: PathBuf,
}

impl Fixture {
    fn new() -> Self {
        let root = std::env::current_dir()
            .unwrap()
            .join("artifacts")
            .join(format!(
                "cli-record-{}-{}",
                std::process::id(),
                NEXT.fetch_add(1, Ordering::Relaxed)
            ));
        fs::create_dir_all(&root).unwrap();
        let state = root.join("service.json");
        Self { root, state }
    }

    fn command(&self, arguments: &[&str]) -> Output {
        self.process(arguments).output().unwrap()
    }

    fn process(&self, arguments: &[&str]) -> Command {
        let mut command = Command::new(env!("CARGO_BIN_EXE_dbgjs"));
        command
            .args(arguments)
            .current_dir(&self.root)
            .env("DBGJS_SERVICE_STATE", &self.state)
            .env("DBGJS_SERVICE_EXE", env!("CARGO_BIN_EXE_dbgjs-service"));
        command
    }

    fn good(&self, arguments: &[&str]) -> Output {
        let output = self.command(arguments);
        assert!(
            output.status.success(),
            "{arguments:?}: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        output
    }

    fn manifest(&self, directory: &Path) -> Value {
        serde_json::from_slice(&fs::read(directory.join("manifest.json")).unwrap()).unwrap()
    }
}

impl Drop for Fixture {
    fn drop(&mut self) {
        if self.state.exists() {
            let _ = self.command(&["service", "stop"]);
        }
        let _ = fs::remove_dir_all(&self.root);
    }
}

fn wait_briefly(child: &mut Child) -> Option<ExitStatus> {
    let started = Instant::now();
    while started.elapsed() < Duration::from_secs(2) {
        if let Some(status) = child.try_wait().unwrap() {
            return Some(status);
        }
        thread::sleep(Duration::from_millis(20));
    }
    None
}

#[cfg(unix)]
#[test]
fn recording_regression_interactive_daemon_view_preserves_terminal_mode() {
    use std::os::fd::FromRawFd;

    let fixture = Fixture::new();
    fixture.good(&["context", "create", ":view", "--set"]);
    let recording = fixture.root.join("tty");
    fixture.good(&["record", "start", "--output", recording.to_str().unwrap()]);
    let (mut master, mut slave) = (0, 0);
    assert_eq!(
        unsafe {
            libc::openpty(
                &mut master,
                &mut slave,
                std::ptr::null_mut(),
                std::ptr::null(),
                std::ptr::null(),
            )
        },
        0
    );
    let _master = unsafe { fs::File::from_raw_fd(master) };
    let slave = unsafe { fs::File::from_raw_fd(slave) };
    let mut child = fixture
        .process(&["daemon", "view"])
        .stdout(Stdio::from(slave))
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    let status = wait_briefly(&mut child);
    if status.is_none() {
        child.kill().unwrap();
    }
    let output = child.wait_with_output().unwrap();
    assert!(
        status.is_none(),
        "terminal daemon view exited instead of remaining live: {status:?}"
    );
    assert!(String::from_utf8_lossy(&output.stderr).contains("not recorded"));
    fixture.good(&["record", "stop"]);
}

#[test]
fn recording_regression_concurrent_command_and_stop_do_not_wait_for_stdin() {
    let fixture = Fixture::new();
    fixture.good(&["context", "create", ":lock", "--set"]);
    let recording = fixture.root.join("concurrency");
    fixture.good(&["record", "start", "--output", recording.to_str().unwrap()]);
    let mut waiting = fixture
        .process(&["target", "eval", "-"])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    thread::sleep(Duration::from_millis(250));
    let mut other = fixture
        .process(&["--version"])
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    let other_status = wait_briefly(&mut other);
    if other_status.is_none() {
        other.kill().unwrap();
    }
    other.wait().unwrap();
    let mut stop = fixture
        .process(&["record", "stop"])
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    let stop_status = wait_briefly(&mut stop);
    if stop_status.is_none() {
        stop.kill().unwrap();
    }
    stop.wait().unwrap();
    waiting.kill().unwrap();
    waiting.wait().unwrap();
    assert!(
        other_status.is_some_and(|status| status.success()),
        "another command blocked behind open stdin"
    );
    assert!(
        stop_status.is_some_and(|status| status.success()),
        "record stop blocked behind open stdin"
    );
    let manifest = fixture.manifest(&recording);
    let entries = manifest["entries"].as_array().unwrap();
    assert_eq!(entries.len(), 1);
    assert_eq!(entries[0]["index"], 2);
    let markdown = fixture.root.join("concurrent.md");
    fixture.good(&[
        "record",
        "export",
        "--input",
        recording.to_str().unwrap(),
        "--output",
        markdown.to_str().unwrap(),
    ]);
    assert!(
        fs::read_to_string(markdown)
            .unwrap()
            .contains("## Command 2 — exit 0")
    );
}

#[test]
fn recording_regression_invalid_eval_exits_with_stdin_still_open() {
    let fixture = Fixture::new();
    fixture.good(&["context", "create", ":stdin", "--set"]);
    let recording = fixture.root.join("stdin");
    fixture.good(&["record", "start", "--output", recording.to_str().unwrap()]);
    let mut child = fixture
        .process(&["target", "eval", "-", "--full", "--max-preview-length", "1"])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    let status = wait_briefly(&mut child);
    if status.is_none() {
        child.kill().unwrap();
    }
    let output = child.wait_with_output().unwrap();
    assert!(
        status.is_some_and(|status| !status.success()),
        "invalid eval waited for terminal EOF"
    );
    assert!(
        String::from_utf8_lossy(&output.stderr)
            .contains("--full and --max-preview-length cannot be combined")
    );
    fixture.good(&["record", "stop"]);
}

#[test]
fn recording_stop_keeps_finalization_of_an_already_started_command() {
    use std::io::Write;

    let fixture = Fixture::new();
    fixture.good(&["context", "create", ":finish", "--set"]);
    let recording = fixture.root.join("inflight");
    fixture.good(&["record", "start", "--output", recording.to_str().unwrap()]);
    let mut child = fixture
        .process(&["target", "eval", "-"])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    let sidecar = recording.join(".000001.scope");
    let started = Instant::now();
    while !sidecar.exists() && started.elapsed() < Duration::from_secs(3) {
        thread::sleep(Duration::from_millis(20));
    }
    assert!(sidecar.exists(), "child never reserved its recording slot");
    fixture.good(&["record", "stop"]);
    child.stdin.take().unwrap().write_all(b"42\n").unwrap();
    let output = child.wait_with_output().unwrap();
    assert!(!output.status.success());
    let entry = fixture.manifest(&recording)["entries"][0].clone();
    assert_eq!(entry["index"], 1);
    assert_eq!(entry["exitCode"], output.status.code().unwrap());
    assert_eq!(
        fs::read(recording.join("000001.stdin.bin")).unwrap(),
        b"42\n"
    );
}

#[test]
fn recording_regression_scope_sidecar_is_redacted_before_child_exit() {
    let fixture = Fixture::new();
    fixture.good(&["context", "create", ":hidden-scope", "--set"]);
    let recording = fixture.root.join("sidecar");
    fixture.good(&[
        "record",
        "start",
        "--output",
        recording.to_str().unwrap(),
        "--redact",
        "hidden",
    ]);
    let mut child = fixture
        .process(&["target", "eval", "-", "--context", ":hidden-scope"])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    let sidecar = recording.join(".000001.scope");
    let started = Instant::now();
    while !sidecar.exists() && started.elapsed() < Duration::from_secs(3) {
        thread::sleep(Duration::from_millis(20));
    }
    let data =
        fs::read_to_string(&sidecar).expect("child should persist scope before reading stdin");
    child.kill().unwrap();
    child.wait().unwrap();
    assert!(
        !data.contains("hidden"),
        "sensitive scope persisted before redaction: {data}"
    );
    assert!(data.contains("[REDACTED]"));
    fixture.good(&["record", "stop"]);
}

#[test]
fn recording_is_opt_in_and_exports_exact_commands_outputs_and_failures() {
    let fixture = Fixture::new();
    let recording = fixture.root.join("private");
    let markdown = fixture.root.join("evidence.md");
    let recording_path = recording.to_str().unwrap();
    let markdown_path = markdown.to_str().unwrap();

    fixture.good(&["--json", "context", "create", ":before"]);
    assert!(!recording.exists());
    assert!(!fixture.state.with_extension("record-active.json").exists());

    fixture.good(&[
        "record",
        "start",
        "--output",
        recording_path,
        "--redact",
        "secret-probe",
    ]);
    let created = fixture.good(&[
        "--json",
        "context",
        "create",
        ":investigation",
        "secret-probe",
        "--set",
    ]);
    assert_eq!(
        serde_json::from_slice::<Value>(&created.stdout).unwrap()["id"],
        "investigation"
    );
    let inspected = fixture.good(&["--json", "context", "show", "--context", ":investigation"]);
    let script = "globalThis.value = `line one\nline two`;\nthrow Error('secret-probe')\n";
    let mut child = fixture
        .process(&["target", "eval", "-", "--context", ":investigation"])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    use std::io::Write;
    child
        .stdin
        .take()
        .unwrap()
        .write_all(script.as_bytes())
        .unwrap();
    let failure = child.wait_with_output().unwrap();
    assert!(
        !failure.status.success(),
        "target eval without a target should fail"
    );

    fixture.good(&["record", "stop"]);
    let manifest = fixture.manifest(&recording);
    let entries = manifest["entries"].as_array().unwrap();
    assert_eq!(entries.len(), 3);
    assert_eq!(
        entries[0]["argv"],
        serde_json::json!([
            "--json",
            "context",
            "create",
            ":investigation",
            "[REDACTED]",
            "--set"
        ])
    );
    assert_eq!(entries[1]["stdoutBytes"], inspected.stdout.len());
    assert_eq!(entries[1]["scope"]["context"], "investigation");
    assert_eq!(entries[1]["structuredJson"], true);
    assert_eq!(entries[2]["stdinBytes"], script.len());
    assert_eq!(entries[2]["exitCode"], failure.status.code().unwrap());
    assert!(
        entries[2]["startedUnixMs"].as_u64().unwrap()
            <= entries[2]["endedUnixMs"].as_u64().unwrap()
    );
    fixture.good(&[
        "record",
        "export",
        "--input",
        recording_path,
        "--format",
        "markdown",
        "--output",
        markdown_path,
    ]);
    let text = fs::read_to_string(&markdown).unwrap();
    assert!(text.contains("Command 3 — exit 1"), "{text}");
    assert!(
        text.contains("TRUNCATED") || text.contains("line one\nline two"),
        "{text}"
    );
    assert!(text.contains("investigation"), "{text}");
    assert!(!text.contains("secret-probe"), "{text}");
    let attachments = markdown.with_extension("attachments");
    assert_eq!(
        fs::read(attachments.join("000002.stdout.bin")).unwrap(),
        String::from_utf8(inspected.stdout)
            .unwrap()
            .replace("secret-probe", "[REDACTED]")
            .as_bytes()
    );
    assert_eq!(
        fs::read(recording.join("000003.stdin.bin")).unwrap(),
        script.replace("secret-probe", "[REDACTED]").as_bytes()
    );
    assert_eq!(
        fs::read(attachments.join("000003.stderr.bin")).unwrap(),
        failure.stderr
    );
    for file in fs::read_dir(&attachments).unwrap() {
        let bytes = fs::read(file.unwrap().path()).unwrap();
        assert!(!bytes.windows(12).any(|window| window == b"secret-probe"));
    }
    assert!(!recording.join("000004.stdout.bin").exists());
}

#[test]
fn recording_tracks_resolved_target_and_generation_across_connections() {
    let fixture = Fixture::new();
    let recording = fixture.root.join("targets");
    let fixture_path =
        Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/selector_browser.mjs");
    fixture.good(&["context", "create", ":multi", "--set"]);
    fixture.good(&["record", "start", "--output", recording.to_str().unwrap()]);
    for connection in ["first", "second"] {
        fixture.good(&[
            "connection",
            "add",
            "--stdio",
            "--topology",
            "browser",
            "--connection",
            connection,
            "--connect",
            "--",
            "node",
            fixture_path.to_str().unwrap(),
        ]);
    }
    let mut values = Vec::new();
    for connection in ["first", "second"] {
        let target = "browser/renderer/target/frame";
        fixture.good(&[
            "target",
            "attach",
            "--force",
            "--connection",
            connection,
            "--target",
            target,
        ]);
        let output = fixture.good(&[
            "target",
            "eval",
            "identity",
            "--connection",
            connection,
            "--target",
            target,
        ]);
        values.push(output.stdout);
    }
    assert!(
        values
            .iter()
            .all(|value| String::from_utf8_lossy(value).contains("browser/renderer/target/frame"))
    );
    fixture.good(&["record", "stop"]);
    let manifest = fixture.manifest(&recording);
    let entries = manifest["entries"].as_array().unwrap();
    assert_eq!(entries.len(), 6);
    for (i, connection) in ["first", "second"].iter().enumerate() {
        assert_eq!(entries[i]["scope"]["connection"], *connection);
        assert_eq!(entries[i]["scope"]["generation"], 1);
        let evaluation = &entries[3 + i * 2];
        assert_eq!(evaluation["scope"]["context"], "multi");
        assert_eq!(evaluation["scope"]["connection"], *connection);
        assert_eq!(evaluation["scope"]["generation"], 1);
        assert!(
            evaluation["scope"]["target"]
                .as_str()
                .unwrap()
                .contains("browser/renderer/target/frame")
        );
        assert_eq!(
            fs::read(recording.join(format!("{:06}.stdout.bin", 4 + i * 2))).unwrap(),
            values[i]
        );
    }
}

#[test]
fn recording_redacts_auth_headers_and_keeps_a_successful_exit_on_disk_failure() {
    let fixture = Fixture::new();
    let recording = fixture.root.join("headers");
    fixture.good(&["record", "start", "--output", recording.to_str().unwrap()]);
    let command = fixture.good(&[
        "context",
        "create",
        ":header",
        "Authorization: Bearer hidden-credential",
    ]);
    assert!(String::from_utf8_lossy(&command.stdout).contains("hidden-credential"));
    assert!(
        !fs::read_to_string(recording.join("000001.stdout.bin"))
            .unwrap()
            .contains("hidden-credential")
    );
    fs::write(recording.join("000002.stdout.bin"), "cannot overwrite").unwrap();
    let result = fixture.good(&["--version"]);
    assert!(
        String::from_utf8_lossy(&result.stderr).contains("recording failed after command exited 0")
    );
    assert!(String::from_utf8_lossy(&result.stdout).contains("dbgjs"));
    assert_eq!(
        fixture.manifest(&recording)["entries"]
            .as_array()
            .unwrap()
            .len(),
        1
    );
    fixture.good(&["record", "stop"]);
    let markdown = fixture.root.join("headers.md");
    fixture.good(&[
        "record",
        "export",
        "--input",
        recording.to_str().unwrap(),
        "--output",
        markdown.to_str().unwrap(),
    ]);
    assert!(
        !fs::read_to_string(&markdown)
            .unwrap()
            .contains("hidden-credential")
    );
    assert!(
        !fs::read_to_string(
            markdown
                .with_extension("attachments")
                .join("000001.stdout.bin")
        )
        .unwrap()
        .contains("hidden-credential")
    );
}

#[test]
fn recording_enforces_storage_limits_without_changing_command_output() {
    let fixture = Fixture::new();
    let recording = fixture.root.join("limited");
    fixture.good(&[
        "record",
        "start",
        "--output",
        recording.to_str().unwrap(),
        "--max-entries",
        "1",
        "--max-bytes",
        "8",
    ]);
    let first = fixture.good(&["--version"]);
    let manifest = fixture.manifest(&recording);
    assert_eq!(manifest["entries"].as_array().unwrap().len(), 1);
    assert!(manifest["entries"][0]["stdoutSaved"].as_u64().unwrap() <= 8);
    assert!(String::from_utf8_lossy(&first.stderr).contains("recording output truncated"));
    let second = fixture.good(&["--version"]);
    assert_eq!(first.stdout, second.stdout);
    assert!(String::from_utf8_lossy(&second.stderr).contains("--max-entries"));
    assert_eq!(
        fixture.manifest(&recording)["entries"]
            .as_array()
            .unwrap()
            .len(),
        1
    );
    fixture.good(&["record", "stop"]);
}

#[test]
fn recording_exports_a_large_output_attachment_with_bounded_excerpt() {
    let fixture = Fixture::new();
    let recording = fixture.root.join("large");
    fixture.good(&["record", "start", "--output", recording.to_str().unwrap()]);
    let display_name = "line\n```markdown\n".repeat(400);
    let output = fixture.good(&["context", "create", ":large", &display_name]);
    assert!(output.stdout.len() > 4096);
    fixture.good(&["record", "stop"]);
    let markdown = fixture.root.join("large.md");
    fixture.good(&[
        "record",
        "export",
        "--input",
        recording.to_str().unwrap(),
        "--output",
        markdown.to_str().unwrap(),
    ]);
    let contents = fs::read_to_string(&markdown).unwrap();
    assert!(contents.contains("excerpt TRUNCATED at 4096 bytes"));
    assert!(contents.contains("[Full local stdout attachment]"));
    assert_eq!(
        fs::read(
            markdown
                .with_extension("attachments")
                .join("000001.stdout.bin")
        )
        .unwrap(),
        output.stdout
    );
}

#[test]
fn recording_omit_policy_excludes_sensitive_arguments_stdin_and_results() {
    let fixture = Fixture::new();
    let recording = fixture.root.join("omitted");
    fixture.good(&[
        "record",
        "start",
        "--output",
        recording.to_str().unwrap(),
        "--omit-args",
        "--omit-stdin",
        "--omit-output",
    ]);
    fixture.good(&["context", "create", ":omitted", "never-persist-this"]);
    fixture.good(&["record", "stop"]);
    let entry = fixture.manifest(&recording)["entries"][0].clone();
    assert!(entry["argv"].is_null());
    assert_eq!(entry["stdoutSaved"], 0);
    assert!(!recording.join("000001.stdout.bin").exists());
    let markdown = fixture.root.join("omitted.md");
    fixture.good(&[
        "record",
        "export",
        "--input",
        recording.to_str().unwrap(),
        "--output",
        markdown.to_str().unwrap(),
    ]);
    assert!(
        !fs::read_to_string(markdown)
            .unwrap()
            .contains("never-persist-this")
    );
}
