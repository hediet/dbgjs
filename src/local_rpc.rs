use std::env;
use std::fs;
use std::fs::OpenOptions;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::Arc;
use std::time::{Duration, Instant};

use atomic_write_file::AtomicWriteFile;
use fs2::FileExt;
use linkrpc::prelude::{CallCtx, DirectoryServiceClient, LinkRpcConnection};
use linkrpc_tokio::ndjson::{NdjsonTransport, Preamble};
use serde::{Deserialize, Serialize};
use tokio::io::{AsyncRead, AsyncWrite};
use tokio::sync::watch;
use tokio::time::{sleep, timeout};

use crate::debugger_service::DebuggerService;
use crate::service_api::{self, DbgServiceClient, ServiceApi};

const STARTUP_TIMEOUT: Duration = Duration::from_secs(10);

pub async fn serve_stdio() -> Result<(), LocalRpcError> {
    let state_directory = tempfile::Builder::new().prefix("dbgjs-stdio-").tempdir()?;
    let (shutdown_sender, mut shutdown_receiver) = watch::channel(false);
    let service = Arc::new(DebuggerService::load(
        shutdown_sender,
        state_directory.path().join("contexts.json"),
    )?);
    let connection = service_connection(
        NdjsonTransport::new(tokio::io::stdin(), tokio::io::stdout()),
        service.clone(),
    )?;
    tokio::select! {
        () = connection.run() => {},
        result = shutdown_receiver.wait_for(|shutdown| *shutdown) => {
            result.map_err(|error| LocalRpcError::Rpc(error.to_string()))?;
        },
    }
    if !*shutdown_receiver.borrow() {
        service
            .shutdown(&CallCtx::default())
            .await
            .map_err(|error| LocalRpcError::Rpc(format!("{error:?}")))?;
        shutdown_receiver
            .wait_for(|shutdown| *shutdown)
            .await
            .map_err(|error| LocalRpcError::Rpc(error.to_string()))?;
    }
    drop(connection);
    drop(service);
    state_directory.close()?;
    Ok(())
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct LocalServiceEndpoint {
    pub process_id: u32,
    pub transport: LocalTransportEndpoint,
    pub token: String,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "camelCase")]
pub enum LocalTransportEndpoint {
    NamedPipe { pipe_name: String },
    UnixSocket { path: PathBuf },
}

pub fn default_state_file() -> PathBuf {
    if let Some(path) = env::var_os("DBGJS_SERVICE_STATE") {
        return PathBuf::from(path);
    }
    if let Some(local_app_data) = env::var_os("LOCALAPPDATA") {
        return PathBuf::from(local_app_data)
            .join("dbgjs")
            .join("service.json");
    }
    if let Some(runtime_dir) = env::var_os("XDG_RUNTIME_DIR") {
        return PathBuf::from(runtime_dir)
            .join("dbgjs")
            .join("service.json");
    }
    if let Some(home) = env::var_os("HOME") {
        return PathBuf::from(home)
            .join(".cache")
            .join("dbgjs")
            .join("service.json");
    }
    env::temp_dir()
        .join(format!("dbgjs-{}", std::process::id()))
        .join("service.json")
}

pub fn persistent_state_file(endpoint_file: &Path) -> PathBuf {
    endpoint_file.with_extension("contexts.json")
}

pub fn startup_error_file(endpoint_file: &Path) -> PathBuf {
    endpoint_file.with_extension("startup-error.txt")
}

pub fn write_startup_error(endpoint_file: &Path, message: &str) -> Result<(), LocalRpcError> {
    let path = startup_error_file(endpoint_file);
    if let Some(parent) = path.parent() {
        ensure_private_directory(parent)?;
    }
    let mut file = AtomicWriteFile::open(&path)?;
    file.write_all(message.as_bytes())?;
    file.commit()?;
    restrict_private_file(&path)?;
    Ok(())
}

