use super::*;
use std::future::Future;
use std::pin::Pin;

use linkrpc::prelude::{JsonRpcMessage, MessageTransport, TransportError};
use linkrpc::protocol::jsonrpc::{JsonRpcRequest, JsonRpcResponse, RequestId, ResponsePayload};
use linkrpc::transport::memory::{MemoryTransport, transport_pair_of};
use serde_json::json;

use crate::connection::transport::session_transport::CdpEnvelope;

struct ManagedMemoryTransport {
    transport: MemoryTransport<CdpEnvelope>,
    stall_close_send: bool,
}

#[async_trait]
impl MessageTransport<CdpEnvelope, CdpEnvelope> for ManagedMemoryTransport {
    async fn send(&self, message: CdpEnvelope) -> Result<(), TransportError> {
        let stall = self.stall_close_send
            && matches!(&message.message, JsonRpcMessage::Request(request) if request.method == "IO.close");
        self.transport.send(message).await?;
        if stall {
            std::future::pending().await
        } else {
            Ok(())
        }
    }

    async fn recv(&self) -> Option<CdpEnvelope> {
        self.transport.recv().await
    }
}

#[async_trait]
impl ManagedCdpTransport for ManagedMemoryTransport {
    fn close_reason(&self) -> Arc<Mutex<Option<String>>> {
        Arc::new(Mutex::new(None))
    }

    async fn wait_closed(&self) -> String {
        std::future::pending().await
    }

    async fn close(&self) {}
}

async fn resource_session() -> (
    CdpConnection,
    CdpDebuggerSession,
    MemoryTransport<CdpEnvelope>,
) {
    resource_session_with_stalled_close(false).await
}

async fn resource_session_with_stalled_close(
    stall_close_send: bool,
) -> (
    CdpConnection,
    CdpDebuggerSession,
    MemoryTransport<CdpEnvelope>,
) {
    let (client, browser) = transport_pair_of();
    let connection = CdpConnection::connect_root_debugger_transport(
        Arc::new(ManagedMemoryTransport {
            transport: client,
            stall_close_send,
        }),
        1,
        "resources".to_owned(),
    )
    .await
    .unwrap();
    let session = connection.take_root_debugger_session().unwrap();
    (connection, session, browser)
}

async fn next_request(browser: &MemoryTransport<CdpEnvelope>) -> JsonRpcRequest {
    let envelope = tokio::time::timeout(Duration::from_secs(2), browser.recv())
        .await
        .expect("CDP request must not hang")
        .unwrap();
    let JsonRpcMessage::Request(request) = envelope.message else {
        panic!("expected a CDP request");
    };
    request
}

async fn request_during_load<F>(
    browser: &MemoryTransport<CdpEnvelope>,
    load: Pin<&mut F>,
) -> JsonRpcRequest
where
    F: Future<Output = Result<Vec<u8>, CdpRuntimeError>>,
{
    tokio::select! {
        request = next_request(browser) => request,
        result = load => panic!("load ended before expected request: {result:?}"),
    }
}

async fn reply(browser: &MemoryTransport<CdpEnvelope>, request: JsonRpcRequest, result: Value) {
    browser
        .send(CdpEnvelope {
            session_id: None,
            message: JsonRpcMessage::Response(JsonRpcResponse {
                id: Some(request.id),
                payload: ResponsePayload::Result(result),
            }),
        })
        .await
        .unwrap();
}

#[tokio::test]
async fn source_map_resource_cancellation_before_send_keeps_transport_open() {
    let (transport, browser) = transport_pair_of::<JsonRpcMessage>();
    let resources = source_map_resources::SourceMapResources::new(transport);
    let mut params = json!({ "url": "https://test/app.js.map" });
    let guard = resources.register(&mut params).unwrap();
    drop(guard);
    let transport = resources.transport();
    transport
        .send(JsonRpcMessage::Request(JsonRpcRequest {
            id: RequestId::Number(1),
            method: "Network.loadNetworkResource".to_owned(),
            params: Some(params),
        }))
        .await
        .unwrap();
    transport
        .send(JsonRpcMessage::Request(JsonRpcRequest {
            id: RequestId::Number(2),
            method: "Runtime.enable".to_owned(),
            params: Some(json!({})),
        }))
        .await
        .unwrap();
    let message = browser.recv().await.unwrap();
    let JsonRpcMessage::Request(request) = message else {
        panic!("expected the non-cancelled request");
    };
    assert_eq!(request.method, "Runtime.enable");
}

#[tokio::test]
async fn source_map_resource_cancellation_during_pending_load_closes_late_stream() {
    let (connection, session, browser) = resource_session().await;
    let mut load =
        Box::pin(session.load_source_map_via_cdp("https://test/app.js.map", Some("frame")));
    let request = request_during_load(&browser, load.as_mut()).await;
    assert_eq!(request.method, "Network.loadNetworkResource");
    assert_eq!(
        request.params.as_ref().unwrap(),
        &json!({
            "url": "https://test/app.js.map",
            "frameId": "frame",
            "options": { "disableCache": false, "includeCredentials": true }
        })
    );
    drop(load);
    reply(
        &browser,
        request,
        json!({ "resource": { "success": true, "stream": "late" } }),
    )
    .await;
    let close = next_request(&browser).await;
    assert_eq!(close.method, "IO.close");
    assert_eq!(close.params.unwrap()["handle"], "late");
    connection.close().await;
}

