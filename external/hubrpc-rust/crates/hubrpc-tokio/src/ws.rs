//! WebSocket transport for hubrpc (the `ws://` / `wss://` endpoint half of M6).
//!
//! Each WebSocket message carries exactly one JSON-RPC frame (text). Unlike the NDJSON socket
//! transport there is **no `hello` preamble** — the auth token rides in the `Authorization: Bearer`
//! header at connect time (see [`connect_ws`]). This matches the TS `openHubChannel` WebSocket path.

use std::pin::Pin;

use async_trait::async_trait;
use futures_util::{Sink, SinkExt, Stream, StreamExt};
use tokio::sync::Mutex;
use tokio_tungstenite::tungstenite::http::header::AUTHORIZATION;
use tokio_tungstenite::tungstenite::{client::IntoClientRequest, Error as WsError, Message};

use hubrpc::prelude::{JsonRpcMessage, MessageTransport, TransportError};

type BoxSink = Pin<Box<dyn Sink<Message, Error = WsError> + Send>>;
type BoxStream = Pin<Box<dyn Stream<Item = Result<Message, WsError>> + Send>>;

/// A JSON-RPC transport over a WebSocket connection (one frame per message).
pub struct WebSocketTransport {
    sink: Mutex<BoxSink>,
    stream: Mutex<BoxStream>,
}

impl WebSocketTransport {
    fn from_split(sink: BoxSink, stream: BoxStream) -> Self {
        WebSocketTransport {
            sink: Mutex::new(sink),
            stream: Mutex::new(stream),
        }
    }

    /// Wrap an already-established WebSocket stream (split sink + stream).
    pub fn new<S>(ws: S) -> Self
    where
        S: Sink<Message, Error = WsError>
            + Stream<Item = Result<Message, WsError>>
            + Send
            + 'static,
    {
        let (sink, stream) = ws.split();
        WebSocketTransport::from_split(Box::pin(sink), Box::pin(stream))
    }
}

#[async_trait]
impl MessageTransport for WebSocketTransport {
    async fn send(&self, msg: JsonRpcMessage) -> Result<(), TransportError> {
        let text = serde_json::to_string(&msg).map_err(|e| TransportError::Other(e.to_string()))?;
        let mut sink = self.sink.lock().await;
        sink.send(Message::Text(text.into()))
            .await
            .map_err(map_ws_err)?;
        sink.flush().await.map_err(map_ws_err)
    }

    async fn recv(&self) -> Option<JsonRpcMessage> {
        let mut stream = self.stream.lock().await;
        loop {
            match stream.next().await {
                Some(Ok(Message::Text(text))) => match serde_json::from_str(&text) {
                    Ok(msg) => return Some(msg),
                    // A malformed frame is unrecoverable mid-stream; close the transport.
                    Err(_) => return None,
                },
                Some(Ok(Message::Binary(bytes))) => match serde_json::from_slice(&bytes) {
                    Ok(msg) => return Some(msg),
                    Err(_) => return None,
                },
                // Control frames are handled by tungstenite; keep waiting for the next data frame.
                Some(Ok(Message::Ping(_) | Message::Pong(_) | Message::Frame(_))) => continue,
                Some(Ok(Message::Close(_))) | None => return None,
                Some(Err(_)) => return None,
            }
        }
    }
}

fn map_ws_err(e: WsError) -> TransportError {
    match e {
        WsError::ConnectionClosed | WsError::AlreadyClosed => TransportError::Closed,
        other => TransportError::Other(other.to_string()),
    }
}

/// Dial a `ws://` / `wss://` hub. The `token` (when non-empty) is sent as
/// `Authorization: Bearer <token>`; there is no `hello` preamble.
pub async fn connect_ws(url: &str, token: &str) -> Result<WebSocketTransport, WsError> {
    let mut request = url.into_client_request()?;
    if !token.is_empty() {
        let value = format!("Bearer {token}").parse().map_err(|_| {
            WsError::Url(tokio_tungstenite::tungstenite::error::UrlError::NoHostName)
        })?;
        request.headers_mut().insert(AUTHORIZATION, value);
    }
    let (ws, _response) = tokio_tungstenite::connect_async(request).await?;
    Ok(WebSocketTransport::new(ws))
}