pub async fn serve_local(
    state_file: &Path,
    shutdown_sender: watch::Sender<bool>,
    shutdown_receiver: watch::Receiver<bool>,
) -> Result<(), LocalRpcError> {
    #[cfg(windows)]
    {
        serve_named_pipe(state_file, shutdown_sender, shutdown_receiver).await
    }
    #[cfg(unix)]
    {
        serve_unix_socket(state_file, shutdown_sender, shutdown_receiver).await
    }
    #[cfg(not(any(windows, unix)))]
    {
        let _ = (state_file, shutdown_sender, shutdown_receiver);
        Err(LocalRpcError::UnsupportedPlatform)
    }
}

#[cfg(windows)]
async fn serve_named_pipe(
    state_file: &Path,
    shutdown_sender: watch::Sender<bool>,
    mut shutdown_receiver: watch::Receiver<bool>,
) -> Result<(), LocalRpcError> {
    use tokio::net::windows::named_pipe::{NamedPipeServer, ServerOptions};

    fn create_server(name: &str, first: bool) -> std::io::Result<NamedPipeServer> {
        ServerOptions::new()
            .first_pipe_instance(first)
            .reject_remote_clients(true)
            .create(name)
    }

    let service = Arc::new(DebuggerService::load(
        shutdown_sender,
        persistent_state_file(state_file),
    )?);
    let token = random_token()?;
    let pipe_id = random_token()?;
    let pipe_name = format!(r"\\.\pipe\dbgjs-{}", &pipe_id[..32]);
    let mut server = create_server(&pipe_name, true)?;
    let endpoint = LocalServiceEndpoint {
        process_id: std::process::id(),
        transport: LocalTransportEndpoint::NamedPipe {
            pipe_name: pipe_name.clone(),
        },
        token,
    };
    write_endpoint(state_file, &endpoint)?;

    loop {
        let accepted = tokio::select! {
            result = server.connect() => Some(result),
            changed = shutdown_receiver.changed() => {
                if changed.is_err() || *shutdown_receiver.borrow() {
                    None
                } else {
                    continue;
                }
            }
        };
        let Some(accepted) = accepted else {
            break;
        };
        accepted?;
        let connected = server;
        server = create_server(&pipe_name, false)?;
        let service = service.clone();
        let token = endpoint.token.clone();
        tokio::spawn(async move {
            let _ = serve_peer(connected, &token, service).await;
        });
    }

    remove_endpoint_if_owned(state_file, &endpoint);
    Ok(())
}

#[cfg(unix)]
async fn serve_unix_socket(
    state_file: &Path,
    shutdown_sender: watch::Sender<bool>,
    mut shutdown_receiver: watch::Receiver<bool>,
) -> Result<(), LocalRpcError> {
    let service = Arc::new(DebuggerService::load(
        shutdown_sender,
        persistent_state_file(state_file),
    )?);
    let token = random_token()?;
    let socket_path = unix_socket_path()?;
    if let Some(parent) = socket_path.parent() {
        ensure_private_directory(parent)?;
    }
    let listener = tokio::net::UnixListener::bind(&socket_path)?;
    restrict_private_file(&socket_path)?;
    let endpoint = LocalServiceEndpoint {
        process_id: std::process::id(),
        transport: LocalTransportEndpoint::UnixSocket {
            path: socket_path.clone(),
        },
        token,
    };
    write_endpoint(state_file, &endpoint)?;

    loop {
        tokio::select! {
            accepted = listener.accept() => {
                let (stream, _) = accepted?;
                let service = service.clone();
                let token = endpoint.token.clone();
                tokio::spawn(async move {
                    let _ = serve_peer(stream, &token, service).await;
                });
            }
            changed = shutdown_receiver.changed() => {
                if changed.is_err() || *shutdown_receiver.borrow() {
                    break;
                }
            }
        }
    }

    remove_endpoint_if_owned(state_file, &endpoint);
    let socket_directory = socket_path.parent().map(Path::to_owned);
    let _ = fs::remove_file(socket_path);
    if let Some(socket_directory) = socket_directory {
        let _ = fs::remove_dir(socket_directory);
    }
    Ok(())
}

