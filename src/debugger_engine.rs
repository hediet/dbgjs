use std::collections::{BTreeMap, BTreeSet};
use std::sync::Arc;

use serde::{Deserialize, Serialize};

use crate::source_view::{ContentCandidate, Position};

const MAX_DIAGNOSTICS: usize = 1024;

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub struct EffectId(pub u64);

#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub struct SessionKey {
    pub connection_generation: u64,
    pub session_id: String,
}

#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub struct ScriptKey {
    pub session: SessionKey,
    pub script_id: String,
}

#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub struct BreakpointKey {
    pub client_id: String,
    pub breakpoint_id: String,
}

#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub struct PhysicalBreakpointKey {
    pub script: ScriptKey,
    pub script_version: u64,
    pub position: Position,
    pub condition: Option<String>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum SessionPhase {
    Configuring,
    Running,
    Paused { epoch: u64 },
    Resuming { epoch: u64 },
    Failed { message: String },
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SessionState {
    pub target_id: String,
    pub parent: Option<SessionKey>,
    pub waiting_for_debugger: bool,
    pub phase: SessionPhase,
    pub next_pause_epoch: u64,
    pub pause: Option<Arc<PauseState>>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PauseState {
    pub epoch: u64,
    pub reason: String,
    pub frames: Arc<Vec<FrameState>>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct FrameState {
    pub call_frame_id: String,
    pub function_name: String,
    pub raw_script: ScriptKey,
    pub raw_position: Position,
    pub projected: FrameProjection,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum FrameProjection {
    Raw,
    Pending(EffectId),
    Resolved {
        source_url: String,
        position: Position,
    },
    Failed {
        message: String,
    },
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ScriptSourceState {
    Unresolved,
    Pending(EffectId),
    Loaded {
        content: Arc<str>,
        source_map: Option<Arc<[u8]>>,
        build_effect: EffectId,
    },
    Resolved(SourceViewState),
    Failed(String),
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SourceViewState {
    pub view_id: EffectId,
    pub logical_sources: Arc<BTreeMap<String, ContentCandidate>>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ScriptState {
    pub url: String,
    pub hash: String,
    pub source_map_url: Option<String>,
    pub version: u64,
    pub source: ScriptSourceState,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum BreakpointBinding {
    WaitingForRemoval(EffectId),
    PendingInstall(EffectId),
    Installed { backend_id: String },
    Failed { message: String },
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct BreakpointState {
    pub generation: u64,
    pub source_url: String,
    pub position: Position,
    pub condition: Option<String>,
    pub pending_mappings: Arc<BTreeMap<ScriptKey, EffectId>>,
    pub bindings: Arc<BTreeMap<PhysicalBreakpointKey, BreakpointBinding>>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum PhysicalBreakpointStatus {
    Installing(EffectId),
    Installed { backend_id: String },
    Removing(EffectId),
    Failed { message: String },
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PhysicalBreakpointState {
    pub owners: Arc<BTreeSet<BreakpointKey>>,
    pub status: PhysicalBreakpointStatus,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Diagnostic {
    IgnoredStaleEffect {
        effect_id: EffectId,
    },
    EffectFailed {
        effect_id: EffectId,
        message: String,
    },
    SourceMapUnavailable {
        script: ScriptKey,
        message: String,
    },
    InvalidTransition {
        description: String,
    },
    CancelledEffects {
        session: SessionKey,
        count: usize,
    },
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DebuggerState {
    pub revision: u64,
    pub connection_generation: u64,
    pub next_effect_id: u64,
    pub sessions: Arc<BTreeMap<SessionKey, Arc<SessionState>>>,
    pub scripts: Arc<BTreeMap<ScriptKey, Arc<ScriptState>>>,
    pub breakpoints: Arc<BTreeMap<BreakpointKey, Arc<BreakpointState>>>,
    pub physical_breakpoints: Arc<BTreeMap<PhysicalBreakpointKey, Arc<PhysicalBreakpointState>>>,
    pending: Arc<BTreeMap<EffectId, PendingEffect>>,
    pub diagnostics: Arc<Vec<Diagnostic>>,
}

impl Default for DebuggerState {
    fn default() -> Self {
        Self {
            revision: 0,
            connection_generation: 0,
            next_effect_id: 1,
            sessions: Arc::new(BTreeMap::new()),
            scripts: Arc::new(BTreeMap::new()),
            breakpoints: Arc::new(BTreeMap::new()),
            physical_breakpoints: Arc::new(BTreeMap::new()),
            pending: Arc::new(BTreeMap::new()),
            diagnostics: Arc::new(Vec::new()),
        }
    }
}

impl DebuggerState {
    pub fn before_connection_generation(generation: u64) -> Self {
        let mut state = Self::default();
        state.connection_generation = generation.saturating_sub(1);
        state
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
enum PendingEffect {
    ConfigureSession {
        session: SessionKey,
    },
    RunIfWaiting {
        session: SessionKey,
    },
    FetchScriptSource {
        script: ScriptKey,
        version: u64,
    },
    BuildSourceView {
        script: ScriptKey,
        version: u64,
    },
    MapBreakpoint {
        breakpoint: BreakpointKey,
        breakpoint_generation: u64,
        script: ScriptKey,
        version: u64,
    },
    InstallBreakpoint {
        physical: PhysicalBreakpointKey,
    },
    RemoveBreakpoint {
        physical: PhysicalBreakpointKey,
        backend_id: String,
    },
    MapFrame {
        session: SessionKey,
        pause_epoch: u64,
        frame_index: usize,
    },
    Resume {
        session: SessionKey,
        pause_epoch: u64,
    },
}

impl PendingEffect {
    fn belongs_to_session(&self, session: &SessionKey) -> bool {
        match self {
            Self::ConfigureSession { session: candidate }
            | Self::RunIfWaiting { session: candidate }
            | Self::MapFrame {
                session: candidate, ..
            }
            | Self::Resume {
                session: candidate, ..
            } => candidate == session,
            Self::FetchScriptSource { script, .. }
            | Self::BuildSourceView { script, .. }
            | Self::MapBreakpoint { script, .. }
            | Self::InstallBreakpoint {
                physical: PhysicalBreakpointKey { script, .. },
            }
            | Self::RemoveBreakpoint {
                physical: PhysicalBreakpointKey { script, .. },
                ..
            } => &script.session == session,
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum Effect {
    ConfigureSession {
        effect_id: EffectId,
        session: SessionKey,
    },
    RunIfWaitingForDebugger {
        effect_id: EffectId,
        session: SessionKey,
    },
    FetchScriptSource {
        effect_id: EffectId,
        script: ScriptKey,
        script_version: u64,
        generated_url: String,
        source_map_url: Option<String>,
    },
    BuildSourceView {
        effect_id: EffectId,
        script: ScriptKey,
        script_version: u64,
        generated_url: String,
        content: Arc<str>,
        source_map: Option<Arc<[u8]>>,
    },
    MapBreakpoint {
        effect_id: EffectId,
        breakpoint: BreakpointKey,
        script: ScriptKey,
        view_id: EffectId,
        source_url: String,
        position: Position,
    },
    InstallBreakpoint {
        effect_id: EffectId,
        physical: PhysicalBreakpointKey,
    },
    RemoveBreakpoint {
        effect_id: EffectId,
        physical: PhysicalBreakpointKey,
        backend_id: String,
    },
    MapFrame {
        effect_id: EffectId,
        session: SessionKey,
        pause_epoch: u64,
        frame_index: usize,
        script: ScriptKey,
        view_id: EffectId,
        position: Position,
    },
    Resume {
        effect_id: EffectId,
        session: SessionKey,
        pause_epoch: u64,
    },
    Step {
        effect_id: EffectId,
        session: SessionKey,
        pause_epoch: u64,
        kind: StepKind,
    },
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum StepKind {
    Into,
    Over,
    Out,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum Input {
    Connected,
    SessionAttached {
        session_id: String,
        target_id: String,
        parent_session_id: Option<String>,
        waiting_for_debugger: bool,
    },
    SessionConfigured {
        effect_id: EffectId,
    },
    CommandAccepted {
        effect_id: EffectId,
    },
    SessionDetached {
        session: SessionKey,
    },
    ScriptParsed {
        session: SessionKey,
        script_id: String,
        url: String,
        hash: String,
        source_map_url: Option<String>,
    },
    RequestScriptSource {
        script: ScriptKey,
    },
    ScriptSourceFetched {
        effect_id: EffectId,
        content: Arc<str>,
        source_map: Option<Arc<[u8]>>,
        source_map_error: Option<String>,
    },
    SourceViewBuilt {
        effect_id: EffectId,
        logical_sources: BTreeMap<String, ContentCandidate>,
    },
    SetBreakpoint {
        key: BreakpointKey,
        source_url: String,
        position: Position,
        condition: Option<String>,
    },
    RemoveBreakpoint {
        key: BreakpointKey,
    },
    BreakpointMapped {
        effect_id: EffectId,
        generated_positions: Vec<Position>,
    },
    BreakpointInstalled {
        effect_id: EffectId,
        backend_id: String,
    },
    BreakpointRemoved {
        effect_id: EffectId,
    },
    Paused {
        session: SessionKey,
        reason: String,
        frames: Vec<RawFrame>,
    },
    FrameMapped {
        effect_id: EffectId,
        source_url: String,
        position: Position,
    },
    ResumeRequested {
        session: SessionKey,
        pause_epoch: u64,
    },
    StepRequested {
        session: SessionKey,
        pause_epoch: u64,
        kind: StepKind,
    },
    Resumed {
        session: SessionKey,
        pause_epoch: u64,
    },
    EffectFailed {
        effect_id: EffectId,
        message: String,
    },
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct RawFrame {
    pub call_frame_id: String,
    pub function_name: String,
    pub script_id: String,
    pub position: Position,
}

#[derive(Clone, Debug)]
pub struct Transition {
    pub state: Arc<DebuggerState>,
    pub effects: Vec<Effect>,
}

pub fn reduce(previous: &Arc<DebuggerState>, input: Input) -> Transition {
    let mut state = (**previous).clone();
    state.revision += 1;
    let mut effects = Vec::new();

    match input {
        Input::Connected => {
            state.connection_generation += 1;
            state.sessions = Arc::new(BTreeMap::new());
            state.scripts = Arc::new(BTreeMap::new());
            state.physical_breakpoints = Arc::new(BTreeMap::new());
            state.pending = Arc::new(BTreeMap::new());
            let mut breakpoints = (*state.breakpoints).clone();
            for breakpoint in breakpoints.values_mut() {
                let breakpoint = Arc::make_mut(breakpoint);
                breakpoint.pending_mappings = Arc::new(BTreeMap::new());
                breakpoint.bindings = Arc::new(BTreeMap::new());
            }
            state.breakpoints = Arc::new(breakpoints);
        }
        Input::SessionAttached {
            session_id,
            target_id,
            parent_session_id,
            waiting_for_debugger,
        } => {
            let session = SessionKey {
                connection_generation: state.connection_generation,
                session_id,
            };
            let parent = parent_session_id.map(|session_id| SessionKey {
                connection_generation: state.connection_generation,
                session_id,
            });
            Arc::make_mut(&mut state.sessions).insert(
                session.clone(),
                Arc::new(SessionState {
                    target_id,
                    parent,
                    waiting_for_debugger,
                    phase: SessionPhase::Configuring,
                    next_pause_epoch: 1,
                    pause: None,
                }),
            );
            let effect_id = allocate_effect(
                &mut state,
                PendingEffect::ConfigureSession {
                    session: session.clone(),
                },
            );
            effects.push(Effect::ConfigureSession { effect_id, session });
        }
        Input::SessionConfigured { effect_id } => {
            let Some(PendingEffect::ConfigureSession { session }) =
                take_pending(&mut state, effect_id)
            else {
                stale_effect(&mut state, effect_id);
                return finish(state, effects);
            };
            if let Some(session_state) = Arc::make_mut(&mut state.sessions).get_mut(&session) {
                let session_state = Arc::make_mut(session_state);
                session_state.phase = SessionPhase::Running;
                if session_state.waiting_for_debugger {
                    let run_effect = allocate_effect(
                        &mut state,
                        PendingEffect::RunIfWaiting {
                            session: session.clone(),
                        },
                    );
                    effects.push(Effect::RunIfWaitingForDebugger {
                        effect_id: run_effect,
                        session,
                    });
                }
            }
        }
        Input::CommandAccepted { effect_id } => match state.pending.get(&effect_id).cloned() {
            Some(PendingEffect::RunIfWaiting { session }) => {
                take_pending(&mut state, effect_id);
                if let Some(session) = Arc::make_mut(&mut state.sessions).get_mut(&session) {
                    Arc::make_mut(session).waiting_for_debugger = false;
                } else {
                    stale_effect(&mut state, effect_id);
                }
            }
            Some(PendingEffect::Resume { .. }) => {
                take_pending(&mut state, effect_id);
            }
            Some(_) => invalid(
                &mut state,
                format!("effect {effect_id:?} does not accept a command-only completion"),
            ),
            None => stale_effect(&mut state, effect_id),
        },
        Input::SessionDetached { session } => detach_session(&mut state, &session),
        Input::ScriptParsed {
            session,
            script_id,
            url,
            hash,
            source_map_url,
        } => {
            if !state.sessions.contains_key(&session) {
                invalid(
                    &mut state,
                    format!("script parsed for unknown session {session:?}"),
                );
                return finish(state, effects);
            }
            let key = ScriptKey { session, script_id };
            let version = state
                .scripts
                .get(&key)
                .map(|script| script.version + 1)
                .unwrap_or(1);
            release_script_version(&mut state, &key, &mut effects);
            Arc::make_mut(&mut state.scripts).insert(
                key.clone(),
                Arc::new(ScriptState {
                    url,
                    hash,
                    source_map_url,
                    version,
                    source: ScriptSourceState::Unresolved,
                }),
            );
            if script_has_breakpoint_demand(&state, &key) || script_has_frame_demand(&state, &key) {
                schedule_source_hydration(&mut state, &key, false, &mut effects);
            }
        }
        Input::RequestScriptSource { script } => {
            if !state.scripts.contains_key(&script) {
                invalid(
                    &mut state,
                    format!("source requested for unknown script {script:?}"),
                );
            } else {
                schedule_source_hydration(&mut state, &script, true, &mut effects);
            }
        }
        Input::ScriptSourceFetched {
            effect_id,
            content,
            source_map,
            source_map_error,
        } => {
            let Some(PendingEffect::FetchScriptSource { script, version }) =
                take_pending(&mut state, effect_id)
            else {
                stale_effect(&mut state, effect_id);
                return finish(state, effects);
            };
            let Some(current) = state.scripts.get(&script) else {
                stale_effect(&mut state, effect_id);
                return finish(state, effects);
            };
            if current.version != version {
                stale_effect(&mut state, effect_id);
                return finish(state, effects);
            }
            if let Some(message) = source_map_error {
                push_diagnostic(
                    &mut state,
                    Diagnostic::SourceMapUnavailable {
                        script: script.clone(),
                        message,
                    },
                );
            }
            let build_effect = allocate_effect(
                &mut state,
                PendingEffect::BuildSourceView {
                    script: script.clone(),
                    version,
                },
            );
            let scripts = Arc::make_mut(&mut state.scripts);
            let script_state = Arc::make_mut(scripts.get_mut(&script).unwrap());
            script_state.source = ScriptSourceState::Loaded {
                content: content.clone(),
                source_map: source_map.clone(),
                build_effect,
            };
            effects.push(Effect::BuildSourceView {
                effect_id: build_effect,
                script,
                script_version: version,
                generated_url: script_state.url.clone(),
                content,
                source_map,
            });
        }
        Input::SourceViewBuilt {
            effect_id,
            logical_sources,
        } => {
            let Some(PendingEffect::BuildSourceView { script, version }) =
                take_pending(&mut state, effect_id)
            else {
                stale_effect(&mut state, effect_id);
                return finish(state, effects);
            };
            let Some(current) = state.scripts.get(&script) else {
                stale_effect(&mut state, effect_id);
                return finish(state, effects);
            };
            if current.version != version {
                stale_effect(&mut state, effect_id);
                return finish(state, effects);
            }
            Arc::make_mut(&mut state.scripts)
                .get_mut(&script)
                .map(Arc::make_mut)
                .unwrap()
                .source = ScriptSourceState::Resolved(SourceViewState {
                view_id: effect_id,
                logical_sources: Arc::new(logical_sources),
            });
            schedule_mappings_for_script(&mut state, &script, &mut effects);
            schedule_frame_mappings_for_script(&mut state, &script, &mut effects);
        }
        Input::SetBreakpoint {
            key,
            source_url,
            position,
            condition,
        } => {
            let generation = state
                .breakpoints
                .get(&key)
                .map(|breakpoint| breakpoint.generation + 1)
                .unwrap_or(1);
            release_breakpoint(&mut state, &key, &mut effects);
            Arc::make_mut(&mut state.breakpoints).insert(
                key.clone(),
                Arc::new(BreakpointState {
                    generation,
                    source_url,
                    position,
                    condition,
                    pending_mappings: Arc::new(BTreeMap::new()),
                    bindings: Arc::new(BTreeMap::new()),
                }),
            );
            let scripts: Vec<_> = state.scripts.keys().cloned().collect();
            for script in scripts {
                if script_has_source(&state, &script, &key) {
                    schedule_mapping(&mut state, &key, &script, &mut effects);
                } else if script_may_expose_breakpoint(&state, &script, &key) {
                    schedule_source_hydration(&mut state, &script, true, &mut effects);
                }
            }
        }
        Input::RemoveBreakpoint { key } => {
            release_breakpoint(&mut state, &key, &mut effects);
            Arc::make_mut(&mut state.breakpoints).remove(&key);
        }
        Input::BreakpointMapped {
            effect_id,
            generated_positions,
        } => {
            let Some(PendingEffect::MapBreakpoint {
                breakpoint,
                breakpoint_generation,
                script,
                version,
            }) = take_pending(&mut state, effect_id)
            else {
                stale_effect(&mut state, effect_id);
                return finish(state, effects);
            };
            if state.scripts.get(&script).map(|value| value.version) != Some(version) {
                stale_effect(&mut state, effect_id);
                return finish(state, effects);
            }
            let Some(breakpoint_state) = state.breakpoints.get(&breakpoint).cloned() else {
                stale_effect(&mut state, effect_id);
                return finish(state, effects);
            };
            if breakpoint_state.generation != breakpoint_generation {
                stale_effect(&mut state, effect_id);
                return finish(state, effects);
            }
            let mut breakpoints = (*state.breakpoints).clone();
            Arc::make_mut(breakpoints.get_mut(&breakpoint).unwrap()).pending_mappings =
                Arc::new(without_key(&breakpoint_state.pending_mappings, &script));
            state.breakpoints = Arc::new(breakpoints);

            for position in generated_positions {
                bind_physical(&mut state, &breakpoint, &script, position, &mut effects);
            }
        }
        Input::BreakpointInstalled {
            effect_id,
            backend_id,
        } => {
            let Some(PendingEffect::InstallBreakpoint { physical }) =
                take_pending(&mut state, effect_id)
            else {
                stale_effect(&mut state, effect_id);
                return finish(state, effects);
            };
            let Some(physical_state) = state.physical_breakpoints.get(&physical).cloned() else {
                stale_effect(&mut state, effect_id);
                return finish(state, effects);
            };
            let owners = physical_state.owners.clone();
            Arc::make_mut(&mut state.physical_breakpoints)
                .get_mut(&physical)
                .map(Arc::make_mut)
                .unwrap()
                .status = PhysicalBreakpointStatus::Installed {
                backend_id: backend_id.clone(),
            };
            let breakpoints = Arc::make_mut(&mut state.breakpoints);
            for owner in owners.iter() {
                if let Some(breakpoint) = breakpoints.get_mut(owner) {
                    Arc::make_mut(breakpoint).bindings = Arc::new(with_insert(
                        &breakpoint.bindings,
                        physical.clone(),
                        BreakpointBinding::Installed {
                            backend_id: backend_id.clone(),
                        },
                    ));
                }
            }
            if owners.is_empty() {
                schedule_physical_removal(&mut state, &physical, backend_id, &mut effects);
            }
        }
        Input::BreakpointRemoved { effect_id } => {
            let Some(PendingEffect::RemoveBreakpoint { physical, .. }) =
                take_pending(&mut state, effect_id)
            else {
                stale_effect(&mut state, effect_id);
                return finish(state, effects);
            };
            let Some(physical_state) = state.physical_breakpoints.get(&physical).cloned() else {
                stale_effect(&mut state, effect_id);
                return finish(state, effects);
            };
            if !matches!(
                physical_state.status,
                PhysicalBreakpointStatus::Removing(candidate) if candidate == effect_id
            ) {
                stale_effect(&mut state, effect_id);
            } else if physical_state.owners.is_empty() {
                Arc::make_mut(&mut state.physical_breakpoints).remove(&physical);
            } else {
                schedule_physical_install(&mut state, &physical, &mut effects);
            }
        }
        Input::Paused {
            session,
            reason,
            frames,
        } => pause_session(&mut state, &session, reason, frames, &mut effects),
        Input::FrameMapped {
            effect_id,
            source_url,
            position,
        } => {
            let Some(PendingEffect::MapFrame {
                session,
                pause_epoch,
                frame_index,
            }) = take_pending(&mut state, effect_id)
            else {
                stale_effect(&mut state, effect_id);
                return finish(state, effects);
            };
            let Some(session_state) = state.sessions.get(&session) else {
                stale_effect(&mut state, effect_id);
                return finish(state, effects);
            };
            let Some(pause) = &session_state.pause else {
                stale_effect(&mut state, effect_id);
                return finish(state, effects);
            };
            if pause.epoch != pause_epoch {
                stale_effect(&mut state, effect_id);
                return finish(state, effects);
            }
            let mut frames = (*pause.frames).clone();
            if let Some(frame) = frames.get_mut(frame_index) {
                frame.projected = FrameProjection::Resolved {
                    source_url,
                    position,
                };
            }
            let mut sessions = (*state.sessions).clone();
            let session_state = Arc::make_mut(sessions.get_mut(&session).unwrap());
            session_state.pause = Some(Arc::new(PauseState {
                epoch: pause.epoch,
                reason: pause.reason.clone(),
                frames: Arc::new(frames),
            }));
            state.sessions = Arc::new(sessions);
        }
        Input::ResumeRequested {
            session,
            pause_epoch,
        } => {
            let Some(session_state) = state.sessions.get(&session) else {
                invalid(
                    &mut state,
                    format!("resume for unknown session {session:?}"),
                );
                return finish(state, effects);
            };
            if !matches!(
                session_state.phase,
                SessionPhase::Paused { epoch } if epoch == pause_epoch
            ) {
                invalid(
                    &mut state,
                    format!("resume requested for stale pause epoch {pause_epoch}"),
                );
                return finish(state, effects);
            }
            Arc::make_mut(&mut state.sessions)
                .get_mut(&session)
                .map(Arc::make_mut)
                .unwrap()
                .phase = SessionPhase::Resuming { epoch: pause_epoch };
            let effect_id = allocate_effect(
                &mut state,
                PendingEffect::Resume {
                    session: session.clone(),
                    pause_epoch,
                },
            );
            effects.push(Effect::Resume {
                effect_id,
                session,
                pause_epoch,
            });
        }
        Input::StepRequested {
            session,
            pause_epoch,
            kind,
        } => {
            let Some(session_state) = state.sessions.get(&session) else {
                invalid(&mut state, format!("step for unknown session {session:?}"));
                return finish(state, effects);
            };
            if !matches!(
                session_state.phase,
                SessionPhase::Paused { epoch } if epoch == pause_epoch
            ) {
                invalid(
                    &mut state,
                    format!("step requested for stale pause epoch {pause_epoch}"),
                );
                return finish(state, effects);
            }
            Arc::make_mut(&mut state.sessions)
                .get_mut(&session)
                .map(Arc::make_mut)
                .unwrap()
                .phase = SessionPhase::Resuming { epoch: pause_epoch };
            let effect_id = allocate_effect(
                &mut state,
                PendingEffect::Resume {
                    session: session.clone(),
                    pause_epoch,
                },
            );
            effects.push(Effect::Step {
                effect_id,
                session,
                pause_epoch,
                kind,
            });
        }
        Input::Resumed {
            session,
            pause_epoch,
        } => resume_session(&mut state, &session, pause_epoch),
        Input::EffectFailed { effect_id, message } => {
            let Some(pending) = take_pending(&mut state, effect_id) else {
                stale_effect(&mut state, effect_id);
                return finish(state, effects);
            };
            push_diagnostic(
                &mut state,
                Diagnostic::EffectFailed {
                    effect_id,
                    message: message.clone(),
                },
            );
            match pending {
                PendingEffect::FetchScriptSource { script, version }
                | PendingEffect::BuildSourceView { script, version }
                    if state.scripts.get(&script).map(|value| value.version) == Some(version) =>
                {
                    Arc::make_mut(&mut state.scripts)
                        .get_mut(&script)
                        .map(Arc::make_mut)
                        .unwrap()
                        .source = ScriptSourceState::Failed(message.clone());
                    fail_raw_frames_for_script(&mut state, &script, message);
                }
                PendingEffect::FetchScriptSource { .. } | PendingEffect::BuildSourceView { .. } => {
                }
                PendingEffect::MapBreakpoint {
                    breakpoint,
                    breakpoint_generation,
                    script,
                    version,
                } => {
                    if state.scripts.get(&script).map(|value| value.version) == Some(version)
                        && state
                            .breakpoints
                            .get(&breakpoint)
                            .map(|value| value.generation)
                            == Some(breakpoint_generation)
                    {
                        let breakpoint_state = state.breakpoints[&breakpoint].clone();
                        Arc::make_mut(&mut state.breakpoints)
                            .get_mut(&breakpoint)
                            .map(Arc::make_mut)
                            .unwrap()
                            .pending_mappings =
                            Arc::new(without_key(&breakpoint_state.pending_mappings, &script));
                    }
                }
                PendingEffect::InstallBreakpoint { physical } => {
                    let Some(physical_state) = state.physical_breakpoints.get(&physical).cloned()
                    else {
                        return finish(state, effects);
                    };
                    if !matches!(
                        physical_state.status,
                        PhysicalBreakpointStatus::Installing(candidate)
                            if candidate == effect_id
                    ) {
                        return finish(state, effects);
                    }
                    Arc::make_mut(&mut state.physical_breakpoints)
                        .get_mut(&physical)
                        .map(Arc::make_mut)
                        .unwrap()
                        .status = PhysicalBreakpointStatus::Failed {
                        message: message.clone(),
                    };
                    let breakpoints = Arc::make_mut(&mut state.breakpoints);
                    for owner in physical_state.owners.iter() {
                        if let Some(breakpoint) = breakpoints.get_mut(owner) {
                            let breakpoint = Arc::make_mut(breakpoint);
                            breakpoint.bindings = Arc::new(with_insert(
                                &breakpoint.bindings,
                                physical.clone(),
                                BreakpointBinding::Failed {
                                    message: message.clone(),
                                },
                            ));
                        }
                    }
                }
                PendingEffect::RemoveBreakpoint {
                    physical,
                    backend_id,
                } => {
                    if let Some(physical_state) =
                        Arc::make_mut(&mut state.physical_breakpoints).get_mut(&physical)
                    {
                        if matches!(
                            physical_state.status,
                            PhysicalBreakpointStatus::Removing(candidate)
                                if candidate == effect_id
                        ) {
                            Arc::make_mut(physical_state).status =
                                PhysicalBreakpointStatus::Installed { backend_id };
                        }
                    }
                }
                PendingEffect::MapFrame {
                    session,
                    pause_epoch,
                    frame_index,
                } => {
                    fail_frame_projection(
                        &mut state,
                        &session,
                        pause_epoch,
                        frame_index,
                        effect_id,
                        message,
                    );
                }
                PendingEffect::Resume {
                    session,
                    pause_epoch,
                } => {
                    if let Some(session_state) =
                        Arc::make_mut(&mut state.sessions).get_mut(&session)
                    {
                        let session_state = Arc::make_mut(session_state);
                        if matches!(
                            session_state.phase,
                            SessionPhase::Resuming { epoch } if epoch == pause_epoch
                        ) && session_state.pause.as_ref().map(|pause| pause.epoch)
                            == Some(pause_epoch)
                        {
                            session_state.phase = SessionPhase::Paused { epoch: pause_epoch };
                        }
                    }
                }
                PendingEffect::ConfigureSession { session }
                | PendingEffect::RunIfWaiting { session } => {
                    if let Some(session_state) =
                        Arc::make_mut(&mut state.sessions).get_mut(&session)
                    {
                        Arc::make_mut(session_state).phase = SessionPhase::Failed { message };
                    }
                }
            }
        }
    }

    finish(state, effects)
}

fn schedule_mappings_for_script(
    state: &mut DebuggerState,
    script: &ScriptKey,
    effects: &mut Vec<Effect>,
) {
    let breakpoint_keys: Vec<_> = state
        .breakpoints
        .keys()
        .filter(|key| script_has_source(state, script, key))
        .cloned()
        .collect();
    for breakpoint in breakpoint_keys {
        schedule_mapping(state, &breakpoint, script, effects);
    }
}

fn schedule_source_hydration(
    state: &mut DebuggerState,
    script: &ScriptKey,
    retry_failed: bool,
    effects: &mut Vec<Effect>,
) {
    let Some(script_state) = state.scripts.get(script).cloned() else {
        return;
    };
    let should_hydrate = match &script_state.source {
        ScriptSourceState::Unresolved => true,
        ScriptSourceState::Failed(_) => retry_failed,
        ScriptSourceState::Pending(_)
        | ScriptSourceState::Loaded { .. }
        | ScriptSourceState::Resolved(_) => false,
    };
    if !should_hydrate {
        return;
    }
    let effect_id = allocate_effect(
        state,
        PendingEffect::FetchScriptSource {
            script: script.clone(),
            version: script_state.version,
        },
    );
    Arc::make_mut(&mut state.scripts)
        .get_mut(script)
        .map(Arc::make_mut)
        .unwrap()
        .source = ScriptSourceState::Pending(effect_id);
    effects.push(Effect::FetchScriptSource {
        effect_id,
        script: script.clone(),
        script_version: script_state.version,
        generated_url: script_state.url.clone(),
        source_map_url: script_state.source_map_url.clone(),
    });
}

fn script_has_breakpoint_demand(state: &DebuggerState, script: &ScriptKey) -> bool {
    state
        .breakpoints
        .keys()
        .any(|breakpoint| script_may_expose_breakpoint(state, script, breakpoint))
}

fn script_may_expose_breakpoint(
    state: &DebuggerState,
    script: &ScriptKey,
    breakpoint: &BreakpointKey,
) -> bool {
    let Some(script) = state.scripts.get(script) else {
        return false;
    };
    let Some(breakpoint) = state.breakpoints.get(breakpoint) else {
        return false;
    };
    script.url == breakpoint.source_url || script.source_map_url.is_some()
}

fn script_has_frame_demand(state: &DebuggerState, script: &ScriptKey) -> bool {
    state.sessions.values().any(|session| {
        matches!(
            session.phase,
            SessionPhase::Paused { .. } | SessionPhase::Resuming { .. }
        ) && session.pause.as_ref().is_some_and(|pause| {
            pause.frames.iter().any(|frame| {
                &frame.raw_script == script && matches!(frame.projected, FrameProjection::Raw)
            })
        })
    })
}

fn script_has_source(
    state: &DebuggerState,
    script: &ScriptKey,
    breakpoint: &BreakpointKey,
) -> bool {
    let Some(breakpoint) = state.breakpoints.get(breakpoint) else {
        return false;
    };
    let Some(script) = state.scripts.get(script) else {
        return false;
    };
    match &script.source {
        ScriptSourceState::Resolved(view) => {
            view.logical_sources.contains_key(&breakpoint.source_url)
        }
        _ => false,
    }
}

fn schedule_frame_mappings_for_script(
    state: &mut DebuggerState,
    script: &ScriptKey,
    effects: &mut Vec<Effect>,
) {
    let Some(script_state) = state.scripts.get(script) else {
        return;
    };
    let ScriptSourceState::Resolved(view) = &script_state.source else {
        return;
    };
    let view_id = view.view_id;
    let groups: Vec<_> = state
        .sessions
        .iter()
        .filter_map(|(session, session_state)| {
            if !matches!(
                session_state.phase,
                SessionPhase::Paused { .. } | SessionPhase::Resuming { .. }
            ) {
                return None;
            }
            let pause = session_state.pause.as_ref()?;
            let frames: Vec<_> = pause
                .frames
                .iter()
                .enumerate()
                .filter(|(_, frame)| {
                    &frame.raw_script == script && matches!(frame.projected, FrameProjection::Raw)
                })
                .map(|(index, frame)| (index, frame.raw_position))
                .collect();
            (!frames.is_empty()).then(|| (session.clone(), pause.epoch, frames))
        })
        .collect();

    for (session, pause_epoch, frames) in groups {
        let mapped_frames: Vec<_> = frames
            .into_iter()
            .map(|(frame_index, position)| {
                let effect_id = allocate_effect(
                    state,
                    PendingEffect::MapFrame {
                        session: session.clone(),
                        pause_epoch,
                        frame_index,
                    },
                );
                (frame_index, position, effect_id)
            })
            .collect();

        let sessions = Arc::make_mut(&mut state.sessions);
        let session_state = Arc::make_mut(
            sessions
                .get_mut(&session)
                .expect("collected session remains present"),
        );
        let pause = session_state
            .pause
            .as_ref()
            .expect("collected pause remains present");
        debug_assert_eq!(pause.epoch, pause_epoch);
        let mut next_frames = (*pause.frames).clone();
        for (frame_index, position, effect_id) in mapped_frames {
            let frame = next_frames
                .get_mut(frame_index)
                .expect("collected frame remains present");
            debug_assert_eq!(&frame.raw_script, script);
            debug_assert!(matches!(frame.projected, FrameProjection::Raw));
            frame.projected = FrameProjection::Pending(effect_id);
            effects.push(Effect::MapFrame {
                effect_id,
                session: session.clone(),
                pause_epoch,
                frame_index,
                script: script.clone(),
                view_id,
                position,
            });
        }
        session_state.pause = Some(Arc::new(PauseState {
            epoch: pause.epoch,
            reason: pause.reason.clone(),
            frames: Arc::new(next_frames),
        }));
    }
}

fn fail_raw_frames_for_script(state: &mut DebuggerState, script: &ScriptKey, message: String) {
    let mut sessions = (*state.sessions).clone();
    let mut changed = false;
    for session_state in sessions.values_mut() {
        let session_state = Arc::make_mut(session_state);
        let Some(pause) = session_state.pause.as_ref() else {
            continue;
        };
        if !pause.frames.iter().any(|frame| {
            &frame.raw_script == script && matches!(frame.projected, FrameProjection::Raw)
        }) {
            continue;
        }
        let mut frames = (*pause.frames).clone();
        for frame in &mut frames {
            if &frame.raw_script == script && matches!(frame.projected, FrameProjection::Raw) {
                frame.projected = FrameProjection::Failed {
                    message: message.clone(),
                };
            }
        }
        session_state.pause = Some(Arc::new(PauseState {
            epoch: pause.epoch,
            reason: pause.reason.clone(),
            frames: Arc::new(frames),
        }));
        changed = true;
    }
    if changed {
        state.sessions = Arc::new(sessions);
    }
}

fn schedule_mapping(
    state: &mut DebuggerState,
    breakpoint: &BreakpointKey,
    script: &ScriptKey,
    effects: &mut Vec<Effect>,
) {
    let breakpoint_state = state.breakpoints.get(breakpoint).unwrap().clone();
    if breakpoint_state.pending_mappings.contains_key(script) {
        return;
    }
    let script_state = state.scripts.get(script).unwrap().clone();
    let ScriptSourceState::Resolved(view) = &script_state.source else {
        return;
    };
    let effect_id = allocate_effect(
        state,
        PendingEffect::MapBreakpoint {
            breakpoint: breakpoint.clone(),
            breakpoint_generation: breakpoint_state.generation,
            script: script.clone(),
            version: script_state.version,
        },
    );
    Arc::make_mut(&mut state.breakpoints)
        .get_mut(breakpoint)
        .map(Arc::make_mut)
        .unwrap()
        .pending_mappings = Arc::new(with_insert(
        &breakpoint_state.pending_mappings,
        script.clone(),
        effect_id,
    ));
    effects.push(Effect::MapBreakpoint {
        effect_id,
        breakpoint: breakpoint.clone(),
        script: script.clone(),
        view_id: view.view_id,
        source_url: breakpoint_state.source_url.clone(),
        position: breakpoint_state.position,
    });
}

fn bind_physical(
    state: &mut DebuggerState,
    owner: &BreakpointKey,
    script: &ScriptKey,
    position: Position,
    effects: &mut Vec<Effect>,
) {
    let breakpoint = state.breakpoints.get(owner).unwrap().clone();
    let physical = PhysicalBreakpointKey {
        script: script.clone(),
        script_version: state.scripts[script].version,
        position,
        condition: breakpoint.condition.clone(),
    };
    if state.physical_breakpoints.contains_key(&physical) {
        let mut physical_map = (*state.physical_breakpoints).clone();
        let existing = Arc::make_mut(physical_map.get_mut(&physical).unwrap());
        existing.owners = Arc::new(with_set_insert(&existing.owners, owner.clone()));
        let status = existing.status.clone();
        state.physical_breakpoints = Arc::new(physical_map);
        let binding = match status {
            PhysicalBreakpointStatus::Installing(effect_id) => {
                BreakpointBinding::PendingInstall(effect_id)
            }
            PhysicalBreakpointStatus::Installed { backend_id } => {
                BreakpointBinding::Installed { backend_id }
            }
            PhysicalBreakpointStatus::Removing(effect_id) => {
                BreakpointBinding::WaitingForRemoval(effect_id)
            }
            PhysicalBreakpointStatus::Failed { message } => {
                Arc::make_mut(&mut state.breakpoints)
                    .get_mut(owner)
                    .map(Arc::make_mut)
                    .unwrap()
                    .bindings = Arc::new(with_insert(
                    &breakpoint.bindings,
                    physical.clone(),
                    BreakpointBinding::Failed { message },
                ));
                schedule_physical_install(state, &physical, effects);
                return;
            }
        };
        Arc::make_mut(&mut state.breakpoints)
            .get_mut(owner)
            .map(Arc::make_mut)
            .unwrap()
            .bindings = Arc::new(with_insert(&breakpoint.bindings, physical, binding));
        return;
    }

    let effect_id = allocate_effect(
        state,
        PendingEffect::InstallBreakpoint {
            physical: physical.clone(),
        },
    );
    Arc::make_mut(&mut state.physical_breakpoints).insert(
        physical.clone(),
        Arc::new(PhysicalBreakpointState {
            owners: Arc::new(BTreeSet::from([owner.clone()])),
            status: PhysicalBreakpointStatus::Installing(effect_id),
        }),
    );
    Arc::make_mut(&mut state.breakpoints)
        .get_mut(owner)
        .map(Arc::make_mut)
        .unwrap()
        .bindings = Arc::new(with_insert(
        &breakpoint.bindings,
        physical.clone(),
        BreakpointBinding::PendingInstall(effect_id),
    ));
    effects.push(Effect::InstallBreakpoint {
        effect_id,
        physical,
    });
}

fn schedule_physical_install(
    state: &mut DebuggerState,
    physical: &PhysicalBreakpointKey,
    effects: &mut Vec<Effect>,
) {
    let effect_id = allocate_effect(
        state,
        PendingEffect::InstallBreakpoint {
            physical: physical.clone(),
        },
    );
    let Some(physical_state) = Arc::make_mut(&mut state.physical_breakpoints).get_mut(physical)
    else {
        take_pending(state, effect_id);
        invalid(
            state,
            format!("cannot install missing physical breakpoint {physical:?}"),
        );
        return;
    };
    let owners = physical_state.owners.clone();
    Arc::make_mut(physical_state).status = PhysicalBreakpointStatus::Installing(effect_id);
    for owner in owners.iter() {
        if let Some(breakpoint) = Arc::make_mut(&mut state.breakpoints).get_mut(owner) {
            let breakpoint = Arc::make_mut(breakpoint);
            breakpoint.bindings = Arc::new(with_insert(
                &breakpoint.bindings,
                physical.clone(),
                BreakpointBinding::PendingInstall(effect_id),
            ));
        }
    }
    effects.push(Effect::InstallBreakpoint {
        effect_id,
        physical: physical.clone(),
    });
}

fn release_script_version(
    state: &mut DebuggerState,
    script: &ScriptKey,
    effects: &mut Vec<Effect>,
) {
    let mut sessions = (*state.sessions).clone();
    let mut cancelled_frame_mappings = Vec::new();
    let mut changed_sessions = false;
    for session_state in sessions.values_mut() {
        let session_state = Arc::make_mut(session_state);
        let Some(pause) = session_state.pause.as_ref() else {
            continue;
        };
        if !pause.frames.iter().any(|frame| &frame.raw_script == script) {
            continue;
        }
        let mut frames = (*pause.frames).clone();
        for frame in &mut frames {
            if &frame.raw_script != script {
                continue;
            }
            if let FrameProjection::Pending(effect_id) = frame.projected {
                cancelled_frame_mappings.push(effect_id);
            }
            frame.projected = FrameProjection::Raw;
        }
        session_state.pause = Some(Arc::new(PauseState {
            epoch: pause.epoch,
            reason: pause.reason.clone(),
            frames: Arc::new(frames),
        }));
        changed_sessions = true;
    }
    if changed_sessions {
        state.sessions = Arc::new(sessions);
    }
    for effect_id in cancelled_frame_mappings {
        take_pending(state, effect_id);
    }

    let physical_keys: Vec<_> = state
        .physical_breakpoints
        .keys()
        .filter(|physical| &physical.script == script)
        .cloned()
        .collect();
    if physical_keys.is_empty() {
        for breakpoint in Arc::make_mut(&mut state.breakpoints).values_mut() {
            let breakpoint = Arc::make_mut(breakpoint);
            breakpoint.pending_mappings =
                Arc::new(without_key(&breakpoint.pending_mappings, script));
        }
        return;
    }

    let physical_key_set: BTreeSet<_> = physical_keys.iter().cloned().collect();
    for breakpoint in Arc::make_mut(&mut state.breakpoints).values_mut() {
        let breakpoint = Arc::make_mut(breakpoint);
        breakpoint.pending_mappings = Arc::new(without_key(&breakpoint.pending_mappings, script));
        breakpoint.bindings = Arc::new(
            breakpoint
                .bindings
                .iter()
                .filter(|(physical, _)| !physical_key_set.contains(*physical))
                .map(|(physical, binding)| (physical.clone(), binding.clone()))
                .collect(),
        );
    }

    let mut physical_breakpoints = (*state.physical_breakpoints).clone();
    let mut removals = Vec::new();
    for physical in physical_keys {
        let Some(physical_state) = physical_breakpoints.get_mut(&physical) else {
            continue;
        };
        let physical_state = Arc::make_mut(physical_state);
        physical_state.owners = Arc::new(BTreeSet::new());
        match &physical_state.status {
            PhysicalBreakpointStatus::Installed { backend_id } => {
                removals.push((physical, backend_id.clone()));
            }
            PhysicalBreakpointStatus::Failed { .. } => {
                physical_breakpoints.remove(&physical);
            }
            PhysicalBreakpointStatus::Installing(_) | PhysicalBreakpointStatus::Removing(_) => {}
        }
    }
    state.physical_breakpoints = Arc::new(physical_breakpoints);
    for (physical, backend_id) in removals {
        schedule_physical_removal(state, &physical, backend_id, effects);
    }
}

fn release_breakpoint(state: &mut DebuggerState, key: &BreakpointKey, effects: &mut Vec<Effect>) {
    let Some(breakpoint) = state.breakpoints.get(key).cloned() else {
        return;
    };

    let pending_ids: BTreeSet<_> = breakpoint.pending_mappings.values().copied().collect();
    Arc::make_mut(&mut state.pending).retain(|effect_id, _| !pending_ids.contains(effect_id));

    let mut physical_breakpoints = (*state.physical_breakpoints).clone();
    let mut removals = Vec::new();
    for physical in breakpoint.bindings.keys() {
        let Some(physical_state) = physical_breakpoints.get_mut(physical) else {
            continue;
        };
        let physical_state = Arc::make_mut(physical_state);
        let mut owners = (*physical_state.owners).clone();
        owners.remove(key);
        physical_state.owners = Arc::new(owners);
        if physical_state.owners.is_empty() {
            match &physical_state.status {
                PhysicalBreakpointStatus::Installed { backend_id } => {
                    removals.push((physical.clone(), backend_id.clone()));
                }
                PhysicalBreakpointStatus::Failed { .. } => {
                    physical_breakpoints.remove(physical);
                }
                PhysicalBreakpointStatus::Installing(_) | PhysicalBreakpointStatus::Removing(_) => {
                }
            }
        }
    }
    state.physical_breakpoints = Arc::new(physical_breakpoints);

    for (physical, backend_id) in removals {
        schedule_physical_removal(state, &physical, backend_id, effects);
    }
}

fn schedule_physical_removal(
    state: &mut DebuggerState,
    physical: &PhysicalBreakpointKey,
    backend_id: String,
    effects: &mut Vec<Effect>,
) {
    let effect_id = allocate_effect(
        state,
        PendingEffect::RemoveBreakpoint {
            physical: physical.clone(),
            backend_id: backend_id.clone(),
        },
    );
    let Some(physical_state) = Arc::make_mut(&mut state.physical_breakpoints).get_mut(physical)
    else {
        take_pending(state, effect_id);
        invalid(
            state,
            format!("cannot remove missing physical breakpoint {physical:?}"),
        );
        return;
    };
    Arc::make_mut(physical_state).status = PhysicalBreakpointStatus::Removing(effect_id);
    effects.push(Effect::RemoveBreakpoint {
        effect_id,
        physical: physical.clone(),
        backend_id,
    });
}

fn pause_session(
    state: &mut DebuggerState,
    session: &SessionKey,
    reason: String,
    raw_frames: Vec<RawFrame>,
    effects: &mut Vec<Effect>,
) {
    let Some(existing) = state.sessions.get(session).cloned() else {
        invalid(state, format!("pause for unknown session {session:?}"));
        return;
    };
    match existing.phase {
        SessionPhase::Running => {}
        SessionPhase::Resuming { epoch } => {
            Arc::make_mut(&mut state.pending).retain(|_, effect| {
                !matches!(
                    effect,
                    PendingEffect::Resume {
                        session: candidate,
                        pause_epoch,
                    } if candidate == session && *pause_epoch == epoch
                )
            });
        }
        _ => {
            invalid(
                state,
                format!("pause is invalid while session is {:?}", existing.phase),
            );
            return;
        }
    }
    let epoch = existing.next_pause_epoch;
    let frames: Vec<_> = raw_frames
        .into_iter()
        .map(|frame| FrameState {
            call_frame_id: frame.call_frame_id,
            function_name: frame.function_name,
            raw_script: ScriptKey {
                session: session.clone(),
                script_id: frame.script_id,
            },
            raw_position: frame.position,
            projected: FrameProjection::Raw,
        })
        .collect();
    let mut scripts = Vec::new();
    let mut seen_scripts = BTreeSet::new();
    for frame in &frames {
        if seen_scripts.insert(frame.raw_script.clone()) {
            scripts.push(frame.raw_script.clone());
        }
    }

    let mut sessions = (*state.sessions).clone();
    let session_state = Arc::make_mut(sessions.get_mut(session).unwrap());
    session_state.phase = SessionPhase::Paused { epoch };
    session_state.next_pause_epoch += 1;
    session_state.pause = Some(Arc::new(PauseState {
        epoch,
        reason,
        frames: Arc::new(frames),
    }));
    state.sessions = Arc::new(sessions);

    for script in scripts {
        match state.scripts.get(&script).map(|script| &script.source) {
            Some(ScriptSourceState::Resolved(_)) => {
                schedule_frame_mappings_for_script(state, &script, effects);
            }
            Some(ScriptSourceState::Unresolved) => {
                schedule_source_hydration(state, &script, false, effects);
            }
            Some(
                ScriptSourceState::Pending(_)
                | ScriptSourceState::Loaded { .. }
                | ScriptSourceState::Failed(_),
            )
            | None => {}
        }
    }
}

fn resume_session(state: &mut DebuggerState, session: &SessionKey, pause_epoch: u64) {
    let Some(existing) = state.sessions.get(session).cloned() else {
        invalid(state, format!("resumed unknown session {session:?}"));
        return;
    };
    let valid_phase = matches!(
        existing.phase,
        SessionPhase::Paused { epoch } | SessionPhase::Resuming { epoch }
            if epoch == pause_epoch
    );
    if !valid_phase || existing.pause.as_ref().map(|pause| pause.epoch) != Some(pause_epoch) {
        invalid(
            state,
            format!("resumed event for stale pause epoch {pause_epoch}"),
        );
        return;
    }
    let mut sessions = (*state.sessions).clone();
    let session_state = Arc::make_mut(sessions.get_mut(session).unwrap());
    session_state.phase = SessionPhase::Running;
    session_state.pause = None;
    state.sessions = Arc::new(sessions);
    Arc::make_mut(&mut state.pending).retain(|_, effect| {
        !matches!(
            effect,
            PendingEffect::MapFrame {
                session: candidate,
                pause_epoch: candidate_epoch,
                ..
            } if candidate == session && *candidate_epoch == pause_epoch
        )
    });
}

fn detach_session(state: &mut DebuggerState, session: &SessionKey) {
    Arc::make_mut(&mut state.sessions).remove(session);
    let scripts_to_remove: BTreeSet<_> = state
        .scripts
        .keys()
        .filter(|script| &script.session == session)
        .cloned()
        .collect();
    Arc::make_mut(&mut state.scripts).retain(|script, _| !scripts_to_remove.contains(script));
    Arc::make_mut(&mut state.physical_breakpoints)
        .retain(|physical, _| !scripts_to_remove.contains(&physical.script));
    let breakpoints = Arc::make_mut(&mut state.breakpoints);
    for breakpoint in breakpoints.values_mut() {
        let breakpoint = Arc::make_mut(breakpoint);
        breakpoint.pending_mappings = Arc::new(
            breakpoint
                .pending_mappings
                .iter()
                .filter(|(script, _)| !scripts_to_remove.contains(*script))
                .map(|(key, value)| (key.clone(), *value))
                .collect(),
        );
        breakpoint.bindings = Arc::new(
            breakpoint
                .bindings
                .iter()
                .filter(|(physical, _)| !scripts_to_remove.contains(&physical.script))
                .map(|(key, value)| (key.clone(), value.clone()))
                .collect(),
        );
    }
    let before = state.pending.len();
    Arc::make_mut(&mut state.pending).retain(|_, effect| !effect.belongs_to_session(session));
    let cancelled = before - state.pending.len();
    if cancelled > 0 {
        push_diagnostic(
            state,
            Diagnostic::CancelledEffects {
                session: session.clone(),
                count: cancelled,
            },
        );
    }
}

fn fail_frame_projection(
    state: &mut DebuggerState,
    session: &SessionKey,
    pause_epoch: u64,
    frame_index: usize,
    effect_id: EffectId,
    message: String,
) {
    let Some(session_state) = state.sessions.get(session) else {
        return;
    };
    let Some(pause) = &session_state.pause else {
        return;
    };
    if pause.epoch != pause_epoch
        || !matches!(
            pause.frames.get(frame_index).map(|frame| &frame.projected),
            Some(FrameProjection::Pending(candidate)) if *candidate == effect_id
        )
    {
        return;
    }
    let mut frames = (*pause.frames).clone();
    frames[frame_index].projected = FrameProjection::Failed { message };
    let mut sessions = (*state.sessions).clone();
    let session_state = Arc::make_mut(sessions.get_mut(session).unwrap());
    session_state.pause = Some(Arc::new(PauseState {
        epoch: pause.epoch,
        reason: pause.reason.clone(),
        frames: Arc::new(frames),
    }));
    state.sessions = Arc::new(sessions);
}

fn allocate_effect(state: &mut DebuggerState, pending: PendingEffect) -> EffectId {
    let id = EffectId(state.next_effect_id);
    state.next_effect_id += 1;
    Arc::make_mut(&mut state.pending).insert(id, pending);
    id
}

fn take_pending(state: &mut DebuggerState, effect_id: EffectId) -> Option<PendingEffect> {
    Arc::make_mut(&mut state.pending).remove(&effect_id)
}

fn stale_effect(state: &mut DebuggerState, effect_id: EffectId) {
    push_diagnostic(state, Diagnostic::IgnoredStaleEffect { effect_id });
}

fn invalid(state: &mut DebuggerState, description: String) {
    push_diagnostic(state, Diagnostic::InvalidTransition { description });
}

fn push_diagnostic(state: &mut DebuggerState, diagnostic: Diagnostic) {
    let diagnostics = Arc::make_mut(&mut state.diagnostics);
    if diagnostics.len() == MAX_DIAGNOSTICS {
        diagnostics.remove(0);
    }
    diagnostics.push(diagnostic);
}

fn finish(state: DebuggerState, effects: Vec<Effect>) -> Transition {
    Transition {
        state: Arc::new(state),
        effects,
    }
}

fn with_insert<K: Ord + Clone, V: Clone>(map: &BTreeMap<K, V>, key: K, value: V) -> BTreeMap<K, V> {
    let mut result = map.clone();
    result.insert(key, value);
    result
}

fn without_key<K: Ord + Clone, V: Clone>(map: &BTreeMap<K, V>, key: &K) -> BTreeMap<K, V> {
    let mut result = map.clone();
    result.remove(key);
    result
}

fn with_set_insert<T: Ord + Clone>(set: &BTreeSet<T>, value: T) -> BTreeSet<T> {
    let mut result = set.clone();
    result.insert(value);
    result
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reuses_unaffected_state_nodes() {
        let initial = Arc::new(DebuggerState::default());
        let connected = reduce(&initial, Input::Connected).state;
        let attached = reduce(
            &connected,
            Input::SessionAttached {
                session_id: "s1".into(),
                target_id: "t1".into(),
                parent_session_id: None,
                waiting_for_debugger: false,
            },
        )
        .state;
        let sessions = attached.sessions.clone();
        let scripts = attached.scripts.clone();

        let with_breakpoint = reduce(
            &attached,
            Input::SetBreakpoint {
                key: breakpoint_key(),
                source_url: "src/app.ts".into(),
                position: Position::ZERO,
                condition: None,
            },
        )
        .state;

        assert!(Arc::ptr_eq(&sessions, &with_breakpoint.sessions));
        assert!(Arc::ptr_eq(&scripts, &with_breakpoint.scripts));
        assert_eq!(with_breakpoint.revision, attached.revision + 1);
    }

    #[test]
    fn script_metadata_is_lazy_and_explicit_requests_are_deduplicated() {
        let (state, session) = configured_session();
        let parsed = reduce(
            &state,
            Input::ScriptParsed {
                session,
                script_id: "1".into(),
                url: "bundle.js".into(),
                hash: "hash".into(),
                source_map_url: Some("bundle.js.map".into()),
            },
        );
        assert!(parsed.effects.is_empty());
        let script = parsed.state.scripts.keys().next().unwrap().clone();
        assert!(matches!(
            parsed.state.scripts[&script].source,
            ScriptSourceState::Unresolved
        ));

        let requested = reduce(
            &parsed.state,
            Input::RequestScriptSource {
                script: script.clone(),
            },
        );
        assert!(matches!(
            requested.effects.as_slice(),
            [Effect::FetchScriptSource {
                script: candidate,
                ..
            }] if candidate == &script
        ));
        let requested_again = reduce(
            &requested.state,
            Input::RequestScriptSource {
                script: script.clone(),
            },
        );
        assert!(requested_again.effects.is_empty());
        assert!(matches!(
            requested_again.state.scripts[&script].source,
            ScriptSourceState::Pending(_)
        ));
    }

    #[test]
    fn breakpoint_intent_hydrates_only_scripts_that_can_expose_its_source() {
        let (state, session) = configured_session();
        let unmapped = reduce(
            &state,
            Input::ScriptParsed {
                session: session.clone(),
                script_id: "plain".into(),
                url: "plain.js".into(),
                hash: "plain".into(),
                source_map_url: None,
            },
        );
        let mapped = reduce(
            &unmapped.state,
            Input::ScriptParsed {
                session,
                script_id: "bundle".into(),
                url: "bundle.js".into(),
                hash: "bundle".into(),
                source_map_url: Some("bundle.js.map".into()),
            },
        );
        let bundle = mapped
            .state
            .scripts
            .keys()
            .find(|script| script.script_id == "bundle")
            .unwrap()
            .clone();
        let with_breakpoint = reduce(
            &mapped.state,
            Input::SetBreakpoint {
                key: breakpoint_key(),
                source_url: "src/app.ts".into(),
                position: Position::ZERO,
                condition: None,
            },
        );

        assert!(matches!(
            with_breakpoint.effects.as_slice(),
            [Effect::FetchScriptSource {
                script: candidate,
                ..
            }] if candidate == &bundle
        ));
        assert!(matches!(
            with_breakpoint
                .state
                .scripts
                .values()
                .find(|script| script.url == "plain.js")
                .unwrap()
                .source,
            ScriptSourceState::Unresolved
        ));
    }

    #[test]
    fn resolving_a_paused_script_projects_every_matching_raw_frame() {
        let (state, session) = configured_session();
        let parsed = reduce(
            &state,
            Input::ScriptParsed {
                session: session.clone(),
                script_id: "1".into(),
                url: "bundle.js".into(),
                hash: "hash".into(),
                source_map_url: Some("bundle.js.map".into()),
            },
        );
        let script = parsed.state.scripts.keys().next().unwrap().clone();
        let paused = reduce(
            &parsed.state,
            Input::Paused {
                session: session.clone(),
                reason: "breakpoint".into(),
                frames: vec![
                    RawFrame {
                        call_frame_id: "frame-1".into(),
                        function_name: "first".into(),
                        script_id: "1".into(),
                        position: Position::ZERO,
                    },
                    RawFrame {
                        call_frame_id: "frame-2".into(),
                        function_name: "second".into(),
                        script_id: "1".into(),
                        position: Position { line: 1, column: 2 },
                    },
                ],
            },
        );
        let Effect::FetchScriptSource {
            effect_id: fetch_id,
            ..
        } = paused.effects[0]
        else {
            panic!("pause should hydrate its unresolved script");
        };
        assert!(
            paused.state.sessions[&session]
                .pause
                .as_ref()
                .unwrap()
                .frames
                .iter()
                .all(|frame| matches!(frame.projected, FrameProjection::Raw))
        );

        let fetched = reduce(
            &paused.state,
            Input::ScriptSourceFetched {
                effect_id: fetch_id,
                content: Arc::from("compiled"),
                source_map: Some(Arc::from([])),
                source_map_error: None,
            },
        );
        let Effect::BuildSourceView {
            effect_id: view_id, ..
        } = fetched.effects[0]
        else {
            panic!("fetched source should build a view");
        };
        let built = reduce(
            &fetched.state,
            Input::SourceViewBuilt {
                effect_id: view_id,
                logical_sources: BTreeMap::new(),
            },
        );

        assert_eq!(built.effects.len(), 2);
        assert!(built.effects.iter().enumerate().all(|(index, effect)| {
            matches!(
                effect,
                Effect::MapFrame {
                    session: candidate_session,
                    frame_index,
                    script: candidate_script,
                    ..
                } if candidate_session == &session
                    && *frame_index == index
                    && candidate_script == &script
            )
        }));
        assert!(
            built.state.sessions[&session]
                .pause
                .as_ref()
                .unwrap()
                .frames
                .iter()
                .all(|frame| matches!(frame.projected, FrameProjection::Pending(_)))
        );
    }

    #[test]
    fn resume_before_source_build_cancels_new_frame_mapping_work() {
        let (state, session) = configured_session();
        let parsed = reduce(
            &state,
            Input::ScriptParsed {
                session: session.clone(),
                script_id: "1".into(),
                url: "bundle.js".into(),
                hash: "hash".into(),
                source_map_url: Some("bundle.js.map".into()),
            },
        );
        let paused = reduce(
            &parsed.state,
            Input::Paused {
                session: session.clone(),
                reason: "breakpoint".into(),
                frames: vec![RawFrame {
                    call_frame_id: "frame".into(),
                    function_name: "run".into(),
                    script_id: "1".into(),
                    position: Position::ZERO,
                }],
            },
        );
        let Effect::FetchScriptSource {
            effect_id: fetch_id,
            ..
        } = paused.effects[0]
        else {
            panic!("pause should hydrate its script");
        };
        let fetched = reduce(
            &paused.state,
            Input::ScriptSourceFetched {
                effect_id: fetch_id,
                content: Arc::from("compiled"),
                source_map: None,
                source_map_error: None,
            },
        );
        let Effect::BuildSourceView {
            effect_id: view_id, ..
        } = fetched.effects[0]
        else {
            panic!("source should build a view");
        };
        let resume_requested = reduce(
            &fetched.state,
            Input::ResumeRequested {
                session: session.clone(),
                pause_epoch: 1,
            },
        );
        let Effect::Resume {
            effect_id: resume_id,
            ..
        } = resume_requested.effects[0]
        else {
            panic!("resume should reach the backend");
        };
        let resume_accepted = reduce(
            &resume_requested.state,
            Input::CommandAccepted {
                effect_id: resume_id,
            },
        );
        let built = reduce(
            &resume_accepted.state,
            Input::SourceViewBuilt {
                effect_id: view_id,
                logical_sources: BTreeMap::new(),
            },
        );
        let Effect::MapFrame {
            effect_id: mapping_id,
            ..
        } = built.effects[0]
        else {
            panic!("resuming pause can still receive source projection work");
        };
        let resumed = reduce(
            &built.state,
            Input::Resumed {
                session,
                pause_epoch: 1,
            },
        );
        assert!(
            resumed
                .state
                .sessions
                .values()
                .next()
                .unwrap()
                .pause
                .is_none()
        );

        let stale_mapping = reduce(
            &resumed.state,
            Input::FrameMapped {
                effect_id: mapping_id,
                source_url: "bundle.js".into(),
                position: Position::ZERO,
            },
        );
        assert!(matches!(
            stale_mapping.state.diagnostics.last(),
            Some(Diagnostic::IgnoredStaleEffect { effect_id }) if *effect_id == mapping_id
        ));
    }

    #[test]
    fn reparsing_a_paused_script_redrives_hydration_for_the_new_version() {
        let (state, session) = configured_session();
        let parsed = reduce(
            &state,
            Input::ScriptParsed {
                session: session.clone(),
                script_id: "1".into(),
                url: "bundle.js".into(),
                hash: "old".into(),
                source_map_url: Some("bundle.js.map".into()),
            },
        );
        let paused = reduce(
            &parsed.state,
            Input::Paused {
                session: session.clone(),
                reason: "breakpoint".into(),
                frames: vec![RawFrame {
                    call_frame_id: "frame".into(),
                    function_name: "run".into(),
                    script_id: "1".into(),
                    position: Position::ZERO,
                }],
            },
        );
        let Effect::FetchScriptSource {
            effect_id: old_fetch,
            ..
        } = paused.effects[0]
        else {
            panic!("pause should hydrate the old version");
        };

        let reparsed = reduce(
            &paused.state,
            Input::ScriptParsed {
                session,
                script_id: "1".into(),
                url: "bundle.js".into(),
                hash: "new".into(),
                source_map_url: Some("bundle.js.map".into()),
            },
        );
        let Effect::FetchScriptSource {
            effect_id: new_fetch,
            script,
            script_version,
            ..
        } = &reparsed.effects[0]
        else {
            panic!("active frame demand should hydrate the new version");
        };
        assert_ne!(*new_fetch, old_fetch);
        assert_eq!(*script_version, 2);
        assert!(matches!(
            reparsed.state.scripts[script].source,
            ScriptSourceState::Pending(candidate) if candidate == *new_fetch
        ));

        let stale = reduce(
            &reparsed.state,
            Input::ScriptSourceFetched {
                effect_id: old_fetch,
                content: Arc::from("old"),
                source_map: None,
                source_map_error: None,
            },
        );
        assert!(matches!(
            stale.state.scripts[script].source,
            ScriptSourceState::Pending(candidate) if candidate == *new_fetch
        ));
    }

    #[test]
    fn stale_effects_cannot_update_reparsed_scripts() {
        let (state, session) = configured_session();
        let first = reduce(
            &state,
            Input::ScriptParsed {
                session: session.clone(),
                script_id: "1".into(),
                url: "bundle.js".into(),
                hash: "old".into(),
                source_map_url: None,
            },
        );
        assert!(first.effects.is_empty());
        let script = first.state.scripts.keys().next().unwrap().clone();
        let requested = reduce(
            &first.state,
            Input::RequestScriptSource {
                script: script.clone(),
            },
        );
        let Effect::FetchScriptSource {
            effect_id: old_fetch,
            ..
        } = requested.effects[0]
        else {
            panic!("expected fetch");
        };
        let second = reduce(
            &requested.state,
            Input::ScriptParsed {
                session,
                script_id: "1".into(),
                url: "bundle.js".into(),
                hash: "new".into(),
                source_map_url: None,
            },
        );
        let stale = reduce(
            &second.state,
            Input::ScriptSourceFetched {
                effect_id: old_fetch,
                content: Arc::from("old"),
                source_map: None,
                source_map_error: None,
            },
        );
        let script = stale.state.scripts.values().next().unwrap();
        assert_eq!(script.hash, "new");
        assert!(matches!(script.source, ScriptSourceState::Unresolved));
        assert!(matches!(
            stale.state.diagnostics.last(),
            Some(Diagnostic::IgnoredStaleEffect { effect_id }) if *effect_id == old_fetch
        ));
    }

    #[test]
    fn deduplicates_physical_breakpoints_across_clients() {
        let (mut state, session, script, view_id) = resolved_script();
        let generated = Position { line: 0, column: 5 };
        let mut install_effect = None;
        for client in ["a", "b"] {
            let key = BreakpointKey {
                client_id: client.into(),
                breakpoint_id: "bp".into(),
            };
            let added = reduce(
                &state,
                Input::SetBreakpoint {
                    key,
                    source_url: "src/app.ts".into(),
                    position: Position::ZERO,
                    condition: None,
                },
            );
            let Effect::MapBreakpoint {
                effect_id,
                view_id: effect_view,
                ..
            } = added.effects[0]
            else {
                panic!("expected map");
            };
            assert_eq!(effect_view, view_id);
            let mapped = reduce(
                &added.state,
                Input::BreakpointMapped {
                    effect_id,
                    generated_positions: vec![generated],
                },
            );
            if client == "a" {
                let Effect::InstallBreakpoint { effect_id, .. } = mapped.effects[0] else {
                    panic!("expected install");
                };
                install_effect = Some(effect_id);
            } else {
                assert!(mapped.effects.is_empty());
            }
            state = mapped.state;
        }
        assert_eq!(state.physical_breakpoints.len(), 1);
        assert_eq!(
            state
                .physical_breakpoints
                .values()
                .next()
                .unwrap()
                .owners
                .len(),
            2
        );
        let installed = reduce(
            &state,
            Input::BreakpointInstalled {
                effect_id: install_effect.unwrap(),
                backend_id: "chrome-bp-1".into(),
            },
        );
        assert!(installed.state.breakpoints.values().all(|breakpoint| {
            matches!(
                breakpoint.bindings.values().next(),
                Some(BreakpointBinding::Installed { backend_id }) if backend_id == "chrome-bp-1"
            )
        }));
        assert_eq!(script.session, session);
    }

    #[test]
    fn pause_epoch_invalidates_frame_handles_and_stale_mappings() {
        let (state, session, script, _) = resolved_script();
        let paused = reduce(
            &state,
            Input::Paused {
                session: session.clone(),
                reason: "breakpoint".into(),
                frames: vec![RawFrame {
                    call_frame_id: "frame-1".into(),
                    function_name: "main".into(),
                    script_id: script.script_id,
                    position: Position::ZERO,
                }],
            },
        );
        let Effect::MapFrame {
            effect_id,
            pause_epoch,
            ..
        } = paused.effects[0]
        else {
            panic!("expected frame mapping");
        };
        let resumed = reduce(
            &paused.state,
            Input::Resumed {
                session: session.clone(),
                pause_epoch,
            },
        );
        assert!(resumed.state.sessions[&session].pause.is_none());
        let late = reduce(
            &resumed.state,
            Input::FrameMapped {
                effect_id,
                source_url: "src/app.ts".into(),
                position: Position::ZERO,
            },
        );
        assert!(matches!(
            late.state.diagnostics.last(),
            Some(Diagnostic::IgnoredStaleEffect { .. })
        ));

        let paused_again = reduce(
            &late.state,
            Input::Paused {
                session: session.clone(),
                reason: "step".into(),
                frames: Vec::new(),
            },
        );
        assert!(matches!(
            paused_again.state.sessions[&session].phase,
            SessionPhase::Paused { epoch } if epoch > pause_epoch
        ));
    }

    #[test]
    fn stale_resumed_event_cannot_clear_a_newer_pause() {
        let (state, session) = configured_session();
        let first_pause = reduce(
            &state,
            Input::Paused {
                session: session.clone(),
                reason: "first".into(),
                frames: Vec::new(),
            },
        );
        let SessionPhase::Paused { epoch: first_epoch } =
            first_pause.state.sessions[&session].phase
        else {
            panic!("expected first pause");
        };
        let resumed = reduce(
            &first_pause.state,
            Input::Resumed {
                session: session.clone(),
                pause_epoch: first_epoch,
            },
        );
        let second_pause = reduce(
            &resumed.state,
            Input::Paused {
                session: session.clone(),
                reason: "second".into(),
                frames: Vec::new(),
            },
        );
        let SessionPhase::Paused {
            epoch: second_epoch,
        } = second_pause.state.sessions[&session].phase
        else {
            panic!("expected second pause");
        };
        assert!(second_epoch > first_epoch);

        let stale = reduce(
            &second_pause.state,
            Input::Resumed {
                session: session.clone(),
                pause_epoch: first_epoch,
            },
        );
        assert!(matches!(
            stale.state.sessions[&session].phase,
            SessionPhase::Paused { epoch } if epoch == second_epoch
        ));
        assert!(matches!(
            stale.state.diagnostics.last(),
            Some(Diagnostic::InvalidTransition { .. })
        ));
    }

    #[test]
    fn replacing_a_breakpoint_rejects_its_old_mapping() {
        let (state, _, _, _) = resolved_script();
        let key = breakpoint_key();
        let first = reduce(
            &state,
            Input::SetBreakpoint {
                key: key.clone(),
                source_url: "src/app.ts".into(),
                position: Position::ZERO,
                condition: None,
            },
        );
        let Effect::MapBreakpoint {
            effect_id: old_mapping,
            ..
        } = first.effects[0]
        else {
            panic!("expected first mapping");
        };
        let replacement_position = Position { line: 1, column: 2 };
        let replacement = reduce(
            &first.state,
            Input::SetBreakpoint {
                key: key.clone(),
                source_url: "src/app.ts".into(),
                position: replacement_position,
                condition: None,
            },
        );
        let late = reduce(
            &replacement.state,
            Input::BreakpointMapped {
                effect_id: old_mapping,
                generated_positions: vec![Position { line: 9, column: 9 }],
            },
        );

        assert_eq!(late.state.breakpoints[&key].generation, 2);
        assert_eq!(late.state.breakpoints[&key].position, replacement_position);
        assert!(late.state.breakpoints[&key].bindings.is_empty());
        assert!(late.state.physical_breakpoints.is_empty());
        assert!(matches!(
            late.state.diagnostics.last(),
            Some(Diagnostic::IgnoredStaleEffect { .. })
        ));
    }

    #[test]
    fn failed_effects_leave_terminal_or_retryable_state() {
        let (state, session, script, _) = resolved_script();
        let key = breakpoint_key();
        let mapping = reduce(
            &state,
            Input::SetBreakpoint {
                key: key.clone(),
                source_url: "src/app.ts".into(),
                position: Position::ZERO,
                condition: None,
            },
        );
        let Effect::MapBreakpoint {
            effect_id: mapping_id,
            ..
        } = mapping.effects[0]
        else {
            panic!("expected mapping");
        };
        let mapping_failed = reduce(
            &mapping.state,
            Input::EffectFailed {
                effect_id: mapping_id,
                message: "mapping failed".into(),
            },
        );
        assert!(
            mapping_failed.state.breakpoints[&key]
                .pending_mappings
                .is_empty()
        );

        let remapped = reduce(
            &mapping_failed.state,
            Input::SetBreakpoint {
                key: key.clone(),
                source_url: "src/app.ts".into(),
                position: Position::ZERO,
                condition: None,
            },
        );
        let Effect::MapBreakpoint {
            effect_id: mapping_id,
            ..
        } = remapped.effects[0]
        else {
            panic!("expected replacement mapping");
        };
        let installing = reduce(
            &remapped.state,
            Input::BreakpointMapped {
                effect_id: mapping_id,
                generated_positions: vec![Position::ZERO],
            },
        );
        let Effect::InstallBreakpoint {
            effect_id: install_id,
            physical,
        } = installing.effects[0].clone()
        else {
            panic!("expected installation");
        };
        let install_failed = reduce(
            &installing.state,
            Input::EffectFailed {
                effect_id: install_id,
                message: "install failed".into(),
            },
        );
        assert!(matches!(
            install_failed.state.physical_breakpoints[&physical].status,
            PhysicalBreakpointStatus::Failed { .. }
        ));
        assert!(matches!(
            install_failed.state.breakpoints[&key].bindings[&physical],
            BreakpointBinding::Failed { .. }
        ));

        let paused = reduce(
            &install_failed.state,
            Input::Paused {
                session: session.clone(),
                reason: "breakpoint".into(),
                frames: vec![RawFrame {
                    call_frame_id: "frame".into(),
                    function_name: "main".into(),
                    script_id: script.script_id,
                    position: Position::ZERO,
                }],
            },
        );
        let Effect::MapFrame {
            effect_id: frame_mapping,
            ..
        } = paused.effects[0]
        else {
            panic!("expected frame mapping");
        };
        let frame_failed = reduce(
            &paused.state,
            Input::EffectFailed {
                effect_id: frame_mapping,
                message: "frame mapping failed".into(),
            },
        );
        assert!(matches!(
            frame_failed.state.sessions[&session]
                .pause
                .as_ref()
                .unwrap()
                .frames[0]
                .projected,
            FrameProjection::Failed { .. }
        ));

        let pause_epoch = frame_failed.state.sessions[&session]
            .pause
            .as_ref()
            .unwrap()
            .epoch;
        let resuming = reduce(
            &frame_failed.state,
            Input::ResumeRequested {
                session: session.clone(),
                pause_epoch,
            },
        );
        let Effect::Resume {
            effect_id: resume_id,
            ..
        } = resuming.effects[0]
        else {
            panic!("expected resume");
        };
        let resume_failed = reduce(
            &resuming.state,
            Input::EffectFailed {
                effect_id: resume_id,
                message: "resume failed".into(),
            },
        );
        assert!(matches!(
            resume_failed.state.sessions[&session].phase,
            SessionPhase::Paused { epoch } if epoch == pause_epoch
        ));
    }

    #[test]
    fn removing_last_owner_uninstalls_the_physical_breakpoint() {
        let (state, _, _, _) = resolved_script();
        let key = breakpoint_key();
        let mapping = reduce(
            &state,
            Input::SetBreakpoint {
                key: key.clone(),
                source_url: "src/app.ts".into(),
                position: Position::ZERO,
                condition: None,
            },
        );
        let Effect::MapBreakpoint {
            effect_id: mapping_id,
            ..
        } = mapping.effects[0]
        else {
            panic!("expected mapping");
        };
        let installing = reduce(
            &mapping.state,
            Input::BreakpointMapped {
                effect_id: mapping_id,
                generated_positions: vec![Position::ZERO],
            },
        );
        let Effect::InstallBreakpoint {
            effect_id: install_id,
            ..
        } = installing.effects[0]
        else {
            panic!("expected install");
        };
        let installed = reduce(
            &installing.state,
            Input::BreakpointInstalled {
                effect_id: install_id,
                backend_id: "backend-1".into(),
            },
        );
        let removing = reduce(
            &installed.state,
            Input::RemoveBreakpoint { key: key.clone() },
        );
        let Effect::RemoveBreakpoint {
            effect_id: remove_id,
            ref backend_id,
            ..
        } = removing.effects[0]
        else {
            panic!("expected backend removal");
        };
        assert_eq!(backend_id, "backend-1");
        assert!(!removing.state.breakpoints.contains_key(&key));

        let removed = reduce(
            &removing.state,
            Input::BreakpointRemoved {
                effect_id: remove_id,
            },
        );
        assert!(removed.state.physical_breakpoints.is_empty());
    }

    #[test]
    fn breakpoint_added_during_removal_is_reinstalled_after_removal() {
        let (state, _, _, _) = resolved_script();
        let first_key = breakpoint_key();
        let mapping = reduce(
            &state,
            Input::SetBreakpoint {
                key: first_key.clone(),
                source_url: "src/app.ts".into(),
                position: Position::ZERO,
                condition: None,
            },
        );
        let Effect::MapBreakpoint {
            effect_id: mapping_id,
            ..
        } = mapping.effects[0]
        else {
            panic!("expected mapping");
        };
        let installing = reduce(
            &mapping.state,
            Input::BreakpointMapped {
                effect_id: mapping_id,
                generated_positions: vec![Position::ZERO],
            },
        );
        let Effect::InstallBreakpoint {
            effect_id: install_id,
            ..
        } = installing.effects[0]
        else {
            panic!("expected install");
        };
        let installed = reduce(
            &installing.state,
            Input::BreakpointInstalled {
                effect_id: install_id,
                backend_id: "backend-1".into(),
            },
        );
        let removing = reduce(&installed.state, Input::RemoveBreakpoint { key: first_key });
        let Effect::RemoveBreakpoint {
            effect_id: remove_id,
            ..
        } = removing.effects[0]
        else {
            panic!("expected removal");
        };

        let second_key = BreakpointKey {
            client_id: "second-client".into(),
            breakpoint_id: "second-breakpoint".into(),
        };
        let second_mapping = reduce(
            &removing.state,
            Input::SetBreakpoint {
                key: second_key.clone(),
                source_url: "src/app.ts".into(),
                position: Position::ZERO,
                condition: None,
            },
        );
        let Effect::MapBreakpoint {
            effect_id: second_mapping_id,
            ..
        } = second_mapping.effects[0]
        else {
            panic!("expected second mapping");
        };
        let waiting = reduce(
            &second_mapping.state,
            Input::BreakpointMapped {
                effect_id: second_mapping_id,
                generated_positions: vec![Position::ZERO],
            },
        );
        assert!(waiting.effects.is_empty());
        assert!(matches!(
            waiting.state.breakpoints[&second_key]
                .bindings
                .values()
                .next(),
            Some(BreakpointBinding::WaitingForRemoval(candidate))
                if *candidate == remove_id
        ));

        let reinstalling = reduce(
            &waiting.state,
            Input::BreakpointRemoved {
                effect_id: remove_id,
            },
        );
        let Effect::InstallBreakpoint {
            effect_id: reinstall_id,
            ..
        } = reinstalling.effects[0]
        else {
            panic!("expected reinstall");
        };
        let reinstalled = reduce(
            &reinstalling.state,
            Input::BreakpointInstalled {
                effect_id: reinstall_id,
                backend_id: "backend-2".into(),
            },
        );
        assert!(matches!(
            reinstalled.state.breakpoints[&second_key]
                .bindings
                .values()
                .next(),
            Some(BreakpointBinding::Installed { backend_id }) if backend_id == "backend-2"
        ));
    }

    #[test]
    fn unavailable_source_maps_fall_back_with_an_explicit_diagnostic() {
        let (state, session) = configured_session();
        let parsed = reduce(
            &state,
            Input::ScriptParsed {
                session,
                script_id: "1".into(),
                url: "https://example.com/app.js".into(),
                hash: "hash".into(),
                source_map_url: Some("https://example.com/missing.js.map".into()),
            },
        );
        let script = parsed.state.scripts.keys().next().unwrap().clone();
        let requested = reduce(
            &parsed.state,
            Input::RequestScriptSource {
                script: script.clone(),
            },
        );
        let Effect::FetchScriptSource {
            effect_id,
            script: effect_script,
            ..
        } = requested.effects[0].clone()
        else {
            panic!("expected source fetch");
        };
        assert_eq!(effect_script, script);
        let fetched = reduce(
            &requested.state,
            Input::ScriptSourceFetched {
                effect_id,
                content: Arc::from("compiled"),
                source_map: None,
                source_map_error: Some("HTTP 404".into()),
            },
        );

        assert!(matches!(
            fetched.effects.as_slice(),
            [Effect::BuildSourceView {
                source_map: None,
                ..
            }]
        ));
        assert!(fetched.state.diagnostics.iter().any(|diagnostic| {
            matches!(
                diagnostic,
                Diagnostic::SourceMapUnavailable {
                    script: candidate,
                    message,
                } if candidate == &script && message == "HTTP 404"
            )
        }));
    }

    fn configured_session() -> (Arc<DebuggerState>, SessionKey) {
        let connected = reduce(&Arc::new(DebuggerState::default()), Input::Connected).state;
        let attached = reduce(
            &connected,
            Input::SessionAttached {
                session_id: "s1".into(),
                target_id: "t1".into(),
                parent_session_id: None,
                waiting_for_debugger: false,
            },
        );
        let Effect::ConfigureSession { effect_id, session } = attached.effects[0].clone() else {
            panic!("expected configure");
        };
        let configured = reduce(&attached.state, Input::SessionConfigured { effect_id });
        (configured.state, session)
    }

    fn resolved_script() -> (Arc<DebuggerState>, SessionKey, ScriptKey, EffectId) {
        let (state, session) = configured_session();
        let parsed = reduce(
            &state,
            Input::ScriptParsed {
                session: session.clone(),
                script_id: "1".into(),
                url: "bundle.js".into(),
                hash: "hash".into(),
                source_map_url: Some("bundle.js.map".into()),
            },
        );
        let script = parsed.state.scripts.keys().next().unwrap().clone();
        let requested = reduce(
            &parsed.state,
            Input::RequestScriptSource {
                script: script.clone(),
            },
        );
        let Effect::FetchScriptSource {
            effect_id,
            script: effect_script,
            ..
        } = requested.effects[0].clone()
        else {
            panic!("expected fetch");
        };
        assert_eq!(effect_script, script);
        let fetched = reduce(
            &requested.state,
            Input::ScriptSourceFetched {
                effect_id,
                content: Arc::from("compiled"),
                source_map: Some(Arc::from([])),
                source_map_error: None,
            },
        );
        let Effect::BuildSourceView {
            effect_id: view_id, ..
        } = fetched.effects[0]
        else {
            panic!("expected view build");
        };
        let resolved = reduce(
            &fetched.state,
            Input::SourceViewBuilt {
                effect_id: view_id,
                logical_sources: BTreeMap::from([(
                    "src/app.ts".into(),
                    ContentCandidate {
                        content: crate::content_store::ContentStore::default().intern("source"),
                        provenance: crate::source_view::Provenance::Workspace {
                            logical_url: "src/app.ts".into(),
                        },
                    },
                )]),
            },
        );
        (resolved.state, session, script, view_id)
    }

    fn breakpoint_key() -> BreakpointKey {
        BreakpointKey {
            client_id: "client".into(),
            breakpoint_id: "bp".into(),
        }
    }
}
