//! `Channel`: correlates outbound requests with their responses and dispatches inbound
//! requests/notifications to a [`RequestHandler`]. Runtime-agnostic — the caller drives the
//! read loop by spawning [`Channel::run`] on whatever executor they use.

use std::collections::HashMap;
use std::future::Future;
use std::pin::Pin;
use std::sync::atomic::{AtomicI64, Ordering};
use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use futures::channel::oneshot;
use futures::stream::{FuturesUnordered, StreamExt};
use futures::FutureExt;

use crate::protocol::json_value::JsonValue;
use crate::protocol::jsonrpc::{
    error_codes, JsonRpcError, JsonRpcMessage, JsonRpcNotification, JsonRpcRequest,
    JsonRpcResponse, RequestId, ResponsePayload,
};
use crate::transport::message::MessageTransport;

/// Handles inbound requests and notifications arriving on a [`Channel`].
#[async_trait]
pub trait RequestHandler: Send + Sync {
    /// Handle an inbound request, producing a result or a JSON-RPC error.
    async fn handle_request(
        &self,
        method: String,
        params: JsonValue,
    ) -> Result<JsonValue, JsonRpcError>;

    /// Handle an inbound notification (no response is sent).
    async fn handle_notification(&self, _method: String, _params: JsonValue) {}
}

/// A no-op handler that rejects every request with `methodNotFound`.
pub struct RejectingHandler;

#[async_trait]
impl RequestHandler for RejectingHandler {
    async fn handle_request(
        &self,
        method: String,
        _params: JsonValue,
    ) -> Result<JsonValue, JsonRpcError> {
        Err(JsonRpcError::new(
            error_codes::METHOD_NOT_FOUND,
            format!("method not found: {method}"),
        ))
    }
}

type PendingMap = Mutex<HashMap<RequestId, oneshot::Sender<Result<JsonValue, JsonRpcError>>>>;
type DispatchFuture = Pin<Box<dyn Future<Output = ()> + Send>>;

struct ChannelInner {
    transport: Box<dyn MessageTransport>,
    handler: Box<dyn RequestHandler>,
    pending: PendingMap,
    next_id: AtomicI64,
}

/// A bidirectional JSON-RPC channel over a single transport.
#[derive(Clone)]
pub struct Channel {
    inner: Arc<ChannelInner>,
}

impl Channel {
    pub fn new(transport: Box<dyn MessageTransport>, handler: Box<dyn RequestHandler>) -> Self {
        Channel {
            inner: Arc::new(ChannelInner {
                transport,
                handler,
                pending: Mutex::new(HashMap::new()),
                next_id: AtomicI64::new(1),
            }),
        }
    }

    /// Issue a request and await its response.
    pub async fn call(&self, method: &str, params: JsonValue) -> Result<JsonValue, JsonRpcError> {
        let id = RequestId::Number(self.inner.next_id.fetch_add(1, Ordering::SeqCst));
        let (tx, rx) = oneshot::channel();
        self.inner.pending.lock().unwrap().insert(id.clone(), tx);

        let request = JsonRpcMessage::Request(JsonRpcRequest {
            id: id.clone(),
            method: method.to_string(),
            params: Some(params),
        });
        if self.inner.transport.send(request).await.is_err() {
            self.inner.pending.lock().unwrap().remove(&id);
            return Err(JsonRpcError::new(
                error_codes::INTERNAL_ERROR,
                "transport closed before request was sent",
            ));
        }

        match rx.await {
            Ok(result) => result,
            Err(_) => Err(JsonRpcError::new(
                error_codes::PEER_DISCONNECTED,
                "channel closed before response arrived",
            )),
        }
    }

    /// Fire a notification (no response expected).
    pub async fn notify(&self, method: &str, params: JsonValue) -> Result<(), JsonRpcError> {
        let note = JsonRpcMessage::Notification(JsonRpcNotification {
            method: method.to_string(),
            params: Some(params),
        });
        self.inner
            .transport
            .send(note)
            .await
            .map_err(|_| JsonRpcError::new(error_codes::INTERNAL_ERROR, "transport closed"))
    }

    /// Drive the inbound read loop until the transport closes. Spawn this on your executor.
    pub async fn run(&self) {
        let mut dispatches = FuturesUnordered::<DispatchFuture>::new();
        let mut receive = self.inner.transport.recv();
        loop {
            if dispatches.is_empty() {
                let Some(message) = receive.as_mut().await else {
                    break;
                };
                receive = self.inner.transport.recv();
                self.accept_message(message, &mut dispatches);
                continue;
            }

            futures::select! {
                message = receive.as_mut().fuse() => {
                    let Some(message) = message else {
                        break;
                    };
                    receive = self.inner.transport.recv();
                    self.accept_message(message, &mut dispatches);
                }
                _ = dispatches.next().fuse() => {}
            }
        }
        self.fail_all_pending();
    }

    fn accept_message(
        &self,
        message: JsonRpcMessage,
        dispatches: &mut FuturesUnordered<DispatchFuture>,
    ) {
        match message {
            JsonRpcMessage::Response(response) => self.resolve_response(response),
            JsonRpcMessage::Request(request) => {
                let channel = self.clone();
                dispatches.push(Box::pin(async move {
                    channel.dispatch_request(request).await;
                }));
            }
            JsonRpcMessage::Notification(notification) => {
                let channel = self.clone();
                dispatches.push(Box::pin(async move {
                    let params = notification.params.unwrap_or(JsonValue::Null);
                    channel
                        .inner
                        .handler
                        .handle_notification(notification.method, params)
                        .await;
                }));
            }
        }
    }

