use std::env;
use std::io::ErrorKind;
#[cfg(unix)]
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::time::{Duration, Instant, SystemTime};

use async_trait::async_trait;
use base64::Engine as _;
use base64::engine::general_purpose::STANDARD as BASE64_STANDARD;
use linkrpc::connection::channel::{Channel, RequestHandler};
use linkrpc::prelude::{JsonRpcError, JsonRpcMessage, MessageTransport, MuxError, TransportError};
use serde_json::Value;
use sha2::{Digest, Sha256};
use tokio::io::AsyncWriteExt;
use tokio::sync::{Mutex, broadcast, mpsc, watch};

use crate::cdp::{
    CdpClient, DebuggerLocation, DebuggerPausedParams, DebuggerScriptParsedParams,
    NetworkLoadNetworkResourceOptions, NetworkLoadNetworkResourceParams,
    RuntimeConsoleApicalledParams, TargetTargetCreatedParams, TargetTargetDestroyedParams,
    TargetTargetInfoChangedParams,
};
use crate::cdp_transport::ManagedCdpTransport;
use crate::debugger_engine::{Effect, Input, RawFrame, RawScope, SessionKey, StepKind};
use crate::session_transport::CdpSessionMux;
use crate::source_view::{Position, SourceMapData};
use crate::websocket_transport::{CdpWebSocketError, CdpWebSocketTransport};

#[path = "cdp_source_map_resources.rs"]
mod source_map_resources;
use source_map_resources::SourceMapResources;

const SOURCE_MAP_RESOURCE_TIMEOUT: Duration = Duration::from_secs(30);

/// Backlog for each session's raw CDP event broadcast. Generous because relay consumers must
/// not silently miss console/network/lifecycle events while draining a burst.
const RAW_EVENT_BUFFER: usize = 1024;

pub struct CdpConnection {
    transport: Arc<dyn ManagedCdpTransport>,
    mux: CdpSessionMux,
    root: CdpClient<Channel>,
    root_events: Mutex<Option<mpsc::UnboundedReceiver<Result<RootCdpEvent, CdpRuntimeEventError>>>>,
    root_debugger: std::sync::Mutex<Option<CdpDebuggerSession>>,
    close_reason: Arc<Mutex<Option<String>>>,
}

impl CdpConnection {
    pub async fn connect(endpoint: &str) -> Result<Self, CdpRuntimeError> {
        let transport = Arc::new(CdpWebSocketTransport::connect(endpoint).await?);
        Self::connect_transport(transport).await
    }

    pub async fn connect_transport<T>(transport: Arc<T>) -> Result<Self, CdpRuntimeError>
    where
        T: ManagedCdpTransport + 'static,
    {
        let close_reason = transport.close_reason();
        let mux = CdpSessionMux::new(transport.clone());
        let (root_event_sender, root_event_receiver) = mpsc::unbounded_channel();
        let root_channel = Channel::new(
            Box::new(mux.open_root().map_err(CdpRuntimeError::OpenSession)?),
            Box::new(
                RootCdpEventHandler {
                    sender: root_event_sender,
                }
                .into_dispatcher(),
            ),
        );
        let root = CdpClient::root(root_channel.clone());
        let mux_loop = mux.clone();
        tokio::spawn(async move { mux_loop.run().await });
        tokio::spawn(async move { root_channel.run().await });
        Ok(Self {
            transport,
            mux,
            root,
            root_events: Mutex::new(Some(root_event_receiver)),
            root_debugger: std::sync::Mutex::new(None),
            close_reason,
        })
    }

    pub async fn connect_root_debugger(
        endpoint: &str,
        connection_generation: u64,
        session_id: String,
    ) -> Result<Self, CdpRuntimeError> {
        let transport = Arc::new(CdpWebSocketTransport::connect(endpoint).await?);
        Self::connect_root_debugger_transport(transport, connection_generation, session_id).await
    }

    pub async fn connect_root_debugger_transport<T>(
        transport: Arc<T>,
        connection_generation: u64,
        session_id: String,
    ) -> Result<Self, CdpRuntimeError>
    where
        T: ManagedCdpTransport + 'static,
    {
        let close_reason = transport.close_reason();
        let mux = CdpSessionMux::new(transport.clone());
        let session = SessionKey {
            connection_generation,
            session_id,
        };
        let (event_sender, event_receiver) = mpsc::unbounded_channel();
        let heap_snapshot = Arc::new(Mutex::new(None));
        let (heap_snapshot_progress, _) = watch::channel(None);
        let (raw_events, _) = broadcast::channel(RAW_EVENT_BUFFER);
        let raw_event_history = Arc::new(std::sync::Mutex::new(Vec::new()));
        let source_map_resources =
            SourceMapResources::new(mux.open_root().map_err(CdpRuntimeError::OpenSession)?);
        let root_channel = CdpEventHandler {
            session: session.clone(),
            sender: event_sender,
            heap_snapshot: heap_snapshot.clone(),
            heap_snapshot_progress: heap_snapshot_progress.clone(),
            raw_events: raw_events.clone(),
            raw_event_history: raw_event_history.clone(),
        }
        .into_channel(source_map_resources.transport());
        let root = CdpClient::root(root_channel.clone());
        let debugger = CdpDebuggerSession {
            session,
            client: CdpClient::root(root_channel.clone()),
            channel: root_channel.clone(),
            events: event_receiver,
            source_map_frame_id: Mutex::new(None),
            source_map_resources,
            source_map_cache_enabled: AtomicBool::new(true),
            source_map_cache_hits: AtomicU64::new(0),
            source_map_cache_misses: AtomicU64::new(0),
            source_map_cache_bypasses: AtomicU64::new(0),
            heap_snapshot,
            heap_snapshot_progress,
            raw_events,
            raw_event_history,
        };
        let mux_loop = mux.clone();
        tokio::spawn(async move { mux_loop.run().await });
        tokio::spawn(async move { root_channel.run().await });
        let (_, root_event_receiver) = mpsc::unbounded_channel();
        Ok(Self {
            transport,
            mux,
            root,
            root_events: Mutex::new(Some(root_event_receiver)),
            root_debugger: std::sync::Mutex::new(Some(debugger)),
            close_reason,
        })
    }

    pub fn root(&self) -> &CdpClient<Channel> {
        &self.root
    }

    pub fn open_session(&self, session: SessionKey) -> Result<CdpDebuggerSession, CdpRuntimeError> {
        let (event_sender, event_receiver) = mpsc::unbounded_channel();
        let heap_snapshot = Arc::new(Mutex::new(None));
        let (heap_snapshot_progress, _) = watch::channel(None);
        let (raw_events, _) = broadcast::channel(RAW_EVENT_BUFFER);
        let raw_event_history = Arc::new(std::sync::Mutex::new(Vec::new()));
        let source_map_resources = SourceMapResources::new(
            self.mux
                .open_session(session.session_id.clone())
                .map_err(CdpRuntimeError::OpenSession)?,
        );
        let channel = CdpEventHandler {
            session: session.clone(),
            sender: event_sender,
            heap_snapshot: heap_snapshot.clone(),
            heap_snapshot_progress: heap_snapshot_progress.clone(),
            raw_events: raw_events.clone(),
            raw_event_history: raw_event_history.clone(),
        }
        .into_channel(source_map_resources.transport());
        let client = CdpClient::root(channel.clone());
        let run_channel = channel.clone();
        tokio::spawn(async move { run_channel.run().await });
        Ok(CdpDebuggerSession {
            session,
            client,
            channel: channel.clone(),
            events: event_receiver,
            source_map_frame_id: Mutex::new(None),
            source_map_resources,
            source_map_cache_enabled: AtomicBool::new(true),
            source_map_cache_hits: AtomicU64::new(0),
            source_map_cache_misses: AtomicU64::new(0),
            source_map_cache_bypasses: AtomicU64::new(0),
            heap_snapshot,
            heap_snapshot_progress,
            raw_events,
            raw_event_history,
        })
    }

    pub fn retire_session(&self, session_id: &str) {
        self.mux.retire_session(session_id);
    }

    pub fn take_root_debugger_session(&self) -> Option<CdpDebuggerSession> {
        self.root_debugger.lock().unwrap().take()
    }

    pub async fn take_root_events(
        &self,
    ) -> Option<mpsc::UnboundedReceiver<Result<RootCdpEvent, CdpRuntimeEventError>>> {
        self.root_events.lock().await.take()
    }

    pub fn close_reason(&self) -> Arc<Mutex<Option<String>>> {
        self.close_reason.clone()
    }

    pub async fn wait_closed(&self) -> String {
        let reason = self.transport.wait_closed().await;
        self.mux.dispose();
        reason
    }

    pub async fn close(&self) {
        self.transport.close().await;
        self.mux.dispose();
    }
}

#[derive(Debug)]
pub enum RootCdpEvent {
    TargetCreated(TargetTargetCreatedParams),
    TargetChanged(TargetTargetInfoChangedParams),
    TargetDestroyed(TargetTargetDestroyedParams),
}

#[derive(Clone)]
struct RootCdpEventHandler {
    sender: mpsc::UnboundedSender<Result<RootCdpEvent, CdpRuntimeEventError>>,
}

struct RootCdpEventDispatcher {
    handler: RootCdpEventHandler,
    router: linkrpc::binding::InterfaceRouter,
}

impl RootCdpEventHandler {
    fn into_dispatcher(self) -> RootCdpEventDispatcher {
        let router = linkrpc::binding::InterfaceRouter::new();
        crate::cdp::target_events::DOMAIN
            .register(
                &router,
                Arc::new(crate::cdp::target_events::TargetEventsServer::new(
                    Arc::new(self.clone()),
                )),
            )
            .expect("generated Target events binding is valid");
        RootCdpEventDispatcher {
            handler: self,
            router,
        }
    }
}

