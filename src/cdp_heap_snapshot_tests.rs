use super::*;
use linkrpc::prelude::JsonRpcMessage;
use linkrpc::protocol::jsonrpc::{JsonRpcNotification, JsonRpcResponse, ResponsePayload};
use linkrpc::transport::memory::{MemoryTransport, transport_pair_of};
use serde_json::json;

fn heap_session() -> (CdpDebuggerSession, MemoryTransport) {
    let (transport, browser) = transport_pair_of();
    let source_map_resources = SourceMapResources::new(transport);
    let session = SessionKey {
        connection_generation: 1,
        session_id: "heap-test".into(),
    };
    let (sender, events) = mpsc::unbounded_channel();
    let heap_snapshot = Arc::new(Mutex::new(None));
    let (heap_snapshot_progress, _) = watch::channel(None);
    let (raw_events, _) = broadcast::channel(RAW_EVENT_BUFFER);
    let raw_event_history = Arc::new(std::sync::Mutex::new(Vec::new()));
    let channel = CdpEventHandler {
        session: session.clone(),
        sender,
        heap_snapshot: heap_snapshot.clone(),
        heap_snapshot_progress: heap_snapshot_progress.clone(),
        raw_events: raw_events.clone(),
        raw_event_history: raw_event_history.clone(),
    }
    .into_channel(source_map_resources.transport());
    (
        CdpDebuggerSession {
            session,
            client: CdpClient::root(channel.clone()),
            channel,
            events,
            source_map_frame_id: Mutex::new(None),
            source_map_resources,
            source_map_cache_enabled: AtomicBool::new(true),
            source_map_cache_hits: AtomicU64::new(0),
            source_map_cache_misses: AtomicU64::new(0),
            source_map_cache_bypasses: AtomicU64::new(0),
            heap_snapshot,
            heap_snapshot_progress,
            raw_events,
            raw_event_history,
        },
        browser,
    )
}

async fn notification(browser: &MemoryTransport, method: &str, params: Value) {
    browser
        .send(JsonRpcMessage::Notification(JsonRpcNotification {
            method: method.into(),
            params: Some(params),
        }))
        .await
        .unwrap();
}

#[tokio::test]
async fn heap_snapshot_response_cannot_overtake_blocked_chunks() {
    let (session, browser) = heap_session();
    let directory = std::env::temp_dir().join(format!(
        "dbgjs-heap-completion-{}-{}",
        std::process::id(),
        SystemTime::now()
            .duration_since(SystemTime::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    let destination = directory.join("snapshot.heapsnapshot");
    session
        .begin_heap_snapshot(destination.clone())
        .await
        .unwrap();
    let mut raw_events = session.raw_events.subscribe();
    let heap_profiler = session.client.heap_profiler();
    let mut request = Box::pin(heap_profiler.take_heap_snapshot(None, None, None, None));
    assert!(futures_util::poll!(request.as_mut()).is_pending());
    let JsonRpcMessage::Request(sent) = browser.recv().await.unwrap() else {
        panic!("expected takeHeapSnapshot request");
    };
    assert_eq!(sent.method, "HeapProfiler.takeHeapSnapshot");
    let blocked_writer = session.heap_snapshot.lock().await;
    notification(
        &browser,
        "HeapProfiler.reportHeapSnapshotProgress",
        json!({"done": 10, "total": 10, "finished": true}),
    )
    .await;
    let chunks = [r#"{"strings":[""#, "PieceTree", r#"TextBuffer"]}"#];
    for chunk in chunks {
        notification(
            &browser,
            "HeapProfiler.addHeapSnapshotChunk",
            json!({"chunk": chunk}),
        )
        .await;
    }
    browser
        .send(JsonRpcMessage::Response(JsonRpcResponse {
            id: Some(sent.id),
            payload: ResponsePayload::Result(json!({})),
        }))
        .await
        .unwrap();

    let mut run = Box::pin(session.channel.run());
    assert!(futures_util::poll!(run.as_mut()).is_pending());
    let premature_response = futures_util::poll!(request.as_mut()).is_ready();
    drop(blocked_writer);
    if premature_response {
        drop(run);
        session.abort_heap_snapshot().await;
        tokio::fs::remove_dir_all(&directory).await.unwrap();
        panic!("takeHeapSnapshot responded while chunk writes were blocked");
    }
    tokio::select! {
        result = &mut request => result.unwrap(),
        _ = &mut run => panic!("channel stopped before the heap response"),
    };
    drop(run);
    let result = session.finish_heap_snapshot().await.unwrap();
    let content = tokio::fs::read(&destination).await.unwrap();
    tokio::fs::remove_dir_all(&directory).await.unwrap();

    assert_eq!(content, chunks.concat().as_bytes());
    assert_eq!(result.bytes_written, content.len() as u64);
    assert_eq!(
        serde_json::from_slice::<Value>(&content).unwrap(),
        json!({"strings": ["PieceTreeTextBuffer"]})
    );
    assert_eq!(
        raw_events.try_recv().unwrap().method,
        "HeapProfiler.reportHeapSnapshotProgress"
    );
    for chunk in chunks {
        assert_eq!(
            raw_events.try_recv().unwrap().params,
            json!({"chunk": chunk})
        );
    }
    assert!(raw_events.try_recv().is_err());
}

#[tokio::test]
async fn heap_snapshot_chunk_errors_prevent_publication_after_successful_response() {
    let (session, browser) = heap_session();
    let directory = std::env::temp_dir().join(format!(
        "dbgjs-heap-chunk-error-{}-{}",
        std::process::id(),
        SystemTime::now()
            .duration_since(SystemTime::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    tokio::fs::create_dir_all(&directory).await.unwrap();
    let destination = directory.join("snapshot.heapsnapshot");
    let previous = br#"{"strings":["previous"]}"#;
    tokio::fs::write(&destination, previous).await.unwrap();
    session
        .begin_heap_snapshot(destination.clone())
        .await
        .unwrap();
    let temporary = session
        .heap_snapshot
        .lock()
        .await
        .as_ref()
        .unwrap()
        .temporary
        .clone();
    let heap_profiler = session.client.heap_profiler();
    let mut request = Box::pin(heap_profiler.take_heap_snapshot(None, None, None, None));
    assert!(futures_util::poll!(request.as_mut()).is_pending());
    let JsonRpcMessage::Request(sent) = browser.recv().await.unwrap() else {
        panic!("expected takeHeapSnapshot request");
    };
    notification(
        &browser,
        "HeapProfiler.addHeapSnapshotChunk",
        json!({"chunk": "{\"strings\":["}),
    )
    .await;
    notification(
        &browser,
        "HeapProfiler.addHeapSnapshotChunk",
        json!({"chunk": 42}),
    )
    .await;
    browser
        .send(JsonRpcMessage::Response(JsonRpcResponse {
            id: Some(sent.id),
            payload: ResponsePayload::Result(json!({})),
        }))
        .await
        .unwrap();
    let mut run = Box::pin(session.channel.run());
    tokio::select! {
        result = &mut request => result.unwrap(),
        _ = &mut run => panic!("channel stopped before the heap response"),
    };
    drop(run);
    let error = session.finish_heap_snapshot_bytes().await.unwrap_err();
    assert!(
        error
            .to_string()
            .contains("HeapProfiler.addHeapSnapshotChunk")
    );
    assert_eq!(tokio::fs::read(&destination).await.unwrap(), previous);
    assert!(!temporary.exists());
    tokio::fs::remove_dir_all(&directory).await.unwrap();
}