#[cfg(unix)]
fn unix_socket_path() -> Result<PathBuf, LocalRpcError> {
    use std::os::unix::ffi::OsStrExt;

    const CONSERVATIVE_SUN_PATH_LIMIT: usize = 100;
    let token = random_token()?;
    let directory_name = format!("dbgjs-{}", &token[..16]);
    let candidates = [
        env::temp_dir().join(&directory_name).join("service.sock"),
        Path::new("/tmp").join(directory_name).join("service.sock"),
    ];
    candidates
        .into_iter()
        .find(|path| path.as_os_str().as_bytes().len() < CONSERVATIVE_SUN_PATH_LIMIT)
        .ok_or(LocalRpcError::UnixSocketPathTooLong)
}

pub async fn connect_endpoint(
    endpoint: &LocalServiceEndpoint,
) -> Result<DbgServiceClient, LocalRpcError> {
    connect_endpoint_with_validation(endpoint, true).await
}

async fn connect_endpoint_with_validation(
    endpoint: &LocalServiceEndpoint,
    validate_interface: bool,
) -> Result<DbgServiceClient, LocalRpcError> {
    match &endpoint.transport {
        #[cfg(windows)]
        LocalTransportEndpoint::NamedPipe { pipe_name } => {
            let stream = open_named_pipe(pipe_name).await?;
            connect_stream(stream, &endpoint.token, validate_interface).await
        }
        #[cfg(unix)]
        LocalTransportEndpoint::UnixSocket { path } => {
            let stream = tokio::net::UnixStream::connect(path).await?;
            connect_stream(stream, &endpoint.token, validate_interface).await
        }
        _ => Err(LocalRpcError::UnsupportedTransport(
            endpoint.transport.clone(),
        )),
    }
}

#[cfg(windows)]
async fn open_named_pipe(
    pipe_name: &str,
) -> Result<tokio::net::windows::named_pipe::NamedPipeClient, std::io::Error> {
    use tokio::net::windows::named_pipe::ClientOptions;

    const ERROR_PIPE_BUSY: i32 = 231;
    let deadline = Instant::now() + STARTUP_TIMEOUT;
    loop {
        match ClientOptions::new().open(pipe_name) {
            Ok(client) => return Ok(client),
            Err(error)
                if error.raw_os_error() == Some(ERROR_PIPE_BUSY) && Instant::now() < deadline =>
            {
                sleep(Duration::from_millis(5)).await;
            }
            Err(error) => return Err(error),
        }
    }
}

async fn connect_stream<S>(
    stream: S,
    token: &str,
    validate_interface: bool,
) -> Result<DbgServiceClient, LocalRpcError>
where
    S: AsyncRead + AsyncWrite + Send + 'static,
{
    let transport = NdjsonTransport::from_stream(stream);
    transport
        .write_preamble(&Preamble::new(Some(token.to_owned())))
        .await?;
    let connection = LinkRpcConnection::new(Box::new(transport));
    let run = connection.clone();
    tokio::spawn(async move { run.run().await });
    if validate_interface {
        let directory = DirectoryServiceClient::new(connection.clone());
        for expected in service_api::interfaces() {
            let listing = directory
                .list(Some(expected.id().to_owned()), None, None, None, None)
                .await
                .map_err(|error| LocalRpcError::Rpc(format!("{error:?}")))?;
            let Some(actual) = listing
                .items
                .iter()
                .find(|item| item.interface_id == expected.id())
            else {
                return Err(LocalRpcError::InterfaceMissing(expected.id().to_owned()));
            };
            if actual.interface_hash != expected.schema_hash() {
                return Err(LocalRpcError::InterfaceHashMismatch {
                    interface_id: expected.id().to_owned(),
                    expected: expected.schema_hash().to_owned(),
                    actual: actual.interface_hash.clone(),
                });
            }
        }
    }
    Ok(DbgServiceClient::new(connection))
}

