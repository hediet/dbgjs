use std::collections::{BTreeMap, VecDeque};
use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use atomic_write_file::AtomicWriteFile;
use hubrpc::prelude::{CallCtx, JsonRpcError, error_codes};
use serde::{Deserialize, Serialize};
use tokio::sync::{Mutex, watch};
use tokio::time::{Instant, timeout_at};

use crate::cdp::{
    BrowserGetVersionParams, TargetAttachToTargetParams, TargetDetachFromTargetParams,
    TargetGetTargetsParams, TargetSetDiscoverTargetsParams, TargetTargetInfo,
};
use crate::connection_provider::{ConnectionRuntime, validate_configuration};
use crate::context_engine::{
    BreakpointState, ConnectionAttempt, ConnectionState, ContextEffect, ContextInput, ContextState,
    ContextTransitionError, EffectCompletion, RuntimeObservation, UserCommand, reduce_context,
};
use crate::debugger_engine::{SessionKey, StepKind};
use crate::service_api::{
    BreakpointSnapshot, BreakpointSpec, BreakpointStatus, ConnectionConfiguration,
    ConnectionSnapshot, ConnectionStatus, ContextEventSnapshot, ContextObservation,
    ContextSnapshot, ContextSummary, CoverageSnapshot, DebuggerServiceApi, EvaluationSnapshot,
    HeapCaptureResult, HeapClassSnapshot, HeapSnapshotProgress, HeapSnapshotResult, LogpointSpec,
    MutationOptions, ObservationCursor, ObservationResult, ServiceInfo, SourceContentSnapshot,
    SourceMatchSnapshot, SourceSnapshotInfo, StepKind as ApiStepKind, TargetDebuggerSnapshot,
    TargetSnapshot, TargetWaitPredicate,
};
use crate::target_debugger::{TargetBreakpointSpec, TargetDebuggerError, TargetDebuggerHandle};

#[derive(Clone)]
pub struct DebuggerService {
    agent_instance_id: String,
    state: Arc<Mutex<ServiceState>>,
    persistence_path: PathBuf,
    shutdown: watch::Sender<bool>,
    revision_signal: watch::Sender<u64>,
}

impl DebuggerService {
    pub fn load(
        shutdown: watch::Sender<bool>,
        persistence_path: PathBuf,
    ) -> Result<Self, ServicePersistenceError> {
        let state = load_state(&persistence_path)?;
        let (revision_signal, _) = watch::channel(0);
        Ok(Self {
            agent_instance_id: random_instance_id()?,
            state: Arc::new(Mutex::new(state)),
            persistence_path,
            shutdown,
            revision_signal,
        })
    }

    fn persist(&self, state: &ServiceState) -> Result<(), ServicePersistenceError> {
        let stored = StoredServiceState::from(state);
        if let Some(parent) = self.persistence_path.parent() {
            fs::create_dir_all(parent)?;
        }
        let bytes = serde_json::to_vec_pretty(&stored)?;
        let mut file = AtomicWriteFile::open(&self.persistence_path)?;
        file.write_all(&bytes)?;
        file.commit()?;
        Ok(())
    }

    fn persist_or_restore(
        &self,
        state: &mut ServiceState,
        previous: ServiceState,
    ) -> Result<(), JsonRpcError> {
        if let Err(error) = self.persist(state) {
            *state = previous;
            return Err(JsonRpcError::new(
                error_codes::INTERNAL_ERROR,
                format!("failed to persist debugger context state: {error}"),
            ));
        }
        Ok(())
    }

    fn supervise_runtime(
        &self,
        context_id: String,
        connection_id: String,
        configuration_version: u64,
        generation: u64,
        runtime: Arc<ConnectionRuntime>,
    ) {
        let service = self.clone();
        tokio::spawn(async move {
            let reason = runtime.wait_closed().await;
            let runtime_key = (context_id.clone(), connection_id.clone());
            let mut state = service.state.lock().await;
            let is_current_runtime = state
                .runtimes
                .get(&runtime_key)
                .is_some_and(|current| Arc::ptr_eq(current, &runtime));
            if is_current_runtime {
                state.runtimes.remove(&runtime_key);
                state
                    .target_debuggers
                    .retain(|(candidate_context, candidate_connection, _), _| {
                        candidate_context != &context_id || candidate_connection != &connection_id
                    });
                if let Some(context) = state.contexts.get(&context_id).cloned() {
                    let transition = reduce_context(
                        &context,
                        ContextInput::RuntimeObservation(RuntimeObservation::ConnectionClosed {
                            connection_id,
                            attempt: ConnectionAttempt {
                                configuration_version,
                                generation,
                            },
                            reason,
                        }),
                    )
                    .expect("runtime observations do not fail");
                    service.commit_context(&mut state, &context_id, transition);
                }
            }
            drop(state);
            runtime.close().await;
        });
    }

    async fn supervise_target_events(
        &self,
        context_id: String,
        connection_id: String,
        configuration_version: u64,
        generation: u64,
        runtime: Arc<ConnectionRuntime>,
    ) {
        let Some(mut events) = runtime.take_root_events().await else {
            return;
        };
        let service = self.clone();
        tokio::spawn(async move {
            let attempt = ConnectionAttempt {
                configuration_version,
                generation,
            };
            while let Some(event) = events.recv().await {
                let observation = match event {
                    Ok(crate::cdp_runtime::RootCdpEvent::TargetCreated(params)) => {
                        RuntimeObservation::TargetUpserted {
                            connection_id: connection_id.clone(),
                            attempt,
                            target: target_snapshot(params.target_info),
                        }
                    }
                    Ok(crate::cdp_runtime::RootCdpEvent::TargetChanged(params)) => {
                        RuntimeObservation::TargetUpserted {
                            connection_id: connection_id.clone(),
                            attempt,
                            target: target_snapshot(params.target_info),
                        }
                    }
                    Ok(crate::cdp_runtime::RootCdpEvent::TargetDestroyed(params)) => {
                        RuntimeObservation::TargetRemoved {
                            connection_id: connection_id.clone(),
                            attempt,
                            target_id: params.target_id,
                        }
                    }
                    Err(error) => {
                        eprintln!("failed to decode root CDP event: {error}");
                        continue;
                    }
                };
                let mut state = service.state.lock().await;
                let runtime_is_current = state
                    .runtimes
                    .get(&(context_id.clone(), connection_id.clone()))
                    .is_some_and(|current| Arc::ptr_eq(current, &runtime));
                let Some(context) = state.contexts.get(&context_id).cloned() else {
                    break;
                };
                if !runtime_is_current {
                    break;
                }
                let transition =
                    reduce_context(&context, ContextInput::RuntimeObservation(observation))
                        .expect("target observations do not fail");
                if transition.change == crate::context_engine::ContextChange::None {
                    continue;
                }
                let removed_target =
                    transition
                        .events
                        .iter()
                        .find_map(|event| match &event.event {
                            crate::context_engine::ContextEvent::TargetDestroyed {
                                target_id,
                                ..
                            } => Some(target_id.clone()),
                            _ => None,
                        });
                service.commit_context(&mut state, &context_id, transition);
                if let Some(target_id) = removed_target {
                    state.target_debuggers.remove(&(
                        context_id.clone(),
                        connection_id.clone(),
                        target_id,
                    ));
                }
            }
        });
    }

    async fn supervise_provider_target_events(
        &self,
        context_id: String,
        connection_id: String,
        configuration_version: u64,
        generation: u64,
        runtime: Arc<ConnectionRuntime>,
    ) {
        let Some(mut events) = runtime.take_provider_target_events().await else {
            return;
        };
        let service = self.clone();
        tokio::spawn(async move {
            let attempt = ConnectionAttempt {
                configuration_version,
                generation,
            };
            while let Some(event) = events.recv().await {
                let (observation, target_to_attach, target_to_remove) = match event {
                    crate::connection_provider::ProviderTargetEvent::Upsert(target) => {
                        let target_id = target.target_id.clone();
                        (
                            RuntimeObservation::TargetUpserted {
                                connection_id: connection_id.clone(),
                                attempt,
                                target,
                            },
                            Some(target_id),
                            None,
                        )
                    }
                    crate::connection_provider::ProviderTargetEvent::Removed(target_id) => (
                        RuntimeObservation::TargetRemoved {
                            connection_id: connection_id.clone(),
                            attempt,
                            target_id: target_id.clone(),
                        },
                        None,
                        Some(target_id),
                    ),
                };
                let mut state = service.state.lock().await;
                let runtime_is_current = state
                    .runtimes
                    .get(&(context_id.clone(), connection_id.clone()))
                    .is_some_and(|current| Arc::ptr_eq(current, &runtime));
                let Some(context) = state.contexts.get(&context_id).cloned() else {
                    break;
                };
                if !runtime_is_current {
                    break;
                }
                let transition =
                    reduce_context(&context, ContextInput::RuntimeObservation(observation))
                        .expect("provider target observations do not fail");
                service.commit_context(&mut state, &context_id, transition);
                if let Some(target_id) = target_to_remove {
                    state.target_debuggers.remove(&(
                        context_id.clone(),
                        connection_id.clone(),
                        target_id,
                    ));
                }
                drop(state);

                if let Some(target_id) = target_to_attach
                    && service
                        .attach_target(
                            &CallCtx::default(),
                            context_id.clone(),
                            connection_id.clone(),
                            target_id.clone(),
                        )
                        .await
                        .is_ok()
                {
                    service
                        .mark_target_attached(
                            &context_id,
                            &connection_id,
                            target_id,
                            attempt,
                            &runtime,
                        )
                        .await;
                }
            }
        });
    }

