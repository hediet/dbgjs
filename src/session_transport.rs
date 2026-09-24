use std::collections::{HashMap, VecDeque};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, Weak};

use async_trait::async_trait;
use linkrpc::connection::channel::{Channel, RejectingHandler, RequestHandler};
use linkrpc::prelude::{
    JsonRpcError, JsonRpcMessage, MessageTransport, MultiplexedTransport, MuxChannel, MuxCodec,
    MuxError, TransportError, error_codes,
};
use serde::{Deserialize, Deserializer, Serialize, Serializer};
use serde_json::Value;
use tokio::sync::{mpsc, watch};
use tokio::task::JoinHandle;

pub type SessionId = String;
pub type OpenSessionError = MuxError;

const ROOT_CHANNEL: &str = "$cdp-root";

#[derive(Clone, Debug, PartialEq)]
pub struct CdpEnvelope {
    pub session_id: Option<SessionId>,
    pub message: JsonRpcMessage,
}

impl Serialize for CdpEnvelope {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        let Value::Object(mut object) =
            serde_json::to_value(&self.message).map_err(serde::ser::Error::custom)?
        else {
            return Err(serde::ser::Error::custom(
                "JSON-RPC message did not serialize to an object",
            ));
        };

        object.remove("jsonrpc");
        if let Some(session_id) = &self.session_id {
            object.insert("sessionId".into(), Value::String(session_id.clone()));
        }
        object.serialize(serializer)
    }
}

impl<'de> Deserialize<'de> for CdpEnvelope {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let Value::Object(mut object) = Value::deserialize(deserializer)? else {
            return Err(serde::de::Error::custom("CDP message must be an object"));
        };
        let session_id = match object.remove("sessionId") {
            Some(Value::String(value)) => Some(value),
            Some(_) => {
                return Err(serde::de::Error::custom("CDP sessionId must be a string"));
            }
            None => None,
        };
        object.insert("jsonrpc".into(), Value::String("2.0".into()));
        let message =
            serde_json::from_value(Value::Object(object)).map_err(serde::de::Error::custom)?;
        Ok(Self {
            session_id,
            message,
        })
    }
}

#[derive(Clone, Copy, Debug, Default)]
pub struct CdpEnvelopeCodec;

impl MuxCodec<CdpEnvelope> for CdpEnvelopeCodec {
    fn encode(&self, channel_id: &str, message: JsonRpcMessage) -> CdpEnvelope {
        CdpEnvelope {
            session_id: (channel_id != ROOT_CHANNEL).then(|| channel_id.to_owned()),
            message,
        }
    }

    fn decode(&self, envelope: CdpEnvelope) -> Option<(String, JsonRpcMessage)> {
        Some((
            envelope
                .session_id
                .unwrap_or_else(|| ROOT_CHANNEL.to_owned()),
            envelope.message,
        ))
    }
}

type InnerMux = MultiplexedTransport<CdpEnvelope, CdpEnvelopeCodec>;
type InnerChannel = MuxChannel<CdpEnvelope, CdpEnvelopeCodec>;

#[derive(Clone)]
pub struct CdpSessionMux {
    inner: Arc<InnerMux>,
    channels: Arc<Mutex<HashMap<String, Weak<InnerChannel>>>>,
    raw_routes: Arc<Mutex<HashMap<String, Arc<RawSessionRoute>>>>,
    disposed: Arc<AtomicBool>,
}

impl CdpSessionMux {
    pub fn new(raw: Arc<dyn MessageTransport<CdpEnvelope, CdpEnvelope>>) -> Self {
        Self {
            inner: Arc::new(MultiplexedTransport::with_codec(raw, CdpEnvelopeCodec)),
            channels: Arc::new(Mutex::new(HashMap::new())),
            raw_routes: Arc::new(Mutex::new(HashMap::new())),
            disposed: Arc::new(AtomicBool::new(false)),
        }
    }

    pub fn open_root(&self) -> Result<CdpSessionTransport, OpenSessionError> {
        self.open(ROOT_CHANNEL)
    }

    pub fn open_session(
        &self,
        session_id: SessionId,
    ) -> Result<CdpSessionTransport, OpenSessionError> {
        self.open(&session_id)
    }