pub async fn connect_existing(state_file: &Path) -> Result<DbgServiceClient, LocalRpcError> {
    let endpoint = read_endpoint(state_file)?;
    let client = connect_endpoint(&endpoint).await?;
    let info = client
        .service
        .service_info()
        .await
        .map_err(|error| LocalRpcError::Rpc(format!("{error:?}")))?;
    if info.process_id != endpoint.process_id {
        return Err(LocalRpcError::EndpointOwnerChanged {
            expected: endpoint.process_id,
            actual: info.process_id,
        });
    }
    Ok(client)
}

pub async fn ensure_service(state_file: &Path) -> Result<DbgServiceClient, LocalRpcError> {
    if let Ok(client) = connect_existing(state_file).await {
        return Ok(client);
    }

    if let Some(parent) = state_file.parent() {
        ensure_private_directory(parent)?;
    }
    let startup_lock = OpenOptions::new()
        .create(true)
        .read(true)
        .write(true)
        .open(state_file.with_extension("startup.lock"))?;
    let lock_deadline = Instant::now() + STARTUP_TIMEOUT;
    loop {
        match startup_lock.try_lock_exclusive() {
            Ok(()) => break,
            Err(error) if is_startup_lock_contention(&error) => {
                if let Ok(client) = connect_existing(state_file).await {
                    return Ok(client);
                }
                if Instant::now() >= lock_deadline {
                    return Err(LocalRpcError::StartupLockTimeout);
                }
                sleep(Duration::from_millis(50)).await;
            }
            Err(error) => return Err(error.into()),
        }
    }
    match connect_existing(state_file).await {
        Ok(client) => return Ok(client),
        Err(LocalRpcError::InterfaceHashMismatch { .. } | LocalRpcError::InterfaceMissing(_)) => {
            shutdown_incompatible_service(state_file).await?;
        }
        Err(_) => {}
    }

    let startup_error = startup_error_file(state_file);
    let _ = fs::remove_file(&startup_error);
    let executable = spawn_service(state_file)?;
    let deadline = Instant::now() + STARTUP_TIMEOUT;
    let mut last_error = None;
    while Instant::now() < deadline {
        match connect_existing(state_file).await {
            Ok(client) => return Ok(client),
            Err(LocalRpcError::InterfaceHashMismatch {
                interface_id,
                expected,
                actual,
            }) => {
                return Err(LocalRpcError::SpawnedServiceInterfaceMismatch {
                    executable: executable.display().to_string(),
                    interface_id,
                    expected,
                    actual,
                });
            }
            Err(LocalRpcError::InterfaceMissing(interface_id)) => {
                return Err(LocalRpcError::SpawnedServiceInterfaceMissing {
                    executable: executable.display().to_string(),
                    interface_id,
                });
            }
            Err(error) => last_error = Some(error),
        }
        if let Ok(message) = fs::read_to_string(&startup_error) {
            return Err(LocalRpcError::StartupFailed(message));
        }
        sleep(Duration::from_millis(50)).await;
    }
    Err(LocalRpcError::StartupTimeout {
        last_error: last_error.map(Box::new),
    })
}

async fn shutdown_incompatible_service(state_file: &Path) -> Result<(), LocalRpcError> {
    timeout(STARTUP_TIMEOUT, async {
        let endpoint = read_endpoint(state_file)?;
        let client = connect_endpoint_with_validation(&endpoint, false).await?;
        let info = client
            .service
            .service_info()
            .await
            .map_err(|error| LocalRpcError::Rpc(format!("{error:?}")))?;
        if info.process_id != endpoint.process_id {
            return Err(LocalRpcError::EndpointOwnerChanged {
                expected: endpoint.process_id,
                actual: info.process_id,
            });
        }
        client
            .service
            .shutdown()
            .await
            .map_err(|error| LocalRpcError::Rpc(format!("{error:?}")))?;
        loop {
            if connect_endpoint_with_validation(&endpoint, false)
                .await
                .is_err()
            {
                return Ok(());
            }
            sleep(Duration::from_millis(25)).await;
        }
    })
    .await
    .map_err(|_| LocalRpcError::StartupTimeout { last_error: None })?
}

pub fn read_endpoint(path: &Path) -> Result<LocalServiceEndpoint, LocalRpcError> {
    let bytes = fs::read(path)?;
    Ok(serde_json::from_slice(&bytes)?)
}

