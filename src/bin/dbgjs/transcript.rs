use std::env;
use std::ffi::OsString;
use std::fs::{self, File, OpenOptions};
use std::io::{self, IsTerminal, Read, Write};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::mpsc;
use std::thread;
use std::time::{Instant, SystemTime, UNIX_EPOCH};

use atomic_write_file::AtomicWriteFile;
use dbgjs::local_rpc::connect_existing;
use fs2::FileExt;
use percent_encoding::{NON_ALPHANUMERIC, utf8_percent_encode};
use regex::Regex;
use serde::{Deserialize, Serialize};

use super::{ResolvedScope, ScopeOptions};

const DEFAULT_MAX_ENTRIES: usize = 500;
const DEFAULT_MAX_BYTES: usize = 32 * 1024 * 1024;
const EXCERPT_BYTES: usize = 4096;

#[derive(Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
struct Manifest {
    version: u32,
    max_entries: usize,
    max_bytes: usize,
    #[serde(default)]
    next_index: usize,
    redact: Vec<String>,
    omit_args: bool,
    omit_stdin: bool,
    omit_output: bool,
    entries: Vec<Entry>,
}

#[derive(Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
struct Entry {
    #[serde(default)]
    index: usize,
    started_unix_ms: u128,
    ended_unix_ms: u128,
    duration_ms: u128,
    argv: Option<Vec<String>>,
    stdin_bytes: usize,
    stdout_bytes: usize,
    stderr_bytes: usize,
    stdin_saved: usize,
    stdout_saved: usize,
    stderr_saved: usize,
    stdin_truncated: bool,
    stdout_truncated: bool,
    stderr_truncated: bool,
    exit_code: i32,
    scope: Scope,
    version: String,
    build_commit: String,
    build_dirty: String,
    structured_json: bool,
}

#[derive(Clone, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(super) struct Scope {
    context: Option<String>,
    connection: Option<String>,
    target: Option<String>,
    generation: Option<u64>,
}

fn active_file(state: &Path) -> PathBuf {
    state.with_extension("record-active.json")
}

fn lock_file(state: &Path) -> PathBuf {
    state.with_extension("record.lock")
}

fn manifest_file(directory: &Path) -> PathBuf {
    directory.join("manifest.json")
}

fn private_file(path: &Path) -> io::Result<File> {
    let mut options = OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    options.open(path)
}

fn private_directory(path: &Path) -> io::Result<()> {
    let mut builder = fs::DirBuilder::new();
    #[cfg(unix)]
    {
        use std::os::unix::fs::DirBuilderExt;
        builder.mode(0o700);
    }
    builder.create(path)
}

fn save_json<T: Serialize>(path: &Path, value: &T) -> io::Result<()> {
    let bytes = serde_json::to_vec_pretty(value)?;
    let mut file = AtomicWriteFile::open(path)?;
    file.write_all(&bytes)?;
    file.commit()?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(path, fs::Permissions::from_mode(0o600))?;
    }
    Ok(())
}

fn open_lock(state: &Path) -> io::Result<File> {
    if let Some(parent) = state.parent() {
        if !parent.exists() {
            fs::create_dir_all(parent)?;
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt;
                fs::set_permissions(parent, fs::Permissions::from_mode(0o700))?;
            }
        }
    }
    let mut options = OpenOptions::new();
    options.create(true).read(true).write(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    let file = options.open(lock_file(state))?;
    file.lock_exclusive()?;
    Ok(file)
}

fn absolute(path: &str) -> io::Result<PathBuf> {
    let path = PathBuf::from(path);
    Ok(if path.is_absolute() {
        path
    } else {
        env::current_dir()?.join(path)
    })
}

fn number(value: Option<&OsString>, option: &str) -> Result<usize, String> {
    let number = value
        .and_then(|value| value.to_str())
        .ok_or_else(|| format!("{option} requires a positive integer"))?
        .parse::<usize>()
        .map_err(|_| format!("{option} requires a positive integer"))?;
    if number == 0 {
        return Err(format!("{option} requires a positive integer"));
    }
    Ok(number)
}

