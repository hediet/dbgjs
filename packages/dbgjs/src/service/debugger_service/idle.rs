use super::*;

#[derive(Default)]
pub(super) struct ConnectionActivity {
    clock: std::sync::Mutex<ActivityClock>,
}

struct ActivityClock {
    last_used: Instant,
    operations: usize,
}

impl Default for ActivityClock {
    fn default() -> Self {
        Self {
            last_used: Instant::now(),
            operations: 0,
        }
    }
}

pub(super) struct ActivityGuard {
    connections: Vec<Arc<ConnectionActivity>>,
}

impl ActivityGuard {
    pub(super) fn new(connections: Vec<Arc<ConnectionActivity>>) -> Self {
        for connection in &connections {
            let mut clock = connection.clock.lock().unwrap();
            clock.operations += 1;
            clock.last_used = Instant::now();
        }
        Self { connections }
    }
}

impl Drop for ActivityGuard {
    fn drop(&mut self) {
        for connection in &self.connections {
            let mut clock = connection.clock.lock().unwrap();
            clock.operations -= 1;
            clock.last_used = Instant::now();
        }
    }
}

impl DebuggerService {
    pub(super) async fn disconnect_connection_internal(
        &self,
        connection_ref: ConnectionRef,
        idle_only: bool,
    ) -> Result<ContextSnapshot, JsonRpcError> {
        let ConnectionRef {
            context_id,
            connection_id,
        } = connection_ref;
        let (runtime, attempt) = {
            let mut state = self.state.lock().await;
            let context = state
                .contexts
                .get(&context_id)
                .cloned()
                .ok_or_else(|| not_found("context", &context_id))?;
            if idle_only && !idle_connection_expired(&state, &context_id, &connection_id) {
                return Ok(service_snapshot(&state, &self.agent_instance_id, &context_id).unwrap());
            }
            let transition = reduce_context(
                &context,
                ContextInput::UserCommand(UserCommand::DisconnectConnection {
                    connection_id: connection_id.clone(),
                }),
            )
            .map_err(transition_rpc_error)?;
            let attempt = match transition.effects.as_slice() {
                [] => {
                    return Ok(
                        service_snapshot(&state, &self.agent_instance_id, &context_id).unwrap(),
                    );
                }
                [ContextEffect::Disconnect { attempt, .. }] => *attempt,
                effects => panic!("disconnect command emitted unexpected effects: {effects:?}"),
            };
            self.commit_context(&mut state, &context_id, transition);
            let key = (context_id.clone(), connection_id.clone());
            let runtime = state.runtimes.remove(&key);
            state.connection_activity.remove(&key);
            retract_connection_resource_graph(
                &mut state,
                &context_id,
                &connection_id,
                attempt.generation,
            );
            remove_connection_debugger_registrations(&mut state, &context_id, &connection_id);
            state.pause_children_leases.remove(&key);
            cancel_playwright_proxies(
                &mut state,
                &context_id,
                &connection_id,
                Some(attempt.generation),
            );
            (runtime, attempt)
        };
        self.release_relay_attachments_for_connection(&context_id, &connection_id)
            .await;
        if let Some(runtime) = runtime {
            runtime.close().await;
        }
        let mut state = self.state.lock().await;
        let context = state
            .contexts
            .get(&context_id)
            .cloned()
            .ok_or_else(|| not_found("context", &context_id))?;
        let transition = reduce_context(
            &context,
            ContextInput::EffectCompletion(EffectCompletion::ConnectionClosed {
                connection_id,
                attempt,
            }),
        )
        .map_err(transition_rpc_error)?;
        Ok(self.commit_context(&mut state, &context_id, transition))
    }

    pub(super) async fn mutate_idle_timeout(
        &self,
        context_id: &str,
        command: UserCommand,
    ) -> Result<ContextSnapshot, JsonRpcError> {
        let mut state = self.state.lock().await;
        let previous = state.clone();
        let context = state
            .contexts
            .get(context_id)
            .ok_or_else(|| not_found("context", context_id))?;
        let transition = reduce_context(context, ContextInput::UserCommand(command))
            .map_err(transition_rpc_error)?;
        let result = self.commit_context(&mut state, context_id, transition);
        self.persist_or_restore(&mut state, previous)?;
        Ok(result)
    }