    async fn mark_target_attached(
        &self,
        context_id: &str,
        connection_id: &str,
        target_id: String,
        attempt: ConnectionAttempt,
        runtime: &Arc<ConnectionRuntime>,
    ) {
        let mut state = self.state.lock().await;
        if !state
            .runtimes
            .get(&(context_id.to_owned(), connection_id.to_owned()))
            .is_some_and(|current| Arc::ptr_eq(current, runtime))
        {
            return;
        }
        let Some(context) = state.contexts.get(context_id).cloned() else {
            return;
        };
        let Some(mut target) = context
            .connections
            .get(connection_id)
            .and_then(|connection| connection.targets.get(&target_id))
            .cloned()
        else {
            return;
        };
        target.attached = true;
        let transition = reduce_context(
            &context,
            ContextInput::RuntimeObservation(RuntimeObservation::TargetUpserted {
                connection_id: connection_id.to_owned(),
                attempt,
                target,
            }),
        )
        .expect("provider target attachment observations do not fail");
        self.commit_context(&mut state, context_id, transition);
    }

    fn commit_context(
        &self,
        state: &mut ServiceState,
        context_id: &str,
        transition: crate::context_engine::ContextTransition,
    ) -> ContextSnapshot {
        let events = transition
            .events
            .iter()
            .map(context_event_snapshot)
            .collect::<Vec<_>>();
        state
            .contexts
            .insert(context_id.to_owned(), transition.state);
        let result = service_snapshot(state, &self.agent_instance_id, context_id)
            .expect("context was inserted above");
        let history = state.history.entry(context_id.to_owned()).or_default();
        history.push_back(ContextObservation {
            snapshot: result.clone(),
            events,
        });
        while history.len() > 256 {
            history.pop_front();
        }
        self.revision_signal
            .send_modify(|revision| *revision = revision.wrapping_add(1));
        result
    }

    fn check_mutation_options(
        &self,
        state: &ServiceState,
        context_id: &str,
        options: &MutationOptions,
    ) -> Result<Option<ContextSnapshot>, JsonRpcError> {
        let context = state
            .contexts
            .get(context_id)
            .ok_or_else(|| not_found("context", context_id))?;
        if let Some(request_id) = &options.request_id
            && state
                .completed_requests
                .contains_key(&(context_id.to_owned(), request_id.clone()))
        {
            return Ok(Some(
                service_snapshot(state, &self.agent_instance_id, context_id)
                    .expect("context was checked above"),
            ));
        }
        if let Some(expected) = options.expected_revision
            && expected != context.revision
        {
            return Err(JsonRpcError::new(
                error_codes::INVALID_REQUEST,
                format!(
                    "revisionConflict: expected revision {expected}, current revision {}",
                    context.revision
                ),
            ));
        }
        Ok(None)
    }

    fn complete_request(
        &self,
        state: &mut ServiceState,
        context_id: &str,
        options: &MutationOptions,
        revision: u64,
    ) {
        if let Some(request_id) = &options.request_id {
            state
                .completed_requests
                .insert((context_id.to_owned(), request_id.clone()), revision);
        }
    }

    async fn publish_breakpoint_application(&self, context_id: &str, breakpoint_id: &str) {
        let mut state = self.state.lock().await;
        let Some(context) = state.contexts.get(context_id).cloned() else {
            return;
        };
        let transition = reduce_context(
            &context,
            ContextInput::RuntimeObservation(RuntimeObservation::BreakpointApplicationsChanged {
                breakpoint_id: breakpoint_id.to_owned(),
            }),
        )
        .expect("breakpoint application observations do not fail");
        self.commit_context(&mut state, context_id, transition);
    }
}

fn context_event_snapshot(event: &crate::context_engine::RevisionEvent) -> ContextEventSnapshot {
    use crate::context_engine::ContextEvent;
    let (kind, subject_id) = match &event.event {
        ContextEvent::ContextUpdated => ("context.updated", None),
        ContextEvent::ConnectionConfigured { connection_id } => {
            ("connection.configured", Some(connection_id.clone()))
        }
        ContextEvent::ConnectionConnecting { connection_id, .. } => {
            ("connection.connecting", Some(connection_id.clone()))
        }
        ContextEvent::ConnectionConnected { connection_id, .. } => {
            ("connection.connected", Some(connection_id.clone()))
        }
        ContextEvent::ConnectionFailed { connection_id, .. } => {
            ("connection.failed", Some(connection_id.clone()))
        }
        ContextEvent::ConnectionDisconnecting { connection_id, .. } => {
            ("connection.disconnecting", Some(connection_id.clone()))
        }
        ContextEvent::ConnectionDisconnected { connection_id, .. } => {
            ("connection.disconnected", Some(connection_id.clone()))
        }
        ContextEvent::ConnectionRemoved { connection_id } => {
            ("connection.removed", Some(connection_id.clone()))
        }
        ContextEvent::BreakpointUpdated { breakpoint_id } => {
            ("breakpoint.updated", Some(breakpoint_id.clone()))
        }
        ContextEvent::BreakpointRemoved { breakpoint_id } => {
            ("breakpoint.removed", Some(breakpoint_id.clone()))
        }
        ContextEvent::TargetCreated { target_id, .. } => {
            ("target.created", Some(target_id.clone()))
        }
        ContextEvent::TargetChanged { target_id, .. } => {
            ("target.changed", Some(target_id.clone()))
        }
        ContextEvent::TargetDestroyed { target_id, .. } => {
            ("target.destroyed", Some(target_id.clone()))
        }
    };
    ContextEventSnapshot {
        revision: event.revision,
        kind: kind.to_owned(),
        subject_id,
    }
}

#[derive(Clone, Default)]
struct ServiceState {
    contexts: BTreeMap<String, Arc<ContextState>>,
    runtimes: BTreeMap<(String, String), Arc<ConnectionRuntime>>,
    target_debuggers: BTreeMap<(String, String, String), TargetDebuggerHandle>,
    history: BTreeMap<String, VecDeque<ContextObservation>>,
    completed_requests: BTreeMap<(String, String), u64>,
}

#[async_trait::async_trait]
impl DebuggerServiceApi for DebuggerService {
    async fn service_info(&self, _ctx: &CallCtx) -> Result<ServiceInfo, JsonRpcError> {
        Ok(ServiceInfo {
            process_id: std::process::id(),
            agent_instance_id: self.agent_instance_id.clone(),
        })
    }

    async fn list_contexts(&self, _ctx: &CallCtx) -> Result<Vec<ContextSummary>, JsonRpcError> {
        let state = self.state.lock().await;
        Ok(state
            .contexts
            .iter()
            .map(|(id, context)| ContextSummary {
                agent_instance_id: self.agent_instance_id.clone(),
                id: id.clone(),
                display_name: context.display_name.clone(),
                revision: context.revision,
                connection_count: context.connections.len() as u32,
                breakpoint_count: context.breakpoints.len() as u32,
            })
            .collect())
    }

    async fn put_context(
        &self,
        _ctx: &CallCtx,
        context_id: String,
        display_name: Option<String>,
    ) -> Result<ContextSnapshot, JsonRpcError> {
        validate_id("context", &context_id)?;
        let mut state = self.state.lock().await;
        let previous = state.clone();
        let context = state
            .contexts
            .get(&context_id)
            .cloned()
            .unwrap_or_else(|| ContextState::new(context_id.clone()));
        let transition = reduce_context(
            &context,
            ContextInput::UserCommand(UserCommand::PutContext { display_name }),
        )
        .map_err(transition_rpc_error)?;
        let result = self.commit_context(&mut state, &context_id, transition);
        self.persist_or_restore(&mut state, previous)?;
        Ok(result)
    }

    async fn get_context(
        &self,
        _ctx: &CallCtx,
        context_id: String,
    ) -> Result<ContextSnapshot, JsonRpcError> {
        let state = self.state.lock().await;
        if !state.contexts.contains_key(&context_id) {
            return Err(not_found("context", &context_id));
        }
        Ok(
            service_snapshot(&state, &self.agent_instance_id, &context_id)
                .expect("context was checked above"),
        )
    }

    async fn observe_context(
        &self,
        _ctx: &CallCtx,
        context_id: String,
        cursor: ObservationCursor,
        timeout_ms: u64,
    ) -> Result<ObservationResult, JsonRpcError> {
        let requested_revision = match cursor {
            ObservationCursor::Current => None,
            ObservationCursor::After { revision } => Some(revision),
        };
        let deadline = Instant::now() + Duration::from_millis(timeout_ms);
        let mut signal = self.revision_signal.subscribe();
        loop {
            {
                let state = self.state.lock().await;
                if !state.contexts.contains_key(&context_id) {
                    return Err(not_found("context", &context_id));
                }
                if requested_revision.is_none() {
                    return Ok(ObservationResult::Items {
                        items: vec![ContextObservation {
                            snapshot: service_snapshot(
                                &state,
                                &self.agent_instance_id,
                                &context_id,
                            )
                            .expect("context was checked above"),
                            events: Vec::new(),
                        }],
                    });
                }
                let requested = requested_revision.expect("checked above");
                let current = service_snapshot(&state, &self.agent_instance_id, &context_id)
                    .expect("context was checked above");
                let history = state.history.get(&context_id);
                let oldest_available = history
                    .and_then(|history| history.front())
                    .map(|observation| observation.snapshot.revision);
                let history_gap = oldest_available
                    .is_some_and(|oldest| requested.saturating_add(1) < oldest)
                    || (oldest_available.is_none() && requested < current.revision);
                if history_gap {
                    return Ok(ObservationResult::HistoryGap {
                        requested_revision: requested,
                        oldest_available_revision: oldest_available.unwrap_or(current.revision),
                        current,
                    });
                }
                let items = history
                    .into_iter()
                    .flatten()
                    .filter(|item| item.snapshot.revision > requested)
                    .cloned()
                    .collect::<Vec<_>>();
                if !items.is_empty() || timeout_ms == 0 {
                    return Ok(ObservationResult::Items { items });
                }
            }
            if timeout_at(deadline, signal.changed()).await.is_err() {
                return Ok(ObservationResult::Items { items: Vec::new() });
            }
        }
    }