fn argument<'a>(value: Option<&'a OsString>, option: &str) -> Result<&'a str, String> {
    value
        .and_then(|value| value.to_str())
        .ok_or_else(|| format!("{option} requires a value"))
}

fn handle_record(args: &[OsString], state: &Path) -> Result<i32, String> {
    match args.get(1).and_then(|value| value.to_str()) {
        Some("start") => {
            let mut directory = None;
            let mut manifest = Manifest {
                version: 1,
                max_entries: DEFAULT_MAX_ENTRIES,
                max_bytes: DEFAULT_MAX_BYTES,
                next_index: 1,
                redact: Vec::new(),
                omit_args: false,
                omit_stdin: false,
                omit_output: false,
                entries: Vec::new(),
            };
            let mut index = 2;
            while index < args.len() {
                let option = args[index].to_string_lossy();
                match option.as_ref() {
                    "--output" => {
                        index += 1;
                        directory = Some(absolute(argument(args.get(index), "--output")?).map_err(|e| e.to_string())?);
                    }
                    "--max-entries" => {
                        index += 1;
                        manifest.max_entries = number(args.get(index), "--max-entries")?;
                    }
                    "--max-bytes" => {
                        index += 1;
                        manifest.max_bytes = number(args.get(index), "--max-bytes")?;
                    }
                    "--redact" => {
                        index += 1;
                        let literal = argument(args.get(index), "--redact")?;
                        if literal.is_empty() {
                            return Err("--redact requires a nonempty literal".into());
                        }
                        manifest.redact.push(literal.to_owned());
                    }
                    "--omit-args" => manifest.omit_args = true,
                    "--omit-stdin" => manifest.omit_stdin = true,
                    "--omit-output" => manifest.omit_output = true,
                    _ => return Err(format!("unknown record start option '{option}'")),
                }
                index += 1;
            }
            let directory = directory.ok_or("--output <new-local-directory> is required")?;
            let _lock = open_lock(state).map_err(|e| e.to_string())?;
            if active_file(state).exists() {
                return Err("a recording is already active; run `dbgjs record stop` first".into());
            }
            private_directory(&directory).map_err(|e| format!("creating {}: {e}", directory.display()))?;
            save_json(&manifest_file(&directory), &manifest).map_err(|e| e.to_string())?;
            save_json(&active_file(state), &directory).map_err(|e| e.to_string())?;
            println!("Recording locally to {}", directory.display());
            Ok(0)
        }
        Some("stop") if args.len() == 2 => {
            let _lock = open_lock(state).map_err(|e| e.to_string())?;
            let directory: PathBuf = serde_json::from_slice(
                &fs::read(active_file(state)).map_err(|e| format!("no active recording: {e}"))?,
            )
            .map_err(|e| e.to_string())?;
            fs::remove_file(active_file(state)).map_err(|e| e.to_string())?;
            println!("Recording stopped: {}", directory.display());
            Ok(0)
        }
        Some("export") => {
            let mut directory = None;
            let mut output = None;
            let mut index = 2;
            while index < args.len() {
                let option = args[index].to_string_lossy();
                match option.as_ref() {
                    "--input" => {
                        index += 1;
                        directory = Some(absolute(argument(args.get(index), "--input")?).map_err(|e| e.to_string())?);
                    }
                    "--output" => {
                        index += 1;
                        output = Some(absolute(argument(args.get(index), "--output")?).map_err(|e| e.to_string())?);
                    }
                    "--format" => {
                        index += 1;
                        if argument(args.get(index), "--format")? != "markdown" {
                            return Err("only --format markdown is supported".into());
                        }
                    }
                    _ => return Err(format!("unknown record export option '{option}'")),
                }
                index += 1;
            }
            export(
                &directory.ok_or("--input <recording-directory> is required")?,
                &output.ok_or("--output <local-markdown-file> is required")?,
            )
            .map_err(|e| e.to_string())?;
            Ok(0)
        }
        _ => Err("usage: dbgjs record start --output <new-directory> [--max-entries N] [--max-bytes N] [--redact LITERAL]... [--omit-args] [--omit-stdin] [--omit-output]\n       dbgjs record stop\n       dbgjs record export --input <directory> --output <file.md> [--format markdown]".into()),
    }
}

