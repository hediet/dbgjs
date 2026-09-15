use std::collections::hash_map::Entry;
use std::collections::{HashMap, HashSet};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use futures_util::{SinkExt, StreamExt};
use serde_json::{Map, Value, json};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::{oneshot, watch};
use tokio::time::Instant;
use tokio_tungstenite::tungstenite::handshake::server::{ErrorResponse, Request, Response};
use tokio_tungstenite::tungstenite::http::StatusCode;
use tokio_tungstenite::tungstenite::protocol::WebSocketConfig;
use tokio_tungstenite::tungstenite::{Error as WebSocketError, Message};
use tokio_tungstenite::{accept_hdr_async_with_config, connect_async_with_config};

const ACCEPT_TIMEOUT: Duration = Duration::from_secs(15);
const HANDSHAKE_TIMEOUT: Duration = Duration::from_secs(1);
const UPSTREAM_CONNECT_TIMEOUT: Duration = Duration::from_secs(10);
const SESSION_TIMEOUT: Duration = Duration::from_secs(45);
const MAX_MESSAGE_SIZE: usize = 16 * 1024 * 1024;

#[derive(Clone, Debug)]
pub enum PlaywrightCdpSource {
    BrowserRoot { endpoint: String },
}

#[derive(Clone, Debug)]
pub struct PlaywrightPageScope {
    pub target_id: String,
    pub browser_context_id: Option<String>,
}

impl PlaywrightPageScope {
    fn client_browser_context_id(&self) -> &str {
        self.browser_context_id
            .as_deref()
            .filter(|id| !id.is_empty())
            .unwrap_or("dbgjs-playwright-default")
    }
}

pub struct PlaywrightProxy {
    pub websocket_url: String,
    pub cancel: watch::Sender<bool>,
    pub completion: oneshot::Receiver<()>,
}

pub async fn start(
    source: PlaywrightCdpSource,
    page: PlaywrightPageScope,
    token: String,
) -> Result<PlaywrightProxy, PlaywrightProxyError> {
    let listener = TcpListener::bind(("127.0.0.1", 0)).await?;
    let address = listener.local_addr()?;
    let path = format!("/session/{token}");
    let websocket_url = format!("ws://127.0.0.1:{}{path}", address.port());
    let (cancel, cancel_receiver) = watch::channel(false);
    let (completion_sender, completion) = oneshot::channel();
    let capability_deadline = Instant::now() + SESSION_TIMEOUT;
    tokio::spawn(async move {
        if let Err(error) = run(
            listener,
            source,
            page,
            path,
            cancel_receiver,
            capability_deadline,
        )
        .await
        {
            eprintln!("Playwright CDP proxy failed: {error}");
        }
        let _ = completion_sender.send(());
    });
    Ok(PlaywrightProxy {
        websocket_url,
        cancel,
        completion,
    })
}

async fn run(
    listener: TcpListener,
    source: PlaywrightCdpSource,
    page: PlaywrightPageScope,
    path: String,
    mut cancel: watch::Receiver<bool>,
    capability_deadline: Instant,
) -> Result<(), PlaywrightProxyError> {
    let Some(client) = accept_authenticated(&listener, &path, &mut cancel).await? else {
        return Ok(());
    };
    drop(listener);

    let config = websocket_config();
    let PlaywrightCdpSource::BrowserRoot { endpoint } = source;
    let Some(upstream) = connect_upstream(
        endpoint,
        config.clone(),
        &mut cancel,
        capability_deadline,
        UPSTREAM_CONNECT_TIMEOUT,
    )
    .await?
    else {
        return Ok(());
    };
    bridge(client, upstream, page, cancel, capability_deadline).await
}

async fn connect_upstream(
    endpoint: String,
    config: WebSocketConfig,
    cancel: &mut watch::Receiver<bool>,
    capability_deadline: Instant,
    connection_timeout: Duration,
) -> Result<
    Option<tokio_tungstenite::WebSocketStream<tokio_tungstenite::MaybeTlsStream<TcpStream>>>,
    PlaywrightProxyError,
> {
    if *cancel.borrow() {
        return Ok(None);
    }
    let connection_deadline = Instant::now() + connection_timeout;
    tokio::select! {
        result = connect_async_with_config(endpoint, Some(config), false) => {
            let (upstream, _) = result?;
            Ok(Some(upstream))
        }
        _ = tokio::time::sleep_until(connection_deadline) => {
            Err(PlaywrightProxyError::UpstreamConnectTimeout)
        }
        _ = tokio::time::sleep_until(capability_deadline) => {
            Err(PlaywrightProxyError::CapabilityTimeout)
        }
        _ = cancel.changed() => Ok(None),
    }
}

async fn accept_authenticated(
    listener: &TcpListener,
    path: &str,
    cancel: &mut watch::Receiver<bool>,
) -> Result<Option<tokio_tungstenite::WebSocketStream<TcpStream>>, PlaywrightProxyError> {
    let deadline = Instant::now() + ACCEPT_TIMEOUT;
    loop {
        if *cancel.borrow() {
            return Ok(None);
        }
        let accepted = tokio::select! {
            result = tokio::time::timeout_at(deadline, listener.accept()) => {
                result.map_err(|_| PlaywrightProxyError::AcceptTimeout)??
            }
            _ = cancel.changed() => return Ok(None),
        };
        let (stream, peer) = accepted;
        if !peer.ip().is_loopback() {
            continue;
        }

        let authenticated = Arc::new(AtomicBool::new(false));
        let observed_authentication = authenticated.clone();
        let expected_path = path.to_owned();
        let handshake = accept_hdr_async_with_config(
            stream,
            move |request: &Request, response: Response| {
                if request.uri().path() != expected_path {
                    return Err(unauthorized());
                }
                observed_authentication.store(true, Ordering::Relaxed);
                Ok(response)
            },
            Some(websocket_config()),
        );
        let result = tokio::select! {
            result = tokio::time::timeout(HANDSHAKE_TIMEOUT, handshake) => result,
            _ = cancel.changed() => return Ok(None),
        };
        match result {
            Ok(Ok(client)) if authenticated.load(Ordering::Relaxed) => return Ok(Some(client)),
            Ok(Ok(_)) => continue,
            Ok(Err(_)) | Err(_) => continue,
        }
    }
}

fn websocket_config() -> WebSocketConfig {
    let mut config = WebSocketConfig::default();
    config.max_message_size = Some(MAX_MESSAGE_SIZE);
    config.max_frame_size = Some(MAX_MESSAGE_SIZE);
    config
}

fn unauthorized() -> ErrorResponse {
    tokio_tungstenite::tungstenite::http::Response::builder()
        .status(StatusCode::UNAUTHORIZED)
        .body(Some("invalid Playwright proxy capability".to_owned()))
        .expect("static authentication response is valid")
}