    async fn delete_context(
        &self,
        _ctx: &CallCtx,
        context_id: String,
        options: MutationOptions,
    ) -> Result<bool, JsonRpcError> {
        let mut state = self.state.lock().await;
        if options.request_id.as_ref().is_some_and(|request_id| {
            state
                .completed_requests
                .contains_key(&(context_id.clone(), request_id.clone()))
        }) {
            return Ok(true);
        }
        if let Some(existing) = self.check_mutation_options(&state, &context_id, &options)? {
            return Ok(existing.id == context_id);
        }
        if state
            .runtimes
            .keys()
            .any(|(candidate_context, _)| candidate_context == &context_id)
        {
            return Err(invalid_state(
                "all context connections must be disconnected before deletion",
            ));
        }
        let previous = state.clone();
        if state.contexts.remove(&context_id).is_none() {
            return Err(not_found("context", &context_id));
        }
        self.complete_request(&mut state, &context_id, &options, 0);
        state.history.remove(&context_id);
        state
            .target_debuggers
            .retain(|(candidate_context, _, _), _| candidate_context != &context_id);
        self.persist_or_restore(&mut state, previous)?;
        Ok(true)
    }

    async fn put_connection(
        &self,
        _ctx: &CallCtx,
        context_id: String,
        connection_id: String,
        configuration: ConnectionConfiguration,
    ) -> Result<ContextSnapshot, JsonRpcError> {
        validate_id("connection", &connection_id)?;
        validate_connection_configuration(&configuration)?;

        let mut state = self.state.lock().await;
        let previous = state.clone();
        let context = state
            .contexts
            .get(&context_id)
            .cloned()
            .ok_or_else(|| not_found("context", &context_id))?;
        let transition = reduce_context(
            &context,
            ContextInput::UserCommand(UserCommand::PutConnection {
                connection_id,
                configuration,
            }),
        )
        .map_err(transition_rpc_error)?;
        let result = self.commit_context(&mut state, &context_id, transition);
        self.persist_or_restore(&mut state, previous)?;
        Ok(result)
    }

    async fn connect_connection(
        &self,
        _ctx: &CallCtx,
        context_id: String,
        connection_id: String,
    ) -> Result<ContextSnapshot, JsonRpcError> {
        let (configuration, attempt) = {
            let mut state = self.state.lock().await;
            let context = state
                .contexts
                .get(&context_id)
                .cloned()
                .ok_or_else(|| not_found("context", &context_id))?;
            let transition = reduce_context(
                &context,
                ContextInput::UserCommand(UserCommand::ConnectConnection {
                    connection_id: connection_id.clone(),
                }),
            )
            .map_err(transition_rpc_error)?;
            let (configuration, attempt) = match transition.effects.as_slice() {
                [
                    ContextEffect::Connect {
                        configuration,
                        attempt,
                        ..
                    },
                ] => (configuration.clone(), *attempt),
                effects => panic!("connect command emitted unexpected effects: {effects:?}"),
            };
            self.commit_context(&mut state, &context_id, transition);
            (configuration, attempt)
        };

        let connected = connect_runtime(&configuration, attempt.generation).await;
        let mut state = self.state.lock().await;
        let context = state
            .contexts
            .get(&context_id)
            .cloned()
            .ok_or_else(|| not_found("context", &context_id))?;
        let runtime_key = (context_id.clone(), connection_id.clone());
        let (completion, runtime) = match connected {
            Ok((runtime, product, protocol_version, targets)) => (
                EffectCompletion::ConnectionOpened {
                    connection_id: connection_id.clone(),
                    attempt,
                    product,
                    protocol_version,
                    targets: targets
                        .into_iter()
                        .map(|target| (target.target_id.clone(), target))
                        .collect(),
                },
                Some(runtime),
            ),
            Err(message) => (
                EffectCompletion::ConnectionOpenFailed {
                    connection_id: connection_id.clone(),
                    attempt,
                    message,
                },
                None,
            ),
        };
        let transition = match reduce_context(&context, ContextInput::EffectCompletion(completion))
        {
            Ok(transition) => transition,
            Err(error) => {
                drop(state);
                if let Some(runtime) = runtime {
                    runtime.close().await;
                }
                return Err(transition_rpc_error(error));
            }
        };
        let result = snapshot(&self.agent_instance_id, &context_id, &transition.state);
        let auto_attach_targets = transition
            .state
            .connections
            .get(&connection_id)
            .map(|connection| {
                connection
                    .targets
                    .values()
                    .filter(|target| matches!(target.target_type.as_str(), "page" | "node"))
                    .map(|target| target.target_id.clone())
                    .collect::<Vec<_>>()
            })
            .unwrap_or_default();
        self.commit_context(&mut state, &context_id, transition);
        if let Some(runtime) = runtime {
            state.runtimes.insert(runtime_key, runtime.clone());
            self.supervise_runtime(
                context_id.clone(),
                connection_id.clone(),
                attempt.configuration_version,
                attempt.generation,
                runtime,
            );
            self.supervise_target_events(
                context_id.clone(),
                connection_id.clone(),
                attempt.configuration_version,
                attempt.generation,
                state
                    .runtimes
                    .get(&(context_id.clone(), connection_id.clone()))
                    .expect("runtime was inserted above")
                    .clone(),
            )
            .await;
            self.supervise_provider_target_events(
                context_id.clone(),
                connection_id.clone(),
                attempt.configuration_version,
                attempt.generation,
                state
                    .runtimes
                    .get(&(context_id.clone(), connection_id.clone()))
                    .expect("runtime was inserted above")
                    .clone(),
            )
            .await;
        } else {
            state.runtimes.remove(&runtime_key);
        }
        drop(state);
        for target_id in auto_attach_targets {
            let _ = self
                .attach_target(_ctx, context_id.clone(), connection_id.clone(), target_id)
                .await;
        }
        Ok(result)
    }

    async fn disconnect_connection(
        &self,
        _ctx: &CallCtx,
        context_id: String,
        connection_id: String,
    ) -> Result<ContextSnapshot, JsonRpcError> {
        let (runtime, attempt) = {
            let mut state = self.state.lock().await;
            let context = state
                .contexts
                .get(&context_id)
                .cloned()
                .ok_or_else(|| not_found("context", &context_id))?;
            let transition = reduce_context(
                &context,
                ContextInput::UserCommand(UserCommand::DisconnectConnection {
                    connection_id: connection_id.clone(),
                }),
            )
            .map_err(transition_rpc_error)?;
            let attempt = match transition.effects.as_slice() {
                [] => {
                    return Ok(snapshot(
                        &self.agent_instance_id,
                        &context_id,
                        &transition.state,
                    ));
                }
                [ContextEffect::Disconnect { attempt, .. }] => *attempt,
                effects => panic!("disconnect command emitted unexpected effects: {effects:?}"),
            };
            self.commit_context(&mut state, &context_id, transition);
            let runtime = state
                .runtimes
                .remove(&(context_id.clone(), connection_id.clone()));
            state
                .target_debuggers
                .retain(|(candidate_context, candidate_connection, _), _| {
                    candidate_context != &context_id || candidate_connection != &connection_id
                });
            (runtime, attempt)
        };

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
        let result = self.commit_context(&mut state, &context_id, transition);
        Ok(result)
    }

    async fn delete_connection(
        &self,
        _ctx: &CallCtx,
        context_id: String,
        connection_id: String,
        options: MutationOptions,
    ) -> Result<ContextSnapshot, JsonRpcError> {
        let mut state = self.state.lock().await;
        if let Some(existing) = self.check_mutation_options(&state, &context_id, &options)? {
            return Ok(existing);
        }
        let previous = state.clone();
        let context = state
            .contexts
            .get(&context_id)
            .cloned()
            .ok_or_else(|| not_found("context", &context_id))?;
        let transition = reduce_context(
            &context,
            ContextInput::UserCommand(UserCommand::RemoveConnection {
                connection_id: connection_id.clone(),
            }),
        )
        .map_err(transition_rpc_error)?;
        let result = self.commit_context(&mut state, &context_id, transition);
        self.complete_request(&mut state, &context_id, &options, result.revision);
        state
            .target_debuggers
            .retain(|(candidate_context, candidate_connection, _), _| {
                candidate_context != &context_id || candidate_connection != &connection_id
            });
        self.persist_or_restore(&mut state, previous)?;
        Ok(result)
    }

