use std::collections::{BTreeMap, BTreeSet};
use std::future::Future;
use std::path::PathBuf;
use std::time::Duration;

use futures_util::future::join_all;
use sha2::{Digest, Sha256};
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};
use tokio::process::Command;

use crate::connection_provider::ConnectionRuntime;
use crate::service_api::{
    AgentSessionSnapshot, ConnectionConfiguration, ProcessRole, ProcessRootKind, ProcessSnapshot,
    ProcessTargetSnapshot, ProcessTreeSnapshot,
};

#[cfg(windows)]
const PROCESS_QUERY_TIMEOUT: Duration = Duration::from_secs(15);
const VSCODE_IPC_TIMEOUT: Duration = Duration::from_secs(5);
const AGENT_SESSION_QUERY_TIMEOUT: Duration = Duration::from_secs(5);
const AGENT_SESSIONS_HELPER: &str = include_str!("providers/agent_sessions.mjs");

#[derive(Clone, Debug, serde::Deserialize)]
#[serde(rename_all = "PascalCase")]
struct WindowsProcess {
    process_id: u32,
    parent_process_id: u32,
    #[serde(default, deserialize_with = "nullable_string")]
    name: String,
    #[serde(default, deserialize_with = "nullable_string")]
    command_line: String,
    #[serde(default, deserialize_with = "nullable_string")]
    creation_date: String,
    #[serde(default, deserialize_with = "nullable_string")]
    executable_path: String,
    #[serde(default)]
    vscode_process_type: Option<String>,
}

#[derive(Debug)]
struct WindowsProcessStats {
    process_id: u32,
    cpu_percent: u32,
    memory_bytes: u64,
}

#[derive(Clone, Debug)]
struct VscodeStatusProcess {
    display_name: String,
    window_id: Option<u32>,
    window_title: Option<String>,
}

#[derive(Debug, serde::Deserialize)]
struct VscodeMainDiagnostics {
    #[serde(rename = "mainPID")]
    main_pid: u32,
    windows: Vec<VscodeWindowDiagnostics>,
    #[serde(rename = "pidToNames")]
    pid_to_names: Vec<VscodeNamedProcess>,
}

#[derive(Debug, serde::Deserialize)]
struct VscodeWindowDiagnostics {
    id: u32,
    pid: u32,
    title: String,
}

#[derive(Debug, serde::Deserialize)]
struct VscodeNamedProcess {
    pid: u32,
    name: String,
}

#[derive(Debug, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
struct AgentSessionProcess {
    process_id: u32,
    sessions: Vec<AgentSessionSnapshot>,
}

fn nullable_string<'de, D>(deserializer: D) -> Result<String, D::Error>
where
    D: serde::Deserializer<'de>,
{
    Ok(<Option<String> as serde::Deserialize>::deserialize(deserializer)?.unwrap_or_default())
}