async fn bridge(
    client: tokio_tungstenite::WebSocketStream<TcpStream>,
    upstream: tokio_tungstenite::WebSocketStream<tokio_tungstenite::MaybeTlsStream<TcpStream>>,
    page: PlaywrightPageScope,
    mut cancel: watch::Receiver<bool>,
    capability_deadline: Instant,
) -> Result<(), PlaywrightProxyError> {
    let (mut client_sender, mut client_receiver) = client.split();
    let (mut upstream_sender, mut upstream_receiver) = upstream.split();
    let mut scope = ActiveScope::new(page);
    let mut pending = HashMap::<String, PendingRequest>::new();
    let mut internal = HashMap::<String, InternalRequest>::new();
    let mut next_internal_id = -1_i64;
    let deadline = tokio::time::sleep_until(capability_deadline);
    tokio::pin!(deadline);

    loop {
        tokio::select! {
            _ = &mut deadline => return Err(PlaywrightProxyError::CapabilityTimeout),
            _ = cancel.changed() => return Ok(()),
            message = client_receiver.next() => {
                let Some(message) = message else { return Ok(()); };
                let message = message?;
                if let Message::Close(frame) = message {
                    let close = Message::Close(frame);
                    upstream_sender.send(close.clone()).await?;
                    client_sender.send(close).await?;
                    return Ok(());
                }
                let summary = cdp_message_summary(&message);
                let action = client_request(message, &scope, &mut pending, &internal)
                    .inspect_err(|error| eprintln!(
                        "Playwright CDP proxy rejected client message ({summary}): {error}"
                    ))?;
                match action {
                    ClientAction::Forward(message) => upstream_sender.send(message).await?,
                    ClientAction::Reply(message) => client_sender.send(message).await?,
                    ClientAction::AttachSelected(request_id) => {
                        let internal_id = allocate_internal_request_id(
                            &mut next_internal_id,
                            &pending,
                            &internal,
                        )?;
                        reserve_internal_request(
                            &mut internal,
                            id_key(&internal_id)?,
                            InternalRequest::AutoAttach { request_id },
                        )?;
                        upstream_sender.send(json_message(json!({
                            "id": internal_id,
                            "method": "Target.attachToTarget",
                            "params": {
                                "targetId": scope.page.target_id,
                                "flatten": true,
                                "__dbgjsAutoAttach": true
                            }
                        }))?).await?;
                    }
                    ClientAction::Drop => {}
                }
            }
            message = upstream_receiver.next() => {
                let Some(message) = message else { return Ok(()); };
                let message = message?;
                if let Message::Close(frame) = message {
                    client_sender.send(Message::Close(frame)).await?;
                    return Ok(());
                }
                let summary = cdp_message_summary(&message);
                let action = upstream_message(
                    message,
                    &mut scope,
                    &mut pending,
                    &mut internal,
                )
                .inspect_err(|error| eprintln!(
                    "Playwright CDP proxy rejected upstream message ({summary}): {error}"
                ))?;
                match action {
                    UpstreamAction::Forward(message) => client_sender.send(message).await?,
                    UpstreamAction::ForwardAndClose(message) => {
                        client_sender.send(message).await?;
                        return Err(PlaywrightProxyError::SelectedTargetDestroyed);
                    }
                    UpstreamAction::Reply(message) => client_sender.send(message).await?,
                    UpstreamAction::Drop => {}
                    UpstreamAction::Detach(session_id) => {
                        let internal_id = allocate_internal_request_id(
                            &mut next_internal_id,
                            &pending,
                            &internal,
                        )?;
                        reserve_internal_request(
                            &mut internal,
                            id_key(&internal_id)?,
                            InternalRequest::Detach,
                        )?;
                        upstream_sender.send(json_message(json!({
                            "id": internal_id,
                            "method": "Target.detachFromTarget",
                            "params": { "sessionId": session_id }
                        }))?).await?;
                    }
                }

            }
        }
    }
}

fn cdp_message_summary(message: &Message) -> String {
    let bytes = match message {
        Message::Text(text) => text.as_bytes(),
        Message::Binary(bytes) => bytes.as_ref(),
        Message::Close(_) => return "close".to_owned(),
        Message::Ping(_) => return "ping".to_owned(),
        Message::Pong(_) => return "pong".to_owned(),
        Message::Frame(_) => return "frame".to_owned(),
    };
    let Ok(value) = serde_json::from_slice::<Value>(bytes) else {
        return "invalid JSON".to_owned();
    };
    let field = |name: &str| {
        value
            .get(name)
            .map(Value::to_string)
            .unwrap_or_else(|| "-".to_owned())
    };
    format!(
        "id={}, method={}, sessionId={}, hasResult={}, hasError={}",
        field("id"),
        field("method"),
        field("sessionId"),
        value.get("result").is_some(),
        value.get("error").is_some()
    )
}

fn allocate_internal_request_id(
    next_internal_id: &mut i64,
    pending: &HashMap<String, PendingRequest>,
    internal: &HashMap<String, InternalRequest>,
) -> Result<Value, PlaywrightProxyError> {
    loop {
        let id = Value::from(*next_internal_id);
        *next_internal_id = next_internal_id
            .checked_sub(1)
            .ok_or(PlaywrightProxyError::InternalRequestIdExhausted)?;
        let key = id_key(&id)?;
        if !pending.contains_key(&key) && !internal.contains_key(&key) {
            return Ok(id);
        }
    }
}

fn reserve_internal_request(
    internal: &mut HashMap<String, InternalRequest>,
    key: String,
    request: InternalRequest,
) -> Result<(), PlaywrightProxyError> {
    match internal.entry(key) {
        Entry::Vacant(entry) => {
            entry.insert(request);
            Ok(())
        }
        Entry::Occupied(entry) => Err(PlaywrightProxyError::DuplicateRequestId(
            entry.key().clone(),
        )),
    }
}

struct ActiveScope {
    page: PlaywrightPageScope,
    sessions: HashSet<String>,
    browser_broker_sessions: HashSet<String>,
    session_target_ids: HashMap<String, String>,
    descendant_target_ids: HashSet<String>,
    primary_session_id: Option<String>,
}

impl ActiveScope {
    fn new(page: PlaywrightPageScope) -> Self {
        Self {
            page,
            sessions: HashSet::new(),
            browser_broker_sessions: HashSet::new(),
            session_target_ids: HashMap::new(),
            descendant_target_ids: HashSet::new(),
            primary_session_id: None,
        }
    }

    fn register_page_session(&mut self, session_id: &str, target_id: &str) {
        self.sessions.insert(session_id.to_owned());
        self.session_target_ids
            .insert(session_id.to_owned(), target_id.to_owned());
    }

    fn remove_session(&mut self, session_id: &str) -> bool {
        self.browser_broker_sessions.remove(session_id);
        self.session_target_ids.remove(session_id);
        self.sessions.remove(session_id)
    }

    fn remove_descendant_target(&mut self, target_id: &str) -> bool {
        if !self.descendant_target_ids.remove(target_id) {
            return false;
        }
        let sessions = self
            .session_target_ids
            .iter()
            .filter(|(_, candidate)| candidate.as_str() == target_id)
            .map(|(session_id, _)| session_id.clone())
            .collect::<Vec<_>>();
        for session_id in sessions {
            self.remove_session(&session_id);
        }
        true
    }

    fn target_for_session(&self, session_id: Option<&str>) -> Option<&str> {
        match session_id {
            Some(session_id) if self.browser_broker_sessions.contains(session_id) => {
                Some(&self.page.target_id)
            }
            Some(session_id) => self.session_target_ids.get(session_id).map(String::as_str),
            None => Some(&self.page.target_id),
        }
    }
}

#[derive(Clone)]
enum PendingRequest {
    Forwarded {
        method: String,
        session_id: Option<String>,
    },
    InternalAttach,
}

enum InternalRequest {
    AutoAttach { request_id: Value },
    Detach,
}

