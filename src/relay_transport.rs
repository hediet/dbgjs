use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use async_trait::async_trait;
use futures_util::stream::{SplitSink, SplitStream};
use futures_util::{SinkExt, StreamExt};
use hubrpc::prelude::{MessageTransport, TransportError};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::{Mutex, watch};
use tokio::time::Instant;
use tokio_tungstenite::WebSocketStream;
use tokio_tungstenite::accept_hdr_async_with_config;
use tokio_tungstenite::tungstenite::Message;
use tokio_tungstenite::tungstenite::handshake::server::{ErrorResponse, Request, Response};
use tokio_tungstenite::tungstenite::http::StatusCode;
use tokio_tungstenite::tungstenite::protocol::WebSocketConfig;

use crate::cdp_transport::{ManagedCdpTransport, closed_transport_error};
use crate::session_transport::CdpEnvelope;

/// Default time a relay listener waits for its one client to connect before giving up. Kept
/// generous because the relay CLI still has to resolve the service and dial the loopback socket.
pub const DEFAULT_ACCEPT_TIMEOUT: Duration = Duration::from_secs(30);
const HANDSHAKE_TIMEOUT: Duration = Duration::from_secs(1);
const MAX_MESSAGE_SIZE: usize = 128 * 1024 * 1024;

type Socket = WebSocketStream<TcpStream>;

/// A raw CDP-over-WebSocket server transport for one accepted, authenticated loopback client.
/// Speaks the same [`CdpEnvelope`] wire format as `CdpWebSocketTransport`/`CdpStdioTransport`, so
/// the `jsdbg context relay`/`jsdbg target relay` CLI can bridge it to stdio verbatim, without
/// any protocol translation of its own.
pub struct RelayServerTransport {
    sender: Mutex<SplitSink<Socket, Message>>,
    receiver: Mutex<SplitStream<Socket>>,
    close_reason: Arc<Mutex<Option<String>>>,
    closed: watch::Sender<bool>,
}

impl RelayServerTransport {
    fn new(socket: Socket) -> Self {
        let (sender, receiver) = socket.split();
        Self {
            sender: Mutex::new(sender),
            receiver: Mutex::new(receiver),
            close_reason: Arc::new(Mutex::new(None)),
            closed: watch::channel(false).0,
        }
    }

    async fn close_with(&self, reason: String) {
        let mut close_reason = self.close_reason.lock().await;
        if close_reason.is_none() {
            *close_reason = Some(reason);
        }
        self.closed.send_replace(true);
    }
}

#[async_trait]
impl MessageTransport<CdpEnvelope, CdpEnvelope> for RelayServerTransport {
    async fn send(&self, envelope: CdpEnvelope) -> Result<(), TransportError> {
        if *self.closed.borrow() {
            return Err(TransportError::Closed);
        }
        let json = serde_json::to_string(&envelope)
            .map_err(|error| closed_transport_error(error.to_string()))?;
        if let Err(error) = self
            .sender
            .lock()
            .await
            .send(Message::Text(json.into()))
            .await
        {
            self.close_with(format!("relay WebSocket send failed: {error}"))
                .await;
            return Err(closed_transport_error(error.to_string()));
        }
        Ok(())
    }

    async fn recv(&self) -> Option<CdpEnvelope> {
        let mut closed = self.closed.subscribe();
        loop {
            if *closed.borrow() {
                return None;
            }
            let message = {
                let mut receiver = self.receiver.lock().await;
                tokio::select! {
                    changed = closed.changed() => {
                        let _ = changed;
                        return None;
                    }
                    message = receiver.next() => message,
                }
            };
            let Some(message) = message else {
                self.close_with("relay WebSocket stream ended".into()).await;
                return None;
            };
            let message = match message {
                Ok(message) => message,
                Err(error) => {
                    self.close_with(format!("relay WebSocket receive failed: {error}"))
                        .await;
                    return None;
                }
            };
            let bytes = match message {
                Message::Text(text) => text.as_bytes().to_vec(),
                Message::Binary(bytes) => bytes.to_vec(),
                Message::Close(frame) => {
                    self.close_with(format!("relay WebSocket closed: {frame:?}"))
                        .await;
                    return None;
                }
                Message::Ping(_) | Message::Pong(_) | Message::Frame(_) => continue,
            };
            match serde_json::from_slice(&bytes) {
                Ok(envelope) => return Some(envelope),
                Err(error) => {
                    self.close_with(format!("invalid CDP JSON from relay client: {error}"))
                        .await;
                    return None;
                }
            }
        }
    }
}

#[async_trait]
impl ManagedCdpTransport for RelayServerTransport {
    fn close_reason(&self) -> Arc<Mutex<Option<String>>> {
        self.close_reason.clone()
    }

    async fn wait_closed(&self) -> String {
        let mut closed = self.closed.subscribe();
        if !*closed.borrow() {
            let _ = closed.changed().await;
        }
        self.close_reason
            .lock()
            .await
            .clone()
            .unwrap_or_else(|| "relay WebSocket closed".into())
    }

    async fn close(&self) {
        self.close_with("relay WebSocket closed by debugger service".into())
            .await;
        if let Err(error) = self.sender.lock().await.close().await {
            self.close_with(format!("relay WebSocket close handshake failed: {error}"))
                .await;
        }
    }
}

