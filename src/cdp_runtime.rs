use std::env;
use std::io::ErrorKind;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

use async_trait::async_trait;
use base64::Engine as _;
use base64::engine::general_purpose::STANDARD as BASE64_STANDARD;
use hubrpc::connection::channel::{Channel, RejectingHandler, RequestHandler};
use hubrpc::prelude::{JsonRpcError, MuxError};
use serde::de::DeserializeOwned;
use serde_json::Value;
use sha2::{Digest, Sha256};
use tokio::io::AsyncWriteExt;
use tokio::sync::{Mutex, mpsc};

use crate::cdp::{
    CdpClient, DebuggerEnableParams, DebuggerGetScriptSourceParams, DebuggerLocation,
    DebuggerPausedParams, DebuggerRemoveBreakpointParams, DebuggerResumeParams,
    DebuggerScriptParsedParams, DebuggerSetBreakpointParams, DebuggerStepIntoParams,
    DebuggerStepOutParams, DebuggerStepOverParams, IoCloseParams, IoReadParams,
    NetworkLoadNetworkResourceOptions, NetworkLoadNetworkResourceParams, PageGetFrameTreeParams,
    RuntimeConsoleApicalledParams, RuntimeEnableParams, RuntimeRunIfWaitingForDebuggerParams,
};
use crate::debugger_engine::{Effect, Input, RawFrame, SessionKey, StepKind};
use crate::session_transport::CdpSessionMux;
use crate::source_view::Position;
use crate::websocket_transport::{CdpWebSocketError, CdpWebSocketTransport};

pub struct CdpConnection {
    transport: Arc<CdpWebSocketTransport>,
    mux: CdpSessionMux,
    root: CdpClient<Channel>,
    close_reason: Arc<Mutex<Option<String>>>,
}

impl CdpConnection {
    pub async fn connect(endpoint: &str) -> Result<Self, CdpRuntimeError> {
        let transport = Arc::new(CdpWebSocketTransport::connect(endpoint).await?);
        let close_reason = transport.close_reason();
        let mux = CdpSessionMux::new(transport.clone());
        let root_channel = Channel::new(
            Box::new(mux.open_root().map_err(CdpRuntimeError::OpenSession)?),
            Box::new(RejectingHandler),
        );
        let root = CdpClient::root(root_channel.clone());
        let mux_loop = mux.clone();
        tokio::spawn(async move { mux_loop.run().await });
        tokio::spawn(async move { root_channel.run().await });
        Ok(Self {
            transport,
            mux,
            root,
            close_reason,
        })
    }

    pub fn root(&self) -> &CdpClient<Channel> {
        &self.root
    }

    pub fn open_session(&self, session: SessionKey) -> Result<CdpDebuggerSession, CdpRuntimeError> {
        let (event_sender, event_receiver) = mpsc::unbounded_channel();
        let channel = Channel::new(
            Box::new(
                self.mux
                    .open_session(session.session_id.clone())
                    .map_err(CdpRuntimeError::OpenSession)?,
            ),
            Box::new(CdpEventHandler {
                session: session.clone(),
                sender: event_sender,
            }),
        );
        let client = CdpClient::root(channel.clone());
        tokio::spawn(async move { channel.run().await });
        Ok(CdpDebuggerSession {
            session,
            client,
            events: event_receiver,
            source_map_frame_id: Mutex::new(None),
        })
    }

