use std::collections::{HashMap, HashSet};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use futures_util::{SinkExt, StreamExt};
use serde_json::{Value, json};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::{oneshot, watch};
use tokio_tungstenite::tungstenite::handshake::server::{ErrorResponse, Request, Response};
use tokio_tungstenite::tungstenite::http::StatusCode;
use tokio_tungstenite::tungstenite::protocol::WebSocketConfig;
use tokio_tungstenite::tungstenite::{Error as WebSocketError, Message};
use tokio_tungstenite::{accept_hdr_async_with_config, connect_async_with_config};

const ACCEPT_TIMEOUT: Duration = Duration::from_secs(15);
const SESSION_TIMEOUT: Duration = Duration::from_secs(45);
const MAX_MESSAGE_SIZE: usize = 16 * 1024 * 1024;

#[derive(Clone, Debug)]
pub enum PlaywrightCdpSource {
    BrowserRoot { endpoint: String },
}

pub struct PlaywrightProxy {
    pub websocket_url: String,
    pub cancel: watch::Sender<bool>,
    pub completion: oneshot::Receiver<()>,
}

pub async fn start(
    source: PlaywrightCdpSource,
    target_id: String,
    token: String,
) -> Result<PlaywrightProxy, PlaywrightProxyError> {
    let listener = TcpListener::bind(("127.0.0.1", 0)).await?;
    let address = listener.local_addr()?;
    let path = format!("/session/{token}");
    let websocket_url = format!("ws://127.0.0.1:{}{path}", address.port());
    let (cancel, cancel_receiver) = watch::channel(false);
    let (completion_sender, completion) = oneshot::channel();
    tokio::spawn(async move {
        let _ = run(listener, source, target_id, path, cancel_receiver).await;
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
    target_id: String,
    path: String,
    mut cancel: watch::Receiver<bool>,
) -> Result<(), PlaywrightProxyError> {
    let accepted = tokio::select! {
        result = tokio::time::timeout(ACCEPT_TIMEOUT, listener.accept()) => {
            result.map_err(|_| PlaywrightProxyError::AcceptTimeout)??
        }
        _ = cancel.changed() => return Ok(()),
    };
    let (stream, peer) = accepted;
    if !peer.ip().is_loopback() {
        return Err(PlaywrightProxyError::NonLoopbackPeer);
    }
    drop(listener);

    let authenticated = Arc::new(AtomicBool::new(false));
    let observed_authentication = authenticated.clone();
    let expected_path = path.clone();
    let mut config = WebSocketConfig::default();
    config.max_message_size = Some(MAX_MESSAGE_SIZE);
    config.max_frame_size = Some(MAX_MESSAGE_SIZE);
    let client = accept_hdr_async_with_config(
        stream,
        move |request: &Request, response: Response| {
            if request.uri().path() != expected_path {
                return Err(unauthorized());
            }
            observed_authentication.store(true, Ordering::Relaxed);
            Ok(response)
        },
        Some(config.clone()),
    )
    .await?;
    if !authenticated.load(Ordering::Relaxed) {
        return Err(PlaywrightProxyError::AuthenticationFailed);
    }

    let PlaywrightCdpSource::BrowserRoot { endpoint } = source;
    let (upstream, _) = connect_async_with_config(endpoint, Some(config), false).await?;
    bridge(client, upstream, target_id, cancel).await
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
    target_id: String,
    mut cancel: watch::Receiver<bool>,
) -> Result<(), PlaywrightProxyError> {
    let (mut client_sender, mut client_receiver) = client.split();
    let (mut upstream_sender, mut upstream_receiver) = upstream.split();
    let mut pending = HashMap::<String, String>::new();
    let mut allowed_sessions = HashSet::<String>::new();
    let mut internal_id = -1_i64;
    let deadline = tokio::time::sleep(SESSION_TIMEOUT);
    tokio::pin!(deadline);

    loop {
        tokio::select! {
            _ = &mut deadline => return Err(PlaywrightProxyError::SessionTimeout),
            _ = cancel.changed() => return Ok(()),
            message = client_receiver.next() => {
                let Some(message) = message else { return Ok(()); };
                let message = message?;
                match client_request(
                    message,
                    &target_id,
                    &allowed_sessions,
                    &mut pending,
                )? {
                    ClientAction::Forward(message) => upstream_sender.send(message).await?,
                    ClientAction::Reply(message) => client_sender.send(message).await?,
                    ClientAction::Drop => {}
                }
            }
            message = upstream_receiver.next() => {
                let Some(message) = message else { return Ok(()); };
                let message = message?;
                match upstream_message(
                    message,
                    &target_id,
                    &mut allowed_sessions,
                    &mut pending,
                )? {
                    UpstreamAction::Forward(message) => client_sender.send(message).await?,
                    UpstreamAction::Drop => {}
                    UpstreamAction::Detach(session_id) => {
                        upstream_sender.send(json_message(json!({
                            "id": internal_id,
                            "method": "Target.detachFromTarget",
                            "params": { "sessionId": session_id }
                        }))?).await?;
                        internal_id -= 1;
                    }
                }
            }
        }
    }
}

fn client_request(
    message: Message,
    target_id: &str,
    allowed_sessions: &HashSet<String>,
    pending: &mut HashMap<String, String>,
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
        .unwrap_or_default();
    let id = object.get("id").cloned();
    if let Some(session_id) = object.get("sessionId").and_then(Value::as_str)
        && !allowed_sessions.contains(session_id)
    {
        return response_error(id, -32000, "CDP session is outside the selected page");
    }
    if matches!(
        method,
        "Browser.close" | "Target.createTarget" | "Target.createBrowserContext"
    ) {
        return response_error(id, -32000, "operation is outside the selected page");
    }
    if let Some(requested_target) = object
        .get("params")
        .and_then(|params| params.get("targetId"))
        .and_then(Value::as_str)
        && requested_target != target_id
    {
        return response_error(id, -32000, "target is outside the selected page");
    }
    if let Some(id) = object.get("id") {
        pending.insert(id_key(id)?, method.to_owned());
    }
    Ok(ClientAction::Forward(json_message(value)?))
}

fn upstream_message(
    message: Message,
    target_id: &str,
    allowed_sessions: &mut HashSet<String>,
    pending: &mut HashMap<String, String>,
) -> Result<UpstreamAction, PlaywrightProxyError> {
    let Some(mut value) = parse_data_message(message)? else {
        return Ok(UpstreamAction::Drop);
    };
    let Some(object) = value.as_object_mut() else {
        return Err(PlaywrightProxyError::InvalidCdpMessage);
    };

    if let Some(id) = object.get("id") {
        let key = id_key(id)?;
        if key.starts_with('-') {
            return Ok(UpstreamAction::Drop);
        }
        if pending.remove(&key).as_deref() == Some("Target.getTargets")
            && let Some(infos) = object
                .get_mut("result")
                .and_then(|result| result.get_mut("targetInfos"))
                .and_then(Value::as_array_mut)
        {
            infos.retain(|info| target_info_id(info) == Some(target_id));
        }
        return Ok(UpstreamAction::Forward(json_message(value)?));
    }

    let method = object
        .get("method")
        .and_then(Value::as_str)
        .unwrap_or_default();
    if method == "Target.attachedToTarget" {
        let params = object.get("params");
        let session_id = params
            .and_then(|params| params.get("sessionId"))
            .and_then(Value::as_str)
            .ok_or(PlaywrightProxyError::InvalidCdpMessage)?;
        let attached_target = params
            .and_then(|params| params.get("targetInfo"))
            .and_then(target_info_id)
            .ok_or(PlaywrightProxyError::InvalidCdpMessage)?;
        if attached_target == target_id {
            allowed_sessions.insert(session_id.to_owned());
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
        return Ok(if allowed_sessions.remove(session_id) {
            UpstreamAction::Forward(json_message(value)?)
        } else {
            UpstreamAction::Drop
        });
    }
    if matches!(method, "Target.targetCreated" | "Target.targetInfoChanged") {
        let matches = object
            .get("params")
            .and_then(|params| params.get("targetInfo"))
            .and_then(target_info_id)
            == Some(target_id);
        return Ok(if matches {
            UpstreamAction::Forward(json_message(value)?)
        } else {
            UpstreamAction::Drop
        });
    }
    if method == "Target.targetDestroyed" {
        let matches = object
            .get("params")
            .and_then(|params| params.get("targetId"))
            .and_then(Value::as_str)
            == Some(target_id);
        return Ok(if matches {
            UpstreamAction::Forward(json_message(value)?)
        } else {
            UpstreamAction::Drop
        });
    }
    if let Some(session_id) = object.get("sessionId").and_then(Value::as_str)
        && !allowed_sessions.contains(session_id)
    {
        return Ok(UpstreamAction::Drop);
    }
    Ok(UpstreamAction::Forward(json_message(value)?))
}

fn target_info_id(value: &Value) -> Option<&str> {
    value.get("targetId").and_then(Value::as_str)
}

fn response_error(
    id: Option<Value>,
    code: i32,
    message: &str,
) -> Result<ClientAction, PlaywrightProxyError> {
    Ok(match id {
        Some(id) => ClientAction::Reply(json_message(
            json!({ "id": id, "error": { "code": code, "message": message } }),
        )?),
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

enum UpstreamAction {
    Forward(Message),
    Drop,
    Detach(String),
}

enum ClientAction {
    Forward(Message),
    Reply(Message),
    Drop,
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
    #[error("Playwright proxy rejected a non-loopback peer")]
    NonLoopbackPeer,
    #[error("Playwright proxy authentication failed")]
    AuthenticationFailed,
    #[error("Playwright proxy received an invalid CDP message")]
    InvalidCdpMessage,
}

#[cfg(test)]
mod tests {
    use super::*;

    fn text(value: Value) -> Message {
        json_message(value).unwrap()
    }

    fn value(action: UpstreamAction) -> Value {
        let UpstreamAction::Forward(Message::Text(text)) = action else {
            panic!("expected forwarded text")
        };
        serde_json::from_str(&text).unwrap()
    }

    #[test]
    fn target_listing_contains_only_the_selected_page() {
        let mut pending = HashMap::from([("7".to_owned(), "Target.getTargets".to_owned())]);
        let action = upstream_message(
            text(json!({
                "id": 7,
                "result": {
                    "targetInfos": [
                        { "targetId": "selected" },
                        { "targetId": "other" }
                    ]
                }
            })),
            "selected",
            &mut HashSet::new(),
            &mut pending,
        )
        .unwrap();
        assert_eq!(
            value(action)["result"]["targetInfos"],
            json!([{ "targetId": "selected" }])
        );
    }

    #[test]
    fn sessions_for_other_targets_are_detached() {
        let action = upstream_message(
            text(json!({
                "method": "Target.attachedToTarget",
                "params": {
                    "sessionId": "other-session",
                    "targetInfo": { "targetId": "other" }
                }
            })),
            "selected",
            &mut HashSet::new(),
            &mut HashMap::new(),
        )
        .unwrap();
        assert!(matches!(
            action,
            UpstreamAction::Detach(session) if session == "other-session"
        ));
    }

    #[test]
    fn requests_cannot_name_another_target() {
        let response = client_request(
            text(json!({
                "id": 2,
                "method": "Target.activateTarget",
                "params": { "targetId": "other" }
            })),
            "selected",
            &HashSet::new(),
            &mut HashMap::new(),
        )
        .unwrap();
        let ClientAction::Reply(Message::Text(response)) = response else {
            panic!("expected text response")
        };
        let response: Value = serde_json::from_str(&response).unwrap();
        assert_eq!(
            response["error"]["message"],
            "target is outside the selected page"
        );
    }
}
