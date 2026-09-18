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
    requests: std::sync::Mutex<Vec<String>>,
    inbound: tokio::sync::Mutex<tokio::sync::mpsc::UnboundedReceiver<CdpEnvelope>>,
    outbound: tokio::sync::mpsc::UnboundedSender<CdpEnvelope>,
}

#[async_trait]
impl MessageTransport<CdpEnvelope, CdpEnvelope> for SourceTransport {
    async fn send(&self, envelope: CdpEnvelope) -> Result<(), TransportError> {
        let JsonRpcMessage::Request(request) = envelope.message else {
            return Ok(());
        };
        assert_eq!(request.method, "Debugger.getScriptSource");
        let script = request.params.as_ref().unwrap()["scriptId"]
            .as_str()
            .unwrap();
        self.requests.lock().unwrap().push(script.to_owned());
        let Some(response) = self.responses.get(script) else {
            return Ok(());
        };
        let payload = match response {
            Ok(content) => ResponsePayload::Result(json!({ "scriptSource": content })),
            Err(message) => {
                ResponsePayload::Error(linkrpc::prelude::JsonRpcError::new(-32000, message.clone()))
            }
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
        requests: Default::default(),
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
