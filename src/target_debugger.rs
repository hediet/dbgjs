use std::collections::BTreeMap;
use std::sync::Arc;
use std::time::Duration;

use tokio::sync::{mpsc, oneshot, watch};

use crate::cdp_runtime::CdpDebuggerSession;
use crate::content_store::ContentStore;
use crate::debugger_driver::{DebuggerDriver, DebuggerDriverError};
use crate::debugger_engine::{
    BreakpointBinding, BreakpointKey, DebuggerState, FrameProjection, Input, ScriptSourceState,
    SessionKey, SessionPhase,
};
use crate::service_api::{
    FrameProjectionSnapshot, FrameSnapshot, PauseSnapshot, SourceLocation,
    TargetBreakpointSnapshot, TargetBreakpointStatus, TargetDebuggerPhase, TargetDebuggerSnapshot,
    TargetScriptSnapshot, TargetScriptStatus, TargetWaitPredicate,
};
use crate::source_effects::{SourceEffectInterpreter, SourceEffectOptions};
use crate::source_view::Position;

const COMMAND_BUFFER: usize = 32;
const MAX_WAIT: Duration = Duration::from_secs(5 * 60);

#[derive(Clone, Debug)]
pub struct TargetBreakpointSpec {
    pub id: String,
    pub source_url: String,
    pub line: u32,
    pub column: u32,
}

#[derive(Clone)]
pub struct TargetDebuggerHandle {
    commands: mpsc::Sender<TargetCommand>,
    snapshots: watch::Receiver<TargetDebuggerSnapshot>,
    session_id: String,
}

impl TargetDebuggerHandle {
    pub async fn start(
        context_id: String,
        connection_id: String,
        target_id: String,
        connection_generation: u64,
        session: CdpDebuggerSession,
        session_key: SessionKey,
    ) -> Result<Self, TargetDebuggerError> {
        let sources = SourceEffectInterpreter::new(
            SourceEffectOptions::default(),
            Arc::new(ContentStore::default()),
        );
        let mut driver = DebuggerDriver::new(
            Arc::new(DebuggerState::before_connection_generation(
                connection_generation,
            )),
            session,
            sources,
        );
        driver.apply(Input::Connected).await?;
        driver
            .apply(Input::SessionAttached {
                session_id: session_key.session_id.clone(),
                target_id: target_id.clone(),
                parent_session_id: None,
                waiting_for_debugger: false,
            })
            .await?;
        let initial = snapshot(
            &context_id,
            &connection_id,
            &target_id,
            connection_generation,
            &session_key,
            driver.state(),
        );
        let (snapshot_sender, snapshots) = watch::channel(initial);
        let (commands, command_receiver) = mpsc::channel(COMMAND_BUFFER);
        tokio::spawn(run_target(
            context_id,
            connection_id,
            target_id,
            connection_generation,
            session_key.clone(),
            driver,
            command_receiver,
            snapshot_sender,
        ));
        Ok(Self {
            commands,
            snapshots,
            session_id: session_key.session_id,
        })
    }

    pub fn snapshot(&self) -> TargetDebuggerSnapshot {
        self.snapshots.borrow().clone()
    }

    pub async fn set_breakpoint(
        &self,
        context_revision: u64,
        breakpoint: TargetBreakpointSpec,
    ) -> Result<TargetDebuggerSnapshot, TargetDebuggerError> {
        self.command(|response| TargetCommand::SetBreakpoint {
            context_revision,
            breakpoint,
            response,
        })
        .await
    }

    pub fn session_id(&self) -> &str {
        &self.session_id
    }

    pub async fn resume(
        &self,
        pause_epoch: u64,
    ) -> Result<TargetDebuggerSnapshot, TargetDebuggerError> {
        self.command(|response| TargetCommand::Resume {
            pause_epoch,
            response,
        })
        .await
    }