#[async_trait]
impl crate::cdp::target_events::TargetEventsService for RootCdpEventHandler {
    async fn target_created(
        &self,
        _ctx: &linkrpc::prelude::CallCtx,
        target_info: crate::cdp::TargetTargetInfo,
    ) -> Result<(), JsonRpcError> {
        let params = TargetTargetCreatedParams { target_info };
        let _ = self.sender.send(Ok(RootCdpEvent::TargetCreated(params)));
        Ok(())
    }

    async fn target_info_changed(
        &self,
        _ctx: &linkrpc::prelude::CallCtx,
        target_info: crate::cdp::TargetTargetInfo,
    ) -> Result<(), JsonRpcError> {
        let params = TargetTargetInfoChangedParams { target_info };
        let _ = self.sender.send(Ok(RootCdpEvent::TargetChanged(params)));
        Ok(())
    }

    async fn target_destroyed(
        &self,
        _ctx: &linkrpc::prelude::CallCtx,
        target_id: crate::cdp::TargetTargetId,
    ) -> Result<(), JsonRpcError> {
        let params = TargetTargetDestroyedParams { target_id };
        let _ = self.sender.send(Ok(RootCdpEvent::TargetDestroyed(params)));
        Ok(())
    }
}

#[async_trait]
impl RequestHandler for RootCdpEventDispatcher {
    async fn handle_request(&self, method: String, _params: Value) -> Result<Value, JsonRpcError> {
        Err(JsonRpcError::new(
            -32601,
            format!("unexpected browser request: {method}"),
        ))
    }

