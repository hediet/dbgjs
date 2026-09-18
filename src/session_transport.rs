use std::collections::HashMap;
use std::sync::{Arc, Mutex, Weak};

use async_trait::async_trait;
use linkrpc::prelude::{
    JsonRpcMessage, MessageTransport, MultiplexedTransport, MuxChannel, MuxCodec, MuxError,
    TransportError,
};
use serde::{Deserialize, Deserializer, Serialize, Serializer};
use serde_json::Value;

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
}

impl CdpSessionMux {
    pub fn new(raw: Arc<dyn MessageTransport<CdpEnvelope, CdpEnvelope>>) -> Self {
        Self {
            inner: Arc::new(MultiplexedTransport::with_codec(raw, CdpEnvelopeCodec)),
            channels: Arc::new(Mutex::new(HashMap::new())),
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

    pub async fn run(&self) {
        self.inner.run().await;
    }

    pub fn dispose(&self) {
        self.inner.dispose();
        self.channels.lock().unwrap().clear();
    }
}

pub struct CdpSessionTransport {
    id: String,
    inner: Arc<InnerChannel>,
    channels: Weak<Mutex<HashMap<String, Weak<InnerChannel>>>>,
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