async fn serve_peer(
    stream: impl AsyncRead + AsyncWrite + Send + 'static,
    expected_token: &str,
    service: Arc<DebuggerService>,
) -> Result<(), LocalRpcError> {
    let transport = NdjsonTransport::from_stream(stream);
    let Some(preamble) = transport.read_preamble().await? else {
        return Ok(());
    };
    if preamble.hello != 1 || preamble.token.as_deref() != Some(expected_token) {
        return Err(LocalRpcError::AuthenticationFailed);
    }

    let connection = service_connection(transport, service)?;
    connection.run().await;
    Ok(())
}

fn service_connection(
    transport: NdjsonTransport,
    service: Arc<DebuggerService>,
) -> Result<LinkRpcConnection, LocalRpcError> {
    let connection = LinkRpcConnection::new(Box::new(transport));
    service_api::register(&connection, service)?;
    connection.enable_reflection();
    Ok(connection)
}

fn write_endpoint(path: &Path, endpoint: &LocalServiceEndpoint) -> Result<(), LocalRpcError> {
    if let Some(parent) = path.parent() {
        ensure_private_directory(parent)?;
    }
    let mut file = AtomicWriteFile::open(path)?;
    file.write_all(&serde_json::to_vec(endpoint)?)?;
    file.commit()?;
    restrict_private_file(path)?;
    Ok(())
}

fn ensure_private_directory(path: &Path) -> Result<(), std::io::Error> {
    let created = match fs::create_dir(path) {
        Ok(()) => true,
        Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => false,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            fs::create_dir_all(path)?;
            true
        }
        Err(error) => return Err(error),
    };
    #[cfg(unix)]
    if created {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(path, fs::Permissions::from_mode(0o700))?;
    }
    let _ = created;
    Ok(())
}

fn restrict_private_file(path: &Path) -> Result<(), std::io::Error> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(path, fs::Permissions::from_mode(0o600))?;
    }
    let _ = path;
    Ok(())
}

fn remove_endpoint_if_owned(path: &Path, endpoint: &LocalServiceEndpoint) {
    if matches!(read_endpoint(path), Ok(current) if current == *endpoint) {
        let _ = fs::remove_file(path);
    }
}

fn spawn_service(state_file: &Path) -> Result<PathBuf, LocalRpcError> {
    let executable = match env::var_os("DBGJS_SERVICE_EXE") {
        Some(path) => PathBuf::from(path),
        None => {
            let mut path = env::current_exe()?;
            path.set_file_name(if cfg!(windows) {
                "dbgjs-service.exe"
            } else {
                "dbgjs-service"
            });
            path
        }
    };
    let mut command = Command::new(&executable);
    command
        .arg("--state-file")
        .arg(state_file)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    if let Err(source) = spawn_detached(&mut command) {
        return Err(LocalRpcError::Spawn { executable, source });
    }
    Ok(executable)
}

#[cfg(windows)]
fn spawn_detached(command: &mut Command) -> Result<std::process::Child, std::io::Error> {
    use std::os::windows::process::CommandExt;

    const CREATE_BREAKAWAY_FROM_JOB: u32 = 0x0100_0000;
    const DETACHED_PROCESS: u32 = 0x0000_0008;
    const CREATE_NO_WINDOW: u32 = 0x0800_0000;
    command.creation_flags(CREATE_BREAKAWAY_FROM_JOB | CREATE_NO_WINDOW);
    match command.spawn() {
        Ok(child) => Ok(child),
        Err(error) if error.kind() == std::io::ErrorKind::PermissionDenied => {
            command.creation_flags(CREATE_BREAKAWAY_FROM_JOB | DETACHED_PROCESS);
            match command.spawn() {
                Ok(child) => Ok(child),
                Err(error) if error.kind() == std::io::ErrorKind::PermissionDenied => {
                    command.creation_flags(CREATE_NO_WINDOW);
                    command.spawn()
                }
                Err(error) => Err(error),
            }
        }
        Err(error) => Err(error),
    }
}

