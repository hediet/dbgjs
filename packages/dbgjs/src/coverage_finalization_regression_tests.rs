use super::*;
use async_trait::async_trait;
use base64::Engine;
use linkrpc::prelude::{JsonRpcMessage, MessageTransport, TransportError};
use linkrpc::protocol::jsonrpc::{JsonRpcResponse, ResponsePayload};
use serde_json::json;

use crate::cdp_runtime::CdpConnection;
use crate::cdp_transport::ManagedCdpTransport;
use crate::debugger_engine::{CapturedScriptSource, ScriptState};
use crate::session_transport::CdpEnvelope;
use crate::source_view::SourceMapData;

const GENERATED: &str = "function work() { return 1; }";

struct CoverageTransport {
    requests: std::sync::Mutex<Vec<String>>,
    stop_failures: std::sync::atomic::AtomicUsize,
    script_url: String,
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
                    "url": &self.script_url,
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
                    ResponsePayload::Error(linkrpc::prelude::JsonRpcError::new(
                        -32000,
                        "stop failed",
                    ))
                } else {
                    ResponsePayload::Result(json!({}))
                }
            }
            "Profiler.stop" => ResponsePayload::Result(json!({
                "profile": {
                    "nodes": [{
                        "id": 1,
                        "callFrame": {
                            "functionName": "work",
                            "scriptId": "1",
                            "url": &self.script_url,
                            "lineNumber": 0,
                            "columnNumber": 0
                        }
                    }],
                    "startTime": 1.0,
                    "endTime": 2.0,
                    "samples": [1],
                    "timeDeltas": [1]
                }
            })),
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
    coverage_driver_with_source(mapped, None).await
}

async fn coverage_driver_with_source(
    mapped: bool,
    available_source: Option<&str>,
) -> (DebuggerDriver, SessionKey, Arc<CoverageTransport>) {
    coverage_driver_with_url(
        mapped,
        available_source,
        "coverage-test://unit/generated.js",
    )
    .await
}