    async fn handle_notification(&self, method: String, params: Value) {
        if let Err(error) = self.router.dispatch_notification(&method, params).await {
            let _ = self.handler.sender.send(Err(CdpRuntimeEventError::Decode {
                method,
                message: error.message,
            }));
        }
    }
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct HeapSnapshotStreamProgress {
    pub done: i64,
    pub total: i64,
    pub finished: Option<bool>,
    pub bytes_written: u64,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct SourceMapCacheStats {
    pub hits: u64,
    pub misses: u64,
    pub bypasses: u64,
}

struct HeapSnapshotWriter {
    destination: PathBuf,
    temporary: PathBuf,
    file: tokio::fs::File,
    started_at: Instant,
    taking_finished_at: Option<Instant>,
    bytes_written: u64,
    write_error: Option<std::io::Error>,
}

pub(crate) struct HeapSnapshotWriteResult {
    pub(crate) bytes_written: u64,
    pub(crate) taking_duration: Duration,
    pub(crate) retrieving_duration: Duration,
}

pub struct CdpDebuggerSession {
    session: SessionKey,
    client: CdpClient<Channel>,
    channel: Channel,
    events: mpsc::UnboundedReceiver<Result<CdpRuntimeEvent, CdpRuntimeEventError>>,
    source_map_frame_id: Mutex<Option<String>>,
    source_map_resources: Arc<SourceMapResources>,
    source_map_cache_enabled: AtomicBool,
    source_map_cache_hits: AtomicU64,
    source_map_cache_misses: AtomicU64,
    source_map_cache_bypasses: AtomicU64,
    heap_snapshot: Arc<Mutex<Option<HeapSnapshotWriter>>>,
    heap_snapshot_progress: watch::Sender<Option<HeapSnapshotStreamProgress>>,
    raw_events: broadcast::Sender<RawCdpEvent>,
    raw_event_history: Arc<std::sync::Mutex<Vec<RawCdpEvent>>>,
}

impl CdpDebuggerSession {
    pub fn key(&self) -> &SessionKey {
        &self.session
    }

    pub fn client(&self) -> &CdpClient<Channel> {
        &self.client
    }

    pub async fn raw_request(&self, method: &str, params: Value) -> Result<Value, JsonRpcError> {
        self.channel.call(method, params).await
    }

    /// Subscribes to every raw CDP notification observed on this session, independent of
    /// whether the debugger engine reducer recognizes the method. Relay consumers use this
    /// to mirror events verbatim instead of only the typed subset the reducer understands.
    pub fn raw_events_sender(&self) -> broadcast::Sender<RawCdpEvent> {
        self.raw_events.clone()
    }

    pub fn raw_event_history(&self) -> Arc<std::sync::Mutex<Vec<RawCdpEvent>>> {
        self.raw_event_history.clone()
    }

    pub fn set_source_map_cache_enabled(&self, enabled: bool) {
        self.source_map_cache_enabled
            .store(enabled, Ordering::Relaxed);
    }

    pub fn source_map_cache_stats(&self) -> SourceMapCacheStats {
        SourceMapCacheStats {
            hits: self.source_map_cache_hits.load(Ordering::Relaxed),
            misses: self.source_map_cache_misses.load(Ordering::Relaxed),
            bypasses: self.source_map_cache_bypasses.load(Ordering::Relaxed),
        }
    }

    pub fn heap_snapshot_progress(&self) -> watch::Receiver<Option<HeapSnapshotStreamProgress>> {
        self.heap_snapshot_progress.subscribe()
    }

    pub async fn begin_heap_snapshot(&self, destination: PathBuf) -> std::io::Result<()> {
        static TEMPORARY_ID: AtomicU64 = AtomicU64::new(1);

        let started_at = Instant::now();
        let mut snapshot = self.heap_snapshot.lock().await;
        if snapshot.is_some() {
            return Err(std::io::Error::new(
                ErrorKind::AlreadyExists,
                "a heap snapshot is already in progress",
            ));
        }
        let parent = destination
            .parent()
            .filter(|path| !path.as_os_str().is_empty());
        if let Some(parent) = parent {
            #[cfg(unix)]
            let existed = parent.exists();
            tokio::fs::create_dir_all(parent).await?;
            #[cfg(unix)]
            if !existed {
                tokio::fs::set_permissions(parent, std::fs::Permissions::from_mode(0o700)).await?;
            }
        }
        let file_name = destination
            .file_name()
            .ok_or_else(|| {
                std::io::Error::new(
                    ErrorKind::InvalidInput,
                    "heap snapshot destination must name a file",
                )
            })?
            .to_string_lossy();
        let temporary = destination.with_file_name(format!(
            ".{file_name}.{}.{}.tmp",
            std::process::id(),
            TEMPORARY_ID.fetch_add(1, Ordering::Relaxed)
        ));
        let mut options = tokio::fs::OpenOptions::new();
        options.write(true).create_new(true);
        #[cfg(unix)]
        options.mode(0o600);
        let file = options.open(&temporary).await?;
        *snapshot = Some(HeapSnapshotWriter {
            destination,
            temporary,
            file,
            started_at,
            taking_finished_at: None,
            bytes_written: 0,
            write_error: None,
        });
        self.heap_snapshot_progress
            .send_replace(Some(HeapSnapshotStreamProgress::default()));
        Ok(())
    }

    pub async fn finish_heap_snapshot_bytes(&self) -> std::io::Result<u64> {
        Ok(self.finish_heap_snapshot().await?.bytes_written)
    }

    pub(crate) async fn finish_heap_snapshot(&self) -> std::io::Result<HeapSnapshotWriteResult> {
        let Some(mut snapshot) = self.heap_snapshot.lock().await.take() else {
            return Err(std::io::Error::new(
                ErrorKind::NotFound,
                "no heap snapshot is in progress",
            ));
        };
        if let Some(error) = snapshot.write_error.take() {
            drop(snapshot.file);
            remove_temporary_file(&snapshot.temporary).await;
            return Err(error);
        }
        if let Err(error) = snapshot.file.flush().await {
            drop(snapshot.file);
            remove_temporary_file(&snapshot.temporary).await;
            return Err(error);
        }
        if let Err(error) = snapshot.file.sync_data().await {
            drop(snapshot.file);
            remove_temporary_file(&snapshot.temporary).await;
            return Err(error);
        }
        drop(snapshot.file);
        if snapshot.bytes_written == 0 {
            remove_temporary_file(&snapshot.temporary).await;
            return Err(std::io::Error::new(
                ErrorKind::InvalidData,
                "CDP completed the heap snapshot without sending any data",
            ));
        }
        if let Err(error) =
            replace_file_preserving_previous(&snapshot.temporary, &snapshot.destination).await
        {
            remove_temporary_file(&snapshot.temporary).await;
            return Err(error);
        }
        let mut progress = self
            .heap_snapshot_progress
            .borrow()
            .clone()
            .unwrap_or_default();
        progress.finished = Some(true);
        progress.bytes_written = snapshot.bytes_written;
        self.heap_snapshot_progress.send_replace(Some(progress));
        let finished_at = Instant::now();
        let taking_finished_at = snapshot.taking_finished_at.unwrap_or(finished_at);
        Ok(HeapSnapshotWriteResult {
            bytes_written: snapshot.bytes_written,
            taking_duration: taking_finished_at.saturating_duration_since(snapshot.started_at),
            retrieving_duration: finished_at.saturating_duration_since(taking_finished_at),
        })
    }

    pub async fn abort_heap_snapshot(&self) {
        if let Some(snapshot) = self.heap_snapshot.lock().await.take() {
            drop(snapshot.file);
            remove_temporary_file(&snapshot.temporary).await;
        }
    }

    pub async fn next_event(&mut self) -> Option<Result<CdpRuntimeEvent, CdpRuntimeEventError>> {
        self.events.recv().await
    }

    pub async fn execute(&self, effect: &Effect) -> Result<Option<Input>, CdpRuntimeError> {
        match effect {
            Effect::ConfigureSession { effect_id, session } if session == &self.session => {
                self.client
                    .runtime()
                    .enable()
                    .await
                    .map_err(CdpRuntimeError::protocol)?;
                if let Err(error) = self.client.debugger().disable().await {
                    eprintln!(
                        "could not reset the CDP Debugger domain before enabling session replay: {error:?}"
                    );
                }
                self.client
                    .debugger()
                    .enable(None)
                    .await
                    .map_err(CdpRuntimeError::protocol)?;
                Ok(Some(Input::SessionConfigured {
                    effect_id: *effect_id,
                }))
            }
            Effect::RunIfWaitingForDebugger { effect_id, session } if session == &self.session => {
                self.client
                    .runtime()
                    .run_if_waiting_for_debugger()
                    .await
                    .map_err(CdpRuntimeError::protocol)?;
                Ok(Some(Input::CommandAccepted {
                    effect_id: *effect_id,
                }))
            }
            Effect::FetchScriptSource {
                effect_id,
                script,
                generated_url,
                script_hash,
                source_map_url,
                frame_id,
                ..
            } if script.session == self.session => {
                let source = match self
                    .client
                    .debugger()
                    .get_script_source(script.script_id.clone())
                    .await
                {
                    Ok(source) => source,
                    Err(error) => {
                        return Ok(Some(Input::EffectFailed {
                            effect_id: *effect_id,
                            message: CdpRuntimeError::protocol(error).to_string(),
                        }));
                    }
                };
                let (source_map, resolved_source_map_url, source_map_error) = match source_map_url {
                    Some(source_map_url) => {
                        match self
                            .load_source_map(
                                generated_url,
                                script_hash,
                                source_map_url,
                                frame_id.as_deref(),
                            )
                            .await
                        {
                            Ok((source_map, resolved_url)) => {
                                (Some(source_map), Some(resolved_url), None)
                            }
                            Err(error) if error.is_source_map_unavailable() => {
                                (None, None, Some(error.to_string()))
                            }
                            Err(error) => return Err(error),
                        }
                    }
                    None => (None, None, None),
                };
                Ok(Some(Input::ScriptSourceFetched {
                    effect_id: *effect_id,
                    content: Arc::from(source.script_source),
                    source_map,
                    source_map_url: resolved_source_map_url,
                    source_map_error,
                }))
            }
            Effect::InstallBreakpoint {
                effect_id,
                physical,
            } if physical.script.session == self.session => {
                let mut location = DebuggerLocation::new(
                    physical.script.script_id.clone(),
                    i64::from(physical.position.line),
                );
                location.column_number = Some(i64::from(physical.position.column));
                let installed = self
                    .client
                    .debugger()
                    .set_breakpoint(location, physical.condition.clone())
                    .await
                    .map_err(CdpRuntimeError::protocol)?;
                let confirmed_position = crate::source_view::Position {
                    line: u32::try_from(installed.actual_location.line_number).map_err(|_| {
                        CdpRuntimeError::InvalidBreakpointLocation {
                            line: installed.actual_location.line_number,
                            column: installed.actual_location.column_number,
                        }
                    })?,
                    column: u32::try_from(
                        installed.actual_location.column_number.unwrap_or_default(),
                    )
                    .map_err(|_| {
                        CdpRuntimeError::InvalidBreakpointLocation {
                            line: installed.actual_location.line_number,
                            column: installed.actual_location.column_number,
                        }
                    })?,
                };
                Ok(Some(Input::BreakpointInstalled {
                    effect_id: *effect_id,
                    backend_id: installed.breakpoint_id,
                    confirmed_position,
                }))
            }
            Effect::RemoveBreakpoint {
                effect_id,
                physical,
                backend_id,
            } if physical.script.session == self.session => {
                self.client
                    .debugger()
                    .remove_breakpoint(backend_id.clone())
                    .await
                    .map_err(CdpRuntimeError::protocol)?;
                Ok(Some(Input::BreakpointRemoved {
                    effect_id: *effect_id,
                }))
            }
            Effect::Resume {
                effect_id, session, ..
            } if session == &self.session => {
                self.client
                    .debugger()
                    .resume(None)
                    .await
                    .map_err(CdpRuntimeError::protocol)?;
                Ok(Some(Input::CommandAccepted {
                    effect_id: *effect_id,
                }))
            }
            Effect::Step {
                effect_id,
                session,
                kind,
                ..
            } if session == &self.session => {
                match kind {
                    StepKind::Into => {
                        self.client
                            .debugger()
                            .step_into(None, None)
                            .await
                            .map_err(CdpRuntimeError::protocol)?;
                    }
                    StepKind::Over => {
                        self.client
                            .debugger()
                            .step_over(None)
                            .await
                            .map_err(CdpRuntimeError::protocol)?;
                    }
                    StepKind::Out => {
                        self.client
                            .debugger()
                            .step_out()
                            .await
                            .map_err(CdpRuntimeError::protocol)?;
                    }
                };
                Ok(Some(Input::CommandAccepted {
                    effect_id: *effect_id,
                }))
            }
            _ => Ok(None),
        }
    }

    async fn load_source_map(
        &self,
        generated_url: &str,
        script_hash: &str,
        source_map_url: &str,
        frame_id: Option<&str>,
    ) -> Result<(SourceMapData, String), CdpRuntimeError> {
        if source_map_url.starts_with("data:") {
            return decode_source_map_data_url(source_map_url)
                .map(|source_map| (SourceMapData::new(source_map), generated_url.to_owned()));
        }

        let resolved_url = resolve_source_map_url(generated_url, source_map_url)?;
        let cache_enabled = self.source_map_cache_enabled.load(Ordering::Relaxed);
        if !cache_enabled {
            self.source_map_cache_bypasses
                .fetch_add(1, Ordering::Relaxed);
        }
        let cache_path = cache_enabled
            .then(|| source_map_cache_path(script_hash, &resolved_url))
            .flatten();
        if let Some(path) = &cache_path {
            match tokio::fs::read(path).await {
                Ok(bytes) => {
                    if let Some(source_map) = decode_source_map_cache(bytes) {
                        self.source_map_cache_hits.fetch_add(1, Ordering::Relaxed);
                        if let Err(error) =
                            tokio::fs::write(path.with_extension("access"), []).await
                        {
                            eprintln!(
                                "failed to update source-map cache access marker {}: {error}",
                                path.display()
                            );
                        }
                        return Ok((source_map, resolved_url));
                    }
                    eprintln!("ignoring invalid source-map cache entry {}", path.display());
                    if let Err(error) = tokio::fs::remove_file(path).await {
                        eprintln!(
                            "failed to remove invalid source-map cache entry {}: {error}",
                            path.display()
                        );
                    }
                    let _ = tokio::fs::remove_file(path.with_extension("access")).await;
                }
                Err(error) if error.kind() == ErrorKind::NotFound => {}
                Err(error) => {
                    eprintln!(
                        "failed to read source-map cache entry {}: {error}",
                        path.display()
                    );
                }
            }
        }
        if cache_enabled {
            self.source_map_cache_misses.fetch_add(1, Ordering::Relaxed);
        }
        let bytes = match self.load_source_map_via_cdp(&resolved_url, frame_id).await {
            Ok(bytes) => bytes,
            Err(cdp_error) if Self::direct_source_map_scheme(&resolved_url) => {
                Self::load_source_map_direct(&resolved_url)
                    .await
                    .map_err(|direct_error| CdpRuntimeError::SourceMapFallbackFailed {
                        url: resolved_url.to_owned(),
                        cdp: Box::new(cdp_error),
                        direct: direct_error,
                    })?
            }
            Err(error) => return Err(error),
        };
        let source_map = SourceMapData::new(bytes);
        if source_map.is_supported() {
            if let Some(path) = cache_path
                && let Err(error) = write_source_map_cache(&path, &source_map).await
            {
                eprintln!(
                    "failed to write source-map cache entry {}: {error}",
                    path.display()
                );
            }
        } else {
            eprintln!("not caching invalid or unsupported source map {resolved_url}");
        }
        Ok((source_map, resolved_url))
    }

    async fn load_source_map_via_cdp(
        &self,
        resolved_url: &str,
        owning_frame_id: Option<&str>,
    ) -> Result<Vec<u8>, CdpRuntimeError> {
        let cached_frame_id = match owning_frame_id {
            Some(frame_id) => Some(frame_id.to_owned()),
            None => self.source_map_frame_id.lock().await.clone(),
        };
        let frame_id = match cached_frame_id {
            Some(frame_id) => frame_id,
            None => {
                let frame_tree = self.client.page().get_frame_tree().await.map_err(|error| {
                    CdpRuntimeError::SourceMapProtocol {
                        url: resolved_url.to_owned(),
                        source: Box::new(CdpRuntimeError::protocol(error)),
                    }
                })?;
                let frame_id = frame_tree.frame_tree.frame.id;
                *self.source_map_frame_id.lock().await = Some(frame_id.clone());
                frame_id
            }
        };
        let mut params = NetworkLoadNetworkResourceParams::new(
            resolved_url.to_owned(),
            NetworkLoadNetworkResourceOptions::new(false, true),
        );
        params.frame_id = Some(frame_id);
        let mut params = serde_json::to_value(params).expect("CDP resource parameters serialize");
        let resource = self
            .source_map_resources
            .register(&mut params)
            .map_err(|message| CdpRuntimeError::SourceMapProtocol {
                url: resolved_url.to_owned(),
                source: Box::new(CdpRuntimeError::Transport(message.to_owned())),
            })?;
        let loaded = tokio::time::timeout(
            SOURCE_MAP_RESOURCE_TIMEOUT,
            self.channel.call("Network.loadNetworkResource", params),
        )
        .await
        .map_err(|_| CdpRuntimeError::SourceMapProtocol {
            url: resolved_url.to_owned(),
            source: Box::new(CdpRuntimeError::Transport(
                "resource load timed out".to_owned(),
            )),
        })?
        .map_err(|error| CdpRuntimeError::SourceMapProtocol {
            url: resolved_url.to_owned(),
            source: Box::new(CdpRuntimeError::protocol(error)),
        })?;
        let loaded: crate::cdp::NetworkLoadNetworkResourcePageResult =
            serde_json::from_value(loaded["resource"].clone()).map_err(|error| {
                CdpRuntimeError::SourceMapProtocol {
                    url: resolved_url.to_owned(),
                    source: Box::new(CdpRuntimeError::Transport(error.to_string())),
                }
            })?;
        if !loaded.success {
            return Err(CdpRuntimeError::SourceMapLoadFailed {
                url: resolved_url.to_owned(),
                http_status_code: loaded.http_status_code,
                net_error_name: loaded.net_error_name,
            });
        }
        let stream = loaded
            .stream
            .ok_or_else(|| CdpRuntimeError::MissingSourceMapStream(resolved_url.to_owned()))?;
        let read_result = async {
            let mut bytes = Vec::new();
            loop {
                let chunk = tokio::time::timeout(
                    SOURCE_MAP_RESOURCE_TIMEOUT,
                    self.client
                        .io()
                        .read(stream.clone(), None, Some(8 * 1024 * 1024)),
                )
                .await
                .map_err(|_| CdpRuntimeError::SourceMapProtocol {
                    url: resolved_url.to_owned(),
                    source: Box::new(CdpRuntimeError::Transport(
                        "resource read timed out".to_owned(),
                    )),
                })?
                .map_err(|error| CdpRuntimeError::SourceMapProtocol {
                    url: resolved_url.to_owned(),
                    source: Box::new(CdpRuntimeError::protocol(error)),
                })?;
                if chunk.base64_encoded.unwrap_or(false) {
                    bytes.extend(
                        BASE64_STANDARD
                            .decode(chunk.data)
                            .map_err(CdpRuntimeError::DecodeSourceMapBase64)?,
                    );
                } else {
                    bytes.extend(chunk.data.into_bytes());
                }
                if chunk.eof {
                    break;
                }
            }
            Ok(bytes)
        }
        .await;
        let close_result =
            tokio::time::timeout(SOURCE_MAP_RESOURCE_TIMEOUT, self.client.io().close(stream))
                .await
                .map_err(|_| CdpRuntimeError::SourceMapProtocol {
                    url: resolved_url.to_owned(),
                    source: Box::new(CdpRuntimeError::Transport(
                        "resource close timed out".to_owned(),
                    )),
                })?
                .map_err(|error| CdpRuntimeError::SourceMapProtocol {
                    url: resolved_url.to_owned(),
                    source: Box::new(CdpRuntimeError::protocol(error)),
                });
        if close_result.is_ok() {
            resource.disarm();
        }
        match (read_result, close_result) {
            (Ok(bytes), Ok(_)) => Ok(bytes),
            (Err(read), Ok(_)) => Err(read),
            (Ok(_), Err(close)) => Err(close),
            (Err(read), Err(close)) => Err(CdpRuntimeError::SourceMapReadAndClose {
                read: Box::new(read),
                close: Box::new(close),
            }),
        }
    }

    fn direct_source_map_scheme(url: &str) -> bool {
        url::Url::parse(url).is_ok_and(|url| matches!(url.scheme(), "file" | "http" | "https"))
    }

    async fn load_source_map_direct(url: &str) -> Result<Vec<u8>, String> {
        let parsed = url::Url::parse(url).map_err(|error| error.to_string())?;
        match parsed.scheme() {
            "file" => {
                let path = parsed
                    .to_file_path()
                    .map_err(|_| format!("invalid file URL {url}"))?;
                tokio::fs::read(&path)
                    .await
                    .map_err(|error| format!("failed to read {}: {error}", path.display()))
            }
            "http" | "https" => {
                let response = reqwest::get(parsed)
                    .await
                    .map_err(|error| error.to_string())?
                    .error_for_status()
                    .map_err(|error| error.to_string())?;
                response
                    .bytes()
                    .await
                    .map(|bytes| bytes.to_vec())
                    .map_err(|error| error.to_string())
            }
            scheme => Err(format!("unsupported URL scheme {scheme:?}")),
        }
    }
}

/// A verbatim CDP notification observed on one session, independent of whether the debugger
/// engine reducer recognizes `method`. Used to mirror every target event to relay consumers.
#[derive(Clone, Debug)]
pub struct RawCdpEvent {
    pub session: SessionKey,
    pub method: String,
    pub params: Value,
}

#[derive(Debug)]
pub enum CdpRuntimeEvent {
    ScriptParsed {
        session: SessionKey,
        params: DebuggerScriptParsedParams,
    },
    Paused {
        session: SessionKey,
        params: DebuggerPausedParams,
    },
    Resumed {
        session: SessionKey,
    },
    Console {
        session: SessionKey,
        params: RuntimeConsoleApicalledParams,
    },
    Other {
        session: SessionKey,
        method: String,
        params: Value,
    },
}

impl CdpRuntimeEvent {
    pub fn into_input(
        self,
        pause_epoch: Option<u64>,
    ) -> Result<Option<Input>, CdpRuntimeEventError> {
        match self {
            Self::ScriptParsed { session, params } => Ok(Some(Input::ScriptParsedWithProvenance {
                provenance: crate::service_api::ScriptProvenance {
                    execution_context_id: Some(params.execution_context_id),
                    frame_id: params
                        .execution_context_aux_data
                        .as_ref()
                        .and_then(|data| data.get("frameId"))
                        .and_then(Value::as_str)
                        .map(str::to_owned),
                    execution_context_aux_data: params
                        .execution_context_aux_data
                        .map(|data| serde_json::Value::Object(data.into_iter().collect())),
                },
                session,
                script_id: params.script_id,
                url: params.url,
                hash: params.hash,
                source_map_url: params.source_map_url.filter(|url| !url.is_empty()),
            })),
            Self::Paused { session, params } => {
                let hit_breakpoint = params
                    .hit_breakpoints
                    .as_ref()
                    .is_some_and(|breakpoints| !breakpoints.is_empty());
                let frames = params
                    .call_frames
                    .into_iter()
                    .map(|frame| {
                        Ok(RawFrame {
                            call_frame_id: frame.call_frame_id,
                            function_name: frame.function_name,
                            script_id: frame.location.script_id,
                            position: Position {
                                line: to_u32("lineNumber", frame.location.line_number)?,
                                column: to_u32(
                                    "columnNumber",
                                    frame.location.column_number.unwrap_or(0),
                                )?,
                            },
                            scopes: frame
                                .scope_chain
                                .into_iter()
                                .filter_map(|scope| {
                                    let object_id = scope.object.object_id?;
                                    let kind = serde_json::to_value(scope.r#type)
                                        .ok()?
                                        .as_str()?
                                        .to_owned();
                                    Some(RawScope {
                                        kind,
                                        name: scope.name,
                                        object_id,
                                    })
                                })
                                .collect(),
                        })
                    })
                    .collect::<Result<_, CdpRuntimeEventError>>()?;
                let reason = serde_json::to_value(params.reason)
                    .map_err(CdpRuntimeEventError::Serialize)?
                    .as_str()
                    .unwrap_or("other")
                    .to_owned();
                Ok(Some(Input::Paused {
                    session,
                    reason: if reason == "Break on start" && hit_breakpoint {
                        "breakpoint".to_owned()
                    } else {
                        reason
                    },
                    frames,
                }))
            }
            Self::Resumed { session } => {
                let pause_epoch = pause_epoch.ok_or(CdpRuntimeEventError::MissingPauseEpoch)?;
                Ok(Some(Input::Resumed {
                    session,
                    pause_epoch,
                }))
            }
            Self::Console { .. } => Ok(None),
            Self::Other { .. } => Ok(None),
        }
    }
}

#[derive(Clone)]
struct CdpEventHandler {
    session: SessionKey,
    sender: mpsc::UnboundedSender<Result<CdpRuntimeEvent, CdpRuntimeEventError>>,
    heap_snapshot: Arc<Mutex<Option<HeapSnapshotWriter>>>,
    heap_snapshot_progress: watch::Sender<Option<HeapSnapshotStreamProgress>>,
    raw_events: broadcast::Sender<RawCdpEvent>,
    raw_event_history: Arc<std::sync::Mutex<Vec<RawCdpEvent>>>,
}

impl CdpEventHandler {
    fn into_dispatcher(self) -> CdpEventDispatcher {
        let handler = Arc::new(self.clone());
        let router = linkrpc::binding::InterfaceRouter::new();
        crate::cdp::debugger_events::DOMAIN
            .register(
                &router,
                Arc::new(crate::cdp::debugger_events::DebuggerEventsServer::new(
                    handler.clone(),
                )),
            )
            .expect("generated Debugger events binding is valid");
        crate::cdp::runtime_events::DOMAIN
            .register(
                &router,
                Arc::new(crate::cdp::runtime_events::RuntimeEventsServer::new(
                    handler.clone(),
                )),
            )
            .expect("generated Runtime events binding is distinct");
        crate::cdp::heap_profiler_events::DOMAIN
            .register(
                &router,
                Arc::new(crate::cdp::heap_profiler_events::HeapProfilerEventsServer::new(handler)),
            )
            .expect("generated HeapProfiler events binding is distinct");
        CdpEventDispatcher {
            handler: self,
            router,
        }
    }

    fn into_channel(self, transport: impl MessageTransport + 'static) -> Channel {
        let dispatcher = self.into_dispatcher();
        Channel::new(
            Box::new(HeapSnapshotTransport {
                transport: Box::new(transport),
                handler: dispatcher.clone(),
            }),
            Box::new(dispatcher),
        )
    }
}

#[derive(Clone)]
struct CdpEventDispatcher {
    handler: CdpEventHandler,
    router: linkrpc::binding::InterfaceRouter,
}

struct HeapSnapshotTransport {
    transport: Box<dyn MessageTransport>,
    handler: CdpEventDispatcher,
}

#[async_trait]
impl MessageTransport for HeapSnapshotTransport {
    async fn send(&self, message: JsonRpcMessage) -> Result<(), TransportError> {
        self.transport.send(message).await
    }

    async fn recv(&self) -> Option<JsonRpcMessage> {
        loop {
            match self.transport.recv().await? {
                JsonRpcMessage::Notification(notification)
                    if matches!(
                        notification.method.as_str(),
                        "HeapProfiler.addHeapSnapshotChunk"
                            | "HeapProfiler.reportHeapSnapshotProgress"
                    ) =>
                {
                    // Channel dispatches notifications concurrently with responses. Consume heap
                    // events in wire order before it can resolve takeHeapSnapshot and close the writer.
                    self.handler
                        .handle_notification(
                            notification.method,
                            notification.params.unwrap_or(Value::Null),
                        )
                        .await;
                }
                message => return Some(message),
            }
        }
    }
}

#[async_trait]
impl crate::cdp::debugger_events::DebuggerEventsService for CdpEventHandler {
    async fn script_parsed(
        &self,
        _ctx: &linkrpc::prelude::CallCtx,
        script_id: crate::cdp::RuntimeScriptId,
        url: String,
        start_line: i64,
        start_column: i64,
        end_line: i64,
        end_column: i64,
        execution_context_id: crate::cdp::RuntimeExecutionContextId,
        hash: String,
        build_id: Option<String>,
        execution_context_aux_data: Option<std::collections::HashMap<String, Value>>,
        is_live_edit: Option<bool>,
        source_map_url: Option<String>,
        has_source_url: Option<bool>,
        is_module: Option<bool>,
        length: Option<i64>,
        stack_trace: Option<crate::cdp::RuntimeStackTrace>,
        code_offset: Option<i64>,
        script_language: Option<crate::cdp::DebuggerScriptLanguage>,
        debug_symbols: Option<Vec<crate::cdp::DebuggerDebugSymbols>>,
        embedder_name: Option<String>,
        resolved_breakpoints: Option<Vec<crate::cdp::DebuggerResolvedBreakpoint>>,
    ) -> Result<(), JsonRpcError> {
        let params = DebuggerScriptParsedParams {
            script_id,
            url,
            start_line,
            start_column,
            end_line,
            end_column,
            execution_context_id,
            hash,
            build_id,
            execution_context_aux_data,
            is_live_edit,
            source_map_url,
            has_source_url,
            is_module,
            length,
            stack_trace,
            code_offset,
            script_language,
            debug_symbols,
            embedder_name,
            resolved_breakpoints,
        };
        let _ = self.sender.send(Ok(CdpRuntimeEvent::ScriptParsed {
            session: self.session.clone(),
            params,
        }));
        Ok(())
    }

    async fn paused(
        &self,
        _ctx: &linkrpc::prelude::CallCtx,
        call_frames: Vec<crate::cdp::DebuggerCallFrame>,
        reason: String,
        data: Option<std::collections::HashMap<String, Value>>,
        hit_breakpoints: Option<Vec<String>>,
        async_stack_trace: Option<crate::cdp::RuntimeStackTrace>,
        async_stack_trace_id: Option<crate::cdp::RuntimeStackTraceId>,
        async_call_stack_trace_id: Option<crate::cdp::RuntimeStackTraceId>,
    ) -> Result<(), JsonRpcError> {
        let params = DebuggerPausedParams {
            call_frames,
            reason,
            data,
            hit_breakpoints,
            async_stack_trace,
            async_stack_trace_id,
            async_call_stack_trace_id,
        };
        let _ = self.sender.send(Ok(CdpRuntimeEvent::Paused {
            session: self.session.clone(),
            params,
        }));
        Ok(())
    }

    async fn resumed(&self, _ctx: &linkrpc::prelude::CallCtx) -> Result<(), JsonRpcError> {
        let _ = self.sender.send(Ok(CdpRuntimeEvent::Resumed {
            session: self.session.clone(),
        }));
        Ok(())
    }
}

#[async_trait]
impl crate::cdp::runtime_events::RuntimeEventsService for CdpEventHandler {
    async fn console_apicalled(
        &self,
        _ctx: &linkrpc::prelude::CallCtx,
        params: RuntimeConsoleApicalledParams,
    ) -> Result<(), JsonRpcError> {
        let _ = self.sender.send(Ok(CdpRuntimeEvent::Console {
            session: self.session.clone(),
            params,
        }));
        Ok(())
    }
}

#[async_trait]
impl crate::cdp::heap_profiler_events::HeapProfilerEventsService for CdpEventHandler {
    async fn add_heap_snapshot_chunk(
        &self,
        _ctx: &linkrpc::prelude::CallCtx,
        chunk: String,
    ) -> Result<(), JsonRpcError> {
        let mut snapshot = self.heap_snapshot.lock().await;
        if let Some(snapshot) = snapshot.as_mut()
            && snapshot.write_error.is_none()
        {
            match snapshot.file.write_all(chunk.as_bytes()).await {
                Ok(()) => {
                    snapshot.bytes_written =
                        snapshot.bytes_written.saturating_add(chunk.len() as u64);
                    let mut progress = self
                        .heap_snapshot_progress
                        .borrow()
                        .clone()
                        .unwrap_or_default();
                    progress.bytes_written = snapshot.bytes_written;
                    self.heap_snapshot_progress.send_replace(Some(progress));
                }
                Err(error) => snapshot.write_error = Some(error),
            }
        }
        Ok(())
    }

    async fn report_heap_snapshot_progress(
        &self,
        _ctx: &linkrpc::prelude::CallCtx,
        done: i64,
        total: i64,
        finished: Option<bool>,
    ) -> Result<(), JsonRpcError> {
        let mut snapshot = self.heap_snapshot.lock().await;
        let bytes_written = snapshot
            .as_ref()
            .map_or(0, |snapshot| snapshot.bytes_written);
        if let Some(snapshot) = snapshot.as_mut()
            && snapshot.taking_finished_at.is_none()
            && (finished == Some(true) || (total > 0 && done >= total))
        {
            snapshot.taking_finished_at = Some(Instant::now());
        }
        self.heap_snapshot_progress
            .send_replace(Some(HeapSnapshotStreamProgress {
                done,
                total,
                finished,
                bytes_written,
            }));
        Ok(())
    }
}

#[async_trait]
impl RequestHandler for CdpEventDispatcher {
    async fn handle_request(&self, method: String, _params: Value) -> Result<Value, JsonRpcError> {
        Err(JsonRpcError::new(
            -32601,
            format!("unexpected browser request: {method}"),
        ))
    }

    async fn handle_notification(&self, method: String, params: Value) {
        self.handler
            .dispatch_notification(&self.router, method, params)
            .await;
    }
}

impl CdpEventHandler {
    async fn dispatch_notification(
        &self,
        router: &linkrpc::binding::InterfaceRouter,
        method: String,
        params: Value,
    ) {
        {
            let mut history = self.raw_event_history.lock().unwrap();
            match method.as_str() {
                "Debugger.globalObjectCleared" => history.clear(),
                "Debugger.scriptParsed" | "Runtime.executionContextCreated" => {
                    history.push(RawCdpEvent {
                        session: self.session.clone(),
                        method: method.clone(),
                        params: params.clone(),
                    });
                }
                "Runtime.executionContextDestroyed" => {
                    let destroyed_id = params.get("executionContextId");
                    history.retain(|event| {
                        event.method != "Runtime.executionContextCreated"
                            || event.params.pointer("/context/id") != destroyed_id
                    });
                }
                "Runtime.executionContextsCleared" => {
                    history.retain(|event| event.method != "Runtime.executionContextCreated");
                }
                _ => {}
            }
        }
        // Skip the clone entirely when nobody subscribes to raw events (the common case);
        // heap snapshot chunk notifications in particular can carry megabytes of JSON.
        if self.raw_events.receiver_count() > 0 {
            let _ = self.raw_events.send(RawCdpEvent {
                session: self.session.clone(),
                method: method.clone(),
                params: params.clone(),
            });
        }
        let params = if method == "Debugger.scriptParsed" {
            normalize_script_parsed_params(params)
        } else {
            params
        };
        if !matches!(
            method.as_str(),
            "Debugger.scriptParsed"
                | "Debugger.paused"
                | "Debugger.resumed"
                | "Runtime.consoleAPICalled"
                | "HeapProfiler.addHeapSnapshotChunk"
                | "HeapProfiler.reportHeapSnapshotProgress"
        ) {
            let _ = self.sender.send(Ok(CdpRuntimeEvent::Other {
                session: self.session.clone(),
                method,
                params,
            }));
            return;
        }
        match router.dispatch_notification(&method, params.clone()).await {
            Ok(true) => {}
            Ok(false) => {
                let _ = self.sender.send(Ok(CdpRuntimeEvent::Other {
                    session: self.session.clone(),
                    method,
                    params,
                }));
            }
            Err(error) => {
                if matches!(
                    method.as_str(),
                    "HeapProfiler.addHeapSnapshotChunk" | "HeapProfiler.reportHeapSnapshotProgress"
                ) {
                    record_heap_snapshot_error(
                        &self.heap_snapshot,
                        format!("{method}: {}", error.message),
                    )
                    .await;
                }
                let _ = self.sender.send(Err(CdpRuntimeEventError::Decode {
                    method,
                    message: error.message,
                }));
            }
        }
    }
}

fn normalize_script_parsed_params(mut params: Value) -> Value {
    let Some(object) = params.as_object_mut() else {
        return params;
    };
    if let Some(debug_symbols) = object.remove("debugSymbols") {
        let symbols = match debug_symbols {
            Value::Object(symbol) if symbol.get("type").and_then(Value::as_str) == Some("None") => {
                Vec::new()
            }
            Value::Object(symbol) => vec![Value::Object(symbol)],
            Value::Array(symbols) => symbols
                .into_iter()
                .filter(|symbol| symbol.get("type").and_then(Value::as_str) != Some("None"))
                .collect(),
            other => {
                object.insert("debugSymbols".to_owned(), other);
                return params;
            }
        };
        if !symbols.is_empty() {
            object.insert("debugSymbols".to_owned(), Value::Array(symbols));
        }
    }
    params
}

async fn remove_temporary_file(path: &Path) {
    if let Err(error) = tokio::fs::remove_file(path).await
        && error.kind() != ErrorKind::NotFound
    {
        eprintln!(
            "failed to remove temporary heap snapshot {}: {error}",
            path.display()
        );
    }
}

async fn replace_file_preserving_previous(
    temporary: &Path,
    destination: &Path,
) -> std::io::Result<()> {
    static BACKUP_ID: AtomicU64 = AtomicU64::new(1);
    match tokio::fs::rename(temporary, destination).await {
        Ok(()) => return Ok(()),
        Err(error)
            if destination.exists()
                && matches!(
                    error.kind(),
                    ErrorKind::AlreadyExists | ErrorKind::PermissionDenied
                ) => {}
        Err(error) => return Err(error),
    }

    let file_name = destination
        .file_name()
        .ok_or_else(|| {
            std::io::Error::new(
                ErrorKind::InvalidInput,
                "heap snapshot destination must name a file",
            )
        })?
        .to_string_lossy();
    let backup = destination.with_file_name(format!(
        ".{file_name}.{}.{}.backup",
        std::process::id(),
        BACKUP_ID.fetch_add(1, Ordering::Relaxed)
    ));
    tokio::fs::rename(destination, &backup).await?;
    match tokio::fs::rename(temporary, destination).await {
        Ok(()) => {
            if let Err(error) = tokio::fs::remove_file(&backup).await {
                eprintln!(
                    "failed to remove replaced heap snapshot backup {}: {error}",
                    backup.display()
                );
            }
            Ok(())
        }
        Err(replacement_error) => match tokio::fs::rename(&backup, destination).await {
            Ok(()) => Err(replacement_error),
            Err(restore_error) => Err(std::io::Error::other(format!(
                "failed to install heap snapshot ({replacement_error}) and restore the previous snapshot ({restore_error}); previous data remains at {}",
                backup.display()
            ))),
        },
    }
}

async fn record_heap_snapshot_error(
    heap_snapshot: &Mutex<Option<HeapSnapshotWriter>>,
    message: String,
) {
    if let Some(snapshot) = heap_snapshot.lock().await.as_mut()
        && snapshot.write_error.is_none()
    {
        snapshot.write_error = Some(std::io::Error::new(ErrorKind::InvalidData, message));
    }
}

fn to_u32(field: &'static str, value: i64) -> Result<u32, CdpRuntimeEventError> {
    u32::try_from(value).map_err(|_| CdpRuntimeEventError::InvalidPosition { field, value })
}

fn decode_source_map_data_url(url: &str) -> Result<Vec<u8>, CdpRuntimeError> {
    let data = url
        .strip_prefix("data:")
        .ok_or_else(|| CdpRuntimeError::InvalidSourceMapDataUrl(url.to_owned()))?;
    let (metadata, payload) = data
        .split_once(',')
        .ok_or_else(|| CdpRuntimeError::InvalidSourceMapDataUrl(url.to_owned()))?;
    if metadata
        .split(';')
        .any(|component| component.eq_ignore_ascii_case("base64"))
    {
        BASE64_STANDARD
            .decode(payload)
            .map_err(CdpRuntimeError::DecodeSourceMapBase64)
    } else {
        Ok(percent_encoding::percent_decode_str(payload).collect())
    }
}

fn resolve_source_map_url(
    generated_url: &str,
    source_map_url: &str,
) -> Result<String, CdpRuntimeError> {
    if let Ok(url) = url::Url::parse(source_map_url) {
        return Ok(url.into());
    }

    let generated =
        url::Url::parse(generated_url).map_err(|source| CdpRuntimeError::InvalidSourceMapUrl {
            generated_url: generated_url.to_owned(),
            source_map_url: source_map_url.to_owned(),
            source,
        })?;
    generated
        .join(source_map_url)
        .map(String::from)
        .map_err(|source| CdpRuntimeError::InvalidSourceMapUrl {
            generated_url: generated_url.to_owned(),
            source_map_url: source_map_url.to_owned(),
            source,
        })
}

fn source_map_cache_path(script_hash: &str, resolved_url: &str) -> Option<PathBuf> {
    let directory = if let Some(path) = env::var_os("DBGJS_SOURCE_MAP_CACHE") {
        PathBuf::from(path)
    } else if let Some(state_file) = env::var_os("DBGJS_SERVICE_STATE") {
        PathBuf::from(state_file)
            .parent()
            .map(|parent| parent.join("source-map-cache"))?
    } else if let Some(path) = env::var_os("LOCALAPPDATA") {
        PathBuf::from(path).join("dbgjs").join("source-map-cache")
    } else if let Some(path) = env::var_os("XDG_CACHE_HOME") {
        PathBuf::from(path).join("dbgjs").join("source-map-cache")
    } else if let Some(path) = env::var_os("HOME") {
        PathBuf::from(path)
            .join(".cache")
            .join("dbgjs")
            .join("source-map-cache")
    } else {
        return None;
    };
    let mut hasher = Sha256::new();
    hasher.update(script_hash.as_bytes());
    hasher.update([0]);
    hasher.update(resolved_url.as_bytes());
    Some(directory.join(format!("{:x}.map", hasher.finalize())))
}

async fn write_source_map_cache(
    path: &Path,
    source_map: &SourceMapData,
) -> Result<(), std::io::Error> {
    const MAGIC: &[u8] = b"dbgjs-source-map-v1\n";
    static TEMPORARY_ID: AtomicU64 = AtomicU64::new(1);
    let Some(parent) = path.parent() else {
        return Err(std::io::Error::new(
            ErrorKind::InvalidInput,
            "source-map cache path has no parent",
        ));
    };
    tokio::fs::create_dir_all(parent).await?;
    let temporary = path.with_extension(format!(
        "{}.{}.tmp",
        std::process::id(),
        TEMPORARY_ID.fetch_add(1, Ordering::Relaxed)
    ));
    let digest = format!("{}\n", source_map.content_hash());
    let mut file = tokio::fs::File::create(&temporary).await?;
    file.write_all(MAGIC).await?;
    file.write_all(digest.as_bytes()).await?;
    file.write_all(source_map).await?;
    file.sync_data().await?;
    drop(file);
    match tokio::fs::rename(&temporary, path).await {
        Ok(()) => {
            if let Err(error) = tokio::fs::write(path.with_extension("access"), []).await {
                eprintln!(
                    "failed to create source-map cache access marker {}: {error}",
                    path.display()
                );
            }
            cleanup_source_map_cache(parent).await;
            Ok(())
        }
        Err(_error) if path.exists() => {
            tokio::fs::remove_file(&temporary).await?;
            Ok(())
        }

        Err(error) => {
            if let Err(remove_error) = tokio::fs::remove_file(&temporary).await
                && remove_error.kind() != ErrorKind::NotFound
            {
                eprintln!(
                    "failed to clean temporary source-map cache entry {}: {remove_error}",
                    temporary.display()
                );
            }
            Err(error)
        }
    }
}

async fn cleanup_source_map_cache(directory: &Path) {
    const MAX_AGE: Duration = Duration::from_secs(30 * 24 * 60 * 60);
    const MAX_ENTRIES: usize = 64;
    const MAX_BYTES: u64 = 2 * 1024 * 1024 * 1024;

    let Ok(mut directory_entries) = tokio::fs::read_dir(directory).await else {
        return;
    };
    let now = SystemTime::now();
    let mut entries = Vec::new();
    while let Ok(Some(entry)) = directory_entries.next_entry().await {
        let path = entry.path();
        if path
            .extension()
            .is_some_and(|extension| extension == "access")
        {
            if !path.with_extension("map").exists()
                && let Err(error) = tokio::fs::remove_file(&path).await
                && error.kind() != ErrorKind::NotFound
            {
                eprintln!(
                    "failed to remove orphaned source-map access marker {}: {error}",
                    path.display()
                );
            }
            continue;
        }
        let Ok(metadata) = entry.metadata().await else {
            continue;
        };
        if !metadata.is_file() {
            continue;
        }
        let access_path = path.with_extension("access");
        let modified = tokio::fs::metadata(&access_path)
            .await
            .and_then(|metadata| metadata.modified())
            .or_else(|_| metadata.modified())
            .unwrap_or(SystemTime::UNIX_EPOCH);
        let expired = now.duration_since(modified).is_ok_and(|age| age > MAX_AGE);
        let temporary = path.extension().is_some_and(|extension| extension == "tmp");
        if expired || temporary {
            if let Err(error) = tokio::fs::remove_file(&path).await
                && error.kind() != ErrorKind::NotFound
            {
                eprintln!(
                    "failed to remove stale source-map cache entry {}: {error}",
                    path.display()
                );
            }
            let _ = tokio::fs::remove_file(access_path).await;
            continue;
        }
        entries.push((modified, metadata.len(), path, access_path));
    }
    entries.sort_by_key(|(modified, _, _, _)| *modified);
    let mut total_bytes = entries.iter().map(|(_, size, _, _)| *size).sum::<u64>();
    let remove_count = entries.len().saturating_sub(MAX_ENTRIES);
    for (index, (_, size, path, access_path)) in entries.into_iter().enumerate() {
        if index >= remove_count && total_bytes <= MAX_BYTES {
            break;
        }
        match tokio::fs::remove_file(&path).await {
            Ok(()) => {
                total_bytes = total_bytes.saturating_sub(size);
                let _ = tokio::fs::remove_file(access_path).await;
            }
            Err(error) if error.kind() == ErrorKind::NotFound => {
                total_bytes = total_bytes.saturating_sub(size);
                let _ = tokio::fs::remove_file(access_path).await;
            }
            Err(error) => eprintln!(
                "failed to prune source-map cache entry {}: {error}",
                path.display()
            ),
        }
    }
}

fn decode_source_map_cache(mut bytes: Vec<u8>) -> Option<SourceMapData> {
    const MAGIC: &[u8] = b"dbgjs-source-map-v1\n";
    let remainder = bytes.strip_prefix(MAGIC)?;
    let newline = remainder.iter().position(|byte| *byte == b'\n')?;
    let expected: [u8; 64] = remainder[..newline].try_into().ok()?;
    let payload_start = MAGIC.len() + newline + 1;
    drop(bytes.drain(..payload_start));
    let source_map = SourceMapData::new(bytes);
    if source_map.content_hash().to_string().as_bytes() != expected.as_slice() {
        return None;
    }
    source_map.is_supported().then_some(source_map)
}

#[derive(Debug, thiserror::Error)]
pub enum CdpRuntimeError {
    #[error(transparent)]
    WebSocket(#[from] CdpWebSocketError),
    #[error("failed to open CDP session: {0}")]
    OpenSession(MuxError),
    #[error("CDP transport failed: {0}")]
    Transport(String),
    #[error("CDP protocol error {code}: {message}")]
    Protocol {
        code: i64,
        message: String,
        data: Option<Value>,
    },
    #[error("CDP returned an invalid confirmed breakpoint location {line}:{column:?}")]
    InvalidBreakpointLocation { line: i64, column: Option<i64> },
    #[error(
        "failed to resolve source-map URL {source_map_url:?} against generated URL {generated_url:?}: {source}"
    )]
    InvalidSourceMapUrl {
        generated_url: String,
        source_map_url: String,
        source: url::ParseError,
    },
    #[error("invalid source-map data URL: {0}")]
    InvalidSourceMapDataUrl(String),
    #[error("invalid base64 source map: {0}")]
    DecodeSourceMapBase64(base64::DecodeError),
    #[error(
        "failed to load source map {url}: HTTP status {http_status_code:?}, network error {net_error_name:?}"
    )]
    SourceMapLoadFailed {
        url: String,
        http_status_code: Option<f64>,
        net_error_name: Option<String>,
    },
    #[error("failed to load source map {url} through CDP: {source}")]
    SourceMapProtocol {
        url: String,
        source: Box<CdpRuntimeError>,
    },
    #[error(
        "failed to load source map {url} through CDP ({cdp}) and direct resource loading ({direct})"
    )]
    SourceMapFallbackFailed {
        url: String,
        cdp: Box<CdpRuntimeError>,
        direct: String,
    },
    #[error("CDP returned no stream for successfully loaded source map {0}")]
    MissingSourceMapStream(String),
    #[error("failed to read source map stream ({read}) and close it ({close})")]
    SourceMapReadAndClose {
        read: Box<CdpRuntimeError>,
        close: Box<CdpRuntimeError>,
    },
}