fn client_request(
    message: Message,
    scope: &ActiveScope,
    pending: &mut HashMap<String, PendingRequest>,
    internal: &HashMap<String, InternalRequest>,
) -> Result<ClientAction, PlaywrightProxyError> {
    let Some(mut value) = parse_data_message(message)? else {
        return Ok(ClientAction::Drop);
    };
    let Some(object) = value.as_object_mut() else {
        return Err(PlaywrightProxyError::InvalidCdpMessage);
    };
    let method = object
        .get("method")
        .and_then(Value::as_str)
        .ok_or(PlaywrightProxyError::InvalidCdpMessage)?
        .to_owned();
    let id = object.get("id").cloned();
    let response_session = object.get("sessionId").cloned();
    if let Some(id) = &id {
        let key = id_key(id)?;
        if pending.contains_key(&key) || internal.contains_key(&key) {
            return Err(PlaywrightProxyError::DuplicateRequestId(key));
        }
    }
    if let Err(error) = validate_request_identifiers(object, &method, scope) {
        return scope_error(id, response_session, error);
    }
    let session = object.get("sessionId").and_then(Value::as_str);

    let policy = match session {
        Some(session_id) if scope.browser_broker_sessions.contains(session_id) => {
            browser_broker_method_policy(&method)
        }
        Some(_) => session_method_policy(&method),
        None => root_method_policy(&method),
    };
    match policy {
        MethodPolicy::Deny => scope_error(
            id,
            response_session,
            ScopeViolation::MethodNotAllowed {
                method: method.clone(),
            },
        ),
        MethodPolicy::SyntheticSuccess => response_result(id, response_session, json!({})),
        MethodPolicy::AttachSelected => match id {
            Some(id) => {
                reserve_pending_request(pending, id_key(&id)?, PendingRequest::InternalAttach)?;
                Ok(ClientAction::AttachSelected(id))
            }
            None => Ok(ClientAction::Drop),
        },
        MethodPolicy::RewriteSelectedTarget => {
            let params = object
                .entry("params")
                .or_insert_with(|| Value::Object(Map::new()));
            let Some(params) = params.as_object_mut() else {
                return Err(PlaywrightProxyError::InvalidCdpMessage);
            };
            params.insert(
                "targetId".to_owned(),
                Value::String(scope.page.target_id.clone()),
            );
            forward_request(value, method, pending)
        }
        MethodPolicy::RequireSelectedTarget => {
            let requested = object
                .get("params")
                .and_then(|params| params.get("targetId"))
                .and_then(Value::as_str);
            if requested != Some(&scope.page.target_id) {
                return scope_error(id, response_session, ScopeViolation::MissingSelectedTarget);
            }
            forward_request(value, method, pending)
        }
        MethodPolicy::Forward => forward_request(value, method, pending),
    }
}

fn forward_request(
    value: Value,
    method: String,
    pending: &mut HashMap<String, PendingRequest>,
) -> Result<ClientAction, PlaywrightProxyError> {
    if let Some(id) = value.get("id") {
        reserve_pending_request(
            pending,
            id_key(id)?,
            PendingRequest::Forwarded {
                method,
                session_id: value
                    .get("sessionId")
                    .and_then(Value::as_str)
                    .map(str::to_owned),
            },
        )?;
    }
    Ok(ClientAction::Forward(json_message(value)?))
}

fn reserve_pending_request(
    pending: &mut HashMap<String, PendingRequest>,
    key: String,
    request: PendingRequest,
) -> Result<(), PlaywrightProxyError> {
    match pending.entry(key) {
        Entry::Vacant(entry) => {
            entry.insert(request);
            Ok(())
        }
        Entry::Occupied(entry) => Err(PlaywrightProxyError::DuplicateRequestId(
            entry.key().clone(),
        )),
    }
}

fn root_method_policy(method: &str) -> MethodPolicy {
    match method {
        "Browser.getVersion" | "Target.getTargets" | "Target.attachToBrowserTarget" => {
            MethodPolicy::Forward
        }
        "Target.getTargetInfo" => MethodPolicy::RewriteSelectedTarget,
        "Target.attachToTarget" | "Target.activateTarget" | "Target.closeTarget" => {
            MethodPolicy::RequireSelectedTarget
        }
        "Target.detachFromTarget" => MethodPolicy::Forward,
        "Target.setAutoAttach" => MethodPolicy::AttachSelected,
        "Browser.setDownloadBehavior" | "Browser.setWindowBounds" | "Target.setDiscoverTargets" => {
            MethodPolicy::SyntheticSuccess
        }
        _ => MethodPolicy::Deny,
    }
}

fn browser_broker_method_policy(method: &str) -> MethodPolicy {
    match method {
        "Target.attachToTarget" => MethodPolicy::RequireSelectedTarget,
        "Target.detachFromTarget" => MethodPolicy::Forward,
        _ => MethodPolicy::Deny,
    }
}

fn session_method_policy(method: &str) -> MethodPolicy {
    match method {
        "Browser.getWindowForTarget" | "Target.getTargetInfo" => MethodPolicy::Forward,
        "Browser.setDownloadBehavior" | "Browser.setWindowBounds" | "Target.setDiscoverTargets" => {
            MethodPolicy::SyntheticSuccess
        }
        "Target.setAutoAttach" => MethodPolicy::Forward,
        _ if is_page_session_method(method) => MethodPolicy::Forward,
        _ => MethodPolicy::Deny,
    }
}

fn is_page_session_method(method: &str) -> bool {
    let Some((domain, name)) = method.split_once('.') else {
        return false;
    };
    match domain {
        "Accessibility" | "CSS" | "DOM" | "Emulation" | "Fetch" | "Input" | "Log"
        | "Performance" => true,
        "Network" => matches!(
            name,
            "disable"
                | "emulateNetworkConditions"
                | "emulateNetworkConditionsByRule"
                | "enable"
                | "getRequestPostData"
                | "getResponseBody"
                | "getResponseBodyForInterception"
                | "replayXHR"
                | "setAcceptedEncodings"
                | "clearAcceptedEncodings"
                | "setBlockedURLs"
                | "setBypassServiceWorker"
                | "setCacheDisabled"
                | "setExtraHTTPHeaders"
                | "setUserAgentOverride"
        ),
        "Page" => matches!(
            name,
            "addScriptToEvaluateOnNewDocument"
                | "bringToFront"
                | "captureScreenshot"
                | "close"
                | "createIsolatedWorld"
                | "disable"
                | "enable"
                | "getAppManifest"
                | "getFrameTree"
                | "getLayoutMetrics"
                | "getNavigationHistory"
                | "getResourceContent"
                | "handleJavaScriptDialog"
                | "navigate"
                | "navigateToHistoryEntry"
                | "printToPDF"
                | "reload"
                | "removeScriptToEvaluateOnNewDocument"
                | "setBypassCSP"
                | "setDocumentContent"
                | "setFontFamilies"
                | "setFontSizes"
                | "setInterceptFileChooserDialog"
                | "setLifecycleEventsEnabled"
                | "stopLoading"
        ),
        "Runtime" => matches!(
            name,
            "addBinding"
                | "awaitScript"
                | "callFunctionOn"
                | "compileScript"
                | "disable"
                | "discardConsoleEntries"
                | "enable"
                | "evaluate"
                | "getIsolateId"
                | "getProperties"
                | "globalLexicalScopeNames"
                | "queryObjects"
                | "releaseObject"
                | "releaseObjectGroup"
                | "removeBinding"
                | "runIfWaitingForDebugger"
                | "runScript"
        ),
        _ => false,
    }
}

enum MethodPolicy {
    Forward,
    SyntheticSuccess,
    AttachSelected,
    RewriteSelectedTarget,
    RequireSelectedTarget,
    Deny,
}

