use std::sync::Arc;
#[cfg(test)]
use std::sync::atomic::{AtomicUsize, Ordering};

use async_trait::async_trait;
use futures_util::stream::{SplitSink, SplitStream};
use futures_util::{SinkExt, StreamExt};
use hubrpc::prelude::{MessageTransport, TransportError};
use tokio::net::TcpStream;
use tokio::sync::{Mutex, watch};
use tokio_tungstenite::tungstenite::Message;
use tokio_tungstenite::tungstenite::protocol::WebSocketConfig;
use tokio_tungstenite::{MaybeTlsStream, WebSocketStream, connect_async_with_config};

use crate::cdp_transport::ManagedCdpTransport;
use crate::session_transport::CdpEnvelope;

type Socket = WebSocketStream<MaybeTlsStream<TcpStream>>;
const MAX_CDP_MESSAGE_SIZE: usize = 128 * 1024 * 1024;

pub struct CdpWebSocketTransport {
    sender: Mutex<SplitSink<Socket, Message>>,
    receiver: Mutex<SplitStream<Socket>>,
    close_reason: Arc<Mutex<Option<String>>>,
    closed: watch::Sender<bool>,
    #[cfg(test)]
    largest_received_message_size: AtomicUsize,
}

impl CdpWebSocketTransport {
    pub async fn connect(endpoint: &str) -> Result<Self, CdpWebSocketError> {
        let mut config = WebSocketConfig::default();
        config.max_message_size = Some(MAX_CDP_MESSAGE_SIZE);
        config.max_frame_size = Some(MAX_CDP_MESSAGE_SIZE);
        let (socket, _) = connect_async_with_config(endpoint, Some(config), false)
            .await
            .map_err(CdpWebSocketError::Connect)?;
        let (sender, receiver) = socket.split();
        Ok(Self {
            sender: Mutex::new(sender),
            receiver: Mutex::new(receiver),
            close_reason: Arc::new(Mutex::new(None)),
            closed: watch::channel(false).0,
            #[cfg(test)]
            largest_received_message_size: AtomicUsize::new(0),
        })
    }

    #[cfg(test)]
    pub(crate) fn reset_largest_received_message_size(&self) {
        self.largest_received_message_size
            .store(0, Ordering::Relaxed);
    }

    #[cfg(test)]
    pub(crate) fn largest_received_message_size(&self) -> usize {
        self.largest_received_message_size.load(Ordering::Relaxed)
    }

    pub fn close_reason(&self) -> Arc<Mutex<Option<String>>> {
        self.close_reason.clone()
    }

    pub async fn wait_closed(&self) -> String {
        let mut closed = self.closed.subscribe();
        if !*closed.borrow() {
            let _ = closed.changed().await;
        }
        self.close_reason
            .lock()
            .await
            .clone()
            .unwrap_or_else(|| "WebSocket closed".into())
    }