    fn open(&self, id: &str) -> Result<CdpSessionTransport, OpenSessionError> {
        let channel = self.inner.add_channel(id)?;
        self.channels
            .lock()
            .unwrap()
            .insert(id.to_owned(), Arc::downgrade(&channel));
        Ok(CdpSessionTransport {
            id: id.to_owned(),
            inner: channel,
            channels: Arc::downgrade(&self.channels),
        })
    }

    pub fn retire_session(&self, session_id: &str) {
        // Native CDP may issue this same ID on a later attachment. Keep its raw channel alive
        // (and its request counter monotonic) so the mux never routes a late reply to a new call.
        if self.raw_routes.lock().unwrap().contains_key(session_id) {
            return;
        }
        if let Some(channel) = self
            .channels
            .lock()
            .unwrap()
            .remove(session_id)
            .and_then(|channel| channel.upgrade())
        {
            channel.dispose();
        }
    }

    fn open_raw_session(&self, id: String) -> Result<Arc<RawCdpSession>, OpenSessionError> {
        let mut routes = self.raw_routes.lock().unwrap();
        let route = self.ensure_raw_route(&mut routes, &id)?;
        if route.active.swap(true, Ordering::SeqCst) {
            return Err(MuxError::AlreadyUsed(id));
        }
        Ok(Arc::new(RawCdpSession {
            route,
            closed: watch::channel(false).0,
            closed_flag: AtomicBool::new(false),
        }))
    }

    /// Shares an already-routed native child channel with raw CDP callers.
    pub fn raw_channel(&self, id: &str) -> Option<Channel> {
        self.raw_routes
            .lock()
            .unwrap()
            .get(id)
            .map(|route| route.channel.clone())
    }

    pub fn ensure_raw_channel(&self, id: &str) -> Result<Channel, OpenSessionError> {
        let mut routes = self.raw_routes.lock().unwrap();
        Ok(self.ensure_raw_route(&mut routes, id)?.channel.clone())
    }

    pub fn forward_raw_notifications(&self, id: &str, handler: Arc<dyn RequestHandler>) {
        if let Some(route) = self.raw_routes.lock().unwrap().get(id) {
            let _ = route
                .notifications
                .send(RawNotification::Listener(Some(handler)));
        }
    }

    pub fn pause_raw_notifications(&self, id: &str) {
        if let Some(route) = self.raw_routes.lock().unwrap().get(id) {
            let _ = route.notifications.send(RawNotification::Listener(None));
        }
    }

    fn ensure_raw_route(
        &self,
        routes: &mut HashMap<String, Arc<RawSessionRoute>>,
        id: &str,
    ) -> Result<Arc<RawSessionRoute>, OpenSessionError> {
        if self.disposed.load(Ordering::SeqCst) {
            return Err(MuxError::Disposed);
        }
        if let Some(route) = routes.get(id) {
            return Ok(route.clone());
        }
        let (notifications, mut receiver) = mpsc::unbounded_channel();
        let channel = Channel::new(
            Box::new(self.open_session(id.to_owned())?),
            Box::new(RawNotificationHandler {
                notifications: notifications.clone(),
            }),
        );
        let task = tokio::spawn({
            let channel = channel.clone();
            async move { channel.run().await }
        });
        let notification_task = tokio::spawn(async move {
            let mut listener: Option<Arc<dyn RequestHandler>> = None;
            let mut pending = VecDeque::new();
            let mut detached = false;
            while let Some(notification) = receiver.recv().await {
                match notification {
                    RawNotification::Listener(next) => {
                        listener = next;
                        if let Some(handler) = &listener {
                            detached = false;
                            while let Some((method, params)) = pending.pop_front() {
                                handler.handle_notification(method, params).await;
                            }
                        } else {
                            pending.clear();
                            detached = true;
                        }
                    }
                    RawNotification::Event(method, params) => {
                        if let Some(handler) = &listener {
                            handler.handle_notification(method, params).await;
                        } else if !detached {
                            // Buffer only the initial attach handoff, not stale events from
                            // a detached session whose native ID may later be reused.
                            if pending.len() == 256 {
                                pending.pop_front();
                            }
                            pending.push_back((method, params));
                        }
                    }
                }
            }
        });
        let route = Arc::new(RawSessionRoute {
            channel,
            task,
            notification_task,
            notifications,
            active: AtomicBool::new(false),
        });
        routes.insert(id.to_owned(), route.clone());
        Ok(route)
    }