fn upstream_message(
    message: Message,
    scope: &mut ActiveScope,
    pending: &mut HashMap<String, PendingRequest>,
    internal: &mut HashMap<String, InternalRequest>,
) -> Result<UpstreamAction, PlaywrightProxyError> {
    let Some(mut value) = parse_data_message(message)? else {
        return Ok(UpstreamAction::Drop);
    };
    let Some(object) = value.as_object_mut() else {
        return Err(PlaywrightProxyError::InvalidCdpMessage);
    };

    if let Some(id) = object.get("id") {
        let key = id_key(id)?;
        if let Some(request) = internal.remove(&key) {
            return internal_response(object, request, scope, pending);
        }
        let Some(PendingRequest::Forwarded { method, session_id }) = pending.get(&key).cloned()
        else {
            return Ok(UpstreamAction::Drop);
        };
        pending.remove(&key);
        match method.as_str() {
            "Target.getTargets" => {
                let Some(infos) = object
                    .get_mut("result")
                    .and_then(|result| result.get_mut("targetInfos"))
                    .and_then(Value::as_array_mut)
                else {
                    return Err(PlaywrightProxyError::InvalidCdpMessage);
                };
                infos.retain(|info| target_info_id(info) == Some(&scope.page.target_id));
                for info in infos {
                    validate_target_info(info, &scope.page, &scope.page.target_id)?;
                    project_target_info(info, &scope.page)?;
                }
            }
            "Target.getTargetInfo" => {
                let info = object
                    .get_mut("result")
                    .and_then(|result| result.get_mut("targetInfo"))
                    .ok_or(PlaywrightProxyError::InvalidCdpMessage)?;
                let expected_target = scope.target_for_session(session_id.as_deref()).ok_or(
                    PlaywrightProxyError::ScopeViolation(ScopeViolation::SessionId),
                )?;
                validate_target_info(info, &scope.page, expected_target)?;
                project_target_info(info, &scope.page)?;
            }
            "Target.attachToTarget" => {
                let session_id = object
                    .get("result")
                    .and_then(|result| result.get("sessionId"))
                    .and_then(Value::as_str)
                    .ok_or(PlaywrightProxyError::InvalidCdpMessage)?;
                let target_id = scope.page.target_id.clone();
                scope.register_page_session(session_id, &target_id);
            }
            "Target.attachToBrowserTarget" => {
                let session_id = object
                    .get("result")
                    .and_then(|result| result.get("sessionId"))
                    .and_then(Value::as_str)
                    .ok_or(PlaywrightProxyError::InvalidCdpMessage)?;
                scope.sessions.insert(session_id.to_owned());
                scope.browser_broker_sessions.insert(session_id.to_owned());
            }
            _ => {}
        }
        return Ok(UpstreamAction::Forward(json_message(value)?));
    }

    let method = object
        .get("method")
        .and_then(Value::as_str)
        .ok_or(PlaywrightProxyError::InvalidCdpMessage)?;
    if method == "Target.attachedToTarget" {
        let parent_session_id = object.get("sessionId").and_then(Value::as_str);
        let params = object
            .get("params")
            .ok_or(PlaywrightProxyError::InvalidCdpMessage)?;
        let session_id = params
            .get("sessionId")
            .and_then(Value::as_str)
            .ok_or(PlaywrightProxyError::InvalidCdpMessage)?;
        let target_info = params
            .get("targetInfo")
            .ok_or(PlaywrightProxyError::InvalidCdpMessage)?;
        let target_id = target_info_id(target_info)
            .ok_or(PlaywrightProxyError::InvalidCdpMessage)?
            .to_owned();
        if target_id == scope.page.target_id {
            let allowed_parent = parent_session_id
                .is_none_or(|parent| scope.browser_broker_sessions.contains(parent));
            if !allowed_parent {
                return Ok(UpstreamAction::Detach(session_id.to_owned()));
            }
            validate_target_info(target_info, &scope.page, &target_id)?;
            scope.register_page_session(session_id, &target_id);
            let target_info = object
                .get_mut("params")
                .and_then(|params| params.get_mut("targetInfo"))
                .ok_or(PlaywrightProxyError::InvalidCdpMessage)?;
            project_target_info(target_info, &scope.page)?;
            return Ok(UpstreamAction::Forward(json_message(value)?));
        }
        if is_browser_broker_target_info(target_info) {
            if parent_session_id.is_some() {
                return Ok(UpstreamAction::Detach(session_id.to_owned()));
            }
            scope.sessions.insert(session_id.to_owned());
            scope.browser_broker_sessions.insert(session_id.to_owned());
            return Ok(UpstreamAction::Drop);
        }
        let verified_parent = parent_session_id
            .and_then(|parent| scope.session_target_ids.get(parent))
            .is_some();
        if verified_parent && validate_descendant_target_info(target_info, &scope.page).is_ok() {
            scope.descendant_target_ids.insert(target_id.clone());
            scope.register_page_session(session_id, &target_id);
            let target_info = object
                .get_mut("params")
                .and_then(|params| params.get_mut("targetInfo"))
                .ok_or(PlaywrightProxyError::InvalidCdpMessage)?;
            project_target_info(target_info, &scope.page)?;
            return Ok(UpstreamAction::Forward(json_message(value)?));
        }
        return Ok(UpstreamAction::Detach(session_id.to_owned()));
    }
    if method == "Target.detachedFromTarget" {
        let session_id = object
            .get("params")
            .and_then(|params| params.get("sessionId"))
            .and_then(Value::as_str)
            .ok_or(PlaywrightProxyError::InvalidCdpMessage)?;
        if !scope.remove_session(session_id) {
            return Ok(UpstreamAction::Drop);
        }
        return Ok(if scope.primary_session_id.as_deref() == Some(session_id) {
            scope.primary_session_id = None;
            UpstreamAction::ForwardAndClose(json_message(value)?)
        } else {
            UpstreamAction::Forward(json_message(value)?)
        });
    }
    if matches!(method, "Target.targetCreated" | "Target.targetInfoChanged") {
        let target_info = object
            .get("params")
            .and_then(|params| params.get("targetInfo"))
            .ok_or(PlaywrightProxyError::InvalidCdpMessage)?;
        let target_id =
            target_info_id(target_info).ok_or(PlaywrightProxyError::InvalidCdpMessage)?;
        if target_id == scope.page.target_id {
            validate_target_info(target_info, &scope.page, target_id)?;
        } else if object
            .get("sessionId")
            .and_then(Value::as_str)
            .is_some_and(|parent| scope.session_target_ids.contains_key(parent))
            && validate_descendant_target_info(target_info, &scope.page).is_ok()
        {
            scope.descendant_target_ids.insert(target_id.to_owned());
        } else {
            return Ok(UpstreamAction::Drop);
        }
        let target_info = object
            .get_mut("params")
            .and_then(|params| params.get_mut("targetInfo"))
            .ok_or(PlaywrightProxyError::InvalidCdpMessage)?;
        project_target_info(target_info, &scope.page)?;
        return Ok(UpstreamAction::Forward(json_message(value)?));
    }
    if method == "Target.targetDestroyed" {
        let target_id = object
            .get("params")
            .and_then(|params| params.get("targetId"))
            .and_then(Value::as_str)
            .ok_or(PlaywrightProxyError::InvalidCdpMessage)?;
        return Ok(if target_id == scope.page.target_id {
            UpstreamAction::ForwardAndClose(json_message(value)?)
        } else if scope.remove_descendant_target(target_id) {
            UpstreamAction::Forward(json_message(value)?)
        } else {
            UpstreamAction::Drop
        });
    }
    let Some(session_id) = object.get("sessionId").and_then(Value::as_str) else {
        return Ok(UpstreamAction::Drop);
    };
    if !scope.sessions.contains(session_id) {
        return Ok(UpstreamAction::Drop);
    }
    if scope.browser_broker_sessions.contains(session_id) {
        return Ok(UpstreamAction::Drop);
    }
    Ok(UpstreamAction::Forward(json_message(value)?))
}

fn internal_response(
    object: &Map<String, Value>,
    request: InternalRequest,
    scope: &mut ActiveScope,
    pending: &mut HashMap<String, PendingRequest>,
) -> Result<UpstreamAction, PlaywrightProxyError> {
    match request {
        InternalRequest::Detach => Ok(UpstreamAction::Drop),
        InternalRequest::AutoAttach { request_id } => {
            if !matches!(
                pending.remove(&id_key(&request_id)?),
                Some(PendingRequest::InternalAttach)
            ) {
                return Err(PlaywrightProxyError::InvalidCdpMessage);
            }
            if let Some(error) = object.get("error") {
                return Ok(UpstreamAction::Reply(json_message(json!({
                    "id": request_id,
                    "error": error
                }))?));
            }
            if let Some(session_id) = object
                .get("result")
                .and_then(|result| result.get("sessionId"))
                .and_then(Value::as_str)
            {
                let target_id = scope.page.target_id.clone();
                scope.register_page_session(session_id, &target_id);
                scope.primary_session_id = Some(session_id.to_owned());
            }
            Ok(UpstreamAction::Reply(json_message(json!({
                "id": request_id,
                "result": {}
            }))?))
        }
    }
}