    fn resolve_response(&self, resp: JsonRpcResponse) {
        let Some(id) = resp.id else { return };
        let sender = self.inner.pending.lock().unwrap().remove(&id);
        if let Some(sender) = sender {
            let result = match resp.payload {
                ResponsePayload::Result(v) => Ok(v),
                ResponsePayload::Error(e) => Err(e),
            };
            let _ = sender.send(result);
        }
    }

    async fn dispatch_request(&self, req: JsonRpcRequest) {
        let params = req.params.unwrap_or(JsonValue::Null);
        let result = self.inner.handler.handle_request(req.method, params).await;
        let payload = match result {
            Ok(v) => ResponsePayload::Result(v),
            Err(e) => ResponsePayload::Error(e),
        };
        let response = JsonRpcMessage::Response(JsonRpcResponse {
            id: Some(req.id),
            payload,
        });
        let _ = self.inner.transport.send(response).await;
    }

    fn fail_all_pending(&self) {
        let mut pending = self.inner.pending.lock().unwrap();
        for (_, sender) in pending.drain() {
            let _ = sender.send(Err(JsonRpcError::new(
                error_codes::PEER_DISCONNECTED,
                "channel closed",
            )));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::transport::memory::transport_pair;
    use serde_json::json;
    use std::time::Duration;

    struct Adder;

    #[async_trait]
    impl RequestHandler for Adder {
        async fn handle_request(
            &self,
            method: String,
            params: JsonValue,
        ) -> Result<JsonValue, JsonRpcError> {
            if method != "add" {
                return Err(JsonRpcError::new(error_codes::METHOD_NOT_FOUND, method));
            }
            let a = params["a"].as_i64().unwrap_or(0);
            let b = params["b"].as_i64().unwrap_or(0);
            Ok(json!(a + b))
        }
    }

    #[tokio::test]
    async fn call_round_trips_over_pair() {
        let (a, b) = transport_pair();
        let client = Channel::new(Box::new(a), Box::new(RejectingHandler));
        let server = Channel::new(Box::new(b), Box::new(Adder));

        let client_loop = client.clone();
        let server_loop = server.clone();
        tokio::spawn(async move { client_loop.run().await });
        tokio::spawn(async move { server_loop.run().await });

        let result = client.call("add", json!({ "a": 2, "b": 3 })).await.unwrap();
        assert_eq!(result, json!(5));
    }

    #[tokio::test]
    async fn unknown_method_errors() {
        let (a, b) = transport_pair();
        let client = Channel::new(Box::new(a), Box::new(RejectingHandler));
        let server = Channel::new(Box::new(b), Box::new(Adder));

        let server_loop = server.clone();
        let client_loop = client.clone();
        tokio::spawn(async move { server_loop.run().await });
        tokio::spawn(async move { client_loop.run().await });

        let err = client.call("nope", JsonValue::Null).await.unwrap_err();
        assert_eq!(err.code, error_codes::METHOD_NOT_FOUND);
    }

    struct OutOfOrderHandler {
        slow_started: Mutex<Option<oneshot::Sender<()>>>,
        release_slow: Mutex<Option<oneshot::Receiver<()>>>,
    }

    #[async_trait]
    impl RequestHandler for OutOfOrderHandler {
        async fn handle_request(
            &self,
            method: String,
            _params: JsonValue,
        ) -> Result<JsonValue, JsonRpcError> {
            match method.as_str() {
                "slow" => {
                    if let Some(sender) = self.slow_started.lock().unwrap().take() {
                        let _ = sender.send(());
                    }
                    let receiver = self.release_slow.lock().unwrap().take().unwrap();
                    let _ = receiver.await;
                    Ok(json!("slow"))
                }
                "fast" => Ok(json!("fast")),
                _ => Err(JsonRpcError::new(error_codes::METHOD_NOT_FOUND, method)),
            }
        }
    }

    #[tokio::test]
    async fn later_request_can_respond_before_long_running_request() {
        let (client_transport, server_transport) = transport_pair();
        let client = Channel::new(Box::new(client_transport), Box::new(RejectingHandler));
        let (slow_started_tx, slow_started_rx) = oneshot::channel();
        let (release_slow_tx, release_slow_rx) = oneshot::channel();
        let server = Channel::new(
            Box::new(server_transport),
            Box::new(OutOfOrderHandler {
                slow_started: Mutex::new(Some(slow_started_tx)),
                release_slow: Mutex::new(Some(release_slow_rx)),
            }),
        );

        let client_loop = client.clone();
        let server_loop = server.clone();
        tokio::spawn(async move { client_loop.run().await });
        tokio::spawn(async move { server_loop.run().await });

        let slow_client = client.clone();
        let slow = tokio::spawn(async move { slow_client.call("slow", JsonValue::Null).await });
        slow_started_rx.await.unwrap();

        let fast =
            tokio::time::timeout(Duration::from_secs(1), client.call("fast", JsonValue::Null))
                .await
                .expect("fast request must not wait for the slow request")
                .unwrap();
        assert_eq!(fast, json!("fast"));

        let _ = release_slow_tx.send(());
        assert_eq!(slow.await.unwrap().unwrap(), json!("slow"));
    }
}
