use super::*;
use async_trait::async_trait;
use base64::Engine;
use linkrpc::prelude::{JsonRpcMessage, MessageTransport, TransportError};
use linkrpc::protocol::jsonrpc::{JsonRpcResponse, ResponsePayload};
use serde_json::json;

use crate::cdp_runtime::CdpConnection;
use crate::cdp_transport::ManagedCdpTransport;
use crate::debugger_engine::ScriptState;
use crate::session_transport::CdpEnvelope;

struct SourceTransport {
    responses: BTreeMap<String, Result<String, String>>,
    replayed_scripts: Vec<(String, String)>,
    requests: std::sync::Mutex<Vec<String>>,
    breakpoint_requests: std::sync::Mutex<Vec<serde_json::Value>>,
    inbound: tokio::sync::Mutex<tokio::sync::mpsc::UnboundedReceiver<CdpEnvelope>>,
    outbound: tokio::sync::mpsc::UnboundedSender<CdpEnvelope>,
}

#[async_trait]
impl MessageTransport<CdpEnvelope, CdpEnvelope> for SourceTransport {
    async fn send(&self, envelope: CdpEnvelope) -> Result<(), TransportError> {
        let JsonRpcMessage::Request(request) = envelope.message else {
            return Ok(());
        };
        let payload = match request.method.as_str() {
            "Runtime.enable" | "Debugger.disable" => ResponsePayload::Result(json!({})),
            "Debugger.enable" => {
                for (script_id, url) in &self.replayed_scripts {
                    self.outbound
                        .send(CdpEnvelope {
                            session_id: envelope.session_id.clone(),
                            message: JsonRpcMessage::Notification(
                                linkrpc::prelude::JsonRpcNotification {
                                    method: "Debugger.scriptParsed".into(),
                                    params: Some(json!({
                                        "scriptId": script_id,
                                        "url": url,
                                        "startLine": 0,
                                        "startColumn": 0,
                                        "endLine": 1,
                                        "endColumn": 0,
                                        "executionContextId": 1,
                                        "hash": script_id,
                                    })),
                                },
                            ),
                        })
                        .map_err(|_| TransportError::Closed)?;
                }
                ResponsePayload::Result(json!({ "debuggerId": "source-test" }))
            }
            "Debugger.getScriptSource" => {
                let script = request.params.as_ref().unwrap()["scriptId"].as_str().unwrap();
                self.requests.lock().unwrap().push(script.to_owned());
                let Some(response) = self.responses.get(script) else {
                    return Ok(());
                };
                match response {
                    Ok(content) => ResponsePayload::Result(json!({ "scriptSource": content })),
                    Err(message) => ResponsePayload::Error(linkrpc::prelude::JsonRpcError::new(-32000, message.clone())),
                }
            }
            "Debugger.setBreakpoint" => {
                let params = request.params.as_ref().unwrap();
                self.breakpoint_requests.lock().unwrap().push(params.clone());
                ResponsePayload::Result(json!({ "breakpointId": "runtime-bp", "actualLocation": params["location"] }))
            }
            other => panic!("unexpected CDP method {other}"),
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
impl ManagedCdpTransport for SourceTransport {
    fn close_reason(&self) -> Arc<tokio::sync::Mutex<Option<String>>> {
        Arc::new(tokio::sync::Mutex::new(None))
    }

    async fn wait_closed(&self) -> String {
        std::future::pending().await
    }

    async fn close(&self) {}
}

async fn source_driver(
    scripts: &[(&str, &str, Option<String>)],
    responses: BTreeMap<String, Result<String, String>>,
) -> (DebuggerDriver, Arc<SourceTransport>) {
    let (outbound, inbound) = tokio::sync::mpsc::unbounded_channel();
    let transport = Arc::new(SourceTransport {
        responses,
        replayed_scripts: Vec::new(),
        requests: Default::default(),
        breakpoint_requests: Default::default(),
        inbound: tokio::sync::Mutex::new(inbound),
        outbound,
    });
    let connection = CdpConnection::connect_root_debugger_transport(
        transport.clone(),
        1,
        "source-test".to_owned(),
    )
    .await
    .unwrap();
    let mut state =
        (*crate::debugger_engine::reduce(&Arc::new(DebuggerState::default()), Input::Connected)
            .state)
            .clone();
    for (id, url, source_map_url) in scripts {
        Arc::make_mut(&mut state.scripts).insert(
            ScriptKey {
                session: SessionKey {
                    connection_generation: 1,
                    session_id: "source-test".to_owned(),
                },
                script_id: (*id).to_owned(),
            },
            Arc::new(ScriptState {
                url: (*url).to_owned(),
                hash: (*id).to_owned(),
                source_map_url: source_map_url.clone(),
                version: 1,
                source: ScriptSourceState::Unresolved,
                provenance: Default::default(),
                captured_source: None,
            }),
        );
    }
    let mut sources = SourceEffectInterpreter::new(
        SourceEffectOptions::default(),
        Arc::new(ContextSourceModel::new()),
        "source-test",
    );
    sources.retain_for_state(&state);
    let driver = DebuggerDriver::new(
        Arc::new(state),
        connection.take_root_debugger_session().unwrap(),
        sources,
    );
    (driver, transport)
}

async fn source_target(
    replayed_scripts: Vec<(String, String)>,
    responses: BTreeMap<String, Result<String, String>>,
) -> TargetDebuggerHandle {
    let (outbound, inbound) = tokio::sync::mpsc::unbounded_channel();
    let transport = Arc::new(SourceTransport {
        responses,
        replayed_scripts,
        requests: Default::default(),
        breakpoint_requests: Default::default(),
        inbound: tokio::sync::Mutex::new(inbound),
        outbound,
    });
    let connection = CdpConnection::connect_root_debugger_transport(
        transport,
        1,
        "source-test".to_owned(),
    )
    .await
    .unwrap();
    let session = SessionKey {
        connection_generation: 1,
        session_id: "source-test".to_owned(),
    };
    TargetDebuggerHandle::start(
        "source-test".to_owned(),
        "source-test".to_owned(),
        "runtime".to_owned(),
        1,
        connection.take_root_debugger_session().unwrap(),
        session,
        false,
        Arc::new(ContextSourceModel::new()),
    )
    .await
    .unwrap()
}

#[tokio::test]
async fn immediate_runtime_source_search_waits_for_initial_script_replay() {
    for _ in 0..32 {
        let target = source_target(
            vec![(
                "workbench".to_owned(),
                "file:///out/vs/workbench/workbench.desktop.main.js".to_owned(),
            )],
            BTreeMap::from([(
                "workbench".to_owned(),
                Ok("super(message || \"An unexpected bug occurred.\")".to_owned()),
            )]),
        )
        .await;

        let batch = target
            .source_search_batch(
                Some("workbench.desktop.main.js".to_owned()),
                true,
                SearchControl::with_deadline(
                    std::time::Instant::now() + Duration::from_secs(1),
                ),
            )
            .await
            .unwrap();
        assert_eq!(batch.sources.len(), 1);
        assert!(
            batch.sources[0]
                .content
                .contains("An unexpected bug occurred.")
        );

        let no_match = tokio::time::timeout(
            Duration::from_millis(100),
            target.source_search_batch(
                Some("missing.js".to_owned()),
                true,
                SearchControl::default(),
            ),
        )
        .await
        .unwrap()
        .unwrap();
        assert!(no_match.sources.is_empty());
    }
}

#[tokio::test]
async fn initial_script_replay_remains_within_search_deadline_and_cancellation() {
    for control in {
        let cancelled = SearchControl::default();
        cancelled.cancel();
        [
            SearchControl::with_deadline(std::time::Instant::now()),
            cancelled,
        ]
    } {
        let target = source_target(
            vec![(
                "workbench".to_owned(),
                "file:///out/vs/workbench/workbench.desktop.main.js".to_owned(),
            )],
            BTreeMap::from([("workbench".to_owned(), Ok("content".to_owned()))]),
        )
        .await;
        let result = target
            .source_search_batch(
                Some("workbench.desktop.main.js".to_owned()),
                true,
                control,
            )
            .await;
        assert!(matches!(
            result,
            Err(TargetDebuggerError::SourceSearch(
                SearchError::DeadlineExceeded | SearchError::Cancelled
            ))
        ));
    }
}

#[tokio::test]
async fn runtime_breakpoint_uses_zero_hop_route_without_loading_sources_or_maps() {
    let (mut driver, transport) = source_driver(
        &[
            ("bundle", "https://test/bundle.min.js", Some("https://test/hung.map".to_owned())),
            ("unrelated", "https://test/vendor.js", Some("https://test/also-hung.map".to_owned())),
        ],
        BTreeMap::new(),
    ).await;
    let key = BreakpointKey { client_id: "test".to_owned(), breakpoint_id: "runtime-hit".to_owned() };
    tokio::time::timeout(Duration::from_secs(1), driver.apply(Input::SetBreakpoint {
        key: key.clone(),
        source_url: "https://test/bundle.min.js".to_owned(),
        position: Position { line: 8, column: 414 },
        condition: Some("false".to_owned()),
    })).await.unwrap().unwrap();
    assert!(transport.requests.lock().unwrap().is_empty());
    assert_eq!(*transport.breakpoint_requests.lock().unwrap(), vec![json!({
        "location": { "scriptId": "bundle", "lineNumber": 8, "columnNumber": 414 },
        "condition": "false",
    })]);
    let breakpoint = &driver.state().breakpoints[&key];
    assert_eq!(breakpoint.bindings.len(), 1);
    assert!(breakpoint.bindings.values().all(|binding| matches!(binding, BreakpointBinding::Installed { .. })));
    assert!(driver.state().scripts.values().all(|script| matches!(script.source, ScriptSourceState::Unresolved)));
    assert_eq!(driver.source_effects().retained_view_count(), 0);
    let session = driver.state().scripts.keys().next().unwrap().session.clone();
    tokio::time::timeout(Duration::from_secs(1), driver.apply(Input::ScriptParsed {
        session,
        script_id: "later".to_owned(),
        url: "https://test/later.js".to_owned(),
        hash: "later".to_owned(),
        source_map_url: Some("https://test/later.map".to_owned()),
    })).await.unwrap().unwrap();
    assert!(transport.requests.lock().unwrap().is_empty());
    assert_eq!(transport.breakpoint_requests.lock().unwrap().len(), 1);
}

#[tokio::test]
async fn runtime_source_search_skips_unrelated_scripts_and_unresponsive_maps() {
    let map_server = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let map_url = format!("http://{}/bundle.js.map", map_server.local_addr().unwrap());
    let (driver, transport) = source_driver(
        &[
            ("bundle", "https://test/bundle.js", Some(map_url.clone())),
            ("unrelated", "https://test/vendor.js", Some(map_url)),
        ],
        BTreeMap::from([("bundle".to_owned(), Ok("const needle = 42;".to_owned()))]),
    )
    .await;
    let control = SearchControl::with_deadline(std::time::Instant::now() + Duration::from_secs(1));
    let batch = runtime_source_search_batch(&driver, Some("bundle.js"), &control)
        .await
        .unwrap();
    assert_eq!(batch.sources.len(), 1);
    let source = &batch.sources[0];
    assert_eq!(source.path, "https://test/bundle.js");
    assert_eq!(source.kind, "runtime");
    assert_eq!(&*source.content, "const needle = 42;");
    assert_eq!(
        source.content_hash,
        crate::content_store::ContentHash::of_bytes(source.content.as_bytes())
    );
    assert_eq!(batch.skipped_sources, 0);
    assert_eq!(*transport.requests.lock().unwrap(), ["bundle"]);
    assert!(
        tokio::time::timeout(Duration::from_millis(20), map_server.accept())
            .await
            .is_err()
    );
    assert!(
        driver
            .state()
            .scripts
            .values()
            .all(|script| matches!(script.source, ScriptSourceState::Unresolved))
    );
}

#[tokio::test]
async fn runtime_source_search_preserves_later_authored_discovery_and_ignores_cached_maps() {
    let map = json!({
        "version": 3, "sources": ["src/editor.ts"], "sourcesContent": ["export const authoredNeedle = 42;"],
        "names": [], "mappings": "AAAA"
    });
    let map_url = format!(
        "data:application/json;base64,{}",
        base64::engine::general_purpose::STANDARD.encode(map.to_string())
    );
    let (mut driver, transport) = source_driver(
        &[("bundle", "https://test/bundle.js", Some(map_url))],
        BTreeMap::from([(
            "bundle".to_owned(),
            Ok("const runtimeNeedle = 42;".to_owned()),
        )]),
    )
    .await;
    let control = SearchControl::default();
    let before = runtime_source_search_batch(&driver, None, &control)
        .await
        .unwrap();
    assert_eq!(before.sources.len(), 1);
    acquire_sources(
        &mut driver,
        SourceAcquisition::Search(Some("editor.ts")),
        None,
    )
    .await
    .unwrap();
    let authored = driver
        .source_effects()
        .search_source_batch(driver.state(), Some("editor.ts"), &control)
        .unwrap();
    assert!(
        authored
            .sources
            .iter()
            .any(|source| source.content.contains("authoredNeedle"))
    );
    let after = runtime_source_search_batch(&driver, None, &control)
        .await
        .unwrap();
    assert_eq!(after.sources.len(), 1);
    assert_eq!(after.sources[0].kind, "runtime");
    assert_eq!(
        after.sources[0].content_hash,
        before.sources[0].content_hash
    );
    let absent = runtime_source_search_batch(&driver, Some("editor.ts"), &control)
        .await
        .unwrap();
    assert!(absent.sources.is_empty());
    assert_eq!(*transport.requests.lock().unwrap(), ["bundle", "bundle"]);
}

#[tokio::test]
async fn runtime_source_search_reports_fetch_failures_and_deduplicates_scripts() {
    let (driver, _) = source_driver(
        &[
            ("bad", "https://test/bad.js", None),
            ("good", "https://test/good.js", None),
            ("same", "https://test/good.js", None),
            ("stale", "https://test/good.js", None),
        ],
        BTreeMap::from([
            ("bad".to_owned(), Err("script was collected".to_owned())),
            ("good".to_owned(), Ok("needle".to_owned())),
            ("same".to_owned(), Ok("needle".to_owned())),
            (
                "stale".to_owned(),
                Err("old script was collected".to_owned()),
            ),
        ]),
    )
    .await;
    let batch = runtime_source_search_batch(&driver, None, &SearchControl::default())
        .await
        .unwrap();
    assert_eq!(batch.sources.len(), 1);
    assert_eq!(batch.skipped_sources, 1);
    assert_eq!(batch.skipped[0].path, "https://test/bad.js");
    assert!(batch.skipped[0].reason.contains("script was collected"));
}

#[tokio::test]
async fn runtime_source_search_cancels_inflight_requests_without_changing_hydration_state() {
    for deadline in [false, true] {
        let (driver, transport) = source_driver(
            &[
                ("a-hung", "https://test/hung.js", None),
                ("z-next", "https://test/next.js", None),
            ],
            BTreeMap::new(),
        )
        .await;
        let control = if deadline {
            SearchControl::with_deadline(std::time::Instant::now() + Duration::from_millis(30))
        } else {
            SearchControl::default()
        };
        let cancel = control.clone();
        let cancel_transport = transport.clone();
        let cancellation = tokio::spawn(async move {
            while cancel_transport.requests.lock().unwrap().is_empty() {
                tokio::task::yield_now().await;
            }
            if !deadline {
                cancel.cancel();
            }
        });
        let result = tokio::time::timeout(
            Duration::from_secs(2),
            runtime_source_search_batch(&driver, None, &control),
        )
        .await
        .unwrap();
        cancellation.await.unwrap();
        assert!(matches!(result, Err(TargetDebuggerError::SourceSearch(_))));
        assert_eq!(*transport.requests.lock().unwrap(), ["a-hung"]);
        assert!(
            driver
                .state()
                .scripts
                .values()
                .all(|script| matches!(script.source, ScriptSourceState::Unresolved))
        );
    }
}

#[tokio::test]
async fn source_acquisition_searches_metadata_and_reports_individual_fetch_failures() {
    let (mut driver, transport) = source_driver(
        &[
            ("good", "https://test/editor.js", None),
            ("bad", "https://test/editor-bad.js", None),
            ("other", "https://test/unrelated.js", None),
        ],
        BTreeMap::from([
            ("good".to_owned(), Ok("const searchable = 42;".to_owned())),
            ("bad".to_owned(), Err("script was collected".to_owned())),
        ]),
    )
    .await;
    let control = SearchControl::default();
    assert!(
        driver
            .generated_source_content(driver.state().scripts.keys().next().unwrap())
            .is_none()
    );
    acquire_sources(
        &mut driver,
        SourceAcquisition::Search(Some("editor")),
        Some(&control),
    )
    .await
    .unwrap();
    let batch = driver
        .source_effects()
        .search_source_batch(driver.state(), Some("editor"), &control)
        .unwrap();
    assert!(
        batch
            .sources
            .iter()
            .any(|source| source.content.contains("searchable"))
    );
    assert_eq!(batch.skipped_sources, 1);
    assert_eq!(batch.skipped[0].path, "https://test/editor-bad.js");
    assert!(batch.skipped[0].reason.contains("script was collected"));
    assert_eq!(*transport.requests.lock().unwrap(), ["bad", "good"]);
}

#[tokio::test]
async fn source_acquisition_normalizes_first_formatted_access_without_a_map() {
    let url = "https://test/editor.min.js";
    let (mut driver, transport) = source_driver(
        &[("script", url, None)],
        BTreeMap::from([(
            "script".to_owned(),
            Ok("function editor(){return 42;}editor();".to_owned()),
        )]),
    )
    .await;
    let formatted = format!("{url}?formatted");
    hydrate_source_for_path(&mut driver, &formatted)
        .await
        .unwrap();
    let script = driver.state().scripts.keys().next().unwrap();
    let content = driver.logical_source_content(script, &formatted).unwrap();
    assert!(content.contains('\n'));
    assert!(content.contains("return 42"));
    let batch = driver
        .source_effects()
        .search_source_batch(driver.state(), Some(&formatted), &SearchControl::default())
        .unwrap();
    assert!(batch.sources.iter().any(|source| source.path == formatted));
    hydrate_source_for_path(&mut driver, &formatted)
        .await
        .unwrap();
    assert_eq!(*transport.requests.lock().unwrap(), ["script"]);
}

#[tokio::test]
async fn formatted_breakpoint_routes_through_the_graph_to_the_runtime_script() {
    let url = "https://test/editor.min.js";
    let (mut driver, transport) = source_driver(
        &[("script", url, None)],
        BTreeMap::from([(
            "script".to_owned(),
            Ok("function editor(){return 42;}editor();".to_owned()),
        )]),
    )
    .await;
    let formatted = format!("{url}?formatted");
    hydrate_source_for_path(&mut driver, &formatted)
        .await
        .unwrap();
    let formatted_position = Position { line: 1, column: 0 };
    let expected = driver
        .source_effects()
        .map_source_position(&formatted, formatted_position)
        .into_iter()
        .find(|(_, _, direction, _)| direction == "authored-to-generated")
        .expect("formatted position maps to the runtime script")
        .1;

    driver
        .apply(Input::SetBreakpoint {
            key: BreakpointKey {
                client_id: "test".to_owned(),
                breakpoint_id: "formatted-hit".to_owned(),
            },
            source_url: formatted,
            position: formatted_position,
            condition: None,
        })
        .await
        .unwrap();

    assert_eq!(transport.breakpoint_requests.lock().unwrap().len(), 1);
    assert_eq!(
        transport.breakpoint_requests.lock().unwrap()[0]["location"],
        json!({
            "scriptId": "script",
            "lineNumber": expected.line,
            "columnNumber": expected.column,
        })
    );
}

#[tokio::test]
async fn source_acquisition_discovers_authored_names_not_present_in_bundle_url() {
    let map = json!({
        "version": 3, "sources": ["src/editor.ts"], "sourcesContent": ["export const authoredNeedle = 42;"],
        "names": [], "mappings": "AAAA"
    });
    let map_url = format!(
        "data:application/json;base64,{}",
        base64::engine::general_purpose::STANDARD.encode(map.to_string())
    );
    let (mut driver, transport) = source_driver(
        &[("bundle", "https://test/dist/bundle.js", Some(map_url))],
        BTreeMap::from([("bundle".to_owned(), Ok("const x=42;".to_owned()))]),
    )
    .await;
    acquire_sources(
        &mut driver,
        SourceAcquisition::Search(Some("editor.ts")),
        None,
    )
    .await
    .unwrap();
    let batch = driver
        .source_effects()
        .search_source_batch(driver.state(), Some("editor.ts"), &SearchControl::default())
        .unwrap();
    assert!(
        batch
            .sources
            .iter()
            .any(|source| source.content.contains("authoredNeedle"))
    );
    assert_eq!(batch.skipped_sources, 0);
    assert_eq!(*transport.requests.lock().unwrap(), ["bundle"]);
}

#[tokio::test]
async fn source_acquisition_keeps_runtime_searchable_when_source_map_loading_fails() {
    let (mut driver, _) = source_driver(
        &[(
            "bundle",
            "https://test/bundle.js",
            Some("data:application/json;base64,bm90IGEgbWFw".to_owned()),
        )],
        BTreeMap::from([(
            "bundle".to_owned(),
            Ok("const runtimeNeedle=42;".to_owned()),
        )]),
    )
    .await;
    acquire_sources(&mut driver, SourceAcquisition::Search(None), None)
        .await
        .unwrap();
    let batch = driver
        .source_effects()
        .search_source_batch(driver.state(), None, &SearchControl::default())
        .unwrap();
    assert!(
        batch
            .sources
            .iter()
            .any(|source| source.content.contains("runtimeNeedle"))
    );
    assert_eq!(batch.skipped_sources, 1);
    assert_eq!(batch.skipped[0].kind, "source-map");
    assert!(
        batch.skipped[0]
            .reason
            .contains("could not discover authored sources")
    );
}

#[tokio::test]
async fn source_acquisition_cancellation_completes_inflight_effect_and_stops_followup_fetches() {
    for deadline in [false, true] {
        let (mut driver, transport) = source_driver(
            &[
                ("a-hung", "https://test/hung.js", None),
                ("z-next", "https://test/next.js", None),
            ],
            BTreeMap::new(),
        )
        .await;
        let control = if deadline {
            SearchControl::with_deadline(std::time::Instant::now() + Duration::from_millis(30))
        } else {
            SearchControl::default()
        };
        let cancel = control.clone();
        let cancel_transport = transport.clone();
        let cancellation = tokio::spawn(async move {
            while cancel_transport.requests.lock().unwrap().is_empty() {
                tokio::task::yield_now().await;
            }
            if !deadline {
                cancel.cancel();
            }
        });
        let result = tokio::time::timeout(
            Duration::from_secs(2),
            acquire_sources(&mut driver, SourceAcquisition::Search(None), Some(&control)),
        )
        .await
        .unwrap();
        cancellation.await.unwrap();
        assert!(matches!(result, Err(TargetDebuggerError::SourceSearch(_))));
        assert_eq!(*transport.requests.lock().unwrap(), ["a-hung"]);
        assert!(matches!(
            driver.state().scripts.values().next().unwrap().source,
            ScriptSourceState::Failed(_)
        ));
        assert!(
            driver
                .state()
                .scripts
                .values()
                .all(|script| { !matches!(script.source, ScriptSourceState::Pending(_)) })
        );
        assert!(
            !source_acquisition_candidates(driver.state(), SourceAcquisition::Search(None))
                .is_empty()
        );
    }
}
