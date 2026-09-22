use std::sync::Arc;

use dbgjs::cdp::{
    CdpClient, CdpEventsClient, DebuggerPausedParams, DebuggerScriptParsedParams,
    DebuggerSetBreakpointByUrlParams, RuntimeEvaluateParams, RuntimeRemoteObjectType,
    TargetAttachToTargetParams,
};
use dbgjs::session_transport::{CdpEnvelope, CdpSessionMux};
use linkrpc::connection::channel::{Channel, RejectingHandler};
use linkrpc::prelude::{CallCtx, InterfaceHandler, JsonRpcError, JsonRpcMessage, MessageTransport};
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

#[test]
fn attach_to_target_accepts_the_dbgjs_auto_attach_hint() {
    let params: TargetAttachToTargetParams = serde_json::from_value(json!({
        "targetId": "target-1",
        "flatten": true,
        "__dbgjsAutoAttach": true
    }))
    .unwrap();
    assert_eq!(params.dbgjs_auto_attach, Some(true));
    assert_eq!(
        serde_json::to_value(params).unwrap()["__dbgjsAutoAttach"],
        true
    );
}

struct DefaultRuntimeService;

impl dbgjs::cdp::runtime::RuntimeService for DefaultRuntimeService {}

#[test]
fn generated_cdp_provider_trait_can_use_default_methods() {
    fn assert_provider<T: dbgjs::cdp::runtime::RuntimeService>() {}
    assert_provider::<DefaultRuntimeService>();
}

#[derive(Default)]
struct RuntimeEventReceiver {
    contexts: std::sync::Mutex<Vec<i64>>,
}

#[async_trait::async_trait]
impl dbgjs::cdp::runtime_events::RuntimeEventsService for RuntimeEventReceiver {
    async fn console_apicalled(
        &self,
        _ctx: &CallCtx,
        params: dbgjs::cdp::RuntimeConsoleApicalledParams,
    ) -> Result<(), JsonRpcError> {
        self.contexts.lock().unwrap().push(params.execution_context_id);
        Ok(())
    }
}

#[tokio::test]
async fn generated_event_contract_sends_and_receives_flat_root_and_session_notifications() {
    let (sender_raw, receiver_raw) = transport_pair_of::<CdpEnvelope>();
    let mux = CdpSessionMux::new(Arc::new(sender_raw));
    let receiver = Arc::new(RuntimeEventReceiver::default());
    let event_router = linkrpc::binding::InterfaceRouter::new();
    dbgjs::cdp::runtime_events::DOMAIN
        .register(
            &event_router,
            Arc::new(dbgjs::cdp::runtime_events::RuntimeEventsServer::new(
                receiver.clone(),
            )),
        )
        .unwrap();

    for (context, session_id) in [(1, None), (2, Some("child-session"))] {
        let transport = match session_id {
            None => mux.open_root().unwrap(),
            Some(id) => mux.open_session(id.to_owned()).unwrap(),
        };
        let channel = Channel::new(Box::new(transport), Box::new(RejectingHandler));
        let params = json!({
            "type": "log",
            "args": [],
            "executionContextId": context,
            "timestamp": 42.0,
        });
        CdpEventsClient::root(channel)
            .runtime()
            .console_apicalled(serde_json::from_value(params.clone()).unwrap())
            .await
            .unwrap();
        let envelope = tokio::time::timeout(
            std::time::Duration::from_secs(5),
            receiver_raw.recv(),
        )
        .await
        .expect("notification arrives")
        .unwrap();
        assert_eq!(envelope.session_id.as_deref(), session_id);
        let JsonRpcMessage::Notification(notification) = envelope.message else {
            panic!("expected a notification without a request id");
        };
        assert_eq!(notification.method, "Runtime.consoleAPICalled");
        let received_params = notification.params.expect("event has parameters");
        assert_eq!(received_params, params);
        event_router
            .dispatch_notification(&notification.method, received_params)
            .await
            .unwrap();
    }
    assert_eq!(*receiver.contexts.lock().unwrap(), vec![1, 2]);
}

#[tokio::test]
async fn command_and_event_implementations_register_on_separate_routers() {
    let command_router = linkrpc::binding::InterfaceRouter::new();
    dbgjs::cdp::runtime::DOMAIN
        .register(
            &command_router,
            Arc::new(dbgjs::cdp::runtime::RuntimeServer::new(Arc::new(
                DefaultRuntimeService,
            ))),
        )
        .unwrap();
    let event_router = linkrpc::binding::InterfaceRouter::new();
    dbgjs::cdp::runtime_events::DOMAIN
        .register(
            &event_router,
            Arc::new(dbgjs::cdp::runtime_events::RuntimeEventsServer::new(Arc::new(
                RuntimeEventReceiver::default(),
            ))),
        )
        .unwrap();
    for (router, method) in [
        (&command_router, "Runtime.executionContextsCleared"),
        (&event_router, "Runtime.enable"),
    ] {
        let error = InterfaceHandler::handle_request(router, method, json!({}), CallCtx::default())
            .await
            .unwrap_err();
        assert_eq!(error.code, -32601);
    }
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
        .target().attach_to_target(target_params)
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
        .runtime().evaluate(runtime_params)
        .await
        .expect("runtime evaluates");
    assert_eq!(runtime.result.r#type, RuntimeRemoteObjectType::Number);
    assert_eq!(runtime.result.value, Some(json!(42)));

    let breakpoint_params: DebuggerSetBreakpointByUrlParams =
        serde_json::from_value(json!({ "lineNumber": 0, "url": "file:///app.js" })).unwrap();
    let breakpoint = child_client
        .debugger().set_breakpoint_by_url(breakpoint_params)
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