pub(super) fn entry(args: &[OsString], state: &Path) -> io::Result<Option<i32>> {
    if args.first().is_some_and(|arg| arg == "record") {
        return Ok(Some(match handle_record(args, state) {
            Ok(code) => code,
            Err(error) => {
                eprintln!("dbgjs: recording: {error}");
                1
            }
        }));
    }
    if !active_file(state).exists() {
        return Ok(None);
    }
    if io::stdout().is_terminal()
        && args
            .windows(2)
            .any(|pair| pair[0] == "daemon" && pair[1] == "view")
    {
        eprintln!(
            "dbgjs: recording: interactive daemon view is not recorded; terminal view remains live"
        );
        return Ok(None);
    }
    let lock = match open_lock(state) {
        Ok(lock) => lock,
        Err(error) => {
            eprintln!("dbgjs: recording unavailable: {error}");
            return Ok(None);
        }
    };
    let directory: PathBuf = match fs::read(active_file(state))
        .and_then(|bytes| serde_json::from_slice(&bytes).map_err(io::Error::other))
    {
        Ok(directory) => directory,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(error) => {
            eprintln!("dbgjs: recording unavailable: {error}");
            return Ok(None);
        }
    };
    let mut manifest: Manifest = match fs::read(manifest_file(&directory))
        .and_then(|bytes| serde_json::from_slice(&bytes).map_err(io::Error::other))
    {
        Ok(manifest) => manifest,
        Err(error) => {
            eprintln!("dbgjs: recording unavailable: {error}");
            return Ok(None);
        }
    };
    let index = manifest
        .next_index
        .max(
            manifest
                .entries
                .iter()
                .map(|entry| entry.index)
                .max()
                .unwrap_or(0)
                .saturating_add(1),
        )
        .max(manifest.entries.len().saturating_add(1));
    if index > manifest.max_entries {
        eprintln!("dbgjs: recording reached the --max-entries limit; command runs unrecorded");
        return Ok(None);
    }
    manifest.next_index = index.saturating_add(1);
    if let Err(error) = save_json(&manifest_file(&directory), &manifest) {
        eprintln!("dbgjs: recording unavailable: cannot reserve command: {error}");
        return Ok(None);
    }
    drop(lock);
    let status = record_command(args, state, &directory, index, &manifest)?;
    Ok(Some(status))
}

fn now_ms() -> u128 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis()
}

fn capture<R: Read, W: Write>(
    mut source: R,
    mut destination: W,
    limit: usize,
) -> io::Result<(Vec<u8>, usize)> {
    let mut collected = Vec::new();
    let mut size = 0usize;
    let mut buffer = [0u8; 8192];
    loop {
        let count = source.read(&mut buffer)?;
        if count == 0 {
            break;
        }
        destination.write_all(&buffer[..count])?;
        size = size.saturating_add(count);
        collected.extend_from_slice(&buffer[..count.min(limit.saturating_sub(collected.len()))]);
    }
    destination.flush()?;
    Ok((collected, size))
}

fn stdin_script(args: &[OsString]) -> bool {
    let arguments = args
        .iter()
        .map(|arg| arg.to_string_lossy())
        .collect::<Vec<_>>();
    (arguments.iter().any(|arg| arg == "eval")
        && arguments.iter().any(|arg| arg == "target")
        && arguments.iter().any(|arg| arg == "-"))
        || (arguments.first().is_some_and(|arg| arg == "playwright")
            && arguments.iter().any(|arg| arg == "-"))
}

fn output_path(directory: &Path, index: usize, stream: &str) -> PathBuf {
    directory.join(format!("{index:06}.{stream}.bin"))
}