/// A bound loopback listener plus the URL its one client should connect to. Binding is kept
/// separate from accepting so callers can register relay exclusivity state before any client
/// has connected, matching the requirement that a relay takes ownership immediately.
pub struct RelayListener {
    listener: TcpListener,
    path: String,
    pub websocket_url: String,
}

/// Binds a loopback listener and computes its capability URL. `kind` (e.g. `"context"` or
/// `"target"`) only labels the path for diagnostics; `token` is the random capability secret.
pub async fn bind(kind: &str, token: &str) -> Result<RelayListener, RelayTransportError> {
    let listener = TcpListener::bind(("127.0.0.1", 0)).await?;
    let address = listener.local_addr()?;
    let path = format!("/relay/{kind}/{token}");
    let websocket_url = format!("ws://127.0.0.1:{}{path}", address.port());
    Ok(RelayListener {
        listener,
        path,
        websocket_url,
    })
}

impl RelayListener {
    /// Waits for the one authenticated loopback client this relay will ever serve. Returns
    /// `Ok(None)` if `cancel` fires or no client connects before `timeout` elapses.
    pub async fn accept(
        self,
        timeout: Duration,
        cancel: &mut watch::Receiver<bool>,
    ) -> Result<Option<RelayServerTransport>, RelayTransportError> {
        let deadline = Instant::now() + timeout;
        loop {
            if *cancel.borrow() {
                return Ok(None);
            }
            let accepted = tokio::select! {
                result = tokio::time::timeout_at(deadline, self.listener.accept()) => {
                    match result {
                        Ok(accepted) => accepted?,
                        Err(_) => return Ok(None),
                    }
                }
                _ = cancel.changed() => return Ok(None),
            };
            let (stream, peer) = accepted;
            if !peer.ip().is_loopback() {
                continue;
            }

            let authenticated = Arc::new(AtomicBool::new(false));
            let observed_authentication = authenticated.clone();
            let expected_path = self.path.clone();
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
                Ok(Ok(socket)) if authenticated.load(Ordering::Relaxed) => {
                    return Ok(Some(RelayServerTransport::new(socket)));
                }
                Ok(Ok(_)) | Ok(Err(_)) | Err(_) => continue,
            }
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
        .body(Some("invalid relay capability".to_owned()))
        .expect("static authentication response is valid")
}

#[derive(Debug, thiserror::Error)]
pub enum RelayTransportError {
    #[error(transparent)]
    Io(#[from] std::io::Error),
}

#[cfg(test)]
mod tests {
    use super::*;
    use hubrpc::prelude::{JsonRpcMessage, JsonRpcRequest};
    use hubrpc::protocol::jsonrpc::RequestId;
    use serde_json::json;
    use tokio_tungstenite::connect_async;

    #[tokio::test]
    async fn rejects_connections_to_the_wrong_path() {
        let listener = bind("context", "secret-token").await.unwrap();
        let websocket_url = listener.websocket_url.clone();
        let (_cancel, mut cancel_receiver) = watch::channel(false);
        let accept = tokio::spawn(async move {
            listener
                .accept(Duration::from_secs(2), &mut cancel_receiver)
                .await
        });

        let wrong_url = websocket_url.replace("secret-token", "wrong-token");
        assert!(connect_async(&wrong_url).await.is_err());

        let (socket, _) = connect_async(&websocket_url).await.unwrap();
        drop(socket);
        assert!(accept.await.unwrap().unwrap().is_some());
    }

    #[tokio::test]
    async fn cancelling_before_a_client_connects_stops_the_accept_loop() {
        let listener = bind("target", "another-token").await.unwrap();
        let (cancel, mut cancel_receiver) = watch::channel(false);
        let accept = tokio::spawn(async move {
            listener
                .accept(Duration::from_secs(5), &mut cancel_receiver)
                .await
        });
        cancel.send_replace(true);
        assert!(accept.await.unwrap().unwrap().is_none());
    }

    #[tokio::test]
    async fn transport_round_trips_cdp_envelopes_as_compact_json() {
        let listener = bind("context", "round-trip-token").await.unwrap();
        let websocket_url = listener.websocket_url.clone();
        let (_cancel, mut cancel_receiver) = watch::channel(false);
        let accept = tokio::spawn(async move {
            listener
                .accept(Duration::from_secs(2), &mut cancel_receiver)
                .await
        });
        let (client, _) = connect_async(&websocket_url).await.unwrap();
        let server = accept.await.unwrap().unwrap().unwrap();

        let envelope = CdpEnvelope {
            session_id: Some("session-a".into()),
            message: JsonRpcMessage::Request(JsonRpcRequest {
                id: RequestId::Number(1),
                method: "Target.getTargets".into(),
                params: Some(json!({})),
            }),
        };
        server.send(envelope.clone()).await.unwrap();

        let (mut client_sender, mut client_receiver) = client.split();
        let received = client_receiver.next().await.unwrap().unwrap();
        let Message::Text(text) = received else {
            panic!("expected a text frame");
        };
        assert_eq!(
            serde_json::from_str::<CdpEnvelope>(&text).unwrap(),
            envelope
        );

        client_sender
            .send(Message::Text(
                serde_json::to_string(&envelope).unwrap().into(),
            ))
            .await
            .unwrap();
        assert_eq!(server.recv().await.unwrap(), envelope);
    }
}