    pub async fn close(&self) {
        self.close_with("WebSocket closed by debugger service".into())
            .await;
        if let Err(error) = self.sender.lock().await.close().await {
            self.close_with(format!("WebSocket close handshake failed: {error}"))
                .await;
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
impl MessageTransport<CdpEnvelope, CdpEnvelope> for CdpWebSocketTransport {
    async fn send(&self, envelope: CdpEnvelope) -> Result<(), TransportError> {
        if *self.closed.borrow() {
            return Err(TransportError::Closed);
        }

        let json = serde_json::to_string(&envelope)
            .map_err(|error| TransportError::Other(error.to_string()))?;
        if let Err(error) = self
            .sender
            .lock()
            .await
            .send(Message::Text(json.into()))
            .await
        {
            self.close_with(format!("WebSocket send failed: {error}"))
                .await;
            return Err(TransportError::Other(error.to_string()));
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
                self.close_with("WebSocket stream ended".into()).await;
                return None;
            };
            let message = match message {
                Ok(message) => message,
                Err(error) => {
                    self.close_with(format!("WebSocket receive failed: {error}"))
                        .await;
                    return None;
                }
            };
            let bytes = match message {
                Message::Text(text) => text.as_bytes().to_vec(),
                Message::Binary(bytes) => bytes.to_vec(),
                Message::Close(frame) => {
                    self.close_with(format!("WebSocket closed: {frame:?}"))
                        .await;
                    return None;
                }
                Message::Ping(_) | Message::Pong(_) | Message::Frame(_) => continue,
            };
            #[cfg(test)]
            self.largest_received_message_size
                .fetch_max(bytes.len(), Ordering::Relaxed);
            match serde_json::from_slice(&bytes) {
                Ok(envelope) => return Some(envelope),
                Err(error) => {
                    self.close_with(format!("invalid CDP JSON: {error}")).await;
                    return None;
                }
            }
        }
    }
}

#[async_trait]
impl ManagedCdpTransport for CdpWebSocketTransport {
    fn close_reason(&self) -> Arc<Mutex<Option<String>>> {
        CdpWebSocketTransport::close_reason(self)
    }

    async fn wait_closed(&self) -> String {
        CdpWebSocketTransport::wait_closed(self).await
    }

    async fn close(&self) {
        CdpWebSocketTransport::close(self).await;
    }
}

#[derive(Debug, thiserror::Error)]
pub enum CdpWebSocketError {
    #[error("failed to connect to CDP WebSocket: {0}")]
    Connect(tokio_tungstenite::tungstenite::Error),
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::net::TcpListener;
    use tokio::time::{Duration, timeout};
    use tokio_tungstenite::accept_async;

    #[tokio::test]
    async fn explicit_close_wakes_receiver_and_closes_socket() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let server = tokio::spawn(async move {
            let (stream, _) = listener.accept().await.unwrap();
            let mut socket = accept_async(stream).await.unwrap();
            timeout(Duration::from_secs(2), socket.next())
                .await
                .expect("client should close the socket")
        });

        let transport = Arc::new(
            CdpWebSocketTransport::connect(&format!("ws://{address}"))
                .await
                .unwrap(),
        );
        let receiver = {
            let transport = transport.clone();
            tokio::spawn(async move { transport.recv().await })
        };
        let closed = {
            let transport = transport.clone();
            tokio::spawn(async move { transport.wait_closed().await })
        };
        transport.close().await;

        assert!(
            timeout(Duration::from_secs(2), receiver)
                .await
                .expect("receiver should wake")
                .unwrap()
                .is_none()
        );
        assert_eq!(
            timeout(Duration::from_secs(2), closed)
                .await
                .expect("close waiter should wake")
                .unwrap(),
            "WebSocket closed by debugger service"
        );
        let server_message = server.await.unwrap();
        assert!(matches!(server_message, Some(Ok(Message::Close(_))) | None));
        assert_eq!(
            transport.close_reason().lock().await.as_deref(),
            Some("WebSocket closed by debugger service")
        );
    }

    #[tokio::test]
    async fn peer_disconnect_wakes_close_waiter() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let server = tokio::spawn(async move {
            let (stream, _) = listener.accept().await.unwrap();
            drop(accept_async(stream).await.unwrap());
        });

        let transport = Arc::new(
            CdpWebSocketTransport::connect(&format!("ws://{address}"))
                .await
                .unwrap(),
        );
        let receiver = {
            let transport = transport.clone();
            tokio::spawn(async move { transport.recv().await })
        };
        let closed = {
            let transport = transport.clone();
            tokio::spawn(async move { transport.wait_closed().await })
        };

        server.await.unwrap();
        assert!(
            timeout(Duration::from_secs(2), receiver)
                .await
                .expect("receiver should observe peer disconnect")
                .unwrap()
                .is_none()
        );
        let reason = timeout(Duration::from_secs(2), closed)
            .await
            .expect("close waiter should observe peer disconnect")
            .unwrap();
        assert!(
            reason.starts_with("WebSocket receive failed:")
                || reason == "WebSocket stream ended"
                || reason.starts_with("WebSocket closed:")
        );
    }
}