async fn coverage_driver_with_url(
    mapped: bool,
    available_source: Option<&str>,
    script_url: &str,
) -> (DebuggerDriver, SessionKey, Arc<CoverageTransport>) {
    let (outbound, inbound) = mpsc::unbounded_channel();
    let transport = Arc::new(CoverageTransport {
        requests: Default::default(),
        stop_failures: Default::default(),
        script_url: script_url.to_owned(),
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
            "sourcesContent": [available_source.unwrap_or(GENERATED)],
            "names": [],
            "mappings": "AAAA"
        });
        let map_bytes = if available_source.is_some() {
            format!("INVALID_MAP_SHOULD_NOT_PARSE_{}", "m".repeat(512 * 1024)).into_bytes()
        } else {
            source_map.to_string().into_bytes()
        };
        Arc::make_mut(&mut state.scripts).insert(
            ScriptKey {
                session: session.clone(),
                script_id: "1".into(),
            },
            Arc::new(ScriptState {
                url: script_url.to_owned(),
                hash: "coverage-regression".into(),
                source_map_url: Some(format!(
                    "data:application/json;base64,{}",
                    base64::engine::general_purpose::STANDARD.encode(&map_bytes)
                )),
                version: 1,
                source: ScriptSourceState::Unresolved,
                captured_source: available_source.map(|content| CapturedScriptSource {
                    content: Arc::from(content),
                    source_map: Some(SourceMapData::new(map_bytes)),
                    source_map_url: None,
                    source_map_error: None,
                }),
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
async fn raw_take_and_stop_do_not_fetch_or_parse_large_available_sources_and_maps() {
    let source = format!("UNPERSISTED_SOURCE_BYTES_{}", "s".repeat(512 * 1024));
    let (mut driver, session, transport) = coverage_driver_with_source(true, Some(&source)).await;
    let cache_before = driver.source_map_cache_stats();
    let mut recording = CoverageRecording::default();
    let captured = capture_coverage(
        &mut driver,
        &session,
        &mut recording,
        Some("first".into()),
    )
    .await
    .unwrap();
    let mut recording = Some(recording);
    let completed = finish_coverage_recording(&driver, &mut recording)
        .await
        .unwrap();
    let mut stopped = completed.snapshot();
    attach_coverage_provenance(&driver, &session, &mut stopped);

    assert_eq!(driver.source_map_cache_stats(), cache_before);
    assert_eq!(
        *transport.requests.lock().unwrap(),
        [
            "Profiler.takePreciseCoverage",
            "Profiler.takePreciseCoverage",
            "Profiler.stopPreciseCoverage",
        ]
    );
    assert!(
        stopped.sources[0].functions[0].ranges[0].count
            > captured.sources[0].functions[0].ranges[0].count
    );
    for snapshot in [captured, stopped] {
        let payload = serde_json::to_vec(&snapshot).unwrap();
        assert!(
            payload.len() < 2_000,
            "capture grew with source/map: {} bytes",
            payload.len()
        );
        assert!(
            !payload
                .windows(b"UNPERSISTED_SOURCE_BYTES_".len())
                .any(|part| part == b"UNPERSISTED_SOURCE_BYTES_")
        );
        assert!(
            snapshot.sources[0]
                .provenance
                .as_ref()
                .unwrap()
                .source_map_url
                .is_none()
        );
    }
}

#[tokio::test]
async fn raw_cpu_stop_only_requests_profile_and_does_not_serialize_source_or_map() {
    let source = format!("UNPERSISTED_SOURCE_BYTES_{}", "s".repeat(512 * 1024));
    let (driver, session, transport) = coverage_driver_with_source(true, Some(&source)).await;
    let cache_before = driver.source_map_cache_stats();
    let stopped = driver.client().profiler().stop().await.unwrap();
    let mut snapshot = cpu_profile_snapshot("raw-profile".into(), None, stopped.profile).unwrap();
    let script = &driver.state().scripts[&ScriptKey {
        session,
        script_id: "1".into(),
    }];
    snapshot
        .script_provenance
        .insert("1".into(), capture_script_provenance(script));

    assert_eq!(driver.source_map_cache_stats(), cache_before);
    assert_eq!(*transport.requests.lock().unwrap(), ["Profiler.stop"]);
    assert!(snapshot.functions.is_empty());
    assert_eq!(snapshot.samples, [1]);
    assert_eq!(snapshot.time_deltas_micros, [1]);
    let payload = serde_json::to_vec(&snapshot).unwrap();
    assert!(
        payload.len() < 2_000,
        "profile grew with source/map: {} bytes",
        payload.len()
    );
    assert!(
        !payload
            .windows(b"UNPERSISTED_SOURCE_BYTES_".len())
            .any(|part| part == b"UNPERSISTED_SOURCE_BYTES_")
    );
    assert!(snapshot.script_provenance["1"].source_map_url.is_none());
}

#[tokio::test]
async fn raw_profiler_captures_never_persist_inline_script_url_source_bytes() {
    let script_url = format!(
        "data:text/javascript,{}",
        "UNPERSISTED_INLINE_SCRIPT_BYTES_".repeat(10_000)
    );
    let (mut driver, session, transport) = coverage_driver_with_url(true, None, &script_url).await;
    let mut recording = CoverageRecording::default();
    let coverage = capture_coverage(
        &mut driver,
        &session,
        &mut recording,
        Some("inline-script".into()),
    )
    .await
    .unwrap();
    let profile = driver.client().profiler().stop().await.unwrap();
    let cpu = cpu_profile_snapshot("inline-cpu".into(), None, profile.profile).unwrap();
    assert_eq!(coverage.sources[0].script_id, "1");
    assert_eq!(cpu.nodes[0].call_frame.script_id, "1");
    assert_eq!(cpu.samples, [1]);
    for bytes in [
        serde_json::to_vec(&coverage).unwrap(),
        serde_json::to_vec(&cpu).unwrap(),
    ] {
        assert!(
            bytes.len() < 2_000,
            "raw payload contains inline source: {} bytes",
            bytes.len()
        );
        assert!(
            !bytes
                .windows(b"UNPERSISTED_INLINE_SCRIPT_BYTES_".len())
                .any(|part| part == b"UNPERSISTED_INLINE_SCRIPT_BYTES_")
        );
    }
    assert_eq!(
        *transport.requests.lock().unwrap(),
        ["Profiler.takePreciseCoverage", "Profiler.stop"]
    );
}

#[tokio::test]
async fn captured_provenance_omits_oversized_non_inline_urls() {
    let (driver, session, _) = coverage_driver(true).await;
    let key = ScriptKey {
        session,
        script_id: "1".into(),
    };
    let mut script = driver.state().scripts[&key].as_ref().clone();
    script.url = format!(
        "https://example.test/script?{}",
        "SECRET_SOURCE_BYTES_".repeat(500)
    );
    script.source_map_url = Some(format!(
        "https://example.test/script.map?{}",
        "SECRET_MAP_BYTES_".repeat(500)
    ));
    let provenance = capture_script_provenance(&script);
    assert_eq!(provenance.url, "<oversized-script-url>");
    assert!(provenance.source_map_url.is_none());
    let serialized = serde_json::to_string(&provenance).unwrap();
    assert!(!serialized.contains("SECRET_SOURCE_BYTES_"));
    assert!(!serialized.contains("SECRET_MAP_BYTES_"));
}

#[tokio::test]
async fn captured_provenance_reuses_cdp_sha256_without_source_hydration() {
    let (driver, session, transport) = coverage_driver(true).await;
    let key = ScriptKey {
        session,
        script_id: "1".into(),
    };
    let mut script = driver.state().scripts[&key].as_ref().clone();
    let digest = format!("{:x}", Sha256::digest(GENERATED.as_bytes()));
    script.hash = digest.clone();
    let provenance = capture_script_provenance(&script);
    assert_eq!(provenance.source_sha256.as_deref(), Some(digest.as_str()));
    assert!(transport.requests.lock().unwrap().is_empty());
}

#[tokio::test]
async fn named_and_unnamed_captures_persist_raw_without_enrichment() {
    for capture_id in [None, Some("named")] {
        let (mut driver, session, transport) = coverage_driver(true).await;
        let mut recording = CoverageRecording::default();
        let snapshot = capture_coverage(
            &mut driver,
            &session,
            &mut recording,
            capture_id.map(str::to_owned),
        )
        .await
        .unwrap();
        let function = &snapshot.sources[0].functions[0];
        assert!(!function.block_coverage);
        assert!(
            snapshot.sources[0]
                .provenance
                .as_ref()
                .unwrap()
                .source_map_url
                .is_none()
        );
        assert!(function.effective_ranges.is_empty());
        assert!(function.authored_location.is_none());
        assert!(function.generated_location.is_none());
        assert_eq!(
            *transport.requests.lock().unwrap(),
            ["Profiler.takePreciseCoverage"]
        );
        if let Some(capture_id) = capture_id {
            assert_eq!(recording.captures[capture_id], snapshot);
            let requests = transport.requests.lock().unwrap().len();
            assert!(matches!(
                capture_coverage(
                    &mut driver,
                    &session,
                    &mut recording,
                    Some(capture_id.into()),
                )
                .await,
                Err(TargetDebuggerError::CoverageCaptureAlreadyExists(_))
            ));
            assert_eq!(transport.requests.lock().unwrap().len(), requests);
        }
    }
}

#[tokio::test]
async fn coverage_capture_delegates_effective_ranges_to_view() {
    let (mut driver, session, _) = coverage_driver(false).await;
    let snapshot = capture_coverage(
        &mut driver,
        &session,
        &mut CoverageRecording::default(),
        Some("unmapped".into()),
    )
    .await
    .unwrap();
    let function = &snapshot.sources[0].functions[0];
    assert!(function.effective_ranges.is_empty());
    assert_eq!(function.ranges[0].count, 1);
    assert!(function.authored_location.is_none());
    assert!(function.generated_location.is_none());
    assert!(snapshot.sources[0].provenance.is_some());
    let mut viewed = snapshot.clone();
    crate::capture_projection::project_stored_coverage(&mut viewed);
    assert_eq!(viewed.sources[0].functions[0].effective_ranges.len(), 1);
    assert!(!viewed.projection_diagnostics.is_empty());
}

#[tokio::test]
async fn stopped_raw_coverage_survives_unavailable_view_projection() {
    let (driver, session, transport) = coverage_driver(false).await;
    let mut recording = Some(CoverageRecording::default());
    let completed = finish_coverage_recording(&driver, &mut recording)
        .await
        .unwrap();
    assert!(recording.is_none());
    let mut raw = completed.snapshot();
    attach_coverage_provenance(&driver, &session, &mut raw);
    let payload = serde_json::to_vec(&raw).unwrap();
    let mut viewed = raw.clone();
    crate::capture_projection::project_stored_coverage(&mut viewed);
    assert_eq!(viewed.sources[0].functions[0].effective_ranges.len(), 1);
    assert!(!viewed.projection_diagnostics.is_empty());
    assert_eq!(serde_json::to_vec(&raw).unwrap(), payload);
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
    )
    .await
    .unwrap();
    let mut recording = Some(active);
    transport.stop_failures.store(1, Ordering::SeqCst);
    assert!(
        finish_coverage_recording(&driver, &mut recording)
            .await
            .is_err()
    );
    let retained = recording.as_ref().unwrap();
    assert_eq!(retained.captures["baseline"], baseline);
    assert_eq!(
        retained.snapshot().sources[0].functions[0].ranges[0].count,
        2
    );
    let completed = finish_coverage_recording(&driver, &mut recording)
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
async fn successive_captures_and_stop_preserve_complete_cumulative_coverage() {
    let (mut driver, session, _) = coverage_driver(false).await;
    let mut active = CoverageRecording::default();
    let baseline = capture_coverage(
        &mut driver,
        &session,
        &mut active,
        Some("baseline".into()),
    )
    .await
    .unwrap();
    let selected = capture_coverage(
        &mut driver,
        &session,
        &mut active,
        Some("selected".into()),
    )
    .await
    .unwrap();
    assert_eq!(baseline.sources[0].functions[0].ranges[0].count, 1);
    assert_eq!(selected.sources[0].functions[0].ranges[0].count, 2);
    assert!(
        exclude_coverage(selected.clone(), &baseline)
            .sources
            .is_empty()
    );
    assert_eq!(active.captures["baseline"], baseline);
    assert_eq!(active.captures["selected"], selected);
    let mut recording = Some(active);
    let completed = finish_coverage_recording(&driver, &mut recording)
        .await
        .unwrap();
    assert_eq!(
        completed.snapshot().sources[0].functions[0].ranges[0].count,
        3
    );
    assert_eq!(completed.captures["baseline"], baseline);
    assert_eq!(completed.captures["selected"], selected);
}