fn project_target_info(
    target_info: &mut Value,
    page: &PlaywrightPageScope,
) -> Result<(), PlaywrightProxyError> {
    let selected = target_info_id(target_info) == Some(&page.target_id);
    let info = target_info
        .as_object_mut()
        .ok_or(PlaywrightProxyError::InvalidCdpMessage)?;
    if selected && info.get("type").and_then(Value::as_str) == Some("other") {
        info.insert("type".to_owned(), Value::String("page".to_owned()));
    }
    info.insert(
        "browserContextId".to_owned(),
        Value::String(page.client_browser_context_id().to_owned()),
    );
    Ok(())
}

fn validate_target_info(
    target_info: &Value,
    page: &PlaywrightPageScope,
    expected_target_id: &str,
) -> Result<(), PlaywrightProxyError> {
    if target_info_id(target_info) != Some(expected_target_id) {
        return Err(PlaywrightProxyError::ScopeViolation(
            ScopeViolation::TargetId,
        ));
    }
    if target_info
        .get("browserContextId")
        .and_then(Value::as_str)
        .is_some_and(|context| page.browser_context_id.as_deref() != Some(context))
    {
        return Err(PlaywrightProxyError::ScopeViolation(
            ScopeViolation::BrowserContextId,
        ));
    }
    Ok(())
}

fn is_browser_broker_target_info(target_info: &Value) -> bool {
    match target_info.get("type").and_then(Value::as_str) {
        Some("browser") => true,
        Some("other") => {
            target_info.get("title").and_then(Value::as_str) == Some("")
                && target_info.get("url").and_then(Value::as_str) == Some("")
                && target_info.get("browserContextId").is_none()
        }
        _ => false,
    }
}

fn validate_descendant_target_info(
    target_info: &Value,
    page: &PlaywrightPageScope,
) -> Result<(), PlaywrightProxyError> {
    if target_info.get("type").and_then(Value::as_str) != Some("iframe") {
        return Err(PlaywrightProxyError::ScopeViolation(
            ScopeViolation::TargetId,
        ));
    }
    let target_id = target_info_id(target_info).ok_or(PlaywrightProxyError::InvalidCdpMessage)?;
    if target_id == page.target_id {
        return Err(PlaywrightProxyError::ScopeViolation(
            ScopeViolation::TargetId,
        ));
    }
    validate_target_info(target_info, page, target_id)
}

fn validate_request_identifiers(
    request: &Map<String, Value>,
    method: &str,
    scope: &ActiveScope,
) -> Result<(), ScopeViolation> {
    let routing_session = match request.get("sessionId") {
        Some(Value::String(session_id)) if scope.sessions.contains(session_id) => {
            Some(session_id.as_str())
        }
        Some(_) => return Err(ScopeViolation::SessionId),
        None => None,
    };
    let params = match request.get("params") {
        Some(Value::Object(params)) => Some(params),
        Some(_) => return Err(ScopeViolation::InvalidParams),
        None => None,
    };

    match method {
        "Target.attachToTarget" | "Target.activateTarget" | "Target.closeTarget" => {
            validate_optional_identifier(
                params,
                "targetId",
                |target_id| target_id == scope.page.target_id,
                ScopeViolation::TargetId,
            )
        }
        "Target.getTargetInfo" | "Browser.getWindowForTarget" => {
            let expected_target = scope
                .target_for_session(routing_session)
                .ok_or(ScopeViolation::SessionId)?;
            validate_optional_identifier(
                params,
                "targetId",
                |target_id| target_id == expected_target,
                ScopeViolation::TargetId,
            )
        }
        "Target.detachFromTarget" => validate_optional_identifier(
            params,
            "sessionId",
            |session_id| scope.sessions.contains(session_id),
            ScopeViolation::SessionId,
        ),
        "Browser.setDownloadBehavior" => validate_optional_identifier(
            params,
            "browserContextId",
            |context_id| scope.page.client_browser_context_id() == context_id,
            ScopeViolation::BrowserContextId,
        ),
        _ => Ok(()),
    }
}

fn validate_optional_identifier(
    params: Option<&Map<String, Value>>,
    field: &str,
    predicate: impl FnOnce(&str) -> bool,
    violation: ScopeViolation,
) -> Result<(), ScopeViolation> {
    let Some(value) = params.and_then(|params| params.get(field)) else {
        return Ok(());
    };
    value
        .as_str()
        .is_some_and(predicate)
        .then_some(())
        .ok_or(violation)
}

fn target_info_id(value: &Value) -> Option<&str> {
    value.get("targetId").and_then(Value::as_str)
}

fn response_result(
    id: Option<Value>,
    session_id: Option<Value>,
    result: Value,
) -> Result<ClientAction, PlaywrightProxyError> {
    Ok(match id {
        Some(id) => {
            let mut response = json!({ "id": id, "result": result });
            if let Some(session_id) = session_id {
                response["sessionId"] = session_id;
            }
            ClientAction::Reply(json_message(response)?)
        }
        None => ClientAction::Drop,
    })
}

fn scope_error(
    id: Option<Value>,
    session_id: Option<Value>,
    violation: ScopeViolation,
) -> Result<ClientAction, PlaywrightProxyError> {
    Ok(match id {
        Some(id) => {
            let mut response = json!({
                "id": id,
                "error": { "code": -32000, "message": violation.to_string() }
            });
            if let Some(session_id) = session_id {
                response["sessionId"] = session_id;
            }
            ClientAction::Reply(json_message(response)?)
        }
        None => ClientAction::Drop,
    })
}

fn parse_data_message(message: Message) -> Result<Option<Value>, PlaywrightProxyError> {
    match message {
        Message::Text(text) => Ok(Some(serde_json::from_slice(text.as_bytes())?)),
        Message::Binary(bytes) => Ok(Some(serde_json::from_slice(&bytes)?)),
        Message::Close(_) => Ok(None),
        Message::Ping(_) | Message::Pong(_) | Message::Frame(_) => Ok(None),
    }
}

fn json_message(value: Value) -> Result<Message, PlaywrightProxyError> {
    Ok(Message::Text(serde_json::to_string(&value)?.into()))
}

fn id_key(id: &Value) -> Result<String, PlaywrightProxyError> {
    Ok(serde_json::to_string(id)?)
}

enum ClientAction {
    Forward(Message),
    Reply(Message),
    AttachSelected(Value),
    Drop,
}

enum UpstreamAction {
    Forward(Message),
    ForwardAndClose(Message),
    Reply(Message),
    Drop,
    Detach(String),
}

#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
pub enum ScopeViolation {
    #[error("CDP method '{method}' is outside the selected page allowlist")]
    MethodNotAllowed { method: String },
    #[error("CDP request must name the selected target")]
    MissingSelectedTarget,
    #[error("target identifier is outside the selected page")]
    TargetId,
    #[error("session identifier is outside the selected page")]
    SessionId,
    #[error("browser context identifier is outside the selected page")]
    BrowserContextId,
    #[error("CDP request parameters do not match the method schema")]
    InvalidParams,
}