fn scrub(text: &str, literals: &[String]) -> String {
    let mut text = text.to_owned();
    for literal in literals {
        text = text.replace(literal, "[REDACTED]");
    }
    let headers = Regex::new(r#"(?im)\b(authorization|proxy-authorization|cookie|set-cookie|x-api-key)(["']?\s*[:=]\s*)(?:"(?:\\.|[^"\\])*"|'[^']*'|[^\r\n,}]+)"#).unwrap();
    text = headers.replace_all(&text, "$1$2[REDACTED]").into_owned();
    let flags = Regex::new(r"(?i)(--(?:password|token|api-key|auth)(?:=|\s+))\S+").unwrap();
    flags.replace_all(&text, "$1[REDACTED]").into_owned()
}

fn scrub_bytes(bytes: &[u8], literals: &[String]) -> Vec<u8> {
    match std::str::from_utf8(bytes) {
        Ok(text) => scrub(text, literals).into_bytes(),
        Err(_) => {
            let mut bytes = bytes.to_vec();
            for literal in literals {
                let needle = literal.as_bytes();
                if needle.is_empty() {
                    continue;
                }
                let mut result = Vec::with_capacity(bytes.len());
                let mut remaining = bytes.as_slice();
                while let Some(pos) = remaining
                    .windows(needle.len())
                    .position(|part| part == needle)
                {
                    result.extend_from_slice(&remaining[..pos]);
                    result.extend_from_slice(b"[REDACTED]");
                    remaining = &remaining[pos + needle.len()..];
                }
                result.extend_from_slice(remaining);
                bytes = result;
            }
            let headers = regex::bytes::Regex::new(r#"(?im)\b(authorization|proxy-authorization|cookie|set-cookie|x-api-key)(["']?\s*[:=]\s*)(?:"(?:\\.|[^"\\])*"|'[^']*'|[^\r\n,}]+)"#).unwrap();
            bytes = headers
                .replace_all(&bytes, &b"$1$2[REDACTED]"[..])
                .into_owned();
            let flags =
                regex::bytes::Regex::new(r"(?i)(--(?:password|token|api-key|auth)(?:=|\s+))\S+")
                    .unwrap();
            bytes = flags.replace_all(&bytes, &b"$1[REDACTED]"[..]).into_owned();
            bytes
        }
    }
}

fn record_command(
    args: &[OsString],
    state: &Path,
    directory: &Path,
    index: usize,
    manifest: &Manifest,
) -> io::Result<i32> {
    let capture_limit = manifest.max_bytes;
    let scope_path = directory.join(format!(".{index:06}.scope"));
    let mut command = Command::new(env::current_exe()?);
    command
        .args(args)
        .env("DBGJS_RECORD_CHILD", "1")
        .env("DBGJS_RECORD_SCOPE_FILE", &scope_path)
        .env("DBGJS_RECORD_MANIFEST_FILE", manifest_file(directory))
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    let script = stdin_script(args);
    if script {
        command.stdin(Stdio::piped());
    } else {
        command.stdin(Stdio::inherit());
    }
    let started_unix_ms = now_ms();
    let clock = Instant::now();
    let mut child = command.spawn()?;
    let stdin_rx = if script {
        let (sender, receiver) = mpsc::channel();
        let stdin = child.stdin.take().expect("piped stdin");
        thread::spawn(move || {
            let _ = sender.send(capture(io::stdin(), stdin, capture_limit));
        });
        Some(receiver)
    } else {
        None
    };
    let stdout = child.stdout.take().expect("piped stdout");
    let stderr = child.stderr.take().expect("piped stderr");
    let out = thread::spawn(move || capture(stdout, io::stdout(), capture_limit));
    let err = thread::spawn(move || capture(stderr, io::stderr(), capture_limit));
    let status = child.wait()?;
    let (input, stdin_bytes) = match stdin_rx {
        Some(receiver) => match receiver.recv_timeout(std::time::Duration::from_millis(50)) {
            Ok(Ok(captured)) => captured,
            Ok(Err(error)) => {
                eprintln!("dbgjs: recording stdin forwarding failed: {error}");
                (Vec::new(), 0)
            }
            Err(error) => {
                eprintln!("dbgjs: recording stdin incomplete when command exited: {error}");
                (Vec::new(), 0)
            }
        },
        None => (Vec::new(), 0),
    };
    let (stdout, stdout_bytes) = match out.join() {
        Ok(Ok(captured)) => captured,
        other => {
            eprintln!("dbgjs: recording stdout unavailable after command exited: {other:?}");
            (Vec::new(), 0)
        }
    };
    let (stderr, stderr_bytes) = match err.join() {
        Ok(Ok(captured)) => captured,
        other => {
            eprintln!("dbgjs: recording stderr unavailable after command exited: {other:?}");
            (Vec::new(), 0)
        }
    };
    let ended_unix_ms = now_ms();
    let duration_ms = clock.elapsed().as_millis();
    let mut scope: Scope = fs::read(&scope_path)
        .ok()
        .and_then(|bytes| serde_json::from_slice(&bytes).ok())
        .unwrap_or_default();
    let _ = fs::remove_file(&scope_path);
    for field in [&mut scope.context, &mut scope.connection, &mut scope.target] {
        if let Some(value) = field {
            *value = scrub(value, &manifest.redact);
        }
    }
    let argv = (!manifest.omit_args).then(|| {
        let mut previous_secret_flag = false;
        args.iter()
            .map(|arg| {
                let text = arg.to_string_lossy();
                let result = if previous_secret_flag {
                    "[REDACTED]".to_owned()
                } else {
                    scrub(&text, &manifest.redact)
                };
                previous_secret_flag = matches!(
                    text.to_ascii_lowercase().as_str(),
                    "--token" | "--password" | "--api-key" | "--auth"
                );
                result
            })
            .collect::<Vec<_>>()
    });
    let exit_code = status.code().unwrap_or_else(|| {
        #[cfg(unix)]
        {
            use std::os::unix::process::ExitStatusExt;
            128 + status.signal().unwrap_or(1)
        }
        #[cfg(not(unix))]
        {
            1
        }
    });
    let safe_stdin = (!manifest.omit_stdin).then(|| scrub_bytes(&input, &manifest.redact));
    let safe_stdout = (!manifest.omit_output).then(|| scrub_bytes(&stdout, &manifest.redact));
    let safe_stderr = (!manifest.omit_output).then(|| scrub_bytes(&stderr, &manifest.redact));
    let persistence = (|| -> io::Result<()> {
        let _lock = open_lock(state)?;
        let mut current: Manifest = serde_json::from_slice(&fs::read(manifest_file(directory))?)?;
        let stored = current.entries.iter().fold(0usize, |sum, entry| {
            sum.saturating_add(entry.stdin_saved)
                .saturating_add(entry.stdout_saved)
                .saturating_add(entry.stderr_saved)
        });
        let mut remaining = current.max_bytes.saturating_sub(stored);
        let mut save = |stream: &str,
                        safe: &[u8],
                        captured: usize,
                        original: usize|
         -> io::Result<(usize, bool)> {
            let size = safe.len().min(remaining);
            remaining -= size;
            if size > 0 {
                private_file(&output_path(directory, index, stream))?.write_all(&safe[..size])?;
            }
            Ok((size, size < safe.len() || captured < original))
        };
        let (stdin_saved, stdin_truncated) = match safe_stdin.as_ref() {
            Some(safe) => save("stdin", safe, input.len(), stdin_bytes)?,
            None => (0, false),
        };
        let (stdout_saved, stdout_truncated) = match safe_stdout.as_ref() {
            Some(safe) => save("stdout", safe, stdout.len(), stdout_bytes)?,
            None => (0, false),
        };
        let (stderr_saved, stderr_truncated) = match safe_stderr.as_ref() {
            Some(safe) => save("stderr", safe, stderr.len(), stderr_bytes)?,
            None => (0, false),
        };
        if stdin_truncated || stdout_truncated || stderr_truncated {
            eprintln!(
                "dbgjs: recording output truncated at the --max-bytes limit; debugger exit status is unchanged"
            );
        }
        current.entries.push(Entry {
            index,
            started_unix_ms,
            ended_unix_ms,
            duration_ms,
            argv,
            stdin_bytes,
            stdout_bytes,
            stderr_bytes,
            stdin_saved,
            stdout_saved,
            stderr_saved,
            stdin_truncated,
            stdout_truncated,
            stderr_truncated,
            exit_code,
            scope,
            version: env!("CARGO_PKG_VERSION").into(),
            build_commit: env!("DBGJS_BUILD_GIT_COMMIT").into(),
            build_dirty: env!("DBGJS_BUILD_GIT_DIRTY").into(),
            structured_json: serde_json::from_slice::<serde_json::Value>(&stdout).is_ok(),
        });
        save_json(&manifest_file(directory), &current)
    })();
    if let Err(error) = persistence {
        eprintln!("dbgjs: recording failed after command exited {exit_code}: {error}");
    }
    Ok(exit_code)
}

fn fence(text: &str) -> String {
    let longest = text
        .split(|character| character != '`')
        .map(str::len)
        .max()
        .unwrap_or(0);
    "`".repeat(longest.max(2) + 1)
}

fn excerpt(bytes: &[u8]) -> (String, bool) {
    let length = bytes.len().min(EXCERPT_BYTES);
    let until_boundary = (0..=length)
        .rev()
        .find(|&index| std::str::from_utf8(&bytes[..index]).is_ok());
    let length = until_boundary.unwrap_or(length);
    (
        String::from_utf8_lossy(&bytes[..length]).into_owned(),
        bytes.len() > length,
    )
}

fn export(directory: &Path, path: &Path) -> io::Result<()> {
    let manifest: Manifest = serde_json::from_slice(&fs::read(manifest_file(directory))?)?;
    if manifest.version != 1 {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "unknown recording format",
        ));
    }
    let attachments = path.with_extension("attachments");
    private_directory(&attachments)?;
    let relative = utf8_percent_encode(
        &attachments.file_name().unwrap().to_string_lossy(),
        NON_ALPHANUMERIC,
    )
    .to_string();
    let mut markdown = String::from(
        "# dbgjs investigation transcript\n\nLocal evidence only; CLI completion does not imply completion of target promises, background tasks, or persistent runtime edits. Times are Unix milliseconds. Attachments contain exact retained bytes except where recording policy masks or omits sensitive data; truncation is explicitly marked.\n\n",
    );
    let mut entries = manifest
        .entries
        .iter()
        .enumerate()
        .map(|(offset, entry)| {
            (
                if entry.index == 0 {
                    offset + 1
                } else {
                    entry.index
                },
                entry,
            )
        })
        .collect::<Vec<_>>();
    entries.sort_by_key(|(index, _)| *index);
    for (index, entry) in entries {
        markdown.push_str(&format!(
            "## Command {index} — exit {}\n\n",
            entry.exit_code
        ));
        markdown.push_str(&format!(
            "- Started: {} ms; ended: {} ms; CLI duration: {} ms\n- Resolved scope: context={}, connection={}, target={}, generation={}\n- dbgjs {} (commit {}, dirty {})\n",
            entry.started_unix_ms, entry.ended_unix_ms, entry.duration_ms,
            serde_json::to_string(&entry.scope.context)?, serde_json::to_string(&entry.scope.connection)?,
            serde_json::to_string(&entry.scope.target)?, serde_json::to_string(&entry.scope.generation)?,
            entry.version, entry.build_commit, entry.build_dirty
        ));
        if entry.structured_json {
            markdown.push_str("- stdout: JSON structured result (recorded bytes below)\n");
        }
        markdown.push_str("- Completion: CLI process exited; target-side asynchronous work and persistent edits are not inferred.\n");
        let Some(argv) = &entry.argv else {
            markdown.push_str("\nInvocation arguments omitted by recording policy.\n\n");
            render_stream(
                directory,
                &attachments,
                &relative,
                &mut markdown,
                index,
                "stdin",
                entry.stdin_bytes,
                entry.stdin_saved,
                entry.stdin_truncated,
                manifest.omit_stdin,
                &manifest.redact,
            )?;
            render_stream(
                directory,
                &attachments,
                &relative,
                &mut markdown,
                index,
                "stdout",
                entry.stdout_bytes,
                entry.stdout_saved,
                entry.stdout_truncated,
                manifest.omit_output,
                &manifest.redact,
            )?;
            render_stream(
                directory,
                &attachments,
                &relative,
                &mut markdown,
                index,
                "stderr",
                entry.stderr_bytes,
                entry.stderr_saved,
                entry.stderr_truncated,
                manifest.omit_output,
                &manifest.redact,
            )?;
            continue;
        };
        let invocation = serde_json::to_string_pretty(argv)?;
        let invocation = scrub(&invocation, &manifest.redact);
        markdown.push_str(
            "\nExact argument vector (invoke `dbgjs` with these positional arguments):\n\n",
        );
        let delimiter = fence(&invocation);
        markdown.push_str(&format!("{delimiter}json\n{invocation}\n{delimiter}\n\n"));
        for (stream, original, saved, truncated, omitted) in [
            (
                "stdin",
                entry.stdin_bytes,
                entry.stdin_saved,
                entry.stdin_truncated,
                manifest.omit_stdin,
            ),
            (
                "stdout",
                entry.stdout_bytes,
                entry.stdout_saved,
                entry.stdout_truncated,
                manifest.omit_output,
            ),
            (
                "stderr",
                entry.stderr_bytes,
                entry.stderr_saved,
                entry.stderr_truncated,
                manifest.omit_output,
            ),
        ] {
            render_stream(
                directory,
                &attachments,
                &relative,
                &mut markdown,
                index,
                stream,
                original,
                saved,
                truncated,
                omitted,
                &manifest.redact,
            )?;
        }
    }
    private_file(path)?.write_all(markdown.as_bytes())?;
    println!("Markdown transcript: {}", path.display());
    Ok(())
}