    pub async fn run(&self) {
        self.inner.run().await;
    }

    pub fn dispose(&self) {
        self.disposed.store(true, Ordering::SeqCst);
        self.inner.dispose();
        self.raw_routes.lock().unwrap().clear();
        self.channels.lock().unwrap().clear();
    }
}

pub struct CdpSessionTransport {
    id: String,
    inner: Arc<InnerChannel>,
    channels: Weak<Mutex<HashMap<String, Weak<InnerChannel>>>>,
}

/// A live native child session on exactly one mux. Requests share its channel until detach or
/// owner loss; the session ID alone is never a cross-endpoint routing key.
struct RawSessionRoute {
    channel: Channel,
    task: JoinHandle<()>,
    notification_task: JoinHandle<()>,
    notifications: mpsc::UnboundedSender<RawNotification>,
    active: AtomicBool,
}

impl Drop for RawSessionRoute {
    fn drop(&mut self) {
        self.task.abort();
        self.notification_task.abort();
    }
}

enum RawNotification {
    Listener(Option<Arc<dyn RequestHandler>>),
    Event(String, Value),
}

struct RawNotificationHandler {
    notifications: mpsc::UnboundedSender<RawNotification>,
}

#[async_trait]
impl RequestHandler for RawNotificationHandler {
    async fn handle_request(&self, method: String, params: Value) -> Result<Value, JsonRpcError> {
        RejectingHandler.handle_request(method, params).await
    }

    async fn handle_notification(&self, method: String, params: Value) {
        let _ = self
            .notifications
            .send(RawNotification::Event(method, params));
    }
}

pub struct RawCdpSession {
    route: Arc<RawSessionRoute>,
    closed: watch::Sender<bool>,
    closed_flag: AtomicBool,
}

impl RawCdpSession {
    pub fn open(mux: &CdpSessionMux, id: String) -> Result<Arc<Self>, OpenSessionError> {
        mux.open_raw_session(id)
    }

    pub async fn request(
        &self,
        method: &str,
        params: Value,
        budget: std::time::Duration,
    ) -> Result<Value, JsonRpcError> {
        let mut closed = self.closed.subscribe();
        if *closed.borrow() {
            return Err(JsonRpcError::new(
                error_codes::PEER_DISCONNECTED,
                "raw CDP session detached; attach again",
            ));
        }
        tokio::select! {
            biased;
            _ = closed.changed() => Err(JsonRpcError::new(error_codes::PEER_DISCONNECTED, "raw CDP session detached; attach again")),
            result = self.route.channel.call(method, params) => result,
            _ = tokio::time::sleep(budget) => Err(JsonRpcError::new(
                error_codes::REQUEST_TIMEOUT,
                format!("raw CDP session request timed out after {} seconds", budget.as_secs()),
            )),
        }
    }

    pub fn close(&self) {
        if !self.closed_flag.swap(true, Ordering::SeqCst) {
            self.closed.send_replace(true);
            self.route.active.store(false, Ordering::SeqCst);
        }
    }
}

impl Drop for RawCdpSession {
    fn drop(&mut self) {
        self.close();
    }
}

#[async_trait]
impl MessageTransport for CdpSessionTransport {
    async fn send(&self, message: JsonRpcMessage) -> Result<(), TransportError> {
        self.inner.send(message).await
    }

    async fn recv(&self) -> Option<JsonRpcMessage> {
        self.inner.recv().await
    }
}