    async fn put_breakpoint(
        &self,
        _ctx: &CallCtx,
        context_id: String,
        breakpoint_id: String,
        source_path: String,
        line: u32,
        column: u32,
    ) -> Result<ContextSnapshot, JsonRpcError> {
        validate_id("breakpoint", &breakpoint_id)?;
        if source_path.is_empty() {
            return Err(invalid_params("source path must not be empty"));
        }
        if line == 0 || column == 0 {
            return Err(invalid_params("breakpoint lines and columns are one-based"));
        }
        let runtime_breakpoint = TargetBreakpointSpec {
            id: breakpoint_id.clone(),
            source_url: source_path.clone(),
            line,
            column,
            condition: None,
        };
        let (result, target_debuggers) = {
            let mut state = self.state.lock().await;
            let previous = state.clone();
            let context = state
                .contexts
                .get(&context_id)
                .cloned()
                .ok_or_else(|| not_found("context", &context_id))?;
            let transition = reduce_context(
                &context,
                ContextInput::UserCommand(UserCommand::PutBreakpoint {
                    breakpoint_id,
                    source_path,
                    line,
                    column,
                    enabled: true,
                    condition: None,
                    target_selector: None,
                }),
            )
            .map_err(transition_rpc_error)?;
            let result = self.commit_context(&mut state, &context_id, transition);
            self.persist_or_restore(&mut state, previous)?;
            let target_debuggers = state
                .target_debuggers
                .iter()
                .filter(|((candidate_context, _, _), _)| candidate_context == &context_id)
                .map(|(_, debugger)| debugger.clone())
                .collect::<Vec<_>>();
            (result, target_debuggers)
        };
        let has_target_debuggers = !target_debuggers.is_empty();
        for debugger in target_debuggers {
            match debugger
                .set_breakpoint(result.revision, runtime_breakpoint.clone())
                .await
            {
                Ok(_) => {
                    debugger.settle(Duration::from_millis(200)).await;
                }
                Err(TargetDebuggerError::Stopped) => {}
                Err(error) => {
                    return Err(internal_error(format!(
                        "breakpoint intent was persisted, but runtime application failed: {error}"
                    )));
                }
            }
        }
        if has_target_debuggers {
            self.publish_breakpoint_application(&context_id, &runtime_breakpoint.id)
                .await;
        }
        let state = self.state.lock().await;
        Ok(
            service_snapshot(&state, &self.agent_instance_id, &context_id)
                .expect("context still exists after breakpoint application"),
        )
    }

    async fn put_breakpoint_spec(
        &self,
        _ctx: &CallCtx,
        context_id: String,
        breakpoint_id: String,
        specification: BreakpointSpec,
        options: MutationOptions,
    ) -> Result<ContextSnapshot, JsonRpcError> {
        validate_id("breakpoint", &breakpoint_id)?;
        validate_breakpoint_spec(&specification)?;
        let runtime_breakpoint = TargetBreakpointSpec {
            id: breakpoint_id.clone(),
            source_url: specification.source_path.clone(),
            line: specification.line,
            column: specification.column,
            condition: specification.condition.clone(),
        };
        let (result, target_debuggers) = {
            let mut state = self.state.lock().await;
            if let Some(existing) = self.check_mutation_options(&state, &context_id, &options)? {
                return Ok(existing);
            }
            let previous = state.clone();
            let context = state
                .contexts
                .get(&context_id)
                .cloned()
                .ok_or_else(|| not_found("context", &context_id))?;
            let transition = reduce_context(
                &context,
                ContextInput::UserCommand(UserCommand::PutBreakpoint {
                    breakpoint_id,
                    source_path: specification.source_path,
                    line: specification.line,
                    column: specification.column,
                    enabled: specification.enabled,
                    condition: specification.condition,
                    target_selector: specification.target_selector.clone(),
                }),
            )
            .map_err(transition_rpc_error)?;
            let result = self.commit_context(&mut state, &context_id, transition);
            self.complete_request(&mut state, &context_id, &options, result.revision);
            self.persist_or_restore(&mut state, previous)?;
            let target_debuggers = state
                .target_debuggers
                .iter()
                .filter(|((candidate_context, _, _), _)| candidate_context == &context_id)
                .map(|((_, _, target_id), debugger)| (target_id.clone(), debugger.clone()))
                .collect::<Vec<_>>();
            (result, target_debuggers)
        };
        let has_target_debuggers = !target_debuggers.is_empty();
        for (target_id, debugger) in target_debuggers {
            let applies_to_target = specification.enabled
                && specification
                    .target_selector
                    .as_ref()
                    .is_none_or(|selector| selector == &target_id);
            if applies_to_target {
                debugger
                    .set_breakpoint(result.revision, runtime_breakpoint.clone())
                    .await
                    .map_err(target_debugger_rpc_error)?;
            } else {
                debugger
                    .remove_breakpoint(result.revision, runtime_breakpoint.id.clone())
                    .await
                    .map_err(target_debugger_rpc_error)?;
            }
        }
        if has_target_debuggers {
            self.publish_breakpoint_application(&context_id, &runtime_breakpoint.id)
                .await;
        }
        let state = self.state.lock().await;
        Ok(
            service_snapshot(&state, &self.agent_instance_id, &context_id)
                .expect("context still exists after breakpoint application"),
        )
    }

    async fn delete_breakpoint(
        &self,
        _ctx: &CallCtx,
        context_id: String,
        breakpoint_id: String,
        options: MutationOptions,
    ) -> Result<ContextSnapshot, JsonRpcError> {
        let (result, target_debuggers) = {
            let mut state = self.state.lock().await;
            if let Some(existing) = self.check_mutation_options(&state, &context_id, &options)? {
                return Ok(existing);
            }
            let previous = state.clone();
            let context = state
                .contexts
                .get(&context_id)
                .cloned()
                .ok_or_else(|| not_found("context", &context_id))?;
            let transition = reduce_context(
                &context,
                ContextInput::UserCommand(UserCommand::RemoveBreakpoint {
                    breakpoint_id: breakpoint_id.clone(),
                }),
            )
            .map_err(transition_rpc_error)?;
            let result = self.commit_context(&mut state, &context_id, transition);
            self.complete_request(&mut state, &context_id, &options, result.revision);
            self.persist_or_restore(&mut state, previous)?;
            let target_debuggers = state
                .target_debuggers
                .iter()
                .filter(|((candidate_context, _, _), _)| candidate_context == &context_id)
                .map(|(_, debugger)| debugger.clone())
                .collect::<Vec<_>>();
            (result, target_debuggers)
        };
        for debugger in target_debuggers {
            debugger
                .remove_breakpoint(result.revision, breakpoint_id.clone())
                .await
                .map_err(target_debugger_rpc_error)?;
        }
        Ok(result)
    }

    async fn list_sources(
        &self,
        _ctx: &CallCtx,
        context_id: String,
        path: Option<String>,
    ) -> Result<Vec<SourceSnapshotInfo>, JsonRpcError> {
        let state = self.state.lock().await;
        if !state.contexts.contains_key(&context_id) {
            return Err(not_found("context", &context_id));
        }
        let mut sources = BTreeMap::new();
        for ((candidate_context, connection_id, target_id), debugger) in &state.target_debuggers {
            if candidate_context != &context_id {
                continue;
            }
            for script in debugger.snapshot().scripts {
                if path.as_ref().is_some_and(|path| !script.url.contains(path)) {
                    continue;
                }
                let authored_sources = match &script.status {
                    crate::service_api::TargetScriptStatus::Resolved { authored_sources } => {
                        authored_sources.clone()
                    }
                    _ => Vec::new(),
                };
                sources.insert(
                    (script.url.clone(), connection_id.clone(), target_id.clone()),
                    SourceSnapshotInfo {
                        path: script.url,
                        kind: "runtime".into(),
                        status: format!("{:?}", script.status),
                        connection_id: Some(connection_id.clone()),
                        target_id: Some(target_id.clone()),
                        source_map_url: script.source_map_url,
                    },
                );
                for authored in authored_sources {
                    sources.insert(
                        (authored.clone(), connection_id.clone(), target_id.clone()),
                        SourceSnapshotInfo {
                            path: authored,
                            kind: "authored".into(),
                            status: "resolved".into(),
                            connection_id: Some(connection_id.clone()),
                            target_id: Some(target_id.clone()),
                            source_map_url: None,
                        },
                    );
                }
            }
        }
        if let Some(context) = state.contexts.get(&context_id) {
            for breakpoint in context.breakpoints.values() {
                if path
                    .as_ref()
                    .is_some_and(|path| !breakpoint.source_path.contains(path))
                {
                    continue;
                }
                sources
                    .entry((breakpoint.source_path.clone(), String::new(), String::new()))
                    .or_insert_with(|| SourceSnapshotInfo {
                        path: breakpoint.source_path.clone(),
                        kind: "intent".into(),
                        status: "known".into(),
                        connection_id: None,
                        target_id: None,
                        source_map_url: None,
                    });
            }
        }
        Ok(sources.into_values().collect())
    }

    async fn show_source(
        &self,
        _ctx: &CallCtx,
        context_id: String,
        path: String,
    ) -> Result<SourceContentSnapshot, JsonRpcError> {
        let debuggers = {
            let state = self.state.lock().await;
            if !state.contexts.contains_key(&context_id) {
                return Err(not_found("context", &context_id));
            }
            state
                .target_debuggers
                .iter()
                .filter(|((candidate_context, _, _), _)| candidate_context == &context_id)
                .map(|(_, debugger)| debugger.clone())
                .collect::<Vec<_>>()
        };
        for debugger in debuggers {
            if let Some(content) = debugger
                .source_content(path.clone())
                .await
                .map_err(target_debugger_rpc_error)?
            {
                return Ok(content);
            }
        }
        let file_path = source_file_path(&path)?;
        let content = fs::read_to_string(&file_path).map_err(|error| {
            internal_error(format!(
                "failed to read source '{}': {error}",
                file_path.display()
            ))
        })?;
        Ok(SourceContentSnapshot { path, content })
    }

    async fn grep_sources(
        &self,
        ctx: &CallCtx,
        context_id: String,
        pattern: String,
    ) -> Result<Vec<SourceMatchSnapshot>, JsonRpcError> {
        if pattern.is_empty() {
            return Err(invalid_params("source grep pattern must not be empty"));
        }
        let sources = self.list_sources(ctx, context_id.clone(), None).await?;
        let mut matches = Vec::new();
        for source in sources {
            let Ok(content) = self
                .show_source(ctx, context_id.clone(), source.path.clone())
                .await
            else {
                continue;
            };
            for (line_index, line) in content.content.lines().enumerate() {
                for (column, _) in line.match_indices(&pattern) {
                    matches.push(SourceMatchSnapshot {
                        path: source.path.clone(),
                        line: line_index as u32 + 1,
                        column: column as u32 + 1,
                        text: line.to_owned(),
                    });
                }
            }
        }
        Ok(matches)
    }