fn render_stream(
    directory: &Path,
    attachments: &Path,
    relative: &str,
    markdown: &mut String,
    index: usize,
    stream: &str,
    original: usize,
    saved: usize,
    truncated: bool,
    omitted: bool,
    redact: &[String],
) -> io::Result<()> {
    markdown.push_str(&format!("### {stream} ({original} bytes produced"));
    if truncated {
        markdown.push_str(&format!(
            ", {saved} bytes retained; TRUNCATED at recording limit"
        ));
    } else if omitted {
        markdown.push_str("; OMITTED by recording policy");
    } else if saved != original {
        markdown.push_str(&format!(", {saved} bytes retained after redaction"));
    }
    markdown.push_str(")\n\n");
    if saved == 0 {
        markdown.push_str("No recorded bytes.\n\n");
        return Ok(());
    }
    let bytes = fs::read(output_path(directory, index, stream))?;
    let bytes = scrub_bytes(&bytes, redact);
    let name = format!("{index:06}.{stream}.bin");
    private_file(&attachments.join(&name))?.write_all(&bytes)?;
    markdown.push_str(&format!(
        "[Full local {stream} attachment]({relative}/{name})"
    ));
    let (text, truncated) = excerpt(&bytes);
    if truncated {
        markdown.push_str(&format!(" — excerpt TRUNCATED at {EXCERPT_BYTES} bytes"));
    }
    markdown.push_str("\n\n");
    let delimiter = fence(&text);
    markdown.push_str(&format!("{delimiter}text\n{text}"));
    if !text.ends_with('\n') {
        markdown.push('\n');
    }
    markdown.push_str(&format!("{delimiter}\n\n"));
    Ok(())
}

