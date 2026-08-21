use std::collections::BTreeMap;
use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use atomic_write_file::AtomicWriteFile;
use hubrpc::prelude::{CallCtx, JsonRpcError, error_codes};
use serde::{Deserialize, Serialize};
use tokio::sync::{Mutex, watch};

use crate::cdp::{
    BrowserGetVersionParams, BrowserGetVersionResult, TargetAttachToTargetParams,
    TargetDetachFromTargetParams, TargetGetTargetsParams, TargetTargetInfo,
};
use crate::connection_provider::{ConnectionRuntime, validate_configuration};
use crate::context_engine::{
    BreakpointState, ConnectionAttempt, ConnectionState, ContextEffect, ContextInput, ContextState,
    ContextTransitionError, EffectCompletion, RuntimeObservation, UserCommand, reduce_context,
};
use crate::debugger_engine::{SessionKey, StepKind};
use crate::service_api::{
    BreakpointSnapshot, BreakpointStatus, ConnectionConfiguration, ConnectionSnapshot,
    ConnectionStatus, ContextSnapshot, ContextSummary, CoverageSnapshot, DebuggerServiceApi,
    EvaluationSnapshot, HeapCaptureResult, HeapClassSnapshot, HeapSnapshotProgress,
    HeapSnapshotResult, LogpointSpec, ServiceInfo, StepKind as ApiStepKind, TargetDebuggerSnapshot,
    TargetSnapshot, TargetWaitPredicate,
};
use crate::target_debugger::{TargetBreakpointSpec, TargetDebuggerError, TargetDebuggerHandle};

#[derive(Clone)]
pub struct DebuggerService {
    agent_instance_id: String,
    state: Arc<Mutex<ServiceState>>,
    persistence_path: PathBuf,
    shutdown: watch::Sender<bool>,
}

impl DebuggerService {
    pub fn load(
        shutdown: watch::Sender<bool>,
        persistence_path: PathBuf,
    ) -> Result<Self, ServicePersistenceError> {
        let state = load_state(&persistence_path)?;
        Ok(Self {
            agent_instance_id: random_instance_id()?,
            state: Arc::new(Mutex::new(state)),
            persistence_path,
            shutdown,
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
                    state.contexts.insert(context_id, transition.state);
                }
            }
            drop(state);
            runtime.close().await;
        });
    }
}

#[derive(Clone, Default)]
struct ServiceState {
    contexts: BTreeMap<String, Arc<ContextState>>,
    runtimes: BTreeMap<(String, String), Arc<ConnectionRuntime>>,
    target_debuggers: BTreeMap<(String, String, String), TargetDebuggerHandle>,
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
        let result = snapshot(&self.agent_instance_id, &context_id, &transition.state);
        state.contexts.insert(context_id.clone(), transition.state);
        self.persist_or_restore(&mut state, previous)?;
        Ok(result)
    }

    async fn get_context(
        &self,
        _ctx: &CallCtx,
        context_id: String,
    ) -> Result<ContextSnapshot, JsonRpcError> {
        let state = self.state.lock().await;
        let context = state
            .contexts
            .get(&context_id)
            .ok_or_else(|| not_found("context", &context_id))?;
        Ok(snapshot(&self.agent_instance_id, &context_id, context))
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
        let result = snapshot(&self.agent_instance_id, &context_id, &transition.state);
        state.contexts.insert(context_id.clone(), transition.state);
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
            state.contexts.insert(context_id.clone(), transition.state);
            (configuration, attempt)
        };

        let connected = connect_runtime(&configuration).await;
        let mut state = self.state.lock().await;
        let context = state
            .contexts
            .get(&context_id)
            .cloned()
            .ok_or_else(|| not_found("context", &context_id))?;
        let runtime_key = (context_id.clone(), connection_id.clone());
        let (completion, runtime) = match connected {
            Ok((runtime, version, targets)) => (
                EffectCompletion::ConnectionOpened {
                    connection_id: connection_id.clone(),
                    attempt,
                    product: version.product,
                    protocol_version: version.protocol_version,
                    targets: targets
                        .into_iter()
                        .map(|target| (target.target_id.clone(), target_snapshot(target)))
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
                    .filter(|target| target.target_type == "page")
                    .map(|target| target.target_id.clone())
                    .collect::<Vec<_>>()
            })
            .unwrap_or_default();
        state.contexts.insert(context_id.clone(), transition.state);
        if let Some(runtime) = runtime {
            state.runtimes.insert(runtime_key, runtime.clone());
            self.supervise_runtime(
                context_id.clone(),
                connection_id.clone(),
                attempt.configuration_version,
                attempt.generation,
                runtime,
            );
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
            state.contexts.insert(context_id.clone(), transition.state);
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
        let result = snapshot(&self.agent_instance_id, &context_id, &transition.state);
        state.contexts.insert(context_id.clone(), transition.state);
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
                }),
            )
            .map_err(transition_rpc_error)?;
            let result = snapshot(&self.agent_instance_id, &context_id, &transition.state);
            state.contexts.insert(context_id.clone(), transition.state);
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
        Ok(result)
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
                detach_session(&runtime, &attached.session_id).await;
                return Err(internal_error(error.to_string()));
            }
        };
        let debugger = match TargetDebuggerHandle::start(
            context_id.clone(),
            connection_id.clone(),
            target_id.clone(),
            generation,
            session,
            session_key,
        )
        .await
        {
            Ok(debugger) => debugger,
            Err(error) => {
                detach_session(&runtime, &attached.session_id).await;
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
            detach_session(&runtime, &attached.session_id).await;
            return Err(invalid_state(
                "connection changed while the target was being attached",
            ));
        }
        if let Some(existing) = state.target_debuggers.get(&debugger_key) {
            let snapshot = existing.snapshot();
            drop(state);
            detach_session(&runtime, &attached.session_id).await;
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
            .map(|(id, breakpoint)| TargetBreakpointSpec {
                id: id.clone(),
                source_url: breakpoint.source_path.clone(),
                line: breakpoint.line,
                column: breakpoint.column,
                condition: None,
            })
            .collect::<Vec<_>>();
        drop(state);

        for breakpoint in breakpoints {
            if let Err(error) = debugger.set_breakpoint(context_revision, breakpoint).await {
                self.state
                    .lock()
                    .await
                    .target_debuggers
                    .remove(&debugger_key);
                detach_session(&runtime, &attached.session_id).await;
                return Err(target_debugger_rpc_error(error));
            }
        }
        Ok(debugger.settle(Duration::from_millis(200)).await)
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
    runtime.retire_session(session_id);
    let mut detach = TargetDetachFromTargetParams::new();
    detach.session_id = Some(session_id.to_owned());
    let _ = runtime.root().target_detach_from_target(detach).await;
}

async fn connect_runtime(
    configuration: &ConnectionConfiguration,
) -> Result<
    (
        Arc<ConnectionRuntime>,
        BrowserGetVersionResult,
        Vec<TargetTargetInfo>,
    ),
    String,
> {
    let connection = ConnectionRuntime::connect(configuration)
        .await
        .map_err(|error| error.to_string())?;
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
    Ok((connection, version, targets))
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
                status: BreakpointStatus::Unconfirmed,
            })
            .collect(),
    }
}

#[derive(Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
struct StoredServiceState {
    schema_version: u32,
    contexts: BTreeMap<String, StoredContextState>,
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
                                        },
                                    )
                                })
                                .collect(),
                        },
                    )
                })
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
    })
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