    pub(super) async fn connection_activity(&self, connection: &ConnectionRef) -> ActivityGuard {
        self.begin_activity(&connection.context_id, Some(&connection.connection_id))
            .await
    }

    pub(super) async fn context_activity(&self, context_id: &str) -> ActivityGuard {
        self.begin_activity(context_id, None).await
    }

    async fn begin_activity(&self, context_id: &str, connection_id: Option<&str>) -> ActivityGuard {
        let state = self.state.lock().await;
        // Bookkeeping must preserve endpoint validation and offline-capture behavior.
        let connections = state
            .connection_activity
            .iter()
            .filter(|((context, connection), _)| {
                context == context_id && connection_id.is_none_or(|selected| selected == connection)
            })
            .map(|(_, activity)| activity.clone())
            .collect();
        ActivityGuard::new(connections)
    }

    pub fn supervise_idle_connections(self: &Arc<Self>) {
        let service = Arc::downgrade(self);
        let mut shutdown = self.shutdown.subscribe();
        tokio::spawn(async move {
            let mut tick = tokio::time::interval(Duration::from_secs(1));
            loop {
                tokio::select! {
                    _ = tick.tick() => {}
                    _ = shutdown.changed() => return,
                }
                let Some(service) = service.upgrade() else {
                    return;
                };
                let candidates = {
                    let state = service.state.lock().await;
                    state
                        .runtimes
                        .keys()
                        .filter(|(context_id, connection_id)| {
                            idle_connection_expired(&state, context_id, connection_id)
                        })
                        .cloned()
                        .collect::<Vec<_>>()
                };
                for (context_id, connection_id) in candidates {
                    if let Err(error) = service
                        .disconnect_connection_internal(
                            ConnectionRef {
                                context_id,
                                connection_id,
                            },
                            true,
                        )
                        .await
                    {
                        eprintln!("failed to disconnect idle connection: {error:?}");
                    }
                }
            }
        });
    }
}

pub(super) fn validate_idle_timeout(timeout: IdleTimeout) -> Result<(), JsonRpcError> {
    if let IdleTimeout::After { milliseconds } = timeout {
        if milliseconds == 0
            || Instant::now()
                .checked_add(Duration::from_millis(milliseconds))
                .is_none()
        {
            return Err(invalid_params(
                "idle timeout must be a positive, representable duration or infinite",
            ));
        }
    }
    Ok(())
}