#[derive(Debug, thiserror::Error)]
pub enum ProcessDiscoveryError {
    #[error("failed to query processes: {0}")]
    Query(#[source] std::io::Error),
    #[error("process query failed: {0}")]
    QueryFailed(String),
    #[error("process query timed out")]
    QueryTimeout,
    #[error("invalid process data: {0}")]
    InvalidData(#[from] serde_json::Error),
    #[error("VS Code main-process IPC failed: {0}")]
    Ipc(String),
}

pub async fn discover_vscode_process_trees(
    include_stats: bool,
) -> Result<Vec<ProcessTreeSnapshot>, ProcessDiscoveryError> {
    discover_process_trees(ProcessRootKind::Vscode, include_stats).await
}

pub async fn discover_process_trees(
    root_kind: ProcessRootKind,
    include_stats: bool,
) -> Result<Vec<ProcessTreeSnapshot>, ProcessDiscoveryError> {
    let (processes, stats) = if include_stats {
        let (processes, stats) =
            tokio::try_join!(query_windows_processes(), query_windows_process_stats())?;
        (processes, Some(stats))
    } else {
        (query_windows_processes().await?, None)
    };
    let process_metadata = processes
        .iter()
        .cloned()
        .map(|process| (process.process_id, process))
        .collect::<BTreeMap<_, _>>();
    let mut trees = process_trees(processes, root_kind);
    if let Some(stats) = stats {
        apply_process_stats(&mut trees, stats);
    }
    enrich_process_trees(&mut trees, &process_metadata).await;
    Ok(trees)
}

pub async fn discover_recognized_process_trees()
-> Result<Vec<ProcessTreeSnapshot>, ProcessDiscoveryError> {
    let processes = query_windows_processes().await?;
    let process_metadata = processes
        .iter()
        .cloned()
        .map(|process| (process.process_id, process))
        .collect::<BTreeMap<_, _>>();
    let mut trees = Vec::new();
    for kind in [
        ProcessRootKind::Vscode,
        ProcessRootKind::Electron,
        ProcessRootKind::Browser,
        ProcessRootKind::Node,
    ] {
        trees.extend(process_trees(processes.clone(), kind));
    }
    enrich_process_trees(&mut trees, &process_metadata).await;
    Ok(trees)
}

pub async fn populate_process_tree_targets(trees: &mut [ProcessTreeSnapshot]) {
    let observations = join_all(trees.iter().map(|tree| {
        let root_process_id = tree.root_process_id;
        let process_ids = tree
            .processes
            .iter()
            .map(|process| process.process_id)
            .collect::<BTreeSet<_>>();
        async move {
            (
                root_process_id,
                discover_process_tree_targets(root_process_id, &process_ids).await,
            )
        }
    }))
    .await
    .into_iter()
    .collect::<BTreeMap<_, _>>();
    for tree in trees {
        match observations
            .get(&tree.root_process_id)
            .expect("every requested process tree has an observation")
        {
            Ok(targets) => tree.targets = targets.clone(),
            Err(error) => tree.target_discovery_error = Some(error.clone()),
        }
        tree.targets_observed = true;
    }
}

async fn enrich_process_trees(
    trees: &mut [ProcessTreeSnapshot],
    process_metadata: &BTreeMap<u32, WindowsProcess>,
) {
    let diagnostics = join_all(
        trees
            .iter()
            .filter(|tree| tree.root_kind == ProcessRootKind::Vscode)
            .cloned()
            .map(|tree| async move {
                (
                    tree.root_process_id,
                    query_vscode_main_diagnostics(
                        &tree,
                        process_metadata.get(&tree.root_process_id),
                    )
                    .await,
                )
            }),
    );
    let agent_sessions = join_all(
        trees
            .iter()
            .filter_map(|tree| agent_session_query(tree, process_metadata))
            .map(tokio::spawn),
    );
    let (diagnostics, agent_sessions) = tokio::join!(diagnostics, agent_sessions);
    for (root_process_id, diagnostics) in diagnostics {
        if let Ok(diagnostics) = diagnostics
            && let Some(tree) = trees
                .iter_mut()
                .find(|tree| tree.root_process_id == root_process_id)
        {
            apply_vscode_diagnostics(tree, diagnostics);
        }
    }

    for (root_process_id, sessions) in agent_sessions
        .into_iter()
        .filter_map(|result| result.ok()?.ok())
    {
        if let Some(tree) = trees
            .iter_mut()
            .find(|tree| tree.root_process_id == root_process_id)
        {
            apply_agent_sessions(tree, sessions);
        }
    }
}

async fn discover_process_tree_targets(
    root_process_id: u32,
    process_ids: &BTreeSet<u32>,
) -> Result<Vec<ProcessTargetSnapshot>, String> {
    let runtime = ConnectionRuntime::connect(
        &ConnectionConfiguration::ProcessTree {
            root_pid: root_process_id,
        },
        1,
    )
    .await
    .map_err(|error| error.to_string())?;
    let result = async {
        let mut observation = runtime
            .refresh_targets()
            .await
            .ok_or_else(|| "process tree has no observable target source".to_owned())?;
        if !runtime.set_target_discovery(true).await {
            return Err("process tree cannot enable target discovery".to_owned());
        }

        let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
        let mut stable_since = tokio::time::Instant::now();
        loop {
            if matches!(
                tokio::time::timeout(Duration::from_millis(100), observation.revisions.changed(),)
                    .await,
                Ok(Ok(()))
            ) {
                observation = runtime
                    .observe_targets()
                    .ok_or_else(|| "process tree target source disappeared".to_owned())?;
                stable_since = tokio::time::Instant::now();
            }
            if stable_since.elapsed() >= Duration::from_millis(500)
                || tokio::time::Instant::now() >= deadline
            {
                break;
            }
        }

        Ok(observation
            .targets
            .into_iter()
            .map(|target| ProcessTargetSnapshot {
                process_id: target.process_id,
                target: target.snapshot,
            })
            .filter(|target| {
                target.process_id.is_none_or(|process_id| {
                    process_ids.contains(&process_id)
                        && !matches!(
                            target.target.target_type.as_str(),
                            "node" | "process" | "browser"
                        )
                })
            })
            .collect())
    }
    .await;
    runtime.set_target_discovery(false).await;
    runtime.close().await;
    result
}

fn agent_session_query(
    tree: &ProcessTreeSnapshot,
    processes: &BTreeMap<u32, WindowsProcess>,
) -> Option<
    impl Future<Output = Result<(u32, Vec<AgentSessionProcess>), ProcessDiscoveryError>> + use<>,
> {
    let agent_host = tree
        .processes
        .iter()
        .find(|process| process.role == ProcessRole::AgentHost)?;
    let executable = processes
        .get(&agent_host.process_id)?
        .executable_path
        .clone();
    if executable.is_empty() {
        return None;
    }
    let copilot_pids = tree
        .processes
        .iter()
        .filter(|process| process.role == ProcessRole::Copilot)
        .map(|process| process.process_id)
        .collect::<Vec<_>>();
    if copilot_pids.is_empty() {
        return None;
    }
    let root_process_id = tree.root_process_id;
    let agent_host_pid = agent_host.process_id;
    Some(async move {
        query_agent_sessions_on_large_stack(executable, agent_host_pid, copilot_pids)
            .await
            .map(|sessions| (root_process_id, sessions))
    })
}

async fn query_agent_sessions_on_large_stack(
    executable: String,
    agent_host_pid: u32,
    copilot_pids: Vec<u32>,
) -> Result<Vec<AgentSessionProcess>, ProcessDiscoveryError> {
    let runtime = tokio::runtime::Handle::current();
    let (sender, receiver) = tokio::sync::oneshot::channel();
    std::thread::Builder::new()
        .name("dbgjs-agent-session-discovery".to_owned())
        .stack_size(16 * 1024 * 1024)
        .spawn(move || {
            let result = runtime.block_on(query_agent_sessions(
                executable,
                agent_host_pid,
                copilot_pids,
            ));
            let _ = sender.send(result);
        })
        .map_err(|error| ProcessDiscoveryError::Ipc(error.to_string()))?;
    receiver
        .await
        .map_err(|_| ProcessDiscoveryError::Ipc("agent session query stopped".to_owned()))?
}

async fn query_agent_sessions(
    executable: String,
    agent_host_pid: u32,
    copilot_pids: Vec<u32>,
) -> Result<Vec<AgentSessionProcess>, ProcessDiscoveryError> {
    let mut command = Command::new(&executable);
    command
        .args(["--input-type=module", "--eval", AGENT_SESSIONS_HELPER])
        .env("ELECTRON_RUN_AS_NODE", "1")
        .env("DBGJS_AGENT_HOST_PID", agent_host_pid.to_string())
        .env(
            "DBGJS_COPILOT_PIDS",
            serde_json::to_string(&copilot_pids).expect("process ids always serialize"),
        )
        .kill_on_drop(true);
    configure_background_command(&mut command);
    let output = tokio::time::timeout(AGENT_SESSION_QUERY_TIMEOUT, command.output())
        .await
        .map_err(|_| ProcessDiscoveryError::Ipc("agent session query timed out".to_owned()))?
        .map_err(ProcessDiscoveryError::Query)?;
    if !output.status.success() {
        return Err(ProcessDiscoveryError::Ipc(format!(
            "agent session query failed: {}",
            String::from_utf8_lossy(&output.stderr).trim()
        )));
    }
    serde_json::from_slice(&output.stdout).map_err(ProcessDiscoveryError::InvalidData)
}

fn apply_agent_sessions(tree: &mut ProcessTreeSnapshot, sessions: Vec<AgentSessionProcess>) {
    for process_sessions in sessions {
        let Some(process) = tree
            .processes
            .iter_mut()
            .find(|process| process.process_id == process_sessions.process_id)
        else {
            continue;
        };
        process.agent_sessions = process_sessions.sessions;
        process.agent_sessions.sort_by(|left, right| {
            left.title
                .cmp(&right.title)
                .then_with(|| left.internal_id.cmp(&right.internal_id))
        });
    }
}

fn apply_process_stats(
    trees: &mut [ProcessTreeSnapshot],
    stats: BTreeMap<u32, WindowsProcessStats>,
) {
    for process in trees.iter_mut().flat_map(|tree| tree.processes.iter_mut()) {
        if let Some(stats) = stats.get(&process.process_id) {
            process.cpu_percent = Some(stats.cpu_percent);
            process.memory_bytes = Some(stats.memory_bytes);
        }
    }
}

async fn query_vscode_main_diagnostics(
    tree: &ProcessTreeSnapshot,
    root: Option<&WindowsProcess>,
) -> Result<VscodeMainDiagnostics, ProcessDiscoveryError> {
    let user_data_path = tree
        .processes
        .iter()
        .find_map(|process| command_argument(&process.command_line, "--user-data-dir"))
        .ok_or_else(|| ProcessDiscoveryError::Ipc("missing --user-data-dir".into()))?;
    let app_path = tree
        .processes
        .iter()
        .find_map(|process| command_argument(&process.command_line, "--app-path"))
        .map(PathBuf::from)
        .or_else(|| root.and_then(|root| vscode_app_path(&root.executable_path)))
        .ok_or_else(|| ProcessDiscoveryError::Ipc("missing --app-path".into()))?;
    let version = vscode_product_version(app_path).await?;
    let handles = vscode_ipc_handles(
        &user_data_path,
        &version,
        std::env::consts::OS,
        std::env::var("XDG_RUNTIME_DIR").ok().as_deref(),
    );
    let mut last_error = ProcessDiscoveryError::Ipc("no main IPC handle".into());
    for handle in handles {
        match query_vscode_main_ipc(&handle).await {
            Ok(diagnostics) if diagnostics.main_pid == tree.root_process_id => {
                return Ok(diagnostics);
            }
            Ok(_) => last_error = ProcessDiscoveryError::Ipc("main IPC PID mismatch".into()),
            Err(error) => last_error = error,
        }
    }
    Err(last_error)
}

fn vscode_app_path(executable: &str) -> Option<PathBuf> {
    let executable = PathBuf::from(executable);
    let directory = executable.parent()?;
    if directory.file_name().is_some_and(|name| name == "MacOS") {
        Some(directory.parent()?.join("Resources").join("app"))
    } else {
        Some(directory.join("resources").join("app"))
    }
}

fn vscode_ipc_handles(
    user_data_path: &str,
    version: &str,
    os: &str,
    runtime_directory: Option<&str>,
) -> Vec<String> {
    // VS Code: src/vs/base/parts/ipc/node/ipc.net.ts, createStaticIPCHandle.
    let scope = format!("{:x}", Sha256::digest(user_data_path.as_bytes()));
    if os == "windows" {
        return vec![format!(r"\\.\pipe\{}-{version}-main-sock", &scope[..8])];
    }
    let version = version.chars().take(4).collect::<String>();
    let mut handles = Vec::new();
    if os != "macos"
        && let Some(directory) = runtime_directory.filter(|directory| !directory.is_empty())
    {
        handles.push(format!(
            "{directory}/vscode-{}-{version}-main.sock",
            &scope[..8]
        ));
    }
    // Portable VS Code ignores XDG_RUNTIME_DIR; diagnostics verify the responding main PID.
    handles.push(format!("{user_data_path}/{version}-main.sock"));
    handles
}

async fn vscode_product_version(app_path: PathBuf) -> Result<String, ProcessDiscoveryError> {
    for file_name in ["product.json", "package.json"] {
        let path = app_path.join(file_name);
        let bytes = match tokio::fs::read(&path).await {
            Ok(bytes) => bytes,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => continue,
            Err(error) => return Err(ProcessDiscoveryError::Ipc(error.to_string())),
        };
        let metadata: serde_json::Value = serde_json::from_slice(&bytes)?;
        if let Some(version) = metadata.get("version").and_then(serde_json::Value::as_str) {
            return Ok(version.to_owned());
        }
    }
    Err(ProcessDiscoveryError::Ipc(
        "VS Code product metadata has no version".into(),
    ))
}

#[cfg(windows)]
fn configure_background_command(command: &mut Command) {
    const CREATE_NO_WINDOW: u32 = 0x0800_0000;
    command.creation_flags(CREATE_NO_WINDOW);
}

#[cfg(not(windows))]
fn configure_background_command(_command: &mut Command) {}

#[cfg(windows)]
async fn query_windows_processes() -> Result<Vec<WindowsProcess>, ProcessDiscoveryError> {
    const CREATE_NO_WINDOW: u32 = 0x0800_0000;
    let script = r#"
        $ErrorActionPreference = "Stop"
        @(Get-CimInstance Win32_Process |
            Select-Object ProcessId,ParentProcessId,Name,CommandLine,CreationDate,ExecutablePath) |
            ConvertTo-Json -Compress
    "#;
    let mut command = Command::new("powershell.exe");
    command
        .args(["-NoProfile", "-NonInteractive", "-Command", script])
        .kill_on_drop(true)
        .creation_flags(CREATE_NO_WINDOW);
    let output = tokio::time::timeout(PROCESS_QUERY_TIMEOUT, command.output())
        .await
        .map_err(|_| ProcessDiscoveryError::QueryTimeout)?
        .map_err(ProcessDiscoveryError::Query)?;
    if !output.status.success() {
        return Err(ProcessDiscoveryError::QueryFailed(
            String::from_utf8_lossy(&output.stderr).trim().to_owned(),
        ));
    }
    Ok(serde_json::from_slice(&output.stdout)?)
}

async fn query_windows_process_stats()
-> Result<BTreeMap<u32, WindowsProcessStats>, ProcessDiscoveryError> {
    tokio::task::spawn_blocking(|| {
        use sysinfo::{MINIMUM_CPU_UPDATE_INTERVAL, ProcessesToUpdate, System};

        let mut system = System::new();
        system.refresh_processes(ProcessesToUpdate::All, true);
        std::thread::sleep(MINIMUM_CPU_UPDATE_INTERVAL);
        system.refresh_processes(ProcessesToUpdate::All, true);
        Ok(system
            .processes()
            .iter()
            .map(|(process_id, process)| {
                let stats = WindowsProcessStats {
                    process_id: process_id.as_u32(),
                    cpu_percent: process.cpu_usage().round().max(0.0) as u32,
                    memory_bytes: process.memory(),
                };
                (stats.process_id, stats)
            })
            .collect())
    })
    .await
    .map_err(|error| ProcessDiscoveryError::QueryFailed(error.to_string()))?
}

#[cfg(not(windows))]
async fn query_windows_processes() -> Result<Vec<WindowsProcess>, ProcessDiscoveryError> {
    tokio::task::spawn_blocking(|| {
        use sysinfo::{ProcessRefreshKind, ProcessesToUpdate, System, UpdateKind};
        let mut system = System::new();
        system.refresh_processes_specifics(
            ProcessesToUpdate::All,
            true,
            ProcessRefreshKind::nothing()
                .with_cmd(UpdateKind::Always)
                .with_environ(UpdateKind::Always)
                .with_exe(UpdateKind::Always)
                .without_tasks(),
        );
        system
            .processes()
            .iter()
            .map(|(pid, process)| {
                let executable_path = process
                    .exe()
                    .map(|path| path.to_string_lossy().into_owned())
                    .unwrap_or_default();
                let name = process
                    .exe()
                    .and_then(|path| path.file_name())
                    .unwrap_or_else(|| process.name())
                    .to_string_lossy()
                    .into_owned();
                WindowsProcess {
                    process_id: pid.as_u32(),
                    parent_process_id: process.parent().map_or(0, |pid| pid.as_u32()),
                    name,
                    command_line: process
                        .cmd()
                        .iter()
                        .map(|argument| {
                            let argument = argument.to_string_lossy();
                            if argument.contains(char::is_whitespace) {
                                format!("\"{argument}\"")
                            } else {
                                argument.into_owned()
                            }
                        })
                        .collect::<Vec<_>>()
                        .join(" "),
                    creation_date: format!("{:020}", process.start_time()),
                    executable_path,
                    vscode_process_type: process.environ().iter().find_map(|entry| {
                        entry.to_str()?
                            .strip_prefix("VSCODE_CRASH_REPORTER_PROCESS_TYPE=")
                            .map(str::to_owned)
                    }),
                }
            })
            .collect()
    })
    .await
    .map_err(|error| ProcessDiscoveryError::QueryFailed(error.to_string()))
}

#[cfg(windows)]
async fn query_vscode_main_ipc(
    pipe_name: &str,
) -> Result<VscodeMainDiagnostics, ProcessDiscoveryError> {
    use tokio::net::windows::named_pipe::ClientOptions;

    let mut pipe = tokio::time::timeout(VSCODE_IPC_TIMEOUT, async {
        loop {
            match ClientOptions::new().open(pipe_name) {
                Ok(pipe) => return Ok(pipe),
                Err(error) if error.raw_os_error() == Some(231) => {
                    tokio::time::sleep(Duration::from_millis(10)).await;
                }
                Err(error) => return Err(error),
            }
        }
    })
    .await
    .map_err(|_| ProcessDiscoveryError::Ipc("connection timed out".into()))?
    .map_err(|error| ProcessDiscoveryError::Ipc(error.to_string()))?;

    query_vscode_diagnostics_stream(&mut pipe).await
}

async fn query_vscode_diagnostics_stream<S>(
    pipe: &mut S,
) -> Result<VscodeMainDiagnostics, ProcessDiscoveryError>
where
    S: AsyncRead + AsyncWrite + Unpin,
{
    write_ipc_frame(pipe, &serialize_ipc_string("dbgjs"))
        .await
        .map_err(|error| ProcessDiscoveryError::Ipc(error.to_string()))?;
    let initialize = read_regular_ipc_frame(pipe)
        .await
        .map_err(|error| ProcessDiscoveryError::Ipc(error.to_string()))?;
    let (header, _) = deserialize_ipc_message(&initialize)?;
    if header
        .as_array()
        .and_then(|header| header.first())
        .and_then(serde_json::Value::as_u64)
        != Some(200)
    {
        return Err(ProcessDiscoveryError::Ipc(
            "main process did not initialize the IPC channel".into(),
        ));
    }

    let request = serialize_ipc_message(
        &serde_json::json!([100, 0, "diagnostics", "getMainDiagnostics"]),
        None,
    )?;
    write_ipc_frame(pipe, &request)
        .await
        .map_err(|error| ProcessDiscoveryError::Ipc(error.to_string()))?;
    loop {
        let response = read_regular_ipc_frame(pipe)
            .await
            .map_err(|error| ProcessDiscoveryError::Ipc(error.to_string()))?;
        let (header, body) = deserialize_ipc_message(&response)?;
        let Some(header) = header.as_array() else {
            continue;
        };
        let response_type = header.first().and_then(serde_json::Value::as_u64);
        let response_id = header.get(1).and_then(serde_json::Value::as_u64);
        if matches!(response_type, Some(202 | 203)) && response_id == Some(0) {
            let message = body
                .get("message")
                .and_then(serde_json::Value::as_str)
                .map(str::to_owned)
                .unwrap_or_else(|| body.to_string());
            return Err(ProcessDiscoveryError::Ipc(message));
        }
        if response_type != Some(201) || response_id != Some(0) {
            continue;
        }
        return serde_json::from_value(body).map_err(ProcessDiscoveryError::InvalidData);
    }
}

#[cfg(unix)]
async fn query_vscode_main_ipc(
    pipe_name: &str,
) -> Result<VscodeMainDiagnostics, ProcessDiscoveryError> {
    let mut pipe = tokio::time::timeout(
        VSCODE_IPC_TIMEOUT,
        tokio::net::UnixStream::connect(pipe_name),
    )
    .await
    .map_err(|_| ProcessDiscoveryError::Ipc("connection timed out".into()))?
    .map_err(|error| ProcessDiscoveryError::Ipc(error.to_string()))?;
    query_vscode_diagnostics_stream(&mut pipe).await
}

async fn write_ipc_frame<W>(writer: &mut W, body: &[u8]) -> std::io::Result<()>
where
    W: AsyncWrite + Unpin,
{
    let mut header = [0; 13];
    header[0] = 1;
    header[9..13].copy_from_slice(&(body.len() as u32).to_be_bytes());
    writer.write_all(&header).await?;
    writer.write_all(body).await?;
    writer.flush().await
}

async fn read_regular_ipc_frame<R>(reader: &mut R) -> std::io::Result<Vec<u8>>
where
    R: AsyncRead + Unpin,
{
    loop {
        let mut header = [0; 13];
        tokio::time::timeout(VSCODE_IPC_TIMEOUT, reader.read_exact(&mut header))
            .await
            .map_err(|_| {
                std::io::Error::new(std::io::ErrorKind::TimedOut, "IPC read timed out")
            })??;
        let length = u32::from_be_bytes(header[9..13].try_into().unwrap()) as usize;
        if length > 64 * 1024 * 1024 {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                "IPC frame exceeds 64 MiB",
            ));
        }
        let mut body = vec![0; length];
        tokio::time::timeout(VSCODE_IPC_TIMEOUT, reader.read_exact(&mut body))
            .await
            .map_err(|_| {
                std::io::Error::new(std::io::ErrorKind::TimedOut, "IPC body read timed out")
            })??;
        if header[0] == 1 {
            return Ok(body);
        }
    }
}

