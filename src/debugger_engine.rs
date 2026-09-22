use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};
use std::sync::Arc;

use im::{OrdMap, OrdSet};
use serde::{Deserialize, Serialize};

use crate::content_store::ContentHash;
use crate::source_graph::{RevisionNamespace, SourceRevision};
use crate::source_view::{ContentCandidate, Position, SourceMapData};

const MAX_DIAGNOSTICS: usize = 1024;
pub const MAX_BREAKPOINT_CANDIDATES: usize = 8;

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
    pub scopes: Arc<Vec<RawScope>>,
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
        source_map: Option<SourceMapData>,
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
    pub provenance: crate::service_api::ScriptProvenance,
    pub captured_source: Option<CapturedScriptSource>,
}

impl ScriptState {
    pub fn source_revision(&self, key: &ScriptKey) -> SourceRevision {
        if matches!(self.source, ScriptSourceState::Resolved(_))
            && let Some(source) = &self.captured_source
        {
            return SourceRevision::Content(source.content_hash);
        }
        SourceRevision::Version {
            namespace: RevisionNamespace::new("cdp-script").expect("static namespace"),
            value: if self.hash.is_empty() {
                format!("anonymous:{}:{}:{}:{}", key.session.connection_generation, key.session.session_id, key.script_id, self.version)
            } else {
                self.hash.clone()
            },
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CapturedScriptSource {
    pub content: Arc<str>,
    pub content_hash: ContentHash,
    pub source_map: Option<SourceMapData>,
    pub source_map_url: Option<String>,
    pub source_map_error: Option<String>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum BreakpointBinding {
    WaitingForRemoval(EffectId),
    PendingInstall(EffectId),
    Installed { backend_id: String },
    Failed { message: String },
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct BreakpointSourceCandidate {
    pub source_url: String,
    pub revision: SourceRevision,
    pub provenance: crate::source_view::Provenance,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct BreakpointMapping {
    pub generated_position: Position,
    pub quality: String,
    pub generated_url: String,
    pub projection: Vec<String>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum BreakpointAssessmentStatus {
    WaitingForScript,
    SourceNotFound {
        diagnostics: Arc<Vec<String>>,
    },
    AmbiguousSource {
        candidates: Arc<Vec<BreakpointSourceCandidate>>,
        omitted_candidate_count: usize,
    },
    Mapping {
        effect_id: EffectId,
        candidate: BreakpointSourceCandidate,
    },
    Unmapped {
        candidate: BreakpointSourceCandidate,
        diagnostics: Arc<Vec<String>>,
    },
    Applicable {
        candidate: BreakpointSourceCandidate,
        mappings: Arc<Vec<BreakpointMapping>>,
    },
    Failed {
        message: String,
    },
}

#[derive(Debug, PartialEq, Eq)]
pub struct BreakpointAssessment {
    pub script_version: u64,
    pub status: BreakpointAssessmentStatus,
}

#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord)]
struct BreakpointCandidateKey {
    source_url: String,
    revision: SourceRevision,
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct IndexedBreakpointCandidate {
    candidate: BreakpointSourceCandidate,
    representative_script: ScriptKey,
    scripts: OrdSet<ScriptKey>,
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
struct BreakpointCandidateBucket {
    keys: OrdSet<BreakpointCandidateKey>,
    bounded: OrdMap<BreakpointCandidateKey, IndexedBreakpointCandidate>,
    matching_scripts: OrdSet<ScriptKey>,
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct BreakpointCandidateIndex {
    exact: BreakpointCandidateBucket,
    friendly: BreakpointCandidateBucket,
    unresolved: OrdSet<ScriptKey>,
}

impl BreakpointCandidateBucket {
    fn insert(&mut self, script: &ScriptKey, candidate: BreakpointSourceCandidate) {
        let key = BreakpointCandidateKey {
            source_url: candidate.source_url.clone(),
            revision: candidate.revision.clone(),
        };
        self.matching_scripts.insert(script.clone());
        if self.keys.contains(&key) {
            if let Some(indexed) = self.bounded.get_mut(&key) {
                indexed.scripts.insert(script.clone());
                if script < &indexed.representative_script {
                    indexed.representative_script = script.clone();
                    indexed.candidate = candidate;
                }
            }
            return;
        }
        self.keys.insert(key.clone());
        let indexed = IndexedBreakpointCandidate {
            candidate,
            representative_script: script.clone(),
            scripts: OrdSet::unit(script.clone()),
        };
        if self.bounded.len() < MAX_BREAKPOINT_CANDIDATES {
            self.bounded.insert(key, indexed);
            return;
        }
        let Some(largest) = self.bounded.keys().next_back().cloned() else {
            return;
        };
        if key < largest {
            self.bounded.remove(&largest);
            self.bounded.insert(key, indexed);
        }
    }
}

#[derive(Clone, PartialEq, Eq)]
enum BreakpointCandidateSelection {
    None,
    Waiting {
        candidate_scripts: OrdSet<ScriptKey>,
        unresolved_scripts: OrdSet<ScriptKey>,
    },
    Unique {
        candidate: BreakpointSourceCandidate,
        scripts: OrdSet<ScriptKey>,
        friendly: bool,
    },
    Ambiguous {
        candidates: Arc<Vec<BreakpointSourceCandidate>>,
        omitted_candidate_count: usize,
        scripts: OrdSet<ScriptKey>,
    },
}

impl BreakpointCandidateSelection {
    fn special_scripts(&self) -> BTreeSet<ScriptKey> {
        match self {
            Self::None => BTreeSet::new(),
            Self::Waiting {
                candidate_scripts,
                unresolved_scripts,
            } => candidate_scripts
                .iter()
                .chain(unresolved_scripts.iter())
                .cloned()
                .collect(),
            Self::Unique { scripts, .. } | Self::Ambiguous { scripts, .. } => {
                scripts.iter().cloned().collect()
            }
        }
    }

    fn friendly_candidate_selected(&self) -> bool {
        matches!(self, Self::Unique { friendly: true, .. })
    }

    fn same_status_payload(&self, next: &Self) -> bool {
        match (self, next) {
            (Self::None, Self::None) | (Self::Waiting { .. }, Self::Waiting { .. }) => true,
            (
                Self::Unique {
                    candidate: left,
                    friendly: left_friendly,
                    ..
                },
                Self::Unique {
                    candidate: right,
                    friendly: right_friendly,
                    ..
                },
            ) => left == right && left_friendly == right_friendly,
            (
                Self::Ambiguous {
                    candidates: left,
                    omitted_candidate_count: left_omitted,
                    ..
                },
                Self::Ambiguous {
                    candidates: right,
                    omitted_candidate_count: right_omitted,
                    ..
                },
            ) => left == right && left_omitted == right_omitted,
            _ => false,
        }
    }

    fn changed_scripts(&self, next: &Self, completed: &ScriptKey) -> BTreeSet<ScriptKey> {
        let same_status_payload = self.same_status_payload(next);
        if same_status_payload {
            return BTreeSet::from([completed.clone()]);
        }
        let previous = self.special_scripts();
        let next = next.special_scripts();
        let mut changed: BTreeSet<ScriptKey> = previous.union(&next).cloned().collect();
        changed.insert(completed.clone());
        changed
    }
}

impl BreakpointCandidateIndex {
    fn has_exact_runtime_endpoint(&self) -> bool {
        self.exact.bounded.values().any(|candidate| {
            matches!(
                &candidate.candidate.provenance,
                crate::source_view::Provenance::RuntimeSource { .. }
            )
        })
    }

    fn selection(&self) -> BreakpointCandidateSelection {
        let (bucket, friendly) = if self.exact.keys.is_empty() {
            (&self.friendly, true)
        } else {
            (&self.exact, false)
        };
        if bucket.keys.len() > 1 {
            return BreakpointCandidateSelection::Ambiguous {
                candidates: Arc::new(
                    bucket
                        .bounded
                        .values()
                        .map(|candidate| candidate.candidate.clone())
                        .collect(),
                ),
                omitted_candidate_count: bucket.keys.len().saturating_sub(bucket.bounded.len()),
                scripts: bucket.matching_scripts.clone(),
            };
        }
        let Some(indexed) = bucket.bounded.values().next() else {
            return BreakpointCandidateSelection::None;
        };
        if friendly && !self.unresolved.is_empty() {
            BreakpointCandidateSelection::Waiting {
                candidate_scripts: indexed.scripts.clone(),
                unresolved_scripts: self.unresolved.clone(),
            }
        } else {
            BreakpointCandidateSelection::Unique {
                candidate: indexed.candidate.clone(),
                scripts: indexed.scripts.clone(),
                friendly,
            }
        }
    }
}

impl Clone for BreakpointAssessment {
    fn clone(&self) -> Self {
        #[cfg(test)]
        BREAKPOINT_ASSESSMENT_CLONE_COUNT.with(|count| count.set(count.get() + 1));
        Self {
            script_version: self.script_version,
            status: self.status.clone(),
        }
    }
}

#[cfg(test)]
thread_local! {
    static BREAKPOINT_ASSESSMENT_CLONE_COUNT: std::cell::Cell<usize> =
        const { std::cell::Cell::new(0) };
    static BREAKPOINT_RECONCILIATION_SCRIPT_VISITS: std::cell::Cell<usize> =
        const { std::cell::Cell::new(0) };
    static BREAKPOINT_INCREMENTAL_SCRIPT_VISITS: std::cell::Cell<usize> =
        const { std::cell::Cell::new(0) };
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct BreakpointState {
    pub generation: u64,
    pub source_url: String,
    pub position: Position,
    pub condition: Option<String>,
    pub friendly_candidate_selected: bool,
    pub candidate_index: Arc<BreakpointCandidateIndex>,
    pub pending_mappings: Arc<BTreeMap<ScriptKey, EffectId>>,
    pub assessments: Arc<OrdMap<ScriptKey, BreakpointAssessment>>,
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
    pub confirmed_position: Option<Position>,
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
    pub scripts: Arc<OrdMap<ScriptKey, Arc<ScriptState>>>,
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
            scripts: Arc::new(OrdMap::new()),
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
        source_url: String,
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
        script_hash: String,
        source_map_url: Option<String>,
        #[serde(default)]
        frame_id: Option<String>,
    },
    BuildSourceView {
        effect_id: EffectId,
        script: ScriptKey,
        script_version: u64,
        generated_url: String,
        content: Arc<str>,
        source_map: Option<SourceMapData>,
        #[serde(default)]
        source_map_url: Option<String>,
    },
    MapBreakpoint {
        effect_id: EffectId,
        breakpoint: BreakpointKey,
        script: ScriptKey,
        view_id: Option<EffectId>,
        source_url: String,
        source_revision: SourceRevision,
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

impl Effect {
    pub fn effect_id(&self) -> EffectId {
        match self {
            Self::ConfigureSession { effect_id, .. }
            | Self::RunIfWaitingForDebugger { effect_id, .. }
            | Self::FetchScriptSource { effect_id, .. }
            | Self::BuildSourceView { effect_id, .. }
            | Self::MapBreakpoint { effect_id, .. }
            | Self::InstallBreakpoint { effect_id, .. }
            | Self::RemoveBreakpoint { effect_id, .. }
            | Self::MapFrame { effect_id, .. }
            | Self::Resume { effect_id, .. }
            | Self::Step { effect_id, .. } => *effect_id,
        }
    }
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
    ConsoleMessageObserved,
    SessionAttached {
        session_id: String,
        target_id: String,
        parent_session_id: Option<String>,
        waiting_for_debugger: bool,
    },
    SessionConfigured {
        effect_id: EffectId,
    },
    ReleaseIfWaiting {
        session: SessionKey,
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
    ScriptParsedWithProvenance {
        session: SessionKey,
        script_id: String,
        url: String,
        hash: String,
        source_map_url: Option<String>,
        provenance: crate::service_api::ScriptProvenance,
    },
    RequestScriptSource {
        script: ScriptKey,
    },
    RefreshScriptSource {
        script: ScriptKey,
    },
    ScriptSourceFetched {
        effect_id: EffectId,
        content: Arc<str>,
        source_map: Option<SourceMapData>,
        #[serde(default)]
        source_map_url: Option<String>,
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
    BreakpointMappingAssessed {
        effect_id: EffectId,
        mappings: Vec<BreakpointMapping>,
    },
    BreakpointInstalled {
        effect_id: EffectId,
        backend_id: String,
        confirmed_position: Position,
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
    pub scopes: Vec<RawScope>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct RawScope {
    pub kind: String,
    pub name: Option<String>,
    pub object_id: String,
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
            state.scripts = Arc::new(OrdMap::new());
            state.physical_breakpoints = Arc::new(BTreeMap::new());
            state.pending = Arc::new(BTreeMap::new());
            let mut breakpoints = (*state.breakpoints).clone();
            for breakpoint in breakpoints.values_mut() {
                let breakpoint = Arc::make_mut(breakpoint);
                breakpoint.friendly_candidate_selected = false;
                breakpoint.candidate_index = Arc::new(BreakpointCandidateIndex::default());
                breakpoint.pending_mappings = Arc::new(BTreeMap::new());
                breakpoint.assessments = Arc::new(OrdMap::new());
                breakpoint.bindings = Arc::new(BTreeMap::new());
            }
            state.breakpoints = Arc::new(breakpoints);
        }
        Input::ConsoleMessageObserved => {}
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
            }
        }
        Input::ReleaseIfWaiting { session } => {
            let should_release = state.sessions.get(&session).is_some_and(|session| {
                session.waiting_for_debugger && matches!(session.phase, SessionPhase::Running)
            });
            if should_release {
                let effect_id = allocate_effect(
                    &mut state,
                    PendingEffect::RunIfWaiting {
                        session: session.clone(),
                    },
                );
                effects.push(Effect::RunIfWaitingForDebugger { effect_id, session });
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
        Input::ScriptParsedWithProvenance {
            session, script_id, url, hash, source_map_url, provenance,
        } => {
            let key = ScriptKey { session: session.clone(), script_id: script_id.clone() };
            let mut transition = reduce(previous, Input::ScriptParsed {
                session, script_id, url, hash, source_map_url,
            });
            if let Some(script) = Arc::make_mut(&mut Arc::make_mut(&mut transition.state).scripts).get_mut(&key) {
                Arc::make_mut(script).provenance = provenance.clone();
            }
            for effect in &mut transition.effects {
                if let Effect::FetchScriptSource { frame_id, .. } = effect {
                    *frame_id = provenance.frame_id.clone();
                }
            }
            return transition;
        }
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
            let script_was_reparsed = state.scripts.contains_key(&key);
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
                    provenance: Default::default(),
                    captured_source: None,
                }),
            );
            let breakpoint_keys = state.breakpoints.keys().cloned().collect::<Vec<_>>();
            for breakpoint in breakpoint_keys {
                if script_was_reparsed
                    || parsed_script_requires_full_reconciliation(&state, &breakpoint, &key)
                {
                    reconcile_breakpoint(&mut state, &breakpoint, &mut effects);
                } else {
                    let may_expose = script_may_expose_breakpoint(&state, &key, &breakpoint);
                    let status = assessment_without_candidate(
                        &state.scripts[&key],
                        &state.breakpoints[&breakpoint].source_url,
                    );
                    set_breakpoint_assessment(
                        &mut state,
                        &breakpoint,
                        key.clone(),
                        version,
                        status,
                    );
                    if may_expose
                        && !state.breakpoints[&breakpoint]
                            .candidate_index
                            .has_exact_runtime_endpoint()
                    {
                        Arc::make_mut(
                            &mut Arc::make_mut(&mut state.breakpoints)
                                .get_mut(&breakpoint)
                                .map(Arc::make_mut)
                                .unwrap()
                                .candidate_index,
                        )
                        .unresolved
                        .insert(key.clone());
                        schedule_source_hydration(&mut state, &key, false, &mut effects);
                    } else if !state.breakpoints[&breakpoint].bindings.is_empty() {
                        reconcile_physical_bindings(&mut state, &breakpoint, &mut effects);
                    }
                }
            }
            if script_has_frame_demand(&state, &key) {
                schedule_source_hydration(&mut state, &key, false, &mut effects);
            }
        }
        Input::RefreshScriptSource { script } => {
            let Some(current) = previous.scripts.get(&script) else {
                return reduce(previous, Input::RequestScriptSource { script });
            };
            let mut refreshed = reduce(previous, Input::ScriptParsedWithProvenance {
                session: script.session.clone(),
                script_id: script.script_id.clone(),
                url: current.url.clone(),
                hash: current.hash.clone(),
                source_map_url: current.source_map_url.clone(),
                provenance: current.provenance.clone(),
            });
            schedule_source_hydration(
                Arc::make_mut(&mut refreshed.state), &script, true, &mut refreshed.effects,
            );
            return refreshed;
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
            source_map_url,
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
            if let Some(message) = source_map_error.clone() {
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
            script_state.captured_source = Some(CapturedScriptSource {
                content_hash: ContentHash::of_bytes(content.as_bytes()),
                content: content.clone(),
                source_map: source_map.clone(),
                source_map_url: source_map_url.clone(),
                source_map_error,
            });
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
                source_map_url,
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
            reconcile_built_script(&mut state, &script, &mut effects);
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
                    friendly_candidate_selected: false,
                    candidate_index: Arc::new(BreakpointCandidateIndex::default()),
                    pending_mappings: Arc::new(BTreeMap::new()),
                    assessments: Arc::new(OrdMap::new()),
                    bindings: Arc::new(BTreeMap::new()),
                }),
            );
            let failed_scripts = state
                .scripts
                .iter()
                .filter_map(|(script, script_state)| {
                    (matches!(script_state.source, ScriptSourceState::Failed(_))
                        && script_may_expose_breakpoint(&state, script, &key))
                    .then_some(script.clone())
                })
                .collect::<Vec<_>>();
            for script in &failed_scripts {
                schedule_source_hydration(&mut state, script, true, &mut effects);
            }
            if failed_scripts.is_empty() {
                reconcile_breakpoint(&mut state, &key, &mut effects);
            } else {
                let breakpoint_keys = state.breakpoints.keys().cloned().collect::<Vec<_>>();
                for breakpoint in breakpoint_keys {
                    reconcile_breakpoint(&mut state, &breakpoint, &mut effects);
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
            let generated_url = state
                .pending
                .get(&effect_id)
                .and_then(|pending| match pending {
                    PendingEffect::MapBreakpoint { script, .. } => {
                        state.scripts.get(script).map(|script| script.url.clone())
                    }
                    _ => None,
                })
                .unwrap_or_default();
            let mappings = generated_positions
                .into_iter()
                .map(|generated_position| BreakpointMapping {
                    generated_position,
                    quality: "unknown".to_owned(),
                    generated_url: generated_url.clone(),
                    projection: vec!["unavailable".to_owned()],
                })
                .collect();
            complete_breakpoint_mapping(&mut state, effect_id, mappings, &mut effects);
        }
        Input::BreakpointMappingAssessed {
            effect_id,
            mappings,
        } => {
            complete_breakpoint_mapping(&mut state, effect_id, mappings, &mut effects);
        }
        Input::BreakpointInstalled {
            effect_id,
            backend_id,
            confirmed_position,
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
            Arc::make_mut(&mut state.physical_breakpoints)
                .get_mut(&physical)
                .map(Arc::make_mut)
                .unwrap()
                .confirmed_position = Some(confirmed_position);
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
            } else {
                for owner in owners.iter() {
                    reconcile_physical_bindings(&mut state, owner, &mut effects);
                }
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
                    let breakpoint_keys = state.breakpoints.keys().cloned().collect::<Vec<_>>();
                    for breakpoint in breakpoint_keys {
                        reconcile_breakpoint(&mut state, &breakpoint, &mut effects);
                    }
                }
                PendingEffect::FetchScriptSource { .. } | PendingEffect::BuildSourceView { .. } => {
                }
                PendingEffect::MapBreakpoint {
                    breakpoint,
                    breakpoint_generation,
                    script,
                    version,
                    ..
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
                        let breakpoint_state = Arc::make_mut(&mut state.breakpoints)
                            .get_mut(&breakpoint)
                            .map(Arc::make_mut)
                            .unwrap();
                        Arc::make_mut(&mut breakpoint_state.assessments).insert(
                            script,
                            BreakpointAssessment {
                                script_version: version,
                                status: BreakpointAssessmentStatus::Failed { message },
                            },
                        );
                        reconcile_physical_bindings(&mut state, &breakpoint, &mut effects);
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

fn complete_breakpoint_mapping(
    state: &mut DebuggerState,
    effect_id: EffectId,
    mappings: Vec<BreakpointMapping>,
    effects: &mut Vec<Effect>,
) {
    let Some(PendingEffect::MapBreakpoint {
        breakpoint,
        breakpoint_generation,
        script,
        version,
        ..
    }) = take_pending(state, effect_id)
    else {
        stale_effect(state, effect_id);
        return;
    };
    if state.scripts.get(&script).map(|value| value.version) != Some(version) {
        stale_effect(state, effect_id);
        return;
    }
    let Some(breakpoint_state) = state.breakpoints.get(&breakpoint).cloned() else {
        stale_effect(state, effect_id);
        return;
    };
    if breakpoint_state.generation != breakpoint_generation {
        stale_effect(state, effect_id);
        return;
    }
    let Some(assessment) = breakpoint_state.assessments.get(&script) else {
        stale_effect(state, effect_id);
        return;
    };
    let BreakpointAssessmentStatus::Mapping {
        effect_id: expected,
        candidate,
    } = &assessment.status
    else {
        stale_effect(state, effect_id);
        return;
    };
    if *expected != effect_id || assessment.script_version != version {
        stale_effect(state, effect_id);
        return;
    }
    let candidate = candidate.clone();
    let status = if mappings.is_empty() {
        BreakpointAssessmentStatus::Unmapped {
            candidate,
            diagnostics: Arc::new(vec![format!(
                "source matched script '{}' version {version}, but {}:{} has no reverse mapping",
                state.scripts[&script].url,
                breakpoint_state.position.line.saturating_add(1),
                breakpoint_state.position.column.saturating_add(1)
            )]),
        }
    } else {
        BreakpointAssessmentStatus::Applicable {
            candidate,
            mappings: Arc::new(mappings.clone()),
        }
    };
    let breakpoint_mut = Arc::make_mut(&mut state.breakpoints)
        .get_mut(&breakpoint)
        .map(Arc::make_mut)
        .unwrap();
    breakpoint_mut.pending_mappings =
        Arc::new(without_key(&breakpoint_state.pending_mappings, &script));
    Arc::make_mut(&mut breakpoint_mut.assessments).insert(
        script.clone(),
        BreakpointAssessment {
            script_version: version,
            status,
        },
    );

    reconcile_physical_bindings(state, &breakpoint, effects);
}

fn reconcile_breakpoint(
    state: &mut DebuggerState,
    breakpoint: &BreakpointKey,
    effects: &mut Vec<Effect>,
) {
    let Some(breakpoint_state) = state.breakpoints.get(breakpoint).cloned() else {
        return;
    };
    let scripts = state
        .scripts
        .iter()
        .map(|(key, script)| (key.clone(), script.clone()))
        .collect::<Vec<_>>();
    let mut candidate_index = BreakpointCandidateIndex::default();
    let mut hydrate = Vec::new();

    for (script_key, script) in &scripts {
        #[cfg(test)]
        BREAKPOINT_RECONCILIATION_SCRIPT_VISITS.with(|count| count.set(count.get() + 1));
        index_breakpoint_script(
            &mut candidate_index,
            &breakpoint_state.source_url,
            script_key,
            script,
        );
        if matches!(script.source, ScriptSourceState::Unresolved)
            && script_may_expose_source(script, &breakpoint_state.source_url)
        {
            hydrate.push(script_key.clone());
        }
    }

    let discover_sources = !candidate_index.has_exact_runtime_endpoint();
    let selection = candidate_index.selection();
    {
        let breakpoint = Arc::make_mut(&mut state.breakpoints)
            .get_mut(breakpoint)
            .map(Arc::make_mut)
            .unwrap();
        breakpoint.friendly_candidate_selected = selection.friendly_candidate_selected();
        breakpoint.candidate_index = Arc::new(candidate_index);
    }
    for (script_key, _) in &scripts {
        apply_breakpoint_selection_to_script(state, breakpoint, script_key, &selection, effects);
    }

    if discover_sources {
        for script in hydrate {
            schedule_source_hydration(state, &script, true, effects);
        }
    }
    reconcile_physical_bindings(state, breakpoint, effects);
}

fn reconcile_built_script(
    state: &mut DebuggerState,
    script: &ScriptKey,
    effects: &mut Vec<Effect>,
) {
    let Some(script_state) = state.scripts.get(script).cloned() else {
        return;
    };
    let breakpoint_keys = state.breakpoints.keys().cloned().collect::<Vec<_>>();
    for breakpoint_key in breakpoint_keys {
        let breakpoint_state = state.breakpoints[&breakpoint_key].clone();
        let previous_selection = breakpoint_state.candidate_index.selection();
        let mut candidate_index = (*breakpoint_state.candidate_index).clone();
        let completed_was_unresolved = candidate_index.unresolved.contains(script);
        candidate_index.unresolved.remove(script);
        index_breakpoint_script(
            &mut candidate_index,
            &breakpoint_state.source_url,
            script,
            &script_state,
        );
        let selection = candidate_index.selection();
        let selection_payload_changed = !previous_selection.same_status_payload(&selection);
        let affected = previous_selection.changed_scripts(&selection, script);
        let unresolved_remain = !candidate_index.unresolved.is_empty();
        {
            let breakpoint = Arc::make_mut(&mut state.breakpoints)
                .get_mut(&breakpoint_key)
                .map(Arc::make_mut)
                .unwrap();
            breakpoint.friendly_candidate_selected = selection.friendly_candidate_selected();
            breakpoint.candidate_index = Arc::new(candidate_index);
        }
        for affected_script in affected {
            #[cfg(test)]
            BREAKPOINT_INCREMENTAL_SCRIPT_VISITS.with(|count| count.set(count.get() + 1));
            apply_breakpoint_selection_to_script(
                state,
                &breakpoint_key,
                &affected_script,
                &selection,
                effects,
            );
        }
        let breakpoint_state = &state.breakpoints[&breakpoint_key];
        let final_pending_assessment_completed = completed_was_unresolved
            && !unresolved_remain
            && breakpoint_state.pending_mappings.is_empty();
        if !breakpoint_state.bindings.is_empty()
            && (selection_payload_changed || final_pending_assessment_completed)
        {
            reconcile_physical_bindings(state, &breakpoint_key, effects);
        }
    }
}

fn index_breakpoint_script(
    index: &mut BreakpointCandidateIndex,
    requested_source: &str,
    script_key: &ScriptKey,
    script: &ScriptState,
) {
    let endpoint = BreakpointSourceCandidate {
        source_url: script.url.clone(),
        revision: script.source_revision(script_key),
        provenance: crate::source_view::Provenance::RuntimeSource { url: script.url.clone() },
    };
    if source_urls_match(&script.url, requested_source) {
        index.exact.insert(script_key, endpoint);
    } else if friendly_source_matches(&script.url, requested_source) {
        index.friendly.insert(script_key, endpoint);
    }
    match &script.source {
        ScriptSourceState::Resolved(view) => {
            for (source_url, content) in view.logical_sources.iter() {
                if source_urls_match(source_url, &script.url) {
                    continue;
                }
                let candidate = BreakpointSourceCandidate {
                    source_url: source_url.clone(),
                    revision: SourceRevision::Content(content.content),
                    provenance: content.provenance.clone(),
                };
                if source_urls_match(source_url, requested_source) {
                    index.exact.insert(script_key, candidate);
                } else if friendly_source_matches(source_url, requested_source) {
                    index.friendly.insert(script_key, candidate);
                }
            }
        }
        ScriptSourceState::Unresolved
        | ScriptSourceState::Pending(_)
        | ScriptSourceState::Loaded { .. }
            if script_may_expose_source(script, requested_source) =>
        {
            index.unresolved.insert(script_key.clone());
        }
        ScriptSourceState::Unresolved
        | ScriptSourceState::Pending(_)
        | ScriptSourceState::Loaded { .. }
        | ScriptSourceState::Failed(_) => {}
    }
}

fn apply_breakpoint_selection_to_script(
    state: &mut DebuggerState,
    breakpoint: &BreakpointKey,
    script: &ScriptKey,
    selection: &BreakpointCandidateSelection,
    effects: &mut Vec<Effect>,
) {
    let Some(script_state) = state.scripts.get(script).cloned() else {
        cancel_breakpoint_mapping(state, breakpoint, script);
        return;
    };
    match selection {
        BreakpointCandidateSelection::Waiting {
            candidate_scripts,
            unresolved_scripts,
        } if candidate_scripts.contains(script) || unresolved_scripts.contains(script) => {
            replace_breakpoint_assessment(
                state,
                breakpoint,
                script.clone(),
                script_state.version,
                BreakpointAssessmentStatus::WaitingForScript,
            );
        }
        BreakpointCandidateSelection::Unique {
            candidate, scripts, ..
        } if scripts.contains(script) => {
            ensure_mapping(state, breakpoint, script, candidate.clone(), effects);
        }
        BreakpointCandidateSelection::Ambiguous {
            candidates,
            omitted_candidate_count,
            scripts,
        } if scripts.contains(script) => {
            replace_breakpoint_assessment(
                state,
                breakpoint,
                script.clone(),
                script_state.version,
                BreakpointAssessmentStatus::AmbiguousSource {
                    candidates: candidates.clone(),
                    omitted_candidate_count: *omitted_candidate_count,
                },
            );
        }
        BreakpointCandidateSelection::None
        | BreakpointCandidateSelection::Waiting { .. }
        | BreakpointCandidateSelection::Unique { .. }
        | BreakpointCandidateSelection::Ambiguous { .. } => {
            let requested_source = state.breakpoints[breakpoint].source_url.clone();
            replace_breakpoint_assessment(
                state,
                breakpoint,
                script.clone(),
                script_state.version,
                assessment_without_candidate(&script_state, &requested_source),
            );
        }
    }
}

fn parsed_script_requires_full_reconciliation(
    state: &DebuggerState,
    breakpoint: &BreakpointKey,
    script: &ScriptKey,
) -> bool {
    if source_urls_match(&state.scripts[script].url, &state.breakpoints[breakpoint].source_url) {
        return true;
    }
    if !script_may_expose_breakpoint(state, script, breakpoint) {
        return false;
    }
    state.breakpoints[breakpoint].friendly_candidate_selected
}

fn assessment_without_candidate(
    script: &ScriptState,
    requested_source: &str,
) -> BreakpointAssessmentStatus {
    match &script.source {
        ScriptSourceState::Unresolved
        | ScriptSourceState::Pending(_)
        | ScriptSourceState::Loaded { .. }
            if script.source_map_url.is_some()
                || source_urls_match(&script.url, requested_source) =>
        {
            BreakpointAssessmentStatus::WaitingForScript
        }
        ScriptSourceState::Unresolved
        | ScriptSourceState::Pending(_)
        | ScriptSourceState::Loaded { .. } => BreakpointAssessmentStatus::SourceNotFound {
            diagnostics: Arc::new(vec![format!(
                "script '{}' version {} cannot expose '{}' because its URL does not match and it has no source map",
                script.url, script.version, requested_source
            )]),
        },
        ScriptSourceState::Failed(message) => BreakpointAssessmentStatus::Failed {
            message: message.clone(),
        },
        ScriptSourceState::Resolved(view) => BreakpointAssessmentStatus::SourceNotFound {
            diagnostics: Arc::new(vec![format!(
                "script '{}' version {} exposes {} source(s), none matching '{}'",
                script.url,
                script.version,
                view.logical_sources.len(),
                requested_source
            )]),
        },
    }
}

fn set_breakpoint_assessment(
    state: &mut DebuggerState,
    breakpoint: &BreakpointKey,
    script: ScriptKey,
    script_version: u64,
    status: BreakpointAssessmentStatus,
) {
    let breakpoint = Arc::make_mut(&mut state.breakpoints)
        .get_mut(breakpoint)
        .map(Arc::make_mut)
        .unwrap();
    Arc::make_mut(&mut breakpoint.assessments).insert(
        script,
        BreakpointAssessment {
            script_version,
            status,
        },
    );
}

fn replace_breakpoint_assessment(
    state: &mut DebuggerState,
    breakpoint: &BreakpointKey,
    script: ScriptKey,
    script_version: u64,
    status: BreakpointAssessmentStatus,
) {
    cancel_breakpoint_mapping(state, breakpoint, &script);
    set_breakpoint_assessment(state, breakpoint, script, script_version, status);
}

fn ensure_mapping(
    state: &mut DebuggerState,
    breakpoint: &BreakpointKey,
    script: &ScriptKey,
    candidate: BreakpointSourceCandidate,
    effects: &mut Vec<Effect>,
) {
    let script_version = state.scripts[script].version;
    let current = state.breakpoints[breakpoint].assessments.get(script);
    let reusable = current.is_some_and(|assessment| {
        assessment.script_version == script_version
            && match &assessment.status {
                BreakpointAssessmentStatus::Mapping {
                    effect_id,
                    candidate: current,
                } => {
                    current == &candidate
                        && state.breakpoints[breakpoint].pending_mappings.get(script)
                            == Some(effect_id)
                        && state.pending.contains_key(effect_id)
                }
                BreakpointAssessmentStatus::Applicable {
                    candidate: current, ..
                }
                | BreakpointAssessmentStatus::Unmapped {
                    candidate: current, ..
                } => current == &candidate,
                _ => false,
            }
    });
    if reusable {
        return;
    }
    cancel_breakpoint_mapping(state, breakpoint, script);
    schedule_mapping(state, breakpoint, script, candidate, effects);
}

fn cancel_breakpoint_mapping(
    state: &mut DebuggerState,
    breakpoint: &BreakpointKey,
    script: &ScriptKey,
) {
    let Some(effect_id) = state
        .breakpoints
        .get(breakpoint)
        .and_then(|breakpoint| breakpoint.pending_mappings.get(script))
        .copied()
    else {
        return;
    };
    Arc::make_mut(&mut state.pending).remove(&effect_id);
    let breakpoint = Arc::make_mut(&mut state.breakpoints)
        .get_mut(breakpoint)
        .map(Arc::make_mut)
        .unwrap();
    Arc::make_mut(&mut breakpoint.pending_mappings).remove(script);
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
        script_hash: script_state.hash.clone(),
        source_map_url: script_state.source_map_url.clone(),
        frame_id: script_state.provenance.frame_id.clone(),
    });
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
    script_may_expose_source(script, &breakpoint.source_url)
}

fn script_may_expose_source(script: &ScriptState, source_url: &str) -> bool {
    source_urls_match(&script.url, source_url) || script.source_map_url.is_some()
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
    candidate: BreakpointSourceCandidate,
    effects: &mut Vec<Effect>,
) {
    let breakpoint_state = &state.breakpoints[breakpoint];
    if breakpoint_state.pending_mappings.contains_key(script) {
        return;
    }
    let breakpoint_generation = breakpoint_state.generation;
    let breakpoint_position = breakpoint_state.position;
    let script_state = &state.scripts[script];
    let view_id = match &script_state.source {
        ScriptSourceState::Resolved(view) => Some(view.view_id),
        _ => None,
    };
    let script_version = script_state.version;
    let source_url = candidate.source_url.clone();
    let source_revision = candidate.revision.clone();
    let effect_id = allocate_effect(
        state,
        PendingEffect::MapBreakpoint {
            breakpoint: breakpoint.clone(),
            breakpoint_generation,
            script: script.clone(),
            version: script_version,
            source_url: source_url.clone(),
        },
    );
    let breakpoint_mut = Arc::make_mut(&mut state.breakpoints)
        .get_mut(breakpoint)
        .map(Arc::make_mut)
        .unwrap();
    Arc::make_mut(&mut breakpoint_mut.pending_mappings).insert(script.clone(), effect_id);
    Arc::make_mut(&mut breakpoint_mut.assessments).insert(
        script.clone(),
        BreakpointAssessment {
            script_version,
            status: BreakpointAssessmentStatus::Mapping {
                effect_id,
                candidate,
            },
        },
    );
    effects.push(Effect::MapBreakpoint {
        effect_id,
        breakpoint: breakpoint.clone(),
        script: script.clone(),
        view_id,
        source_url,
        source_revision,
        position: breakpoint_position,
    });
}

fn source_urls_match(left: &str, right: &str) -> bool {
    left == right
        || comparable_file_path(left)
            .zip(comparable_file_path(right))
            .is_some_and(|(left, right)| left == right)
}

fn friendly_source_matches(candidate: &str, requested: &str) -> bool {
    let candidate = friendly_source_path(candidate);
    let requested = friendly_source_path(requested);
    if candidate.is_empty() || requested.is_empty() {
        return false;
    }
    candidate == requested
        || candidate.ends_with(&format!("/{requested}"))
        || requested.ends_with(&format!("/{candidate}"))
}

fn friendly_source_path(value: &str) -> String {
    let path = url::Url::parse(value)
        .ok()
        .map(|url| {
            percent_encoding::percent_decode_str(url.path())
                .decode_utf8_lossy()
                .into_owned()
        })
        .unwrap_or_else(|| value.to_owned());
    path.replace('\\', "/")
        .trim_start_matches("./")
        .trim_start_matches('/')
        .to_owned()
}

fn comparable_file_path(value: &str) -> Option<String> {
    let path = if Path::new(value).is_absolute() {
        PathBuf::from(value)
    } else {
        let url = url::Url::parse(value).ok()?;
        if url.scheme() != "file" {
            return None;
        }
        url.to_file_path().ok()?
    };
    let normalized = path.to_string_lossy().replace('\\', "/");
    #[cfg(windows)]
    let normalized = normalized.to_ascii_lowercase();
    Some(normalized)
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
            confirmed_position: None,
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

fn reconcile_physical_bindings(
    state: &mut DebuggerState,
    breakpoint: &BreakpointKey,
    effects: &mut Vec<Effect>,
) {
    let Some(breakpoint_state) = state.breakpoints.get(breakpoint).cloned() else {
        return;
    };
    let mut desired = BTreeSet::new();
    let mut assessment_pending = false;
    for (script, assessment) in breakpoint_state.assessments.iter() {
        match &assessment.status {
            BreakpointAssessmentStatus::Applicable { mappings, .. } => {
                if state.scripts.get(script).map(|value| value.version)
                    != Some(assessment.script_version)
                {
                    continue;
                }
                for mapping in mappings.iter() {
                    desired.insert(PhysicalBreakpointKey {
                        script: script.clone(),
                        script_version: assessment.script_version,
                        position: mapping.generated_position,
                        condition: breakpoint_state.condition.clone(),
                    });
                }
            }
            BreakpointAssessmentStatus::WaitingForScript
            | BreakpointAssessmentStatus::Mapping { .. } => assessment_pending = true,
            BreakpointAssessmentStatus::SourceNotFound { .. }
            | BreakpointAssessmentStatus::AmbiguousSource { .. }
            | BreakpointAssessmentStatus::Unmapped { .. }
            | BreakpointAssessmentStatus::Failed { .. } => {}
        }
    }

    for physical in desired.iter() {
        if !breakpoint_state.bindings.contains_key(physical) {
            bind_physical(
                state,
                breakpoint,
                &physical.script,
                physical.position,
                effects,
            );
        }
    }

    if assessment_pending
        || desired.iter().any(|physical| {
            !matches!(
                state
                    .physical_breakpoints
                    .get(physical)
                    .map(|physical| &physical.status),
                Some(PhysicalBreakpointStatus::Installed { .. })
            )
        })
    {
        return;
    }

    let stale = state.breakpoints[breakpoint]
        .bindings
        .keys()
        .filter(|physical| !desired.contains(*physical))
        .cloned()
        .collect::<Vec<_>>();
    for physical in stale {
        release_physical_binding(state, breakpoint, &physical, effects);
    }
}

fn release_physical_binding(
    state: &mut DebuggerState,
    breakpoint: &BreakpointKey,
    physical: &PhysicalBreakpointKey,
    effects: &mut Vec<Effect>,
) {
    let Some(breakpoint_state) = state.breakpoints.get(breakpoint).cloned() else {
        return;
    };
    if !breakpoint_state.bindings.contains_key(physical) {
        return;
    }
    Arc::make_mut(&mut state.breakpoints)
        .get_mut(breakpoint)
        .map(Arc::make_mut)
        .unwrap()
        .bindings = Arc::new(without_key(&breakpoint_state.bindings, physical));

    let Some(physical_state) = state.physical_breakpoints.get(physical).cloned() else {
        return;
    };
    let owners = Arc::new(with_set_remove(&physical_state.owners, breakpoint));
    Arc::make_mut(&mut state.physical_breakpoints)
        .get_mut(physical)
        .map(Arc::make_mut)
        .unwrap()
        .owners = owners.clone();
    if !owners.is_empty() {
        return;
    }
    match &physical_state.status {
        PhysicalBreakpointStatus::Installed { backend_id } => {
            schedule_physical_removal(state, physical, backend_id.clone(), effects);
        }
        PhysicalBreakpointStatus::Failed { .. } => {
            Arc::make_mut(&mut state.physical_breakpoints).remove(physical);
        }
        PhysicalBreakpointStatus::Installing(_) | PhysicalBreakpointStatus::Removing(_) => {}
    }
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
            Arc::make_mut(&mut breakpoint.assessments).remove(script);
        }
        return;
    }

    let physical_key_set: BTreeSet<_> = physical_keys.iter().cloned().collect();
    for breakpoint in Arc::make_mut(&mut state.breakpoints).values_mut() {
        let breakpoint = Arc::make_mut(breakpoint);
        breakpoint.pending_mappings = Arc::new(without_key(&breakpoint.pending_mappings, script));
        Arc::make_mut(&mut breakpoint.assessments).remove(script);
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
            scopes: Arc::new(frame.scopes),
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
    let scripts = Arc::make_mut(&mut state.scripts);
    for script in &scripts_to_remove {
        scripts.remove(script);
    }
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
        breakpoint.assessments = Arc::new(
            breakpoint
                .assessments
                .iter()
                .filter(|(script, _)| !scripts_to_remove.contains(*script))
                .map(|(key, value)| (key.clone(), value.clone()))
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
    let breakpoint_keys = state.breakpoints.keys().cloned().collect::<Vec<_>>();
    for breakpoint_key in breakpoint_keys {
        let source_url = state.breakpoints[&breakpoint_key].source_url.clone();
        let mut candidate_index = BreakpointCandidateIndex::default();
        for (script_key, script) in state.scripts.iter() {
            index_breakpoint_script(&mut candidate_index, &source_url, script_key, script);
        }
        let selection = candidate_index.selection();
        let breakpoint = Arc::make_mut(&mut state.breakpoints)
            .get_mut(&breakpoint_key)
            .map(Arc::make_mut)
            .unwrap();
        breakpoint.friendly_candidate_selected = selection.friendly_candidate_selected();
        breakpoint.candidate_index = Arc::new(candidate_index);
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

fn with_set_remove<T: Ord + Clone>(set: &BTreeSet<T>, value: &T) -> BTreeSet<T> {
    let mut result = set.clone();
    result.remove(value);
    result
}

#[cfg(test)]
mod tests {
    use super::*;

    #[cfg(windows)]
    #[test]
    fn matches_windows_paths_with_node_file_urls() {
        assert!(source_urls_match(
            "file:///D:/workspace/my%20app.js",
            r"d:\workspace\my app.js",
        ));
        assert!(!source_urls_match(
            "file:///D:/workspace/app.js",
            r"d:\workspace\other.js",
        ));
    }

    #[test]
    fn waiting_session_runs_only_after_explicit_release() {
        let connected = reduce(&Arc::new(DebuggerState::default()), Input::Connected);
        let attached = reduce(
            &connected.state,
            Input::SessionAttached {
                session_id: "session".into(),
                target_id: "target".into(),
                parent_session_id: None,
                waiting_for_debugger: true,
            },
        );
        let Effect::ConfigureSession { effect_id, session } = attached.effects[0].clone() else {
            panic!("expected session configuration");
        };

        let configured = reduce(&attached.state, Input::SessionConfigured { effect_id });
        assert!(configured.effects.is_empty());
        assert!(
            configured
                .state
                .sessions
                .get(&session)
                .is_some_and(|session| session.waiting_for_debugger)
        );

        let released = reduce(
            &configured.state,
            Input::ReleaseIfWaiting {
                session: session.clone(),
            },
        );
        let Effect::RunIfWaitingForDebugger {
            effect_id: release_effect,
            session: released_session,
        } = &released.effects[0]
        else {
            panic!("expected waiting session release");
        };
        assert_eq!(released_session, &session);

        let completed = reduce(
            &released.state,
            Input::CommandAccepted {
                effect_id: *release_effect,
            },
        );
        assert!(
            completed
                .state
                .sessions
                .get(&session)
                .is_some_and(|session| !session.waiting_for_debugger)
        );
        assert!(
            reduce(
                &completed.state,
                Input::ReleaseIfWaiting {
                    session: session.clone(),
                },
            )
            .effects
            .is_empty()
        );
    }

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
    fn script_provenance_selects_owning_frame_for_source_map_fetches() {
        let (state, session) = configured_session();
        let parsed = reduce(&state, Input::ScriptParsedWithProvenance {
            session, script_id: "child-script".into(), url: "https://example.test/app.js".into(),
            hash: "captured-hash".into(), source_map_url: Some("app.js.map".into()),
            provenance: crate::service_api::ScriptProvenance {
                execution_context_id: Some(23),
                execution_context_aux_data: Some(serde_json::json!({"frameId": "child-frame"})),
                frame_id: Some("child-frame".into()),
            },
        });
        let script = parsed.state.scripts.keys().next().unwrap().clone();
        let requested = reduce(&parsed.state, Input::RequestScriptSource { script: script.clone() });
        let [Effect::FetchScriptSource { effect_id, frame_id, .. }] = requested.effects.as_slice()
            else { panic!("missing source fetch") };
        assert_eq!(frame_id.as_deref(), Some("child-frame"));
        let fetched = reduce(&requested.state, Input::ScriptSourceFetched {
            effect_id: *effect_id, content: Arc::from("class a {}"),
            source_map: Some(SourceMapData::new(b"{\"version\":3}".as_slice())),
            source_map_url: Some("https://example.test/app.js.map".into()), source_map_error: None,
        });
        let [Effect::BuildSourceView { effect_id, .. }] = fetched.effects.as_slice()
            else { panic!("missing source view build") };
        let resolved = reduce(&fetched.state, Input::SourceViewBuilt { effect_id: *effect_id, logical_sources: BTreeMap::new() });
        let captured = resolved.state.scripts[&script].captured_source.as_ref().unwrap();
        assert_eq!(captured.content.as_ref(), "class a {}");
        assert!(captured.source_map.is_some());
        assert_eq!(resolved.state.scripts[&script].provenance.execution_context_id, Some(23));
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
                        scopes: vec![],
                    },
                    RawFrame {
                        call_frame_id: "frame-2".into(),
                        function_name: "second".into(),
                        script_id: "1".into(),
                        position: Position { line: 1, column: 2 },
                        scopes: vec![],
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
                source_map: Some(SourceMapData::new([])),
                source_map_url: Some("file:///bundle.js.map".into()),
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
                    scopes: vec![],
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
                source_map_url: None,
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
                    scopes: vec![],
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
                source_map_url: None,
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
                source_map_url: None,
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
            assert_eq!(effect_view, Some(view_id));
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
                confirmed_position: state.physical_breakpoints.keys().next().unwrap().position,
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
    fn script_and_source_reconciliation_preserve_unchanged_installed_binding() {
        let (state, session, script, _) = resolved_script();
        let key = breakpoint_key();
        let mapped = reduce(
            &state,
            Input::SetBreakpoint {
                key: key.clone(),
                source_url: "src/app.ts".into(),
                position: Position::ZERO,
                condition: None,
            },
        );
        let mapping = mapped.effects[0].effect_id();
        let installing = reduce(
            &mapped.state,
            Input::BreakpointMapped {
                effect_id: mapping,
                generated_positions: vec![Position {
                    line: 10,
                    column: 2,
                }],
            },
        );
        let install = installing.effects[0].effect_id();
        let installed = reduce(
            &installing.state,
            Input::BreakpointInstalled {
                effect_id: install,
                backend_id: "backend-stable".into(),
                confirmed_position: Position {
                    line: 10,
                    column: 2,
                },
            },
        );
        let original_assessment = installed.state.breakpoints[&key].assessments[&script].clone();
        let original_physical = installed.state.breakpoints[&key]
            .bindings
            .keys()
            .next()
            .unwrap()
            .clone();
        let second_script = ScriptKey {
            session: session.clone(),
            script_id: "2".into(),
        };

        let parsed = reduce(
            &installed.state,
            Input::ScriptParsed {
                session: session.clone(),
                script_id: "2".into(),
                url: "second.js".into(),
                hash: "second".into(),
                source_map_url: Some("second.js.map".into()),
            },
        );
        assert!(matches!(
            parsed.effects.as_slice(),
            [Effect::FetchScriptSource { .. }]
        ));
        assert_eq!(
            parsed.state.breakpoints[&key].assessments[&script],
            original_assessment
        );
        assert!(matches!(
            parsed.state.physical_breakpoints[&original_physical].status,
            PhysicalBreakpointStatus::Installed { ref backend_id }
                if backend_id == "backend-stable"
        ));

        let fetch = parsed.effects[0].effect_id();
        let fetched = reduce(
            &parsed.state,
            Input::ScriptSourceFetched {
                effect_id: fetch,
                content: Arc::from("compiled second"),
                source_map: Some(SourceMapData::new([])),
                source_map_url: Some("second.js.map".into()),
                source_map_error: None,
            },
        );
        let view = fetched.effects[0].effect_id();
        let built = reduce(
            &fetched.state,
            Input::SourceViewBuilt {
                effect_id: view,
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
        assert!(matches!(
            built.effects.as_slice(),
            [Effect::MapBreakpoint {
                script: mapped_script,
                ..
            }] if mapped_script == &second_script
        ));
        assert_eq!(
            built.state.breakpoints[&key].assessments[&script],
            original_assessment
        );
        assert!(matches!(
            built.state.physical_breakpoints[&original_physical].status,
            PhysicalBreakpointStatus::Installed { ref backend_id }
                if backend_id == "backend-stable"
        ));

        let second_mapping = built.effects[0].effect_id();
        let raced = reduce(
            &built.state,
            Input::ScriptParsed {
                session,
                script_id: "3".into(),
                url: "third.js".into(),
                hash: "third".into(),
                source_map_url: Some("third.js.map".into()),
            },
        );
        assert!(matches!(
            raced.effects.as_slice(),
            [Effect::FetchScriptSource { .. }]
        ));
        assert!(raced.state.pending.contains_key(&second_mapping));
        assert!(matches!(
            raced.state.breakpoints[&key].assessments[&second_script].status,
            BreakpointAssessmentStatus::Mapping { effect_id, .. }
                if effect_id == second_mapping
        ));
        let second_mapped = reduce(
            &raced.state,
            Input::BreakpointMapped {
                effect_id: second_mapping,
                generated_positions: vec![Position {
                    line: 30,
                    column: 6,
                }],
            },
        );
        assert!(matches!(
            second_mapped.effects.as_slice(),
            [Effect::InstallBreakpoint { .. }]
        ));
        assert!(matches!(
            second_mapped.state.physical_breakpoints[&original_physical].status,
            PhysicalBreakpointStatus::Installed { ref backend_id }
                if backend_id == "backend-stable"
        ));
    }

    #[test]
    fn changed_mapping_installs_before_removing_previous_location() {
        let (state, session) = configured_session();
        let first_script = ScriptKey {
            session: session.clone(),
            script_id: "one".into(),
        };
        let first = resolved_script_with_source(
            &state,
            &session,
            "one",
            "one.js",
            "webpack:///one/app.ts",
            "first",
        );
        let key = breakpoint_key();
        let mapping = reduce(
            &first,
            Input::SetBreakpoint {
                key: key.clone(),
                source_url: "app.ts".into(),
                position: Position::ZERO,
                condition: None,
            },
        );
        let mapped = reduce(
            &mapping.state,
            Input::BreakpointMapped {
                effect_id: mapping.effects[0].effect_id(),
                generated_positions: vec![Position { line: 1, column: 1 }],
            },
        );
        let installed = reduce(
            &mapped.state,
            Input::BreakpointInstalled {
                effect_id: mapped.effects[0].effect_id(),
                backend_id: "backend-old".into(),
                confirmed_position: Position { line: 1, column: 1 },
            },
        );
        let old_physical = installed.state.breakpoints[&key]
            .bindings
            .keys()
            .next()
            .unwrap()
            .clone();

        let parsed = reduce(
            &installed.state,
            Input::ScriptParsed {
                session: session.clone(),
                script_id: "two".into(),
                url: "two.js".into(),
                hash: "two".into(),
                source_map_url: Some("two.js.map".into()),
            },
        );
        assert!(parsed.effects.iter().all(|effect| {
            !matches!(
                effect,
                Effect::InstallBreakpoint { .. } | Effect::RemoveBreakpoint { .. }
            )
        }));
        assert!(matches!(
            parsed.state.physical_breakpoints[&old_physical].status,
            PhysicalBreakpointStatus::Installed { ref backend_id }
                if backend_id == "backend-old"
        ));

        let fetched = reduce(
            &parsed.state,
            Input::ScriptSourceFetched {
                effect_id: parsed.effects[0].effect_id(),
                content: Arc::from("compiled two"),
                source_map: Some(SourceMapData::new([])),
                source_map_url: Some("two.js.map".into()),
                source_map_error: None,
            },
        );
        let built = reduce(
            &fetched.state,
            Input::SourceViewBuilt {
                effect_id: fetched.effects[0].effect_id(),
                logical_sources: BTreeMap::from([(
                    "app.ts".into(),
                    ContentCandidate {
                        content: crate::content_store::ContentStore::default().intern("second"),
                        provenance: crate::source_view::Provenance::Workspace {
                            logical_url: "app.ts".into(),
                        },
                    },
                )]),
            },
        );
        let new_mapping = built
            .effects
            .iter()
            .find_map(|effect| match effect {
                Effect::MapBreakpoint {
                    effect_id, script, ..
                } if script != &first_script => Some(*effect_id),
                _ => None,
            })
            .expect("new exact candidate should be mapped");
        assert!(
            built
                .effects
                .iter()
                .all(|effect| !matches!(effect, Effect::RemoveBreakpoint { .. }))
        );
        assert!(matches!(
            built.state.physical_breakpoints[&old_physical].status,
            PhysicalBreakpointStatus::Installed { .. }
        ));

        let remapped = reduce(
            &built.state,
            Input::BreakpointMapped {
                effect_id: new_mapping,
                generated_positions: vec![Position {
                    line: 20,
                    column: 4,
                }],
            },
        );
        let Effect::InstallBreakpoint {
            effect_id: new_install,
            physical: ref new_physical,
        } = remapped.effects[0]
        else {
            panic!("changed location should be installed first");
        };
        assert_eq!(remapped.effects.len(), 1);
        assert!(
            remapped.state.breakpoints[&key]
                .bindings
                .contains_key(&old_physical)
        );
        assert!(matches!(
            remapped.state.physical_breakpoints[&old_physical].status,
            PhysicalBreakpointStatus::Installed { ref backend_id }
                if backend_id == "backend-old"
        ));

        let new_physical = new_physical.clone();
        let new_installed = reduce(
            &remapped.state,
            Input::BreakpointInstalled {
                effect_id: new_install,
                backend_id: "backend-new".into(),
                confirmed_position: new_physical.position,
            },
        );
        assert!(matches!(
            new_installed.effects.as_slice(),
            [Effect::RemoveBreakpoint {
                physical,
                backend_id,
                ..
            }] if physical == &old_physical && backend_id == "backend-old"
        ));
        assert!(
            !new_installed.state.breakpoints[&key]
                .bindings
                .contains_key(&old_physical)
        );
        assert!(matches!(
            new_installed.state.breakpoints[&key].bindings[&new_physical],
            BreakpointBinding::Installed { ref backend_id } if backend_id == "backend-new"
        ));
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
                    scopes: vec![],
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
    fn reparsed_script_rejects_late_mapping_assessment() {
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
            effect_id: old_mapping,
            ..
        } = mapping.effects[0]
        else {
            panic!("expected mapping");
        };

        let reparsed = reduce(
            &mapping.state,
            Input::ScriptParsed {
                session,
                script_id: script.script_id.clone(),
                url: "bundle.js".into(),
                hash: "new".into(),
                source_map_url: Some("bundle.js.map".into()),
            },
        );
        assert_eq!(
            reparsed.state.breakpoints[&key].assessments[&script].script_version,
            2
        );
        assert!(matches!(
            reparsed.state.breakpoints[&key].assessments[&script].status,
            BreakpointAssessmentStatus::WaitingForScript
        ));

        let late = reduce(
            &reparsed.state,
            Input::BreakpointMappingAssessed {
                effect_id: old_mapping,
                mappings: vec![BreakpointMapping {
                    generated_position: Position { line: 9, column: 9 },
                    quality: "exact".into(),
                    generated_url: "bundle.js".into(),
                    projection: vec!["source map".into()],
                }],
            },
        );
        assert!(late.state.breakpoints[&key].bindings.is_empty());
        assert_eq!(
            late.state.breakpoints[&key].assessments[&script].script_version,
            2
        );
        assert!(matches!(
            late.state.diagnostics.last(),
            Some(Diagnostic::IgnoredStaleEffect {
                effect_id
            }) if *effect_id == old_mapping
        ));
    }

    #[test]
    fn ambiguous_source_candidates_are_bounded_without_mapping() {
        let (state, session) = configured_session();
        let key = breakpoint_key();
        let intent = reduce(
            &state,
            Input::SetBreakpoint {
                key: key.clone(),
                source_url: "app.ts".into(),
                position: Position::ZERO,
                condition: None,
            },
        );
        let parsed = reduce(
            &intent.state,
            Input::ScriptParsed {
                session,
                script_id: "1".into(),
                url: "bundle.js".into(),
                hash: "hash".into(),
                source_map_url: Some("bundle.js.map".into()),
            },
        );
        let Effect::FetchScriptSource {
            effect_id: fetch, ..
        } = parsed.effects[0]
        else {
            panic!("breakpoint demand should hydrate the script");
        };
        let fetched = reduce(
            &parsed.state,
            Input::ScriptSourceFetched {
                effect_id: fetch,
                content: Arc::from("compiled"),
                source_map: Some(SourceMapData::new([])),
                source_map_url: Some("bundle.js.map".into()),
                source_map_error: None,
            },
        );
        let Effect::BuildSourceView {
            effect_id: view, ..
        } = fetched.effects[0]
        else {
            panic!("expected source view");
        };
        let store = crate::content_store::ContentStore::default();
        let sources = (0..MAX_BREAKPOINT_CANDIDATES + 3)
            .map(|index| {
                let source_url = format!("webpack:///candidate-{index}/app.ts");
                (
                    source_url.clone(),
                    ContentCandidate {
                        content: store.intern(&format!("source {index}")),
                        provenance: crate::source_view::Provenance::Workspace {
                            logical_url: source_url,
                        },
                    },
                )
            })
            .collect();
        let built = reduce(
            &fetched.state,
            Input::SourceViewBuilt {
                effect_id: view,
                logical_sources: sources,
            },
        );
        assert!(built.effects.is_empty());
        let assessment = built.state.breakpoints[&key]
            .assessments
            .values()
            .next()
            .unwrap();
        let BreakpointAssessmentStatus::AmbiguousSource {
            candidates,
            omitted_candidate_count,
        } = &assessment.status
        else {
            panic!("friendly source must remain ambiguous");
        };
        assert_eq!(candidates.len(), MAX_BREAKPOINT_CANDIDATES);
        assert_eq!(*omitted_candidate_count, 3);
        assert!(built.state.physical_breakpoints.is_empty());
    }

    #[test]
    fn failed_replacement_mapping_removes_obsolete_fallback_binding() {
        let (state, session) = configured_session();
        let first = resolved_script_with_source(
            &state,
            &session,
            "one",
            "one.js",
            "webpack:///one/app.ts",
            "first",
        );
        let key = breakpoint_key();
        let mapping = reduce(
            &first,
            Input::SetBreakpoint {
                key: key.clone(),
                source_url: "app.ts".into(),
                position: Position::ZERO,
                condition: None,
            },
        );
        let mapped = reduce(
            &mapping.state,
            Input::BreakpointMapped {
                effect_id: mapping.effects[0].effect_id(),
                generated_positions: vec![Position { line: 1, column: 1 }],
            },
        );
        let installed = reduce(
            &mapped.state,
            Input::BreakpointInstalled {
                effect_id: mapped.effects[0].effect_id(),
                backend_id: "backend-old".into(),
                confirmed_position: Position { line: 1, column: 1 },
            },
        );
        let old_physical = installed.state.breakpoints[&key]
            .bindings
            .keys()
            .next()
            .unwrap()
            .clone();

        let replacement = resolved_script_with_source(
            &installed.state,
            &session,
            "two",
            "two.js",
            "app.ts",
            "second",
        );
        let replacement_script = ScriptKey {
            session,
            script_id: "two".into(),
        };
        let BreakpointAssessmentStatus::Mapping {
            effect_id: replacement_mapping,
            ..
        } = replacement.breakpoints[&key].assessments[&replacement_script].status
        else {
            panic!("exact replacement candidate should be mapping");
        };
        assert!(matches!(
            replacement.breakpoints[&key].bindings[&old_physical],
            BreakpointBinding::Installed { ref backend_id } if backend_id == "backend-old"
        ));

        let failed = reduce(
            &replacement,
            Input::EffectFailed {
                effect_id: replacement_mapping,
                message: "replacement mapping failed".into(),
            },
        );
        assert!(matches!(
            failed.effects.as_slice(),
            [Effect::RemoveBreakpoint {
                physical,
                backend_id,
                ..
            }] if physical == &old_physical && backend_id == "backend-old"
        ));
        assert!(
            !failed.state.breakpoints[&key]
                .bindings
                .contains_key(&old_physical)
        );
        assert!(matches!(
            failed.state.breakpoints[&key].assessments[&replacement_script].status,
            BreakpointAssessmentStatus::Failed { ref message }
                if message == "replacement mapping failed"
        ));

        let stale_failure = reduce(
            &failed.state,
            Input::EffectFailed {
                effect_id: replacement_mapping,
                message: "late failure".into(),
            },
        );
        assert!(stale_failure.effects.is_empty());
        assert!(matches!(
            stale_failure.state.diagnostics.last(),
            Some(Diagnostic::IgnoredStaleEffect { effect_id })
                if *effect_id == replacement_mapping
        ));
        assert!(matches!(
            stale_failure.state.breakpoints[&key].assessments[&replacement_script].status,
            BreakpointAssessmentStatus::Failed { ref message }
                if message == "replacement mapping failed"
        ));
    }

    #[test]
    fn competing_candidate_invalidates_inflight_physical_install() {
        let (state, session) = configured_session();
        let first = resolved_script_with_source(
            &state,
            &session,
            "one",
            "one.js",
            "webpack:///one/app.ts",
            "first",
        );
        let key = breakpoint_key();
        let intent = reduce(
            &first,
            Input::SetBreakpoint {
                key: key.clone(),
                source_url: "app.ts".into(),
                position: Position::ZERO,
                condition: None,
            },
        );
        let Effect::MapBreakpoint {
            effect_id: mapping, ..
        } = intent.effects[0]
        else {
            panic!("first candidate should map");
        };
        let mapped = reduce(
            &intent.state,
            Input::BreakpointMapped {
                effect_id: mapping,
                generated_positions: vec![Position::ZERO],
            },
        );
        let Effect::InstallBreakpoint {
            effect_id: install, ..
        } = mapped.effects[0]
        else {
            panic!("mapping should start installation");
        };

        let second = resolved_script_with_source(
            &mapped.state,
            &session,
            "two",
            "two.js",
            "webpack:///two/app.ts",
            "second",
        );
        assert!(matches!(
            second.breakpoints[&key]
                .assessments
                .values()
                .find_map(|assessment| match &assessment.status {
                    BreakpointAssessmentStatus::AmbiguousSource { .. } => Some(()),
                    _ => None,
                }),
            Some(())
        ));
        assert!(second.breakpoints[&key].bindings.is_empty());

        let late = reduce(
            &second,
            Input::BreakpointInstalled {
                effect_id: install,
                backend_id: "stale-backend".into(),
                confirmed_position: Position::ZERO,
            },
        );
        assert!(matches!(
            late.effects.as_slice(),
            [Effect::RemoveBreakpoint { .. }]
        ));
        assert!(late.state.breakpoints[&key].bindings.is_empty());
        assert!(
            late.state.breakpoints[&key]
                .assessments
                .values()
                .all(|assessment| matches!(
                    assessment.status,
                    BreakpointAssessmentStatus::AmbiguousSource { .. }
                        | BreakpointAssessmentStatus::SourceNotFound { .. }
                ))
        );
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
                    scopes: vec![],
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
                confirmed_position: Position::ZERO,
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
                confirmed_position: Position::ZERO,
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
                confirmed_position: Position::ZERO,
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
                source_map_url: None,
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

    #[test]
    fn ineligible_parsed_script_is_terminal_and_releases_stale_binding() {
        let (mut state, session) = configured_session();
        let old_script = ScriptKey {
            session: session.clone(),
            script_id: "old".into(),
        };
        let key = breakpoint_key();
        let physical = PhysicalBreakpointKey {
            script: old_script.clone(),
            script_version: 1,
            position: Position::ZERO,
            condition: None,
        };
        {
            let state = Arc::make_mut(&mut state);
            Arc::make_mut(&mut state.scripts).insert(
                old_script.clone(),
                Arc::new(ScriptState {
                    url: "old.js".into(),
                    provenance: Default::default(),
                    captured_source: None,
                    hash: "old-hash".into(),
                    source_map_url: Some("old.js.map".into()),
                    version: 1,
                    source: ScriptSourceState::Resolved(SourceViewState {
                        view_id: EffectId(100),
                        logical_sources: Arc::new(BTreeMap::new()),
                    }),
                }),
            );
            Arc::make_mut(&mut state.breakpoints).insert(
                key.clone(),
                Arc::new(BreakpointState {
                    generation: 1,
                    source_url: "src/app.ts".into(),
                    position: Position::ZERO,
                    condition: None,
                    friendly_candidate_selected: false,
                    candidate_index: Arc::new(BreakpointCandidateIndex::default()),
                    pending_mappings: Arc::new(BTreeMap::new()),
                    assessments: Arc::new(
                        BTreeMap::from([(
                            old_script.clone(),
                            BreakpointAssessment {
                                script_version: 1,
                                status: BreakpointAssessmentStatus::SourceNotFound {
                                    diagnostics: Arc::new(Vec::new()),
                                },
                            },
                        )])
                        .into(),
                    ),
                    bindings: Arc::new(BTreeMap::from([(
                        physical.clone(),
                        BreakpointBinding::Installed {
                            backend_id: "stale-backend".into(),
                        },
                    )])),
                }),
            );
            Arc::make_mut(&mut state.physical_breakpoints).insert(
                physical.clone(),
                Arc::new(PhysicalBreakpointState {
                    owners: Arc::new(BTreeSet::from([key.clone()])),
                    status: PhysicalBreakpointStatus::Installed {
                        backend_id: "stale-backend".into(),
                    },
                    confirmed_position: Some(physical.position),
                }),
            );
        }

        let parsed = reduce(
            &state,
            Input::ScriptParsed {
                session: session.clone(),
                script_id: "unrelated".into(),
                url: "vendor.js".into(),
                hash: "vendor-hash".into(),
                source_map_url: None,
            },
        );
        let unrelated = ScriptKey {
            session,
            script_id: "unrelated".into(),
        };
        assert!(matches!(
            parsed.state.breakpoints[&key].assessments[&unrelated].status,
            BreakpointAssessmentStatus::SourceNotFound { .. }
        ));
        assert!(matches!(
            parsed.effects.as_slice(),
            [Effect::RemoveBreakpoint {
                physical: removed,
                backend_id,
                ..
            }] if removed == &physical && backend_id == "stale-backend"
        ));
        assert!(
            !parsed.state.breakpoints[&key]
                .bindings
                .contains_key(&physical)
        );
        assert!(matches!(
            parsed.state.scripts[&unrelated].source,
            ScriptSourceState::Unresolved
        ));
    }

    #[test]
    fn incremental_script_load_assessment_clones_scale_near_linearly() {
        const BREAKPOINT_COUNT: usize = 32;

        fn load_scripts(script_count: usize) -> (usize, usize, std::time::Duration) {
            let (mut state, session) = configured_session();
            for index in 0..BREAKPOINT_COUNT {
                let state = Arc::make_mut(&mut state);
                Arc::make_mut(&mut state.breakpoints).insert(
                    BreakpointKey {
                        client_id: "benchmark".into(),
                        breakpoint_id: format!("breakpoint-{index}"),
                    },
                    Arc::new(BreakpointState {
                        generation: 1,
                        source_url: format!("source-{index}.ts"),
                        position: Position::ZERO,
                        condition: None,
                        friendly_candidate_selected: false,
                        candidate_index: Arc::new(BreakpointCandidateIndex::default()),
                        pending_mappings: Arc::new(BTreeMap::new()),
                        assessments: Arc::new(OrdMap::new()),
                        bindings: Arc::new(BTreeMap::new()),
                    }),
                );
            }
            BREAKPOINT_ASSESSMENT_CLONE_COUNT.with(|count| count.set(0));
            BREAKPOINT_RECONCILIATION_SCRIPT_VISITS.with(|count| count.set(0));

            let started = std::time::Instant::now();
            for index in 0..script_count {
                let parsed = reduce(
                    &state,
                    Input::ScriptParsed {
                        session: session.clone(),
                        script_id: format!("script-{index}"),
                        url: format!("script-{index}.js"),
                        hash: format!("hash-{index}"),
                        source_map_url: None,
                    },
                );
                assert!(parsed.effects.is_empty());
                state = parsed.state;
            }
            let elapsed = started.elapsed();
            assert!(state.breakpoints.values().all(|breakpoint| {
                breakpoint.assessments.len() == script_count
                    && breakpoint.assessments.values().all(|assessment| {
                        matches!(
                            assessment.status,
                            BreakpointAssessmentStatus::SourceNotFound { .. }
                        )
                    })
            }));
            (
                BREAKPOINT_ASSESSMENT_CLONE_COUNT.with(std::cell::Cell::get),
                BREAKPOINT_RECONCILIATION_SCRIPT_VISITS.with(std::cell::Cell::get),
                elapsed,
            )
        }

        let small = load_scripts(128);
        let large = load_scripts(256);
        eprintln!(
            "{BREAKPOINT_COUNT} breakpoints: 128 scripts = {} clones/{:?}; \
             256 scripts = {} clones/{:?}; full reconciliation visits = {}",
            small.0,
            small.2,
            large.0,
            large.2,
            small.1 + large.1
        );
        assert_eq!(small.1 + large.1, 0);
        assert!(
            large.0.saturating_mul(2) <= small.0.saturating_mul(5),
            "doubling scripts must not cause quadratic assessment cloning: {small:?} -> {large:?}"
        );
        assert!(
            large.0 <= BREAKPOINT_COUNT * 256 * 64,
            "persistent updates must keep assessment cloning bounded per insertion"
        );
    }

    #[test]
    fn mapped_source_completion_scales_with_only_affected_scripts() {
        const SCRIPT_COUNT: usize = 2_000;
        const BREAKPOINT_COUNT: usize = 32;

        let (mut state, session) = configured_session();
        let store = crate::content_store::ContentStore::default();
        let content = store.intern("source");
        let mut scripts = OrdMap::new();
        let mut pending = BTreeMap::new();
        let mut assessments = OrdMap::new();
        let mut unresolved = OrdSet::new();
        for index in 0..SCRIPT_COUNT {
            let script = ScriptKey {
                session: session.clone(),
                script_id: format!("script-{index:04}"),
            };
            let effect_id = EffectId(index as u64 + 10_000);
            scripts.insert(
                script.clone(),
                Arc::new(ScriptState {
                    url: format!("bundle-{index:04}.js"),
                    provenance: Default::default(),
                    captured_source: None,
                    hash: format!("hash-{index:04}"),
                    source_map_url: Some(format!("bundle-{index:04}.js.map")),
                    version: 1,
                    source: ScriptSourceState::Loaded {
                        content: Arc::from("compiled"),
                        source_map: Some(SourceMapData::new([])),
                        build_effect: effect_id,
                    },
                }),
            );
            pending.insert(
                effect_id,
                PendingEffect::BuildSourceView {
                    script: script.clone(),
                    version: 1,
                },
            );
            assessments.insert(
                script.clone(),
                BreakpointAssessment {
                    script_version: 1,
                    status: BreakpointAssessmentStatus::WaitingForScript,
                },
            );
            unresolved.insert(script);
        }
        {
            let state = Arc::make_mut(&mut state);
            state.next_effect_id = 20_000;
            state.scripts = Arc::new(scripts);
            state.pending = Arc::new(pending);
            let candidate_index = Arc::new(BreakpointCandidateIndex {
                unresolved,
                ..BreakpointCandidateIndex::default()
            });
            for index in 0..BREAKPOINT_COUNT {
                Arc::make_mut(&mut state.breakpoints).insert(
                    BreakpointKey {
                        client_id: "benchmark".into(),
                        breakpoint_id: format!("breakpoint-{index:02}"),
                    },
                    Arc::new(BreakpointState {
                        generation: 1,
                        source_url: format!("source-{index:02}.ts"),
                        position: Position::ZERO,
                        condition: None,
                        friendly_candidate_selected: false,
                        candidate_index: candidate_index.clone(),
                        pending_mappings: Arc::new(BTreeMap::new()),
                        assessments: Arc::new(assessments.clone()),
                        bindings: Arc::new(BTreeMap::new()),
                    }),
                );
            }
        }
        BREAKPOINT_ASSESSMENT_CLONE_COUNT.with(|count| count.set(0));
        BREAKPOINT_RECONCILIATION_SCRIPT_VISITS.with(|count| count.set(0));
        BREAKPOINT_INCREMENTAL_SCRIPT_VISITS.with(|count| count.set(0));

        let started = std::time::Instant::now();
        for index in 0..SCRIPT_COUNT {
            let completed = reduce(
                &state,
                Input::SourceViewBuilt {
                    effect_id: EffectId(index as u64 + 10_000),
                    logical_sources: BTreeMap::from([(
                        format!("vendor-{index:04}.ts"),
                        ContentCandidate {
                            content,
                            provenance: crate::source_view::Provenance::Workspace {
                                logical_url: format!("vendor-{index:04}.ts"),
                            },
                        },
                    )]),
                },
            );
            assert!(completed.effects.is_empty());
            state = completed.state;
        }
        let elapsed = started.elapsed();
        let full_visits = BREAKPOINT_RECONCILIATION_SCRIPT_VISITS.with(std::cell::Cell::get);
        let incremental_visits = BREAKPOINT_INCREMENTAL_SCRIPT_VISITS.with(std::cell::Cell::get);
        let assessment_clones = BREAKPOINT_ASSESSMENT_CLONE_COUNT.with(std::cell::Cell::get);
        eprintln!(
            "{SCRIPT_COUNT} mapped scripts x {BREAKPOINT_COUNT} breakpoints: \
             {incremental_visits} affected-script visits, {full_visits} full-script visits, \
             {assessment_clones} assessment clones, {elapsed:?}"
        );

        assert_eq!(full_visits, 0);
        assert_eq!(incremental_visits, SCRIPT_COUNT * BREAKPOINT_COUNT);
        assert!(
            assessment_clones <= incremental_visits * 128,
            "persistent assessment updates must have bounded cloning per affected script"
        );
        assert!(state.breakpoints.values().all(|breakpoint| {
            breakpoint.assessments.len() == SCRIPT_COUNT
                && breakpoint.candidate_index.unresolved.is_empty()
                && breakpoint.assessments.values().all(|assessment| {
                    matches!(
                        assessment.status,
                        BreakpointAssessmentStatus::SourceNotFound { .. }
                    )
                })
        }));
    }

    #[test]
    fn same_source_candidate_membership_updates_only_the_completed_script() {
        const SCRIPT_COUNT: usize = 2_000;
        const BREAKPOINT_COUNT: usize = 32;

        let session = SessionKey {
            connection_generation: 1,
            session_id: "session".into(),
        };
        let store = crate::content_store::ContentStore::default();
        let content = store.intern("shared source");
        let candidate = BreakpointSourceCandidate {
            source_url: "shared.ts".to_owned(),
            revision: SourceRevision::Content(content),
            provenance: crate::source_view::Provenance::Workspace {
                logical_url: "shared.ts".to_owned(),
            },
        };
        let mut indexes = vec![BreakpointCandidateIndex::default(); BREAKPOINT_COUNT];
        let mut selections = indexes
            .iter()
            .map(BreakpointCandidateIndex::selection)
            .collect::<Vec<_>>();
        let started = std::time::Instant::now();
        for index in 0..SCRIPT_COUNT {
            let script = ScriptKey {
                session: session.clone(),
                script_id: format!("script-{index:04}"),
            };
            for breakpoint in 0..BREAKPOINT_COUNT {
                indexes[breakpoint].exact.insert(&script, candidate.clone());
                let next = indexes[breakpoint].selection();
                assert_eq!(
                    selections[breakpoint].changed_scripts(&next, &script),
                    BTreeSet::from([script.clone()])
                );
                selections[breakpoint] = next;
            }
        }
        let elapsed = started.elapsed();
        eprintln!(
            "{SCRIPT_COUNT} same-source scripts x {BREAKPOINT_COUNT} breakpoints: {elapsed:?}"
        );
        assert!(indexes.iter().all(|index| {
            index.exact.keys.len() == 1
                && index.exact.matching_scripts.len() == SCRIPT_COUNT
                && index.selection().same_status_payload(&selections[0])
        }));
    }

    #[test]
    fn candidate_index_deduplicates_and_bounds_diagnostics_during_collection() {
        let session = SessionKey {
            connection_generation: 1,
            session_id: "session".into(),
        };
        let store = crate::content_store::ContentStore::default();
        let mut bucket = BreakpointCandidateBucket::default();
        for index in (0..MAX_BREAKPOINT_CANDIDATES + 3).rev() {
            let script = ScriptKey {
                session: session.clone(),
                script_id: format!("script-{index}"),
            };
            bucket.insert(
                &script,
                BreakpointSourceCandidate {
                    source_url: format!("source-{index:02}.ts"),
                    revision: SourceRevision::Content(store.intern(&format!("content-{index}"))),
                    provenance: crate::source_view::Provenance::Workspace {
                        logical_url: format!("source-{index:02}.ts"),
                    },
                },
            );
        }
        let duplicate_script = ScriptKey {
            session,
            script_id: "duplicate".into(),
        };
        let duplicate_index = 0;
        bucket.insert(
            &duplicate_script,
            BreakpointSourceCandidate {
                source_url: format!("source-{duplicate_index:02}.ts"),
                revision: SourceRevision::Content(store.intern(&format!("content-{duplicate_index}"))),
                provenance: crate::source_view::Provenance::Workspace {
                    logical_url: "duplicate.ts".into(),
                },
            },
        );

        assert_eq!(bucket.keys.len(), MAX_BREAKPOINT_CANDIDATES + 3);
        assert_eq!(bucket.bounded.len(), MAX_BREAKPOINT_CANDIDATES);
        assert_eq!(
            bucket.keys.len() - bucket.bounded.len(),
            3,
            "duplicates must not inflate omitted candidate diagnostics"
        );
        assert_eq!(
            bucket.bounded.keys().next().unwrap().source_url,
            "source-00.ts"
        );
        assert_eq!(
            bucket.bounded.keys().next_back().unwrap().source_url,
            format!("source-{:02}.ts", MAX_BREAKPOINT_CANDIDATES - 1)
        );
    }

    #[test]
    fn new_breakpoint_retries_failed_source_hydration_and_advances_assessments() {
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
        let script = parsed.state.scripts.keys().next().unwrap().clone();
        let first_key = breakpoint_key();
        let first = reduce(
            &parsed.state,
            Input::SetBreakpoint {
                key: first_key.clone(),
                source_url: "src/app.ts".into(),
                position: Position::ZERO,
                condition: None,
            },
        );
        let Effect::FetchScriptSource {
            effect_id: failed_fetch,
            ..
        } = first.effects[0]
        else {
            panic!("first breakpoint should hydrate the source");
        };
        let failed = reduce(
            &first.state,
            Input::EffectFailed {
                effect_id: failed_fetch,
                message: "source unavailable".into(),
            },
        );
        assert!(
            failed.effects.is_empty(),
            "a hydration failure must not immediately retry itself"
        );
        assert!(matches!(
            failed.state.breakpoints[&first_key].assessments[&script].status,
            BreakpointAssessmentStatus::Failed { ref message }
                if message == "source unavailable"
        ));

        let second_key = BreakpointKey {
            client_id: "client".into(),
            breakpoint_id: "bp-2".into(),
        };
        let retrying = reduce(
            &failed.state,
            Input::SetBreakpoint {
                key: second_key.clone(),
                source_url: "src/app.ts".into(),
                position: Position { line: 1, column: 0 },
                condition: None,
            },
        );
        let [
            Effect::FetchScriptSource {
                effect_id: retry_fetch,
                ..
            },
        ] = retrying.effects.as_slice()
        else {
            panic!("new breakpoint intent should retry failed source hydration");
        };
        assert_ne!(*retry_fetch, failed_fetch);
        assert!(matches!(
            retrying.state.scripts[&script].source,
            ScriptSourceState::Pending(effect_id) if effect_id == *retry_fetch
        ));
        for key in [&first_key, &second_key] {
            assert!(matches!(
                retrying.state.breakpoints[key].assessments[&script].status,
                BreakpointAssessmentStatus::WaitingForScript
            ));
        }

        let stale = reduce(
            &retrying.state,
            Input::ScriptSourceFetched {
                effect_id: failed_fetch,
                content: Arc::from("stale"),
                source_map: None,
                source_map_url: None,
                source_map_error: None,
            },
        );
        assert!(matches!(
            stale.state.scripts[&script].source,
            ScriptSourceState::Pending(effect_id) if effect_id == *retry_fetch
        ));
        assert!(matches!(
            stale.state.diagnostics.last(),
            Some(Diagnostic::IgnoredStaleEffect { effect_id }) if *effect_id == failed_fetch
        ));

        let fetched = reduce(
            &stale.state,
            Input::ScriptSourceFetched {
                effect_id: *retry_fetch,
                content: Arc::from("compiled"),
                source_map: Some(SourceMapData::new([])),
                source_map_url: Some("file:///bundle.js.map".into()),
                source_map_error: None,
            },
        );
        let Effect::BuildSourceView {
            effect_id: view_id, ..
        } = fetched.effects[0]
        else {
            panic!("retried hydration should build a source view");
        };
        let built = reduce(
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
        let second_mapping = built
            .effects
            .iter()
            .find_map(|effect| match effect {
                Effect::MapBreakpoint {
                    effect_id,
                    breakpoint,
                    ..
                } if breakpoint == &second_key => Some(*effect_id),
                _ => None,
            })
            .expect("new breakpoint assessment should advance to mapping");
        let mapped = reduce(
            &built.state,
            Input::BreakpointMapped {
                effect_id: second_mapping,
                generated_positions: vec![Position { line: 2, column: 0 }],
            },
        );
        assert!(matches!(
            mapped.state.breakpoints[&second_key].assessments[&script].status,
            BreakpointAssessmentStatus::Applicable { .. }
        ));
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
                source_map: Some(SourceMapData::new([])),
                source_map_url: Some("file:///bundle.js.map".into()),
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

    fn resolved_script_with_source(
        state: &Arc<DebuggerState>,
        session: &SessionKey,
        script_id: &str,
        generated_url: &str,
        source_url: &str,
        content: &str,
    ) -> Arc<DebuggerState> {
        let parsed = reduce(
            state,
            Input::ScriptParsed {
                session: session.clone(),
                script_id: script_id.into(),
                url: generated_url.into(),
                hash: format!("{script_id}-hash"),
                source_map_url: Some(format!("{generated_url}.map")),
            },
        );
        let script = ScriptKey {
            session: session.clone(),
            script_id: script_id.into(),
        };
        let fetch = parsed
            .effects
            .iter()
            .find_map(|effect| match effect {
                Effect::FetchScriptSource {
                    effect_id,
                    script: candidate,
                    ..
                } if candidate == &script => Some(*effect_id),
                _ => None,
            })
            .map(|effect_id| (parsed.state.clone(), effect_id))
            .unwrap_or_else(|| {
                let requested = reduce(
                    &parsed.state,
                    Input::RequestScriptSource {
                        script: script.clone(),
                    },
                );
                (requested.state, requested.effects[0].effect_id())
            });
        let fetched = reduce(
            &fetch.0,
            Input::ScriptSourceFetched {
                effect_id: fetch.1,
                content: Arc::from("compiled"),
                source_map: Some(SourceMapData::new([])),
                source_map_url: Some(format!("{generated_url}.map")),
                source_map_error: None,
            },
        );
        let view = fetched.effects[0].effect_id();
        let store = crate::content_store::ContentStore::default();
        reduce(
            &fetched.state,
            Input::SourceViewBuilt {
                effect_id: view,
                logical_sources: BTreeMap::from([(
                    source_url.into(),
                    ContentCandidate {
                        content: store.intern(content),
                        provenance: crate::source_view::Provenance::Workspace {
                            logical_url: source_url.into(),
                        },
                    },
                )]),
            },
        )
        .state
    }

    fn breakpoint_key() -> BreakpointKey {
        BreakpointKey {
            client_id: "client".into(),
            breakpoint_id: "bp".into(),
        }
    }
}