pub(super) fn idle_connection_expired(
    state: &ServiceState,
    context_id: &str,
    connection_id: &str,
) -> bool {
    let Some(context) = state.contexts.get(context_id) else {
        return false;
    };
    let Some(connection) = context.connections.get(connection_id) else {
        return false;
    };
    if !matches!(connection.status, ConnectionStatus::Connected { .. }) {
        return false;
    }
    let IdleTimeout::After { milliseconds } =
        connection.idle_timeout.unwrap_or(context.idle_timeout)
    else {
        return false;
    };
    let Some(activity) = state
        .connection_activity
        .get(&(context_id.to_owned(), connection_id.to_owned()))
    else {
        return false;
    };
    if state
        .target_debuggers
        .iter()
        .any(|((ctx, conn, _), debugger)| {
            ctx == context_id && conn == connection_id && debugger.blocks_idle_disconnect()
        })
        || state
            .playwright_proxies
            .values()
            .any(|proxy| proxy.context_id == context_id && proxy.connection_id == connection_id)
        || state
            .relays
            .values()
            .any(|relay| relay.context_id == context_id)
    {
        activity.clock.lock().unwrap().last_used = Instant::now();
        return false;
    }
    let clock = activity.clock.lock().unwrap();
    clock.operations == 0 && clock.last_used.elapsed() >= Duration::from_millis(milliseconds)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::api::service_api::{PauseSnapshot, TargetDebuggerPhase};

    async fn service() -> (Arc<DebuggerService>, tempfile::TempDir) {
        let directory = tempfile::tempdir().unwrap();
        let (shutdown, _) = watch::channel(false);
        let service = Arc::new(
            DebuggerService::load(shutdown, directory.path().join("contexts.json")).unwrap(),
        );
        service
            .put_context(&CallCtx::default(), "idle".into(), ContextKind::Named, None)
            .await
            .unwrap();
        for id in ["first", "second"] {
            service
                .put_connection(
                    &CallCtx::default(),
                    reference(id),
                    ConnectionConfiguration::DirectCdp {
                        endpoint: format!("ws://{id}"),
                    },
                )
                .await
                .unwrap();
            let mut state = service.state.lock().await;
            let context = Arc::make_mut(state.contexts.get_mut("idle").unwrap());
            let connection =
                Arc::make_mut(Arc::make_mut(&mut context.connections).get_mut(id).unwrap());
            connection.generation = 1;
            connection.status = ConnectionStatus::Connected {
                product: "test".into(),
                protocol_version: "1.3".into(),
            };
            state.connection_activity.insert(
                ("idle".into(), id.into()),
                Arc::new(ConnectionActivity::default()),
            );
        }
        (service, directory)
    }

    fn reference(id: &str) -> ConnectionRef {
        ConnectionRef {
            context_id: "idle".into(),
            connection_id: id.into(),
        }
    }

    fn timeout(seconds: u64) -> IdleTimeout {
        IdleTimeout::After {
            milliseconds: seconds * 1_000,
        }
    }

    async fn expired(service: &DebuggerService, id: &str) -> bool {
        idle_connection_expired(&*service.state.lock().await, "idle", id)
    }

    #[tokio::test(start_paused = true)]
    async fn idle_timeout_defaults_to_infinite_and_overrides_inherit_dynamically() {
        let (service, _directory) = service().await;
        tokio::time::advance(Duration::from_secs(10)).await;
        assert!(!expired(&service, "first").await);
        service
            .set_context_idle_timeout(&CallCtx::default(), "idle".into(), timeout(5))
            .await
            .unwrap();
        assert!(expired(&service, "first").await);
        service
            .set_connection_idle_timeout(
                &CallCtx::default(),
                reference("first"),
                Some(IdleTimeout::Infinite),
            )
            .await
            .unwrap();
        assert!(!expired(&service, "first").await);
        assert!(expired(&service, "second").await);
        service
            .set_connection_idle_timeout(&CallCtx::default(), reference("first"), None)
            .await
            .unwrap();
        assert!(expired(&service, "first").await);
        service
            .set_connection_idle_timeout(
                &CallCtx::default(),
                reference("second"),
                Some(timeout(20)),
            )
            .await
            .unwrap();
        assert!(!expired(&service, "second").await);
        service
            .set_context_idle_timeout(&CallCtx::default(), "idle".into(), IdleTimeout::Infinite)
            .await
            .unwrap();
        tokio::time::advance(Duration::from_secs(10)).await;
        assert!(!expired(&service, "first").await);
        assert!(expired(&service, "second").await);
    }

    #[tokio::test(start_paused = true)]
    async fn idle_timeout_activity_is_connection_scoped_and_cancellation_restarts_the_clock() {
        let (service, _directory) = service().await;
        service
            .set_context_idle_timeout(&CallCtx::default(), "idle".into(), timeout(2))
            .await
            .unwrap();
        let guard = service.connection_activity(&reference("first")).await;
        tokio::time::advance(Duration::from_secs(3)).await;
        assert!(!expired(&service, "first").await);
        assert!(expired(&service, "second").await);
        drop(guard);
        tokio::time::advance(Duration::from_millis(1_999)).await;
        assert!(!expired(&service, "first").await);
        tokio::time::advance(Duration::from_millis(1)).await;
        assert!(expired(&service, "first").await);
        let guard = service.context_activity("idle").await;
        assert!(!expired(&service, "first").await);
        assert!(!expired(&service, "second").await);
        drop(guard);
    }

    #[tokio::test(start_paused = true)]
    async fn idle_timeout_status_observation_does_not_count_as_activity_and_expiry_is_rechecked() {
        let (service, _directory) = service().await;
        service
            .set_context_idle_timeout(&CallCtx::default(), "idle".into(), timeout(1))
            .await
            .unwrap();
        tokio::time::advance(Duration::from_secs(2)).await;
        service
            .get_context(&CallCtx::default(), "idle".into())
            .await
            .unwrap();
        service
            .observe_context(
                &CallCtx::default(),
                "idle".into(),
                ObservationCursor::Current,
                0,
            )
            .await
            .unwrap();
        assert!(expired(&service, "first").await);
        let guard = service.connection_activity(&reference("first")).await;
        let snapshot = service
            .disconnect_connection_internal(reference("first"), true)
            .await
            .unwrap();
        assert!(matches!(
            snapshot.connections[0].status,
            ConnectionStatus::Connected { .. }
        ));
        drop(guard);
        tokio::time::advance(Duration::from_secs(1)).await;
        let snapshot = service
            .disconnect_connection_internal(reference("first"), true)
            .await
            .unwrap();
        assert!(matches!(
            snapshot.connections[0].status,
            ConnectionStatus::Disconnected
        ));
        assert!(matches!(
            snapshot.connections[1].status,
            ConnectionStatus::Connected { .. }
        ));
    }

    #[tokio::test(start_paused = true)]
    async fn idle_timeout_preserves_paused_targets_and_relay_leases() {
        let (service, _directory) = service().await;
        service
            .set_context_idle_timeout(&CallCtx::default(), "idle".into(), timeout(1))
            .await
            .unwrap();
        let key = ("idle".into(), "first".into(), "page".into());
        let debugger = TargetDebuggerHandle::stub_for_tests(TargetDebuggerSnapshot {
            context_id: "idle".into(),
            connection_id: "first".into(),
            target_id: "page".into(),
            connection_generation: 1,
            revision: 1,
            phase: TargetDebuggerPhase::Paused { epoch: 1 },
            scripts: Vec::new(),
            breakpoints: Vec::new(),
            logs: Vec::new(),
            log_capture: Default::default(),
            pause: Some(PauseSnapshot {
                epoch: 1,
                reason: "test".into(),
                frames: Vec::new(),
                source: None,
            }),
        });
        service
            .state
            .lock()
            .await
            .target_debuggers
            .insert(key.clone(), debugger);
        tokio::time::advance(Duration::from_secs(2)).await;
        assert!(!expired(&service, "first").await);
        assert!(expired(&service, "second").await);
        service.state.lock().await.target_debuggers.remove(&key);
        assert!(!expired(&service, "first").await);
        tokio::time::advance(Duration::from_secs(1)).await;
        assert!(expired(&service, "first").await);
        let (cancel, _) = watch::channel(false);
        service.state.lock().await.relays.insert(
            "relay".into(),
            RelayRegistration {
                context_id: "idle".into(),
                cancel,
            },
        );
        assert!(!expired(&service, "first").await);
        assert!(!expired(&service, "second").await);
    }

    #[tokio::test]
    async fn idle_timeout_policies_survive_restart_and_reject_invalid_durations() {
        let (service, directory) = service().await;
        service
            .set_context_idle_timeout(&CallCtx::default(), "idle".into(), timeout(7_200))
            .await
            .unwrap();
        service
            .set_connection_idle_timeout(
                &CallCtx::default(),
                reference("second"),
                Some(IdleTimeout::Infinite),
            )
            .await
            .unwrap();
        assert!(
            service
                .set_context_idle_timeout(
                    &CallCtx::default(),
                    "idle".into(),
                    IdleTimeout::After { milliseconds: 0 }
                )
                .await
                .is_err()
        );
        assert!(
            service
                .set_connection_idle_timeout(
                    &CallCtx::default(),
                    reference("first"),
                    Some(IdleTimeout::After { milliseconds: 0 })
                )
                .await
                .is_err()
        );
        let (shutdown, _) = watch::channel(false);
        let restored =
            DebuggerService::load(shutdown, directory.path().join("contexts.json")).unwrap();
        let snapshot = restored
            .get_context(&CallCtx::default(), "idle".into())
            .await
            .unwrap();
        assert_eq!(snapshot.idle_timeout, timeout(7_200));
        assert_eq!(snapshot.connections[0].idle_timeout, None);
        assert_eq!(
            snapshot.connections[0].effective_idle_timeout,
            timeout(7_200)
        );
        assert_eq!(
            snapshot.connections[1].idle_timeout,
            Some(IdleTimeout::Infinite)
        );
        assert_eq!(
            snapshot.connections[1].effective_idle_timeout,
            IdleTimeout::Infinite
        );
        assert!(
            snapshot
                .connections
                .iter()
                .all(|connection| connection.status == ConnectionStatus::Disconnected)
        );
        assert!(restored.state.lock().await.connection_activity.is_empty());
    }
}
