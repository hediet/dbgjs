use super::*;
use crate::connection::providers::{ProviderTargetEvent, raw_session_tests};
use crate::connection::transport::cdp_transport::ManagedCdpTransport;
use crate::connection::transport::session_transport::CdpEnvelope;
use linkrpc::prelude::{JsonRpcMessage, MessageTransport, TransportError};
use linkrpc::protocol::jsonrpc::{JsonRpcResponse, ResponsePayload};
use std::sync::atomic::{AtomicUsize, Ordering};
use tokio::sync::{Notify, Semaphore, mpsc};

struct TestTransport {
    sender: mpsc::UnboundedSender<CdpEnvelope>,
    receiver: Mutex<mpsc::UnboundedReceiver<CdpEnvelope>>,
    close_reason: Arc<Mutex<Option<String>>>,
    closed: Notify,
    started: Notify,
    detached: Notify,
    gate: Arc<Semaphore>,
    attaches: AtomicUsize,
    fail: bool,
}

impl TestTransport {
    fn new(blocked: bool, fail: bool) -> Arc<Self> {
        let (sender, receiver) = mpsc::unbounded_channel();
        Arc::new(Self {
            sender,
            receiver: Mutex::new(receiver),
            close_reason: Arc::new(Mutex::new(None)),
            closed: Notify::new(),
            started: Notify::new(),
            detached: Notify::new(),
            gate: Arc::new(Semaphore::new(usize::from(!blocked))),
            attaches: AtomicUsize::new(0),
            fail,
        })
    }
}

#[async_trait::async_trait]
impl MessageTransport<CdpEnvelope, CdpEnvelope> for TestTransport {
    async fn send(&self, envelope: CdpEnvelope) -> Result<(), TransportError> {
        let JsonRpcMessage::Request(request) = envelope.message else {
            return Ok(());
        };
        let attach = request.method == "Target.attachToTarget";
        if request.method == "Target.detachFromTarget" {
            self.detached.notify_one();
        }
        let index = if attach {
            let index = self.attaches.fetch_add(1, Ordering::SeqCst);
            self.started.notify_one();
            index
        } else {
            0
        };
        let sender = self.sender.clone();
        let gate = self.gate.clone();
        let fail = self.fail;
        tokio::spawn(async move {
            if attach {
                let _permit = gate.acquire().await.unwrap();
            }
            let payload = if attach && fail {
                ResponsePayload::Error(internal_error("injected attach failure"))
            } else {
                ResponsePayload::Result(if attach {
                    serde_json::json!({"sessionId": format!("session-{index}")})
                } else if request.method == "Debugger.enable" {
                    serde_json::json!({"debuggerId": "test-debugger"})
                } else {
                    serde_json::json!({})
                })
            };
            let _ = sender.send(CdpEnvelope {
                session_id: envelope.session_id,
                message: JsonRpcMessage::Response(JsonRpcResponse {
                    id: Some(request.id),
                    payload,
                }),
            });
        });
        Ok(())
    }

    async fn recv(&self) -> Option<CdpEnvelope> {
        tokio::select! {
            message = async { self.receiver.lock().await.recv().await } => message,
            _ = self.closed.notified() => None,
        }
    }
}

#[async_trait::async_trait]
impl ManagedCdpTransport for TestTransport {
    fn close_reason(&self) -> Arc<Mutex<Option<String>>> {
        self.close_reason.clone()
    }

    async fn wait_closed(&self) -> String {
        if self.close_reason.lock().await.is_none() {
            self.closed.notified().await;
        }
        "test transport closed".into()
    }

    async fn close(&self) {
        *self.close_reason.lock().await = Some("test transport closed".into());
        self.closed.notify_waiters();
    }
}

fn target(id: &str) -> TargetSnapshot {
    TargetSnapshot {
        target_id: id.into(),
        target_type: "page".into(),
        title: id.into(),
        url: format!("https://{id}.test"),
        attached: false,
        parent_id: None,
        opener_id: None,
        browser_context_id: None,
        subtype: None,
    }
}