#[cfg(not(windows))]
fn spawn_detached(command: &mut Command) -> Result<std::process::Child, std::io::Error> {
    command.spawn()
}

fn is_startup_lock_contention(error: &std::io::Error) -> bool {
    error.kind() == std::io::ErrorKind::WouldBlock
        || error.raw_os_error() == fs2::lock_contended_error().raw_os_error()
}

fn random_token() -> Result<String, LocalRpcError> {
    let mut bytes = [0_u8; 32];
    getrandom::fill(&mut bytes).map_err(|error| LocalRpcError::Random(error.to_string()))?;
    Ok(bytes.iter().map(|byte| format!("{byte:02x}")).collect())
}

#[derive(Debug, thiserror::Error)]
pub enum LocalRpcError {
    #[error(transparent)]
    Io(#[from] std::io::Error),
    #[error(transparent)]
    Json(#[from] serde_json::Error),
    #[error(transparent)]
    Connection(#[from] linkrpc::connection::hub_connection::ConnError),
    #[error("local service authentication failed")]
    AuthenticationFailed,
    #[error("local service does not expose required LinkRPC interface '{0}'")]
    InterfaceMissing(String),
    #[error(
        "LinkRPC interface '{interface_id}' hash mismatch: service has {actual}, client expects {expected}"
    )]
    InterfaceHashMismatch {
        interface_id: String,
        expected: String,
        actual: String,
    },
    #[error(
        "spawned service {executable} exposes incompatible LinkRPC interface '{interface_id}' \
         (service has {actual}, client expects {expected}); rebuild it with \
         `cargo build --bin dbgjs-service`"
    )]
    SpawnedServiceInterfaceMismatch {
        executable: String,
        interface_id: String,
        expected: String,
        actual: String,
    },
    #[error(
        "spawned service {executable} is missing required LinkRPC interface '{interface_id}'; \
         rebuild it with `cargo build --bin dbgjs-service`"
    )]
    SpawnedServiceInterfaceMissing {
        executable: String,
        interface_id: String,
    },
    #[error("service endpoint changed owner from process {expected} to {actual}")]
    EndpointOwnerChanged { expected: u32, actual: u32 },
    #[error("failed to spawn {executable}: {source}")]
    Spawn {
        executable: PathBuf,
        source: std::io::Error,
    },
    #[error("service did not become ready before timeout")]
    StartupTimeout {
        last_error: Option<Box<LocalRpcError>>,
    },
    #[error("timed out waiting for another CLI to start the service")]
    StartupLockTimeout,
    #[error("service failed during startup: {0}")]
    StartupFailed(String),
    #[error("service RPC failed: {0}")]
    Rpc(String),
    #[error("failed to generate authentication token: {0}")]
    Random(String),
    #[error(transparent)]
    Persistence(#[from] crate::debugger_service::ServicePersistenceError),
    #[error("local RPC is unsupported on this platform")]
    UnsupportedPlatform,
    #[error("no temporary directory can hold a Unix socket path within platform limits")]
    UnixSocketPathTooLong,
    #[error("local service transport is unsupported on this platform: {0:?}")]
    UnsupportedTransport(LocalTransportEndpoint),
}

#[cfg(test)]
mod tests {
    use super::*;
    use linkrpc::prelude::{CallCtx, JsonRpcError, RegisterOptions, link_rpc_interface};

    #[link_rpc_interface(id = "dev.dbgjs.cdp-debugger")]
    trait LegacyLifecycleApi {
        async fn service_info() -> Result<service_api::ServiceInfo, JsonRpcError>;
        async fn shutdown() -> Result<bool, JsonRpcError>;
    }

    struct LegacyLifecycle;

    #[async_trait::async_trait]
    impl LegacyLifecycleApi for LegacyLifecycle {
        async fn service_info(
            &self,
            _ctx: &CallCtx,
        ) -> Result<service_api::ServiceInfo, JsonRpcError> {
            Ok(service_api::ServiceInfo {
                process_id: std::process::id(),
                agent_instance_id: "legacy".into(),
            })
        }