    pub async fn wait(
        &self,
        predicate: TargetWaitPredicate,
        timeout: Duration,
    ) -> Result<TargetDebuggerSnapshot, TargetDebuggerError> {
        if timeout.is_zero() || timeout > MAX_WAIT {
            return Err(TargetDebuggerError::InvalidTimeout);
        }
        let mut snapshots = self.snapshots.clone();
        tokio::time::timeout(timeout, async {
            loop {
                let current = snapshots.borrow_and_update().clone();
                if let TargetDebuggerPhase::Failed { message } = &current.phase {
                    return Err(TargetDebuggerError::DriverFailed(message.clone()));
                }
                if let TargetWaitPredicate::BreakpointInstalled { breakpoint_id } = &predicate {
                    if let Some(TargetBreakpointSnapshot {
                        status: TargetBreakpointStatus::Failed { message },
                        ..
                    }) = current
                        .breakpoints
                        .iter()
                        .find(|breakpoint| breakpoint.id == *breakpoint_id)
                    {
                        return Err(TargetDebuggerError::BreakpointFailed {
                            breakpoint_id: breakpoint_id.clone(),
                            message: message.clone(),
                        });
                    }
                }
                if predicate_matches(&current, &predicate) {
                    return Ok(current);
                }
                snapshots
                    .changed()
                    .await
                    .map_err(|_| TargetDebuggerError::Stopped)?;
            }
        })
        .await
        .map_err(|_| TargetDebuggerError::WaitTimedOut)?
    }

    pub async fn settle(&self, maximum: Duration) -> TargetDebuggerSnapshot {
        let mut snapshots = self.snapshots.clone();
        let settled =
            tokio::time::timeout(maximum, async {
                loop {
                    let current = snapshots.borrow_and_update().clone();
                    if current.breakpoints.iter().all(|breakpoint| {
                        !matches!(breakpoint.status, TargetBreakpointStatus::Pending)
                    }) {
                        return current;
                    }
                    if snapshots.changed().await.is_err() {
                        return snapshots.borrow().clone();
                    }
                }
            })
            .await;
        settled.unwrap_or_else(|_| snapshots.borrow().clone())
    }

    async fn command(
        &self,
        create: impl FnOnce(CommandResponse) -> TargetCommand,
    ) -> Result<TargetDebuggerSnapshot, TargetDebuggerError> {
        let (response, receiver) = oneshot::channel();
        self.commands
            .send(create(response))
            .await
            .map_err(|_| TargetDebuggerError::Stopped)?;
        receiver.await.map_err(|_| TargetDebuggerError::Stopped)?
    }
}

type CommandResponse = oneshot::Sender<Result<TargetDebuggerSnapshot, TargetDebuggerError>>;

enum TargetCommand {
    SetBreakpoint {
        context_revision: u64,
        breakpoint: TargetBreakpointSpec,
        response: CommandResponse,
    },
    Resume {
        pause_epoch: u64,
        response: CommandResponse,
    },
}

#[allow(clippy::too_many_arguments)]
async fn run_target(
    context_id: String,
    connection_id: String,
    target_id: String,
    connection_generation: u64,
    session_key: SessionKey,
    mut driver: DebuggerDriver,
    mut commands: mpsc::Receiver<TargetCommand>,
    snapshots: watch::Sender<TargetDebuggerSnapshot>,
) {
    let mut breakpoint_revisions = BTreeMap::<String, u64>::new();
    loop {
        enum Next {
            Command(Option<TargetCommand>),
            Event(Result<bool, DebuggerDriverError>),
        }

        let next = tokio::select! {
            command = commands.recv() => Next::Command(command),
            event = driver.process_next_event() => Next::Event(event),
        };
        match next {
            Next::Command(Some(TargetCommand::SetBreakpoint {
                context_revision,
                breakpoint,
                response,
            })) => {
                if breakpoint_revisions
                    .get(&breakpoint.id)
                    .is_some_and(|current| *current > context_revision)
                {
                    let _ = response.send(Ok(snapshot(
                        &context_id,
                        &connection_id,
                        &target_id,
                        connection_generation,
                        &session_key,
                        driver.state(),
                    )));
                    continue;
                }
                breakpoint_revisions.insert(breakpoint.id.clone(), context_revision);
                let result = apply_breakpoint(&mut driver, &context_id, breakpoint)
                    .await
                    .map(|()| {
                        snapshot(
                            &context_id,
                            &connection_id,
                            &target_id,
                            connection_generation,
                            &session_key,
                            driver.state(),
                        )
                    });
                if let Ok(snapshot) = &result {
                    snapshots.send_replace(snapshot.clone());
                }
                let _ = response.send(result);
            }
            Next::Command(Some(TargetCommand::Resume {
                pause_epoch,
                response,
            })) => {
                let result = resume(&mut driver, &session_key, pause_epoch)
                    .await
                    .map(|()| {
                        snapshot(
                            &context_id,
                            &connection_id,
                            &target_id,
                            connection_generation,
                            &session_key,
                            driver.state(),
                        )
                    });
                if let Ok(snapshot) = &result {
                    snapshots.send_replace(snapshot.clone());
                }
                let _ = response.send(result);
            }
            Next::Command(None) => break,
            Next::Event(Ok(_)) => {
                snapshots.send_replace(snapshot(
                    &context_id,
                    &connection_id,
                    &target_id,
                    connection_generation,
                    &session_key,
                    driver.state(),
                ));
            }
            Next::Event(Err(error)) => {
                let mut failed = snapshot(
                    &context_id,
                    &connection_id,
                    &target_id,
                    connection_generation,
                    &session_key,
                    driver.state(),
                );
                failed.phase = TargetDebuggerPhase::Failed {
                    message: error.to_string(),
                };
                snapshots.send_replace(failed);
                break;
            }
        }
    }
}

