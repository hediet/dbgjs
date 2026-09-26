use std::sync::Arc;

use dbgjs::cdp::{
    CdpClient, CdpEventsClient, DebuggerPausedParams, DebuggerScriptParsedParams,
    RuntimeRemoteObjectType, TargetAttachToTargetParams,
};
use dbgjs::connection::transport::session_transport::{CdpEnvelope, CdpSessionMux};
use linkrpc::connection::channel::{Channel, RejectingHandler};
use linkrpc::prelude::{
    CallCtx, InterfaceHandler, JsonRpcError, JsonRpcMessage, MessageTransport, RpcCallError,
};
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

#[tokio::test]
async fn generated_cdp_provider_default_errors_are_local() {
    let error = dbgjs::cdp::runtime::RuntimeService::enable(
        &DefaultRuntimeService,
        &CallCtx::default(),
    )
    .await
    .unwrap_err();
    assert_eq!(
        error,
        RpcCallError::Local(JsonRpcError::new(-32601, "enable")),
    );
}

#[tokio::test]
async fn generated_cdp_client_preserves_remote_error_origin_and_data() {
    let (client_raw, browser_raw) = transport_pair_of::<JsonRpcMessage>();
    let channel = Channel::new(Box::new(client_raw), Box::new(RejectingHandler));
    let client = CdpClient::root(channel.clone());
    let channel_loop = tokio::spawn(async move { channel.run().await });
    let wire_error = JsonRpcError {
        code: -32000,
        message: "Runtime.enable failed".into(),
        data: Some(json!({ "targetId": "target-1" })),
    };
    let expected_error = wire_error.clone();
    let browser = tokio::spawn(async move {
        let JsonRpcMessage::Request(request) = browser_raw.recv().await.unwrap() else {
            panic!("expected CDP request");
        };
        assert_eq!(request.method, "Runtime.enable");
        browser_raw
            .send(JsonRpcMessage::Response(JsonRpcResponse {
                id: Some(request.id),
                payload: ResponsePayload::Error(wire_error),
            }))
            .await
            .unwrap();
    });
    let error = tokio::time::timeout(
        std::time::Duration::from_secs(5),
        client.runtime().enable(),
    )
    .await
    .expect("remote error arrives")
    .unwrap_err();
    assert_eq!(error, RpcCallError::Remote(expected_error));
    browser.await.unwrap();
    channel_loop.abort();
}

#[derive(Default)]
struct RuntimeEventReceiver {
    contexts: std::sync::Mutex<Vec<i64>>,
    bindings: std::sync::Mutex<Vec<(String, String, i64)>>,
    clears: std::sync::atomic::AtomicUsize,
}

#[async_trait::async_trait]
impl dbgjs::cdp::runtime_events::RuntimeEventsService for RuntimeEventReceiver {
    async fn binding_called(
        &self,
        _ctx: &CallCtx,
        name: String,
        payload: String,
        execution_context_id: dbgjs::cdp::RuntimeExecutionContextId,
    ) -> Result<(), RpcCallError> {
        self.bindings
            .lock()
            .unwrap()
            .push((name, payload, execution_context_id));
        Ok(())
    }

    async fn execution_contexts_cleared(&self, _ctx: &CallCtx) -> Result<(), RpcCallError> {
        self.clears
            .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        Ok(())
    }

    async fn console_apicalled(
        &self,
        _ctx: &CallCtx,
        params: dbgjs::cdp::RuntimeConsoleApicalledParams,
    ) -> Result<(), RpcCallError> {
        self.contexts
            .lock()
            .unwrap()
            .push(params.execution_context_id);
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
        let events = CdpEventsClient::root(channel).runtime();
        events
            .console_apicalled(serde_json::from_value(params.clone()).unwrap())
            .await
            .unwrap();
        events
            .binding_called("bridge".into(), "payload".into(), context)
            .await
            .unwrap();
        events.execution_contexts_cleared().await.unwrap();
        for (method, params) in [
            ("Runtime.consoleAPICalled", params),
            (
                "Runtime.bindingCalled",
                json!({
                    "name": "bridge", "payload": "payload", "executionContextId": context,
                }),
            ),
            ("Runtime.executionContextsCleared", json!({})),
        ] {
            let envelope =
                tokio::time::timeout(std::time::Duration::from_secs(5), receiver_raw.recv())
                    .await
                    .expect("notification arrives")
                    .unwrap();
            assert_eq!(envelope.session_id.as_deref(), session_id);
            let JsonRpcMessage::Notification(notification) = envelope.message else {
                panic!("expected a notification without a request id");
            };
            assert_eq!(notification.method, method);
            let received_params = notification.params.expect("event has parameters");
            assert_eq!(received_params, params);
            event_router
                .dispatch_notification(&notification.method, received_params)
                .await
                .unwrap();
        }
    }
    assert_eq!(*receiver.contexts.lock().unwrap(), vec![1, 2]);
    assert_eq!(
        *receiver.bindings.lock().unwrap(),
        vec![
            ("bridge".into(), "payload".into(), 1),
            ("bridge".into(), "payload".into(), 2),
        ]
    );
    assert_eq!(receiver.clears.load(std::sync::atomic::Ordering::SeqCst), 2);
}

