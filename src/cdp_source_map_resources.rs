use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use async_trait::async_trait;
use linkrpc::prelude::{JsonRpcMessage, MessageTransport, TransportError};
use linkrpc::protocol::jsonrpc::{JsonRpcRequest, RequestId, ResponsePayload};
use serde_json::{Value, json};
use tokio::sync::{OwnedSemaphorePermit, Semaphore};

const OWNER_PARAM: &str = "__dbgjsSourceMapOwner";
const CLOSE_PREFIX: &str = "__dbgjsSourceMapClose/";
const MAX_OUTSTANDING_LOADS: usize = 4;
const CLOSE_SEND_TIMEOUT: Duration = Duration::from_secs(5);

// CDP cannot cancel a pending resource load. Retain bounded response ownership,
// not a waiting task, so even a response arriving after cancellation can be closed.
pub(super) struct SourceMapResources {
    transport: Arc<dyn MessageTransport>,
    next_id: AtomicU64,
    permits: Arc<Semaphore>,
    registered: Mutex<HashMap<u64, Arc<Resource>>>,
    pending: Mutex<HashMap<RequestId, Arc<Resource>>>,
}

struct Resource {
    id: u64,
    state: Mutex<ResourceState>,
    _permit: OwnedSemaphorePermit,
}

#[derive(Default)]
struct ResourceState {
    cancelled: bool,
    stream: Option<String>,
}

pub(super) struct ResourceGuard {
    resources: Arc<SourceMapResources>,
    resource: Arc<Resource>,
}

impl SourceMapResources {
    pub(super) fn new(transport: impl MessageTransport + 'static) -> Arc<Self> {
        Arc::new(Self {
            transport: Arc::new(transport),
            next_id: AtomicU64::new(1),
            permits: Arc::new(Semaphore::new(MAX_OUTSTANDING_LOADS)),
            registered: Mutex::new(HashMap::new()),
            pending: Mutex::new(HashMap::new()),
        })
    }

    pub(super) fn transport(self: &Arc<Self>) -> ResourceTransport {
        ResourceTransport(self.clone())
    }

    pub(super) fn register(
        self: &Arc<Self>,
        params: &mut Value,
    ) -> Result<ResourceGuard, &'static str> {
        let permit = self.permits.clone().try_acquire_owned().map_err(
            |_| "source-map resource limit reached; waiting for outstanding CDP resource responses",
        )?;
        let id = self.next_id.fetch_add(1, Ordering::Relaxed);
        let resource = Arc::new(Resource {
            id,
            state: Mutex::new(ResourceState::default()),
            _permit: permit,
        });
        // The transport removes this local token before sending the CDP request.
        params[OWNER_PARAM] = json!(id);
        self.registered.lock().unwrap().insert(id, resource.clone());
        Ok(ResourceGuard {
            resources: self.clone(),
            resource,
        })
    }

    fn close_abandoned_stream(&self, resource: Arc<Resource>, stream: String) {
        let transport = self.transport.clone();
        tokio::spawn(async move {
            let message = JsonRpcMessage::Request(JsonRpcRequest {
                id: RequestId::String(format!("{CLOSE_PREFIX}{}", resource.id)),
                method: "IO.close".to_owned(),
                params: Some(json!({ "handle": stream })),
            });
            if !matches!(
                tokio::time::timeout(CLOSE_SEND_TIMEOUT, transport.send(message)).await,
                Ok(Ok(()))
            ) {
                eprintln!("failed to send IO.close for abandoned source-map stream");
            }
            drop(resource);
        });
    }
}

impl ResourceGuard {
    pub(super) fn disarm(&self) {
        self.resource.state.lock().unwrap().stream = None;
    }
}

impl Drop for ResourceGuard {
    fn drop(&mut self) {
        self.resources
            .registered
            .lock()
            .unwrap()
            .remove(&self.resource.id);
        let stream = {
            let mut state = self.resource.state.lock().unwrap();
            state.cancelled = true;
            state.stream.take()
        };
        if let Some(stream) = stream {
            self.resources
                .close_abandoned_stream(self.resource.clone(), stream);
        }
    }
}

pub(super) struct ResourceTransport(Arc<SourceMapResources>);

#[async_trait]
impl MessageTransport for ResourceTransport {
    async fn send(&self, mut message: JsonRpcMessage) -> Result<(), TransportError> {
        let mut tracked_id = None;
        if let JsonRpcMessage::Request(request) = &mut message
            && request.method == "Network.loadNetworkResource"
            && let Some(params) = request.params.as_mut().and_then(Value::as_object_mut)
            && let Some(owner) = params.remove(OWNER_PARAM).and_then(|value| value.as_u64())
        {
            let Some(resource) = self.0.registered.lock().unwrap().remove(&owner) else {
                // Cancellation can remove ownership while this request is still queued.
                return Ok(());
            };
            tracked_id = Some(request.id.clone());
            self.0
                .pending
                .lock()
                .unwrap()
                .insert(request.id.clone(), resource);
        }
        let result = self.0.transport.send(message).await;
        if result.is_err()
            && let Some(id) = tracked_id
        {
            self.0.pending.lock().unwrap().remove(&id);
        }
        result
    }

    async fn recv(&self) -> Option<JsonRpcMessage> {
        loop {
            let Some(message) = self.0.transport.recv().await else {
                self.0.pending.lock().unwrap().clear();
                return None;
            };
            if let JsonRpcMessage::Response(response) = &message
                && let Some(id) = &response.id
            {
                if matches!(id, RequestId::String(id) if id.starts_with(CLOSE_PREFIX)) {
                    continue;
                }
                let resource = self.0.pending.lock().unwrap().remove(id);
                if let Some(resource) = resource
                    && let ResponsePayload::Result(result) = &response.payload
                    && let Some(stream) = result["resource"]["stream"].as_str()
                {
                    let abandoned = {
                        let mut state = resource.state.lock().unwrap();
                        if state.cancelled {
                            true
                        } else {
                            state.stream = Some(stream.to_owned());
                            false
                        }
                    };
                    if abandoned {
                        self.0.close_abandoned_stream(resource, stream.to_owned());
                    }
                }
            }
            return Some(message);
        }
    }
}