fn serialize_ipc_string(value: &str) -> Vec<u8> {
    let mut result = vec![1];
    write_vql(&mut result, value.len());
    result.extend_from_slice(value.as_bytes());
    result
}

fn serialize_ipc_message(
    header: &serde_json::Value,
    body: Option<&serde_json::Value>,
) -> Result<Vec<u8>, ProcessDiscoveryError> {
    let mut result = Vec::new();
    serialize_ipc_value(&mut result, header)?;
    match body {
        Some(body) => serialize_ipc_value(&mut result, body)?,
        None => result.push(0),
    }
    Ok(result)
}

fn serialize_ipc_value(
    output: &mut Vec<u8>,
    value: &serde_json::Value,
) -> Result<(), ProcessDiscoveryError> {
    match value {
        serde_json::Value::Null => output.push(0),
        serde_json::Value::String(value) => {
            output.push(1);
            write_vql(output, value.len());
            output.extend_from_slice(value.as_bytes());
        }
        serde_json::Value::Array(values) => {
            output.push(4);
            write_vql(output, values.len());
            for value in values {
                serialize_ipc_value(output, value)?;
            }
        }
        serde_json::Value::Number(value)
            if value.as_u64().is_some_and(|value| value <= i32::MAX as u64) =>
        {
            output.push(6);
            write_vql(output, value.as_u64().unwrap() as usize);
        }
        _ => {
            let value = serde_json::to_vec(value)?;
            output.push(5);
            write_vql(output, value.len());
            output.extend_from_slice(&value);
        }
    }
    Ok(())
}