#[derive(Debug, thiserror::Error)]
pub enum PlaywrightProxyError {
    #[error(transparent)]
    Io(#[from] std::io::Error),
    #[error(transparent)]
    WebSocket(#[from] WebSocketError),
    #[error(transparent)]
    Json(#[from] serde_json::Error),
    #[error("Playwright proxy was not claimed before its deadline")]
    AcceptTimeout,
    #[error("Playwright proxy capability exceeded its deadline")]
    CapabilityTimeout,
    #[error("Playwright proxy upstream connection exceeded its deadline")]
    UpstreamConnectTimeout,
    #[error("duplicate outstanding CDP request id {0}")]
    DuplicateRequestId(String),
    #[error("Playwright proxy exhausted its internal CDP request ids")]
    InternalRequestIdExhausted,
    #[error("selected page target was destroyed")]
    SelectedTargetDestroyed,
    #[error(transparent)]
    ScopeViolation(#[from] ScopeViolation),
    #[error("Playwright proxy received an invalid CDP message")]
    InvalidCdpMessage,
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::time::timeout;
    use tokio_tungstenite::{accept_async, connect_async};

    fn page() -> PlaywrightPageScope {
        PlaywrightPageScope {
            target_id: "selected".to_owned(),
            browser_context_id: Some("selected-context".to_owned()),
        }
    }

    fn scope() -> ActiveScope {
        let mut scope = ActiveScope::new(page());
        scope.register_page_session("selected-session", "selected");
        scope.primary_session_id = Some("selected-session".to_owned());
        scope
    }

    #[test]
    fn target_info_may_omit_its_known_browser_context() {
        validate_target_info(
            &json!({
                "targetId": "selected",
                "type": "page",
                "title": "selected",
                "url": "http://selected.test/"
            }),
            &page(),
            "selected",
        )
        .unwrap();
        assert!(matches!(
            validate_target_info(
                &json!({
                    "targetId": "selected",
                    "browserContextId": "other-context"
                }),
                &page(),
                "selected",
            ),
            Err(PlaywrightProxyError::ScopeViolation(
                ScopeViolation::BrowserContextId
            ))
        ));
    }

    fn text(value: Value) -> Message {
        json_message(value).unwrap()
    }

    fn client_response(action: ClientAction) -> Value {
        let ClientAction::Reply(Message::Text(text)) = action else {
            panic!("expected client reply")
        };
        serde_json::from_str(&text).unwrap()
    }

    #[test]
    fn browser_wide_and_unknown_methods_are_rejected_by_allowlist() {
        for method in [
            "Browser.close",
            "Browser.setPermission",
            "Network.clearBrowserCookies",
            "Page.setDownloadBehavior",
            "Storage.clearCookies",
            "Target.createBrowserContext",
            "Target.createTarget",
        ] {
            let response = client_response(
                client_request(
                    text(json!({ "id": 1, "method": method })),
                    &scope(),
                    &mut HashMap::new(),
                    &HashMap::new(),
                )
                .unwrap(),
            );
            assert!(
                response["error"]["message"]
                    .as_str()
                    .unwrap()
                    .contains("allowlist"),
                "{method} was not rejected: {response}"
            );
        }
    }

    #[test]
    fn duplicate_request_cannot_replace_target_filter_metadata() {
        let mut pending = HashMap::new();
        let internal = HashMap::new();
        let first = client_request(
            text(json!({ "id": 40, "method": "Target.getTargets" })),
            &scope(),
            &mut pending,
            &internal,
        )
        .unwrap();
        assert!(matches!(first, ClientAction::Forward(_)));

        let Err(duplicate) = client_request(
            text(json!({ "id": 40, "method": "Browser.getVersion" })),
            &scope(),
            &mut pending,
            &internal,
        ) else {
            panic!("duplicate request id was accepted")
        };
        assert!(matches!(
            duplicate,
            PlaywrightProxyError::DuplicateRequestId(ref id) if id == "40"
        ));
        assert!(matches!(
            pending.get("40"),
            Some(PendingRequest::Forwarded { method, .. }) if method == "Target.getTargets"
        ));

        let filtered = upstream_message(
            text(json!({
                "id": 40,
                "result": {
                    "targetInfos": [
                        {
                            "targetId": "unrelated",
                            "type": "page",
                            "title": "secret",
                            "url": "http://unrelated.test/",
                            "browserContextId": "other-context"
                        },
                        {
                            "targetId": "selected",
                            "type": "page",
                            "title": "selected",
                            "url": "http://selected.test/",
                            "browserContextId": "selected-context"
                        }
                    ]
                }
            })),
            &mut scope(),
            &mut pending,
            &mut HashMap::new(),
        )
        .unwrap();
        let UpstreamAction::Forward(Message::Text(filtered)) = filtered else {
            panic!("expected filtered response")
        };
        let filtered: Value = serde_json::from_str(&filtered).unwrap();
        assert_eq!(
            filtered["result"]["targetInfos"],
            json!([{
                "targetId": "selected",
                "type": "page",
                "title": "selected",
                "url": "http://selected.test/",
                "browserContextId": "selected-context"
            }])
        );
    }

    #[test]
    fn client_request_id_cannot_collide_with_internal_request() {
        let mut pending = HashMap::new();
        let internal = HashMap::from([("-1".to_owned(), InternalRequest::Detach)]);
        let Err(error) = client_request(
            text(json!({ "id": -1, "method": "Browser.getVersion" })),
            &scope(),
            &mut pending,
            &internal,
        ) else {
            panic!("internal request id collision was accepted")
        };

        assert!(matches!(
            error,
            PlaywrightProxyError::DuplicateRequestId(ref id) if id == "-1"
        ));
        assert!(pending.is_empty());
        assert!(matches!(internal.get("-1"), Some(InternalRequest::Detach)));
    }

    #[test]
    fn internal_request_id_skips_outstanding_client_id() {
        let pending = HashMap::from([(
            "-1".to_owned(),
            PendingRequest::Forwarded {
                method: "Browser.getVersion".to_owned(),
                session_id: None,
            },
        )]);
        let internal = HashMap::new();
        let mut next_internal_id = -1;

        let id = allocate_internal_request_id(&mut next_internal_id, &pending, &internal).unwrap();
        assert_eq!(id, json!(-2));
        assert!(matches!(
            pending.get("-1"),
            Some(PendingRequest::Forwarded { method, .. }) if method == "Browser.getVersion"
        ));
    }

    #[test]
    fn duplicate_internal_attach_client_id_is_rejected() {
        let mut pending = HashMap::new();
        let internal = HashMap::new();
        let first = client_request(
            text(json!({
                "id": 41,
                "method": "Target.setAutoAttach",
                "params": {
                    "autoAttach": true,
                    "waitForDebuggerOnStart": true,
                    "flatten": true
                }
            })),
            &scope(),
            &mut pending,
            &internal,
        )
        .unwrap();
        assert!(matches!(first, ClientAction::AttachSelected(_)));
        assert!(matches!(
            pending.get("41"),
            Some(PendingRequest::InternalAttach)
        ));

        let Err(duplicate) = client_request(
            text(json!({ "id": 41, "method": "Browser.getVersion" })),
            &scope(),
            &mut pending,
            &internal,
        ) else {
            panic!("duplicate internal attach id was accepted")
        };
        assert!(matches!(
            duplicate,
            PlaywrightProxyError::DuplicateRequestId(ref id) if id == "41"
        ));
        assert!(matches!(
            pending.get("41"),
            Some(PendingRequest::InternalAttach)
        ));
    }

    #[test]
    fn schema_identifiers_are_validated_for_their_methods() {
        for (method, params, expected) in [
            (
                "Target.closeTarget",
                json!({ "targetId": "other" }),
                "target identifier",
            ),
            (
                "Target.detachFromTarget",
                json!({ "sessionId": "other" }),
                "session identifier",
            ),
            (
                "Browser.setDownloadBehavior",
                json!({ "browserContextId": "other" }),
                "browser context identifier",
            ),
        ] {
            let response = client_response(
                client_request(
                    text(json!({
                        "id": 2,
                        "method": method,
                        "params": params
                    })),
                    &scope(),
                    &mut HashMap::new(),
                    &HashMap::new(),
                )
                .unwrap(),
            );
            assert!(
                response["error"]["message"]
                    .as_str()
                    .unwrap()
                    .contains(expected),
                "{method} was not rejected: {response}"
            );
        }
    }

    #[test]
    fn opaque_maps_are_not_scanned_for_identifier_key_names() {
        for (method, params) in [
            (
                "Network.setExtraHTTPHeaders",
                json!({ "headers": { "targetId": "opaque" } }),
            ),
            (
                "Runtime.callFunctionOn",
                json!({
                    "functionDeclaration": "() => 1",
                    "arguments": [{
                        "value": {
                            "sessionId": "opaque",
                            "browserContextId": "opaque"
                        }
                    }]
                }),
            ),
        ] {
            let action = client_request(
                text(json!({
                    "id": 3,
                    "method": method,
                    "sessionId": "selected-session",
                    "params": params
                })),
                &scope(),
                &mut HashMap::new(),
                &HashMap::new(),
            )
            .unwrap();
            assert!(
                matches!(action, ClientAction::Forward(_)),
                "{method} treated opaque data as protocol identifiers"
            );
        }
    }

    #[test]
    fn synthetic_session_response_preserves_session_id() {
        let response = client_response(
            client_request(
                text(json!({
                    "id": 3,
                    "method": "Browser.setWindowBounds",
                    "sessionId": "selected-session"
                })),
                &scope(),
                &mut HashMap::new(),
                &HashMap::new(),
            )
            .unwrap(),
        );
        assert_eq!(response["sessionId"], "selected-session");
    }

    #[test]
    fn selected_target_destruction_closes_the_proxy() {
        let action = upstream_message(
            text(json!({
                "method": "Target.targetDestroyed",
                "params": { "targetId": "selected" }
            })),
            &mut scope(),
            &mut HashMap::new(),
            &mut HashMap::new(),
        )
        .unwrap();
        assert!(matches!(action, UpstreamAction::ForwardAndClose(_)));
    }

    #[test]
    fn verified_oopif_descendant_session_is_allowed() {
        let mut scope = scope();
        let action = upstream_message(
            text(json!({
                "method": "Target.attachedToTarget",
                "sessionId": "selected-session",
                "params": {
                    "sessionId": "oopif-session",
                    "targetInfo": {
                        "targetId": "oopif-target",
                        "type": "iframe",
                        "title": "",
                        "url": "http://cross-origin.test/",
                        "browserContextId": "selected-context"
                    },
                    "waitingForDebugger": true
                }
            })),
            &mut scope,
            &mut HashMap::new(),
            &mut HashMap::new(),
        )
        .unwrap();

        assert!(matches!(action, UpstreamAction::Forward(_)));
        assert!(scope.sessions.contains("oopif-session"));
        assert_eq!(
            scope
                .session_target_ids
                .get("oopif-session")
                .map(String::as_str),
            Some("oopif-target")
        );
        assert!(scope.descendant_target_ids.contains("oopif-target"));
    }

    #[test]
    fn unrelated_or_unverified_target_sessions_are_detached() {
        for (parent_session, target_type, browser_context_id) in [
            (None, "iframe", "selected-context"),
            (Some("selected-session"), "page", "selected-context"),
            (Some("selected-session"), "iframe", "other-context"),
        ] {
            let mut scope = scope();
            let mut event = json!({
                "method": "Target.attachedToTarget",
                "params": {
                    "sessionId": "unrelated-session",
                    "targetInfo": {
                        "targetId": "unrelated-target",
                        "type": target_type,
                        "title": "",
                        "url": "http://unrelated.test/",
                        "browserContextId": browser_context_id
                    },
                    "waitingForDebugger": false
                }
            });
            if let Some(parent_session) = parent_session {
                event["sessionId"] = Value::String(parent_session.to_owned());
            }
            let action = upstream_message(
                text(event),
                &mut scope,
                &mut HashMap::new(),
                &mut HashMap::new(),
            )
            .unwrap();
            assert!(matches!(action, UpstreamAction::Detach(_)));
            assert!(!scope.sessions.contains("unrelated-session"));
            assert!(!scope.descendant_target_ids.contains("unrelated-target"));
        }
    }

    #[test]
    fn file_chooser_interception_is_page_scoped() {
        let action = client_request(
            text(json!({
                "id": 8,
                "method": "Page.setInterceptFileChooserDialog",
                "sessionId": "selected-session",
                "params": { "enabled": true }
            })),
            &scope(),
            &mut HashMap::new(),
            &HashMap::new(),
        )
        .unwrap();
        assert!(matches!(action, ClientAction::Forward(_)));
    }

    #[test]
    fn auxiliary_session_detachment_keeps_the_proxy_open() {
        let mut scope = scope();
        scope.register_page_session("auxiliary-session", "selected");
        let action = upstream_message(
            text(json!({
                "method": "Target.detachedFromTarget",
                "params": {
                    "sessionId": "auxiliary-session",
                    "targetId": "selected"
                }
            })),
            &mut scope,
            &mut HashMap::new(),
            &mut HashMap::new(),
        )
        .unwrap();

        assert!(matches!(action, UpstreamAction::Forward(_)));
        assert!(!scope.sessions.contains("auxiliary-session"));
        assert!(scope.sessions.contains("selected-session"));
        assert_eq!(
            scope.primary_session_id.as_deref(),
            Some("selected-session")
        );
        let continued = client_request(
            text(json!({
                "id": 7,
                "method": "Runtime.evaluate",
                "sessionId": "selected-session",
                "params": { "expression": "document.title" }
            })),
            &scope,
            &mut HashMap::new(),
            &HashMap::new(),
        )
        .unwrap();
        assert!(matches!(continued, ClientAction::Forward(_)));
    }

    #[test]
    fn primary_session_detachment_closes_the_proxy() {
        let mut scope = scope();
        let action = upstream_message(
            text(json!({
                "method": "Target.detachedFromTarget",
                "params": {
                    "sessionId": "selected-session",
                    "targetId": "selected"
                }
            })),
            &mut scope,
            &mut HashMap::new(),
            &mut HashMap::new(),
        )
        .unwrap();

        assert!(matches!(action, UpstreamAction::ForwardAndClose(_)));
        assert!(scope.primary_session_id.is_none());
    }

    #[test]
    fn browser_broker_can_only_attach_the_selected_target() {
        let mut scope = scope();
        scope.sessions.insert("browser-broker".to_owned());
        scope
            .browser_broker_sessions
            .insert("browser-broker".to_owned());

        let denied = client_response(
            client_request(
                text(json!({
                    "id": 4,
                    "method": "Runtime.evaluate",
                    "sessionId": "browser-broker",
                    "params": { "expression": "1" }
                })),
                &scope,
                &mut HashMap::new(),
                &HashMap::new(),
            )
            .unwrap(),
        );
        assert!(
            denied["error"]["message"]
                .as_str()
                .unwrap()
                .contains("allowlist")
        );

        let allowed = client_request(
            text(json!({
                "id": 5,
                "method": "Target.attachToTarget",
                "sessionId": "browser-broker",
                "params": { "targetId": "selected", "flatten": true }
            })),
            &scope,
            &mut HashMap::new(),
            &HashMap::new(),
        )
        .unwrap();
        assert!(matches!(allowed, ClientAction::Forward(_)));
    }

    #[test]
    fn browser_broker_session_is_tracked_separately() {
        let mut scope = scope();
        let mut pending = HashMap::from([(
            "6".to_owned(),
            PendingRequest::Forwarded {
                method: "Target.attachToBrowserTarget".to_owned(),
                session_id: None,
            },
        )]);
        let action = upstream_message(
            text(json!({
                "id": 6,
                "result": { "sessionId": "browser-broker" }
            })),
            &mut scope,
            &mut pending,
            &mut HashMap::new(),
        )
        .unwrap();

        assert!(matches!(action, UpstreamAction::Forward(_)));
        assert!(scope.sessions.contains("browser-broker"));
        assert!(scope.browser_broker_sessions.contains("browser-broker"));
        assert_eq!(
            scope.primary_session_id.as_deref(),
            Some("selected-session")
        );
    }

    #[test]
    fn browser_broker_attachment_is_hidden_from_the_client() {
        for target_type in ["browser", "other"] {
            let mut scope = scope();
            let action = upstream_message(
                text(json!({
                    "method": "Target.attachedToTarget",
                    "params": {
                        "sessionId": "browser-broker",
                        "targetInfo": {
                            "targetId": "browser-target",
                            "type": target_type,
                            "title": "",
                            "url": ""
                        },
                        "waitingForDebugger": false
                    }
                })),
                &mut scope,
                &mut HashMap::new(),
                &mut HashMap::new(),
            )
            .unwrap();

            assert!(matches!(action, UpstreamAction::Drop));
            assert!(scope.sessions.contains("browser-broker"));
            assert!(scope.browser_broker_sessions.contains("browser-broker"));
        }
    }

    #[test]
    fn selected_page_attachment_normalizes_current_chromium_target_type() {
        let mut scope = ActiveScope::new(page());
        let action = upstream_message(
            text(json!({
                "method": "Target.attachedToTarget",
                "params": {
                    "sessionId": "selected-session",
                    "targetInfo": {
                        "targetId": "selected",
                        "type": "other",
                        "title": "",
                        "url": ""
                    },
                    "waitingForDebugger": false
                }
            })),
            &mut scope,
            &mut HashMap::new(),
            &mut HashMap::new(),
        )
        .unwrap();

        let UpstreamAction::Forward(Message::Text(message)) = action else {
            panic!("selected page attachment was not forwarded");
        };
        let message = serde_json::from_str::<Value>(&message).unwrap();
        assert_eq!(message["params"]["targetInfo"]["type"], "page");
        assert_eq!(
            message["params"]["targetInfo"]["browserContextId"],
            "selected-context"
        );
        assert!(scope.sessions.contains("selected-session"));
        assert!(!scope.browser_broker_sessions.contains("selected-session"));
    }

    #[tokio::test]
    async fn invalid_authentication_does_not_consume_capability() {
        let (proxy, upstream) = proxy_with_mock_upstream().await;
        let invalid = proxy.websocket_url.replace("/session/", "/invalid/");
        assert!(connect_async(invalid).await.is_err());

        let (client, _) = connect_async(&proxy.websocket_url).await.unwrap();
        drop(client);
        timeout(Duration::from_secs(2), proxy.completion)
            .await
            .unwrap()
            .unwrap();
        upstream.await.unwrap();
    }

    #[tokio::test]
    async fn stalled_unauthenticated_handshake_does_not_hold_capability() {
        let (proxy, upstream) = proxy_with_mock_upstream().await;
        let url = url::Url::parse(&proxy.websocket_url).unwrap();
        let stalled = TcpStream::connect(("127.0.0.1", url.port().unwrap()))
            .await
            .unwrap();
        tokio::time::sleep(HANDSHAKE_TIMEOUT + Duration::from_millis(100)).await;

        let (client, _) = connect_async(&proxy.websocket_url).await.unwrap();
        drop(stalled);
        drop(client);
        timeout(Duration::from_secs(2), proxy.completion)
            .await
            .unwrap()
            .unwrap();
        upstream.await.unwrap();
    }

    #[tokio::test]
    async fn cancellation_interrupts_an_unauthenticated_handshake() {
        let (proxy, upstream) = proxy_with_mock_upstream().await;
        let url = url::Url::parse(&proxy.websocket_url).unwrap();
        let _stalled = TcpStream::connect(("127.0.0.1", url.port().unwrap()))
            .await
            .unwrap();
        proxy.cancel.send_replace(true);
        timeout(Duration::from_millis(500), proxy.completion)
            .await
            .unwrap()
            .unwrap();
        upstream.abort();
    }

    #[tokio::test]
    async fn cancellation_interrupts_a_stalled_upstream_connection() {
        let listener = TcpListener::bind(("127.0.0.1", 0)).await.unwrap();
        let endpoint = format!("ws://{}", listener.local_addr().unwrap());
        let (cancel_sender, mut cancel) = watch::channel(false);
        let connection = tokio::spawn(async move {
            connect_upstream(
                endpoint,
                websocket_config(),
                &mut cancel,
                Instant::now() + Duration::from_secs(5),
                Duration::from_secs(5),
            )
            .await
            .map(|upstream| upstream.is_some())
        });
        let (_stalled_upstream, _) = timeout(Duration::from_secs(1), listener.accept())
            .await
            .unwrap()
            .unwrap();

        cancel_sender.send_replace(true);
        let connected = timeout(Duration::from_millis(500), connection)
            .await
            .unwrap()
            .unwrap()
            .unwrap();
        assert!(!connected);
    }

    #[tokio::test]
    async fn stalled_upstream_connection_has_its_own_timeout() {
        let listener = TcpListener::bind(("127.0.0.1", 0)).await.unwrap();
        let endpoint = format!("ws://{}", listener.local_addr().unwrap());
        let (_cancel_sender, mut cancel) = watch::channel(false);
        let connection = tokio::spawn(async move {
            connect_upstream(
                endpoint,
                websocket_config(),
                &mut cancel,
                Instant::now() + Duration::from_secs(5),
                Duration::from_millis(100),
            )
            .await
            .map(|_| ())
        });
        let (_stalled_upstream, _) = timeout(Duration::from_secs(1), listener.accept())
            .await
            .unwrap()
            .unwrap();

        let error = timeout(Duration::from_secs(1), connection)
            .await
            .unwrap()
            .unwrap()
            .unwrap_err();
        assert!(matches!(
            error,
            PlaywrightProxyError::UpstreamConnectTimeout
        ));
    }

    #[tokio::test]
    async fn stalled_upstream_cannot_outlive_the_capability_deadline() {
        let listener = TcpListener::bind(("127.0.0.1", 0)).await.unwrap();
        let endpoint = format!("ws://{}", listener.local_addr().unwrap());
        let (_cancel_sender, mut cancel) = watch::channel(false);
        let connection = tokio::spawn(async move {
            connect_upstream(
                endpoint,
                websocket_config(),
                &mut cancel,
                Instant::now() + Duration::from_millis(100),
                Duration::from_secs(5),
            )
            .await
            .map(|_| ())
        });
        let (_stalled_upstream, _) = timeout(Duration::from_secs(1), listener.accept())
            .await
            .unwrap()
            .unwrap();

        let error = timeout(Duration::from_secs(1), connection)
            .await
            .unwrap()
            .unwrap()
            .unwrap_err();
        assert!(matches!(error, PlaywrightProxyError::CapabilityTimeout));
    }

    async fn proxy_with_mock_upstream() -> (PlaywrightProxy, tokio::task::JoinHandle<()>) {
        let listener = TcpListener::bind(("127.0.0.1", 0)).await.unwrap();
        let endpoint = format!("ws://{}", listener.local_addr().unwrap());
        let upstream = tokio::spawn(async move {
            let (stream, _) = listener.accept().await.unwrap();
            let socket = accept_async(stream).await.unwrap();
            let (_, mut receiver) = socket.split();
            while receiver.next().await.is_some() {}
        });
        let proxy = start(
            PlaywrightCdpSource::BrowserRoot { endpoint },
            page(),
            "secret".to_owned(),
        )
        .await
        .unwrap();
        (proxy, upstream)
    }
}