#[derive(Default)]
struct TargetCommandReceiver {
    attachments: std::sync::Mutex<Vec<(String, Option<bool>, Option<bool>)>>,
}

#[async_trait::async_trait]
impl dbgjs::cdp::target::TargetService for TargetCommandReceiver {
    async fn attach_to_target(
        &self,
        _ctx: &CallCtx,
        target_id: dbgjs::cdp::TargetTargetId,
        flatten: Option<bool>,
        dbgjs_auto_attach: Option<bool>,
    ) -> Result<dbgjs::cdp::TargetAttachToTargetResult, RpcCallError> {
        self.attachments
            .lock()
            .unwrap()
            .push((target_id, flatten, dbgjs_auto_attach));
        Ok(dbgjs::cdp::TargetAttachToTargetResult {
            session_id: "session".into(),
        })
    }
}

#[tokio::test]
async fn inline_command_receiver_preserves_renames_and_optional_defaults() {
    let receiver = Arc::new(TargetCommandReceiver::default());
    let router = linkrpc::binding::InterfaceRouter::new();
    dbgjs::cdp::target::DOMAIN
        .register(
            &router,
            Arc::new(dbgjs::cdp::target::TargetServer::new(receiver.clone())),
        )
        .unwrap();
    for params in [
        json!({ "targetId": "target-1", "flatten": true, "__dbgjsAutoAttach": false }),
        json!({ "targetId": "target-2" }),
    ] {
        assert_eq!(
            InterfaceHandler::handle_request(
                &router,
                "Target.attachToTarget",
                params,
                CallCtx::default(),
            )
            .await
            .unwrap(),
            json!({ "sessionId": "session" }),
        );
    }
    assert_eq!(
        *receiver.attachments.lock().unwrap(),
        vec![
            ("target-1".into(), Some(true), Some(false)),
            ("target-2".into(), None, None),
        ]
    );
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
            Arc::new(dbgjs::cdp::runtime_events::RuntimeEventsServer::new(
                Arc::new(RuntimeEventReceiver::default()),
            )),
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
            json!({}),
            json!({}),
            json!({ "result": { "type": "number", "value": 42 } }),
            (json!({ "breakpointId": "breakpoint-1", "locations": [] })),
        ] {
            let envelope = browser_raw.recv().await.expect("request arrives");
            let JsonRpcMessage::Request(request) = envelope.message else {
                panic!("expected CDP request");
            };
            observed.push((envelope.session_id.clone(), request.method, request.params));
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

    let target = client
        .target()
        .attach_to_target("target-1".into(), Some(true), Some(false))
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

    child_client.runtime().enable().await.unwrap();
    child_client.runtime().disable().await.unwrap();

    let runtime = child_client
        .runtime()
        .evaluate(
            "6 * 7".into(),
            None,
            None,
            None,
            None,
            Some(true),
            None,
            None,
            None,
            None,
            None,
            None,
            None,
            None,
            None,
            None,
        )
        .await
        .expect("runtime evaluates");
    assert_eq!(runtime.result.r#type, RuntimeRemoteObjectType::Number);
    assert_eq!(runtime.result.value, Some(json!(42)));

    let breakpoint = child_client
        .debugger()
        .set_breakpoint_by_url(0, Some("file:///app.js".into()), None, None, None, None)
        .await
        .expect("breakpoint installs");
    assert_eq!(breakpoint.breakpoint_id, "breakpoint-1");
    assert!(breakpoint.locations.is_empty());

    let observed = browser.await.expect("browser task completes");
    assert_eq!(
        observed,
        vec![
            (
                None,
                "Target.attachToTarget".into(),
                Some(json!({
                    "targetId": "target-1", "flatten": true, "__dbgjsAutoAttach": false,
                }))
            ),
            (
                Some("child-session".into()),
                "Runtime.enable".into(),
                Some(json!({}))
            ),
            (
                Some("child-session".into()),
                "Runtime.disable".into(),
                Some(json!({}))
            ),
            (
                Some("child-session".into()),
                "Runtime.evaluate".into(),
                Some(json!({
                    "expression": "6 * 7", "returnByValue": true,
                }))
            ),
            (
                Some("child-session".into()),
                "Debugger.setBreakpointByUrl".into(),
                Some(json!({ "lineNumber": 0, "url": "file:///app.js" })),
            ),
        ]
    );
}