fn write_vql(output: &mut Vec<u8>, mut value: usize) {
    loop {
        let mut byte = (value & 0x7f) as u8;
        value >>= 7;
        if value != 0 {
            byte |= 0x80;
        }
        output.push(byte);
        if value == 0 {
            break;
        }
    }
}

fn deserialize_ipc_message(
    message: &[u8],
) -> Result<(serde_json::Value, serde_json::Value), ProcessDiscoveryError> {
    let mut offset = 0;
    let header = deserialize_ipc_value(message, &mut offset)?;
    let body = deserialize_ipc_value(message, &mut offset)?;
    Ok((header, body))
}

fn deserialize_ipc_value(
    input: &[u8],
    offset: &mut usize,
) -> Result<serde_json::Value, ProcessDiscoveryError> {
    let data_type = read_byte(input, offset)?;
    match data_type {
        0 => Ok(serde_json::Value::Null),
        1 => {
            let bytes = read_ipc_bytes(input, offset)?;
            let value = std::str::from_utf8(bytes)
                .map_err(|error| ProcessDiscoveryError::Ipc(error.to_string()))?;
            Ok(serde_json::Value::String(value.to_owned()))
        }
        4 => {
            let length = read_vql(input, offset)?;
            let mut values = Vec::with_capacity(length);
            for _ in 0..length {
                values.push(deserialize_ipc_value(input, offset)?);
            }
            Ok(serde_json::Value::Array(values))
        }
        5 => {
            let bytes = read_ipc_bytes(input, offset)?;
            serde_json::from_slice(bytes).map_err(ProcessDiscoveryError::InvalidData)
        }
        6 => Ok(serde_json::Value::from(read_vql(input, offset)?)),
        value => Err(ProcessDiscoveryError::Ipc(format!(
            "unsupported IPC data type {value}"
        ))),
    }
}

fn read_ipc_bytes<'a>(
    input: &'a [u8],
    offset: &mut usize,
) -> Result<&'a [u8], ProcessDiscoveryError> {
    let length = read_vql(input, offset)?;
    let end = offset
        .checked_add(length)
        .filter(|end| *end <= input.len())
        .ok_or_else(|| ProcessDiscoveryError::Ipc("truncated IPC value".into()))?;
    let result = &input[*offset..end];
    *offset = end;
    Ok(result)
}

fn read_vql(input: &[u8], offset: &mut usize) -> Result<usize, ProcessDiscoveryError> {
    let mut result = 0usize;
    for shift in (0..usize::BITS).step_by(7) {
        let byte = read_byte(input, offset)?;
        result |= ((byte & 0x7f) as usize) << shift;
        if byte & 0x80 == 0 {
            return Ok(result);
        }
    }
    Err(ProcessDiscoveryError::Ipc("invalid IPC integer".into()))
}

fn read_byte(input: &[u8], offset: &mut usize) -> Result<u8, ProcessDiscoveryError> {
    let byte = input
        .get(*offset)
        .copied()
        .ok_or_else(|| ProcessDiscoveryError::Ipc("truncated IPC value".into()))?;
    *offset += 1;
    Ok(byte)
}

fn command_argument(command_line: &str, name: &str) -> Option<String> {
    let start = command_line.find(name)?;
    let whole_argument_is_quoted = command_line[..start].ends_with('"');
    let mut value = &command_line[start + name.len()..];
    value = value
        .strip_prefix('=')
        .unwrap_or_else(|| value.trim_start());
    if let Some(value) = value.strip_prefix('"') {
        return value.split_once('"').map(|(value, _)| value.to_owned());
    }
    if whole_argument_is_quoted {
        return value.split_once('"').map(|(value, _)| value.to_owned());
    }
    value
        .split_ascii_whitespace()
        .next()
        .filter(|value| !value.is_empty())
        .map(str::to_owned)
}

fn parse_vscode_status_label(label: &str) -> (String, Option<u32>, Option<String>) {
    if let Some(window) = label.strip_prefix("window [")
        && let Some((window_id, title)) = window.split_once("] (")
        && let Ok(window_id) = window_id.parse::<u32>()
    {
        return (
            "renderer".into(),
            Some(window_id),
            Some(title.strip_suffix(')').unwrap_or(title).to_owned()),
        );
    }
    if let Some((name, window_id)) = label.rsplit_once(" [")
        && let Ok(window_id) = window_id
            .strip_suffix(']')
            .unwrap_or(window_id)
            .parse::<u32>()
    {
        return (name.to_owned(), Some(window_id), None);
    }
    (label.to_owned(), None, None)
}

#[cfg(test)]
fn vscode_process_trees(processes: Vec<WindowsProcess>) -> Vec<ProcessTreeSnapshot> {
    process_trees(processes, ProcessRootKind::Vscode)
}

