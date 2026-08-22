use std::collections::BTreeMap;
use std::sync::Arc;

use serde::{Deserialize, Serialize};

use crate::service_api::{ConnectionConfiguration, ConnectionStatus, TargetSnapshot};

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ContextState {
    pub display_name: String,
    pub revision: u64,
    pub connections: Arc<BTreeMap<String, Arc<ConnectionState>>>,
    pub breakpoints: Arc<BTreeMap<String, Arc<BreakpointState>>>,
}

impl ContextState {
    pub fn new(default_display_name: String) -> Arc<Self> {
        Arc::new(Self {
            display_name: default_display_name,
            revision: 0,
            connections: Arc::new(BTreeMap::new()),
            breakpoints: Arc::new(BTreeMap::new()),
        })
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ConnectionState {
    pub configuration: ConnectionConfiguration,
    pub configuration_version: u64,
    pub generation: u64,
    pub status: ConnectionStatus,
    pub targets: Arc<BTreeMap<String, TargetSnapshot>>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct BreakpointState {
    pub source_path: String,
    pub line: u32,
    pub column: u32,
    #[serde(default = "default_true")]
    pub enabled: bool,
    #[serde(default)]
    pub condition: Option<String>,
    #[serde(default)]
    pub target_selector: Option<String>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ConnectionAttempt {
    pub configuration_version: u64,
    pub generation: u64,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "category", content = "input", rename_all = "camelCase")]
pub enum ContextInput {
    UserCommand(UserCommand),
    RuntimeObservation(RuntimeObservation),
    EffectCompletion(EffectCompletion),
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "camelCase")]
pub enum UserCommand {
    PutContext {
        display_name: Option<String>,
    },
    PutConnection {
        connection_id: String,
        configuration: ConnectionConfiguration,
    },
    RemoveConnection {
        connection_id: String,
    },
    ConnectConnection {
        connection_id: String,
    },
    DisconnectConnection {
        connection_id: String,
    },
    PutBreakpoint {
        breakpoint_id: String,
        source_path: String,
        line: u32,
        column: u32,
        enabled: bool,
        condition: Option<String>,
        target_selector: Option<String>,
    },
    RemoveBreakpoint {
        breakpoint_id: String,
    },
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "camelCase")]
pub enum RuntimeObservation {
    ConnectionClosed {
        connection_id: String,
        attempt: ConnectionAttempt,
        reason: String,
    },
    TargetUpserted {
        connection_id: String,
        attempt: ConnectionAttempt,
        target: TargetSnapshot,
    },
    TargetRemoved {
        connection_id: String,
        attempt: ConnectionAttempt,
        target_id: String,
    },
    BreakpointApplicationsChanged {
        breakpoint_id: String,
    },
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "camelCase")]
pub enum EffectCompletion {
    ConnectionOpened {
        connection_id: String,
        attempt: ConnectionAttempt,
        product: String,
        protocol_version: String,
        targets: BTreeMap<String, TargetSnapshot>,
    },
    ConnectionOpenFailed {
        connection_id: String,
        attempt: ConnectionAttempt,
        message: String,
    },
    ConnectionClosed {
        connection_id: String,
        attempt: ConnectionAttempt,
    },
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "camelCase")]
pub enum ContextEffect {
    Connect {
        connection_id: String,
        configuration: ConnectionConfiguration,
        attempt: ConnectionAttempt,
    },
    Disconnect {
        connection_id: String,
        attempt: ConnectionAttempt,
    },
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "camelCase")]
pub enum ContextEvent {
    ContextUpdated,
    ConnectionConfigured {
        connection_id: String,
    },
    ConnectionConnecting {
        connection_id: String,
        attempt: ConnectionAttempt,
    },
    ConnectionConnected {
        connection_id: String,
        attempt: ConnectionAttempt,
    },
    ConnectionFailed {
        connection_id: String,
        attempt: ConnectionAttempt,
        message: String,
    },
    ConnectionDisconnecting {
        connection_id: String,
        attempt: ConnectionAttempt,
    },
    ConnectionDisconnected {
        connection_id: String,
        attempt: ConnectionAttempt,
    },
    BreakpointUpdated {
        breakpoint_id: String,
    },
    BreakpointRemoved {
        breakpoint_id: String,
    },
    ConnectionRemoved {
        connection_id: String,
    },
    TargetCreated {
        connection_id: String,
        target_id: String,
    },
    TargetChanged {
        connection_id: String,
        target_id: String,
    },
    TargetDestroyed {
        connection_id: String,
        target_id: String,
    },
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RevisionEvent {
    pub revision: u64,
    pub event: ContextEvent,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ContextChange {
    None,
    Durable,
    RuntimeOnly,
}

#[derive(Clone, Debug)]
pub struct ContextTransition {
    pub state: Arc<ContextState>,
    pub effects: Vec<ContextEffect>,
    pub events: Vec<RevisionEvent>,
    pub change: ContextChange,
}

impl ContextTransition {
    fn unchanged(state: &Arc<ContextState>) -> Self {
        Self {
            state: state.clone(),
            effects: Vec::new(),
            events: Vec::new(),
            change: ContextChange::None,
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
pub enum ContextTransitionError {
    #[error("connection '{0}' does not exist")]
    ConnectionNotFound(String),
    #[error("an active connection cannot be replaced until it is disconnected")]
    ActiveConnectionCannotBeReplaced,
    #[error("connection is already active")]
    ConnectionAlreadyActive,
    #[error("connection is already disconnecting")]
    ConnectionAlreadyDisconnecting,
    #[error("an active connection cannot be removed until it is disconnected")]
    ActiveConnectionCannotBeRemoved,
    #[error("connection '{connection_id}' changed while the operation was pending")]
    StaleEffectCompletion { connection_id: String },
}

pub fn reduce_context(
    previous: &Arc<ContextState>,
    input: ContextInput,
) -> Result<ContextTransition, ContextTransitionError> {
    match input {
        ContextInput::UserCommand(command) => reduce_user_command(previous, command),
        ContextInput::RuntimeObservation(observation) => {
            reduce_runtime_observation(previous, observation)
        }
        ContextInput::EffectCompletion(completion) => {
            reduce_effect_completion(previous, completion)
        }
    }
}

fn reduce_user_command(
    previous: &Arc<ContextState>,
    command: UserCommand,
) -> Result<ContextTransition, ContextTransitionError> {
    match command {
        UserCommand::PutContext { display_name } => {
            let mut state = (**previous).clone();
            if let Some(display_name) = display_name {
                state.display_name = display_name;
            }
            Ok(changed(
                state,
                ContextChange::Durable,
                Vec::new(),
                ContextEvent::ContextUpdated,
            ))
        }
        UserCommand::PutConnection {
            connection_id,
            configuration,
        } => {
            if previous
                .connections
                .get(&connection_id)
                .is_some_and(|connection| is_active(&connection.status))
            {
                return Err(ContextTransitionError::ActiveConnectionCannotBeReplaced);
            }

            let mut state = (**previous).clone();
            let connections = Arc::make_mut(&mut state.connections);
            let configuration_version = connections
                .get(&connection_id)
                .map_or(1, |connection| connection.configuration_version + 1);
            let generation = connections
                .get(&connection_id)
                .map_or(0, |connection| connection.generation);
            connections.insert(
                connection_id.clone(),
                Arc::new(ConnectionState {
                    configuration,
                    configuration_version,
                    generation,
                    status: ConnectionStatus::Disconnected,
                    targets: Arc::new(BTreeMap::new()),
                }),
            );
            Ok(changed(
                state,
                ContextChange::Durable,
                Vec::new(),
                ContextEvent::ConnectionConfigured { connection_id },
            ))
        }
        UserCommand::RemoveConnection { connection_id } => {
            let connection = previous
                .connections
                .get(&connection_id)
                .ok_or_else(|| ContextTransitionError::ConnectionNotFound(connection_id.clone()))?;
            if is_active(&connection.status) {
                return Err(ContextTransitionError::ActiveConnectionCannotBeRemoved);
            }
            let mut state = (**previous).clone();
            Arc::make_mut(&mut state.connections).remove(&connection_id);
            Ok(changed(
                state,
                ContextChange::Durable,
                Vec::new(),
                ContextEvent::ConnectionRemoved { connection_id },
            ))
        }
        UserCommand::ConnectConnection { connection_id } => {
            let connection = previous
                .connections
                .get(&connection_id)
                .ok_or_else(|| ContextTransitionError::ConnectionNotFound(connection_id.clone()))?;
            if is_active(&connection.status) {
                return Err(ContextTransitionError::ConnectionAlreadyActive);
            }

            let attempt = ConnectionAttempt {
                configuration_version: connection.configuration_version,
                generation: connection.generation + 1,
            };
            let configuration = connection.configuration.clone();
            let mut state = (**previous).clone();
            let connections = Arc::make_mut(&mut state.connections);
            let connection = Arc::make_mut(
                connections
                    .get_mut(&connection_id)
                    .expect("connection was checked above"),
            );
            connection.generation = attempt.generation;
            connection.status = ConnectionStatus::Connecting;
            connection.targets = Arc::new(BTreeMap::new());
            Ok(changed(
                state,
                ContextChange::RuntimeOnly,
                vec![ContextEffect::Connect {
                    connection_id: connection_id.clone(),
                    configuration,
                    attempt,
                }],
                ContextEvent::ConnectionConnecting {
                    connection_id,
                    attempt,
                },
            ))
        }
        UserCommand::DisconnectConnection { connection_id } => {
            let connection = previous
                .connections
                .get(&connection_id)
                .ok_or_else(|| ContextTransitionError::ConnectionNotFound(connection_id.clone()))?;
            if connection.status == ConnectionStatus::Disconnected {
                return Ok(ContextTransition::unchanged(previous));
            }
            if connection.status == ConnectionStatus::Disconnecting {
                return Err(ContextTransitionError::ConnectionAlreadyDisconnecting);
            }

            let attempt = attempt(connection);
            let mut state = (**previous).clone();
            let connections = Arc::make_mut(&mut state.connections);
            let connection = Arc::make_mut(
                connections
                    .get_mut(&connection_id)
                    .expect("connection was checked above"),
            );
            connection.status = ConnectionStatus::Disconnecting;
            Ok(changed(
                state,
                ContextChange::RuntimeOnly,
                vec![ContextEffect::Disconnect {
                    connection_id: connection_id.clone(),
                    attempt,
                }],
                ContextEvent::ConnectionDisconnecting {
                    connection_id,
                    attempt,
                },
            ))
        }
        UserCommand::PutBreakpoint {
            breakpoint_id,
            source_path,
            line,
            column,
            enabled,
            condition,
            target_selector,
        } => {
            let mut state = (**previous).clone();
            Arc::make_mut(&mut state.breakpoints).insert(
                breakpoint_id.clone(),
                Arc::new(BreakpointState {
                    source_path,
                    line,
                    column,
                    enabled,
                    condition,
                    target_selector,
                }),
            );
            Ok(changed(
                state,
                ContextChange::Durable,
                Vec::new(),
                ContextEvent::BreakpointUpdated { breakpoint_id },
            ))
        }
        UserCommand::RemoveBreakpoint { breakpoint_id } => {
            if !previous.breakpoints.contains_key(&breakpoint_id) {
                return Ok(ContextTransition::unchanged(previous));
            }
            let mut state = (**previous).clone();
            Arc::make_mut(&mut state.breakpoints).remove(&breakpoint_id);
            Ok(changed(
                state,
                ContextChange::Durable,
                Vec::new(),
                ContextEvent::BreakpointRemoved { breakpoint_id },
            ))
        }
    }
}

fn reduce_runtime_observation(
    previous: &Arc<ContextState>,
    observation: RuntimeObservation,
) -> Result<ContextTransition, ContextTransitionError> {
    match observation {
        RuntimeObservation::ConnectionClosed {
            connection_id,
            attempt: observed_attempt,
            reason,
        } => {
            let Some(connection) = previous.connections.get(&connection_id) else {
                return Ok(ContextTransition::unchanged(previous));
            };
            if attempt(connection) != observed_attempt
                || !matches!(connection.status, ConnectionStatus::Connected { .. })
            {
                return Ok(ContextTransition::unchanged(previous));
            }

            let mut state = (**previous).clone();
            let connections = Arc::make_mut(&mut state.connections);
            let connection = Arc::make_mut(
                connections
                    .get_mut(&connection_id)
                    .expect("connection was checked above"),
            );
            connection.status = ConnectionStatus::Failed {
                message: reason.clone(),
            };
            connection.targets = Arc::new(BTreeMap::new());
            Ok(changed(
                state,
                ContextChange::RuntimeOnly,
                Vec::new(),
                ContextEvent::ConnectionFailed {
                    connection_id,
                    attempt: observed_attempt,
                    message: reason,
                },
            ))
        }
        RuntimeObservation::TargetUpserted {
            connection_id,
            attempt: observed_attempt,
            target,
        } => {
            let Some(connection) = previous.connections.get(&connection_id) else {
                return Ok(ContextTransition::unchanged(previous));
            };
            if attempt(connection) != observed_attempt
                || !matches!(connection.status, ConnectionStatus::Connected { .. })
            {
                return Ok(ContextTransition::unchanged(previous));
            }
            let target_id = target.target_id.clone();
            let event = if connection.targets.contains_key(&target_id) {
                ContextEvent::TargetChanged {
                    connection_id: connection_id.clone(),
                    target_id,
                }
            } else {
                ContextEvent::TargetCreated {
                    connection_id: connection_id.clone(),
                    target_id,
                }
            };
            let mut state = (**previous).clone();
            Arc::make_mut(&mut mutable_connection(&mut state, &connection_id).targets)
                .insert(target.target_id.clone(), target);
            Ok(changed(
                state,
                ContextChange::RuntimeOnly,
                Vec::new(),
                event,
            ))
        }
        RuntimeObservation::TargetRemoved {
            connection_id,
            attempt: observed_attempt,
            target_id,
        } => {
            let Some(connection) = previous.connections.get(&connection_id) else {
                return Ok(ContextTransition::unchanged(previous));
            };
            if attempt(connection) != observed_attempt
                || !matches!(connection.status, ConnectionStatus::Connected { .. })
                || !connection.targets.contains_key(&target_id)
            {
                return Ok(ContextTransition::unchanged(previous));
            }
            let mut state = (**previous).clone();
            Arc::make_mut(&mut mutable_connection(&mut state, &connection_id).targets)
                .remove(&target_id);
            Ok(changed(
                state,
                ContextChange::RuntimeOnly,
                Vec::new(),
                ContextEvent::TargetDestroyed {
                    connection_id,
                    target_id,
                },
            ))
        }
        RuntimeObservation::BreakpointApplicationsChanged { breakpoint_id } => {
            if !previous.breakpoints.contains_key(&breakpoint_id) {
                return Ok(ContextTransition::unchanged(previous));
            }
            Ok(changed(
                (**previous).clone(),
                ContextChange::RuntimeOnly,
                Vec::new(),
                ContextEvent::BreakpointUpdated { breakpoint_id },
            ))
        }
    }
}

fn default_true() -> bool {
    true
}

fn reduce_effect_completion(
    previous: &Arc<ContextState>,
    completion: EffectCompletion,
) -> Result<ContextTransition, ContextTransitionError> {
    match completion {
        EffectCompletion::ConnectionOpened {
            connection_id,
            attempt: completed_attempt,
            product,
            protocol_version,
            targets,
        } => {
            require_pending_status(
                previous,
                &connection_id,
                completed_attempt,
                &ConnectionStatus::Connecting,
            )?;
            let mut state = (**previous).clone();
            let connection = mutable_connection(&mut state, &connection_id);
            connection.status = ConnectionStatus::Connected {
                product,
                protocol_version,
            };
            connection.targets = Arc::new(targets);
            Ok(changed(
                state,
                ContextChange::RuntimeOnly,
                Vec::new(),
                ContextEvent::ConnectionConnected {
                    connection_id,
                    attempt: completed_attempt,
                },
            ))
        }
        EffectCompletion::ConnectionOpenFailed {
            connection_id,
            attempt: completed_attempt,
            message,
        } => {
            require_pending_status(
                previous,
                &connection_id,
                completed_attempt,
                &ConnectionStatus::Connecting,
            )?;
            let mut state = (**previous).clone();
            let connection = mutable_connection(&mut state, &connection_id);
            connection.status = ConnectionStatus::Failed {
                message: message.clone(),
            };
            connection.targets = Arc::new(BTreeMap::new());
            Ok(changed(
                state,
                ContextChange::RuntimeOnly,
                Vec::new(),
                ContextEvent::ConnectionFailed {
                    connection_id,
                    attempt: completed_attempt,
                    message,
                },
            ))
        }
        EffectCompletion::ConnectionClosed {
            connection_id,
            attempt: completed_attempt,
        } => {
            require_pending_status(
                previous,
                &connection_id,
                completed_attempt,
                &ConnectionStatus::Disconnecting,
            )?;
            let mut state = (**previous).clone();
            let connection = mutable_connection(&mut state, &connection_id);
            connection.status = ConnectionStatus::Disconnected;
            connection.targets = Arc::new(BTreeMap::new());
            Ok(changed(
                state,
                ContextChange::RuntimeOnly,
                Vec::new(),
                ContextEvent::ConnectionDisconnected {
                    connection_id,
                    attempt: completed_attempt,
                },
            ))
        }
    }
}

fn changed(
    mut state: ContextState,
    change: ContextChange,
    effects: Vec<ContextEffect>,
    event: ContextEvent,
) -> ContextTransition {
    state.revision += 1;
    let revision = state.revision;
    ContextTransition {
        state: Arc::new(state),
        effects,
        events: vec![RevisionEvent { revision, event }],
        change,
    }
}

fn mutable_connection<'a>(
    state: &'a mut ContextState,
    connection_id: &str,
) -> &'a mut ConnectionState {
    Arc::make_mut(
        Arc::make_mut(&mut state.connections)
            .get_mut(connection_id)
            .expect("connection was checked before mutation"),
    )
}

fn require_pending_status(
    state: &ContextState,
    connection_id: &str,
    completed_attempt: ConnectionAttempt,
    required_status: &ConnectionStatus,
) -> Result<(), ContextTransitionError> {
    let current = state.connections.get(connection_id);
    if !current.is_some_and(|connection| {
        attempt(connection) == completed_attempt && &connection.status == required_status
    }) {
        return Err(ContextTransitionError::StaleEffectCompletion {
            connection_id: connection_id.to_owned(),
        });
    }
    Ok(())
}

fn attempt(connection: &ConnectionState) -> ConnectionAttempt {
    ConnectionAttempt {
        configuration_version: connection.configuration_version,
        generation: connection.generation,
    }
}

fn is_active(status: &ConnectionStatus) -> bool {
    matches!(
        status,
        ConnectionStatus::Connecting
            | ConnectionStatus::Disconnecting
            | ConnectionStatus::Connected { .. }
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn command(state: &Arc<ContextState>, command: UserCommand) -> ContextTransition {
        reduce_context(state, ContextInput::UserCommand(command)).unwrap()
    }

    fn configured_context() -> Arc<ContextState> {
        let state = ContextState::new("test".into());
        let transition = command(
            &state,
            UserCommand::PutConnection {
                connection_id: "browser".into(),
                configuration: "ws://browser".into(),
            },
        );
        transition.state
    }

    fn connected_context() -> (Arc<ContextState>, ConnectionAttempt) {
        let configured = configured_context();
        let connecting = command(
            &configured,
            UserCommand::ConnectConnection {
                connection_id: "browser".into(),
            },
        );
        let attempt = match &connecting.effects[0] {
            ContextEffect::Connect { attempt, .. } => *attempt,
            effect => panic!("unexpected effect: {effect:?}"),
        };
        let connected = reduce_context(
            &connecting.state,
            ContextInput::EffectCompletion(EffectCompletion::ConnectionOpened {
                connection_id: "browser".into(),
                attempt,
                product: "Chrome".into(),
                protocol_version: "1.3".into(),
                targets: BTreeMap::new(),
            }),
        )
        .unwrap();
        (connected.state, attempt)
    }

    #[test]
    fn stale_connect_completion_cannot_revive_disconnected_state() {
        let configured = configured_context();
        let connecting = command(
            &configured,
            UserCommand::ConnectConnection {
                connection_id: "browser".into(),
            },
        );
        let attempt = match &connecting.effects[0] {
            ContextEffect::Connect { attempt, .. } => *attempt,
            effect => panic!("unexpected effect: {effect:?}"),
        };
        let disconnecting = command(
            &connecting.state,
            UserCommand::DisconnectConnection {
                connection_id: "browser".into(),
            },
        );
        let disconnected = reduce_context(
            &disconnecting.state,
            ContextInput::EffectCompletion(EffectCompletion::ConnectionClosed {
                connection_id: "browser".into(),
                attempt,
            }),
        )
        .unwrap();

        let error = reduce_context(
            &disconnected.state,
            ContextInput::EffectCompletion(EffectCompletion::ConnectionOpened {
                connection_id: "browser".into(),
                attempt,
                product: "Chrome".into(),
                protocol_version: "1.3".into(),
                targets: BTreeMap::new(),
            }),
        )
        .unwrap_err();
        assert_eq!(
            error,
            ContextTransitionError::StaleEffectCompletion {
                connection_id: "browser".into()
            }
        );
        assert_eq!(
            disconnected.state.connections["browser"].status,
            ConnectionStatus::Disconnected
        );
    }

    #[test]
    fn stale_runtime_observation_is_an_identity_transition() {
        let configured = configured_context();
        let stale = reduce_context(
            &configured,
            ContextInput::RuntimeObservation(RuntimeObservation::ConnectionClosed {
                connection_id: "browser".into(),
                attempt: ConnectionAttempt {
                    configuration_version: 1,
                    generation: 99,
                },
                reason: "old socket".into(),
            }),
        )
        .unwrap();

        assert_eq!(stale.change, ContextChange::None);
        assert!(Arc::ptr_eq(&stale.state, &configured));
        assert!(stale.events.is_empty());
    }

    #[test]
    fn target_lifecycle_updates_only_the_current_connection_generation() {
        let (connected, attempt) = connected_context();
        let target = TargetSnapshot {
            target_id: "page".into(),
            target_type: "page".into(),
            title: "Page".into(),
            url: "https://example.test".into(),
            attached: false,
            parent_id: None,
            opener_id: None,
            browser_context_id: None,
            subtype: None,
        };
        let created = reduce_context(
            &connected,
            ContextInput::RuntimeObservation(RuntimeObservation::TargetUpserted {
                connection_id: "browser".into(),
                attempt,
                target,
            }),
        )
        .unwrap();
        assert!(
            created.state.connections["browser"]
                .targets
                .contains_key("page")
        );
        assert!(matches!(
            created.events[0].event,
            ContextEvent::TargetCreated { .. }
        ));

        let stale = reduce_context(
            &created.state,
            ContextInput::RuntimeObservation(RuntimeObservation::TargetRemoved {
                connection_id: "browser".into(),
                attempt: ConnectionAttempt {
                    generation: attempt.generation + 1,
                    ..attempt
                },
                target_id: "page".into(),
            }),
        )
        .unwrap();
        assert_eq!(stale.change, ContextChange::None);

        let removed = reduce_context(
            &created.state,
            ContextInput::RuntimeObservation(RuntimeObservation::TargetRemoved {
                connection_id: "browser".into(),
                attempt,
                target_id: "page".into(),
            }),
        )
        .unwrap();
        assert!(removed.state.connections["browser"].targets.is_empty());
        assert!(matches!(
            removed.events[0].event,
            ContextEvent::TargetDestroyed { .. }
        ));
    }

    #[test]
    fn lifecycle_deletion_requires_disconnected_connections() {
        let (connected, _) = connected_context();
        assert_eq!(
            reduce_context(
                &connected,
                ContextInput::UserCommand(UserCommand::RemoveConnection {
                    connection_id: "browser".into(),
                }),
            )
            .unwrap_err(),
            ContextTransitionError::ActiveConnectionCannotBeRemoved
        );

        let configured = configured_context();
        let removed = command(
            &configured,
            UserCommand::RemoveConnection {
                connection_id: "browser".into(),
            },
        );
        assert!(removed.state.connections.is_empty());
    }

    #[test]
    fn breakpoint_configuration_and_deletion_are_durable_intent() {
        let state = ContextState::new("test".into());
        let configured = command(
            &state,
            UserCommand::PutBreakpoint {
                breakpoint_id: "conditional".into(),
                source_path: "file:///source.ts".into(),
                line: 4,
                column: 2,
                enabled: false,
                condition: Some("value > 0".into()),
                target_selector: Some("page".into()),
            },
        );
        let breakpoint = &configured.state.breakpoints["conditional"];
        assert!(!breakpoint.enabled);
        assert_eq!(breakpoint.condition.as_deref(), Some("value > 0"));
        assert_eq!(breakpoint.target_selector.as_deref(), Some("page"));

        let removed = command(
            &configured.state,
            UserCommand::RemoveBreakpoint {
                breakpoint_id: "conditional".into(),
            },
        );
        assert!(removed.state.breakpoints.is_empty());
    }

    #[test]
    fn unchanged_nodes_are_reused_between_revisions() {
        let state = ContextState::new("test".into());
        let with_breakpoint = command(
            &state,
            UserCommand::PutBreakpoint {
                breakpoint_id: "bp".into(),
                source_path: "file:///source.ts".into(),
                line: 1,
                column: 1,
                enabled: true,
                condition: None,
                target_selector: None,
            },
        );
        let configured = command(
            &with_breakpoint.state,
            UserCommand::PutConnection {
                connection_id: "browser".into(),
                configuration: "ws://browser".into(),
            },
        );

        assert!(Arc::ptr_eq(
            &with_breakpoint.state.breakpoints,
            &configured.state.breakpoints
        ));
        assert!(Arc::ptr_eq(
            &with_breakpoint.state.breakpoints["bp"],
            &configured.state.breakpoints["bp"]
        ));
    }

    #[test]
    fn replaying_inputs_produces_identical_transitions() {
        let inputs = vec![
            ContextInput::UserCommand(UserCommand::PutContext {
                display_name: Some("Replay".into()),
            }),
            ContextInput::UserCommand(UserCommand::PutConnection {
                connection_id: "browser".into(),
                configuration: "ws://browser".into(),
            }),
            ContextInput::UserCommand(UserCommand::ConnectConnection {
                connection_id: "browser".into(),
            }),
            ContextInput::EffectCompletion(EffectCompletion::ConnectionOpenFailed {
                connection_id: "browser".into(),
                attempt: ConnectionAttempt {
                    configuration_version: 1,
                    generation: 1,
                },
                message: "refused".into(),
            }),
        ];

        let replay = |inputs: &[ContextInput]| {
            let mut state = ContextState::new("replay".into());
            let mut effects = Vec::new();
            let mut events = Vec::new();
            for input in inputs {
                let transition = reduce_context(&state, input.clone()).unwrap();
                state = transition.state;
                effects.extend(transition.effects);
                events.extend(transition.events);
            }
            (state, effects, events)
        };

        let first = replay(&inputs);
        let serialized = serde_json::to_vec(&inputs).unwrap();
        let decoded: Vec<ContextInput> = serde_json::from_slice(&serialized).unwrap();
        let second = replay(&decoded);
        assert_eq!(first.0, second.0);
        assert_eq!(first.1, second.1);
        assert_eq!(first.2, second.2);
    }
}
