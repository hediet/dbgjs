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
    BrowserGetVersionParams, BrowserGetVersionResult, TargetGetTargetsParams, TargetTargetInfo,
};
use crate::cdp_runtime::CdpConnection;
use crate::context_engine::{
    BreakpointState, ConnectionAttempt, ConnectionState, ContextEffect, ContextInput, ContextState,
    ContextTransitionError, EffectCompletion, RuntimeObservation, UserCommand, reduce_context,
};
use crate::service_api::{
    BreakpointSnapshot, BreakpointStatus, ConnectionSnapshot, ConnectionStatus, ContextSnapshot,
    ContextSummary, DebuggerServiceApi, SERVICE_PROTOCOL_VERSION, ServiceInfo, TargetSnapshot,
};

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
        runtime: Arc<CdpConnection>,
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
            if !is_current_runtime {
                return;
            }

            state.runtimes.remove(&runtime_key);
            let Some(context) = state.contexts.get(&context_id).cloned() else {
                return;
            };
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
        });
    }
}

#[derive(Clone, Default)]
struct ServiceState {
    contexts: BTreeMap<String, Arc<ContextState>>,
    runtimes: BTreeMap<(String, String), Arc<CdpConnection>>,
}

#[async_trait::async_trait]
impl DebuggerServiceApi for DebuggerService {
    async fn service_info(&self, _ctx: &CallCtx) -> Result<ServiceInfo, JsonRpcError> {
        Ok(ServiceInfo {
            process_id: std::process::id(),
            protocol_version: SERVICE_PROTOCOL_VERSION,
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
        endpoint: String,
    ) -> Result<ContextSnapshot, JsonRpcError> {
        validate_id("connection", &connection_id)?;
        if !endpoint.starts_with("ws://") && !endpoint.starts_with("wss://") {
            return Err(invalid_params(
                "connection endpoint must use ws:// or wss://",
            ));
        }

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
                endpoint,
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
        let (endpoint, attempt) = {
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
            let (endpoint, attempt) = match transition.effects.as_slice() {
                [
                    ContextEffect::Connect {
                        endpoint, attempt, ..
                    },
                ] => (endpoint.clone(), *attempt),
                effects => panic!("connect command emitted unexpected effects: {effects:?}"),
            };
            state.contexts.insert(context_id.clone(), transition.state);
            (endpoint, attempt)
        };

        let connected = connect_runtime(&endpoint).await;
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
        state.contexts.insert(context_id.clone(), transition.state);
        if let Some(runtime) = runtime {
            state.runtimes.insert(runtime_key, runtime.clone());
            self.supervise_runtime(
                context_id,
                connection_id,
                attempt.configuration_version,
                attempt.generation,
                runtime,
            );
        } else {
            state.runtimes.remove(&runtime_key);
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
        if line == 0 {
            return Err(invalid_params("breakpoint lines are one-based"));
        }
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
        Ok(result)
    }

    async fn shutdown(&self, _ctx: &CallCtx) -> Result<bool, JsonRpcError> {
        let shutdown = self.shutdown.clone();
        tokio::spawn(async move {
            tokio::time::sleep(Duration::from_millis(100)).await;
            shutdown.send_replace(true);
        });
        Ok(true)
    }
}

async fn connect_runtime(
    endpoint: &str,
) -> Result<
    (
        Arc<CdpConnection>,
        BrowserGetVersionResult,
        Vec<TargetTargetInfo>,
    ),
    String,
> {
    let connection = Arc::new(
        CdpConnection::connect(endpoint)
            .await
            .map_err(|error| error.to_string())?,
    );
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
                endpoint: connection.endpoint.clone(),
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
    endpoint: String,
    configuration_version: u64,
}

#[derive(Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
struct StoredBreakpointState {
    source_path: String,
    line: u32,
    column: u32,
}

impl From<&ServiceState> for StoredServiceState {
    fn from(state: &ServiceState) -> Self {
        Self {
            schema_version: 1,
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
                                            endpoint: connection.endpoint.clone(),
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
    let stored: StoredServiceState = serde_json::from_slice(&fs::read(path)?)?;
    if stored.schema_version != 1 {
        return Err(ServicePersistenceError::UnsupportedSchema(
            stored.schema_version,
        ));
    }
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
                                            endpoint: connection.endpoint,
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
    })
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

fn invalid_params(message: &str) -> JsonRpcError {
    JsonRpcError::new(error_codes::INVALID_PARAMS, message)
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