    async fn map_source(
        &self,
        _ctx: &CallCtx,
        context_id: String,
        path: String,
        line: u32,
        column: u32,
    ) -> Result<Vec<crate::service_api::SourceLocation>, JsonRpcError> {
        if line == 0 || column == 0 {
            return Err(invalid_params("source locations are one-based"));
        }
        let debuggers = {
            let state = self.state.lock().await;
            if !state.contexts.contains_key(&context_id) {
                return Err(not_found("context", &context_id));
            }
            state
                .target_debuggers
                .iter()
                .filter(|((candidate_context, _, _), _)| candidate_context == &context_id)
                .map(|(_, debugger)| debugger.clone())
                .collect::<Vec<_>>()
        };
        let mut locations = Vec::new();
        for debugger in debuggers {
            locations.extend(
                debugger
                    .map_source(path.clone(), line, column)
                    .await
                    .map_err(target_debugger_rpc_error)?,
            );
        }
        locations.sort_by(|left, right| {
            (&left.source_url, left.line, left.column).cmp(&(
                &right.source_url,
                right.line,
                right.column,
            ))
        });
        locations.dedup();
        Ok(locations)
    }

    async fn evict_source_caches(
        &self,
        _ctx: &CallCtx,
        context_id: String,
    ) -> Result<u32, JsonRpcError> {
        let debuggers = {
            let state = self.state.lock().await;
            if !state.contexts.contains_key(&context_id) {
                return Err(not_found("context", &context_id));
            }
            state
                .target_debuggers
                .iter()
                .filter(|((candidate_context, _, _), _)| candidate_context == &context_id)
                .map(|(_, debugger)| debugger.clone())
                .collect::<Vec<_>>()
        };
        for debugger in &debuggers {
            debugger
                .evict_source_caches()
                .await
                .map_err(target_debugger_rpc_error)?;
        }
        Ok(debuggers.len() as u32)
    }

    async fn export_sources(
        &self,
        ctx: &CallCtx,
        context_id: String,
        destination: String,
    ) -> Result<Vec<String>, JsonRpcError> {
        let destination = PathBuf::from(destination);
        fs::create_dir_all(&destination).map_err(|error| {
            internal_error(format!(
                "failed to create source export directory '{}': {error}",
                destination.display()
            ))
        })?;
        let sources = self.list_sources(ctx, context_id.clone(), None).await?;
        let mut exported = Vec::new();
        for source in sources {
            let Ok(content) = self
                .show_source(ctx, context_id.clone(), source.path.clone())
                .await
            else {
                continue;
            };
            let source_path = source_file_path(&source.path).unwrap_or_else(|_| {
                PathBuf::from(source.path.rsplit('/').next().unwrap_or("source.js"))
            });
            let name = format!(
                "{:016x}-{}",
                stable_name_hash(&source.path),
                sanitize_file_name(
                    source_path
                        .file_name()
                        .and_then(|name| name.to_str())
                        .unwrap_or("source.js")
                )
            );
            let output = destination.join(name);
            let mut file = AtomicWriteFile::open(&output)
                .map_err(|error| internal_error(error.to_string()))?;
            file.write_all(content.content.as_bytes())
                .map_err(|error| internal_error(error.to_string()))?;
            file.commit()
                .map_err(|error| internal_error(error.to_string()))?;
            exported.push(output.to_string_lossy().into_owned());
        }
        Ok(exported)
    }

    async fn attach_target(
        &self,
        _ctx: &CallCtx,
        context_id: String,
        connection_id: String,
        target_id: String,
    ) -> Result<TargetDebuggerSnapshot, JsonRpcError> {
        let target_id = self
            .resolve_target_id(&context_id, &connection_id, &target_id)
            .await?;
        let debugger_key = (context_id.clone(), connection_id.clone(), target_id.clone());
        let (runtime, generation, failed_session) = {
            let mut state = self.state.lock().await;
            let failed_session = match state.target_debuggers.get(&debugger_key).cloned() {
                Some(debugger)
                    if matches!(
                        debugger.snapshot().phase,
                        crate::service_api::TargetDebuggerPhase::Failed { .. }
                    ) =>
                {
                    state.target_debuggers.remove(&debugger_key);
                    Some(debugger.session_id().to_owned())
                }
                Some(debugger) => return Ok(debugger.snapshot()),
                None => None,
            };
            let state = &*state;
            let context = state
                .contexts
                .get(&context_id)
                .ok_or_else(|| not_found("context", &context_id))?;
            let connection = context
                .connections
                .get(&connection_id)
                .ok_or_else(|| not_found("connection", &connection_id))?;
            if !connection.targets.contains_key(&target_id) {
                return Err(not_found("target", &target_id));
            }
            let runtime = state
                .runtimes
                .get(&(context_id.clone(), connection_id.clone()))
                .cloned()
                .ok_or_else(|| invalid_state("connection is not connected"))?;
            (runtime, connection.generation, failed_session)
        };
        if let Some(session_id) = failed_session {
            detach_session(&runtime, &session_id).await;
        }

        let (session, session_key) = if runtime.is_direct_debugger() {
            let session = runtime
                .take_direct_debugger_session(&target_id)
                .ok_or_else(|| invalid_state("direct debugger target is already attached"))?;
            let key = session.key().clone();
            (session, key)
        } else {
            let mut attach = TargetAttachToTargetParams::new(target_id.clone());
            attach.flatten = Some(true);
            let attached = runtime
                .root()
                .target_attach_to_target(attach)
                .await
                .map_err(|error| cdp_rpc_error("Target.attachToTarget", error))?;
            let session_key = SessionKey {
                connection_generation: generation,
                session_id: attached.session_id.clone(),
            };
            let session = match runtime.open_session(session_key.clone()) {
                Ok(session) => session,
                Err(error) => {
                    detach_session(&runtime, &session_key.session_id).await;
                    return Err(internal_error(error.to_string()));
                }
            };
            (session, session_key)
        };
        let debugger = match TargetDebuggerHandle::start(
            context_id.clone(),
            connection_id.clone(),
            target_id.clone(),
            generation,
            session,
            session_key.clone(),
        )
        .await
        {
            Ok(debugger) => debugger,
            Err(error) => {
                detach_session(&runtime, &session_key.session_id).await;
                return Err(target_debugger_rpc_error(error));
            }
        };

        let mut state = self.state.lock().await;
        let runtime_is_current = state
            .runtimes
            .get(&(context_id.clone(), connection_id.clone()))
            .is_some_and(|current| Arc::ptr_eq(current, &runtime));
        let generation_is_current = state
            .contexts
            .get(&context_id)
            .and_then(|context| context.connections.get(&connection_id))
            .is_some_and(|connection| connection.generation == generation);
        if !runtime_is_current || !generation_is_current {
            drop(state);
            detach_session(&runtime, &session_key.session_id).await;
            return Err(invalid_state(
                "connection changed while the target was being attached",
            ));
        }
        if let Some(existing) = state.target_debuggers.get(&debugger_key) {
            let snapshot = existing.snapshot();
            drop(state);
            detach_session(&runtime, &session_key.session_id).await;
            return Ok(snapshot);
        }
        state
            .target_debuggers
            .insert(debugger_key.clone(), debugger.clone());
        let context = state
            .contexts
            .get(&context_id)
            .expect("context was validated above");
        let context_revision = context.revision;
        let breakpoints = context
            .breakpoints
            .iter()
            .filter(|(_, breakpoint)| breakpoint.enabled)
            .filter(|(_, breakpoint)| {
                breakpoint
                    .target_selector
                    .as_ref()
                    .is_none_or(|selector| selector == &target_id)
            })
            .map(|(id, breakpoint)| TargetBreakpointSpec {
                id: id.clone(),
                source_url: breakpoint.source_path.clone(),
                line: breakpoint.line,
                column: breakpoint.column,
                condition: breakpoint.condition.clone(),
            })
            .collect::<Vec<_>>();
        let breakpoint_ids = breakpoints
            .iter()
            .map(|breakpoint| breakpoint.id.clone())
            .collect::<Vec<_>>();
        drop(state);

        for breakpoint in breakpoints {
            if let Err(error) = debugger.set_breakpoint(context_revision, breakpoint).await {
                let mut state = self.state.lock().await;
                if state
                    .target_debuggers
                    .get(&debugger_key)
                    .is_some_and(|current| current.same_instance(&debugger))
                {
                    state.target_debuggers.remove(&debugger_key);
                }
                drop(state);
                detach_session(&runtime, &session_key.session_id).await;
                return Err(target_debugger_rpc_error(error));
            }
        }
        let snapshot = debugger.settle(Duration::from_millis(200)).await;
        for breakpoint_id in breakpoint_ids {
            self.publish_breakpoint_application(&context_id, &breakpoint_id)
                .await;
        }
        Ok(snapshot)
    }

    async fn get_target(
        &self,
        _ctx: &CallCtx,
        context_id: String,
        connection_id: String,
        target_id: String,
    ) -> Result<TargetDebuggerSnapshot, JsonRpcError> {
        Ok(self
            .target_debugger(&context_id, &connection_id, &target_id)
            .await?
            .snapshot())
    }

    async fn wait_target(
        &self,
        _ctx: &CallCtx,
        context_id: String,
        connection_id: String,
        target_id: String,
        predicate: TargetWaitPredicate,
        timeout_ms: u64,
    ) -> Result<TargetDebuggerSnapshot, JsonRpcError> {
        self.target_debugger(&context_id, &connection_id, &target_id)
            .await?
            .wait(predicate, Duration::from_millis(timeout_ms))
            .await
            .map_err(target_debugger_rpc_error)
    }