async fn apply_breakpoint(
    driver: &mut DebuggerDriver,
    client_id: &str,
    breakpoint: TargetBreakpointSpec,
) -> Result<(), TargetDebuggerError> {
    driver
        .apply(Input::SetBreakpoint {
            key: BreakpointKey {
                client_id: client_id.to_owned(),
                breakpoint_id: breakpoint.id,
            },
            source_url: breakpoint.source_url,
            position: Position {
                line: breakpoint
                    .line
                    .checked_sub(1)
                    .ok_or(TargetDebuggerError::InvalidBreakpointPosition)?,
                column: breakpoint
                    .column
                    .checked_sub(1)
                    .ok_or(TargetDebuggerError::InvalidBreakpointPosition)?,
            },
            condition: None,
        })
        .await?;
    Ok(())
}

async fn resume(
    driver: &mut DebuggerDriver,
    session_key: &SessionKey,
    pause_epoch: u64,
) -> Result<(), TargetDebuggerError> {
    let phase = &driver
        .state()
        .sessions
        .get(session_key)
        .ok_or(TargetDebuggerError::SessionMissing)?
        .phase;
    if !matches!(phase, SessionPhase::Paused { epoch } if *epoch == pause_epoch) {
        return Err(TargetDebuggerError::StalePause(pause_epoch));
    }
    driver
        .apply(Input::ResumeRequested {
            session: session_key.clone(),
            pause_epoch,
        })
        .await?;
    Ok(())
}

fn predicate_matches(snapshot: &TargetDebuggerSnapshot, predicate: &TargetWaitPredicate) -> bool {
    match predicate {
        TargetWaitPredicate::Running => {
            matches!(snapshot.phase, TargetDebuggerPhase::Running)
        }
        TargetWaitPredicate::BreakpointInstalled { breakpoint_id } => {
            snapshot.breakpoints.iter().any(|breakpoint| {
                breakpoint.id == *breakpoint_id
                    && matches!(breakpoint.status, TargetBreakpointStatus::Installed { .. })
            })
        }
        TargetWaitPredicate::Paused { after_epoch } => snapshot
            .pause
            .as_ref()
            .is_some_and(|pause| pause.epoch > *after_epoch),
    }
}