async fn fixture(
    connections: &[(&'static str, Arc<TestTransport>)],
) -> (DebuggerService, Vec<Arc<ConnectionRuntime>>) {
    let mut state = ServiceState::default();
    super::tests::insert_context_with_targets(
        &mut state,
        "ctx",
        connections
            .iter()
            .map(|(id, _)| (*id, 1, vec![target("page")])),
    );
    let context = state.contexts["ctx"].clone();
    let mut runtimes = Vec::new();
    for (id, transport) in connections {
        let runtime = raw_session_tests::runtime_with_transport(1, transport.clone()).await;
        stage_connection_resource_graph(
            &mut state,
            "ctx",
            id,
            &context,
            &BTreeMap::from([("page".into(), target("page"))]),
            &runtime,
            &[],
        )
        .unwrap();
        state
            .runtimes
            .insert(("ctx".into(), (*id).into()), runtime.clone());
        runtimes.push(runtime);
    }
    (
        super::tests::service_with_state(PathBuf::from("unused"), state),
        runtimes,
    )
}

fn attach(
    service: &DebuggerService,
    connection: &'static str,
) -> tokio::task::JoinHandle<Result<TargetAttachmentResult, JsonRpcError>> {
    let service = service.clone();
    tokio::spawn(async move {
        service
            .attach_target(
                &CallCtx::default(),
                crate::api::service_api::TargetRef {
                    connection: crate::api::service_api::ConnectionRef {
                        context_id: "ctx".into(),
                        connection_id: connection.into(),
                    },
                    target_id: "page".into(),
                },
                TargetAttachOptions::default(),
            )
            .await
            .map_err(|error| internal_error(error.to_string()))
    })
}

async fn bounded<F: std::future::Future>(future: F) -> F::Output {
    tokio::time::timeout(Duration::from_secs(2), future)
        .await
        .expect("lifecycle operation stalled")
}

fn target_ref(selector: &str) -> crate::api::service_api::TargetRef {
    crate::api::service_api::TargetRef {
        connection: crate::api::service_api::ConnectionRef {
            context_id: "ctx".into(),
            connection_id: "conn".into(),
        },
        target_id: selector.into(),
    }
}

async fn poll_queued<F: std::future::Future>(mut future: std::pin::Pin<&mut F>) {
    std::future::poll_fn(|cx| {
        assert!(
            future.as_mut().poll(cx).is_pending(),
            "operation must queue behind the held target lock"
        );
        std::task::Poll::Ready(())
    })
    .await;
}

async fn replace_targets(
    service: &DebuggerService,
    runtime: &Arc<ConnectionRuntime>,
    targets: Vec<TargetSnapshot>,
) {
    let mut state = service.state.lock().await;
    let context = state.contexts["ctx"].clone();
    stage_connection_resource_graph(
        &mut state,
        "ctx",
        "conn",
        &context,
        &targets
            .into_iter()
            .map(|target| (target.target_id.clone(), target))
            .collect(),
        runtime,
        &[],
    )
    .unwrap();
}

#[tokio::test]
async fn queued_title_attach_keeps_resolved_target_when_selector_moves() {
    let transport = TestTransport::new(false, false);
    let (service, runtimes) = fixture(&[("conn", transport)]).await;
    let mut page = target("page");
    page.title = "wanted".into();
    let mut other = target("other");
    replace_targets(&service, &runtimes[0], vec![page.clone(), other.clone()]).await;
    let page_lock = service
        .target_attachment_lock("ctx", "conn", "page")
        .await
        .unwrap();
    let page_guard = page_lock.lock().await;
    let other_lock = service
        .target_attachment_lock("ctx", "conn", "other")
        .await
        .unwrap();
    let other_guard = other_lock.lock().await;
    let call = CallCtx::default();
    let mut queued = Box::pin(service.attach_target(
        &call,
        target_ref("wanted"),
        TargetAttachOptions::default(),
    ));
    poll_queued(queued.as_mut()).await;
    page.title = "original".into();
    other.title = "wanted".into();
    replace_targets(&service, &runtimes[0], vec![page, other]).await;
    drop(page_guard);
    let attached = bounded(queued).await.unwrap();
    assert_eq!(
        attached.target.target_id, "page",
        "selector moved while queued; the operation must not attach B while holding A's lock"
    );
    drop(other_guard);
    runtimes[0].close().await;
}

#[tokio::test]
async fn queued_url_detach_keeps_resolved_target_when_selector_moves() {
    let transport = TestTransport::new(false, false);
    let (service, runtimes) = fixture(&[("conn", transport)]).await;
    let mut page = target("page");
    page.url = "https://needle.test".into();
    let mut other = target("other");
    replace_targets(&service, &runtimes[0], vec![page.clone(), other.clone()]).await;
    let call = CallCtx::default();
    service
        .attach_target(&call, target_ref("page"), TargetAttachOptions::default())
        .await
        .unwrap();
    service
        .attach_target(&call, target_ref("other"), TargetAttachOptions::default())
        .await
        .unwrap();
    let page_lock = service
        .target_attachment_lock("ctx", "conn", "page")
        .await
        .unwrap();
    let page_guard = page_lock.lock().await;
    let other_lock = service
        .target_attachment_lock("ctx", "conn", "other")
        .await
        .unwrap();
    let other_guard = other_lock.lock().await;
    let mut queued = Box::pin(service.detach_target(&call, target_ref("needle.test"), None));
    poll_queued(queued.as_mut()).await;
    page.url = "https://original.test".into();
    other.url = "https://needle.test".into();
    replace_targets(&service, &runtimes[0], vec![page, other]).await;
    drop(page_guard);
    bounded(queued).await.unwrap();
    let state = service.state.lock().await;
    assert!(
        !state
            .target_debuggers
            .contains_key(&("ctx".into(), "conn".into(), "page".into())),
        "queued detach must release A, not newly matching B"
    );
    assert!(
        state
            .target_debuggers
            .contains_key(&("ctx".into(), "conn".into(), "other".into()))
    );
    drop(state);
    drop(other_guard);
    runtimes[0].close().await;
}

#[tokio::test]
async fn queued_attach_rejects_reconnect_even_when_physical_resource_is_unchanged() {
    let transport = TestTransport::new(false, false);
    let (service, runtimes) = fixture(&[("conn", transport.clone())]).await;
    let lock = service
        .target_attachment_lock("ctx", "conn", "page")
        .await
        .unwrap();
    let guard = lock.lock().await;
    let call = CallCtx::default();
    let mut queued =
        Box::pin(service.attach_target(&call, target_ref("page"), TargetAttachOptions::default()));
    poll_queued(queued.as_mut()).await;
    let replacement_transport = TestTransport::new(false, false);
    let replacement =
        raw_session_tests::runtime_with_transport(2, replacement_transport.clone()).await;
    {
        let mut state = service.state.lock().await;
        super::tests::insert_context_with_targets(
            &mut state,
            "ctx",
            [("conn", 2, vec![target("page")])],
        );
        let context = state.contexts["ctx"].clone();
        stage_connection_resource_graph(
            &mut state,
            "ctx",
            "conn",
            &context,
            &BTreeMap::from([("page".into(), target("page"))]),
            &replacement,
            &[],
        )
        .unwrap();
        state
            .runtimes
            .insert(("ctx".into(), "conn".into()), replacement.clone());
    }
    drop(guard);
    let result = bounded(queued).await;
    assert!(
        result.is_err(),
        "queued generation-1 request attached to generation 2: {result:?}"
    );
    assert_eq!(transport.attaches.load(Ordering::SeqCst), 0);
    assert_eq!(replacement_transport.attaches.load(Ordering::SeqCst), 0);
    replacement.close().await;
    runtimes[0].close().await;
}

#[tokio::test]
async fn queued_attach_rejects_changed_physical_identity_or_target_incarnation() {
    for recreated in [false, true] {
        let transport = TestTransport::new(false, false);
        let (service, runtimes) = fixture(&[("conn", transport.clone())]).await;
        let lock = service
            .target_attachment_lock("ctx", "conn", "page")
            .await
            .unwrap();
        let guard = lock.lock().await;
        let call = CallCtx::default();
        let mut queued = Box::pin(service.attach_target(
            &call,
            target_ref("page"),
            TargetAttachOptions::default(),
        ));
        poll_queued(queued.as_mut()).await;
        let mut replacement = target("page");
        if recreated {
            replace_targets(&service, &runtimes[0], Vec::new()).await;
        } else {
            replacement.subtype = Some("electron-renderer".into());
        }
        replace_targets(&service, &runtimes[0], vec![replacement]).await;
        drop(guard);
        let result = bounded(queued).await;
        assert!(
            result.is_err(),
            "queued request lost its physical/incarnation identity (recreated={recreated}): {result:?}"
        );
        assert_eq!(transport.attaches.load(Ordering::SeqCst), 0);
        runtimes[0].close().await;
    }
}

#[tokio::test]
async fn provider_removal_is_not_blocked_by_pending_attach() {
    let transport = TestTransport::new(true, false);
    let (service, runtimes) = fixture(&[("conn", transport.clone())]).await;
    {
        let mut state = service.state.lock().await;
        let context = state.contexts["ctx"].clone();
        stage_connection_resource_graph(
            &mut state,
            "ctx",
            "conn",
            &context,
            &BTreeMap::new(),
            &runtimes[0],
            &[],
        )
        .unwrap();
    }
    service
        .supervise_provider_target_events("ctx".into(), "conn".into(), 1, 1, runtimes[0].clone())
        .await;
    let mut updated = target("page");
    updated.title = "updated".into();
    raw_session_tests::emit(&runtimes[0], ProviderTargetEvent::Upsert(updated));
    bounded(transport.started.notified()).await;
    raw_session_tests::emit(&runtimes[0], ProviderTargetEvent::Removed("page".into()));
    bounded(async {
        loop {
            if !context_connection_has_target(
                &*service.state.lock().await,
                "ctx",
                "conn",
                1,
                "page",
            ) {
                break;
            }
            tokio::task::yield_now().await;
        }
    })
    .await;
    transport.gate.add_permits(1);
    bounded(transport.detached.notified()).await;
    assert!(service.state.lock().await.target_debuggers.is_empty());
    runtimes[0].close().await;
}

#[tokio::test]
async fn pending_attach_does_not_publish_for_reused_target_id() {
    let transport = TestTransport::new(true, false);
    let (service, runtimes) = fixture(&[("conn", transport.clone())]).await;
    let pending = attach(&service, "conn");
    bounded(transport.started.notified()).await;
    {
        let mut state = service.state.lock().await;
        let context = state.contexts["ctx"].clone();
        stage_connection_resource_graph(
            &mut state,
            "ctx",
            "conn",
            &context,
            &BTreeMap::new(),
            &runtimes[0],
            &[],
        )
        .unwrap();
        stage_connection_resource_graph(
            &mut state,
            "ctx",
            "conn",
            &context,
            &BTreeMap::from([("page".into(), target("page"))]),
            &runtimes[0],
            &[],
        )
        .unwrap();
    }
    transport.gate.add_permits(1);
    let result = bounded(pending).await.unwrap();
    assert!(
        result.is_err(),
        "old incarnation attached to reused target: {result:?}"
    );
    assert!(service.state.lock().await.target_debuggers.is_empty());
    runtimes[0].close().await;
}

#[tokio::test]
async fn responsive_fixture_can_attach() {
    let transport = TestTransport::new(false, false);
    let (service, runtimes) = fixture(&[("conn", transport)]).await;
    let result = bounded(attach(&service, "conn")).await.unwrap();
    assert!(result.is_ok(), "{result:?}");
    runtimes[0].close().await;
}

#[tokio::test]
async fn discovery_metadata_update_does_not_undo_explicit_detach() {
    let transport = TestTransport::new(false, false);
    let (service, runtimes) = fixture(&[("conn", transport.clone())]).await;
    bounded(attach(&service, "conn")).await.unwrap().unwrap();
    transport.started.notified().await;
    service
        .detach_target(
            &CallCtx::default(),
            crate::api::service_api::TargetRef {
                connection: crate::api::service_api::ConnectionRef {
                    context_id: "ctx".into(),
                    connection_id: "conn".into(),
                },
                target_id: "page".into(),
            },
            Some(1),
        )
        .await
        .unwrap();
    service
        .supervise_provider_target_events("ctx".into(), "conn".into(), 1, 1, runtimes[0].clone())
        .await;
    let mut updated = target("page");
    updated.title = "new title".into();
    raw_session_tests::emit(&runtimes[0], ProviderTargetEvent::Upsert(updated));
    bounded(async {
        loop {
            if context_connection_target(&*service.state.lock().await, "ctx", "conn", 1, "page")
                .is_some_and(|target| target.target.title == "new title")
            {
                break;
            }
            tokio::task::yield_now().await;
        }
    })
    .await;
    assert!(
        tokio::time::timeout(Duration::from_millis(100), transport.started.notified())
            .await
            .is_err(),
        "discovery must not reattach an explicitly detached target on a metadata update"
    );
    assert!(service.state.lock().await.target_debuggers.is_empty());
    runtimes[0].close().await;
}

#[tokio::test]
async fn pending_attach_rejects_disconnected_and_reconnected_generation() {
    let transport = TestTransport::new(true, false);
    let (service, runtimes) = fixture(&[("conn", transport.clone())]).await;
    let pending = attach(&service, "conn");
    bounded(transport.started.notified()).await;
    service
        .disconnect_connection(
            &CallCtx::default(),
            crate::api::service_api::ConnectionRef {
                context_id: "ctx".into(),
                connection_id: "conn".into(),
            },
        )
        .await
        .unwrap();
    let replacement = raw_session_tests::runtime(2).await;
    {
        let mut state = service.state.lock().await;
        super::tests::insert_context_with_targets(
            &mut state,
            "ctx",
            [("conn", 2, vec![target("page")])],
        );
        state
            .runtimes
            .insert(("ctx".into(), "conn".into()), replacement.clone());
    }
    transport.gate.add_permits(1);
    assert!(bounded(pending).await.unwrap().is_err());
    assert!(service.state.lock().await.target_debuggers.is_empty());
    replacement.close().await;
    runtimes[0].close().await;
}

#[tokio::test]
async fn failing_pending_attach_does_not_block_another_connection() {
    let first = TestTransport::new(true, true);
    let second = TestTransport::new(false, false);
    let (service, runtimes) = fixture(&[("first", first.clone()), ("second", second)]).await;
    let pending = attach(&service, "first");
    bounded(first.started.notified()).await;
    let independent = bounded(attach(&service, "second")).await.unwrap().unwrap();
    assert_eq!(independent.target.connection_id, "second");
    first.gate.add_permits(1);
    assert!(bounded(pending).await.unwrap().is_err());
    assert!(service.state.lock().await.target_debuggers.contains_key(&(
        "ctx".into(),
        "second".into(),
        "page".into()
    )));
    for runtime in runtimes {
        runtime.close().await;
    }
}

#[tokio::test]
async fn metadata_update_preserves_pending_attachment_incarnation() {
    let transport = TestTransport::new(true, false);
    let (service, runtimes) = fixture(&[("conn", transport.clone())]).await;
    let pending = attach(&service, "conn");
    bounded(transport.started.notified()).await;
    {
        let mut state = service.state.lock().await;
        let context = state.contexts["ctx"].clone();
        let mut updated = target("page");
        updated.title = "new title".into();
        stage_connection_resource_graph(
            &mut state,
            "ctx",
            "conn",
            &context,
            &BTreeMap::from([("page".into(), updated)]),
            &runtimes[0],
            &[],
        )
        .unwrap();
    }
    transport.gate.add_permits(1);
    bounded(pending).await.unwrap().unwrap();
    runtimes[0].close().await;
}

#[tokio::test]
async fn concurrent_same_target_attach_reuses_one_session() {
    let transport = TestTransport::new(true, false);
    let (service, runtimes) = fixture(&[("conn", transport.clone())]).await;
    let first = attach(&service, "conn");
    bounded(transport.started.notified()).await;
    let second = attach(&service, "conn");
    transport.gate.add_permits(1);
    assert_eq!(
        bounded(first).await.unwrap().unwrap().outcome,
        TargetAttachmentOutcome::Created
    );
    assert_eq!(
        bounded(second).await.unwrap().unwrap().outcome,
        TargetAttachmentOutcome::Reused
    );
    assert_eq!(transport.attaches.load(Ordering::SeqCst), 1);
    runtimes[0].close().await;
}

#[tokio::test]
async fn explicit_detach_serializes_after_pending_attach() {
    let transport = TestTransport::new(true, false);
    let (service, runtimes) = fixture(&[("conn", transport.clone())]).await;
    let pending = attach(&service, "conn");
    bounded(transport.started.notified()).await;
    let detach = {
        let service = service.clone();
        tokio::spawn(async move {
            service
                .detach_target(
                    &CallCtx::default(),
                    crate::api::service_api::TargetRef {
                        connection: crate::api::service_api::ConnectionRef {
                            context_id: "ctx".into(),
                            connection_id: "conn".into(),
                        },
                        target_id: "page".into(),
                    },
                    Some(1),
                )
                .await
        })
    };
    transport.gate.add_permits(1);
    bounded(pending).await.unwrap().unwrap();
    bounded(detach).await.unwrap().unwrap();
    assert!(service.state.lock().await.target_debuggers.is_empty());
    runtimes[0].close().await;
}

#[tokio::test]
async fn old_provider_events_cannot_remove_or_replace_current_generation() {
    let old_transport = TestTransport::new(false, false);
    let (service, old_runtimes) = fixture(&[("conn", old_transport)]).await;
    service
        .supervise_provider_target_events(
            "ctx".into(),
            "conn".into(),
            1,
            1,
            old_runtimes[0].clone(),
        )
        .await;
    let replacement = raw_session_tests::runtime(2).await;
    {
        let mut state = service.state.lock().await;
        super::tests::insert_context_with_targets(
            &mut state,
            "ctx",
            [("conn", 2, vec![target("page")])],
        );
        state
            .runtimes
            .insert(("ctx".into(), "conn".into()), replacement.clone());
    }
    raw_session_tests::emit(
        &old_runtimes[0],
        ProviderTargetEvent::Removed("page".into()),
    );
    bounded(raw_session_tests::wait_events_closed(&old_runtimes[0])).await;
    let state = service.state.lock().await;
    assert!(context_connection_has_target(
        &state, "ctx", "conn", 2, "page"
    ));
    assert!(Arc::ptr_eq(
        &state.runtimes[&("ctx".into(), "conn".into())],
        &replacement
    ));
    drop(state);
    old_runtimes[0].close().await;
    replacement.close().await;
}

#[tokio::test]
async fn physical_target_ownership_is_serialized_across_connection_aliases() {
    let first = TestTransport::new(true, false);
    let second = TestTransport::new(false, false);
    let (service, runtimes) =
        fixture(&[("first", first.clone()), ("second", second.clone())]).await;
    {
        let mut state = service.state.lock().await;
        let mut context = (*state.contexts["ctx"]).clone();
        let mut connections = (*context.connections).clone();
        let mut connection = (*connections["second"]).clone();
        connection.configuration = connections["first"].configuration.clone();
        connections.insert("second".into(), Arc::new(connection));
        context.connections = Arc::new(connections);
        let context = Arc::new(context);
        state.contexts.insert("ctx".into(), context.clone());
        stage_connection_resource_graph(
            &mut state,
            "ctx",
            "second",
            &context,
            &BTreeMap::from([("page".into(), target("page"))]),
            &runtimes[1],
            &[],
        )
        .unwrap();
    }
    let pending = attach(&service, "first");
    bounded(first.started.notified()).await;
    let contender = attach(&service, "second");
    first.gate.add_permits(1);
    bounded(pending).await.unwrap().unwrap();
    let error = bounded(contender).await.unwrap().unwrap_err();
    assert!(error.message.contains("ownership conflict"), "{error:?}");
    assert_eq!(second.attaches.load(Ordering::SeqCst), 0);
    for runtime in runtimes {
        runtime.close().await;
    }
}

#[tokio::test]
async fn pending_public_attach_keeps_relay_registration_exclusive() {
    let transport = TestTransport::new(true, false);
    let (service, runtimes) = fixture(&[("conn", transport.clone())]).await;
    let pending = attach(&service, "conn");
    bounded(transport.started.notified()).await;
    assert!(service.relay_lifecycle_lock.try_write().is_err());
    transport.gate.add_permits(1);
    bounded(pending).await.unwrap().unwrap();
    let guard = bounded(service.relay_lifecycle_lock.write()).await;
    drop(guard);
    runtimes[0].close().await;
}

#[tokio::test]
async fn force_reconnect_refreshes_generation_without_relocking_same_physical_target() {
    use futures_util::SinkExt;
    use tokio_tungstenite::tungstenite::Message;

    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let endpoint = format!("ws://{}", listener.local_addr().unwrap());
    let server = tokio::spawn(async move {
        let mut handlers = Vec::new();
        for _ in 0..2 {
            let (socket, _) = listener.accept().await.unwrap();
            handlers.push(tokio::spawn(async move {
                let mut socket = tokio_tungstenite::accept_async(socket).await.unwrap();
                while let Some(Ok(Message::Text(text))) = socket.next().await {
                    let request: serde_json::Value = serde_json::from_str(&text).unwrap();
                    let result = if request["method"] == "Debugger.enable" {
                        serde_json::json!({"debuggerId": "test"})
                    } else {
                        serde_json::json!({})
                    };
                    socket
                        .send(Message::Text(
                            serde_json::json!({
                                "id": request["id"], "result": result,
                            })
                            .to_string()
                            .into(),
                        ))
                        .await
                        .unwrap();
                }
            }));
        }
        for handler in handlers {
            handler.await.unwrap();
        }
    });
    let mut state = ServiceState::default();
    super::tests::insert_context_with_targets(&mut state, "ctx", [("conn", 0, Vec::new())]);
    let mut context = (*state.contexts["ctx"]).clone();
    let mut connections = (*context.connections).clone();
    let mut connection = (*connections["conn"]).clone();
    connection.configuration = ConnectionConfiguration::NodeInspector { endpoint };
    connection.status = ConnectionStatus::Disconnected;
    connections.insert("conn".into(), Arc::new(connection));
    context.connections = Arc::new(connections);
    state.contexts.insert("ctx".into(), Arc::new(context));
    let service = super::tests::service_with_state(PathBuf::from("unused"), state);
    let call = CallCtx::default();
    let reference = target_ref(&synthetic_node_target_id("conn"));
    bounded(service.connect_connection(&call, reference.connection.clone()))
        .await
        .unwrap();
    let original =
        bounded(service.attach_target(&call, reference.clone(), TargetAttachOptions::default()))
            .await
            .unwrap();
    let replacement = bounded(service.attach_target(
        &call,
        reference.clone(),
        TargetAttachOptions {
            force: true,
            expected_connection_generation: Some(original.target.connection_generation),
        },
    ))
    .await
    .unwrap();
    assert_eq!(replacement.outcome, TargetAttachmentOutcome::Stolen);
    assert_eq!(
        replacement.target.connection_generation,
        original.target.connection_generation + 1
    );
    service
        .disconnect_connection(&call, reference.connection)
        .await
        .unwrap();
    bounded(server).await.unwrap();
}

#[tokio::test]
async fn old_connect_completion_closes_old_socket_without_replacing_reconnect() {
    use futures_util::SinkExt;
    use tokio_tungstenite::tungstenite::Message;

    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let endpoint = format!("ws://{}", listener.local_addr().unwrap());
    let entered = Arc::new(Notify::new());
    let gate = Arc::new(Semaphore::new(0));
    let old_closed = Arc::new(Notify::new());
    let server = {
        let entered = entered.clone();
        let gate = gate.clone();
        let old_closed = old_closed.clone();
        tokio::spawn(async move {
            let mut handlers = Vec::new();
            for index in 0..2 {
                let (socket, _) = listener.accept().await.unwrap();
                let entered = entered.clone();
                let gate = gate.clone();
                let old_closed = old_closed.clone();
                handlers.push(tokio::spawn(async move {
                    let mut socket = tokio_tungstenite::accept_async(socket).await.unwrap();
                    while let Some(Ok(message)) = socket.next().await {
                        let Message::Text(text) = message else { break };
                        let request: serde_json::Value = serde_json::from_str(&text).unwrap();
                        if index == 0 && request["method"] == "Browser.getVersion" {
                            entered.notify_one();
                            let _permit = gate.acquire().await.unwrap();
                        }
                        let result = match request["method"].as_str().unwrap() {
                            "Browser.getVersion" => serde_json::json!({
                                "protocolVersion": "1.3", "product": "Test", "revision": "1",
                                "userAgent": "Test", "jsVersion": "1",
                            }),
                            "Target.getTargets" => serde_json::json!({"targetInfos": []}),
                            _ => serde_json::json!({}),
                        };
                        socket
                            .send(Message::Text(
                                serde_json::json!({
                                    "id": request["id"], "result": result,
                                })
                                .to_string()
                                .into(),
                            ))
                            .await
                            .unwrap();
                    }
                    if index == 0 {
                        old_closed.notify_one();
                    }
                }));
            }
            for handler in handlers {
                handler.await.unwrap();
            }
        })
    };
    let mut state = ServiceState::default();
    super::tests::insert_context_with_targets(&mut state, "ctx", [("conn", 0, Vec::new())]);
    {
        let mut context = (*state.contexts["ctx"]).clone();
        let mut connections = (*context.connections).clone();
        let mut connection = (*connections["conn"]).clone();
        connection.configuration = ConnectionConfiguration::DirectCdp { endpoint };
        connection.status = ConnectionStatus::Disconnected;
        connections.insert("conn".into(), Arc::new(connection));
        context.connections = Arc::new(connections);
        state.contexts.insert("ctx".into(), Arc::new(context));
    }
    let service = super::tests::service_with_state(PathBuf::from("unused"), state);
    let reference = crate::api::service_api::ConnectionRef {
        context_id: "ctx".into(),
        connection_id: "conn".into(),
    };
    let pending = {
        let service = service.clone();
        let reference = reference.clone();
        tokio::spawn(async move {
            service
                .connect_connection(&CallCtx::default(), reference)
                .await
        })
    };
    bounded(entered.notified()).await;
    service
        .disconnect_connection(&CallCtx::default(), reference.clone())
        .await
        .unwrap();
    let current = bounded(service.connect_connection(&CallCtx::default(), reference.clone()))
        .await
        .unwrap();
    assert_eq!(current.connections[0].generation, 2);
    gate.add_permits(1);
    assert!(bounded(pending).await.unwrap().is_err());
    bounded(old_closed.notified()).await;
    {
        let state = service.state.lock().await;
        assert!(state.runtimes.contains_key(&("ctx".into(), "conn".into())));
        assert_eq!(state.contexts["ctx"].connections["conn"].generation, 2);
        assert!(matches!(
            state.contexts["ctx"].connections["conn"].status,
            ConnectionStatus::Connected { .. }
        ));
    }
    service
        .disconnect_connection(&CallCtx::default(), reference)
        .await
        .unwrap();
    bounded(server).await.unwrap();
}
