use std::collections::BTreeMap;
use std::sync::Arc;
use std::time::Duration;

use tokio::sync::{mpsc, oneshot, watch};

use crate::cdp::{
    DebuggerEvaluateOnCallFrameParams, DomGetBoxModelParams, DomGetDocumentParams,
    DomQuerySelectorParams, InputDispatchMouseEventParams, InputDispatchMouseEventParamsType,
    InputMouseButton, ProfilerEnableParams, ProfilerScriptCoverage,
    ProfilerStartPreciseCoverageParams, ProfilerStopPreciseCoverageParams,
    ProfilerTakePreciseCoverageParams,
};
use crate::cdp_runtime::CdpDebuggerSession;
use crate::content_store::ContentStore;
use crate::debugger_driver::{DebuggerDriver, DebuggerDriverError};
use crate::debugger_engine::{
    BreakpointBinding, BreakpointKey, DebuggerState, FrameProjection, Input, ScriptSourceState,
    SessionKey, SessionPhase, StepKind,
};
use crate::service_api::{
    ConsoleMessageSnapshot, CoverageFunctionSnapshot, CoverageRangeSnapshot, CoverageSnapshot,
    CoverageSourceSnapshot, EvaluationSnapshot, FrameProjectionSnapshot, FrameSnapshot,
    PauseSnapshot, SourceExcerpt, SourceExcerptLine, SourceLocation, TargetBreakpointSnapshot,
    TargetBreakpointStatus, TargetDebuggerPhase, TargetDebuggerSnapshot, TargetScriptSnapshot,
    TargetScriptStatus, TargetWaitPredicate,
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
    pub condition: Option<String>,
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
        let initial = snapshot_from_driver(
            &context_id,
            &connection_id,
            &target_id,
            connection_generation,
            &session_key,
            &driver,
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

    pub async fn step(
        &self,
        pause_epoch: u64,
        kind: StepKind,
    ) -> Result<TargetDebuggerSnapshot, TargetDebuggerError> {
        self.command(|response| TargetCommand::Step {
            pause_epoch,
            kind,
            response,
        })
        .await
    }

    pub async fn evaluate(
        &self,
        pause_epoch: Option<u64>,
        frame_index: u32,
        expression: String,
    ) -> Result<EvaluationSnapshot, TargetDebuggerError> {
        let (response, receiver) = oneshot::channel();
        self.commands
            .send(TargetCommand::Evaluate {
                pause_epoch,
                frame_index,
                expression,
                response,
            })
            .await
            .map_err(|_| TargetDebuggerError::Stopped)?;
        receiver.await.map_err(|_| TargetDebuggerError::Stopped)?
    }

    pub async fn click(&self, selector: String) -> Result<(), TargetDebuggerError> {
        let (response, receiver) = oneshot::channel();
        self.commands
            .send(TargetCommand::Click { selector, response })
            .await
            .map_err(|_| TargetDebuggerError::Stopped)?;
        receiver.await.map_err(|_| TargetDebuggerError::Stopped)?
    }

    pub async fn start_coverage(&self) -> Result<(), TargetDebuggerError> {
        let (response, receiver) = oneshot::channel();
        self.commands
            .send(TargetCommand::StartCoverage { response })
            .await
            .map_err(|_| TargetDebuggerError::Stopped)?;
        receiver.await.map_err(|_| TargetDebuggerError::Stopped)?
    }

    pub async fn take_coverage(&self) -> Result<CoverageSnapshot, TargetDebuggerError> {
        let (response, receiver) = oneshot::channel();
        self.commands
            .send(TargetCommand::TakeCoverage { response })
            .await
            .map_err(|_| TargetDebuggerError::Stopped)?;
        receiver.await.map_err(|_| TargetDebuggerError::Stopped)?
    }

    pub async fn stop_coverage(&self) -> Result<CoverageSnapshot, TargetDebuggerError> {
        let (response, receiver) = oneshot::channel();
        self.commands
            .send(TargetCommand::StopCoverage { response })
            .await
            .map_err(|_| TargetDebuggerError::Stopped)?;
        receiver.await.map_err(|_| TargetDebuggerError::Stopped)?
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
    Step {
        pause_epoch: u64,
        kind: StepKind,
        response: CommandResponse,
    },
    Evaluate {
        pause_epoch: Option<u64>,
        frame_index: u32,
        expression: String,
        response: oneshot::Sender<Result<EvaluationSnapshot, TargetDebuggerError>>,
    },
    Click {
        selector: String,
        response: oneshot::Sender<Result<(), TargetDebuggerError>>,
    },
    StartCoverage {
        response: oneshot::Sender<Result<(), TargetDebuggerError>>,
    },
    TakeCoverage {
        response: oneshot::Sender<Result<CoverageSnapshot, TargetDebuggerError>>,
    },
    StopCoverage {
        response: oneshot::Sender<Result<CoverageSnapshot, TargetDebuggerError>>,
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
    let mut coverage = None::<CoverageRecording>;
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
                    let _ = response.send(Ok(snapshot_from_driver(
                        &context_id,
                        &connection_id,
                        &target_id,
                        connection_generation,
                        &session_key,
                        &driver,
                    )));
                    continue;
                }
                breakpoint_revisions.insert(breakpoint.id.clone(), context_revision);
                let result = apply_breakpoint(&mut driver, &context_id, breakpoint)
                    .await
                    .map(|()| {
                        snapshot_from_driver(
                            &context_id,
                            &connection_id,
                            &target_id,
                            connection_generation,
                            &session_key,
                            &driver,
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
                let result = resume_and_settle(&mut driver, &session_key, pause_epoch)
                    .await
                    .map(|()| {
                        snapshot_from_driver(
                            &context_id,
                            &connection_id,
                            &target_id,
                            connection_generation,
                            &session_key,
                            &driver,
                        )
                    });
                if let Ok(snapshot) = &result {
                    snapshots.send_replace(snapshot.clone());
                }
                let _ = response.send(result);
            }
            Next::Command(Some(TargetCommand::Step {
                pause_epoch,
                kind,
                response,
            })) => {
                let result = step_and_settle(&mut driver, &session_key, pause_epoch, kind)
                    .await
                    .map(|()| {
                        snapshot_from_driver(
                            &context_id,
                            &connection_id,
                            &target_id,
                            connection_generation,
                            &session_key,
                            &driver,
                        )
                    });
                if let Ok(snapshot) = &result {
                    snapshots.send_replace(snapshot.clone());
                }
                let _ = response.send(result);
            }
            Next::Command(Some(TargetCommand::Evaluate {
                pause_epoch,
                frame_index,
                expression,
                response,
            })) => {
                let result =
                    evaluate(&driver, &session_key, pause_epoch, frame_index, expression).await;
                let _ = response.send(result);
            }
            Next::Command(Some(TargetCommand::Click { selector, response })) => {
                let prior_epoch = driver
                    .state()
                    .sessions
                    .get(&session_key)
                    .map_or(0, |session| session.next_pause_epoch.saturating_sub(1));
                let result = match begin_click(&driver, selector).await {
                    Err(error) => Err(error),
                    Ok(mut dispatch) => loop {
                        tokio::select! {
                            result = &mut dispatch => {
                                break result
                                    .map_err(|error| TargetDebuggerError::Interaction(error.to_string()))
                                    .and_then(|result| result);
                            }
                            event = driver.process_next_event() => {
                                if let Err(error) = event {
                                    break Err(error.into());
                                }
                                snapshots.send_replace(snapshot_from_driver(
                                    &context_id,
                                    &connection_id,
                                    &target_id,
                                    connection_generation,
                                    &session_key,
                                    &driver,
                                ));
                                if matches!(
                                    driver.state().sessions.get(&session_key).map(|session| &session.phase),
                                    Some(SessionPhase::Paused { epoch }) if *epoch > prior_epoch
                                ) {
                                    dispatch.abort();
                                    let _ = dispatch.await;
                                    break Ok(());
                                }
                            }
                        }
                    },
                };
                let _ = response.send(result);
            }
            Next::Command(Some(TargetCommand::StartCoverage { response })) => {
                let result = if coverage.is_some() {
                    Err(TargetDebuggerError::CoverageAlreadyActive)
                } else {
                    start_coverage(&driver).await.map(|()| {
                        coverage = Some(CoverageRecording::default());
                    })
                };
                let _ = response.send(result);
            }
            Next::Command(Some(TargetCommand::TakeCoverage { response })) => {
                let result = match coverage.as_mut() {
                    Some(recording) => take_coverage(&driver, &session_key, recording).await,
                    None => Err(TargetDebuggerError::CoverageNotActive),
                };
                let _ = response.send(result);
            }
            Next::Command(Some(TargetCommand::StopCoverage { response })) => {
                let result = async {
                    let recording = coverage
                        .as_mut()
                        .ok_or(TargetDebuggerError::CoverageNotActive)?;
                    let snapshot = take_coverage(&driver, &session_key, recording).await?;
                    driver
                        .client()
                        .profiler_stop_precise_coverage(ProfilerStopPreciseCoverageParams::new())
                        .await
                        .map_err(|error| TargetDebuggerError::Coverage(format!("{error:?}")))?;
                    coverage = None;
                    Ok(snapshot)
                }
                .await;
                let _ = response.send(result);
            }
            Next::Command(None) => break,
            Next::Event(Ok(_)) => {
                snapshots.send_replace(snapshot_from_driver(
                    &context_id,
                    &connection_id,
                    &target_id,
                    connection_generation,
                    &session_key,
                    &driver,
                ));
            }

            Next::Event(Err(error)) => {
                let mut failed = snapshot_from_driver(
                    &context_id,
                    &connection_id,
                    &target_id,
                    connection_generation,
                    &session_key,
                    &driver,
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

async fn begin_click(
    driver: &DebuggerDriver,
    selector: String,
) -> Result<tokio::task::JoinHandle<Result<(), TargetDebuggerError>>, TargetDebuggerError> {
    let document = driver
        .client()
        .dom_get_document(DomGetDocumentParams::new())
        .await
        .map_err(|error| TargetDebuggerError::Interaction(format!("{error:?}")))?;
    let node = driver
        .client()
        .dom_query_selector(DomQuerySelectorParams::new(
            document.root.node_id,
            selector.clone(),
        ))
        .await
        .map_err(|error| TargetDebuggerError::Interaction(format!("{error:?}")))?;
    if node.node_id == 0 {
        return Err(TargetDebuggerError::SelectorNotFound(selector));
    }
    let mut box_params = DomGetBoxModelParams::new();
    box_params.node_id = Some(node.node_id);
    let model = driver
        .client()
        .dom_get_box_model(box_params)
        .await
        .map_err(|error| TargetDebuggerError::Interaction(format!("{error:?}")))?
        .model;
    let x = (model.content[0] + model.content[2] + model.content[4] + model.content[6]) / 4.0;
    let y = (model.content[1] + model.content[3] + model.content[5] + model.content[7]) / 4.0;
    let client = driver.client().clone();
    Ok(tokio::spawn(async move {
        for kind in [
            InputDispatchMouseEventParamsType::MouseMoved,
            InputDispatchMouseEventParamsType::MousePressed,
            InputDispatchMouseEventParamsType::MouseReleased,
        ] {
            let mut event = InputDispatchMouseEventParams::new(kind, x, y);
            event.button = Some(InputMouseButton::Left);
            event.click_count = Some(1);
            client
                .input_dispatch_mouse_event(event)
                .await
                .map_err(|error| TargetDebuggerError::Interaction(format!("{error:?}")))?;
        }
        Ok(())
    }))
}

async fn start_coverage(driver: &DebuggerDriver) -> Result<(), TargetDebuggerError> {
    driver
        .client()
        .profiler_enable(ProfilerEnableParams::new())
        .await
        .map_err(|error| TargetDebuggerError::Coverage(format!("{error:?}")))?;
    let mut params = ProfilerStartPreciseCoverageParams::new();
    params.call_count = Some(true);
    params.detailed = Some(true);
    driver
        .client()
        .profiler_start_precise_coverage(params)
        .await
        .map_err(|error| TargetDebuggerError::Coverage(format!("{error:?}")))?;
    Ok(())
}

async fn take_coverage(
    driver: &DebuggerDriver,
    session_key: &SessionKey,
    recording: &mut CoverageRecording,
) -> Result<CoverageSnapshot, TargetDebuggerError> {
    let coverage = driver
        .client()
        .profiler_take_precise_coverage(ProfilerTakePreciseCoverageParams::new())
        .await
        .map_err(|error| TargetDebuggerError::Coverage(format!("{error:?}")))?;
    recording.timestamp_micros = (coverage.timestamp * 1_000_000.0).max(0.0) as u64;
    for script in coverage.result {
        recording.merge(script);
    }
    Ok(recording.snapshot(driver, session_key))
}

#[derive(Default)]
struct CoverageRecording {
    timestamp_micros: u64,
    scripts: BTreeMap<String, AccumulatedScriptCoverage>,
}

#[derive(Default)]
struct AccumulatedScriptCoverage {
    url: String,
    functions: BTreeMap<(String, bool, u32, u32), BTreeMap<(u32, u32), u64>>,
}

impl CoverageRecording {
    fn merge(&mut self, script: ProfilerScriptCoverage) {
        if script.url.is_empty() {
            return;
        }
        let accumulated = self.scripts.entry(script.script_id).or_default();
        accumulated.url = script.url;
        for function in script.functions {
            let root = function
                .ranges
                .first()
                .and_then(|range| {
                    Some((
                        u32::try_from(range.start_offset).ok()?,
                        u32::try_from(range.end_offset).ok()?,
                    ))
                })
                .unwrap_or((0, 0));
            let ranges = accumulated
                .functions
                .entry((
                    function.function_name,
                    function.is_block_coverage,
                    root.0,
                    root.1,
                ))
                .or_default();
            for range in function.ranges {
                let Ok(start) = u32::try_from(range.start_offset) else {
                    continue;
                };
                let Ok(end) = u32::try_from(range.end_offset) else {
                    continue;
                };
                *ranges.entry((start, end)).or_default() += range.count.max(0) as u64;
            }
        }
    }

    fn snapshot(&self, driver: &DebuggerDriver, session_key: &SessionKey) -> CoverageSnapshot {
        CoverageSnapshot {
            timestamp_micros: self.timestamp_micros,
            sources: self
                .scripts
                .iter()
                .filter_map(|(script_id, script)| {
                    let functions = script
                        .functions
                        .iter()
                        .map(|((name, block_coverage, _, _), ranges)| {
                            let ranges = ranges
                                .iter()
                                .map(|((start, end), count)| CoverageRangeSnapshot {
                                    start_offset: *start,
                                    end_offset: *end,
                                    count: *count,
                                })
                                .collect::<Vec<_>>();
                            CoverageFunctionSnapshot {
                                name: if name.is_empty() {
                                    "(anonymous)".to_owned()
                                } else {
                                    name.clone()
                                },
                                block_coverage: *block_coverage,
                                ranges,
                            }
                        })
                        .collect::<Vec<_>>();
                    if functions.is_empty() {
                        return None;
                    }
                    let associated_authored_source = driver
                        .state()
                        .scripts
                        .iter()
                        .find(|(key, _)| key.session == *session_key && key.script_id == *script_id)
                        .and_then(|(_, state)| match &state.source {
                            ScriptSourceState::Resolved(view)
                                if view.logical_sources.len() == 1 =>
                            {
                                view.logical_sources.keys().next().cloned()
                            }
                            _ => None,
                        });
                    Some(CoverageSourceSnapshot {
                        generated_url: script.url.clone(),
                        associated_authored_source,
                        functions,
                    })
                })
                .collect(),
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
            condition: breakpoint.condition,
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

async fn step(
    driver: &mut DebuggerDriver,
    session_key: &SessionKey,
    pause_epoch: u64,
    kind: StepKind,
) -> Result<(), TargetDebuggerError> {
    require_pause(driver, session_key, pause_epoch)?;
    driver
        .apply(Input::StepRequested {
            session: session_key.clone(),
            pause_epoch,
            kind,
        })
        .await?;
    Ok(())
}

async fn step_and_settle(
    driver: &mut DebuggerDriver,
    session_key: &SessionKey,
    pause_epoch: u64,
    kind: StepKind,
) -> Result<(), TargetDebuggerError> {
    step(driver, session_key, pause_epoch, kind).await?;
    settle_execution(
        driver,
        session_key,
        |phase| matches!(phase, SessionPhase::Paused { epoch } if *epoch > pause_epoch),
    )
    .await
}

async fn resume_and_settle(
    driver: &mut DebuggerDriver,
    session_key: &SessionKey,
    pause_epoch: u64,
) -> Result<(), TargetDebuggerError> {
    resume(driver, session_key, pause_epoch).await?;
    settle_execution(driver, session_key, |phase| {
        matches!(phase, SessionPhase::Running)
    })
    .await
}

async fn settle_execution(
    driver: &mut DebuggerDriver,
    session_key: &SessionKey,
    settled: impl Fn(&SessionPhase) -> bool,
) -> Result<(), TargetDebuggerError> {
    tokio::time::timeout(Duration::from_millis(200), async {
        loop {
            let phase = &driver
                .state()
                .sessions
                .get(session_key)
                .ok_or(TargetDebuggerError::SessionMissing)?
                .phase;
            if settled(phase) {
                return Ok::<(), TargetDebuggerError>(());
            }
            driver.process_next_event().await?;
        }
    })
    .await
    .map_err(|_| TargetDebuggerError::SettlementTimedOut)?
}

async fn evaluate(
    driver: &DebuggerDriver,
    session_key: &SessionKey,
    pause_epoch: Option<u64>,
    frame_index: u32,
    expression: String,
) -> Result<EvaluationSnapshot, TargetDebuggerError> {
    let result = if let Some(pause_epoch) = pause_epoch {
        let pause = require_pause(driver, session_key, pause_epoch)?;
        let frame = pause
            .frames
            .get(frame_index as usize)
            .ok_or(TargetDebuggerError::FrameNotFound(frame_index))?;
        let mut params =
            DebuggerEvaluateOnCallFrameParams::new(frame.call_frame_id.clone(), expression.clone());
        params.return_by_value = Some(true);
        params.generate_preview = Some(true);
        let evaluated = driver
            .client()
            .debugger_evaluate_on_call_frame(params)
            .await
            .map_err(|error| TargetDebuggerError::Evaluation(format!("{error:?}")))?;
        if let Some(exception) = evaluated.exception_details {
            return Err(TargetDebuggerError::Evaluation(exception.text));
        }
        evaluated.result
    } else {
        let mut params = crate::cdp::RuntimeEvaluateParams::new(expression.clone());
        params.return_by_value = Some(true);
        params.generate_preview = Some(true);
        let evaluated = driver
            .client()
            .runtime_evaluate(params)
            .await
            .map_err(|error| TargetDebuggerError::Evaluation(format!("{error:?}")))?;
        if let Some(exception) = evaluated.exception_details {
            return Err(TargetDebuggerError::Evaluation(exception.text));
        }
        evaluated.result
    };
    let kind = serde_json::to_value(&result.r#type)
        .ok()
        .and_then(|value| value.as_str().map(ToOwned::to_owned))
        .unwrap_or_else(|| "unknown".to_owned());
    Ok(EvaluationSnapshot {
        expression,
        kind,
        value: result.value,
        unserializable_value: result.unserializable_value,
        description: result.description,
    })
}

fn require_pause<'a>(
    driver: &'a DebuggerDriver,
    session_key: &SessionKey,
    pause_epoch: u64,
) -> Result<&'a Arc<crate::debugger_engine::PauseState>, TargetDebuggerError> {
    let session = driver
        .state()
        .sessions
        .get(session_key)
        .ok_or(TargetDebuggerError::SessionMissing)?;
    if !matches!(session.phase, SessionPhase::Paused { epoch } if epoch == pause_epoch) {
        return Err(TargetDebuggerError::StalePause(pause_epoch));
    }
    session
        .pause
        .as_ref()
        .ok_or(TargetDebuggerError::SessionMissing)
}

fn snapshot_from_driver(
    context_id: &str,
    connection_id: &str,
    target_id: &str,
    connection_generation: u64,
    session_key: &SessionKey,
    driver: &DebuggerDriver,
) -> TargetDebuggerSnapshot {
    let mut result = snapshot(
        context_id,
        connection_id,
        target_id,
        connection_generation,
        session_key,
        driver.state(),
    );
    result.logs = driver
        .console_messages()
        .iter()
        .map(|values| ConsoleMessageSnapshot {
            values: values.clone(),
        })
        .collect();
    if let Some(pause) = result.pause.as_mut()
        && let Some(raw_pause) = driver
            .state()
            .sessions
            .get(session_key)
            .and_then(|session| session.pause.as_ref())
    {
        for (frame, raw_frame) in pause.frames.iter_mut().zip(raw_pause.frames.iter()) {
            if let FrameProjectionSnapshot::Resolved { location } = &frame.projected
                && let Some(content) =
                    driver.logical_source_content(&raw_frame.raw_script, &location.source_url)
            {
                frame.breadcrumb = crate::language_intelligence::breadcrumb(
                    &location.source_url,
                    &content,
                    location.line,
                    location.column,
                );
                if frame.index == 0 {
                    pause.source = Some(source_excerpt(
                        &location.source_url,
                        location,
                        &content,
                        frame.breadcrumb.clone(),
                    ));
                }
            }
        }
    }
    result
}

fn source_excerpt(
    source_url: &str,
    location: &SourceLocation,
    content: &str,
    breadcrumb: Option<String>,
) -> SourceExcerpt {
    let lines = content.lines().collect::<Vec<_>>();
    let current = location.line.saturating_sub(1) as usize;
    let start = current.saturating_sub(4);
    let end = (current + 5).min(lines.len());
    let current_text = lines.get(current).copied().unwrap_or("");
    let utf16_column = location.column.saturating_sub(1) as usize;
    let (byte_column, display_column) = utf16_to_byte_and_display(current_text, utf16_column);
    let remainder = current_text.get(byte_column..).unwrap_or("");
    let highlight_length = remainder
        .chars()
        .take_while(|character| character.is_alphanumeric() || *character == '_')
        .count()
        .max(1) as u32;
    SourceExcerpt {
        source_url: source_url.to_owned(),
        breadcrumb,
        current_line: location.line,
        lines: (start..end)
            .map(|index| SourceExcerptLine {
                line: (index + 1) as u32,
                text: truncate_line(lines[index], 200),
            })
            .collect(),
        highlight_start: display_column.saturating_add(1) as u32,
        highlight_length,
    }
}

fn utf16_to_byte_and_display(line: &str, utf16_column: usize) -> (usize, usize) {
    let mut utf16 = 0;
    let mut display = 0;
    for (byte, character) in line.char_indices() {
        if utf16 >= utf16_column {
            return (byte, display);
        }
        utf16 += character.len_utf16();
        display += 1;
    }
    (line.len(), display)
}

fn truncate_line(line: &str, maximum: usize) -> String {
    if line.chars().count() <= maximum {
        line.to_owned()
    } else {
        format!("{}…", line.chars().take(maximum).collect::<String>())
    }
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
        TargetWaitPredicate::Paused { after_epoch } => matches!(
            snapshot.phase,
            TargetDebuggerPhase::Paused { epoch } if epoch > *after_epoch
        ),
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
            logs: Vec::new(),
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
    let pause = matches!(session.phase, SessionPhase::Paused { .. })
        .then(|| session.pause.as_ref())
        .flatten()
        .map(|pause| PauseSnapshot {
            epoch: pause.epoch,
            reason: pause.reason.clone(),
            source: None,
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
                            FrameProjection::Failed { message } => {
                                FrameProjectionSnapshot::Failed {
                                    message: message.clone(),
                                }
                            }
                        },
                        breadcrumb: None,
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
        logs: Vec::new(),
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
    #[error("frame {0} does not exist in the current pause")]
    FrameNotFound(u32),
    #[error("evaluation failed: {0}")]
    Evaluation(String),
    #[error("interaction failed: {0}")]
    Interaction(String),
    #[error("selector '{0}' did not match an element")]
    SelectorNotFound(String),
    #[error("coverage failed: {0}")]
    Coverage(String),
    #[error("coverage recording is already active")]
    CoverageAlreadyActive,
    #[error("coverage recording is not active")]
    CoverageNotActive,
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
    #[error("debugger command did not settle within 200ms")]
    SettlementTimedOut,
    #[error("target wait timeout must be between 1ms and 5 minutes")]
    InvalidTimeout,
}