pub(super) fn note_context(context: Option<&str>) {
    if let Some(context) = context {
        note(|scope| scope.context = Some(context.to_owned()));
    }
}

pub(super) fn note_options(context: Option<&str>, connection: Option<&str>, target: Option<&str>) {
    note(|scope| {
        scope.context = context.map(str::to_owned);
        scope.connection = connection.map(str::to_owned);
        scope.target = target.map(str::to_owned);
    });
}

pub(super) fn note_scope(scope: &ResolvedScope, snapshot: &dbgjs::service_api::ContextSnapshot) {
    note(|record| {
        record.context = Some(scope.context.clone());
        record.connection = Some(scope.connection.clone());
        record.target = Some(scope.target.clone());
        record.generation = snapshot
            .connections
            .iter()
            .find(|connection| connection.id == scope.connection)
            .map(|connection| connection.generation);
    });
}

pub(super) async fn enrich_connection(state: &Path, selection_file: &Path, options: &ScopeOptions) {
    let Some(path) = env::var_os("DBGJS_RECORD_SCOPE_FILE") else {
        return;
    };
    let Ok(bytes) = fs::read(path) else {
        return;
    };
    let Ok(scope) = serde_json::from_slice::<Scope>(&bytes) else {
        return;
    };
    if scope.connection.is_none() {
        return;
    }
    let selected = super::load_selection(selection_file).ok();
    let context = options
        .context
        .clone()
        .or_else(|| selected.as_ref()?.context.clone());
    let Some(context) = context else { return };
    let connection = options.connection.clone().or_else(|| {
        selected.as_ref().and_then(|selection| {
            (selection.context.as_deref() == Some(context.as_str()))
                .then(|| selection.connection.clone())
                .flatten()
        })
    });
    let Some(connection) = connection else { return };
    if let Ok(client) = connect_existing(state).await
        && let Ok(snapshot) = client.contexts.get_context(context).await
    {
        note(|scope| {
            scope.generation = snapshot
                .connections
                .iter()
                .find(|candidate| candidate.id == connection)
                .map(|connection| connection.generation);
        });
    }
}