    async fn resume_target(
        &self,
        _ctx: &CallCtx,
        context_id: String,
        connection_id: String,
        target_id: String,
        pause_epoch: u64,
    ) -> Result<TargetDebuggerSnapshot, JsonRpcError> {
        self.target_debugger(&context_id, &connection_id, &target_id)
            .await?
            .resume(pause_epoch)
            .await
            .map_err(target_debugger_rpc_error)
    }

    async fn step_target(
        &self,
        _ctx: &CallCtx,
        context_id: String,
        connection_id: String,
        target_id: String,
        pause_epoch: u64,
        kind: ApiStepKind,
    ) -> Result<TargetDebuggerSnapshot, JsonRpcError> {
        self.target_debugger(&context_id, &connection_id, &target_id)
            .await?
            .step(
                pause_epoch,
                match kind {
                    ApiStepKind::Into => StepKind::Into,
                    ApiStepKind::Over => StepKind::Over,
                    ApiStepKind::Out => StepKind::Out,
                },
            )
            .await
            .map_err(target_debugger_rpc_error)
    }

    async fn evaluate_target(
        &self,
        _ctx: &CallCtx,
        context_id: String,
        connection_id: String,
        target_id: String,
        pause_epoch: Option<u64>,
        frame_index: u32,
        expression: String,
    ) -> Result<EvaluationSnapshot, JsonRpcError> {
        self.target_debugger(&context_id, &connection_id, &target_id)
            .await?
            .evaluate(pause_epoch, frame_index, expression)
            .await
            .map_err(target_debugger_rpc_error)
    }

    async fn set_logpoint(
        &self,
        _ctx: &CallCtx,
        context_id: String,
        connection_id: String,
        target_id: String,
        logpoint_id: String,
        source_url: String,
        line: u32,
        column: u32,
        expression: String,
    ) -> Result<TargetDebuggerSnapshot, JsonRpcError> {
        self.set_logpoints(
            _ctx,
            context_id,
            connection_id,
            target_id,
            vec![LogpointSpec {
                id: logpoint_id,
                source_url,
                line,
                column,
                expression,
            }],
        )
        .await
    }

    async fn set_logpoints(
        &self,
        _ctx: &CallCtx,
        context_id: String,
        connection_id: String,
        target_id: String,
        logpoints: Vec<LogpointSpec>,
    ) -> Result<TargetDebuggerSnapshot, JsonRpcError> {
        if logpoints.is_empty() {
            return Err(invalid_params("at least one logpoint is required"));
        }
        let breakpoints = logpoints
            .into_iter()
            .map(|logpoint| {
                validate_id("logpoint", &logpoint.id)?;
                if logpoint.line == 0 || logpoint.column == 0 {
                    return Err(invalid_params("logpoint lines and columns are one-based"));
                }
                Ok(TargetBreakpointSpec {
                    id: format!("log:{}", logpoint.id),
                    source_url: logpoint.source_url,
                    line: logpoint.line,
                    column: logpoint.column,
                    condition: Some(format!(
                        "console.log({}, JSON.stringify(({}))), false",
                        serde_json::to_string(&logpoint.id)
                            .map_err(|error| internal_error(error.to_string()))?,
                        logpoint.expression
                    )),
                })
            })
            .collect::<Result<Vec<_>, JsonRpcError>>()?;
        let debugger = self
            .target_debugger(&context_id, &connection_id, &target_id)
            .await?;
        debugger
            .set_breakpoints(u64::MAX, breakpoints)
            .await
            .map_err(target_debugger_rpc_error)
    }

    async fn click_target(
        &self,
        _ctx: &CallCtx,
        context_id: String,
        connection_id: String,
        target_id: String,
        selector: String,
    ) -> Result<bool, JsonRpcError> {
        self.target_debugger(&context_id, &connection_id, &target_id)
            .await?
            .click(selector)
            .await
            .map_err(target_debugger_rpc_error)?;
        Ok(true)
    }

    async fn key_target(
        &self,
        _ctx: &CallCtx,
        context_id: String,
        connection_id: String,
        target_id: String,
        chord: String,
    ) -> Result<bool, JsonRpcError> {
        self.target_debugger(&context_id, &connection_id, &target_id)
            .await?
            .key(chord)
            .await
            .map_err(target_debugger_rpc_error)?;
        Ok(true)
    }

    async fn type_target(
        &self,
        _ctx: &CallCtx,
        context_id: String,
        connection_id: String,
        target_id: String,
        text: String,
    ) -> Result<bool, JsonRpcError> {
        self.target_debugger(&context_id, &connection_id, &target_id)
            .await?
            .type_text(text)
            .await
            .map_err(target_debugger_rpc_error)?;
        Ok(true)
    }

    async fn start_coverage(
        &self,
        _ctx: &CallCtx,
        context_id: String,
        connection_id: String,
        target_id: String,
    ) -> Result<bool, JsonRpcError> {
        self.target_debugger(&context_id, &connection_id, &target_id)
            .await?
            .start_coverage()
            .await
            .map_err(target_debugger_rpc_error)?;
        Ok(true)
    }

    async fn take_coverage(
        &self,
        _ctx: &CallCtx,
        context_id: String,
        connection_id: String,
        target_id: String,
        capture_id: Option<String>,
        exclude_capture_id: Option<String>,
    ) -> Result<CoverageSnapshot, JsonRpcError> {
        self.target_debugger(&context_id, &connection_id, &target_id)
            .await?
            .take_coverage(capture_id, exclude_capture_id)
            .await
            .map_err(target_debugger_rpc_error)
    }

    async fn stop_coverage(
        &self,
        _ctx: &CallCtx,
        context_id: String,
        connection_id: String,
        target_id: String,
        exclude_capture_id: Option<String>,
    ) -> Result<CoverageSnapshot, JsonRpcError> {
        self.target_debugger(&context_id, &connection_id, &target_id)
            .await?
            .stop_coverage(exclude_capture_id)
            .await
            .map_err(target_debugger_rpc_error)
    }

    async fn finish_coverage(
        &self,
        _ctx: &CallCtx,
        context_id: String,
        connection_id: String,
        target_id: String,
        exclude_capture_id: Option<String>,
    ) -> Result<bool, JsonRpcError> {
        self.target_debugger(&context_id, &connection_id, &target_id)
            .await?
            .finish_coverage(exclude_capture_id)
            .await
            .map_err(target_debugger_rpc_error)?;
        Ok(true)
    }

    async fn get_coverage(
        &self,
        _ctx: &CallCtx,
        context_id: String,
        connection_id: String,
        target_id: String,
        capture_id: String,
        source_path: Option<String>,
        no_cache: bool,
    ) -> Result<CoverageSnapshot, JsonRpcError> {
        self.target_debugger(&context_id, &connection_id, &target_id)
            .await?
            .get_coverage(capture_id, source_path, no_cache)
            .await
            .map_err(target_debugger_rpc_error)
    }

    async fn take_heap_snapshot(
        &self,
        _ctx: &CallCtx,
        context_id: String,
        connection_id: String,
        target_id: String,
        path: String,
        capture_numeric_value: bool,
        expose_internals: bool,
    ) -> Result<HeapSnapshotResult, JsonRpcError> {
        self.target_debugger(&context_id, &connection_id, &target_id)
            .await?
            .take_heap_snapshot(path, capture_numeric_value, expose_internals)
            .await
            .map_err(target_debugger_rpc_error)
    }

    async fn capture_heap_snapshot(
        &self,
        _ctx: &CallCtx,
        context_id: String,
        connection_id: String,
        target_id: String,
        capture_id: Option<String>,
        capture_numeric_value: bool,
        expose_internals: bool,
    ) -> Result<HeapCaptureResult, JsonRpcError> {
        self.target_debugger(&context_id, &connection_id, &target_id)
            .await?
            .capture_heap_snapshot(capture_id, capture_numeric_value, expose_internals)
            .await
            .map_err(target_debugger_rpc_error)
    }

    async fn get_heap_classes(
        &self,
        _ctx: &CallCtx,
        context_id: String,
        connection_id: String,
        target_id: String,
        capture_id: String,
        filter: Option<String>,
        no_cache: bool,
    ) -> Result<HeapClassSnapshot, JsonRpcError> {
        self.target_debugger(&context_id, &connection_id, &target_id)
            .await?
            .get_heap_classes(capture_id, filter, no_cache)
            .await
            .map_err(target_debugger_rpc_error)
    }

    async fn get_heap_snapshot_progress(
        &self,
        _ctx: &CallCtx,
        context_id: String,
        connection_id: String,
        target_id: String,
    ) -> Result<Option<HeapSnapshotProgress>, JsonRpcError> {
        Ok(self
            .target_debugger(&context_id, &connection_id, &target_id)
            .await?
            .heap_snapshot_progress())
    }

    async fn shutdown(&self, _ctx: &CallCtx) -> Result<bool, JsonRpcError> {
        let service = self.clone();
        tokio::spawn(async move {
            let runtimes = {
                let mut state = service.state.lock().await;
                state.target_debuggers.clear();
                std::mem::take(&mut state.runtimes)
                    .into_values()
                    .collect::<Vec<_>>()
            };
            for runtime in runtimes {
                runtime.close().await;
            }
            tokio::time::sleep(Duration::from_millis(100)).await;
            service.shutdown.send_replace(true);
        });
        Ok(true)
    }
}

impl DebuggerService {
    async fn target_debugger(
        &self,
        context_id: &str,
        connection_id: &str,
        target_id: &str,
    ) -> Result<TargetDebuggerHandle, JsonRpcError> {
        let target_id = self
            .resolve_target_id(context_id, connection_id, target_id)
            .await?;
        self.state
            .lock()
            .await
            .target_debuggers
            .get(&(
                context_id.to_owned(),
                connection_id.to_owned(),
                target_id.clone(),
            ))
            .cloned()
            .ok_or_else(|| not_found("attached target", &target_id))
    }