impl CdpRuntimeError {
    fn is_source_map_unavailable(&self) -> bool {
        matches!(
            self,
            Self::InvalidSourceMapUrl { .. }
                | Self::InvalidSourceMapDataUrl(_)
                | Self::DecodeSourceMapBase64(_)
                | Self::SourceMapLoadFailed { .. }
                | Self::SourceMapProtocol { .. }
                | Self::SourceMapFallbackFailed { .. }
                | Self::MissingSourceMapStream(_)
                | Self::SourceMapReadAndClose { .. }
        )
    }

    fn protocol(error: JsonRpcError) -> Self {
        Self::Protocol {
            code: error.code,
            message: error.message,
            data: error.data,
        }
    }
}

#[cfg(test)]
#[path = "cdp_source_map_resource_tests.rs"]
mod source_map_resource_tests;

#[cfg(test)]
#[path = "cdp_heap_snapshot_tests.rs"]
mod heap_snapshot_tests;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn script_events_preserve_execution_context_and_owning_frame() {
        let params: DebuggerScriptParsedParams = serde_json::from_value(serde_json::json!({
            "scriptId": "7", "url": "https://example.test/app.js",
            "startLine": 0, "startColumn": 0, "endLine": 1, "endColumn": 0,
            "executionContextId": 23, "hash": "captured",
            "executionContextAuxData": {"frameId": "child-frame", "isDefault": true}
        }))
        .unwrap();
        let session = SessionKey {
            connection_generation: 1,
            session_id: "test".into(),
        };
        let input = CdpRuntimeEvent::ScriptParsed { session, params }
            .into_input(None)
            .unwrap()
            .unwrap();
        let Input::ScriptParsedWithProvenance { provenance, .. } = input else {
            panic!("script event lost provenance")
        };
        assert_eq!(provenance.execution_context_id, Some(23));
        assert_eq!(provenance.frame_id.as_deref(), Some("child-frame"));
        assert_eq!(
            provenance.execution_context_aux_data.unwrap()["isDefault"],
            true
        );
    }

    #[test]
    fn accepts_node_single_debug_symbols_object() {
        let params = serde_json::json!({
            "scriptId": "1",
            "url": "wasm://wasm/example",
            "startLine": 0,
            "startColumn": 0,
            "endLine": 0,
            "endColumn": 1,
            "executionContextId": 1,
            "hash": "hash",
            "debugSymbols": { "type": "None" }
        });

        let parsed: DebuggerScriptParsedParams =
            serde_json::from_value(normalize_script_parsed_params(params)).unwrap();

        assert!(parsed.debug_symbols.is_none());
    }

    #[test]
    fn resolves_relative_source_maps_against_the_generated_script() {
        assert_eq!(
            resolve_source_map_url(
                "https://cdn.example.com/assets/app.js",
                "../maps/app.js.map"
            )
            .unwrap(),
            "https://cdn.example.com/maps/app.js.map"
        );
    }

    #[test]
    fn preserves_absolute_source_map_urls() {
        assert_eq!(
            resolve_source_map_url(
                "https://cdn.example.com/assets/app.js",
                "https://maps.example.com/app.js.map"
            )
            .unwrap(),
            "https://maps.example.com/app.js.map"
        );
    }

    #[test]
    fn normalizes_backslashes_in_absolute_source_map_urls() {
        assert_eq!(
            resolve_source_map_url(
                "vscode-file://vscode-app/c:/resources/app/out/vs/workbench/workbench.js",
                "https://main.vscode-cdn.net/sourcemaps/commit/core/vs\\workbench\\workbench.js.map"
            )
            .unwrap(),
            "https://main.vscode-cdn.net/sourcemaps/commit/core/vs/workbench/workbench.js.map"
        );
    }

    #[tokio::test]
    async fn loads_source_maps_directly_from_file_urls() {
        let path = std::env::temp_dir().join(format!(
            "dbgjs-direct-source-map-{}.map",
            std::process::id()
        ));
        let source_map = br#"{"version":3,"sources":[],"names":[],"mappings":""}"#;
        tokio::fs::write(&path, source_map).await.unwrap();
        let url = url::Url::from_file_path(&path).unwrap();

        let loaded = CdpDebuggerSession::load_source_map_direct(url.as_str())
            .await
            .unwrap();

        assert_eq!(loaded, source_map);
        tokio::fs::remove_file(path).await.unwrap();
    }

    #[test]
    fn source_map_cache_identity_includes_script_hash_and_url() {
        let first = source_map_cache_path("script-a", "https://example.com/app.js.map").unwrap();
        let changed_script =
            source_map_cache_path("script-b", "https://example.com/app.js.map").unwrap();
        let changed_url =
            source_map_cache_path("script-a", "https://example.com/other.js.map").unwrap();
        assert_ne!(first, changed_script);
        assert_ne!(first, changed_url);
    }

    #[test]
    fn source_map_cache_payload_is_content_verified() {
        let source_map = br#"{"version":3,"sources":[],"names":[],"mappings":""}"#;
        let mut cached = b"dbgjs-source-map-v1\n".to_vec();
        cached.extend(format!("{:x}\n", Sha256::digest(source_map)).as_bytes());
        cached.extend(source_map);
        assert_eq!(
            decode_source_map_cache(cached.clone()).as_deref(),
            Some(source_map.as_slice())
        );
        *cached.last_mut().unwrap() ^= 1;
        assert!(decode_source_map_cache(cached).is_none());

        let invalid = b"<html>temporary CDN error</html>";
        let mut cached = b"dbgjs-source-map-v1\n".to_vec();
        cached.extend(format!("{:x}\n", Sha256::digest(invalid)).as_bytes());
        cached.extend(invalid);
        assert!(decode_source_map_cache(cached).is_none());
    }

    #[tokio::test]
    async fn source_map_cache_reuses_the_digest_without_changing_the_disk_format() {
        let directory = std::env::temp_dir().join(format!(
            "dbgjs-source-map-digest-test-{}-{}",
            std::process::id(),
            SystemTime::now()
                .duration_since(SystemTime::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let path = directory.join("source.map");
        let bytes = br#"{"version":3,"sources":[],"names":[],"mappings":""}"#;
        let source_map = SourceMapData::new(bytes.as_slice());
        assert!(source_map.is_supported());
        write_source_map_cache(&path, &source_map).await.unwrap();
        let cached = tokio::fs::read(&path).await.unwrap();
        let mut expected =
            format!("dbgjs-source-map-v1\n{:x}\n", Sha256::digest(bytes)).into_bytes();
        expected.extend(bytes);
        assert_eq!(cached, expected);
        let restored = decode_source_map_cache(cached).unwrap();
        assert_eq!(restored.content_hash(), source_map.content_hash());
        assert_eq!(restored, source_map);
        tokio::fs::remove_dir_all(directory).await.unwrap();
    }

    #[tokio::test]
    async fn source_map_cache_cleanup_bounds_retained_hashes() {
        let directory = std::env::temp_dir().join(format!(
            "dbgjs-source-map-cache-test-{}-{}",
            std::process::id(),
            SystemTime::now()
                .duration_since(SystemTime::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        tokio::fs::create_dir_all(&directory).await.unwrap();
        for index in 0..65 {
            tokio::fs::write(directory.join(format!("{index}.map")), [index as u8])
                .await
                .unwrap();
            tokio::fs::write(directory.join(format!("{index}.access")), [])
                .await
                .unwrap();
        }
        cleanup_source_map_cache(&directory).await;
        let mut entries = tokio::fs::read_dir(&directory).await.unwrap();
        let mut maps = 0;
        while let Some(entry) = entries.next_entry().await.unwrap() {
            maps += usize::from(
                entry
                    .path()
                    .extension()
                    .is_some_and(|extension| extension == "map"),
            );
        }
        assert_eq!(maps, 64);
        tokio::fs::remove_dir_all(&directory).await.unwrap();
    }

    #[tokio::test]
    async fn heap_snapshot_replacement_preserves_complete_new_content() {
        let directory = std::env::temp_dir().join(format!(
            "dbgjs-heap-replace-test-{}-{}",
            std::process::id(),
            SystemTime::now()
                .duration_since(SystemTime::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        tokio::fs::create_dir_all(&directory).await.unwrap();
        let destination = directory.join("snapshot.heapsnapshot");
        let temporary = directory.join("snapshot.tmp");
        tokio::fs::write(&destination, b"previous").await.unwrap();
        tokio::fs::write(&temporary, b"replacement").await.unwrap();
        replace_file_preserving_previous(&temporary, &destination)
            .await
            .unwrap();
        assert_eq!(tokio::fs::read(&destination).await.unwrap(), b"replacement");
        assert!(!temporary.exists());
        tokio::fs::remove_dir_all(&directory).await.unwrap();
    }

    #[tokio::test]
    async fn heap_snapshot_notifications_stream_to_disk_and_report_progress() {
        let path = std::env::temp_dir().join(format!(
            "dbgjs-heap-stream-{}-{}.tmp",
            std::process::id(),
            AtomicU64::new(1).fetch_add(1, Ordering::Relaxed)
        ));
        let file = tokio::fs::File::create(&path).await.unwrap();
        let heap_snapshot = Arc::new(Mutex::new(Some(HeapSnapshotWriter {
            destination: path.clone(),
            temporary: path.clone(),
            file,
            started_at: Instant::now(),
            taking_finished_at: None,
            bytes_written: 0,
            write_error: None,
        })));
        let (heap_snapshot_progress, _) = watch::channel(None);
        let (sender, mut events) = mpsc::unbounded_channel();
        let (raw_events, _) = broadcast::channel(RAW_EVENT_BUFFER);
        let raw_event_history = Arc::new(std::sync::Mutex::new(Vec::new()));
        let handler = CdpEventHandler {
            session: SessionKey {
                connection_generation: 1,
                session_id: "session".to_owned(),
            },
            sender,
            heap_snapshot: heap_snapshot.clone(),
            heap_snapshot_progress: heap_snapshot_progress.clone(),
            raw_events,
            raw_event_history,
        }
        .into_dispatcher();

        handler
            .handle_notification(
                "HeapProfiler.addHeapSnapshotChunk".to_owned(),
                serde_json::json!({ "chunk": "{\"snapshot\":{}}" }),
            )
            .await;
        handler
            .handle_notification(
                "HeapProfiler.reportHeapSnapshotProgress".to_owned(),
                serde_json::json!({ "done": 7, "total": 10, "finished": false }),
            )
            .await;

        heap_snapshot
            .lock()
            .await
            .as_mut()
            .unwrap()
            .file
            .flush()
            .await
            .unwrap();
        assert_eq!(
            tokio::fs::read_to_string(&path).await.unwrap(),
            "{\"snapshot\":{}}"
        );
        assert_eq!(
            heap_snapshot_progress.borrow().clone().unwrap(),
            HeapSnapshotStreamProgress {
                done: 7,
                total: 10,
                finished: Some(false),
                bytes_written: 15,
            }
        );
        assert!(events.try_recv().is_err());

        handler
            .handle_notification(
                "HeapProfiler.reportHeapSnapshotProgress".to_owned(),
                serde_json::json!({ "done": 10, "total": 10, "finished": true }),
            )
            .await;
        assert!(
            heap_snapshot
                .lock()
                .await
                .as_ref()
                .unwrap()
                .taking_finished_at
                .is_some()
        );

        drop(heap_snapshot.lock().await.take());
        tokio::fs::remove_file(path).await.unwrap();
    }

    #[tokio::test]
    async fn raw_events_mirror_every_notification_including_untyped_ones() {
        let heap_snapshot = Arc::new(Mutex::new(None));
        let (heap_snapshot_progress, _) = watch::channel(None);
        let (sender, mut events) = mpsc::unbounded_channel();
        let (raw_events, _) = broadcast::channel(RAW_EVENT_BUFFER);
        let raw_event_history = Arc::new(std::sync::Mutex::new(Vec::new()));
        let handler = CdpEventHandler {
            session: SessionKey {
                connection_generation: 1,
                session_id: "session".to_owned(),
            },
            sender,
            heap_snapshot,
            heap_snapshot_progress,
            raw_events: raw_events.clone(),
            raw_event_history,
        }
        .into_dispatcher();
        let mut subscriber = raw_events.subscribe();

        // A method the reducer does not recognize (`CdpRuntimeEvent::Other`) must still be
        // mirrored verbatim, not only the typed subset `into_input` understands.
        handler
            .handle_notification(
                "Page.customSignal".to_owned(),
                serde_json::json!({ "flag": true }),
            )
            .await;
        let untyped = subscriber.recv().await.unwrap();
        assert_eq!(untyped.method, "Page.customSignal");
        assert_eq!(untyped.params, serde_json::json!({ "flag": true }));
        assert_eq!(untyped.session.session_id, "session");
        assert!(matches!(
            events.try_recv().unwrap().unwrap(),
            CdpRuntimeEvent::Other { .. }
        ));

        // A method the reducer *does* recognize must be mirrored raw as well as forwarded to
        // the typed event channel: relay consumers do not depend on the reducer's behavior.
        handler
            .handle_notification("Debugger.resumed".to_owned(), serde_json::json!({}))
            .await;
        let typed = subscriber.recv().await.unwrap();
        assert_eq!(typed.method, "Debugger.resumed");
        assert!(matches!(
            events.try_recv().unwrap().unwrap(),
            CdpRuntimeEvent::Resumed { .. }
        ));

        // Recognized generated events still report typed decode failures instead of silently
        // degrading to `Other`.
        handler
            .handle_notification(
                "Debugger.paused".to_owned(),
                serde_json::json!({ "reason": "other" }),
            )
            .await;
        assert!(matches!(
            events.try_recv().unwrap(),
            Err(CdpRuntimeEventError::Decode { ref method, .. })
                if method == "Debugger.paused"
        ));
        assert_eq!(subscriber.recv().await.unwrap().method, "Debugger.paused");

        // Subscribing late still only observes events sent afterward (broadcast semantics),
        // and a second independent subscriber receives its own copy of the same event.
        let mut second_subscriber = raw_events.subscribe();
        handler
            .handle_notification("Network.loadingFinished".to_owned(), serde_json::json!({}))
            .await;
        assert_eq!(
            subscriber.recv().await.unwrap().method,
            "Network.loadingFinished"
        );
        assert_eq!(
            second_subscriber.recv().await.unwrap().method,
            "Network.loadingFinished"
        );
        assert!(matches!(
            events.try_recv().unwrap().unwrap(),
            CdpRuntimeEvent::Other { ref method, .. }
                if method == "Network.loadingFinished"
        ));
    }
}

#[derive(Debug, thiserror::Error)]
pub enum CdpRuntimeEventError {
    #[error("failed to deserialize {method}: {source}")]
    Deserialize {
        method: String,
        source: serde_json::Error,
    },
    #[error("failed to decode {method}: {message}")]
    Decode { method: String, message: String },
    #[error("failed to serialize CDP event field: {0}")]
    Serialize(serde_json::Error),
    #[error("CDP event field {field} has invalid position {value}")]
    InvalidPosition { field: &'static str, value: i64 },
    #[error("Debugger.resumed requires the reducer's current pause epoch")]
    MissingPauseEpoch,
}