fn note(change: impl FnOnce(&mut Scope)) {
    let Some(path) = env::var_os("DBGJS_RECORD_SCOPE_FILE") else {
        return;
    };
    let Some(manifest_file) = env::var_os("DBGJS_RECORD_MANIFEST_FILE") else {
        eprintln!("dbgjs: recording scope unavailable: no recording policy");
        return;
    };
    let manifest: Manifest = match fs::read(manifest_file)
        .and_then(|bytes| serde_json::from_slice(&bytes).map_err(io::Error::other))
    {
        Ok(manifest) => manifest,
        Err(error) => {
            eprintln!("dbgjs: recording scope unavailable: {error}");
            return;
        }
    };
    let path = PathBuf::from(path);
    let mut scope: Scope = fs::read(&path)
        .ok()
        .and_then(|bytes| serde_json::from_slice(&bytes).ok())
        .unwrap_or_default();
    change(&mut scope);
    for field in [&mut scope.context, &mut scope.connection, &mut scope.target] {
        if let Some(value) = field {
            *value = scrub(value, &manifest.redact);
        }
    }
    if let Err(error) = save_json(&path, &scope) {
        eprintln!("dbgjs: recording scope unavailable: {error}");
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn multiline_and_invalid_utf8_redaction_does_not_expose_authentication() {
        let value = b"\xff\nAuthorization: Bearer hidden secret\n{\"cookie\":\"hidden-cookie\"}\n--token=s3cr3t\n";
        let scrubbed = scrub_bytes(value, &[]);
        assert!(scrubbed.starts_with(b"\xff\n"));
        assert!(!String::from_utf8_lossy(&scrubbed).contains("hidden"));
        assert!(!String::from_utf8_lossy(&scrubbed).contains("s3cr3t"));
        assert!(String::from_utf8_lossy(&scrubbed).contains("[REDACTED]"));
    }

    #[test]
    fn fences_are_longer_than_embedded_markdown_fences() {
        let content = "line 1\n````\nline 2\n";
        assert!(fence(content).len() > 4);
    }
}