fn process_trees(
    processes: Vec<WindowsProcess>,
    root_kind: ProcessRootKind,
) -> Vec<ProcessTreeSnapshot> {
    let by_pid = processes
        .iter()
        .map(|process| (process.process_id, process))
        .collect::<BTreeMap<_, _>>();
    let mut children = BTreeMap::<u32, Vec<u32>>::new();
    for process in &processes {
        if let Some(parent) = by_pid.get(&process.parent_process_id)
            && !can_be_process_child(parent, process)
        {
            continue;
        }
        children
            .entry(process.parent_process_id)
            .or_default()
            .push(process.process_id);
    }
    for child_ids in children.values_mut() {
        child_ids.sort_unstable();
    }

    let candidates = processes
        .iter()
        .filter(|process| process_root_kind(process, &by_pid, &children) == Some(root_kind))
        .map(|process| process.process_id)
        .collect::<BTreeSet<_>>();
    let mut roots = candidates
        .iter()
        .copied()
        .filter(|process_id| {
            root_kind != ProcessRootKind::Node
                || !has_candidate_ancestor(*process_id, &candidates, &by_pid)
        })
        .collect::<Vec<_>>();
    roots.sort_unstable();

    roots
        .into_iter()
        .map(|root_pid| {
            let mut snapshots = Vec::new();
            append_process_tree(
                root_pid,
                root_pid,
                None,
                root_kind,
                &by_pid,
                &children,
                &mut BTreeSet::new(),
                &mut snapshots,
            );
            ProcessTreeSnapshot {
                root_process_id: root_pid,
                root_kind,
                processes: snapshots,
                runtime_metadata_available: false,
                targets: Vec::new(),
                targets_observed: false,
                target_discovery_error: None,
            }
        })
        .collect()
}

fn append_process_tree(
    process_id: u32,
    root_process_id: u32,
    parent_process_id: Option<u32>,
    root_kind: ProcessRootKind,
    processes: &BTreeMap<u32, &WindowsProcess>,
    children: &BTreeMap<u32, Vec<u32>>,
    visited: &mut BTreeSet<u32>,
    snapshots: &mut Vec<ProcessSnapshot>,
) {
    if !visited.insert(process_id) {
        return;
    }
    let Some(process) = processes.get(&process_id) else {
        return;
    };
    if root_kind != ProcessRootKind::Node
        && process_id != root_process_id
        && process_root_kind(process, processes, children) == Some(root_kind)
    {
        return;
    }
    if process
        .command_line
        .to_ascii_lowercase()
        .contains("process._debugprocess(")
    {
        return;
    }
    let attachable = process_id == root_process_id
        || is_node_process(process)
        || matches!(
            root_kind,
            ProcessRootKind::Vscode | ProcessRootKind::Electron
        ) && is_renderer_process(process);
    snapshots.push(ProcessSnapshot {
        process_id,
        parent_process_id,
        attachable,
        debug_target_id: attachable.then(|| {
            if process_id == root_process_id {
                "$node-root".to_owned()
            } else {
                format!(
                    "process-{process_id}-{}",
                    process_instance_id(&process.creation_date)
                )
            }
        }),
        name: process.name.clone(),
        command_line: process.command_line.clone(),
        creation_date: process.creation_date.clone(),
        role: if process_id == root_process_id {
            match root_kind {
                ProcessRootKind::Vscode => ProcessRole::VscodeMain,
                ProcessRootKind::Node => ProcessRole::Node,
                ProcessRootKind::Electron => ProcessRole::ElectronMain,
                ProcessRootKind::Browser => ProcessRole::BrowserMain,
            }
        } else if attachable {
            process_role(process)
        } else {
            non_javascript_process_role(process)
        },
        display_name: None,
        window_id: None,
        window_title: None,
        cpu_percent: None,
        memory_bytes: None,
        agent_sessions: Vec::new(),
    });
    let next_parent = Some(process_id);
    for child_id in children.get(&process_id).into_iter().flatten() {
        append_process_tree(
            *child_id,
            root_process_id,
            next_parent,
            root_kind,
            processes,
            children,
            visited,
            snapshots,
        );
    }

    fn process_instance_id(creation_date: &str) -> String {
        let value = creation_date
            .chars()
            .filter(|character| {
                character.is_ascii_alphanumeric() || matches!(character, '_' | '.' | '-')
            })
            .collect::<String>();
        if value.is_empty() {
            "unknown".to_owned()
        } else {
            value
        }
    }
}

fn has_candidate_ancestor(
    process_id: u32,
    candidates: &BTreeSet<u32>,
    processes: &BTreeMap<u32, &WindowsProcess>,
) -> bool {
    let mut visited = BTreeSet::new();
    let mut child = processes.get(&process_id).copied();
    while let Some(current_child) = child {
        let current = current_child.parent_process_id;
        if !visited.insert(current) {
            return false;
        }
        let Some(parent) = processes.get(&current).copied() else {
            return false;
        };
        if !can_be_process_child(parent, current_child) {
            return false;
        }
        if candidates.contains(&current) {
            return true;
        }
        child = Some(parent);
    }
    false
}

fn can_be_process_child(parent: &WindowsProcess, child: &WindowsProcess) -> bool {
    parent.creation_date.is_empty()
        || child.creation_date.is_empty()
        || parent.creation_date <= child.creation_date
}

fn apply_vscode_diagnostics(tree: &mut ProcessTreeSnapshot, diagnostics: VscodeMainDiagnostics) {
    if diagnostics.main_pid != tree.root_process_id {
        return;
    }
    let mut status = diagnostics
        .windows
        .into_iter()
        .map(|window| {
            (
                window.pid,
                VscodeStatusProcess {
                    display_name: "renderer".into(),
                    window_id: Some(window.id),
                    window_title: Some(window.title),
                },
            )
        })
        .collect::<BTreeMap<_, _>>();
    for process in diagnostics.pid_to_names {
        let (display_name, window_id, window_title) = parse_vscode_status_label(&process.name);
        let incoming = VscodeStatusProcess {
            display_name,
            window_id,
            window_title,
        };
        status
            .entry(process.pid)
            .and_modify(|current| {
                current.display_name.clone_from(&incoming.display_name);
                if incoming.window_id.is_some() {
                    current.window_id = incoming.window_id;
                }
                if incoming.window_title.is_some() {
                    current.window_title.clone_from(&incoming.window_title);
                }
            })
            .or_insert(incoming);
    }
    let windows = status
        .values()
        .filter_map(|process| Some((process.window_id?, process.window_title.as_ref()?.clone())))
        .collect::<BTreeMap<_, _>>();
    let window_configs = tree
        .processes
        .iter()
        .filter_map(|process| {
            let window_id = status.get(&process.process_id)?.window_id?;
            let config = command_argument(&process.command_line, "--vscode-window-config")?;
            Some((config, window_id))
        })
        .collect::<BTreeMap<_, _>>();
    let mut inherited_windows = BTreeMap::<u32, u32>::new();
    for process in &mut tree.processes {
        let status_process = status.get(&process.process_id);
        if let Some(status_process) = status_process
            && is_status_display_name(&status_process.display_name)
        {
            process.display_name = Some(status_process.display_name.clone());
            process.role =
                role_from_status_name(&status_process.display_name, process.role.clone());
        }
        let window_id = status_process
            .and_then(|process| process.window_id)
            .or_else(|| {
                command_argument(&process.command_line, "--vscode-window-config")
                    .and_then(|config| window_configs.get(&config).copied())
            })
            .or_else(|| {
                process
                    .parent_process_id
                    .and_then(|parent_id| inherited_windows.get(&parent_id).copied())
            });
        if let Some(window_id) = window_id {
            process.window_id = Some(window_id);
            process.window_title = windows.get(&window_id).cloned();
            inherited_windows.insert(process.process_id, window_id);
        }
    }
    tree.runtime_metadata_available = true;
}

fn is_status_display_name(name: &str) -> bool {
    !name.starts_with('"')
        && !name.contains(":\\")
        && !name.starts_with("electron-nodejs (")
        && name.len() <= 80
}

fn role_from_status_name(name: &str, fallback: ProcessRole) -> ProcessRole {
    match name {
        "extension-host" => ProcessRole::ExtensionHost,
        "file-watcher" | "fileWatcher" => ProcessRole::FileWatcher,
        "pty-host" | "ptyHost" => ProcessRole::PtyHost,
        "agent-host" | "agentHost" => ProcessRole::AgentHost,
        "renderer" => ProcessRole::Renderer,
        _ => fallback,
    }
}