impl Drop for CdpSessionTransport {
    fn drop(&mut self) {
        self.inner.dispose();
        if let Some(channels) = self.channels.upgrade() {
            channels.lock().unwrap().remove(&self.id);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use linkrpc::connection::channel::{Channel, RejectingHandler, RequestHandler};
    use linkrpc::protocol::jsonrpc::{JsonRpcResponse, RequestId, ResponsePayload};
    use linkrpc::transport::memory::transport_pair_of;
    use serde_json::json;

    #[derive(Default)]
    struct EventCollector {
        events: Mutex<Vec<(String, Value)>>,
    }

    #[async_trait]
    impl RequestHandler for EventCollector {
        async fn handle_request(
            &self,
            method: String,
            _params: Value,
        ) -> Result<Value, linkrpc::prelude::JsonRpcError> {
            panic!("unexpected request: {method}");
        }

        async fn handle_notification(&self, method: String, params: Value) {
            self.events.lock().unwrap().push((method, params));
        }
    }

    #[test]
    fn envelope_is_plain_cdp_json() {
        let envelope = CdpEnvelope {
            session_id: Some("session-a".into()),
            message: JsonRpcMessage::Request(linkrpc::prelude::JsonRpcRequest {
                id: RequestId::Number(7),
                method: "Debugger.enable".into(),
                params: Some(json!({ "maxScriptsCacheSize": 1024 })),
            }),
        };

        let value = serde_json::to_value(&envelope).unwrap();
        assert_eq!(value["sessionId"], "session-a");
        assert_eq!(value["method"], "Debugger.enable");
        assert!(value.get("jsonrpc").is_none());
        assert_eq!(
            serde_json::from_value::<CdpEnvelope>(value).unwrap(),
            envelope
        );
    }

    #[tokio::test]
    async fn root_and_child_channels_have_independent_request_ids() {
        let (client_raw, browser_raw) = transport_pair_of::<CdpEnvelope>();
        let mux = CdpSessionMux::new(Arc::new(client_raw));
        let root = Channel::new(
            Box::new(mux.open_root().unwrap()),
            Box::new(RejectingHandler),
        );
        let child = Channel::new(
            Box::new(mux.open_session("child-a".into()).unwrap()),
            Box::new(RejectingHandler),
        );

        let mux_loop = mux.clone();
        tokio::spawn(async move { mux_loop.run().await });
        let root_loop = root.clone();
        tokio::spawn(async move { root_loop.run().await });
        let child_loop = child.clone();
        tokio::spawn(async move { child_loop.run().await });

        let browser = tokio::spawn(async move {
            let first = browser_raw.recv().await.unwrap();
            let second = browser_raw.recv().await.unwrap();
            let mut requests = [first, second];
            requests.sort_by_key(|request| request.session_id.clone());

            assert_eq!(requests[0].session_id, None);
            assert_eq!(requests[1].session_id.as_deref(), Some("child-a"));
            for request in requests {
                let JsonRpcMessage::Request(request_message) = request.message else {
                    panic!("expected request");
                };
                assert_eq!(request_message.id, RequestId::Number(1));
                browser_raw
                    .send(CdpEnvelope {
                        session_id: request.session_id,
                        message: JsonRpcMessage::Response(JsonRpcResponse {
                            id: Some(request_message.id),
                            payload: ResponsePayload::Result(json!({
                                "method": request_message.method
                            })),
                        }),
                    })
                    .await
                    .unwrap();
            }
        });

        let (root_result, child_result) = tokio::join!(
            root.call("Browser.getVersion", json!({})),
            child.call("Runtime.enable", json!({}))
        );
        assert_eq!(root_result.unwrap()["method"], "Browser.getVersion");
        assert_eq!(child_result.unwrap()["method"], "Runtime.enable");
        browser.await.unwrap();
    }

    #[tokio::test]
    async fn events_are_routed_and_retired_ids_cannot_reopen() {
        let (client_raw, browser_raw) = transport_pair_of::<CdpEnvelope>();
        let mux = CdpSessionMux::new(Arc::new(client_raw));
        let collector = Arc::new(EventCollector::default());
        let child = Channel::new(
            Box::new(mux.open_session("child-a".into()).unwrap()),
            Box::new(SharedHandler(collector.clone())),
        );

        let mux_loop = mux.clone();
        tokio::spawn(async move { mux_loop.run().await });
        let child_loop = child.clone();
        tokio::spawn(async move { child_loop.run().await });

        browser_raw
            .send(CdpEnvelope {
                session_id: Some("child-a".into()),
                message: JsonRpcMessage::Notification(linkrpc::prelude::JsonRpcNotification {
                    method: "Debugger.scriptParsed".into(),
                    params: Some(json!({ "scriptId": "1" })),
                }),
            })
            .await
            .unwrap();

        tokio::task::yield_now().await;
        assert_eq!(
            collector.events.lock().unwrap().as_slice(),
            &[("Debugger.scriptParsed".into(), json!({ "scriptId": "1" }))]
        );

        mux.retire_session("child-a");
        assert!(matches!(
            mux.open_session("child-a".into()),
            Err(MuxError::AlreadyUsed(id)) if id == "child-a"
        ));
    }

    #[tokio::test]
    async fn raw_native_id_reuse_does_not_deliver_late_reply_to_new_attachment() {
        let (client_raw, browser_raw) = transport_pair_of::<CdpEnvelope>();
        let mux = CdpSessionMux::new(Arc::new(client_raw));
        let loop_mux = mux.clone();
        tokio::spawn(async move { loop_mux.run().await });

        let first = RawCdpSession::open(&mux, "native".into()).unwrap();
        let pending = tokio::spawn({
            let first = first.clone();
            async move {
                first
                    .request(
                        "Runtime.evaluate",
                        json!({}),
                        std::time::Duration::from_secs(2),
                    )
                    .await
            }
        });
        let old = browser_raw.recv().await.unwrap();
        let JsonRpcMessage::Request(old) = old.message else {
            panic!("expected first call")
        };
        mux.retire_session("native");
        first.close();
        assert_eq!(
            pending.await.unwrap().unwrap_err().code,
            error_codes::PEER_DISCONNECTED
        );

        let second = RawCdpSession::open(&mux, "native".into()).unwrap();
        let next = tokio::spawn({
            let second = second.clone();
            async move {
                second
                    .request(
                        "Runtime.enable",
                        json!({}),
                        std::time::Duration::from_secs(2),
                    )
                    .await
            }
        });
        let new = browser_raw.recv().await.unwrap();
        let JsonRpcMessage::Request(new) = new.message else {
            panic!("expected second call")
        };
        assert_ne!(old.id, new.id);
        browser_raw
            .send(CdpEnvelope {
                session_id: Some("native".into()),
                message: JsonRpcMessage::Response(JsonRpcResponse {
                    id: Some(old.id),
                    payload: ResponsePayload::Result(json!({"stale": true})),
                }),
            })
            .await
            .unwrap();
        browser_raw
            .send(CdpEnvelope {
                session_id: Some("native".into()),
                message: JsonRpcMessage::Response(JsonRpcResponse {
                    id: Some(new.id),
                    payload: ResponsePayload::Result(json!({"ok": true})),
                }),
            })
            .await
            .unwrap();
        assert_eq!(next.await.unwrap().unwrap(), json!({"ok": true}));
        assert_eq!(
            first
                .request(
                    "Runtime.enable",
                    json!({}),
                    std::time::Duration::from_millis(20)
                )
                .await
                .unwrap_err()
                .code,
            error_codes::PEER_DISCONNECTED
        );
        second.close();
        mux.dispose();
    }

    #[test]
    fn dropping_a_transport_retires_its_route() {
        let (client_raw, _browser_raw) = transport_pair_of::<CdpEnvelope>();
        let mux = CdpSessionMux::new(Arc::new(client_raw));

        drop(mux.open_session("child-a".into()).unwrap());
        assert!(mux.channels.lock().unwrap().is_empty());
        assert!(matches!(
            mux.open_session("child-a".into()),
            Err(MuxError::AlreadyUsed(id)) if id == "child-a"
        ));

        drop(mux.open_root().unwrap());
        assert!(mux.channels.lock().unwrap().is_empty());
        assert!(matches!(
            mux.open_root(),
            Err(MuxError::AlreadyUsed(id)) if id == ROOT_CHANNEL
        ));
    }

    struct SharedHandler(Arc<EventCollector>);

    #[async_trait]
    impl RequestHandler for SharedHandler {
        async fn handle_request(
            &self,
            method: String,
            params: Value,
        ) -> Result<Value, linkrpc::prelude::JsonRpcError> {
            self.0.handle_request(method, params).await
        }

        async fn handle_notification(&self, method: String, params: Value) {
            self.0.handle_notification(method, params).await;
        }
    }
}
