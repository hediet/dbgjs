use super::*;
use async_trait::async_trait;
use hubrpc::prelude::{JsonRpcMessage, MessageTransport, TransportError};
use hubrpc::protocol::jsonrpc::{JsonRpcResponse, ResponsePayload};
use serde_json::json;

use crate::cdp_runtime::CdpConnection;
use crate::cdp_transport::ManagedCdpTransport;
use crate::debugger_engine::ScriptState;
use crate::session_transport::CdpEnvelope;

const VALID_MAP: &str = r#"{"version":3,"sources":["original.ts"],"sourcesContent":["class Original {}"],"names":[],"mappings":"AAAA"}"#;

struct RetryTransport {
    invalid_first_map: bool,
    map_requests: AtomicU64,
    source_requests: AtomicU64,
    inbound: tokio::sync::Mutex<mpsc::UnboundedReceiver<CdpEnvelope>>,
    outbound: mpsc::UnboundedSender<CdpEnvelope>,
}

#[async_trait]
impl MessageTransport<CdpEnvelope, CdpEnvelope> for RetryTransport {
    async fn send(&self, envelope: CdpEnvelope) -> Result<(), TransportError> {
        let JsonRpcMessage::Request(request) = envelope.message else {
            return Ok(());
        };
        let result = match request.method.as_str() {
            "Debugger.getScriptSource" => {
                self.source_requests.fetch_add(1, Ordering::SeqCst);
                json!({"scriptSource": "class a{}"})
            }
            "Network.loadNetworkResource" => {
                assert_eq!(request.params.as_ref().unwrap()["frameId"], "child-frame");
                let attempt = self.map_requests.fetch_add(1, Ordering::SeqCst);
                if attempt == 0 && !self.invalid_first_map {
                    json!({"resource":{"success":false,"httpStatusCode":503}})
                } else {
                    json!({"resource":{"success":true,"stream":"map-stream"}})
                }
            }
            "IO.read" => {
                let invalid =
                    self.invalid_first_map && self.map_requests.load(Ordering::SeqCst) == 1;
                json!({"data": if invalid { "{invalid-map" } else { VALID_MAP }, "eof":true})
            }
            "IO.close" => json!({}),
            method => panic!("unexpected CDP request: {method}"),
        };
        self.outbound
            .send(CdpEnvelope {
                session_id: envelope.session_id,
                message: JsonRpcMessage::Response(JsonRpcResponse {
                    id: Some(request.id),
                    payload: ResponsePayload::Result(result),
                }),
            })
            .map_err(|_| TransportError::Closed)
    }

    async fn recv(&self) -> Option<CdpEnvelope> {
        self.inbound.lock().await.recv().await
    }
}

#[async_trait]
impl ManagedCdpTransport for RetryTransport {
    fn close_reason(&self) -> Arc<tokio::sync::Mutex<Option<String>>> {
        Arc::new(tokio::sync::Mutex::new(None))
    }

    async fn wait_closed(&self) -> String {
        std::future::pending().await
    }

    async fn close(&self) {}
}

#[tokio::test]
async fn heap_new_capture_retries_transient_map_failure_without_changing_old_capture() {
    verify_map_retry(false).await;
}

#[tokio::test]
async fn heap_new_capture_retries_invalid_map_without_changing_old_capture() {
    verify_map_retry(true).await;
}

async fn verify_map_retry(invalid_first_map: bool) {
    let (outbound, inbound) = mpsc::unbounded_channel();
    let transport = Arc::new(RetryTransport {
        invalid_first_map,
        map_requests: AtomicU64::new(0),
        source_requests: AtomicU64::new(0),
        inbound: tokio::sync::Mutex::new(inbound),
        outbound,
    });
    let session = SessionKey {
        connection_generation: 1,
        session_id: "heap-retry".into(),
    };
    let connection = CdpConnection::connect_root_debugger_transport(
        transport.clone(),
        1,
        session.session_id.clone(),
    )
    .await
    .unwrap();
    let mut state =
        (*crate::debugger_engine::reduce(&Arc::new(DebuggerState::default()), Input::Connected)
            .state)
            .clone();
    let script = ScriptKey {
        session: session.clone(),
        script_id: "7".into(),
    };
    Arc::make_mut(&mut state.sessions).insert(
        session.clone(),
        Arc::new(crate::debugger_engine::SessionState {
            target_id: "target".into(),
            parent: None,
            waiting_for_debugger: false,
            phase: SessionPhase::Running,
            next_pause_epoch: 1,
            pause: None,
        }),
    );
    Arc::make_mut(&mut state.scripts).insert(
        script.clone(),
        Arc::new(ScriptState {
            url: "vscode-webview://heap-retry/app.js".into(),
            hash: format!(
                "heap-retry-{}-{invalid_first_map}",
                std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .unwrap()
                    .as_nanos()
            ),
            source_map_url: Some("app.js.map".into()),
            version: 1,
            source: ScriptSourceState::Unresolved,
            captured_source: None,
            provenance: ScriptProvenance {
                frame_id: Some("child-frame".into()),
                ..Default::default()
            },
        }),
    );
    let sources = SourceEffectInterpreter::new(
        SourceEffectOptions::default(),
        Arc::new(ContextSourceModel::new()),
        "heap-retry",
    );
    let mut driver = DebuggerDriver::new(
        Arc::new(state),
        connection.take_root_debugger_session().unwrap(),
        sources,
    );
    let first = capture_heap_mapping(&mut driver, &session, 1).await;
    assert_eq!(
        first.scripts[0].mapping_status,
        HeapMappingStatus::MapLoadingFailed
    );
    assert!(matches!(
        driver.state().scripts[&script].source,
        ScriptSourceState::Resolved(_)
    ));
    let stored_first = serde_json::to_vec(&first).unwrap();
    let second = capture_heap_mapping(&mut driver, &session, 1).await;
    assert_eq!(second.scripts[0].mapping_status, HeapMappingStatus::Mapped);
    assert_eq!(driver.source_map_cache_stats().bypasses, 1);
    assert_eq!(transport.map_requests.load(Ordering::SeqCst), 2);
    assert_eq!(transport.source_requests.load(Ordering::SeqCst), 2);
    assert_eq!(first.scripts[0].hash, second.scripts[0].hash);
    assert_eq!(serde_json::to_vec(&first).unwrap(), stored_first);
    let groups = vec![HeapConstructorGroup {
        script_id: 7,
        line: 0,
        column: 0,
        generated_name: "a".into(),
        instance_count: 1,
        shallow_size: 16,
        instances: Vec::new(),
    }];
    let first_reloaded: HeapMappingSnapshot = serde_json::from_slice(&stored_first).unwrap();
    let old = project_heap_classes("first".into(), &groups, None, Some(&first_reloaded)).unwrap();
    let new = project_heap_classes("second".into(), &groups, None, Some(&second)).unwrap();
    assert_eq!(old.classes[0].name, "a");
    assert_eq!(
        old.analysis.mapping_status,
        HeapMappingStatus::MapLoadingFailed
    );
    assert_eq!(new.classes[0].name, "Original");
    capture_heap_mapping(&mut driver, &session, 1).await;
    assert_eq!(transport.map_requests.load(Ordering::SeqCst), 2);
    connection.close().await;
}