    async fn resolve_target_id(
        &self,
        context_id: &str,
        connection_id: &str,
        selector: &str,
    ) -> Result<String, JsonRpcError> {
        let state = self.state.lock().await;
        let connection = state
            .contexts
            .get(context_id)
            .ok_or_else(|| not_found("context", context_id))?
            .connections
            .get(connection_id)
            .ok_or_else(|| not_found("connection", connection_id))?;
        if connection.targets.contains_key(selector) {
            return Ok(selector.to_owned());
        }
        let matches = connection
            .targets
            .values()
            .filter(|target| {
                target.target_type == selector || target.title == selector || target.url == selector
            })
            .map(|target| target.target_id.clone())
            .collect::<Vec<_>>();
        match matches.as_slice() {
            [target_id] => Ok(target_id.clone()),
            [] => Err(not_found("target selector", selector)),
            _ => Err(invalid_params(&format!(
                "target selector '{selector}' is ambiguous across {} targets",
                matches.len()
            ))),
        }
    }
}

async fn detach_session(runtime: &ConnectionRuntime, session_id: &str) {
    if runtime.is_direct_debugger() {
        return;
    }
    runtime.retire_session(session_id);
    let mut detach = TargetDetachFromTargetParams::new();
    detach.session_id = Some(session_id.to_owned());
    let _ = runtime.root().target_detach_from_target(detach).await;
}

async fn connect_runtime(
    configuration: &ConnectionConfiguration,
    connection_generation: u64,
) -> Result<(Arc<ConnectionRuntime>, String, String, Vec<TargetSnapshot>), String> {
    let connection = ConnectionRuntime::connect(configuration, connection_generation)
        .await
        .map_err(|error| error.to_string())?;
    if connection.is_direct_debugger() {
        return Ok((
            connection,
            "Node.js".to_owned(),
            "1.3".to_owned(),
            vec![TargetSnapshot {
                target_id: "$node-root".to_owned(),
                target_type: "node".to_owned(),
                title: "Node.js".to_owned(),
                url: match configuration {
                    ConnectionConfiguration::Node { program, .. } => program.clone(),
                    _ => String::new(),
                },
                attached: true,
                parent_id: None,
                opener_id: None,
                browser_context_id: None,
                subtype: None,
            }],
        ));
    }
    let version = match connection
        .root()
        .browser_get_version(BrowserGetVersionParams::new())
        .await
    {
        Ok(version) => version,
        Err(error) => {
            connection.close().await;
            return Err(format!("Browser.getVersion failed: {error:?}"));
        }
    };
    if let Err(error) = connection
        .root()
        .target_set_discover_targets(TargetSetDiscoverTargetsParams::new(true))
        .await
    {
        connection.close().await;
        return Err(format!("Target.setDiscoverTargets failed: {error:?}"));
    }
    let targets = match connection
        .root()
        .target_get_targets(TargetGetTargetsParams::new())
        .await
    {
        Ok(targets) => targets.target_infos,
        Err(error) => {
            connection.close().await;
            return Err(format!("Target.getTargets failed: {error:?}"));
        }
    };
    Ok((
        connection,
        version.product,
        version.protocol_version,
        targets.into_iter().map(target_snapshot).collect(),
    ))
}

fn target_snapshot(target: TargetTargetInfo) -> TargetSnapshot {
    TargetSnapshot {
        target_id: target.target_id,
        target_type: target.r#type,
        title: target.title,
        url: target.url,
        attached: target.attached,
        parent_id: target.parent_id,
        opener_id: target.opener_id,
        browser_context_id: target.browser_context_id,
        subtype: target.subtype,
    }
}

fn snapshot(agent_instance_id: &str, id: &str, context: &ContextState) -> ContextSnapshot {
    ContextSnapshot {
        agent_instance_id: agent_instance_id.to_owned(),
        id: id.to_owned(),
        display_name: context.display_name.clone(),
        revision: context.revision,
        connections: context
            .connections
            .iter()
            .map(|(id, connection)| ConnectionSnapshot {
                id: id.clone(),
                configuration: connection.configuration.clone(),
                generation: connection.generation,
                status: connection.status.clone(),
                targets: connection.targets.values().cloned().collect(),
            })
            .collect(),
        breakpoints: context
            .breakpoints
            .iter()
            .map(|(id, breakpoint)| BreakpointSnapshot {
                id: id.clone(),
                source_path: breakpoint.source_path.clone(),
                line: breakpoint.line,
                column: breakpoint.column,
                status: if breakpoint.enabled {
                    BreakpointStatus::Pending
                } else {
                    BreakpointStatus::Disabled
                },
                enabled: breakpoint.enabled,
                condition: breakpoint.condition.clone(),
                target_selector: breakpoint.target_selector.clone(),
            })
            .collect(),
    }
}

fn service_snapshot(
    state: &ServiceState,
    agent_instance_id: &str,
    id: &str,
) -> Option<ContextSnapshot> {
    let context = state.contexts.get(id)?;
    let mut result = snapshot(agent_instance_id, id, context);
    for breakpoint in &mut result.breakpoints {
        let specification = context
            .breakpoints
            .get(&breakpoint.id)
            .expect("snapshot breakpoint originates from context state");
        if !specification.enabled {
            breakpoint.status = BreakpointStatus::Disabled;
            continue;
        }
        let applications = state
            .target_debuggers
            .iter()
            .filter(|((candidate_context, _, target_id), _)| {
                candidate_context == id
                    && specification
                        .target_selector
                        .as_ref()
                        .is_none_or(|selector| selector == target_id)
            })
            .filter_map(|(_, debugger)| {
                debugger
                    .snapshot()
                    .breakpoints
                    .into_iter()
                    .find(|candidate| candidate.id == breakpoint.id)
            })
            .collect::<Vec<_>>();
        let installed = applications
            .iter()
            .filter(|application| {
                matches!(
                    application.status,
                    crate::service_api::TargetBreakpointStatus::Installed { .. }
                )
            })
            .count() as u32;
        breakpoint.status = if applications.is_empty() {
            BreakpointStatus::Pending
        } else if installed == applications.len() as u32 {
            BreakpointStatus::Bound {
                application_count: installed,
            }
        } else if installed > 0 {
            BreakpointStatus::PartiallyBound {
                application_count: installed,
            }
        } else if let Some(message) = applications
            .iter()
            .find_map(|application| match &application.status {
                crate::service_api::TargetBreakpointStatus::Failed { message } => {
                    Some(message.clone())
                }
                _ => None,
            })
        {
            BreakpointStatus::Failed { message }
        } else {
            BreakpointStatus::Pending
        };
    }
    Some(result)
}

#[derive(Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
struct StoredServiceState {
    schema_version: u32,
    contexts: BTreeMap<String, StoredContextState>,
    #[serde(default)]
    completed_requests: Vec<StoredCompletedRequest>,
}

#[derive(Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
struct StoredCompletedRequest {
    context_id: String,
    request_id: String,
    revision: u64,
}

#[derive(Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
struct StoredContextState {
    display_name: String,
    revision: u64,
    connections: BTreeMap<String, StoredConnectionState>,
    breakpoints: BTreeMap<String, StoredBreakpointState>,
}

#[derive(Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
struct StoredConnectionState {
    configuration: ConnectionConfiguration,
    configuration_version: u64,
}

