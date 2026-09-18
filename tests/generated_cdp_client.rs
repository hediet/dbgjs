use std::sync::Arc;

use dbgjs::cdp::{
    CdpClient, DebuggerPausedParams, DebuggerScriptParsedParams, DebuggerSetBreakpointByUrlParams,
    RuntimeEvaluateParams, RuntimeRemoteObjectType, TargetAttachToTargetParams,
};
use dbgjs::session_transport::{CdpEnvelope, CdpSessionMux};
use linkrpc::connection::channel::{Channel, RejectingHandler};
use linkrpc::prelude::{JsonRpcMessage, MessageTransport};
use linkrpc::protocol::jsonrpc::{JsonRpcResponse, ResponsePayload};
use linkrpc::transport::memory::transport_pair_of;
use serde_json::json;

#[test]
fn script_parsed_accepts_events_without_newer_build_id() {
    let event: DebuggerScriptParsedParams = serde_json::from_value(json!({
        "scriptId": "1",
        "url": "file:///app.js",
        "startLine": 0,
        "startColumn": 0,
        "endLine": 1,
        "endColumn": 0,
        "executionContextId": 1,
        "hash": "abc"
    }))
    .unwrap();
    assert_eq!(event.build_id, None);
}

#[test]
fn debugger_paused_accepts_node_specific_reasons() {
    let event: DebuggerPausedParams = serde_json::from_value(json!({
        "callFrames": [],
        "reason": "Break on start"
    }))
    .unwrap();
    assert_eq!(event.reason, "Break on start");
}

#[tokio::test]
async fn generated_target_runtime_and_debugger_clients_use_the_flat_cdp_channel() {
    let (client_raw, browser_raw) = transport_pair_of::<CdpEnvelope>();
    let mux = CdpSessionMux::new(Arc::new(client_raw));
    let channel = Channel::new(
        Box::new(mux.open_root().expect("root channel opens")),
        Box::new(RejectingHandler),
    );
    let client = CdpClient::root(channel.clone());
    assert_eq!(
        client.debugger_script_parsed_event_name(),
        "Debugger.scriptParsed"
    );

    let mux_loop = mux.clone();
    tokio::spawn(async move { mux_loop.run().await });
    let channel_loop = channel.clone();
    tokio::spawn(async move { channel_loop.run().await });

    let browser = tokio::spawn(async move {
        let mut observed = Vec::new();
        for result in [
            json!({ "sessionId": "child-session" }),
            json!({ "result": { "type": "number", "value": 42 } }),
            (json!({ "breakpointId": "breakpoint-1", "locations": [] })),
        ] {
            let envelope = browser_raw.recv().await.expect("request arrives");
            let JsonRpcMessage::Request(request) = envelope.message else {
                panic!("expected CDP request");
            };
            observed.push((envelope.session_id.clone(), request.method));
            browser_raw
                .send(CdpEnvelope {
                    session_id: envelope.session_id,
                    message: JsonRpcMessage::Response(JsonRpcResponse {
                        id: Some(request.id),
                        payload: ResponsePayload::Result(result),
                    }),
                })
                .await
                .expect("response sends");
        }
        observed
    });

    let target_params: TargetAttachToTargetParams =
        serde_json::from_value(json!({ "targetId": "target-1", "flatten": true })).unwrap();
    let target = client
        .target_attach_to_target(target_params)
        .await
        .expect("target attaches");
    assert_eq!(target.session_id, "child-session");

    let child_channel = Channel::new(
        Box::new(
            mux.open_session(target.session_id.clone())
                .expect("child channel opens"),
        ),
        Box::new(RejectingHandler),
    );
    let child_client = CdpClient::root(child_channel.clone());
    tokio::spawn(async move { child_channel.run().await });

    let runtime_params: RuntimeEvaluateParams =
        serde_json::from_value(json!({ "expression": "6 * 7", "returnByValue": true })).unwrap();
    let runtime = child_client
        .runtime_evaluate(runtime_params)
        .await
        .expect("runtime evaluates");
    assert_eq!(runtime.result.r#type, RuntimeRemoteObjectType::Number);
    assert_eq!(runtime.result.value, Some(json!(42)));

    let breakpoint_params: DebuggerSetBreakpointByUrlParams =
        serde_json::from_value(json!({ "lineNumber": 0, "url": "file:///app.js" })).unwrap();
    let breakpoint = child_client
        .debugger_set_breakpoint_by_url(breakpoint_params)
        .await
        .expect("breakpoint installs");
    assert_eq!(breakpoint.breakpoint_id, "breakpoint-1");
    assert!(breakpoint.locations.is_empty());

    let observed = browser.await.expect("browser task completes");
    assert_eq!(
        observed,
        vec![
            (None, "Target.attachToTarget".into()),
            (Some("child-session".into()), "Runtime.evaluate".into()),
            (
                Some("child-session".into()),
                "Debugger.setBreakpointByUrl".into()
            ),
        ]
    );
}