        async fn shutdown(&self, _ctx: &CallCtx) -> Result<bool, JsonRpcError> {
            Ok(true)
        }
    }

    async fn connect_lifecycle_only(
        current_schema: bool,
        validate_interface: bool,
    ) -> Result<DbgServiceClient, LocalRpcError> {
        let (client_stream, server_stream) = tokio::io::duplex(64 * 1024);
        tokio::spawn(async move {
            let transport = NdjsonTransport::from_stream(server_stream);
            let preamble = transport.read_preamble().await.unwrap().unwrap();
            assert_eq!(preamble.token.as_deref(), Some("test"));
            let connection = LinkRpcConnection::new(Box::new(transport));
            let definition = if current_schema {
                service_api::service_api::interface()
            } else {
                legacy_lifecycle_api::interface()
            };
            connection
                .register(
                    Arc::new(definition),
                    Arc::new(LegacyLifecycleApiServer::new(Arc::new(LegacyLifecycle))),
                    RegisterOptions::default(),
                )
                .unwrap();
            connection.enable_reflection();
            connection.run().await;
        });
        connect_stream(client_stream, "test", validate_interface).await
    }

    #[tokio::test]
    async fn service_validation_checks_facets_beyond_lifecycle() {
        let result = connect_lifecycle_only(true, true).await;
        assert!(matches!(
            result,
            Err(LocalRpcError::InterfaceMissing(id)) if id == service_api::context_api::interface().id()
        ));
    }

    #[tokio::test]
    async fn service_lifecycle_keeps_old_daemon_shutdown_compatible() {
        assert!(matches!(
            connect_lifecycle_only(false, true).await,
            Err(LocalRpcError::InterfaceHashMismatch { interface_id, .. })
                if interface_id == service_api::service_api::interface().id()
        ));
        let client = connect_lifecycle_only(false, false).await.unwrap();
        assert_eq!(
            client
                .service
                .service_info()
                .await
                .unwrap()
                .agent_instance_id,
            "legacy"
        );
        assert!(client.service.shutdown().await.unwrap());
    }

