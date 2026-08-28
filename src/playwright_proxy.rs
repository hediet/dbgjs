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
    tokio::spawn(async move {
        let _ = run(listener, source, page, path, cancel_receiver).await;
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
) -> Result<(), PlaywrightProxyError> {
    let Some(client) = accept_authenticated(&listener, &path, &mut cancel).await? else {
        return Ok(());
    };
    drop(listener);

    let config = websocket_config();
    let PlaywrightCdpSource::BrowserRoot { endpoint } = source;
    let (upstream, _) = connect_async_with_config(endpoint, Some(config.clone()), false).await?;
    bridge(client, upstream, page, cancel).await
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
) -> Result<(), PlaywrightProxyError> {
    let (mut client_sender, mut client_receiver) = client.split();
    let (mut upstream_sender, mut upstream_receiver) = upstream.split();
    let mut scope = ActiveScope::new(page);
    let mut pending = HashMap::<String, PendingRequest>::new();
    let mut internal = HashMap::<String, InternalRequest>::new();
    let mut next_internal_id = -1_i64;
    let deadline = tokio::time::sleep(SESSION_TIMEOUT);
    tokio::pin!(deadline);

    loop {
        tokio::select! {
            _ = &mut deadline => return Err(PlaywrightProxyError::SessionTimeout),
            _ = cancel.changed() => return Ok(()),
            message = client_receiver.next() => {
                let Some(message) = message else { return Ok(()); };
                match client_request(message?, &scope, &mut pending)? {
                    ClientAction::Forward(message) => upstream_sender.send(message).await?,
                    ClientAction::Reply(message) => client_sender.send(message).await?,
                    ClientAction::AttachSelected(request_id) => {
                        let internal_id = Value::from(next_internal_id);
                        next_internal_id -= 1;
                        internal.insert(
                            id_key(&internal_id)?,
                            InternalRequest::AutoAttach { request_id },
                        );
                        upstream_sender.send(json_message(json!({
                            "id": internal_id,
                            "method": "Target.attachToTarget",
                            "params": {
                                "targetId": scope.page.target_id,
                                "flatten": true
                            }
                        }))?).await?;
                    }
                    ClientAction::Drop => {}
                }
            }
            message = upstream_receiver.next() => {
                let Some(message) = message else { return Ok(()); };
                match upstream_message(
                    message?,
                    &mut scope,
                    &mut pending,
                    &mut internal,
                )? {
                    UpstreamAction::Forward(message) => client_sender.send(message).await?,
                    UpstreamAction::ForwardAndClose(message) => {
                        client_sender.send(message).await?;
                        return Err(PlaywrightProxyError::SelectedTargetDestroyed);
                    }
                    UpstreamAction::Reply(message) => client_sender.send(message).await?,
                    UpstreamAction::Drop => {}
                    UpstreamAction::Detach(session_id) => {
                        let internal_id = Value::from(next_internal_id);
                        next_internal_id -= 1;
                        internal.insert(id_key(&internal_id)?, InternalRequest::Detach);
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

struct ActiveScope {
    page: PlaywrightPageScope,
    sessions: HashSet<String>,
}

impl ActiveScope {
    fn new(page: PlaywrightPageScope) -> Self {
        Self {
            page,
            sessions: HashSet::new(),
        }
    }

    fn validate_identifiers(&self, value: &Value) -> Result<(), ScopeViolation> {
        validate_identifiers(value, &self.page, &self.sessions)
    }
}

#[derive(Clone)]
struct PendingRequest {
    method: String,
}

enum InternalRequest {
    AutoAttach { request_id: Value },
    Detach,
}

fn client_request(
    message: Message,
    scope: &ActiveScope,
    pending: &mut HashMap<String, PendingRequest>,
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
    if let Err(error) = scope.validate_identifiers(&Value::Object(object.clone())) {
        return scope_error(id, response_session, error);
    }
    let session = object.get("sessionId").and_then(Value::as_str);

    let policy = if session.is_some() {
        session_method_policy(&method)
    } else {
        root_method_policy(&method)
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
            Some(id) => Ok(ClientAction::AttachSelected(id)),
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
        pending.insert(id_key(id)?, PendingRequest { method });
    }
    Ok(ClientAction::Forward(json_message(value)?))
}

fn root_method_policy(method: &str) -> MethodPolicy {
    match method {
        "Browser.getVersion" | "Target.getTargets" => MethodPolicy::Forward,
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

fn session_method_policy(method: &str) -> MethodPolicy {
    match method {
        "Browser.getWindowForTarget" | "Target.getTargetInfo" => MethodPolicy::Forward,
        "Browser.setDownloadBehavior"
        | "Browser.setWindowBounds"
        | "Target.setAutoAttach"
        | "Target.setDiscoverTargets" => MethodPolicy::SyntheticSuccess,
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
            return internal_response(object, request, scope);
        }
        let Some(request) = pending.remove(&key) else {
            return Ok(UpstreamAction::Drop);
        };
        match request.method.as_str() {
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
                    validate_target_info(info, &scope.page)?;
                }
            }
            "Target.getTargetInfo" => {
                let info = object
                    .get("result")
                    .and_then(|result| result.get("targetInfo"))
                    .ok_or(PlaywrightProxyError::InvalidCdpMessage)?;
                validate_target_info(info, &scope.page)?;
            }
            "Target.attachToTarget" => {
                let session_id = object
                    .get("result")
                    .and_then(|result| result.get("sessionId"))
                    .and_then(Value::as_str)
                    .ok_or(PlaywrightProxyError::InvalidCdpMessage)?;
                scope.sessions.insert(session_id.to_owned());
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
        if target_info_id(target_info) == Some(&scope.page.target_id) {
            validate_target_info(target_info, &scope.page)?;
            scope.sessions.insert(session_id.to_owned());
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
        return Ok(if scope.sessions.remove(session_id) {
            UpstreamAction::ForwardAndClose(json_message(value)?)
        } else {
            UpstreamAction::Drop
        });
    }
    if matches!(method, "Target.targetCreated" | "Target.targetInfoChanged") {
        let target_info = object
            .get("params")
            .and_then(|params| params.get("targetInfo"))
            .ok_or(PlaywrightProxyError::InvalidCdpMessage)?;
        return Ok(
            if target_info_id(target_info) == Some(&scope.page.target_id) {
                validate_target_info(target_info, &scope.page)?;
                UpstreamAction::Forward(json_message(value)?)
            } else {
                UpstreamAction::Drop
            },
        );
    }
    if method == "Target.targetDestroyed" {
        let target_id = object
            .get("params")
            .and_then(|params| params.get("targetId"))
            .and_then(Value::as_str)
            .ok_or(PlaywrightProxyError::InvalidCdpMessage)?;
        return Ok(if target_id == scope.page.target_id {
            UpstreamAction::ForwardAndClose(json_message(value)?)
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
    Ok(UpstreamAction::Forward(json_message(value)?))
}

fn internal_response(
    object: &Map<String, Value>,
    request: InternalRequest,
    scope: &mut ActiveScope,
) -> Result<UpstreamAction, PlaywrightProxyError> {
    match request {
        InternalRequest::Detach => Ok(UpstreamAction::Drop),
        InternalRequest::AutoAttach { request_id } => {
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
                scope.sessions.insert(session_id.to_owned());
            }
            Ok(UpstreamAction::Reply(json_message(json!({
                "id": request_id,
                "result": {}
            }))?))
        }
    }
}

fn validate_target_info(
    target_info: &Value,
    page: &PlaywrightPageScope,
) -> Result<(), PlaywrightProxyError> {
    if target_info_id(target_info) != Some(&page.target_id) {
        return Err(PlaywrightProxyError::ScopeViolation(
            ScopeViolation::TargetId,
        ));
    }
    let context = target_info
        .get("browserContextId")
        .and_then(Value::as_str)
        .map(str::to_owned);
    if context != page.browser_context_id {
        return Err(PlaywrightProxyError::ScopeViolation(
            ScopeViolation::BrowserContextId,
        ));
    }
    Ok(())
}

fn validate_identifiers(
    value: &Value,
    page: &PlaywrightPageScope,
    sessions: &HashSet<String>,
) -> Result<(), ScopeViolation> {
    match value {
        Value::Array(items) => {
            for item in items {
                validate_identifiers(item, page, sessions)?;
            }
        }
        Value::Object(object) => {
            for (key, value) in object {
                match key.as_str() {
                    "targetId" => validate_string(value, |id| id == page.target_id)
                        .then_some(())
                        .ok_or(ScopeViolation::TargetId)?,
                    "targetIds" => validate_string_array(value, |id| id == page.target_id)
                        .then_some(())
                        .ok_or(ScopeViolation::TargetId)?,
                    "sessionId" => validate_string(value, |id| sessions.contains(id))
                        .then_some(())
                        .ok_or(ScopeViolation::SessionId)?,
                    "sessionIds" => validate_string_array(value, |id| sessions.contains(id))
                        .then_some(())
                        .ok_or(ScopeViolation::SessionId)?,
                    "browserContextId" => {
                        validate_string(value, |id| page.browser_context_id.as_deref() == Some(id))
                            .then_some(())
                            .ok_or(ScopeViolation::BrowserContextId)?
                    }
                    "browserContextIds" => validate_string_array(value, |id| {
                        page.browser_context_id.as_deref() == Some(id)
                    })
                    .then_some(())
                    .ok_or(ScopeViolation::BrowserContextId)?,
                    _ => validate_identifiers(value, page, sessions)?,
                }
            }
        }
        _ => {}
    }
    Ok(())
}

fn validate_string(value: &Value, predicate: impl FnOnce(&str) -> bool) -> bool {
    value.as_str().is_some_and(predicate)
}

fn validate_string_array(value: &Value, predicate: impl Fn(&str) -> bool) -> bool {
    value.as_array().is_some_and(|items| {
        items
            .iter()
            .all(|item| item.as_str().is_some_and(&predicate))
    })
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
    #[error("Playwright proxy session exceeded its deadline")]
    SessionTimeout,
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
        scope.sessions.insert("selected-session".to_owned());
        scope
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
    fn every_scope_identifier_is_validated() {
        for (field, value, expected) in [
            ("targetId", json!("other"), "target identifier"),
            (
                "targetIds",
                json!(["selected", "other"]),
                "target identifier",
            ),
            ("sessionId", json!("other"), "session identifier"),
            (
                "sessionIds",
                json!(["selected-session", "other"]),
                "session identifier",
            ),
            (
                "browserContextId",
                json!("other"),
                "browser context identifier",
            ),
            (
                "browserContextIds",
                json!(["selected-context", "other"]),
                "browser context identifier",
            ),
        ] {
            let mut params = Map::new();
            params.insert(field.to_owned(), value);
            let response = client_response(
                client_request(
                    text(json!({
                        "id": 2,
                        "method": "Runtime.evaluate",
                        "sessionId": "selected-session",
                        "params": params
                    })),
                    &scope(),
                    &mut HashMap::new(),
                )
                .unwrap(),
            );
            assert!(
                response["error"]["message"]
                    .as_str()
                    .unwrap()
                    .contains(expected),
                "{field} was not rejected: {response}"
            );
        }
    }

    #[test]
    fn synthetic_session_response_preserves_session_id() {
        let response = client_response(
            client_request(
                text(json!({
                    "id": 3,
                    "method": "Target.setAutoAttach",
                    "sessionId": "selected-session"
                })),
                &scope(),
                &mut HashMap::new(),
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
