use super::*;
use async_trait::async_trait;
use base64::Engine;
use hubrpc::prelude::{JsonRpcMessage, MessageTransport, TransportError};
use hubrpc::protocol::jsonrpc::{JsonRpcResponse, ResponsePayload};
use serde_json::json;

use crate::cdp_runtime::CdpConnection;
use crate::cdp_transport::ManagedCdpTransport;
use crate::debugger_engine::ScriptState;
use crate::session_transport::CdpEnvelope;

const GENERATED: &str = "function work() { return 1; }";

struct CoverageTransport {
    requests: std::sync::Mutex<Vec<String>>,
    stop_failures: std::sync::atomic::AtomicUsize,
    inbound: tokio::sync::Mutex<mpsc::UnboundedReceiver<CdpEnvelope>>,
    outbound: mpsc::UnboundedSender<CdpEnvelope>,
}

#[async_trait]
impl MessageTransport<CdpEnvelope, CdpEnvelope> for CoverageTransport {
    async fn send(&self, envelope: CdpEnvelope) -> Result<(), TransportError> {
        let JsonRpcMessage::Request(request) = envelope.message else {
            return Ok(());
        };
        self.requests.lock().unwrap().push(request.method.clone());
        let payload = match request.method.as_str() {
            "Profiler.takePreciseCoverage" => ResponsePayload::Result(json!({
                "timestamp": 1,
                "result": [{
                    "scriptId": "1",
                    "url": "coverage-test://unit/generated.js",
                    "functions": [{
                        "functionName": "work",
                        "isBlockCoverage": false,
                        "ranges": [{"startOffset": 0, "endOffset": GENERATED.len(), "count": 1}]
                    }]
                }]
            })),
            "Profiler.stopPreciseCoverage" => {
                if self
                    .stop_failures
                    .fetch_update(Ordering::SeqCst, Ordering::SeqCst, |remaining| {
                        remaining.checked_sub(1)
                    })
                    .is_ok()
                {
                    ResponsePayload::Error(hubrpc::prelude::JsonRpcError::new(
                        -32000,
                        "stop failed",
                    ))
                } else {
                    ResponsePayload::Result(json!({}))
                }
            }
            "Debugger.getScriptSource" => {
                ResponsePayload::Result(json!({"scriptSource": GENERATED}))
            }
            method => panic!("unexpected CDP request: {method}"),
        };
        self.outbound
            .send(CdpEnvelope {
                session_id: envelope.session_id,
                message: JsonRpcMessage::Response(JsonRpcResponse {
                    id: Some(request.id),
                    payload,
                }),
            })
            .map_err(|_| TransportError::Closed)
    }

    async fn recv(&self) -> Option<CdpEnvelope> {
        self.inbound.lock().await.recv().await
    }
}

#[async_trait]
impl ManagedCdpTransport for CoverageTransport {
    fn close_reason(&self) -> Arc<tokio::sync::Mutex<Option<String>>> {
        Arc::new(tokio::sync::Mutex::new(None))
    }

    async fn wait_closed(&self) -> String {
        std::future::pending().await
    }

    async fn close(&self) {}
}