fn process_root_kind(
    process: &WindowsProcess,
    processes: &BTreeMap<u32, &WindowsProcess>,
    children: &BTreeMap<u32, Vec<u32>>,
) -> Option<ProcessRootKind> {
    if is_vscode_main_candidate(process) {
        Some(ProcessRootKind::Vscode)
    } else if is_browser_main_candidate(process) {
        Some(ProcessRootKind::Browser)
    } else if is_electron_main_candidate(process, processes, children) {
        Some(ProcessRootKind::Electron)
    } else if is_node_process(process) {
        Some(ProcessRootKind::Node)
    } else {
        None
    }
}

fn is_browser_main_candidate(process: &WindowsProcess) -> bool {
    let name = process.name.to_ascii_lowercase();
    let command = process.command_line.to_ascii_lowercase();
    !command.contains("--type=")
        && matches!(
            name.as_str(),
            "chrome.exe"
                | "chrome-headless-shell.exe"
                | "chromium.exe"
                | "msedge.exe"
                | "brave.exe"
                | "brave-browser.exe"
                | "opera.exe"
        )
}

fn is_electron_main_candidate(
    process: &WindowsProcess,
    processes: &BTreeMap<u32, &WindowsProcess>,
    children: &BTreeMap<u32, Vec<u32>>,
) -> bool {
    let name = process.name.to_ascii_lowercase();
    let command = process.command_line.to_ascii_lowercase();
    if command.contains("--type=") || command.contains("--ms-enable-electron-run-as-node") {
        return false;
    }
    name == "electron.exe"
        || children
            .get(&process.process_id)
            .into_iter()
            .flatten()
            .filter_map(|child| processes.get(child))
            .any(|child| {
                child.name.eq_ignore_ascii_case(&process.name) && is_renderer_process(child)
            })
}

fn is_renderer_process(process: &WindowsProcess) -> bool {
    process
        .command_line
        .to_ascii_lowercase()
        .contains("--type=renderer")
}

fn is_node_process(process: &WindowsProcess) -> bool {
    let name = process.name.to_ascii_lowercase();
    let command = process.command_line.to_ascii_lowercase();
    !command.contains("process._debugprocess(")
        && (matches!(name.as_str(), "node.exe" | "node" | "nodejs")
            || command.contains("node.mojom.nodeservice")
            || command.contains("--node-ipc")
            || command.contains("bootstrap-fork")
            || command.contains("tsserver.js")
            || command.contains("typingsinstaller.js"))
}

fn is_attachable_vscode_process(process: &WindowsProcess) -> bool {
    let command = process.command_line.to_ascii_lowercase();
    !command.contains("process._debugprocess(")
        && (is_renderer_process(process)
            || is_node_process(process)
            || is_packaged_vscode_executable(&process.name.to_ascii_lowercase())
                && command.contains("--stdio"))
}