    #[cfg(unix)]
    #[test]
    fn private_directory_setup_preserves_existing_directory_permissions() {
        use std::os::unix::fs::PermissionsExt;

        let root = env::temp_dir().join(format!(
            "dbgjs-directory-permissions-{}-{}",
            std::process::id(),
            random_token().unwrap()
        ));
        fs::create_dir(&root).unwrap();
        fs::set_permissions(&root, fs::Permissions::from_mode(0o755)).unwrap();

        ensure_private_directory(&root).unwrap();
        assert_eq!(
            fs::metadata(&root).unwrap().permissions().mode() & 0o777,
            0o755
        );

        let created = root.join("created");
        ensure_private_directory(&created).unwrap();
        assert_eq!(
            fs::metadata(&created).unwrap().permissions().mode() & 0o777,
            0o700
        );
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn spawned_service_mismatch_reports_the_rebuild_command() {
        let error = LocalRpcError::SpawnedServiceInterfaceMismatch {
            executable: "target/debug/dbgjs-service".to_owned(),
            interface_id: "debugger".to_owned(),
            expected: "new".to_owned(),
            actual: "old".to_owned(),
        };

        let message = error.to_string();
        assert!(message.contains("target/debug/dbgjs-service"));
        assert!(message.contains("service has old, client expects new"));
        assert!(message.contains("cargo build --bin dbgjs-service"));
    }

    #[cfg(windows)]
    #[test]
    fn windows_lock_violation_is_startup_contention() {
        assert!(is_startup_lock_contention(
            &std::io::Error::from_raw_os_error(33)
        ));
        assert!(!is_startup_lock_contention(&std::io::Error::other(
            "not a lock error"
        )));
        assert!(!is_startup_lock_contention(
            &std::io::Error::from_raw_os_error(5)
        ));
    }

    #[tokio::test]
    async fn service_facets_share_state_and_route_over_native_local_ipc() {
        let state_file = env::temp_dir().join(format!(
            "dbgjs-local-rpc-{}-{}.json",
            std::process::id(),
            random_token().unwrap()
        ));
        let (shutdown_sender, shutdown_receiver) = watch::channel(false);
        let server_state_file = state_file.clone();
        let server = tokio::spawn(async move {
            serve_local(&server_state_file, shutdown_sender, shutdown_receiver)
                .await
                .unwrap();
        });

        let deadline = Instant::now() + Duration::from_secs(5);
        let client = loop {
            if let Ok(client) = connect_existing(&state_file).await {
                break client;
            }
            assert!(Instant::now() < deadline, "service did not become ready");
            sleep(Duration::from_millis(10)).await;
        };

        let created = client
            .contexts
            .put_context(
                "shop".into(),
                crate::context_identity::ContextKind::Named,
                Some("Shop".into()),
            )
            .await
            .unwrap();
        assert_eq!(created.revision, 1);

        let connection_ref = service_api::ConnectionRef {
            context_id: "shop".into(),
            connection_id: "server".into(),
        };
        let server_connection = client
            .contexts
            .put_connection(connection_ref.clone(), "ws://127.0.0.1:9229".into())
            .await
            .unwrap();
        assert_eq!(server_connection.connections.len(), 1);
        let browser_connection = client
            .contexts
            .put_connection(
                service_api::ConnectionRef {
                    context_id: "shop".into(),
                    connection_id: "browser".into(),
                },
                "ws://127.0.0.1:9222".into(),
            )
            .await
            .unwrap();
        assert_eq!(
            browser_connection
                .connections
                .iter()
                .map(|connection| connection.id.as_str())
                .collect::<Vec<_>>(),
            vec!["browser", "server"]
        );

        let with_breakpoint = client
            .contexts
            .put_breakpoint(
                "shop".into(),
                "shared-validation".into(),
                "file:///workspace/shared/validation.ts".into(),
                41,
                1,
            )
            .await
            .unwrap();
        assert_eq!(with_breakpoint.breakpoints.len(), 1);
        assert_eq!(
            with_breakpoint.breakpoints[0].status,
            crate::service_api::BreakpointStatus::Pending
        );

        assert_eq!(
            client.service.service_info().await.unwrap().process_id,
            std::process::id()
        );
        let formatting = client
            .sources
            .set_source_formatting("shop".into(), service_api::SourceFormattingMode::On)
            .await
            .unwrap();
        assert_eq!(
            client
                .contexts
                .get_context("shop".into())
                .await
                .unwrap()
                .source_formatting,
            formatting,
        );
        assert!(
            client
                .captures
                .list_captures("shop".into())
                .await
                .unwrap()
                .is_empty()
        );
        assert!(!client.relay.close_relay("missing".into()).await.unwrap());

        let target_ref = service_api::TargetRef {
            connection: connection_ref,
            target_id: "missing".into(),
        };
        let errors = [
            client
                .targets
                .get_target(target_ref.clone())
                .await
                .unwrap_err(),
            client
                .cdp
                .raw_cdp_request(
                    target_ref.clone(),
                    "Runtime.evaluate".into(),
                    serde_json::json!({"expression": "1"}),
                    true,
                )
                .await
                .unwrap_err(),
            client
                .browser
                .click_target(target_ref.clone(), "button".into())
                .await
                .unwrap_err(),
            client
                .coverage
                .start_coverage(target_ref.clone())
                .await
                .unwrap_err(),
            client
                .cpu
                .start_cpu_profile(target_ref.clone(), None)
                .await
                .unwrap_err(),
            client
                .heap
                .get_heap_snapshot_progress(target_ref)
                .await
                .unwrap_err(),
        ];
        for error in errors {
            assert_eq!(error.code, linkrpc::prelude::error_codes::INVALID_PARAMS);
            assert_eq!(
                error.message,
                "target selector 'missing' did not match a discovered target; target discovery may be incomplete"
            );
        }

        assert!(client.service.shutdown().await.unwrap());
        server.await.unwrap();
        assert!(!state_file.exists());
        let _ = fs::remove_file(persistent_state_file(&state_file));
    }
}