fn snapshot(
    context_id: &str,
    connection_id: &str,
    target_id: &str,
    connection_generation: u64,
    session_key: &SessionKey,
    state: &DebuggerState,
) -> TargetDebuggerSnapshot {
    let Some(session) = state.sessions.get(session_key) else {
        return TargetDebuggerSnapshot {
            context_id: context_id.to_owned(),
            connection_id: connection_id.to_owned(),
            target_id: target_id.to_owned(),
            connection_generation,
            revision: state.revision,
            phase: TargetDebuggerPhase::Failed {
                message: "debugger session is no longer available".to_owned(),
            },
            scripts: Vec::new(),
            breakpoints: Vec::new(),
            pause: None,
        };
    };
    let phase = match &session.phase {
        SessionPhase::Configuring => TargetDebuggerPhase::Running,
        SessionPhase::Running => TargetDebuggerPhase::Running,
        SessionPhase::Paused { epoch } => TargetDebuggerPhase::Paused { epoch: *epoch },
        SessionPhase::Resuming { epoch } => TargetDebuggerPhase::Resuming { epoch: *epoch },
        SessionPhase::Failed { message } => TargetDebuggerPhase::Failed {
            message: message.clone(),
        },
    };
    let breakpoints = state
        .breakpoints
        .iter()
        .filter(|(key, _)| key.client_id == context_id)
        .map(|(key, breakpoint)| {
            let installed = breakpoint
                .bindings
                .values()
                .filter(|binding| matches!(binding, BreakpointBinding::Installed { .. }))
                .count();
            let failure = breakpoint
                .bindings
                .values()
                .find_map(|binding| match binding {
                    BreakpointBinding::Failed { message } => Some(message.clone()),
                    _ => None,
                });
            let status = if installed > 0 {
                TargetBreakpointStatus::Installed {
                    binding_count: u32::try_from(installed).unwrap_or(u32::MAX),
                }
            } else if let Some(message) = failure {
                TargetBreakpointStatus::Failed { message }
            } else {
                TargetBreakpointStatus::Pending
            };
            TargetBreakpointSnapshot {
                id: key.breakpoint_id.clone(),
                source_url: breakpoint.source_url.clone(),
                line: breakpoint.position.line.saturating_add(1),
                column: breakpoint.position.column.saturating_add(1),
                status,
            }
        })
        .collect();
    let scripts = state
        .scripts
        .iter()
        .filter(|(key, _)| key.session == *session_key)
        .map(|(_, script)| TargetScriptSnapshot {
            url: script.url.clone(),
            source_map_url: script.source_map_url.clone(),
            status: match &script.source {
                ScriptSourceState::Unresolved => TargetScriptStatus::Unresolved,
                ScriptSourceState::Pending(_) | ScriptSourceState::Loaded { .. } => {
                    TargetScriptStatus::Pending
                }
                ScriptSourceState::Resolved(view) => TargetScriptStatus::Resolved {
                    authored_sources: view.logical_sources.keys().cloned().collect(),
                },
                ScriptSourceState::Failed(message) => TargetScriptStatus::Failed {
                    message: message.clone(),
                },
            },
        })
        .collect();
    let pause = session.pause.as_ref().map(|pause| PauseSnapshot {
        epoch: pause.epoch,
        reason: pause.reason.clone(),
        frames: pause
            .frames
            .iter()
            .enumerate()
            .map(|(index, frame)| {
                let raw_url = state
                    .scripts
                    .get(&frame.raw_script)
                    .map_or_else(String::new, |script| script.url.clone());
                FrameSnapshot {
                    index: u32::try_from(index).unwrap_or(u32::MAX),
                    function_name: frame.function_name.clone(),
                    raw: source_location(
                        raw_url,
                        frame.raw_position.line,
                        frame.raw_position.column,
                    ),
                    projected: match &frame.projected {
                        FrameProjection::Raw => FrameProjectionSnapshot::Raw,
                        FrameProjection::Pending(_) => FrameProjectionSnapshot::Pending,
                        FrameProjection::Resolved {
                            source_url,
                            position,
                        } => FrameProjectionSnapshot::Resolved {
                            location: source_location(
                                source_url.clone(),
                                position.line,
                                position.column,
                            ),
                        },
                        FrameProjection::Failed { message } => FrameProjectionSnapshot::Failed {
                            message: message.clone(),
                        },
                    },
                }
            })
            .collect(),
    });
    TargetDebuggerSnapshot {
        context_id: context_id.to_owned(),
        connection_id: connection_id.to_owned(),
        target_id: target_id.to_owned(),
        connection_generation,
        revision: state.revision,
        phase,
        scripts,
        breakpoints,
        pause,
    }
}

fn source_location(source_url: String, line: u32, column: u32) -> SourceLocation {
    SourceLocation {
        source_url,
        line: line.saturating_add(1),
        column: column.saturating_add(1),
    }
}

#[derive(Debug, thiserror::Error)]
pub enum TargetDebuggerError {
    #[error(transparent)]
    Driver(#[from] DebuggerDriverError),
    #[error("breakpoint line and column must be one-based")]
    InvalidBreakpointPosition,
    #[error("the debugger session is no longer available")]
    SessionMissing,
    #[error("pause epoch {0} is stale")]
    StalePause(u64),
    #[error("target debugger stopped")]
    Stopped,
    #[error("target debugger failed: {0}")]
    DriverFailed(String),
    #[error("breakpoint '{breakpoint_id}' failed: {message}")]
    BreakpointFailed {
        breakpoint_id: String,
        message: String,
    },
    #[error("target wait timed out")]
    WaitTimedOut,
    #[error("target wait timeout must be between 1ms and 5 minutes")]
    InvalidTimeout,
}