fn is_vscode_main_candidate(process: &WindowsProcess) -> bool {
    let name = process.name.to_ascii_lowercase();
    let command = process.command_line.to_ascii_lowercase();
    let executable = process.executable_path.to_ascii_lowercase();
    let packaged = is_packaged_vscode_executable(&name)
        || name == "electron" && [
            "/visual studio code.app/contents/macos/",
            "/visual studio code - insiders.app/contents/macos/",
            "/code - oss.app/contents/macos/",
            "/vscodium.app/contents/macos/",
            "/cursor.app/contents/macos/",
            "/windsurf.app/contents/macos/",
        ].iter().any(|bundle| executable.contains(bundle));
    let source_build = matches!(name.as_str(), "electron.exe" | "electron")
        && (command.contains(r"\vscode\") || command.contains("/vscode/"))
        && (command.contains(r"\out\main.js") || command.contains("/out/main.js"));
    ((packaged && !is_attachable_vscode_process(process)) || source_build)
        && !command.contains("--type=")
        && !command.contains("--ms-enable-electron-run-as-node")
        && !command.contains(r"\out\cli.js")
        && !command.contains("/out/cli.js")
}

fn is_packaged_vscode_executable(name: &str) -> bool {
    matches!(
        name,
        "code.exe"
            | "code - insiders.exe"
            | "code - oss.exe"
            | "code-oss.exe"
            | "codium.exe"
            | "vscodium.exe"
            | "cursor.exe"
            | "windsurf.exe"
            | "code"
            | "code-insiders"
            | "code - insiders"
            | "code - oss"
            | "code-oss"
            | "codium"
            | "vscodium"
            | "cursor"
            | "windsurf"
    )
}

fn process_role(process: &WindowsProcess) -> ProcessRole {
    let command = process.command_line.to_ascii_lowercase();
    if command.contains("--type=renderer") {
        ProcessRole::Renderer
    } else if command.contains("tsserver.js") {
        ProcessRole::TypeScriptServer
    } else if command.contains("typingsinstaller.js") {
        ProcessRole::TypeScriptInstaller
    } else if command.contains("language-server")
        || command.contains("languageserver")
        || command.contains("eslintserver")
    {
        ProcessRole::LanguageServer
    } else if command.contains("extensionhost")
        || command.contains("node.mojom.nodeservice")
            && (command.contains("--inspect-port=0")
                || process.vscode_process_type.as_deref() == Some("extensionHost"))
    {
        ProcessRole::ExtensionHost
    } else if command.contains("ptyhost") {
        ProcessRole::PtyHost
    } else if command.contains("filewatcher") || command.contains("watcherservice") {
        ProcessRole::FileWatcher
    } else if command.contains("agenthost") {
        ProcessRole::AgentHost
    } else if command.contains("copilot") {
        ProcessRole::Copilot
    } else if command.contains("node.mojom.nodeservice") {
        ProcessRole::NodeUtility
    } else {
        ProcessRole::Node
    }
}

fn non_javascript_process_role(process: &WindowsProcess) -> ProcessRole {
    let name = process.name.to_ascii_lowercase();
    let command = process.command_line.to_ascii_lowercase();
    if command.contains("copilot") {
        ProcessRole::Copilot
    } else if name == "claude.exe" || command.contains("claude") {
        ProcessRole::Claude
    } else if name == "codex.exe" || command.contains("codex") {
        ProcessRole::Codex
    } else if command.contains("--type=gpu-process") {
        ProcessRole::Gpu
    } else if command.contains("network.mojom.networkservice") {
        ProcessRole::NetworkService
    } else if command.contains("audio.mojom.audioservice") {
        ProcessRole::AudioService
    } else if command.contains("--type=crashpad-handler") {
        ProcessRole::Crashpad
    } else if command.contains("--type=utility") {
        ProcessRole::Utility
    } else {
        ProcessRole::Other
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn derives_platform_specific_vscode_ipc_handles() {
        assert_eq!(
            vscode_ipc_handles("abc", "1.104.2", "windows", Some("/run/user/1000")),
            vec![r"\\.\pipe\ba7816bf-1.104.2-main-sock"]
        );
        assert_eq!(
            vscode_ipc_handles("abc", "1.104.2", "linux", Some("/run/user/1000")),
            vec!["/run/user/1000/vscode-ba7816bf-1.10-main.sock", "abc/1.10-main.sock"]
        );
        assert_eq!(
            vscode_ipc_handles("abc", "1.104.2", "macos", Some("/run/user/1000")),
            vec!["abc/1.10-main.sock"]
        );
        assert_eq!(
            vscode_ipc_handles("abc", "1.104.2", "linux", None),
            vec!["abc/1.10-main.sock"]
        );
    }

    #[test]
    fn recognizes_unix_vscode_main_and_extension_host() {
        for (name, executable) in [
            ("code", "/opt/VS Code/code"),
            ("code-insiders", "/opt/code-insiders/code-insiders"),
            ("Electron", "/Applications/Visual Studio Code.app/Contents/MacOS/Electron"),
            ("Electron", "/Applications/Visual Studio Code - Insiders.app/Contents/MacOS/Electron"),
        ] {
            let mut main = process(10, 1, name, "--user-data-dir=/home/user/profile");
            main.executable_path = executable.into();
            let host = process(11, 10, "Code Helper (Plugin)",
                "--type=utility --utility-sub-type=node.mojom.NodeService --inspect-port=0");
            let trees = vscode_process_trees(vec![main, host]);
            assert_eq!(trees.len(), 1, "{executable}");
            assert_eq!(trees[0].processes[0].role, ProcessRole::VscodeMain);
            assert_eq!(trees[0].processes[1].role, ProcessRole::ExtensionHost);
            assert!(trees[0].processes[1].attachable);
        }
        let mut other = process(20, 1, "Electron", "");
        other.executable_path = "/Applications/Other.app/Contents/MacOS/Electron".into();
        assert!(!is_vscode_main_candidate(&other));
        assert!(!is_vscode_main_candidate(&process(21, 1, "code", "--type=zygote")));
        assert!(!is_vscode_main_candidate(&process(22, 1, "code", "/opt/code/resources/app/out/cli.js")));
        assert!(is_node_process(&process(23, 1, "node", "node server.js")));
    }

    #[test]
    fn recognizes_inspected_utility_extension_hosts_from_vscode_metadata() {
        let mut host = process(
            11,
            10,
            "Code Helper (Plugin)",
            "--type=utility --utility-sub-type=node.mojom.NodeService --nolazy --inspect=127.0.0.1:45678",
        );
        assert_eq!(process_role(&host), ProcessRole::NodeUtility);
        host.vscode_process_type = Some("extensionHost".into());
        assert!(is_node_process(&host));
        assert_eq!(process_role(&host), ProcessRole::ExtensionHost);
        let mut inherited_environment = process(12, 11, "node", "node server.js");
        inherited_environment.vscode_process_type = host.vscode_process_type;
        assert_eq!(process_role(&inherited_environment), ProcessRole::Node);
    }

    #[test]
    fn finds_packaged_vscode_metadata_without_app_path_switch() {
        assert_eq!(
            vscode_app_path("/Applications/Visual Studio Code.app/Contents/MacOS/Electron"),
            Some(PathBuf::from("/Applications/Visual Studio Code.app/Contents/Resources/app"))
        );
        assert_eq!(
            vscode_app_path("/opt/code/code"),
            Some(PathBuf::from("/opt/code/resources/app"))
        );
        assert_eq!(
            command_argument(r#"code "--user-data-dir=/home/user/Code Profile""#, "--user-data-dir"),
            Some("/home/user/Code Profile".into())
        );
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn discovers_current_unix_process_metadata() {
        let processes = query_windows_processes().await.unwrap();
        let current = processes.iter().find(|process| process.process_id == std::process::id())
            .expect("the current process must be observable");
        assert!(!current.name.is_empty());
        assert!(!current.command_line.is_empty());
        assert_eq!(current.creation_date.len(), 20);
        assert!(current.creation_date.parse::<u64>().unwrap() > 0);
        assert!(!current.executable_path.is_empty());
        let output = Command::new("node")
            .args([
                "-e",
                "const {execFileSync}=require('node:child_process'); const started=execFileSync('ps',['-p',process.argv[1],'-o','lstart='],{env:{...process.env,LC_ALL:'C'},encoding:'utf8'}).trim(); process.stdout.write(String(Date.parse(started)/1000).padStart(20,'0'));",
                &std::process::id().to_string(),
            ])
            .output()
            .await
            .unwrap();
        assert!(output.status.success(), "{}", String::from_utf8_lossy(&output.stderr));
        assert_eq!(current.creation_date, String::from_utf8(output.stdout).unwrap());
    }

    #[tokio::test]
    async fn queries_diagnostics_over_a_platform_independent_stream() {
        let (mut client, mut server) = tokio::io::duplex(4096);
        let server = tokio::spawn(async move {
            assert_eq!(
                read_regular_ipc_frame(&mut server).await.unwrap(),
                serialize_ipc_string("dbgjs")
            );
            write_ipc_frame(
                &mut server,
                &serialize_ipc_message(&serde_json::json!([200]), None).unwrap(),
            ).await.unwrap();
            let request = read_regular_ipc_frame(&mut server).await.unwrap();
            assert_eq!(
                deserialize_ipc_message(&request).unwrap().0,
                serde_json::json!([100, 0, "diagnostics", "getMainDiagnostics"])
            );
            write_ipc_frame(
                &mut server,
                &serialize_ipc_message(
                    &serde_json::json!([201, 0]),
                    Some(&serde_json::json!({
                        "mainPID": 42,
                        "windows": [],
                        "pidToNames": [{ "pid": 43, "name": "extension-host" }]
                    })),
                ).unwrap(),
            ).await.unwrap();
        });
        let diagnostics = query_vscode_diagnostics_stream(&mut client).await.unwrap();
        assert_eq!(diagnostics.main_pid, 42);
        assert_eq!(diagnostics.pid_to_names[0].pid, 43);
        assert_eq!(diagnostics.pid_to_names[0].name, "extension-host");
        server.await.unwrap();
    }

    #[test]
    fn parses_vscode_window_and_utility_ownership() {
        assert_eq!(
            parse_vscode_status_label("window [3] (dbgjs - out.txt)"),
            (
                "renderer".into(),
                Some(3),
                Some("dbgjs - out.txt".into())
            )
        );
        assert_eq!(
            parse_vscode_status_label("extension-host [3]"),
            ("extension-host".into(), Some(3), None)
        );
    }

    #[test]
    fn serializes_and_deserializes_vscode_ipc_messages() {
        let message = serialize_ipc_message(
            &serde_json::json!([100, 0, "diagnostics", "getMainDiagnostics"]),
            None,
        )
        .unwrap();
        let (header, body) = deserialize_ipc_message(&message).unwrap();
        assert_eq!(
            header,
            serde_json::json!([100, 0, "diagnostics", "getMainDiagnostics"])
        );
        assert_eq!(body, serde_json::Value::Null);
    }

    #[test]
    fn extracts_quoted_electron_switches() {
        let command = r#""Code.exe" --type=renderer --user-data-dir="C:\Data\Code Stable" --app-path="C:\Apps\VS Code\resources\app""#;
        assert_eq!(
            command_argument(command, "--user-data-dir").as_deref(),
            Some(r"C:\Data\Code Stable")
        );
        assert_eq!(
            command_argument(command, "--app-path").as_deref(),
            Some(r"C:\Apps\VS Code\resources\app")
        );
        assert_eq!(
            command_argument(
                r#""Code.exe" "--user-data-dir=C:\Data\Alternate Profile""#,
                "--user-data-dir"
            )
            .as_deref(),
            Some(r"C:\Data\Alternate Profile")
        );
    }

    #[test]
    fn preserves_browser_view_renderers_missing_from_window_diagnostics() {
        let mut tree = vscode_process_trees(vec![
            process(10, 1, "Code.exe", r#""Code.exe""#),
            process(
                11,
                10,
                "Code.exe",
                r#""Code.exe" --type=renderer --renderer-client-id=1"#,
            ),
            process(
                12,
                10,
                "Code.exe",
                r#""Code.exe" --type=renderer --renderer-client-id=2"#,
            ),
        ])
        .remove(0);
        apply_vscode_diagnostics(
            &mut tree,
            VscodeMainDiagnostics {
                main_pid: 10,
                windows: vec![VscodeWindowDiagnostics {
                    id: 3,
                    pid: 11,
                    title: "Workbench".into(),
                }],
                pid_to_names: Vec::new(),
            },
        );

        let browser_view = tree
            .processes
            .iter()
            .find(|process| process.process_id == 12)
            .expect("browser view renderer remains discoverable");
        assert_eq!(browser_view.role, ProcessRole::Renderer);
        assert_eq!(browser_view.window_id, None);
    }

    #[test]
    fn preserves_renderer_window_when_pid_name_is_less_specific() {
        let mut tree = vscode_process_trees(vec![
            process(10, 1, "Code.exe", r#""Code.exe""#),
            process(
                11,
                10,
                "Code.exe",
                r#""Code.exe" --type=renderer --renderer-client-id=1"#,
            ),
        ])
        .remove(0);
        apply_vscode_diagnostics(
            &mut tree,
            VscodeMainDiagnostics {
                main_pid: 10,
                windows: vec![VscodeWindowDiagnostics {
                    id: 3,
                    pid: 11,
                    title: "Workbench".into(),
                }],
                pid_to_names: vec![VscodeNamedProcess {
                    pid: 11,
                    name: "renderer".into(),
                }],
            },
        );

        let renderer = tree
            .processes
            .iter()
            .find(|process| process.process_id == 11)
            .expect("renderer remains discoverable");
        assert_eq!(renderer.display_name.as_deref(), Some("renderer"));
        assert_eq!(renderer.window_id, Some(3));
        assert_eq!(renderer.window_title.as_deref(), Some("Workbench"));
    }

    #[test]
    fn associates_renderers_that_share_a_vscode_window_config() {
        let mut tree = vscode_process_trees(vec![
            process(10, 1, "Code.exe", r#""Code.exe""#),
            process(
                11,
                10,
                "Code.exe",
                r#""Code.exe" --type=renderer --vscode-window-config=vscode:window-a"#,
            ),
            process(
                12,
                10,
                "Code.exe",
                r#""Code.exe" --type=renderer --vscode-window-config=vscode:window-a"#,
            ),
        ])
        .remove(0);
        apply_vscode_diagnostics(
            &mut tree,
            VscodeMainDiagnostics {
                main_pid: 10,
                windows: vec![VscodeWindowDiagnostics {
                    id: 3,
                    pid: 11,
                    title: "Workbench".into(),
                }],
                pid_to_names: vec![VscodeNamedProcess {
                    pid: 12,
                    name: "window".into(),
                }],
            },
        );

        for process_id in [11, 12] {
            let renderer = tree
                .processes
                .iter()
                .find(|process| process.process_id == process_id)
                .expect("renderer remains discoverable");
            assert_eq!(renderer.window_id, Some(3));
            assert_eq!(renderer.window_title.as_deref(), Some("Workbench"));
        }
    }

    #[test]
    fn finds_top_level_vscode_roots_and_preserves_non_javascript_ancestry() {
        let trees = vscode_process_trees(vec![
            process(10, 1, "Code - Insiders.exe", r#""Code - Insiders.exe""#),
            process(
                11,
                10,
                "Code - Insiders.exe",
                r#""Code - Insiders.exe" --type=renderer"#,
            ),
            process(12, 11, "node.exe", r#""node.exe" "C:\server\tsserver.js""#),
            process(13, 12, "rustup.exe", "rustup.exe"),
            process(14, 13, "node.exe", r#""node.exe" "C:\nested\server.js""#),
            process(20, 14, "Code.exe", r#""Code.exe" --user-data-dir C:\other"#),
            process(
                21,
                20,
                "Code.exe",
                r#""Code.exe" --type=renderer --user-data-dir=C:\other"#,
            ),
            process(
                22,
                21,
                "Code.exe",
                r#""Code.exe" "C:\resources\app\node_modules\agent\index.js" --stdio"#,
            ),
        ]);

        assert_eq!(trees.len(), 2);
        assert_eq!(
            trees[0].processes[1].debug_target_id.as_deref(),
            Some("process-11-20260823000000.000000000")
        );
        assert_eq!(
            trees[0]
                .processes
                .iter()
                .map(|process| process.process_id)
                .collect::<Vec<_>>(),
            vec![10, 11, 12, 13, 14]
        );
        assert!(!trees[0].processes[3].attachable);
        assert_eq!(trees[0].processes[3].role, ProcessRole::Other);
        assert_eq!(trees[0].processes[4].parent_process_id, Some(13));
        assert_eq!(
            trees[1]
                .processes
                .iter()
                .map(|process| process.process_id)
                .collect::<Vec<_>>(),
            vec![20, 21, 22]
        );
        assert_eq!(trees[1].processes[2].parent_process_id, Some(21));
    }

    #[test]
    fn recognizes_non_attachable_copilot_processes() {
        let process = process(
            11,
            10,
            "Code - Insiders.exe",
            r#""Code - Insiders.exe" "C:\resources\app\node_modules.asar.unpacked\@github\copilot-win32-x64\index.js" --headless --stdio"#,
        );

        assert_eq!(non_javascript_process_role(&process), ProcessRole::Copilot);
    }

    #[test]
    fn recognizes_source_builds() {
        let trees = vscode_process_trees(vec![process(
            40,
            1,
            "electron.exe",
            r#""electron.exe" D:\dev\microsoft\vscode\out\main.js"#,
        )]);
        assert_eq!(trees.len(), 1);
        assert_eq!(trees[0].root_process_id, 40);
    }

    #[test]
    fn recognizes_code_oss() {
        let trees = vscode_process_trees(vec![process(
            50,
            1,
            "Code - OSS.exe",
            r#""Code - OSS.exe""#,
        )]);
        assert_eq!(trees.len(), 1);
        assert_eq!(trees[0].root_process_id, 50);
    }

    #[test]
    fn recognizes_node_electron_and_browser_roots() {
        let processes = vec![
            process(10, 1, "node.exe", r#""node.exe" app.js"#),
            process(11, 10, "helper.exe", "helper.exe"),
            process(20, 1, "electron.exe", r#""electron.exe" app.js"#),
            process(21, 20, "electron.exe", r#""electron.exe" --type=renderer"#),
            process(30, 1, "chrome.exe", r#""chrome.exe""#),
            process(31, 30, "chrome.exe", r#""chrome.exe" --type=renderer"#),
        ];

        let node = process_trees(processes.clone(), ProcessRootKind::Node);
        assert_eq!(node.len(), 1);
        assert_eq!(node[0].root_process_id, 10);
        assert_eq!(node[0].root_kind, ProcessRootKind::Node);
        assert_eq!(
            node[0]
                .processes
                .iter()
                .map(|process| process.process_id)
                .collect::<Vec<_>>(),
            vec![10, 11]
        );

        let electron = process_trees(processes.clone(), ProcessRootKind::Electron);
        assert_eq!(electron.len(), 1);
        assert_eq!(electron[0].root_process_id, 20);
        assert_eq!(electron[0].processes[0].role, ProcessRole::ElectronMain);
        assert_eq!(electron[0].processes[1].role, ProcessRole::Renderer);

        let browser = process_trees(processes, ProcessRootKind::Browser);
        assert_eq!(browser.len(), 1);
        assert_eq!(browser[0].root_process_id, 30);
        assert_eq!(browser[0].processes[0].role, ProcessRole::BrowserMain);
        assert!(!browser[0].processes[1].attachable);
    }

    #[test]
    fn returns_a_covering_forest_for_nested_matching_runtimes() {
        let trees = process_trees(
            vec![
                process(10, 1, "node.exe", r#""node.exe" parent.js"#),
                process(11, 10, "helper.exe", "helper.exe"),
                process(12, 11, "node.exe", r#""node.exe" child.js"#),
                process(13, 12, "helper.exe", "helper.exe"),
            ],
            ProcessRootKind::Node,
        );

        assert_eq!(
            trees
                .iter()
                .map(|tree| tree.root_process_id)
                .collect::<Vec<_>>(),
            vec![10]
        );
        assert_eq!(
            trees[0]
                .processes
                .iter()
                .map(|process| process.process_id)
                .collect::<Vec<_>>(),
            vec![10, 11, 12, 13]
        );
    }

    #[test]
    fn rejects_parent_links_to_newer_reused_process_ids() {
        let mut root = process(10, 1, "chrome.exe", r#""chrome.exe""#);
        root.creation_date = "20260823020000.000000+000".to_owned();
        let mut stale_child = process(20, 10, "csrss.exe", "csrss.exe");
        stale_child.creation_date = "20260823010000.000000+000".to_owned();
        let stale_descendant = process(30, 20, "services.exe", "services.exe");

        let trees = process_trees(
            vec![root, stale_child, stale_descendant],
            ProcessRootKind::Browser,
        );

        assert_eq!(trees.len(), 1);
        assert_eq!(
            trees[0]
                .processes
                .iter()
                .map(|process| process.process_id)
                .collect::<Vec<_>>(),
            vec![10]
        );
    }

    fn process(
        process_id: u32,
        parent_process_id: u32,
        name: &str,
        command_line: &str,
    ) -> WindowsProcess {
        WindowsProcess {
            process_id,
            parent_process_id,
            name: name.to_owned(),
            command_line: command_line.to_owned(),
            creation_date: "20260823000000.000000+000".to_owned(),
            executable_path: String::new(),
            vscode_process_type: None,
        }
    }
}