#[derive(Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
struct StoredBreakpointState {
    source_path: String,
    line: u32,
    column: u32,
    #[serde(default = "stored_default_true")]
    enabled: bool,
    #[serde(default)]
    condition: Option<String>,
    #[serde(default)]
    target_selector: Option<String>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct StoredServiceStateV1 {
    contexts: BTreeMap<String, StoredContextStateV1>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct StoredContextStateV1 {
    display_name: String,
    revision: u64,
    connections: BTreeMap<String, StoredConnectionStateV1>,
    breakpoints: BTreeMap<String, StoredBreakpointState>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct StoredConnectionStateV1 {
    endpoint: String,
    configuration_version: u64,
}

impl From<&ServiceState> for StoredServiceState {
    fn from(state: &ServiceState) -> Self {
        Self {
            schema_version: 2,
            contexts: state
                .contexts
                .iter()
                .map(|(id, context)| {
                    (
                        id.clone(),
                        StoredContextState {
                            display_name: context.display_name.clone(),
                            revision: context.revision,
                            connections: context
                                .connections
                                .iter()
                                .map(|(id, connection)| {
                                    (
                                        id.clone(),
                                        StoredConnectionState {
                                            configuration: connection.configuration.clone(),
                                            configuration_version: connection.configuration_version,
                                        },
                                    )
                                })
                                .collect(),
                            breakpoints: context
                                .breakpoints
                                .iter()
                                .map(|(id, breakpoint)| {
                                    (
                                        id.clone(),
                                        StoredBreakpointState {
                                            source_path: breakpoint.source_path.clone(),
                                            line: breakpoint.line,
                                            column: breakpoint.column,
                                            enabled: breakpoint.enabled,
                                            condition: breakpoint.condition.clone(),
                                            target_selector: breakpoint.target_selector.clone(),
                                        },
                                    )
                                })
                                .collect(),
                        },
                    )
                })
                .collect(),
            completed_requests: state
                .completed_requests
                .iter()
                .map(
                    |((context_id, request_id), revision)| StoredCompletedRequest {
                        context_id: context_id.clone(),
                        request_id: request_id.clone(),
                        revision: *revision,
                    },
                )
                .collect(),
        }
    }
}

fn load_state(path: &Path) -> Result<ServiceState, ServicePersistenceError> {
    if !path.exists() {
        return Ok(ServiceState::default());
    }
    let bytes = fs::read(path)?;
    let schema_version = serde_json::from_slice::<serde_json::Value>(&bytes)?
        .get("schemaVersion")
        .and_then(serde_json::Value::as_u64)
        .ok_or(ServicePersistenceError::MissingSchemaVersion)?;
    let stored = match schema_version {
        1 => migrate_v1(serde_json::from_slice(&bytes)?),
        2 => serde_json::from_slice(&bytes)?,
        version => return Err(ServicePersistenceError::UnsupportedSchema(version as u32)),
    };
    let completed_requests = stored
        .completed_requests
        .into_iter()
        .map(|request| ((request.context_id, request.request_id), request.revision))
        .collect();
    Ok(ServiceState {
        contexts: stored
            .contexts
            .into_iter()
            .map(|(id, context)| {
                (
                    id,
                    Arc::new(ContextState {
                        display_name: context.display_name,
                        revision: context.revision,
                        connections: Arc::new(
                            context
                                .connections
                                .into_iter()
                                .map(|(id, connection)| {
                                    (
                                        id,
                                        Arc::new(ConnectionState {
                                            configuration: connection.configuration,
                                            configuration_version: connection.configuration_version,
                                            generation: 0,
                                            status: ConnectionStatus::Disconnected,
                                            targets: Arc::new(BTreeMap::new()),
                                        }),
                                    )
                                })
                                .collect(),
                        ),
                        breakpoints: Arc::new(
                            context
                                .breakpoints
                                .into_iter()
                                .map(|(id, breakpoint)| {
                                    (
                                        id,
                                        Arc::new(BreakpointState {
                                            source_path: breakpoint.source_path,
                                            line: breakpoint.line,
                                            column: breakpoint.column,
                                            enabled: breakpoint.enabled,
                                            condition: breakpoint.condition,
                                            target_selector: breakpoint.target_selector,
                                        }),
                                    )
                                })
                                .collect(),
                        ),
                    }),
                )
            })
            .collect(),
        runtimes: BTreeMap::new(),
        target_debuggers: BTreeMap::new(),
        history: BTreeMap::new(),
        completed_requests,
    })
}

fn stored_default_true() -> bool {
    true
}

fn migrate_v1(stored: StoredServiceStateV1) -> StoredServiceState {
    StoredServiceState {
        schema_version: 2,
        contexts: stored
            .contexts
            .into_iter()
            .map(|(id, context)| {
                (
                    id,
                    StoredContextState {
                        display_name: context.display_name,
                        revision: context.revision,
                        connections: context
                            .connections
                            .into_iter()
                            .map(|(id, connection)| {
                                (
                                    id,
                                    StoredConnectionState {
                                        configuration: connection.endpoint.into(),
                                        configuration_version: connection.configuration_version,
                                    },
                                )
                            })
                            .collect(),
                        breakpoints: context.breakpoints,
                    },
                )
            })
            .collect(),
        completed_requests: Vec::new(),
    }
}

fn random_instance_id() -> Result<String, ServicePersistenceError> {
    let mut bytes = [0_u8; 16];
    getrandom::fill(&mut bytes)
        .map_err(|error| ServicePersistenceError::Random(error.to_string()))?;
    Ok(bytes.iter().map(|byte| format!("{byte:02x}")).collect())
}

#[derive(Debug, thiserror::Error)]
pub enum ServicePersistenceError {
    #[error(transparent)]
    Io(#[from] std::io::Error),
    #[error(transparent)]
    Json(#[from] serde_json::Error),
    #[error("unsupported debugger state schema version {0}")]
    UnsupportedSchema(u32),
    #[error("debugger state does not declare a schema version")]
    MissingSchemaVersion,
    #[error("failed to generate agent instance identity: {0}")]
    Random(String),
}

fn validate_id(kind: &str, id: &str) -> Result<(), JsonRpcError> {
    if id.is_empty()
        || !id
            .chars()
            .all(|character| character.is_ascii_alphanumeric() || "-_.".contains(character))
    {
        return Err(invalid_params(&format!(
            "{kind} id must contain only ASCII letters, digits, '-', '_', or '.'"
        )));
    }
    Ok(())
}

fn validate_connection_configuration(
    configuration: &ConnectionConfiguration,
) -> Result<(), JsonRpcError> {
    validate_configuration(configuration).map_err(|error| invalid_params(&error.to_string()))
}

fn validate_breakpoint_spec(specification: &BreakpointSpec) -> Result<(), JsonRpcError> {
    if specification.source_path.is_empty() {
        return Err(invalid_params("source path must not be empty"));
    }
    if specification.line == 0 || specification.column == 0 {
        return Err(invalid_params("breakpoint lines and columns are one-based"));
    }
    if specification.condition.as_deref() == Some("") {
        return Err(invalid_params("breakpoint condition must not be empty"));
    }
    Ok(())
}

fn source_file_path(path: &str) -> Result<PathBuf, JsonRpcError> {
    if let Ok(url) = url::Url::parse(path) {
        if url.scheme() != "file" {
            return Err(invalid_params(
                "source content is currently available only for local file sources",
            ));
        }
        return url
            .to_file_path()
            .map_err(|_| invalid_params("source file URL is invalid"));
    }
    Ok(PathBuf::from(path))
}

fn stable_name_hash(value: &str) -> u64 {
    value.bytes().fold(0xcbf29ce484222325, |hash, byte| {
        (hash ^ u64::from(byte)).wrapping_mul(0x100000001b3)
    })
}

fn sanitize_file_name(value: &str) -> String {
    value
        .chars()
        .map(|character| {
            if character.is_ascii_alphanumeric() || ".-_".contains(character) {
                character
            } else {
                '_'
            }
        })
        .collect()
}

fn invalid_params(message: &str) -> JsonRpcError {
    JsonRpcError::new(error_codes::INVALID_PARAMS, message)
}

fn invalid_state(message: &str) -> JsonRpcError {
    JsonRpcError::new(error_codes::INVALID_REQUEST, message)
}

fn internal_error(message: impl Into<String>) -> JsonRpcError {
    JsonRpcError::new(error_codes::INTERNAL_ERROR, message.into())
}

fn cdp_rpc_error(operation: &str, error: JsonRpcError) -> JsonRpcError {
    internal_error(format!("{operation} failed: {error:?}"))
}

fn target_debugger_rpc_error(error: TargetDebuggerError) -> JsonRpcError {
    let code = match error {
        TargetDebuggerError::InvalidBreakpointPosition
        | TargetDebuggerError::StalePause(_)
        | TargetDebuggerError::FrameNotFound(_)
        | TargetDebuggerError::SelectorNotFound(_)
        | TargetDebuggerError::UnsupportedKeyChord(_)
        | TargetDebuggerError::CoverageAlreadyActive
        | TargetDebuggerError::CoverageNotActive
        | TargetDebuggerError::CoverageCaptureNotFound(_)
        | TargetDebuggerError::CoverageCaptureAlreadyExists(_)
        | TargetDebuggerError::HeapCaptureNotFound(_)
        | TargetDebuggerError::InvalidHeapFilter(_)
        | TargetDebuggerError::InvalidTimeout => error_codes::INVALID_PARAMS,
        TargetDebuggerError::WaitTimedOut
        | TargetDebuggerError::SettlementTimedOut
        | TargetDebuggerError::Stopped
        | TargetDebuggerError::SessionMissing
        | TargetDebuggerError::BreakpointFailed { .. }
        | TargetDebuggerError::Evaluation(_)
        | TargetDebuggerError::Interaction(_)
        | TargetDebuggerError::Coverage(_)
        | TargetDebuggerError::HeapSnapshot(_)
        | TargetDebuggerError::BatchRollback { .. }
        | TargetDebuggerError::DriverFailed(_)
        | TargetDebuggerError::Driver(_) => error_codes::INTERNAL_ERROR,
    };
    JsonRpcError::new(code, error.to_string())
}

fn not_found(kind: &str, id: &str) -> JsonRpcError {
    JsonRpcError::new(
        error_codes::INVALID_PARAMS,
        format!("{kind} '{id}' does not exist"),
    )
}

fn transition_rpc_error(error: ContextTransitionError) -> JsonRpcError {
    let code = match error {
        ContextTransitionError::StaleEffectCompletion { .. } => error_codes::INTERNAL_ERROR,
        ContextTransitionError::ConnectionNotFound(_)
        | ContextTransitionError::ActiveConnectionCannotBeReplaced
        | ContextTransitionError::ActiveConnectionCannotBeRemoved
        | ContextTransitionError::ConnectionAlreadyActive
        | ContextTransitionError::ConnectionAlreadyDisconnecting => error_codes::INVALID_PARAMS,
    };
    JsonRpcError::new(code, error.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn migrates_endpoint_only_persistence_to_direct_cdp_configuration() {
        let path = std::env::temp_dir().join(format!(
            "jsdbg-persistence-v1-{}-{}.json",
            std::process::id(),
            random_instance_id().unwrap()
        ));
        fs::write(
            &path,
            br#"{
                "schemaVersion": 1,
                "contexts": {
                    "legacy": {
                        "displayName": "Legacy",
                        "revision": 2,
                        "connections": {
                            "browser": {
                                "endpoint": "ws://127.0.0.1:9222",
                                "configurationVersion": 1
                            }
                        },
                        "breakpoints": {}
                    }
                }
            }"#,
        )
        .unwrap();

        let state = load_state(&path).unwrap();
        let connection = &state.contexts["legacy"].connections["browser"];
        assert_eq!(
            connection.configuration,
            ConnectionConfiguration::DirectCdp {
                endpoint: "ws://127.0.0.1:9222".into()
            }
        );
        let _ = fs::remove_file(path);
    }
}