#[tokio::test]
async fn source_map_resource_cancellation_during_read_closes_without_read_response() {
    let (connection, session, browser) = resource_session().await;
    let mut load =
        Box::pin(session.load_source_map_via_cdp("https://test/app.js.map", Some("frame")));
    let request = request_during_load(&browser, load.as_mut()).await;
    reply(
        &browser,
        request,
        json!({ "resource": { "success": true, "stream": "reading" } }),
    )
    .await;
    let read = request_during_load(&browser, load.as_mut()).await;
    assert_eq!(read.method, "IO.read");
    drop(load);
    let close = next_request(&browser).await;
    assert_eq!(close.method, "IO.close");
    assert_eq!(close.params.unwrap()["handle"], "reading");
    connection.close().await;
}

#[tokio::test]
async fn source_map_resource_cancellation_after_response_before_read_closes_stream() {
    let (connection, session, browser) = resource_session().await;
    let mut load =
        Box::pin(session.load_source_map_via_cdp("https://test/app.js.map", Some("frame")));
    let request = request_during_load(&browser, load.as_mut()).await;
    reply(
        &browser,
        request,
        json!({ "resource": { "success": true, "stream": "delivered" } }),
    )
    .await;
    let channel = session.channel.clone();
    let barrier = tokio::spawn(async move { channel.call("Runtime.enable", json!({})).await });
    let request = next_request(&browser).await;
    assert_eq!(request.method, "Runtime.enable");
    reply(&browser, request, json!({})).await;
    barrier.await.unwrap().unwrap();
    drop(load);
    let close = next_request(&browser).await;
    assert_eq!(close.method, "IO.close");
    assert_eq!(close.params.unwrap()["handle"], "delivered");
    connection.close().await;
}

#[tokio::test]
async fn source_map_resource_success_reads_and_closes_normally() {
    let (connection, session, browser) = resource_session().await;
    let mut load =
        Box::pin(session.load_source_map_via_cdp("https://test/app.js.map", Some("frame")));
    let request = request_during_load(&browser, load.as_mut()).await;
    reply(
        &browser,
        request,
        json!({ "resource": { "success": true, "stream": "normal" } }),
    )
    .await;
    let read = request_during_load(&browser, load.as_mut()).await;
    assert_eq!(read.method, "IO.read");
    reply(&browser, read, json!({ "data": "{}", "eof": true })).await;
    let close = request_during_load(&browser, load.as_mut()).await;
    assert_eq!(close.method, "IO.close");
    reply(&browser, close, json!({})).await;
    assert_eq!(load.await.unwrap(), b"{}");
    assert!(
        tokio::time::timeout(Duration::from_millis(20), browser.recv())
            .await
            .is_err()
    );
    connection.close().await;
}

#[tokio::test]
async fn source_map_resource_cleanup_send_tasks_are_bounded_and_time_out() {
    let (connection, session, browser) = resource_session_with_stalled_close(true).await;
    for index in 0..4 {
        let mut load =
            Box::pin(session.load_source_map_via_cdp("https://test/app.js.map", Some("frame")));
        let request = request_during_load(&browser, load.as_mut()).await;
        drop(load);
        reply(
            &browser,
            request,
            json!({ "resource": {
            "success": true, "stream": format!("stalled-close-{index}")
        } }),
        )
        .await;
        assert_eq!(next_request(&browser).await.method, "IO.close");
    }
    let result = session
        .load_source_map_via_cdp("https://test/app.js.map", Some("frame"))
        .await;
    assert!(
        result
            .unwrap_err()
            .to_string()
            .contains("resource limit reached")
    );
    tokio::time::timeout(Duration::from_secs(7), async {
        loop {
            if session
                .source_map_resources
                .register(&mut json!({}))
                .is_ok()
            {
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("stalled cleanup dispatch must release capacity within its deadline");
    connection.close().await;
}

#[tokio::test]
async fn source_map_resource_failed_load_still_closes_returned_stream() {
    let (connection, session, browser) = resource_session().await;
    let mut load =
        Box::pin(session.load_source_map_via_cdp("https://test/app.js.map", Some("frame")));
    let request = request_during_load(&browser, load.as_mut()).await;
    reply(
        &browser,
        request,
        json!({ "resource": {
        "success": false, "stream": "failed", "httpStatusCode": 404
    } }),
    )
    .await;
    assert!(matches!(
        load.await,
        Err(CdpRuntimeError::SourceMapLoadFailed { .. })
    ));
    let close = next_request(&browser).await;
    assert_eq!(close.method, "IO.close");
    assert_eq!(close.params.unwrap()["handle"], "failed");
    connection.close().await;
}

#[tokio::test]
async fn source_map_resource_pending_cancellations_are_bounded_and_recover_on_response() {
    let (connection, session, browser) = resource_session().await;
    let mut requests = Vec::new();
    for _ in 0..4 {
        let mut load =
            Box::pin(session.load_source_map_via_cdp("https://test/app.js.map", Some("frame")));
        requests.push(request_during_load(&browser, load.as_mut()).await);
        drop(load);
    }
    let result = session
        .load_source_map_via_cdp("https://test/app.js.map", Some("frame"))
        .await;
    assert!(
        result
            .unwrap_err()
            .to_string()
            .contains("resource limit reached")
    );
    for (index, request) in requests.into_iter().enumerate() {
        reply(
            &browser,
            request,
            json!({ "resource": {
            "success": true, "stream": format!("cancelled-{index}")
        } }),
        )
        .await;
        let close = next_request(&browser).await;
        assert_eq!(close.method, "IO.close");
        assert_eq!(
            close.params.unwrap()["handle"],
            format!("cancelled-{index}")
        );
    }
    tokio::task::yield_now().await;
    let mut load =
        Box::pin(session.load_source_map_via_cdp("https://test/app.js.map", Some("frame")));
    let request = request_during_load(&browser, load.as_mut()).await;
    assert_eq!(request.method, "Network.loadNetworkResource");
    reply(
        &browser,
        request,
        json!({ "resource": { "success": false } }),
    )
    .await;
    assert!(load.await.is_err());
    connection.close().await;
}