    pub fn retire_session(&self, session_id: &str) {
        self.mux.retire_session(session_id);
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

pub struct CdpDebuggerSession {
    session: SessionKey,
    client: CdpClient<Channel>,
    events: mpsc::UnboundedReceiver<Result<CdpRuntimeEvent, CdpRuntimeEventError>>,
    source_map_frame_id: Mutex<Option<String>>,
}

impl CdpDebuggerSession {
    pub fn key(&self) -> &SessionKey {
        &self.session
    }

    pub fn client(&self) -> &CdpClient<Channel> {
        &self.client
    }

    pub async fn next_event(&mut self) -> Option<Result<CdpRuntimeEvent, CdpRuntimeEventError>> {
        self.events.recv().await
    }

    pub async fn execute(&self, effect: &Effect) -> Result<Option<Input>, CdpRuntimeError> {
        match effect {
            Effect::ConfigureSession { effect_id, session } if session == &self.session => {
                self.client
                    .runtime_enable(RuntimeEnableParams::new())
                    .await
                    .map_err(CdpRuntimeError::protocol)?;
                self.client
                    .debugger_enable(DebuggerEnableParams::new())
                    .await
                    .map_err(CdpRuntimeError::protocol)?;
                Ok(Some(Input::SessionConfigured {
                    effect_id: *effect_id,
                }))
            }
            Effect::RunIfWaitingForDebugger { effect_id, session } if session == &self.session => {
                self.client
                    .runtime_run_if_waiting_for_debugger(
                        RuntimeRunIfWaitingForDebuggerParams::new(),
                    )
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
                ..
            } if script.session == self.session => {
                let source = self
                    .client
                    .debugger_get_script_source(DebuggerGetScriptSourceParams::new(
                        script.script_id.clone(),
                    ))
                    .await
                    .map_err(CdpRuntimeError::protocol)?;
                let (source_map, source_map_error) = match source_map_url {
                    Some(source_map_url) => {
                        match self
                            .load_source_map(generated_url, script_hash, source_map_url)
                            .await
                        {
                            Ok(source_map) => (Some(Arc::from(source_map)), None),
                            Err(error) if error.is_source_map_unavailable() => {
                                (None, Some(error.to_string()))
                            }
                            Err(error) => return Err(error),
                        }
                    }
                    None => (None, None),
                };
                Ok(Some(Input::ScriptSourceFetched {
                    effect_id: *effect_id,
                    content: Arc::from(source.script_source),
                    source_map,
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
                let mut params = DebuggerSetBreakpointParams::new(location);
                params.condition = physical.condition.clone();
                let installed = self
                    .client
                    .debugger_set_breakpoint(params)
                    .await
                    .map_err(CdpRuntimeError::protocol)?;
                Ok(Some(Input::BreakpointInstalled {
                    effect_id: *effect_id,
                    backend_id: installed.breakpoint_id,
                }))
            }
            Effect::RemoveBreakpoint {
                effect_id,
                physical,
                backend_id,
            } if physical.script.session == self.session => {
                self.client
                    .debugger_remove_breakpoint(DebuggerRemoveBreakpointParams::new(
                        backend_id.clone(),
                    ))
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
                    .debugger_resume(DebuggerResumeParams::new())
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
                            .debugger_step_into(DebuggerStepIntoParams::new())
                            .await
                            .map_err(CdpRuntimeError::protocol)?;
                    }
                    StepKind::Over => {
                        self.client
                            .debugger_step_over(DebuggerStepOverParams::new())
                            .await
                            .map_err(CdpRuntimeError::protocol)?;
                    }
                    StepKind::Out => {
                        self.client
                            .debugger_step_out(DebuggerStepOutParams::new())
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
    ) -> Result<Vec<u8>, CdpRuntimeError> {
        if source_map_url.starts_with("data:") {
            return decode_source_map_data_url(source_map_url);
        }

        let resolved_url = resolve_source_map_url(generated_url, source_map_url)?;
        let cache_path = source_map_cache_path(script_hash, &resolved_url);
        if let Some(path) = &cache_path {
            match tokio::fs::read(path).await {
                Ok(bytes) => {
                    if let Some(source_map) = decode_source_map_cache(&bytes) {
                        return Ok(source_map);
                    }
                    eprintln!("ignoring invalid source-map cache entry {}", path.display());
                    if let Err(error) = tokio::fs::remove_file(path).await {
                        eprintln!(
                            "failed to remove invalid source-map cache entry {}: {error}",
                            path.display()
                        );
                    }
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
        let cached_frame_id = { self.source_map_frame_id.lock().await.clone() };
        let frame_id = match cached_frame_id {
            Some(frame_id) => frame_id,
            None => {
                let frame_tree = self
                    .client
                    .page_get_frame_tree(PageGetFrameTreeParams::new())
                    .await
                    .map_err(CdpRuntimeError::protocol)?;
                let frame_id = frame_tree.frame_tree.frame.id;
                *self.source_map_frame_id.lock().await = Some(frame_id.clone());
                frame_id
            }
        };
        let mut params = NetworkLoadNetworkResourceParams::new(
            resolved_url.clone(),
            NetworkLoadNetworkResourceOptions::new(false, true),
        );
        params.frame_id = Some(frame_id);
        let loaded = self
            .client
            .network_load_network_resource(params)
            .await
            .map_err(CdpRuntimeError::protocol)?
            .resource;
        if !loaded.success {
            return Err(CdpRuntimeError::SourceMapLoadFailed {
                url: resolved_url,
                http_status_code: loaded.http_status_code,
                net_error_name: loaded.net_error_name,
            });
        }
        let stream = loaded
            .stream
            .ok_or_else(|| CdpRuntimeError::MissingSourceMapStream(resolved_url.clone()))?;
        let read_result = async {
            let mut bytes = Vec::new();
            loop {
                let mut params = IoReadParams::new(stream.clone());
                params.size = Some(8 * 1024 * 1024);
                let chunk = self
                    .client
                    .io_read(params)
                    .await
                    .map_err(CdpRuntimeError::protocol)?;
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
        let close_result = self
            .client
            .io_close(IoCloseParams::new(stream))
            .await
            .map_err(CdpRuntimeError::protocol);
        match (read_result, close_result) {
            (Ok(bytes), Ok(_)) => {
                if source_map_is_supported(&bytes) {
                    if let Some(path) = cache_path
                        && let Err(error) = write_source_map_cache(&path, &bytes).await
                    {
                        eprintln!(
                            "failed to write source-map cache entry {}: {error}",
                            path.display()
                        );
                    }
                } else {
                    eprintln!("not caching invalid or unsupported source map {resolved_url}");
                }
                Ok(bytes)
            }
            (Err(read), Ok(_)) => Err(read),
            (Ok(_), Err(close)) => Err(close),
            (Err(read), Err(close)) => Err(CdpRuntimeError::SourceMapReadAndClose {
                read: Box::new(read),
                close: Box::new(close),
            }),
        }
    }
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
            Self::ScriptParsed { session, params } => Ok(Some(Input::ScriptParsed {
                session,
                script_id: params.script_id,
                url: params.url,
                hash: params.hash,
                source_map_url: params.source_map_url.filter(|url| !url.is_empty()),
            })),
            Self::Paused { session, params } => {
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
                        })
                    })
                    .collect::<Result<_, CdpRuntimeEventError>>()?;
                Ok(Some(Input::Paused {
                    session,
                    reason: serde_json::to_value(params.reason)
                        .map_err(CdpRuntimeEventError::Serialize)?
                        .as_str()
                        .unwrap_or("other")
                        .to_owned(),
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

struct CdpEventHandler {
    session: SessionKey,
    sender: mpsc::UnboundedSender<Result<CdpRuntimeEvent, CdpRuntimeEventError>>,
}

#[async_trait]
impl RequestHandler for CdpEventHandler {
    async fn handle_request(&self, method: String, _params: Value) -> Result<Value, JsonRpcError> {
        Err(JsonRpcError::new(
            -32601,
            format!("unexpected browser request: {method}"),
        ))
    }

    async fn handle_notification(&self, method: String, params: Value) {
        let event = match method.as_str() {
            "Debugger.scriptParsed" => {
                deserialize(&method, params).map(|params| CdpRuntimeEvent::ScriptParsed {
                    session: self.session.clone(),
                    params,
                })
            }
            "Debugger.paused" => {
                deserialize(&method, params).map(|params| CdpRuntimeEvent::Paused {
                    session: self.session.clone(),
                    params,
                })
            }
            "Debugger.resumed" => Ok(CdpRuntimeEvent::Resumed {
                session: self.session.clone(),
            }),
            "Runtime.consoleAPICalled" => {
                deserialize(&method, params).map(|params| CdpRuntimeEvent::Console {
                    session: self.session.clone(),
                    params,
                })
            }
            _ => Ok(CdpRuntimeEvent::Other {
                session: self.session.clone(),
                method,
                params,
            }),
        };
        let _ = self.sender.send(event);
    }
}

fn deserialize<T: DeserializeOwned>(
    method: &str,
    params: Value,
) -> Result<T, CdpRuntimeEventError> {
    serde_json::from_value(params).map_err(|source| CdpRuntimeEventError::Deserialize {
        method: method.to_owned(),
        source,
    })
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
    let directory = if let Some(path) = env::var_os("JSDBG_SOURCE_MAP_CACHE") {
        PathBuf::from(path)
    } else if let Some(state_file) = env::var_os("JSDBG_SERVICE_STATE") {
        PathBuf::from(state_file)
            .parent()
            .map(|parent| parent.join("source-map-cache"))?
    } else if let Some(path) = env::var_os("LOCALAPPDATA") {
        PathBuf::from(path)
            .join("hediet")
            .join("cdp-client")
            .join("source-map-cache")
    } else if let Some(path) = env::var_os("XDG_CACHE_HOME") {
        PathBuf::from(path)
            .join("hediet")
            .join("cdp-client")
            .join("source-map-cache")
    } else if let Some(path) = env::var_os("HOME") {
        PathBuf::from(path)
            .join(".cache")
            .join("hediet")
            .join("cdp-client")
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

async fn write_source_map_cache(path: &Path, bytes: &[u8]) -> Result<(), std::io::Error> {
    const MAGIC: &[u8] = b"jsdbg-source-map-v1\n";
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
    let digest = format!("{:x}\n", Sha256::digest(bytes));
    let mut file = tokio::fs::File::create(&temporary).await?;
    file.write_all(MAGIC).await?;
    file.write_all(digest.as_bytes()).await?;
    file.write_all(bytes).await?;
    file.sync_data().await?;
    drop(file);
    match tokio::fs::rename(&temporary, path).await {
        Ok(()) => Ok(()),
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

fn decode_source_map_cache(bytes: &[u8]) -> Option<Vec<u8>> {
    const MAGIC: &[u8] = b"jsdbg-source-map-v1\n";
    let remainder = bytes.strip_prefix(MAGIC)?;
    let newline = remainder.iter().position(|byte| *byte == b'\n')?;
    let expected = std::str::from_utf8(&remainder[..newline]).ok()?;
    let source_map = &remainder[newline + 1..];
    (format!("{:x}", Sha256::digest(source_map)) == expected && source_map_is_supported(source_map))
        .then(|| source_map.to_vec())
}

fn source_map_is_supported(bytes: &[u8]) -> bool {
    match sourcemap::decode_slice(bytes) {
        Ok(sourcemap::DecodedMap::Regular(_)) => true,
        Ok(sourcemap::DecodedMap::Index(index)) => index.flatten().is_ok(),
        Ok(sourcemap::DecodedMap::Hermes(_)) | Err(_) => false,
    }
}

#[derive(Debug, thiserror::Error)]
pub enum CdpRuntimeError {
    #[error(transparent)]
    WebSocket(#[from] CdpWebSocketError),
    #[error("failed to open CDP session: {0}")]
    OpenSession(MuxError),
    #[error("CDP protocol error {code}: {message}")]
    Protocol {
        code: i64,
        message: String,
        data: Option<Value>,
    },
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
                | Self::MissingSourceMapStream(_)
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
mod tests {
    use super::*;

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
        let mut cached = b"jsdbg-source-map-v1\n".to_vec();
        cached.extend(format!("{:x}\n", Sha256::digest(source_map)).as_bytes());
        cached.extend(source_map);
        assert_eq!(
            decode_source_map_cache(&cached).as_deref(),
            Some(source_map.as_slice())
        );
        *cached.last_mut().unwrap() ^= 1;
        assert!(decode_source_map_cache(&cached).is_none());

        let invalid = b"<html>temporary CDN error</html>";
        let mut cached = b"jsdbg-source-map-v1\n".to_vec();
        cached.extend(format!("{:x}\n", Sha256::digest(invalid)).as_bytes());
        cached.extend(invalid);
        assert!(decode_source_map_cache(&cached).is_none());
    }
}

#[derive(Debug, thiserror::Error)]
pub enum CdpRuntimeEventError {
    #[error("failed to deserialize {method}: {source}")]
    Deserialize {
        method: String,
        source: serde_json::Error,
    },
    #[error("failed to serialize CDP event field: {0}")]
    Serialize(serde_json::Error),
    #[error("CDP event field {field} has invalid position {value}")]
    InvalidPosition { field: &'static str, value: i64 },
    #[error("Debugger.resumed requires the reducer's current pause epoch")]
    MissingPauseEpoch,
}