async fn coverage_driver(mapped: bool) -> (DebuggerDriver, SessionKey, Arc<CoverageTransport>) {
    let (outbound, inbound) = mpsc::unbounded_channel();
    let transport = Arc::new(CoverageTransport {
        requests: Default::default(),
        stop_failures: Default::default(),
        inbound: tokio::sync::Mutex::new(inbound),
        outbound,
    });
    let session = SessionKey {
        connection_generation: 1,
        session_id: "coverage-regression".into(),
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
    if mapped {
        let source_map = json!({
            "version": 3,
            "sources": ["authored.ts"],
            "sourcesContent": [GENERATED],
            "names": [],
            "mappings": "AAAA"
        });
        Arc::make_mut(&mut state.scripts).insert(
            ScriptKey {
                session: session.clone(),
                script_id: "1".into(),
            },
            Arc::new(ScriptState {
                url: "coverage-test://unit/generated.js".into(),
                hash: "coverage-regression".into(),
                source_map_url: Some(format!(
                    "data:application/json;base64,{}",
                    base64::engine::general_purpose::STANDARD.encode(source_map.to_string())
                )),
                version: 1,
                source: ScriptSourceState::Unresolved,
                captured_source: None,
                provenance: Default::default(),
            }),
        );
    }
    let mut sources = SourceEffectInterpreter::new(
        SourceEffectOptions::default(),
        Arc::new(ContextSourceModel::new()),
        "coverage-regression",
    );
    sources.retain_for_state(&state);
    let driver = DebuggerDriver::new(
        Arc::new(state),
        connection.take_root_debugger_session().unwrap(),
        sources,
    );
    (driver, session, transport)
}

#[tokio::test]
async fn named_and_unnamed_non_raw_captures_project_but_raw_captures_do_not() {
    for (capture_id, raw) in [
        (None, false),
        (Some("named"), false),
        (None, true),
        (Some("raw"), true),
    ] {
        let (mut driver, session, transport) = coverage_driver(true).await;
        let mut recording = CoverageRecording::default();
        let snapshot = capture_coverage(
            &mut driver,
            &session,
            &mut recording,
            capture_id.map(str::to_owned),
            None,
            raw,
        )
        .await
        .unwrap();
        let function = &snapshot.sources[0].functions[0];
        assert!(!function.block_coverage);
        if raw {
            assert!(function.effective_ranges.is_empty());
            assert!(function.authored_location.is_none());
            assert!(function.generated_location.is_none());
            assert_eq!(
                *transport.requests.lock().unwrap(),
                ["Profiler.takePreciseCoverage"]
            );
        } else {
            assert_eq!(function.effective_ranges.len(), 1);
            assert!(
                function
                    .authored_location
                    .as_ref()
                    .unwrap()
                    .source_url
                    .ends_with("authored.ts")
            );
            assert!(function.generated_location.is_some());
        }
        if let Some(capture_id) = capture_id {
            assert_eq!(recording.captures[capture_id], snapshot);
            let requests = transport.requests.lock().unwrap().len();
            assert!(matches!(
                capture_coverage(
                    &mut driver,
                    &session,
                    &mut recording,
                    Some(capture_id.into()),
                    None,
                    raw,
                )
                .await,
                Err(TargetDebuggerError::CoverageCaptureAlreadyExists(_))
            ));
            assert_eq!(transport.requests.lock().unwrap().len(), requests);
        }
    }
}

#[tokio::test]
async fn non_raw_coverage_has_effective_ranges_without_source_metadata() {
    let (mut driver, session, _) = coverage_driver(false).await;
    let snapshot = capture_coverage(
        &mut driver,
        &session,
        &mut CoverageRecording::default(),
        Some("unmapped".into()),
        None,
        false,
    )
    .await
    .unwrap();
    let function = &snapshot.sources[0].functions[0];
    assert_eq!(function.effective_ranges, function.ranges);
    assert_eq!(function.effective_ranges[0].count, 1);
    assert!(function.authored_location.is_none());
    assert!(function.generated_location.is_none());
}

#[tokio::test]
async fn stop_precedes_projection_and_failed_projection_preserves_raw_evidence_for_retry() {
    let (mut driver, session, transport) = coverage_driver(false).await;
    let mut recording = Some(CoverageRecording::default());
    let (completed, _) = finish_coverage_recording(&driver, &mut recording, None)
        .await
        .unwrap();
    assert!(recording.is_none());
    let raw = completed.snapshot();
    let mut pending = Some(raw.clone());
    let error = project_stopped_coverage(&mut pending, async |snapshot| {
        assert_eq!(
            *transport.requests.lock().unwrap(),
            [
                "Profiler.takePreciseCoverage",
                "Profiler.stopPreciseCoverage"
            ]
        );
        snapshot.sources.clear();
        Err(TargetDebuggerError::Coverage("projection failed".into()))
    })
    .await
    .unwrap_err();
    assert!(error.to_string().contains("recording stopped"));
    assert!(error.to_string().contains("raw coverage is retained"));
    assert_eq!(pending.as_ref(), Some(&raw));
    let snapshot = project_stopped_coverage(&mut pending, async |snapshot| {
        project_coverage(&mut driver, &session, snapshot, None, false).await
    })
    .await
    .unwrap();
    assert_eq!(snapshot.sources[0].functions[0].effective_ranges.len(), 1);
    assert!(pending.is_none());
    assert_eq!(transport.requests.lock().unwrap().len(), 2);
}

#[tokio::test]
async fn failed_native_stop_retains_accumulated_counts_and_named_captures_for_retry() {
    let (mut driver, session, transport) = coverage_driver(false).await;
    let mut active = CoverageRecording::default();
    let baseline = capture_coverage(
        &mut driver,
        &session,
        &mut active,
        Some("baseline".into()),
        None,
        true,
    )
    .await
    .unwrap();
    let mut recording = Some(active);
    transport.stop_failures.store(1, Ordering::SeqCst);
    assert!(
        finish_coverage_recording(&driver, &mut recording, None)
            .await
            .is_err()
    );
    let retained = recording.as_ref().unwrap();
    assert_eq!(retained.captures["baseline"], baseline);
    assert_eq!(
        retained.snapshot().sources[0].functions[0].ranges[0].count,
        2
    );
    let (completed, _) = finish_coverage_recording(&driver, &mut recording, None)
        .await
        .unwrap();
    assert!(recording.is_none());
    assert_eq!(
        completed.snapshot().sources[0].functions[0].ranges[0].count,
        3
    );
    assert_eq!(completed.captures["baseline"], baseline);
}

#[tokio::test]
async fn invalid_stop_baseline_does_not_take_or_stop_coverage() {
    let (driver, _, transport) = coverage_driver(false).await;
    let mut recording = Some(CoverageRecording::default());
    assert!(matches!(
        finish_coverage_recording(&driver, &mut recording, Some("missing".into())).await,
        Err(TargetDebuggerError::CoverageCaptureNotFound(_))
    ));
    assert!(recording.is_some());
    assert!(transport.requests.lock().unwrap().is_empty());
}
