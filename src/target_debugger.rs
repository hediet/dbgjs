use std::collections::{BTreeMap, BTreeSet};
use std::fs::File;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

use rayon::prelude::*;
use tokio::sync::{broadcast, mpsc, oneshot, watch};

use crate::cdp::{
    DebuggerEvaluateOnCallFrameParams, DomGetBoxModelParams, DomGetDocumentParams,
    DomQuerySelectorParams, HeapProfilerTakeHeapSnapshotParams, InputDispatchKeyEventParams,
    InputDispatchKeyEventParamsType, InputDispatchMouseEventParams,
    InputDispatchMouseEventParamsType, InputInsertTextParams, InputMouseButton,
    PageCaptureScreenshotParams, PageCaptureScreenshotParamsFormat, ProfilerEnableParams,
    ProfilerProfile, ProfilerScriptCoverage, ProfilerSetSamplingIntervalParams,
    ProfilerStartParams, ProfilerStartPreciseCoverageParams, ProfilerStopParams,
    ProfilerStopPreciseCoverageParams, ProfilerTakePreciseCoverageParams,
    RuntimeCallFunctionOnParams, RuntimeExceptionDetails, RuntimeGetPropertiesParams,
    RuntimeInternalPropertyDescriptor, RuntimePropertyDescriptor, RuntimeReleaseObjectGroupParams,
    RuntimeReleaseObjectParams, RuntimeRemoteObject, RuntimeRemoteObjectSubtype,
    RuntimeRemoteObjectType,
};
use crate::cdp_runtime::CdpDebuggerSession;
use crate::context_source_model::ContextSourceModel;
use crate::debugger_driver::{DebuggerDriver, DebuggerDriverError};
use crate::debugger_engine::{
    BreakpointAssessmentStatus, BreakpointBinding, BreakpointKey, BreakpointMapping,
    BreakpointSourceCandidate, DebuggerState, FrameProjection, Input, PhysicalBreakpointKey,
    ScriptKey, ScriptSourceState, SessionKey, SessionPhase, StepKind,
};
use crate::heap_graph::{
    AggregateBy, CostPolicy, EdgePolicy, HeapGraph, NodeIndex, NodeSelector, PathDirection,
    PathOptions, TextMatcher, TraversalDirection, parse_heap_graph,
};
use crate::heap_snapshot::{HeapConstructorGroup, parse_constructor_groups};
use crate::promise_debugging::{
    has_live_promise_evidence, inspect_heap_promises, inspect_live_promise, remote_value_snapshot,
};
use crate::service_api::{
    BreakpointApplicationSnapshot, BreakpointApplicationStatus, BreakpointMappingSnapshot,
    BreakpointScriptAssessmentSnapshot, BreakpointScriptAssessmentStatus,
    BreakpointSourceCandidateSnapshot, ConsoleMessageSnapshot, CoverageAnalysisSnapshot,
    CoverageFunctionSnapshot, CoverageRangeSnapshot, CoverageSnapshot, CoverageSourceSnapshot,
    CpuProfileAnalysisSnapshot, CpuProfileCallFrameSnapshot, CpuProfileFunctionSnapshot,
    CpuProfileNodeSnapshot, CpuProfilePositionTickSnapshot, CpuProfileSnapshot, EvaluationSnapshot,
    FrameProjectionSnapshot, FrameSnapshot, HeapAggregateBy, HeapAggregateEntrySnapshot,
    HeapAggregateSnapshot, HeapCaptureResult, HeapClassAnalysisSnapshot, HeapClassSnapshot,
    HeapClassSnapshotEntry, HeapDiffEntrySnapshot, HeapDiffSnapshot, HeapDominatorSnapshot,
    HeapEdgePolicy, HeapInstanceSnapshot, HeapNodeLocationSnapshot, HeapNodeSelectionSnapshot,
    HeapNodeSelector, HeapNodeSnapshot, HeapPathCost, HeapPathDirection, HeapPathOptions,
    HeapPathSnapshot, HeapPathStepSnapshot, HeapReferenceDirection, HeapReferenceSnapshot,
    HeapReferencesSnapshot, HeapSnapshotProgress, HeapSnapshotResult, HeapSnapshotTiming,
    HeapTraversalDirection, PauseSnapshot, PromiseSelectionSnapshot, PromiseState, ScopeSnapshot,
    ScreenshotSnapshot, SourceContentSnapshot, SourceExcerpt, SourceExcerptLine,
    SourceGraphViewSnapshot, SourceLocation, SourceMappingSnapshot, TargetBreakpointSnapshot,
    TargetBreakpointStatus, TargetDebuggerPhase, TargetDebuggerSnapshot, TargetScriptSnapshot,
    TargetScriptStatus, TargetWaitPredicate, ValueInspectionOptions, ValuePreviewSnapshot,
    ValuePropertySnapshot, ValueSelector, ValueSnapshot, VariableSnapshot,
};
use crate::source_effects::{SourceEffectInterpreter, SourceEffectOptions};
use crate::source_search::{HydratedSourceBatch, SearchControl, SearchError};
use crate::source_view::Position;

const COMMAND_BUFFER: usize = 32;
const MAX_WAIT: Duration = Duration::from_secs(5 * 60);
const RAW_CDP_TIMEOUT: Duration = Duration::from_secs(30);

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
    pause_events: broadcast::Sender<TargetDebuggerSnapshot>,
    session_id: String,
    heap_snapshot_progress: watch::Receiver<Option<crate::cdp_runtime::HeapSnapshotStreamProgress>>,
    raw_events: broadcast::Sender<crate::cdp_runtime::RawCdpEvent>,
    raw_event_history: Arc<std::sync::Mutex<Vec<crate::cdp_runtime::RawCdpEvent>>>,
}

impl TargetDebuggerHandle {
    pub(crate) fn same_instance(&self, other: &Self) -> bool {
        self.commands.same_channel(&other.commands)
    }

    pub async fn start(
        context_id: String,
        connection_id: String,
        target_id: String,
        connection_generation: u64,
        session: CdpDebuggerSession,
        session_key: SessionKey,
        waiting_for_debugger: bool,
        source_model: Arc<ContextSourceModel>,
    ) -> Result<Self, TargetDebuggerError> {
        let sources = SourceEffectInterpreter::new(
            SourceEffectOptions::default(),
            source_model,
            format!("{connection_id}/{target_id}"),
        );
        let mut driver = DebuggerDriver::new(
            Arc::new(DebuggerState::before_connection_generation(
                connection_generation,
            )),
            session,
            sources,
        );
        let heap_snapshot_progress = driver.heap_snapshot_progress();
        let raw_events = driver.raw_events_sender();
        let raw_event_history = driver.raw_event_history();
        driver.apply(Input::Connected).await?;
        driver
            .apply(Input::SessionAttached {
                session_id: session_key.session_id.clone(),
                target_id: target_id.clone(),
                parent_session_id: None,
                waiting_for_debugger,
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
        let (pause_events, _) = broadcast::channel(COMMAND_BUFFER);
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
            pause_events.clone(),
        ));
        Ok(Self {
            commands,
            snapshots,
            pause_events,
            session_id: session_key.session_id,
            heap_snapshot_progress,
            raw_events,
            raw_event_history,
        })
    }

    /// Subscribes to every raw CDP notification observed on this target, mirroring the wire
    /// event verbatim rather than only the subset the debugger engine reducer understands.
    /// Relay dispatchers use this to forward console, network, and lifecycle events.
    pub fn subscribe_raw_events(&self) -> broadcast::Receiver<crate::cdp_runtime::RawCdpEvent> {
        self.raw_events.subscribe()
    }

    pub fn raw_event_history(&self) -> Arc<std::sync::Mutex<Vec<crate::cdp_runtime::RawCdpEvent>>> {
        self.raw_event_history.clone()
    }

    pub fn snapshot(&self) -> TargetDebuggerSnapshot {
        self.snapshots.borrow().clone()
    }

    pub async fn set_breakpoint(
        &self,
        context_revision: u64,
        breakpoint: TargetBreakpointSpec,
    ) -> Result<TargetDebuggerSnapshot, TargetDebuggerError> {
        self.set_breakpoints(context_revision, vec![breakpoint])
            .await
    }

    pub async fn set_breakpoints(
        &self,
        context_revision: u64,
        breakpoints: Vec<TargetBreakpointSpec>,
    ) -> Result<TargetDebuggerSnapshot, TargetDebuggerError> {
        self.command(|response| TargetCommand::SetBreakpoints {
            context_revision,
            breakpoints,
            response,
        })
        .await
    }

    pub async fn remove_breakpoint(
        &self,
        context_revision: u64,
        breakpoint_id: String,
    ) -> Result<TargetDebuggerSnapshot, TargetDebuggerError> {
        self.command(|response| TargetCommand::RemoveBreakpoint {
            context_revision,
            breakpoint_id,
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

    pub async fn release_if_waiting(&self) -> Result<TargetDebuggerSnapshot, TargetDebuggerError> {
        self.command(|response| TargetCommand::ReleaseIfWaiting { response })
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

    pub async fn scope_variables(
        &self,
        pause_epoch: u64,
        frame_index: u32,
        scope_index: u32,
    ) -> Result<Vec<VariableSnapshot>, TargetDebuggerError> {
        let (response, receiver) = oneshot::channel();
        self.commands
            .send(TargetCommand::ScopeVariables {
                pause_epoch,
                frame_index,
                scope_index,
                response,
            })
            .await
            .map_err(|_| TargetDebuggerError::Stopped)?;
        receiver.await.map_err(|_| TargetDebuggerError::Stopped)?
    }

    pub async fn object_properties(
        &self,
        pause_epoch: Option<u64>,
        object_id: String,
    ) -> Result<Vec<VariableSnapshot>, TargetDebuggerError> {
        let (response, receiver) = oneshot::channel();
        self.commands
            .send(TargetCommand::ObjectProperties {
                pause_epoch,
                object_id,
                response,
            })
            .await
            .map_err(|_| TargetDebuggerError::Stopped)?;
        receiver.await.map_err(|_| TargetDebuggerError::Stopped)?
    }

    pub async fn raw_cdp_request(
        &self,
        method: String,
        params: serde_json::Value,
    ) -> Result<serde_json::Value, hubrpc::prelude::JsonRpcError> {
        let (response, receiver) = oneshot::channel();
        self.commands
            .send(TargetCommand::RawCdpRequest {
                method,
                params,
                response,
            })
            .await
            .map_err(|_| {
                hubrpc::prelude::JsonRpcError::new(
                    hubrpc::prelude::error_codes::PEER_DISCONNECTED,
                    "target debugger stopped before CDP request was sent",
                )
            })?;
        receiver.await.map_err(|_| {
            hubrpc::prelude::JsonRpcError::new(
                hubrpc::prelude::error_codes::PEER_DISCONNECTED,
                "target debugger stopped before CDP response arrived",
            )
        })?
    }

    pub async fn inspect_value(
        &self,
        pause_epoch: Option<u64>,
        selector: ValueSelector,
        options: ValueInspectionOptions,
    ) -> Result<ValueSnapshot, TargetDebuggerError> {
        Self::validate_value_inspection(&selector, &options)?;
        let (response, receiver) = oneshot::channel();
        self.commands
            .send(TargetCommand::InspectValue {
                pause_epoch,
                selector,
                options,
                response,
            })
            .await
            .map_err(|_| TargetDebuggerError::Stopped)?;
        receiver.await.map_err(|_| TargetDebuggerError::Stopped)?
    }

    pub async fn source_content(
        &self,
        path: String,
    ) -> Result<Option<SourceContentSnapshot>, TargetDebuggerError> {
        let (response, receiver) = oneshot::channel();
        self.commands
            .send(TargetCommand::SourceContent { path, response })
            .await
            .map_err(|_| TargetDebuggerError::Stopped)?;
        receiver.await.map_err(|_| TargetDebuggerError::Stopped)?
    }

    fn validate_value_inspection(
        selector: &ValueSelector,
        options: &ValueInspectionOptions,
    ) -> Result<(), TargetDebuggerError> {
        if matches!(selector, ValueSelector::RemoteObject { .. }) && !options.retain_references {
            return Err(TargetDebuggerError::InvalidValueInspection(
                "an existing remote object requires retained references; use an expression for an ephemeral bounded preview"
                    .to_owned(),
            ));
        }
        Ok(())
    }

    pub async fn resolved_source_paths(
        &self,
    ) -> Result<Vec<(String, String)>, TargetDebuggerError> {
        let (response, receiver) = oneshot::channel();
        self.commands
            .send(TargetCommand::ResolvedSourcePaths { response })
            .await
            .map_err(|_| TargetDebuggerError::Stopped)?;
        receiver.await.map_err(|_| TargetDebuggerError::Stopped)?
    }

    pub async fn source_search_batch(
        &self,
        path_selector: Option<String>,
        control: SearchControl,
    ) -> Result<HydratedSourceBatch, TargetDebuggerError> {
        let (response, receiver) = oneshot::channel();
        self.commands
            .send(TargetCommand::SourceSearchBatch {
                path_selector,
                control,
                response,
            })
            .await
            .map_err(|_| TargetDebuggerError::Stopped)?;
        receiver.await.map_err(|_| TargetDebuggerError::Stopped)?
    }

    pub async fn explain_source(
        &self,
        path: String,
    ) -> Result<Vec<SourceGraphViewSnapshot>, TargetDebuggerError> {
        let (response, receiver) = oneshot::channel();
        self.commands
            .send(TargetCommand::ExplainSource { path, response })
            .await
            .map_err(|_| TargetDebuggerError::Stopped)?;
        receiver.await.map_err(|_| TargetDebuggerError::Stopped)?
    }

    pub async fn map_source(
        &self,
        path: String,
        line: u32,
        column: u32,
    ) -> Result<Vec<SourceMappingSnapshot>, TargetDebuggerError> {
        let (response, receiver) = oneshot::channel();
        self.commands
            .send(TargetCommand::MapSource {
                path,
                line,
                column,
                response,
            })
            .await
            .map_err(|_| TargetDebuggerError::Stopped)?;
        receiver.await.map_err(|_| TargetDebuggerError::Stopped)?
    }

    pub async fn evict_source_caches(&self) -> Result<(), TargetDebuggerError> {
        let (response, receiver) = oneshot::channel();
        self.commands
            .send(TargetCommand::EvictSourceCaches { response })
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

    pub async fn key(&self, chord: String) -> Result<(), TargetDebuggerError> {
        let (response, receiver) = oneshot::channel();
        self.commands
            .send(TargetCommand::Key { chord, response })
            .await
            .map_err(|_| TargetDebuggerError::Stopped)?;
        receiver.await.map_err(|_| TargetDebuggerError::Stopped)?
    }

    pub async fn type_text(&self, text: String) -> Result<(), TargetDebuggerError> {
        let (response, receiver) = oneshot::channel();
        self.commands
            .send(TargetCommand::TypeText { text, response })
            .await
            .map_err(|_| TargetDebuggerError::Stopped)?;
        receiver.await.map_err(|_| TargetDebuggerError::Stopped)?
    }

    pub async fn capture_screenshot(&self) -> Result<ScreenshotSnapshot, TargetDebuggerError> {
        let (response, receiver) = oneshot::channel();
        self.commands
            .send(TargetCommand::CaptureScreenshot { response })
            .await
            .map_err(|_| TargetDebuggerError::Stopped)?;
        receiver.await.map_err(|_| TargetDebuggerError::Stopped)?
    }

    pub async fn take_heap_snapshot(
        &self,
        path: String,
        capture_numeric_value: bool,
        expose_internals: bool,
    ) -> Result<HeapSnapshotResult, TargetDebuggerError> {
        let (response, receiver) = oneshot::channel();
        self.commands
            .send(TargetCommand::TakeHeapSnapshot {
                path,
                capture_numeric_value,
                expose_internals,
                response,
            })
            .await
            .map_err(|_| TargetDebuggerError::Stopped)?;
        receiver.await.map_err(|_| TargetDebuggerError::Stopped)?
    }

    pub async fn capture_heap_snapshot(
        &self,
        capture_id: Option<String>,
        capture_numeric_value: bool,
        expose_internals: bool,
    ) -> Result<HeapCaptureResult, TargetDebuggerError> {
        let (response, receiver) = oneshot::channel();
        self.commands
            .send(TargetCommand::CaptureHeapSnapshot {
                capture_id,
                capture_numeric_value,
                expose_internals,
                response,
            })
            .await
            .map_err(|_| TargetDebuggerError::Stopped)?;
        receiver.await.map_err(|_| TargetDebuggerError::Stopped)?
    }

    pub async fn copy_heap_capture(
        &self,
        capture_id: String,
        destination: String,
    ) -> Result<(), TargetDebuggerError> {
        let (response, receiver) = oneshot::channel();
        self.commands
            .send(TargetCommand::CopyHeapCapture {
                capture_id,
                destination,
                response,
            })
            .await
            .map_err(|_| TargetDebuggerError::Stopped)?;
        receiver.await.map_err(|_| TargetDebuggerError::Stopped)?
    }

    pub async fn delete_stored_capture(
        &self,
        capture_id: String,
    ) -> Result<bool, TargetDebuggerError> {
        let (response, receiver) = oneshot::channel();
        self.commands
            .send(TargetCommand::DeleteStoredCapture {
                capture_id,
                response,
            })
            .await
            .map_err(|_| TargetDebuggerError::Stopped)?;
        receiver.await.map_err(|_| TargetDebuggerError::Stopped)?
    }

    pub async fn get_heap_classes(
        &self,
        capture_id: String,
        filter: Option<String>,
        no_cache: bool,
    ) -> Result<HeapClassSnapshot, TargetDebuggerError> {
        let (response, receiver) = oneshot::channel();
        self.commands
            .send(TargetCommand::GetHeapClasses {
                capture_id,
                filter,
                no_cache,
                response,
            })
            .await
            .map_err(|_| TargetDebuggerError::Stopped)?;
        receiver.await.map_err(|_| TargetDebuggerError::Stopped)?
    }

    pub async fn select_promises(
        &self,
        capture_id: String,
        state: Option<PromiseState>,
        limit: u32,
        max_preview_length: u32,
    ) -> Result<PromiseSelectionSnapshot, TargetDebuggerError> {
        let (response, receiver) = oneshot::channel();
        self.commands
            .send(TargetCommand::SelectPromises {
                capture_id,
                state,
                limit,
                max_preview_length,
                response,
            })
            .await
            .map_err(|_| TargetDebuggerError::Stopped)?;
        receiver.await.map_err(|_| TargetDebuggerError::Stopped)?
    }

    pub async fn select_heap_nodes(
        &self,
        capture_id: String,
        selector: HeapNodeSelector,
        max_string_length: Option<u32>,
        include_dominators: bool,
    ) -> Result<HeapNodeSelectionSnapshot, TargetDebuggerError> {
        let (response, receiver) = oneshot::channel();
        self.commands
            .send(TargetCommand::SelectHeapNodes {
                capture_id,
                selector,
                max_string_length,
                include_dominators,
                response,
            })
            .await
            .map_err(|_| TargetDebuggerError::Stopped)?;
        receiver.await.map_err(|_| TargetDebuggerError::Stopped)?
    }

    pub async fn get_heap_references(
        &self,
        reference: String,
        direction: HeapReferenceDirection,
        edge_policy: HeapEdgePolicy,
        limit: u32,
        max_string_length: Option<u32>,
    ) -> Result<HeapReferencesSnapshot, TargetDebuggerError> {
        let (response, receiver) = oneshot::channel();
        self.commands
            .send(TargetCommand::GetHeapReferences {
                reference,
                direction,
                edge_policy,
                limit,
                max_string_length,
                response,
            })
            .await
            .map_err(|_| TargetDebuggerError::Stopped)?;
        receiver.await.map_err(|_| TargetDebuggerError::Stopped)?
    }

    pub async fn get_heap_path(
        &self,
        from: String,
        to: String,
        options: HeapPathOptions,
        max_string_length: Option<u32>,
    ) -> Result<Option<HeapPathSnapshot>, TargetDebuggerError> {
        let (response, receiver) = oneshot::channel();
        self.commands
            .send(TargetCommand::GetHeapPath {
                from,
                to,
                options,
                max_string_length,
                response,
            })
            .await
            .map_err(|_| TargetDebuggerError::Stopped)?;
        receiver.await.map_err(|_| TargetDebuggerError::Stopped)?
    }

    pub async fn get_heap_dominator_chain(
        &self,
        reference: String,
        max_string_length: Option<u32>,
    ) -> Result<HeapDominatorSnapshot, TargetDebuggerError> {
        let (response, receiver) = oneshot::channel();
        self.commands
            .send(TargetCommand::GetHeapDominatorChain {
                reference,
                max_string_length,
                response,
            })
            .await
            .map_err(|_| TargetDebuggerError::Stopped)?;
        receiver.await.map_err(|_| TargetDebuggerError::Stopped)?
    }

    pub async fn aggregate_heap_snapshot(
        &self,
        capture_id: String,
        by: HeapAggregateBy,
        limit: u32,
        max_string_length: Option<u32>,
    ) -> Result<HeapAggregateSnapshot, TargetDebuggerError> {
        let (response, receiver) = oneshot::channel();
        self.commands
            .send(TargetCommand::AggregateHeapSnapshot {
                capture_id,
                by,
                limit,
                max_string_length,
                response,
            })
            .await
            .map_err(|_| TargetDebuggerError::Stopped)?;
        receiver.await.map_err(|_| TargetDebuggerError::Stopped)?
    }

    pub async fn diff_heap_snapshots(
        &self,
        older_capture_id: String,
        newer_capture_id: String,
        by: HeapAggregateBy,
        limit: u32,
        max_string_length: Option<u32>,
    ) -> Result<HeapDiffSnapshot, TargetDebuggerError> {
        let (response, receiver) = oneshot::channel();
        self.commands
            .send(TargetCommand::DiffHeapSnapshots {
                older_capture_id,
                newer_capture_id,
                by,
                limit,
                max_string_length,
                response,
            })
            .await
            .map_err(|_| TargetDebuggerError::Stopped)?;
        receiver.await.map_err(|_| TargetDebuggerError::Stopped)?
    }

    pub fn heap_snapshot_progress(&self) -> Option<HeapSnapshotProgress> {
        self.heap_snapshot_progress
            .borrow()
            .clone()
            .map(|progress| HeapSnapshotProgress {
                done: progress.done,
                total: progress.total,
                finished: progress.finished,
                bytes_written: progress.bytes_written,
            })
    }

    pub async fn start_coverage(&self) -> Result<(), TargetDebuggerError> {
        let (response, receiver) = oneshot::channel();
        self.commands
            .send(TargetCommand::StartCoverage { response })
            .await
            .map_err(|_| TargetDebuggerError::Stopped)?;
        receiver.await.map_err(|_| TargetDebuggerError::Stopped)?
    }

    pub async fn take_coverage(
        &self,
        capture_id: Option<String>,
        exclude_capture_id: Option<String>,
    ) -> Result<CoverageSnapshot, TargetDebuggerError> {
        let (response, receiver) = oneshot::channel();
        self.commands
            .send(TargetCommand::TakeCoverage {
                capture_id,
                exclude_capture_id,
                response,
            })
            .await
            .map_err(|_| TargetDebuggerError::Stopped)?;
        receiver.await.map_err(|_| TargetDebuggerError::Stopped)?
    }

    pub async fn stop_coverage(
        &self,
        exclude_capture_id: Option<String>,
    ) -> Result<CoverageSnapshot, TargetDebuggerError> {
        self.stop_coverage_with_projection(exclude_capture_id).await
    }

    pub async fn finish_coverage(
        &self,
        exclude_capture_id: Option<String>,
    ) -> Result<(), TargetDebuggerError> {
        let (response, receiver) = oneshot::channel();
        self.commands
            .send(TargetCommand::FinishCoverage {
                exclude_capture_id,
                response,
            })
            .await
            .map_err(|_| TargetDebuggerError::Stopped)?;
        receiver.await.map_err(|_| TargetDebuggerError::Stopped)??;
        Ok(())
    }

    async fn stop_coverage_with_projection(
        &self,
        exclude_capture_id: Option<String>,
    ) -> Result<CoverageSnapshot, TargetDebuggerError> {
        let (response, receiver) = oneshot::channel();
        self.commands
            .send(TargetCommand::StopCoverage {
                exclude_capture_id,
                response,
            })
            .await
            .map_err(|_| TargetDebuggerError::Stopped)?;
        receiver.await.map_err(|_| TargetDebuggerError::Stopped)?
    }

    pub async fn get_coverage(
        &self,
        capture_id: String,
        source_path: Option<String>,
        no_cache: bool,
    ) -> Result<CoverageSnapshot, TargetDebuggerError> {
        let (response, receiver) = oneshot::channel();
        self.commands
            .send(TargetCommand::GetCoverage {
                capture_id,
                source_path,
                no_cache,
                response,
            })
            .await
            .map_err(|_| TargetDebuggerError::Stopped)?;
        receiver.await.map_err(|_| TargetDebuggerError::Stopped)?
    }

    pub async fn start_cpu_profile(
        &self,
        sampling_interval_micros: Option<u64>,
    ) -> Result<(), TargetDebuggerError> {
        let (response, receiver) = oneshot::channel();
        self.commands
            .send(TargetCommand::StartCpuProfile {
                sampling_interval_micros,
                response,
            })
            .await
            .map_err(|_| TargetDebuggerError::Stopped)?;
        receiver.await.map_err(|_| TargetDebuggerError::Stopped)?
    }

    pub async fn stop_cpu_profile(
        &self,
        capture_id: Option<String>,
    ) -> Result<CpuProfileSnapshot, TargetDebuggerError> {
        let (response, receiver) = oneshot::channel();
        self.commands
            .send(TargetCommand::StopCpuProfile {
                capture_id,
                response,
            })
            .await
            .map_err(|_| TargetDebuggerError::Stopped)?;
        receiver.await.map_err(|_| TargetDebuggerError::Stopped)?
    }

    pub async fn get_cpu_profile(
        &self,
        capture_id: String,
        source_path: Option<String>,
        no_cache: bool,
        project: bool,
    ) -> Result<CpuProfileSnapshot, TargetDebuggerError> {
        let (response, receiver) = oneshot::channel();
        self.commands
            .send(TargetCommand::GetCpuProfile {
                capture_id,
                source_path,
                no_cache,
                project,
                response,
            })
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
        let mut pause_events = self.pause_events.subscribe();
        tokio::time::timeout(timeout, async {
            loop {
                let current = snapshots.borrow_and_update().clone();
                if let TargetDebuggerPhase::Failed { message } = &current.phase {
                    return Err(TargetDebuggerError::DriverFailed(message.clone()));
                }
                if let Some(error) = breakpoint_wait_failure(&current, &predicate) {
                    return Err(error);
                }
                if predicate_matches(&current, &predicate) {
                    return Ok(current);
                }
                tokio::select! {
                    changed = snapshots.changed() => {
                        changed.map_err(|_| TargetDebuggerError::Stopped)?;
                    }
                    event = pause_events.recv(),
                        if matches!(predicate, TargetWaitPredicate::Paused { .. }) =>
                    {
                        match event {
                            Ok(snapshot) if predicate_matches(&snapshot, &predicate) => {
                                return Ok(snapshot);
                            }
                            Ok(_) | Err(broadcast::error::RecvError::Lagged(_)) => {}
                            Err(broadcast::error::RecvError::Closed) => {
                                return Err(TargetDebuggerError::Stopped);
                            }
                        }
                    }
                }
            }
        })
        .await
        .map_err(|_| TargetDebuggerError::WaitTimedOut)?
    }

    pub async fn settle(&self, maximum: Duration) -> TargetDebuggerSnapshot {
        let mut snapshots = self.snapshots.clone();
        let settled = tokio::time::timeout(maximum, async {
            loop {
                let current = snapshots.borrow_and_update().clone();
                if current.breakpoints.iter().all(|breakpoint| {
                    !matches!(
                        breakpoint.status,
                        TargetBreakpointStatus::WaitingForScript
                            | TargetBreakpointStatus::Applicable { .. }
                            | TargetBreakpointStatus::Installing { .. }
                    )
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
    SetBreakpoints {
        context_revision: u64,
        breakpoints: Vec<TargetBreakpointSpec>,
        response: CommandResponse,
    },
    RemoveBreakpoint {
        context_revision: u64,
        breakpoint_id: String,
        response: CommandResponse,
    },
    ReleaseIfWaiting {
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
    ScopeVariables {
        pause_epoch: u64,
        frame_index: u32,
        scope_index: u32,
        response: oneshot::Sender<Result<Vec<VariableSnapshot>, TargetDebuggerError>>,
    },
    ObjectProperties {
        pause_epoch: Option<u64>,
        object_id: String,
        response: oneshot::Sender<Result<Vec<VariableSnapshot>, TargetDebuggerError>>,
    },
    RawCdpRequest {
        method: String,
        params: serde_json::Value,
        response: oneshot::Sender<Result<serde_json::Value, hubrpc::prelude::JsonRpcError>>,
    },
    InspectValue {
        pause_epoch: Option<u64>,
        selector: ValueSelector,
        options: ValueInspectionOptions,
        response: oneshot::Sender<Result<ValueSnapshot, TargetDebuggerError>>,
    },
    SourceContent {
        path: String,
        response: oneshot::Sender<Result<Option<SourceContentSnapshot>, TargetDebuggerError>>,
    },
    ResolvedSourcePaths {
        response: oneshot::Sender<Result<Vec<(String, String)>, TargetDebuggerError>>,
    },
    SourceSearchBatch {
        path_selector: Option<String>,
        control: SearchControl,
        response: oneshot::Sender<Result<HydratedSourceBatch, TargetDebuggerError>>,
    },
    ExplainSource {
        path: String,
        response: oneshot::Sender<Result<Vec<SourceGraphViewSnapshot>, TargetDebuggerError>>,
    },
    MapSource {
        path: String,
        line: u32,
        column: u32,
        response: oneshot::Sender<Result<Vec<SourceMappingSnapshot>, TargetDebuggerError>>,
    },
    EvictSourceCaches {
        response: oneshot::Sender<Result<(), TargetDebuggerError>>,
    },
    Click {
        selector: String,
        response: oneshot::Sender<Result<(), TargetDebuggerError>>,
    },
    Key {
        chord: String,
        response: oneshot::Sender<Result<(), TargetDebuggerError>>,
    },
    TypeText {
        text: String,
        response: oneshot::Sender<Result<(), TargetDebuggerError>>,
    },
    CaptureScreenshot {
        response: oneshot::Sender<Result<ScreenshotSnapshot, TargetDebuggerError>>,
    },
    TakeHeapSnapshot {
        path: String,
        capture_numeric_value: bool,
        expose_internals: bool,
        response: oneshot::Sender<Result<HeapSnapshotResult, TargetDebuggerError>>,
    },
    CaptureHeapSnapshot {
        capture_id: Option<String>,
        capture_numeric_value: bool,
        expose_internals: bool,
        response: oneshot::Sender<Result<HeapCaptureResult, TargetDebuggerError>>,
    },
    CopyHeapCapture {
        capture_id: String,
        destination: String,
        response: oneshot::Sender<Result<(), TargetDebuggerError>>,
    },
    DeleteStoredCapture {
        capture_id: String,
        response: oneshot::Sender<Result<bool, TargetDebuggerError>>,
    },
    GetHeapClasses {
        capture_id: String,
        filter: Option<String>,
        no_cache: bool,
        response: oneshot::Sender<Result<HeapClassSnapshot, TargetDebuggerError>>,
    },
    SelectPromises {
        capture_id: String,
        state: Option<PromiseState>,
        limit: u32,
        max_preview_length: u32,
        response: oneshot::Sender<Result<PromiseSelectionSnapshot, TargetDebuggerError>>,
    },
    SelectHeapNodes {
        capture_id: String,
        selector: HeapNodeSelector,
        max_string_length: Option<u32>,
        include_dominators: bool,
        response: oneshot::Sender<Result<HeapNodeSelectionSnapshot, TargetDebuggerError>>,
    },
    GetHeapReferences {
        reference: String,
        direction: HeapReferenceDirection,
        edge_policy: HeapEdgePolicy,
        limit: u32,
        max_string_length: Option<u32>,
        response: oneshot::Sender<Result<HeapReferencesSnapshot, TargetDebuggerError>>,
    },
    GetHeapPath {
        from: String,
        to: String,
        options: HeapPathOptions,
        max_string_length: Option<u32>,
        response: oneshot::Sender<Result<Option<HeapPathSnapshot>, TargetDebuggerError>>,
    },
    GetHeapDominatorChain {
        reference: String,
        max_string_length: Option<u32>,
        response: oneshot::Sender<Result<HeapDominatorSnapshot, TargetDebuggerError>>,
    },
    AggregateHeapSnapshot {
        capture_id: String,
        by: HeapAggregateBy,
        limit: u32,
        max_string_length: Option<u32>,
        response: oneshot::Sender<Result<HeapAggregateSnapshot, TargetDebuggerError>>,
    },
    DiffHeapSnapshots {
        older_capture_id: String,
        newer_capture_id: String,
        by: HeapAggregateBy,
        limit: u32,
        max_string_length: Option<u32>,
        response: oneshot::Sender<Result<HeapDiffSnapshot, TargetDebuggerError>>,
    },
    StartCoverage {
        response: oneshot::Sender<Result<(), TargetDebuggerError>>,
    },
    TakeCoverage {
        capture_id: Option<String>,
        exclude_capture_id: Option<String>,
        response: oneshot::Sender<Result<CoverageSnapshot, TargetDebuggerError>>,
    },
    StopCoverage {
        exclude_capture_id: Option<String>,
        response: oneshot::Sender<Result<CoverageSnapshot, TargetDebuggerError>>,
    },
    FinishCoverage {
        exclude_capture_id: Option<String>,
        response: oneshot::Sender<Result<(), TargetDebuggerError>>,
    },
    GetCoverage {
        capture_id: String,
        source_path: Option<String>,
        no_cache: bool,
        response: oneshot::Sender<Result<CoverageSnapshot, TargetDebuggerError>>,
    },
    StartCpuProfile {
        sampling_interval_micros: Option<u64>,
        response: oneshot::Sender<Result<(), TargetDebuggerError>>,
    },
    StopCpuProfile {
        capture_id: Option<String>,
        response: oneshot::Sender<Result<CpuProfileSnapshot, TargetDebuggerError>>,
    },
    GetCpuProfile {
        capture_id: String,
        source_path: Option<String>,
        no_cache: bool,
        project: bool,
        response: oneshot::Sender<Result<CpuProfileSnapshot, TargetDebuggerError>>,
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
    pause_events: broadcast::Sender<TargetDebuggerSnapshot>,
) {
    let mut breakpoint_revisions = BTreeMap::<String, u64>::new();
    let mut coverage = None::<CoverageRecording>;
    let mut coverage_objects = BTreeMap::<String, CoverageSnapshot>::new();
    let mut completed_recordings = BTreeMap::<String, CoverageRecording>::new();
    let mut cpu_profile = None::<CpuProfileRecording>;
    let mut cpu_profiles = BTreeMap::<String, CpuProfileSnapshot>::new();
    let mut heap_captures = BTreeMap::<String, StoredHeapCapture>::new();
    let mut heap_constructor_groups = BTreeMap::<String, Arc<Vec<HeapConstructorGroup>>>::new();
    let mut heap_graphs = BTreeMap::<String, Arc<HeapGraph>>::new();
    let mut heap_aliases = BTreeMap::<(String, String), String>::new();
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
            Next::Command(Some(TargetCommand::SetBreakpoints {
                context_revision,
                breakpoints,
                response,
            })) => {
                let result = async {
                    let mut applied = Vec::new();
                    for breakpoint in breakpoints {
                        if breakpoint_revisions
                            .get(&breakpoint.id)
                            .is_some_and(|current| *current > context_revision)
                        {
                            continue;
                        }
                        let previous_revision = breakpoint_revisions.get(&breakpoint.id).copied();
                        let previous = breakpoint_spec(
                            driver.state(),
                            &BreakpointKey {
                                client_id: context_id.clone(),
                                breakpoint_id: breakpoint.id.clone(),
                            },
                        );
                        breakpoint_revisions.insert(breakpoint.id.clone(), context_revision);
                        let breakpoint_id = breakpoint.id.clone();
                        applied.push((breakpoint_id.clone(), previous, previous_revision));
                        if let Err(install) =
                            apply_breakpoint(&mut driver, &context_id, breakpoint).await
                        {
                            let mut rollback_failures = Vec::new();
                            for (applied_id, prior, prior_revision) in applied.into_iter().rev() {
                                let rollback = match prior {
                                    Some(prior) => {
                                        apply_breakpoint(&mut driver, &context_id, prior).await
                                    }
                                    None => {
                                        remove_breakpoint(&mut driver, &context_id, &applied_id)
                                            .await
                                    }
                                };
                                match prior_revision {
                                    Some(revision) => {
                                        breakpoint_revisions.insert(applied_id.clone(), revision);
                                    }
                                    None => {
                                        breakpoint_revisions.remove(&applied_id);
                                    }
                                }
                                if let Err(rollback) = rollback {
                                    rollback_failures.push(format!("{applied_id}: {rollback}"));
                                }
                            }
                            match previous_revision {
                                Some(revision) => {
                                    breakpoint_revisions.insert(breakpoint_id, revision);
                                }
                                None => {
                                    breakpoint_revisions.remove(&breakpoint_id);
                                }
                            }
                            return if rollback_failures.is_empty() {
                                Err(install)
                            } else {
                                Err(TargetDebuggerError::BatchRollback {
                                    install: install.to_string(),
                                    rollback: rollback_failures.join("; "),
                                })
                            };
                        }
                    }
                    Ok(snapshot_from_driver(
                        &context_id,
                        &connection_id,
                        &target_id,
                        connection_generation,
                        &session_key,
                        &driver,
                    ))
                }
                .await;
                if let Ok(snapshot) = &result {
                    publish_snapshot(&snapshots, &pause_events, snapshot.clone());
                }
                let _ = response.send(result);
            }
            Next::Command(Some(TargetCommand::RawCdpRequest {
                method,
                params,
                response,
            })) => {
                let result =
                    tokio::time::timeout(RAW_CDP_TIMEOUT, driver.raw_cdp_request(&method, params))
                        .await
                        .map_err(|_| {
                            hubrpc::prelude::JsonRpcError::new(
                                hubrpc::prelude::error_codes::REQUEST_TIMEOUT,
                                "raw CDP request timed out after 30 seconds",
                            )
                        })
                        .and_then(|result| result);
                let _ = response.send(result);
            }
            Next::Command(Some(TargetCommand::RemoveBreakpoint {
                context_revision,
                breakpoint_id,
                response,
            })) => {
                let result = async {
                    if breakpoint_revisions
                        .get(&breakpoint_id)
                        .is_none_or(|current| *current <= context_revision)
                    {
                        remove_breakpoint(&mut driver, &context_id, &breakpoint_id).await?;
                        breakpoint_revisions.insert(breakpoint_id, context_revision);
                    }
                    Ok(snapshot_from_driver(
                        &context_id,
                        &connection_id,
                        &target_id,
                        connection_generation,
                        &session_key,
                        &driver,
                    ))
                }
                .await;
                if let Ok(snapshot) = &result {
                    publish_snapshot(&snapshots, &pause_events, snapshot.clone());
                }
                let _ = response.send(result);
            }
            Next::Command(Some(TargetCommand::ReleaseIfWaiting { response })) => {
                let result = release_waiting_target(&mut driver, &session_key)
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
                    publish_snapshot(&snapshots, &pause_events, snapshot.clone());
                }
                let _ = response.send(result);
            }
            Next::Command(Some(TargetCommand::SourceContent { path, response })) => {
                let result = driver.state().scripts.iter().find_map(|(key, script)| {
                    if script.url == path {
                        return driver.generated_source_content(key).map(|content| {
                            SourceContentSnapshot {
                                path: path.clone(),
                                content: content.to_string(),
                                start_line: 1,
                                end_line: content.lines().count() as u32,
                                total_lines: content.lines().count() as u32,
                            }
                        });
                    }
                    driver
                        .logical_source_content(key, &path)
                        .map(|content| SourceContentSnapshot {
                            path: path.clone(),
                            content: content.to_string(),
                            start_line: 1,
                            end_line: content.lines().count() as u32,
                            total_lines: content.lines().count() as u32,
                        })
                });
                let _ = response.send(Ok(result));
            }
            Next::Command(Some(TargetCommand::ResolvedSourcePaths { response })) => {
                let paths = driver.source_effects().resolved_source_paths();
                let _ = response.send(Ok(paths));
            }
            Next::Command(Some(TargetCommand::SourceSearchBatch {
                path_selector,
                control,
                response,
            })) => {
                complete_source_search_batch(response, &control, || {
                    driver.source_effects().search_source_batch(
                        driver.state(),
                        path_selector.as_deref(),
                        &control,
                    )
                });
            }
            Next::Command(Some(TargetCommand::ExplainSource { path, response })) => {
                let explanations = driver.source_effects().explain_source(&path);
                let _ = response.send(Ok(explanations));
            }
            Next::Command(Some(TargetCommand::MapSource {
                path,
                line,
                column,
                response,
            })) => {
                let position = Position {
                    line: line.saturating_sub(1),
                    column: column.saturating_sub(1),
                };
                let locations = driver
                    .source_effects()
                    .map_source_position(&path, position)
                    .into_iter()
                    .map(
                        |(source_url, position, direction, quality)| SourceMappingSnapshot {
                            connection_id: String::new(),
                            target_id: String::new(),
                            source_url,
                            line: position.line + 1,
                            column: position.column + 1,
                            direction,
                            quality,
                        },
                    )
                    .collect();
                let _ = response.send(Ok(locations));
            }
            Next::Command(Some(TargetCommand::EvictSourceCaches { response })) => {
                driver.clear_source_caches();
                let _ = response.send(Ok(()));
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
                    publish_snapshot(&snapshots, &pause_events, snapshot.clone());
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
                    publish_snapshot(&snapshots, &pause_events, snapshot.clone());
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
            Next::Command(Some(TargetCommand::ScopeVariables {
                pause_epoch,
                frame_index,
                scope_index,
                response,
            })) => {
                let result =
                    scope_variables(&driver, &session_key, pause_epoch, frame_index, scope_index)
                        .await;
                let _ = response.send(result);
            }
            Next::Command(Some(TargetCommand::ObjectProperties {
                pause_epoch,
                object_id,
                response,
            })) => {
                let result = object_properties(&driver, &session_key, pause_epoch, object_id).await;
                let _ = response.send(result);
            }
            Next::Command(Some(TargetCommand::InspectValue {
                pause_epoch,
                selector,
                options,
                response,
            })) => {
                let object_group =
                    (!options.retain_references).then(|| "jsdbg-ephemeral-value".to_owned());
                let result = inspect_value(
                    &driver,
                    &session_key,
                    pause_epoch,
                    selector,
                    &options,
                    object_group.as_deref(),
                )
                .await;
                let result = if let Some(object_group) = object_group {
                    let release = driver
                        .client()
                        .runtime_release_object_group(RuntimeReleaseObjectGroupParams {
                            object_group,
                        })
                        .await
                        .map_err(|error| {
                            TargetDebuggerError::Properties(format!(
                                "failed to release ephemeral evaluation values: {error:?}"
                            ))
                        });
                    match (result, release) {
                        (Ok(value), Ok(_)) => Ok(value.without_references()),
                        (Ok(_), Err(error)) => Err(error),
                        (Err(error), _) => Err(error),
                    }
                } else {
                    result
                };
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
                                publish_snapshot(
                                    &snapshots,
                                    &pause_events,
                                    snapshot_from_driver(
                                        &context_id,
                                        &connection_id,
                                        &target_id,
                                        connection_generation,
                                        &session_key,
                                        &driver,
                                    ),
                                );
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
            Next::Command(Some(TargetCommand::Key { chord, response })) => {
                let _ = response.send(key(&driver, &chord).await);
            }
            Next::Command(Some(TargetCommand::TypeText { text, response })) => {
                let prior_epoch = driver
                    .state()
                    .sessions
                    .get(&session_key)
                    .map_or(0, |session| session.next_pause_epoch.saturating_sub(1));
                let mut dispatch = begin_type_text(&driver, text);
                let result = loop {
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
                            publish_snapshot(
                                &snapshots,
                                &pause_events,
                                snapshot_from_driver(
                                    &context_id,
                                    &connection_id,
                                    &target_id,
                                    connection_generation,
                                    &session_key,
                                    &driver,
                                ),
                            );
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
                };
                let _ = response.send(result);
            }
            Next::Command(Some(TargetCommand::CaptureScreenshot { response })) => {
                let mut params = PageCaptureScreenshotParams::new();
                params.format = Some(PageCaptureScreenshotParamsFormat::Png);
                params.from_surface = Some(true);
                params.capture_beyond_viewport = Some(false);
                let result = driver
                    .client()
                    .page_capture_screenshot(params)
                    .await
                    .map(|result| ScreenshotSnapshot {
                        media_type: "image/png".to_owned(),
                        data_base64: result.data,
                    })
                    .map_err(|error| TargetDebuggerError::Screenshot(format!("{error:?}")));
                let _ = response.send(result);
            }
            Next::Command(Some(TargetCommand::StartCoverage { response })) => {
                let result = if coverage.is_some() {
                    Err(TargetDebuggerError::CoverageAlreadyActive)
                } else {
                    start_coverage(&mut driver, &session_key).await.map(|()| {
                        coverage = Some(CoverageRecording::default());
                    })
                };
                let _ = response.send(result);
            }
            Next::Command(Some(TargetCommand::TakeCoverage {
                capture_id,
                exclude_capture_id,
                response,
            })) => {
                let result = match coverage.as_mut() {
                    Some(recording)
                        if capture_id.as_ref().is_some_and(|capture_id| {
                            recording.captures.contains_key(capture_id)
                        }) =>
                    {
                        Err(TargetDebuggerError::CoverageCaptureAlreadyExists(
                            capture_id.unwrap(),
                        ))
                    }
                    Some(recording) => {
                        let snapshot = take_coverage(&driver, recording).await;
                        let snapshot = snapshot.and_then(|snapshot| match exclude_capture_id {
                            Some(capture_id) => recording
                                .captures
                                .get(&capture_id)
                                .map(|baseline| {
                                    CoverageRecording::exclude_coverage(snapshot, baseline)
                                })
                                .ok_or(TargetDebuggerError::CoverageCaptureNotFound(capture_id)),
                            None => Ok(snapshot),
                        });
                        let mut snapshot = snapshot;
                        if capture_id.is_none()
                            && let Ok(snapshot) = &mut snapshot
                            && let Err(error) =
                                project_coverage(&mut driver, &session_key, snapshot, None, false)
                                    .await
                        {
                            snapshot.sources.clear();
                            let _ = response.send(Err(error));
                            continue;
                        }
                        if let (Ok(snapshot), Some(capture_id)) = (&snapshot, capture_id) {
                            recording.captures.insert(capture_id, snapshot.clone());
                        }
                        snapshot
                    }
                    None => Err(TargetDebuggerError::CoverageNotActive),
                };
                let _ = response.send(result);
            }
            Next::Command(Some(TargetCommand::StopCoverage {
                exclude_capture_id,
                response,
            })) => {
                let result = async {
                    let recording = coverage
                        .as_mut()
                        .ok_or(TargetDebuggerError::CoverageNotActive)?;
                    let snapshot = take_coverage(&driver, recording).await?;
                    let snapshot = match exclude_capture_id {
                        Some(capture_id) => {
                            let baseline =
                                recording.captures.get(&capture_id).ok_or_else(|| {
                                    TargetDebuggerError::CoverageCaptureNotFound(capture_id.clone())
                                })?;
                            CoverageRecording::exclude_coverage(snapshot, baseline)
                        }
                        None => snapshot,
                    };
                    let stored = snapshot.clone();
                    let mut snapshot = snapshot;
                    project_coverage(&mut driver, &session_key, &mut snapshot, None, false).await?;
                    driver
                        .client()
                        .profiler_stop_precise_coverage(ProfilerStopPreciseCoverageParams::new())
                        .await
                        .map_err(|error| TargetDebuggerError::Coverage(format!("{error:?}")))?;
                    coverage = None;
                    Ok((snapshot, stored))
                }
                .await;
                if let Ok((_, stored)) = &result {
                    coverage_objects.insert(".".to_owned(), stored.clone());
                }
                let _ = response.send(result.map(|(snapshot, _)| snapshot));
            }
            Next::Command(Some(TargetCommand::FinishCoverage {
                exclude_capture_id,
                response,
            })) => {
                let result = async {
                    let recording = coverage
                        .as_mut()
                        .ok_or(TargetDebuggerError::CoverageNotActive)?;
                    update_coverage(&driver, recording).await?;
                    let mut completed = recording.clone();
                    if let Some(capture_id) = exclude_capture_id {
                        let baseline = recording
                            .captures
                            .get(&capture_id)
                            .cloned()
                            .ok_or(TargetDebuggerError::CoverageCaptureNotFound(capture_id))?;
                        completed.exclude_baseline(&baseline);
                    }
                    driver
                        .client()
                        .profiler_stop_precise_coverage(ProfilerStopPreciseCoverageParams::new())
                        .await
                        .map_err(|error| TargetDebuggerError::Coverage(format!("{error:?}")))?;
                    completed_recordings.insert(".".to_owned(), completed);
                    coverage = None;
                    Ok(())
                }
                .await;
                let _ = response.send(result);
            }
            Next::Command(Some(TargetCommand::GetCoverage {
                capture_id,
                source_path,
                no_cache,
                response,
            })) => {
                let result = async {
                    let mut snapshot = match completed_recordings.get(&capture_id) {
                        Some(recording) => recording.snapshot(),
                        None => coverage_objects.get(&capture_id).cloned().ok_or_else(|| {
                            TargetDebuggerError::CoverageCaptureNotFound(capture_id.clone())
                        })?,
                    };
                    let started = Instant::now();
                    let cache_before = driver.source_map_cache_stats();
                    project_coverage(
                        &mut driver,
                        &session_key,
                        &mut snapshot,
                        source_path.as_deref(),
                        no_cache,
                    )
                    .await?;
                    let cache_after = driver.source_map_cache_stats();
                    snapshot.analysis = Some(CoverageAnalysisSnapshot {
                        duration_micros: started.elapsed().as_micros() as u64,
                        source_map_cache_hits: cache_after.hits.saturating_sub(cache_before.hits),
                        source_map_cache_misses: cache_after
                            .misses
                            .saturating_sub(cache_before.misses),
                        source_map_cache_bypasses: cache_after
                            .bypasses
                            .saturating_sub(cache_before.bypasses),
                    });
                    Ok(snapshot)
                }
                .await;
                let _ = response.send(result);
            }
            Next::Command(Some(TargetCommand::StartCpuProfile {
                sampling_interval_micros,
                response,
            })) => {
                let result = if cpu_profile.is_some() {
                    Err(TargetDebuggerError::CpuProfileAlreadyActive)
                } else {
                    start_cpu_profile(&driver, sampling_interval_micros)
                        .await
                        .map(|()| {
                            cpu_profile = Some(CpuProfileRecording {
                                sampling_interval_micros,
                            });
                        })
                };
                let _ = response.send(result);
            }
            Next::Command(Some(TargetCommand::StopCpuProfile {
                capture_id,
                response,
            })) => {
                let result = async {
                    let recording = cpu_profile.ok_or(TargetDebuggerError::CpuProfileNotActive)?;
                    let capture_id = capture_id.unwrap_or_else(|| ".".to_owned());
                    if capture_id != "." && cpu_profiles.contains_key(&capture_id) {
                        return Err(TargetDebuggerError::CpuProfileCaptureAlreadyExists(
                            capture_id,
                        ));
                    }
                    let stopped = driver
                        .client()
                        .profiler_stop(ProfilerStopParams::new())
                        .await
                        .map_err(|error| TargetDebuggerError::CpuProfile(format!("{error:?}")))?;
                    cpu_profile = None;
                    let snapshot = cpu_profile_snapshot(
                        capture_id.clone(),
                        recording.sampling_interval_micros,
                        stopped.profile,
                    )?;
                    cpu_profiles.insert(capture_id.clone(), snapshot.clone());
                    if capture_id != "." {
                        let mut latest = snapshot.clone();
                        latest.capture_id = ".".to_owned();
                        cpu_profiles.insert(".".to_owned(), latest);
                    }
                    Ok(snapshot)
                }
                .await;
                let _ = response.send(result);
            }
            Next::Command(Some(TargetCommand::GetCpuProfile {
                capture_id,
                source_path,
                no_cache,
                project,
                response,
            })) => {
                let result = async {
                    let mut snapshot = cpu_profiles.get(&capture_id).cloned().ok_or_else(|| {
                        TargetDebuggerError::CpuProfileCaptureNotFound(capture_id.clone())
                    })?;
                    if !project {
                        return Ok(snapshot);
                    }
                    let started = Instant::now();
                    let cache_before = driver.source_map_cache_stats();
                    project_cpu_profile(
                        &mut driver,
                        &session_key,
                        &mut snapshot,
                        source_path.as_deref(),
                        no_cache,
                    )
                    .await?;
                    let cache_after = driver.source_map_cache_stats();
                    snapshot.analysis = Some(CpuProfileAnalysisSnapshot {
                        duration_micros: started.elapsed().as_micros() as u64,
                        source_map_cache_hits: cache_after.hits.saturating_sub(cache_before.hits),
                        source_map_cache_misses: cache_after
                            .misses
                            .saturating_sub(cache_before.misses),
                        source_map_cache_bypasses: cache_after
                            .bypasses
                            .saturating_sub(cache_before.bypasses),
                    });
                    Ok(snapshot)
                }
                .await;
                let _ = response.send(result);
            }
            Next::Command(Some(TargetCommand::TakeHeapSnapshot {
                path,
                capture_numeric_value,
                expose_internals,
                response,
            })) => {
                let result = async {
                    driver
                        .begin_heap_snapshot(PathBuf::from(&path))
                        .await
                        .map_err(|error| TargetDebuggerError::HeapSnapshot(error.to_string()))?;
                    let mut params = HeapProfilerTakeHeapSnapshotParams::new();
                    params.report_progress = Some(true);
                    params.capture_numeric_value = capture_numeric_value.then_some(true);
                    params.expose_internals = expose_internals.then_some(true);
                    if let Err(error) = driver
                        .client()
                        .heap_profiler_take_heap_snapshot(params)
                        .await
                    {
                        driver.abort_heap_snapshot().await;
                        return Err(TargetDebuggerError::HeapSnapshot(format!("{error:?}")));
                    }
                    let written = driver
                        .finish_heap_snapshot()
                        .await
                        .map_err(|error| TargetDebuggerError::HeapSnapshot(error.to_string()))?;
                    Ok(HeapSnapshotResult {
                        path,
                        bytes_written: written.bytes_written,
                        timing: heap_snapshot_timing(&written),
                    })
                }
                .await;
                let _ = response.send(result);
            }
            Next::Command(Some(TargetCommand::CaptureHeapSnapshot {
                capture_id,
                capture_numeric_value,
                expose_internals,
                response,
            })) => {
                let capture_id = capture_id.unwrap_or_else(|| ".".to_owned());
                let path = temporary_heap_snapshot_path();
                let result = take_heap_snapshot(
                    &driver,
                    path.clone(),
                    capture_numeric_value,
                    expose_internals,
                )
                .await
                .map(|written| HeapCaptureResult {
                    capture_id: capture_id.clone(),
                    bytes_written: written.bytes_written,
                    timing: heap_snapshot_timing(&written),
                });
                if let Ok(capture) = &result {
                    let timing = capture.timing.clone();
                    if let Some(previous) =
                        heap_captures.insert(capture_id.clone(), StoredHeapCapture { path, timing })
                    {
                        let _ = tokio::fs::remove_file(previous.path).await;
                    }
                    heap_constructor_groups.remove(&capture_id);
                    heap_graphs.remove(&capture_id);
                    heap_aliases.retain(|(stored_capture, _), _| stored_capture != &capture_id);
                }
                let _ = response.send(result);
            }
            Next::Command(Some(TargetCommand::CopyHeapCapture {
                capture_id,
                destination,
                response,
            })) => {
                let result = async {
                    let capture = heap_captures.get(&capture_id).ok_or_else(|| {
                        TargetDebuggerError::HeapCaptureNotFound(capture_id.clone())
                    })?;
                    if let Some(parent) = Path::new(&destination).parent() {
                        tokio::fs::create_dir_all(parent).await.map_err(|error| {
                            TargetDebuggerError::HeapSnapshot(error.to_string())
                        })?;
                    }
                    let mut source = tokio::fs::File::open(&capture.path)
                        .await
                        .map_err(|error| TargetDebuggerError::HeapSnapshot(error.to_string()))?;
                    let mut output = tokio::fs::OpenOptions::new()
                        .write(true)
                        .create_new(true)
                        .open(&destination)
                        .await
                        .map_err(|error| TargetDebuggerError::HeapSnapshot(error.to_string()))?;
                    tokio::io::copy(&mut source, &mut output)
                        .await
                        .map_err(|error| TargetDebuggerError::HeapSnapshot(error.to_string()))?;
                    Ok(())
                }
                .await;
                let _ = response.send(result);
            }
            Next::Command(Some(TargetCommand::DeleteStoredCapture {
                capture_id,
                response,
            })) => {
                let result = async {
                    if let Some(capture) = heap_captures.get(&capture_id) {
                        match tokio::fs::remove_file(&capture.path).await {
                            Ok(()) => {}
                            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
                            Err(error) => {
                                return Err(TargetDebuggerError::HeapSnapshot(format!(
                                    "failed to delete stored heap capture '{}': {error}",
                                    capture.path.display()
                                )));
                            }
                        }
                    }
                    let mut removed = coverage
                        .as_mut()
                        .and_then(|recording| recording.captures.remove(&capture_id))
                        .is_some();
                    removed |= cpu_profiles.remove(&capture_id).is_some();
                    removed |= heap_captures.remove(&capture_id).is_some();
                    heap_constructor_groups.remove(&capture_id);
                    heap_graphs.remove(&capture_id);
                    heap_aliases.retain(|(stored_capture, _), _| stored_capture != &capture_id);
                    Ok(removed)
                }
                .await;
                let _ = response.send(result);
            }
            Next::Command(Some(TargetCommand::GetHeapClasses {
                capture_id,
                filter,
                no_cache,
                response,
            })) => {
                let result = async {
                    let parse_started = Instant::now();
                    let filter = filter
                        .as_deref()
                        .map(regex::Regex::new)
                        .transpose()
                        .map_err(|error| {
                            TargetDebuggerError::InvalidHeapFilter(error.to_string())
                        })?;
                    let (groups, used_cached_groups) =
                        match heap_constructor_groups.get(&capture_id).cloned() {
                            Some(groups) => (groups, true),
                            None => {
                                let capture =
                                    heap_captures.get(&capture_id).cloned().ok_or_else(|| {
                                        TargetDebuggerError::HeapCaptureNotFound(capture_id.clone())
                                    })?;
                                let path = capture.path;
                                let groups = Arc::new(
                                    tokio::task::spawn_blocking(move || {
                                        let file = File::open(path)?;
                                        parse_constructor_groups(file)
                                            .map_err(std::io::Error::other)
                                    })
                                    .await
                                    .map_err(|error| {
                                        TargetDebuggerError::HeapSnapshot(error.to_string())
                                    })?
                                    .map_err(|error| {
                                        TargetDebuggerError::HeapSnapshot(error.to_string())
                                    })?,
                                );
                                heap_constructor_groups.insert(capture_id.clone(), groups.clone());
                                (groups, false)
                            }
                        };
                    let parse_duration = parse_started.elapsed();
                    let projection_started = Instant::now();
                    let (mut snapshot, source_map_hydration_duration) = project_heap_classes(
                        &mut driver,
                        &session_key,
                        capture_id.clone(),
                        &groups,
                        filter.as_ref(),
                        no_cache,
                    )
                    .await?;
                    snapshot.analysis = HeapClassAnalysisSnapshot {
                        snapshot_timing: heap_captures
                            .get(&capture_id)
                            .map(|capture| capture.timing.clone()),
                        parse_duration_micros: parse_duration.as_micros() as u64,
                        projection_duration_micros: projection_started.elapsed().as_micros() as u64,
                        source_map_hydration_duration_micros: source_map_hydration_duration
                            .as_micros()
                            as u64,
                        constructor_group_count: groups.len() as u64,
                        used_cached_groups,
                    };
                    heap_aliases.retain(|(stored_capture, _), _| stored_capture != &capture_id);
                    for class in &snapshot.classes {
                        for instance in &class.instances {
                            heap_aliases.insert(
                                (capture_id.clone(), instance.alias.clone()),
                                instance.heap_object_id.clone(),
                            );
                        }
                    }
                    Ok(snapshot)
                }
                .await;
                let _ = response.send(result);
            }
            Next::Command(Some(TargetCommand::SelectPromises {
                capture_id,
                state,
                limit,
                max_preview_length,
                response,
            })) => {
                let result = async {
                    let (graph, graph_parse_duration, used_cached_graph) =
                        load_heap_graph(&capture_id, &heap_captures, &mut heap_graphs).await?;
                    let (promises, total_promises) = inspect_heap_promises(
                        &graph,
                        &capture_id,
                        state,
                        limit,
                        max_preview_length,
                    )
                    .map_err(heap_analysis_error)?;
                    Ok(PromiseSelectionSnapshot {
                        capture_id,
                        omitted_promise_count: total_promises.saturating_sub(promises.len() as u64),
                        total_promises,
                        promises,
                        graph_parse_duration_micros: graph_parse_duration.as_micros() as u64,
                        used_cached_graph,
                    })
                }
                .await;
                let _ = response.send(result);
            }
            Next::Command(Some(TargetCommand::SelectHeapNodes {
                capture_id,
                selector,
                max_string_length,
                include_dominators,
                response,
            })) => {
                let result = async {
                    let (graph, graph_parse_duration, used_cached_graph) =
                        load_heap_graph(&capture_id, &heap_captures, &mut heap_graphs).await?;
                    let heap_object_id = selector
                        .heap_object_id
                        .as_deref()
                        .map(parse_heap_object_id)
                        .transpose()?;
                    if selector.name.is_some() && selector.name_regex.is_some() {
                        return Err(TargetDebuggerError::InvalidHeapSelector(
                            "--name and --name-regex are mutually exclusive".to_owned(),
                        ));
                    }
                    if selector.string_contains.is_some() && selector.string_regex.is_some() {
                        return Err(TargetDebuggerError::InvalidHeapSelector(
                            "stringContains and stringRegex are mutually exclusive".to_owned(),
                        ));
                    }
                    let name_regex = selector
                        .name_regex
                        .as_deref()
                        .map(regex::Regex::new)
                        .transpose()
                        .map_err(|error| {
                            TargetDebuggerError::InvalidHeapSelector(error.to_string())
                        })?;
                    let string_regex = selector
                        .string_regex
                        .as_deref()
                        .map(regex::Regex::new)
                        .transpose()
                        .map_err(|error| {
                            TargetDebuggerError::InvalidHeapSelector(error.to_string())
                        })?;
                    let mut graph_selector = NodeSelector::new();
                    if let Some(heap_object_id) = heap_object_id {
                        graph_selector = graph_selector.heap_object_id(heap_object_id);
                    }
                    if let Some(node_type) = selector.node_type.as_deref() {
                        graph_selector = graph_selector.node_type(node_type);
                    }
                    if let Some(name) = selector.name.as_deref() {
                        graph_selector = graph_selector.raw_name(TextMatcher::Exact(name));
                    } else if let Some(regex) = name_regex.as_ref() {
                        graph_selector = graph_selector.raw_name(TextMatcher::Regex(regex));
                    }
                    if let Some(value) = selector.string_contains.as_deref() {
                        graph_selector = graph_selector.string_value(TextMatcher::Contains(value));
                    } else if let Some(regex) = string_regex.as_ref() {
                        graph_selector = graph_selector.string_value(TextMatcher::Regex(regex));
                    }
                    if let Some(size) = selector.min_shallow_size {
                        graph_selector = graph_selector.min_shallow_size(size);
                    }
                    if let Some(size) = selector.max_shallow_size {
                        graph_selector = graph_selector.max_shallow_size(size);
                    }
                    if let Some(limit) = selector.limit {
                        graph_selector = graph_selector.limit(limit as usize);
                    }
                    let selection = graph.select_with_stats(&graph_selector);
                    let dominators = include_dominators
                        .then(|| graph.dominators())
                        .transpose()
                        .map_err(heap_analysis_error)?;
                    let nodes = selection
                        .nodes
                        .into_iter()
                        .map(|node| {
                            heap_node_snapshot(
                                &graph,
                                &capture_id,
                                node,
                                max_string_length,
                                dominators,
                            )
                        })
                        .collect::<Result<Vec<_>, _>>()?;
                    Ok(HeapNodeSelectionSnapshot {
                        capture_id,
                        total_nodes: graph.node_count() as u64,
                        total_edges: graph.edge_count() as u64,
                        nodes,
                        incomplete_string_count: selection.incomplete_string_count,
                        graph_parse_duration_micros: graph_parse_duration.as_micros() as u64,
                        used_cached_graph,
                    })
                }
                .await;
                let _ = response.send(result);
            }
            Next::Command(Some(TargetCommand::GetHeapReferences {
                reference,
                direction,
                edge_policy,
                limit,
                max_string_length,
                response,
            })) => {
                let result = async {
                    let (capture_id, heap_object_id) = parse_heap_reference(&reference)?;
                    let (graph, _, _) =
                        load_heap_graph(&capture_id, &heap_captures, &mut heap_graphs).await?;
                    let node = heap_node_by_id(&graph, heap_object_id)?;
                    let mut references = Vec::new();
                    if matches!(
                        direction,
                        HeapReferenceDirection::Outgoing | HeapReferenceDirection::Both
                    ) {
                        references.extend(
                            graph
                                .outgoing_references(node)
                                .map_err(heap_analysis_error)?
                                .filter(|reference| {
                                    edge_policy == HeapEdgePolicy::All
                                        || reference.edge_type != "weak"
                                })
                                .map(|reference| {
                                    heap_reference_snapshot(&graph, &capture_id, reference)
                                })
                                .collect::<Result<Vec<_>, _>>()?,
                        );
                    }
                    if matches!(
                        direction,
                        HeapReferenceDirection::Incoming | HeapReferenceDirection::Both
                    ) {
                        references.extend(
                            graph
                                .incoming_references(node)
                                .map_err(heap_analysis_error)?
                                .filter(|reference| {
                                    edge_policy == HeapEdgePolicy::All
                                        || reference.edge_type != "weak"
                                })
                                .map(|reference| {
                                    heap_reference_snapshot(&graph, &capture_id, reference)
                                })
                                .collect::<Result<Vec<_>, _>>()?,
                        );
                    }
                    references.sort_by_key(|reference| reference.edge_index);
                    let omitted_reference_count =
                        references.len().saturating_sub(limit as usize) as u64;
                    references.truncate(limit as usize);
                    Ok(HeapReferencesSnapshot {
                        capture_id: capture_id.clone(),
                        node: heap_node_snapshot(
                            &graph,
                            &capture_id,
                            node,
                            max_string_length,
                            None,
                        )?,
                        direction,
                        edge_policy,
                        references,
                        omitted_reference_count,
                    })
                }
                .await;
                let _ = response.send(result);
            }
            Next::Command(Some(TargetCommand::GetHeapPath {
                from,
                to,
                options,
                max_string_length,
                response,
            })) => {
                let result = async {
                    let (from_capture, from_id) = parse_heap_reference(&from)?;
                    let (to_capture, to_id) = parse_heap_reference(&to)?;
                    if from_capture != to_capture {
                        return Err(TargetDebuggerError::IncompatibleHeapCaptures {
                            older: from_capture,
                            newer: to_capture,
                        });
                    }
                    let capture_id = from_capture;
                    let (graph, _, _) =
                        load_heap_graph(&capture_id, &heap_captures, &mut heap_graphs).await?;
                    let from_node = heap_node_by_id(&graph, from_id)?;
                    let to_node = heap_node_by_id(&graph, to_id)?;
                    let path = graph
                        .shortest_path(
                            from_node,
                            to_node,
                            PathOptions {
                                direction: heap_path_direction(options.direction),
                                edge_policy: heap_edge_policy(options.edge_policy),
                                cost: heap_path_cost(options.cost),
                            },
                        )
                        .map_err(heap_analysis_error)?;
                    path.map(|path| {
                        let nodes = path
                            .nodes
                            .iter()
                            .copied()
                            .map(|node| {
                                heap_node_snapshot(
                                    &graph,
                                    &capture_id,
                                    node,
                                    max_string_length,
                                    None,
                                )
                            })
                            .collect::<Result<Vec<_>, _>>()?;
                        let steps = path
                            .steps
                            .iter()
                            .map(|step| heap_path_step_snapshot(&graph, &capture_id, *step))
                            .collect::<Result<Vec<_>, _>>()?;
                        Ok(HeapPathSnapshot {
                            capture_id,
                            from,
                            to,
                            cost: path.cost,
                            nodes,
                            steps,
                        })
                    })
                    .transpose()
                }
                .await;
                let _ = response.send(result);
            }
            Next::Command(Some(TargetCommand::GetHeapDominatorChain {
                reference,
                max_string_length,
                response,
            })) => {
                let result = async {
                    let (capture_id, heap_object_id) = parse_heap_reference(&reference)?;
                    let (graph, _, _) =
                        load_heap_graph(&capture_id, &heap_captures, &mut heap_graphs).await?;
                    let node = heap_node_by_id(&graph, heap_object_id)?;
                    let dominators = graph.dominators().map_err(heap_analysis_error)?;
                    let mut chain = Vec::new();
                    let mut current = node;
                    while let Some(dominator) = dominators.immediate_dominator(current) {
                        chain.push(heap_node_snapshot(
                            &graph,
                            &capture_id,
                            dominator,
                            max_string_length,
                            Some(dominators),
                        )?);
                        current = dominator;
                    }
                    Ok(HeapDominatorSnapshot {
                        capture_id: capture_id.clone(),
                        node: heap_node_snapshot(
                            &graph,
                            &capture_id,
                            node,
                            max_string_length,
                            Some(dominators),
                        )?,
                        chain,
                    })
                }
                .await;
                let _ = response.send(result);
            }
            Next::Command(Some(TargetCommand::AggregateHeapSnapshot {
                capture_id,
                by,
                limit,
                max_string_length,
                response,
            })) => {
                let result = async {
                    let (graph, _, _) =
                        load_heap_graph(&capture_id, &heap_captures, &mut heap_graphs).await?;
                    let aggregate = graph.aggregate(heap_aggregate_by(by));
                    let incomplete_string_count = aggregate.incomplete_string_count;
                    let mut entries = aggregate
                        .groups
                        .into_iter()
                        .filter(|(_, value)| value.count != 0 || value.shallow_size != 0)
                        .map(|(key, value)| {
                            let (key, key_truncated) = bounded_heap_text(&key, max_string_length);
                            Ok(HeapAggregateEntrySnapshot {
                                key,
                                key_truncated,
                                count: value.count,
                                shallow_size: u64::try_from(value.shallow_size).map_err(|_| {
                                    TargetDebuggerError::HeapAnalysis(
                                        "aggregate shallow size exceeds u64".to_owned(),
                                    )
                                })?,
                            })
                        })
                        .collect::<Result<Vec<_>, TargetDebuggerError>>()?;
                    entries
                        .sort_by_key(|entry| std::cmp::Reverse((entry.shallow_size, entry.count)));
                    let omitted_entry_count = entries.len().saturating_sub(limit as usize) as u64;
                    entries.truncate(limit as usize);
                    Ok(HeapAggregateSnapshot {
                        capture_id,
                        by,
                        entries,
                        omitted_entry_count,
                        incomplete_string_count,
                    })
                }
                .await;
                let _ = response.send(result);
            }
            Next::Command(Some(TargetCommand::DiffHeapSnapshots {
                older_capture_id,
                newer_capture_id,
                by,
                limit,
                max_string_length,
                response,
            })) => {
                let result = async {
                    let (older, _, _) =
                        load_heap_graph(&older_capture_id, &heap_captures, &mut heap_graphs)
                            .await?;
                    let (newer, _, _) =
                        load_heap_graph(&newer_capture_id, &heap_captures, &mut heap_graphs)
                            .await?;
                    let diff = older.diff(&newer, heap_aggregate_by(by));
                    let older_incomplete_string_count = diff.older_incomplete_string_count;
                    let newer_incomplete_string_count = diff.newer_incomplete_string_count;
                    let mut entries = diff
                        .groups
                        .into_iter()
                        .filter(|(_, value)| value.count != 0 || value.shallow_size != 0)
                        .map(|(key, value)| {
                            let (key, key_truncated) = bounded_heap_text(&key, max_string_length);
                            Ok(HeapDiffEntrySnapshot {
                                key,
                                key_truncated,
                                count_delta: i64::try_from(value.count).map_err(|_| {
                                    TargetDebuggerError::HeapAnalysis(
                                        "aggregate count delta exceeds i64".to_owned(),
                                    )
                                })?,
                                shallow_size_delta: i64::try_from(value.shallow_size).map_err(
                                    |_| {
                                        TargetDebuggerError::HeapAnalysis(
                                            "aggregate shallow size delta exceeds i64".to_owned(),
                                        )
                                    },
                                )?,
                            })
                        })
                        .collect::<Result<Vec<_>, TargetDebuggerError>>()?;
                    entries.sort_by_key(|entry| {
                        std::cmp::Reverse(entry.shallow_size_delta.unsigned_abs())
                    });
                    entries.truncate(limit as usize);
                    Ok(HeapDiffSnapshot {
                        older_capture_id,
                        newer_capture_id,
                        by,
                        entries,
                        older_incomplete_string_count,
                        newer_incomplete_string_count,
                    })
                }
                .await;
                let _ = response.send(result);
            }
            Next::Command(None) => break,
            Next::Event(Ok(_)) => {
                publish_snapshot(
                    &snapshots,
                    &pause_events,
                    snapshot_from_driver(
                        &context_id,
                        &connection_id,
                        &target_id,
                        connection_generation,
                        &session_key,
                        &driver,
                    ),
                );
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
                publish_snapshot(&snapshots, &pause_events, failed);
                break;
            }
        }
    }
    for capture in heap_captures.into_values() {
        let _ = tokio::fs::remove_file(capture.path).await;
    }
}

fn complete_source_search_batch<T>(
    response: oneshot::Sender<Result<T, TargetDebuggerError>>,
    control: &SearchControl,
    hydrate: impl FnOnce() -> Result<T, SearchError>,
) {
    if response.is_closed() {
        return;
    }
    let result = control
        .check()
        .and_then(|()| hydrate())
        .map_err(TargetDebuggerError::SourceSearch);
    let _ = response.send(result);
}

fn publish_snapshot(
    snapshots: &watch::Sender<TargetDebuggerSnapshot>,
    pause_events: &broadcast::Sender<TargetDebuggerSnapshot>,
    snapshot: TargetDebuggerSnapshot,
) {
    if matches!(snapshot.phase, TargetDebuggerPhase::Paused { .. }) {
        let _ = pause_events.send(snapshot.clone());
    }
    snapshots.send_replace(snapshot);
}

fn temporary_heap_snapshot_path() -> PathBuf {
    static TEMPORARY_ID: AtomicU64 = AtomicU64::new(1);
    let directory = if let Some(state_file) = std::env::var_os("JSDBG_SERVICE_STATE") {
        PathBuf::from(state_file)
            .parent()
            .map(|parent| parent.join("heap-captures"))
            .unwrap_or_else(|| std::env::temp_dir().join("jsdbg-heap-captures"))
    } else if let Some(local_app_data) = std::env::var_os("LOCALAPPDATA") {
        PathBuf::from(local_app_data)
            .join("hediet")
            .join("cdp-client")
            .join("heap-captures")
    } else if let Some(home) = std::env::var_os("HOME") {
        PathBuf::from(home)
            .join(".cache")
            .join("hediet")
            .join("cdp-client")
            .join("heap-captures")
    } else {
        std::env::temp_dir().join(format!("jsdbg-heap-captures-{}", std::process::id()))
    };
    directory.join(format!(
        "jsdbg-heap-{}-{}.heapsnapshot",
        std::process::id(),
        TEMPORARY_ID.fetch_add(1, Ordering::Relaxed)
    ))
}

#[derive(Clone)]
struct StoredHeapCapture {
    path: PathBuf,
    timing: HeapSnapshotTiming,
}

async fn load_heap_graph(
    capture_id: &str,
    captures: &BTreeMap<String, StoredHeapCapture>,
    graphs: &mut BTreeMap<String, Arc<HeapGraph>>,
) -> Result<(Arc<HeapGraph>, Duration, bool), TargetDebuggerError> {
    let started = Instant::now();
    if let Some(graph) = graphs.get(capture_id) {
        return Ok((graph.clone(), started.elapsed(), true));
    }
    let path = captures
        .get(capture_id)
        .ok_or_else(|| TargetDebuggerError::HeapCaptureNotFound(capture_id.to_owned()))?
        .path
        .clone();
    let graph = Arc::new(
        tokio::task::spawn_blocking(move || {
            let file = File::open(path)?;
            parse_heap_graph(file).map_err(std::io::Error::other)
        })
        .await
        .map_err(|error| TargetDebuggerError::HeapSnapshot(error.to_string()))?
        .map_err(|error| TargetDebuggerError::HeapSnapshot(error.to_string()))?,
    );
    let duration = started.elapsed();
    graphs.insert(capture_id.to_owned(), graph.clone());
    Ok((graph, duration, false))
}

fn parse_heap_object_id(value: &str) -> Result<u64, TargetDebuggerError> {
    value.parse().map_err(|error| {
        TargetDebuggerError::InvalidHeapReference(format!(
            "heap object id '{value}' is not an unsigned integer: {error}"
        ))
    })
}

fn parse_heap_reference(reference: &str) -> Result<(String, u64), TargetDebuggerError> {
    let (capture_id, heap_object_id) = reference.rsplit_once('#').ok_or_else(|| {
        TargetDebuggerError::InvalidHeapReference(format!(
            "'{reference}' must use <capture>#<heap-object-id>"
        ))
    })?;
    if capture_id.is_empty() {
        return Err(TargetDebuggerError::InvalidHeapReference(
            "the capture id cannot be empty".to_owned(),
        ));
    }
    Ok((capture_id.to_owned(), parse_heap_object_id(heap_object_id)?))
}

fn heap_node_by_id(
    graph: &HeapGraph,
    heap_object_id: u64,
) -> Result<NodeIndex, TargetDebuggerError> {
    graph
        .node_by_heap_object_id(heap_object_id)
        .ok_or(TargetDebuggerError::HeapNodeNotFound(
            heap_object_id.to_string(),
        ))
}

fn heap_node_reference(capture_id: &str, heap_object_id: u64) -> String {
    format!("{capture_id}#{heap_object_id}")
}

fn bounded_heap_text(value: &str, max_length: Option<u32>) -> (String, bool) {
    let Some(max_length) = max_length else {
        return (value.to_owned(), false);
    };
    let max_length = max_length as usize;
    let mut end = value.len();
    let mut chars = value.char_indices();
    if let Some((index, _)) = chars.nth(max_length) {
        end = index;
    }
    (value[..end].to_owned(), end < value.len())
}

fn heap_node_snapshot(
    graph: &HeapGraph,
    capture_id: &str,
    node: NodeIndex,
    max_string_length: Option<u32>,
    dominators: Option<&crate::heap_graph::DominatorAnalysis>,
) -> Result<HeapNodeSnapshot, TargetDebuggerError> {
    let summary = graph.node_summary(node).map_err(heap_analysis_error)?;
    let (name, name_truncated) = bounded_heap_text(summary.raw_name, max_string_length);
    let reconstructed = graph
        .reconstructed_string(node, max_string_length.map(|length| length as usize))
        .map_err(heap_analysis_error)?;
    let (string_value, string_truncated) = reconstructed
        .map(|value| (Some(value.value), value.truncated))
        .unwrap_or((None, name_truncated));
    let locations = graph
        .locations_for_node(node)
        .map_err(heap_analysis_error)?
        .map(|location| HeapNodeLocationSnapshot {
            script_id: location.script_id,
            line: location.line,
            column: location.column,
        })
        .collect();
    Ok(HeapNodeSnapshot {
        reference: heap_node_reference(capture_id, summary.heap_object_id),
        node_index: node.0,
        node_type: summary.node_type.to_owned(),
        heap_object_id: summary.heap_object_id.to_string(),
        name,
        string_value,
        string_truncated,
        shallow_size: summary.shallow_size,
        outgoing_reference_count: summary.outgoing_references as u64,
        incoming_reference_count: summary.incoming_references as u64,
        locations,
        immediate_dominator: dominators
            .and_then(|analysis| analysis.immediate_dominator(node))
            .map(|dominator| {
                graph
                    .node_summary(dominator)
                    .map(|summary| heap_node_reference(capture_id, summary.heap_object_id))
            })
            .transpose()
            .map_err(heap_analysis_error)?,
        retained_size: dominators.and_then(|analysis| analysis.retained_size(node)),
    })
}

fn heap_reference_snapshot(
    graph: &HeapGraph,
    capture_id: &str,
    reference: crate::heap_graph::HeapReference<'_>,
) -> Result<HeapReferenceSnapshot, TargetDebuggerError> {
    let source = graph
        .node_summary(reference.source)
        .map_err(heap_analysis_error)?;
    let target = graph
        .node_summary(reference.target)
        .map_err(heap_analysis_error)?;
    Ok(HeapReferenceSnapshot {
        edge_index: reference.edge.0,
        edge_type: reference.edge_type.to_owned(),
        name: reference.name.map(str::to_owned),
        name_or_index: reference.name_or_index,
        source: heap_node_reference(capture_id, source.heap_object_id),
        target: heap_node_reference(capture_id, target.heap_object_id),
    })
}

fn heap_path_step_snapshot(
    graph: &HeapGraph,
    capture_id: &str,
    step: crate::heap_graph::PathStep,
) -> Result<HeapPathStepSnapshot, TargetDebuggerError> {
    let source = match step.direction {
        TraversalDirection::Outgoing => step.from,
        TraversalDirection::Incoming => step.to,
    };
    let reference = graph
        .outgoing_references(source)
        .map_err(heap_analysis_error)?
        .find(|reference| reference.edge == step.edge)
        .ok_or_else(|| {
            TargetDebuggerError::HeapAnalysis(format!("path edge {} does not exist", step.edge.0))
        })?;
    let from = graph.node_summary(step.from).map_err(heap_analysis_error)?;
    let to = graph.node_summary(step.to).map_err(heap_analysis_error)?;
    Ok(HeapPathStepSnapshot {
        from: heap_node_reference(capture_id, from.heap_object_id),
        to: heap_node_reference(capture_id, to.heap_object_id),
        edge_index: reference.edge.0,
        edge_type: reference.edge_type.to_owned(),
        name: reference.name.map(str::to_owned),
        name_or_index: reference.name_or_index,
        direction: match step.direction {
            TraversalDirection::Outgoing => HeapTraversalDirection::Outgoing,
            TraversalDirection::Incoming => HeapTraversalDirection::Incoming,
        },
    })
}

fn heap_path_direction(direction: HeapPathDirection) -> PathDirection {
    match direction {
        HeapPathDirection::Outgoing => PathDirection::Outgoing,
        HeapPathDirection::Incoming => PathDirection::Incoming,
        HeapPathDirection::Either => PathDirection::Either,
    }
}

fn heap_edge_policy(policy: HeapEdgePolicy) -> EdgePolicy {
    match policy {
        HeapEdgePolicy::Strong => EdgePolicy::Strong,
        HeapEdgePolicy::All => EdgePolicy::All,
    }
}

fn heap_path_cost(cost: HeapPathCost) -> CostPolicy {
    match cost {
        HeapPathCost::Edges => CostPolicy::Edges,
        HeapPathCost::Readable => CostPolicy::Readable,
    }
}

fn heap_aggregate_by(by: HeapAggregateBy) -> AggregateBy {
    match by {
        HeapAggregateBy::NodeType => AggregateBy::NodeType,
        HeapAggregateBy::Name => AggregateBy::RawName,
        HeapAggregateBy::StringValue => AggregateBy::StringValue,
    }
}

fn heap_analysis_error(error: crate::heap_graph::AnalysisError) -> TargetDebuggerError {
    TargetDebuggerError::HeapAnalysis(error.to_string())
}

fn heap_snapshot_timing(
    written: &crate::cdp_runtime::HeapSnapshotWriteResult,
) -> HeapSnapshotTiming {
    HeapSnapshotTiming {
        taking_duration_micros: written.taking_duration.as_micros() as u64,
        retrieving_duration_micros: written.retrieving_duration.as_micros() as u64,
    }
}

async fn take_heap_snapshot(
    driver: &DebuggerDriver,
    path: PathBuf,
    capture_numeric_value: bool,
    expose_internals: bool,
) -> Result<crate::cdp_runtime::HeapSnapshotWriteResult, TargetDebuggerError> {
    driver
        .begin_heap_snapshot(path)
        .await
        .map_err(|error| TargetDebuggerError::HeapSnapshot(error.to_string()))?;
    let mut params = HeapProfilerTakeHeapSnapshotParams::new();
    params.report_progress = Some(true);
    params.capture_numeric_value = capture_numeric_value.then_some(true);
    params.expose_internals = expose_internals.then_some(true);
    if let Err(error) = driver
        .client()
        .heap_profiler_take_heap_snapshot(params)
        .await
    {
        driver.abort_heap_snapshot().await;
        return Err(TargetDebuggerError::HeapSnapshot(format!("{error:?}")));
    }
    driver
        .finish_heap_snapshot()
        .await
        .map_err(|error| TargetDebuggerError::HeapSnapshot(error.to_string()))
}

struct ProjectedHeapClass {
    name: String,
    source_url: String,
    location: SourceLocation,
    generated_name: String,
    instance_count: u64,
    shallow_size: u64,
    instances: Vec<crate::heap_snapshot::HeapInstanceRecord>,
}

struct MappedHeapConstructor<'a> {
    group: &'a HeapConstructorGroup,
    generated_url: String,
    generated_location: SourceLocation,
    mapped: Option<(String, Position, Arc<str>)>,
}

pub fn stored_heap_classes(
    path: &Path,
    capture_id: String,
    filter: Option<&str>,
) -> Result<HeapClassSnapshot, TargetDebuggerError> {
    let started = Instant::now();
    let filter = filter
        .map(regex::Regex::new)
        .transpose()
        .map_err(|error| TargetDebuggerError::InvalidHeapFilter(error.to_string()))?;
    let file =
        File::open(path).map_err(|error| TargetDebuggerError::HeapSnapshot(error.to_string()))?;
    let groups = parse_constructor_groups(file)
        .map_err(|error| TargetDebuggerError::HeapSnapshot(error.to_string()))?;
    let parse_duration = started.elapsed();
    let mut alias_counters = BTreeMap::<String, u64>::new();
    let mut classes = groups
        .iter()
        .filter(|group| {
            filter.as_ref().is_none_or(|filter| {
                filter.is_match(&group.generated_name)
                    || filter.is_match(&format!("script:{}", group.script_id))
            })
        })
        .map(|group| {
            let name = heap_class_display_name(&group.generated_name).to_owned();
            let instances = group
                .instances
                .iter()
                .take(20)
                .map(|instance| {
                    let counter = alias_counters.entry(name.clone()).or_default();
                    *counter = counter.saturating_add(1);
                    HeapInstanceSnapshot {
                        alias: format!("{name}@{}", *counter),
                        heap_object_id: instance.heap_object_id.to_string(),
                        shallow_size: instance.shallow_size,
                    }
                })
                .collect::<Vec<_>>();
            HeapClassSnapshotEntry {
                name,
                source_url: format!("script:{}", group.script_id),
                location: source_location(
                    format!("script:{}", group.script_id),
                    group.line,
                    group.column,
                ),
                generated_name: group.generated_name.clone(),
                instance_count: group.instance_count,
                shallow_size: group.shallow_size,
                omitted_instance_count: group.instance_count.saturating_sub(instances.len() as u64),
                instances,
            }
        })
        .collect::<Vec<_>>();
    classes.sort_by_key(|class| std::cmp::Reverse(class.instance_count));
    Ok(HeapClassSnapshot {
        capture_id,
        total_instances: classes.iter().map(|class| class.instance_count).sum(),
        total_shallow_size: classes.iter().map(|class| class.shallow_size).sum(),
        classes,
        analysis: HeapClassAnalysisSnapshot {
            snapshot_timing: None,
            parse_duration_micros: parse_duration.as_micros() as u64,
            projection_duration_micros: 0,
            source_map_hydration_duration_micros: 0,
            constructor_group_count: groups.len() as u64,
            used_cached_groups: false,
        },
    })
}

async fn project_heap_classes(
    driver: &mut DebuggerDriver,
    session_key: &SessionKey,
    capture_id: String,
    groups: &[HeapConstructorGroup],
    filter: Option<&regex::Regex>,
    no_cache: bool,
) -> Result<(HeapClassSnapshot, Duration), TargetDebuggerError> {
    let scripts = groups
        .iter()
        .map(|group| ScriptKey {
            session: session_key.clone(),
            script_id: group.script_id.to_string(),
        })
        .collect::<BTreeSet<_>>();
    driver.set_source_map_cache_enabled(!no_cache);
    let hydration_started = Instant::now();
    let mut hydration_result = Ok(());
    for script in &scripts {
        let eligible = driver.state().scripts.get(script).is_some_and(|state| {
            state.source_map_url.is_some() && matches!(state.source, ScriptSourceState::Unresolved)
        });
        if eligible {
            hydration_result = driver
                .apply(Input::RequestScriptSource {
                    script: script.clone(),
                })
                .await
                .map(|_| ());
            if hydration_result.is_err() {
                break;
            }
        }
    }
    driver.set_source_map_cache_enabled(true);
    hydration_result?;
    let source_map_hydration_duration = hydration_started.elapsed();

    let state = driver.state().clone();
    let source_effects = driver.source_effects();
    let mapped_groups = groups
        .iter()
        .map(|group| {
            let script = ScriptKey {
                session: session_key.clone(),
                script_id: group.script_id.to_string(),
            };
            let generated_url = state.scripts.get(&script).map_or_else(
                || format!("script:{}", group.script_id),
                |state| state.url.clone(),
            );
            MappedHeapConstructor {
                group,
                generated_location: source_location(
                    generated_url.clone(),
                    group.line,
                    group.column,
                ),
                mapped: source_effects.project_generated_position(
                    &state,
                    &script,
                    Position {
                        line: group.line,
                        column: group.column,
                    },
                ),
                generated_url,
            }
        })
        .collect::<Vec<_>>();
    source_effects.prepare_breadcrumbs(
        &state,
        &mapped_groups
            .iter()
            .filter_map(|mapped| {
                mapped.mapped.as_ref().map(|(source_url, _, content)| {
                    (
                        ScriptKey {
                            session: session_key.clone(),
                            script_id: mapped.group.script_id.to_string(),
                        },
                        source_url.clone(),
                        content.clone(),
                    )
                })
            })
            .collect::<Vec<_>>(),
    );
    let mut projected = BTreeMap::<(String, u32, u32, String), ProjectedHeapClass>::new();
    for mapped_group in mapped_groups {
        let group = mapped_group.group;
        let script = ScriptKey {
            session: session_key.clone(),
            script_id: group.script_id.to_string(),
        };
        let (source_url, location, name) = match mapped_group.mapped {
            Some((source_url, position, content)) => {
                let location = source_location(source_url.clone(), position.line, position.column);
                let name = source_effects
                    .breadcrumb(
                        &state,
                        &script,
                        &source_url,
                        location.line,
                        location.column,
                        &content,
                    )
                    .map(|name| heap_class_display_name(&name).to_owned())
                    .unwrap_or_else(|| group.generated_name.clone());
                (source_url, location, name)
            }
            None => (
                mapped_group.generated_url,
                mapped_group.generated_location,
                group.generated_name.clone(),
            ),
        };
        if filter.is_some_and(|filter| {
            !filter.is_match(&name)
                && !filter.is_match(&source_url)
                && !filter.is_match(&group.generated_name)
        }) {
            continue;
        }
        let class = projected
            .entry((
                source_url.clone(),
                location.line,
                location.column,
                name.clone(),
            ))
            .or_insert_with(|| ProjectedHeapClass {
                name,
                source_url,
                location,
                generated_name: group.generated_name.clone(),
                instance_count: 0,
                shallow_size: 0,
                instances: Vec::new(),
            });
        class.instance_count = class.instance_count.saturating_add(group.instance_count);
        class.shallow_size = class.shallow_size.saturating_add(group.shallow_size);
        let remaining = 20_usize.saturating_sub(class.instances.len());
        class
            .instances
            .extend(group.instances.iter().take(remaining).cloned());
    }

    let mut classes = projected.into_values().collect::<Vec<_>>();
    classes.sort_by_key(|class| std::cmp::Reverse(class.instance_count));
    let total_instances = classes.iter().map(|class| class.instance_count).sum();
    let total_shallow_size = classes.iter().map(|class| class.shallow_size).sum();
    let mut alias_counters = BTreeMap::<String, u64>::new();
    let classes = classes
        .into_iter()
        .map(|class| {
            let instances = class
                .instances
                .into_iter()
                .map(|instance| {
                    let counter = alias_counters.entry(class.name.clone()).or_default();
                    *counter = counter.saturating_add(1);
                    HeapInstanceSnapshot {
                        alias: format!("{}@{}", class.name, *counter),
                        heap_object_id: instance.heap_object_id.to_string(),
                        shallow_size: instance.shallow_size,
                    }
                })
                .collect::<Vec<_>>();
            HeapClassSnapshotEntry {
                name: class.name,
                source_url: class.source_url,
                location: class.location,
                generated_name: class.generated_name,
                instance_count: class.instance_count,
                shallow_size: class.shallow_size,
                omitted_instance_count: class.instance_count.saturating_sub(instances.len() as u64),
                instances,
            }
        })
        .collect();
    Ok((
        HeapClassSnapshot {
            capture_id,
            total_instances,
            total_shallow_size,
            classes,
            analysis: HeapClassAnalysisSnapshot {
                snapshot_timing: None,
                parse_duration_micros: 0,
                projection_duration_micros: 0,
                source_map_hydration_duration_micros: 0,
                constructor_group_count: groups.len() as u64,
                used_cached_groups: true,
            },
        },
        source_map_hydration_duration,
    ))
}

fn heap_class_display_name(name: &str) -> &str {
    name.strip_suffix(".constructor").unwrap_or(name)
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

fn begin_type_text(
    driver: &DebuggerDriver,
    text: String,
) -> tokio::task::JoinHandle<Result<(), TargetDebuggerError>> {
    let client = driver.client().clone();
    tokio::spawn(async move {
        client
            .input_insert_text(InputInsertTextParams::new(text))
            .await
            .map(|_| ())
            .map_err(|error| TargetDebuggerError::Interaction(format!("{error:?}")))
    })
}

async fn key(driver: &DebuggerDriver, chord: &str) -> Result<(), TargetDebuggerError> {
    let keys = match chord.to_ascii_lowercase().as_str() {
        "ctrl+n" | "control+n" => vec![(2, "KeyN", "n", 78, None)],
        "ctrl+k,ctrl+m" | "control+k,control+m" => {
            vec![(2, "KeyK", "k", 75, None), (2, "KeyM", "m", 77, None)]
        }
        "ctrl+k,n" | "control+k,n" => {
            vec![(2, "KeyK", "k", 75, None), (0, "KeyN", "n", 78, None)]
        }
        "enter" => vec![(0, "Enter", "Enter", 13, Some("\r"))],
        "accept" => vec![(0, "Enter", "Enter", 13, None)],
        "arrowup" | "up" => vec![(0, "ArrowUp", "ArrowUp", 38, None)],
        _ => return Err(TargetDebuggerError::UnsupportedKeyChord(chord.to_owned())),
    };
    for (modifiers, code, key, virtual_key, text) in keys {
        let mut event_types = vec![InputDispatchKeyEventParamsType::RawKeyDown];
        if text.is_some() {
            event_types.push(InputDispatchKeyEventParamsType::Char);
        }
        event_types.push(InputDispatchKeyEventParamsType::KeyUp);
        for kind in event_types {
            let is_key_down = kind == InputDispatchKeyEventParamsType::RawKeyDown;
            let is_char = kind == InputDispatchKeyEventParamsType::Char;
            let mut event = InputDispatchKeyEventParams::new(kind);
            event.modifiers = Some(modifiers);
            event.code = Some(code.to_owned());
            event.key = Some(key.to_owned());
            event.windows_virtual_key_code = Some(virtual_key);
            event.native_virtual_key_code = Some(virtual_key);
            if is_key_down || is_char {
                event.text = text.map(str::to_owned);
                event.unmodified_text = text.map(str::to_owned);
            }
            driver
                .client()
                .input_dispatch_key_event(event)
                .await
                .map_err(|error| TargetDebuggerError::Interaction(format!("{error:?}")))?;
        }
    }
    Ok(())
}

async fn start_coverage(
    driver: &mut DebuggerDriver,
    _session_key: &SessionKey,
) -> Result<(), TargetDebuggerError> {
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
    recording: &mut CoverageRecording,
) -> Result<CoverageSnapshot, TargetDebuggerError> {
    update_coverage(driver, recording).await?;
    Ok(recording.snapshot())
}

async fn update_coverage(
    driver: &DebuggerDriver,
    recording: &mut CoverageRecording,
) -> Result<(), TargetDebuggerError> {
    let coverage = driver
        .client()
        .profiler_take_precise_coverage(ProfilerTakePreciseCoverageParams::new())
        .await
        .map_err(|error| TargetDebuggerError::Coverage(format!("{error:?}")))?;
    recording.timestamp_micros = (coverage.timestamp * 1_000_000.0).max(0.0) as u64;
    for script in coverage.result {
        recording.merge(script);
    }
    Ok(())
}

fn effective_coverage_ranges(ranges: &[CoverageRangeSnapshot]) -> Vec<CoverageRangeSnapshot> {
    let mut boundaries = ranges
        .iter()
        .flat_map(|range| [range.start_offset, range.end_offset])
        .collect::<Vec<_>>();
    boundaries.sort_unstable();
    boundaries.dedup();
    let mut effective = Vec::<CoverageRangeSnapshot>::new();
    for window in boundaries.windows(2) {
        let start = window[0];
        let end = window[1];
        if start == end {
            continue;
        }
        let Some(range) = ranges
            .iter()
            .filter(|range| range.start_offset <= start && range.end_offset >= end)
            .min_by_key(|range| range.end_offset.saturating_sub(range.start_offset))
        else {
            continue;
        };
        if range.count == 0 {
            continue;
        }
        if let Some(previous) = effective.last_mut()
            && previous.end_offset == start
            && previous.count == range.count
        {
            previous.end_offset = end;
            continue;
        }
        effective.push(CoverageRangeSnapshot {
            start_offset: start,
            end_offset: end,
            count: range.count,
            authored_start: None,
            authored_end: None,
        });
    }
    effective
}

async fn project_coverage(
    driver: &mut DebuggerDriver,
    session_key: &SessionKey,
    snapshot: &mut CoverageSnapshot,
    source_path: Option<&str>,
    no_cache: bool,
) -> Result<(), TargetDebuggerError> {
    let source_already_resolved = source_path.is_some_and(|path| {
        driver
            .state()
            .scripts
            .keys()
            .filter(|key| key.session == *session_key)
            .any(|script| script_contains_source(driver, script, path))
    });
    let mut candidates = snapshot
        .sources
        .iter()
        .filter_map(|source| {
            let count = source
                .functions
                .iter()
                .flat_map(|function| &function.ranges)
                .map(|range| range.count)
                .sum::<u64>();
            let key = ScriptKey {
                session: session_key.clone(),
                script_id: source.script_id.clone(),
            };
            let script = driver.state().scripts.get(&key)?;
            let eligible = matches!(script.source, ScriptSourceState::Unresolved);
            (count > 0 && eligible).then_some((script.source_map_url.is_some(), count, key))
        })
        .collect::<Vec<_>>();
    candidates.sort_by_key(|(mapped, count, _)| std::cmp::Reverse((*mapped, *count)));
    if !source_already_resolved {
        driver.set_source_map_cache_enabled(!no_cache);
        let mut hydration_result = Ok(());
        for (_, _, script) in candidates {
            hydration_result = driver
                .apply(Input::RequestScriptSource {
                    script: script.clone(),
                })
                .await
                .map(|_| ());
            if hydration_result.is_err() {
                break;
            }
            if source_path.is_none_or(|path| script_contains_source(driver, &script, path)) {
                break;
            }
        }
        driver.set_source_map_cache_enabled(true);
        hydration_result?;
    }

    let state = driver.state().clone();
    let source_effects = driver.source_effects();
    snapshot.sources.par_iter_mut().for_each(|source| {
        let script_key = ScriptKey {
            session: session_key.clone(),
            script_id: source.script_id.clone(),
        };
        source.associated_authored_source =
            state
                .scripts
                .get(&script_key)
                .and_then(|state| match &state.source {
                    ScriptSourceState::Resolved(view) if view.logical_sources.len() == 1 => {
                        view.logical_sources.keys().next().cloned()
                    }
                    _ => None,
                });
        source.functions.par_iter_mut().for_each(|function| {
            function.generated_location = source_effects
                .generated_position(&state, &script_key, function.root_start_offset)
                .map(|position| {
                    source_location(source.generated_url.clone(), position.line, position.column)
                });
            for range in &mut function.ranges {
                let start = source_effects.project_generated_offset(
                    &state,
                    &script_key,
                    range.start_offset,
                );
                let end = source_effects.project_generated_offset(
                    &state,
                    &script_key,
                    range.end_offset.saturating_sub(1),
                );
                if let Some((source_url, position, content)) = start {
                    let location = source_location(source_url, position.line, position.column);
                    range.authored_start = Some(location);
                    let _ = content;
                }
                if let Some((source_url, position, _)) = end {
                    range.authored_end =
                        Some(source_location(source_url, position.line, position.column));
                }
            }
            function.effective_ranges = effective_coverage_ranges(&function.ranges);
            let mut first_projected = None;
            for range in &mut function.effective_ranges {
                if let Some((source_url, position, content)) =
                    source_effects.project_generated_offset(&state, &script_key, range.start_offset)
                {
                    let location = source_location(source_url, position.line, position.column);
                    range.authored_start = Some(location.clone());
                    first_projected.get_or_insert((location, content));
                }
                if let Some((source_url, position, _)) = source_effects.project_generated_offset(
                    &state,
                    &script_key,
                    range.end_offset.saturating_sub(1),
                ) {
                    range.authored_end =
                        Some(source_location(source_url, position.line, position.column));
                }
            }
            if let Some((location, content)) = first_projected {
                function.authored_location = Some(location.clone());
                let _ = content;
            }
        });
    });
    let mut file_lines = BTreeMap::<String, BTreeSet<u32>>::new();
    for range in snapshot
        .sources
        .iter()
        .flat_map(|source| &source.functions)
        .flat_map(|function| &function.effective_ranges)
    {
        if let (Some(start), Some(end)) = (&range.authored_start, &range.authored_end)
            && start.source_url == end.source_url
        {
            file_lines
                .entry(start.source_url.clone())
                .or_default()
                .extend(start.line..=end.line.max(start.line));
        }
    }
    let mut ranked_files = file_lines.into_iter().collect::<Vec<_>>();
    ranked_files.sort_by_key(|(_, lines)| std::cmp::Reverse(lines.len()));
    let enriched_files = match source_path {
        Some(prefix) => {
            let prefix = normalize_source_path(prefix);
            ranked_files
                .into_iter()
                .map(|(source, _)| source)
                .filter(|source| normalize_source_path(source).starts_with(prefix))
                .collect::<BTreeSet<_>>()
        }
        None => ranked_files
            .into_iter()
            .map(|(source, _)| source)
            .collect::<BTreeSet<_>>(),
    };
    snapshot.sources.par_iter_mut().for_each(|source| {
        let script_key = ScriptKey {
            session: session_key.clone(),
            script_id: source.script_id.clone(),
        };
        source.functions.par_iter_mut().for_each(|function| {
            if let Some(location) = &function.authored_location
                && enriched_files.contains(&location.source_url)
                && let Some((_, _, content)) = source_effects.project_generated_offset(
                    &state,
                    &script_key,
                    function.effective_ranges[0].start_offset,
                )
            {
                function.breadcrumb = callback_aware_breadcrumb(
                    source_effects.breadcrumb(
                        &state,
                        &script_key,
                        &location.source_url,
                        location.line,
                        location.column,
                        &content,
                    ),
                    &function.name,
                );
            }
            if function.breadcrumb.is_none()
                && let Some(location) = function.generated_location.as_ref()
                && source_path.is_none_or(|path| {
                    normalize_source_path(&location.source_url)
                        .starts_with(normalize_source_path(path))
                })
            {
                function.breadcrumb = generated_script_callback_breadcrumb(
                    source_effects,
                    &state,
                    &script_key,
                    location,
                    &function.name,
                );
            }
        });
    });
    Ok(())
}

fn script_contains_source(driver: &DebuggerDriver, script: &ScriptKey, source_path: &str) -> bool {
    let source_path = normalize_source_path(source_path);
    driver
        .state()
        .scripts
        .get(script)
        .and_then(|state| match &state.source {
            ScriptSourceState::Resolved(view) => Some(&view.logical_sources),
            _ => None,
        })
        .is_some_and(|sources| {
            sources
                .keys()
                .any(|source| normalize_source_path(source).starts_with(&source_path))
        })
}

fn normalize_source_path(path: &str) -> &str {
    path.trim_start_matches("../").trim_start_matches("./")
}

#[derive(Clone, Copy)]
struct CpuProfileRecording {
    sampling_interval_micros: Option<u64>,
}

#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord)]
struct CpuProfileFunctionKey {
    name: String,
    source_url: String,
    line: u32,
    column: u32,
}

async fn start_cpu_profile(
    driver: &DebuggerDriver,
    sampling_interval_micros: Option<u64>,
) -> Result<(), TargetDebuggerError> {
    if sampling_interval_micros.is_some_and(|interval| interval == 0 || interval > i32::MAX as u64)
    {
        return Err(TargetDebuggerError::InvalidCpuProfileSamplingInterval);
    }
    driver
        .client()
        .profiler_enable(ProfilerEnableParams::new())
        .await
        .map_err(|error| TargetDebuggerError::CpuProfile(format!("{error:?}")))?;
    if let Some(interval) = sampling_interval_micros {
        driver
            .client()
            .profiler_set_sampling_interval(ProfilerSetSamplingIntervalParams::new(interval as i64))
            .await
            .map_err(|error| TargetDebuggerError::CpuProfile(format!("{error:?}")))?;
    }
    driver
        .client()
        .profiler_start(ProfilerStartParams::new())
        .await
        .map(|_| ())
        .map_err(|error| TargetDebuggerError::CpuProfile(format!("{error:?}")))
}

fn cpu_profile_snapshot(
    capture_id: String,
    sampling_interval_micros: Option<u64>,
    profile: ProfilerProfile,
) -> Result<CpuProfileSnapshot, TargetDebuggerError> {
    let samples = profile.samples.unwrap_or_default();
    let raw_time_deltas = profile.time_deltas.unwrap_or_default();
    if samples.len() != raw_time_deltas.len() {
        return Err(TargetDebuggerError::InvalidCpuProfile(format!(
            "received {} samples but {} time deltas",
            samples.len(),
            raw_time_deltas.len()
        )));
    }
    let time_deltas_micros = raw_time_deltas
        .into_iter()
        .map(|delta| {
            u64::try_from(delta).map_err(|_| {
                TargetDebuggerError::InvalidCpuProfile(format!(
                    "received a negative sample time delta ({delta})"
                ))
            })
        })
        .collect::<Result<Vec<_>, _>>()?;
    let nodes = profile
        .nodes
        .into_iter()
        .map(|node| CpuProfileNodeSnapshot {
            id: node.id,
            call_frame: CpuProfileCallFrameSnapshot {
                function_name: node.call_frame.function_name,
                script_id: node.call_frame.script_id,
                url: node.call_frame.url,
                line_number: node.call_frame.line_number,
                column_number: node.call_frame.column_number,
            },
            hit_count: node.hit_count,
            children: node.children.unwrap_or_default(),
            deopt_reason: node.deopt_reason,
            position_ticks: node
                .position_ticks
                .unwrap_or_default()
                .into_iter()
                .map(|tick| CpuProfilePositionTickSnapshot {
                    line: tick.line,
                    ticks: tick.ticks,
                })
                .collect(),
            authored_location: None,
            breadcrumb: None,
            self_time_micros: 0,
            total_time_micros: 0,
            sample_count: 0,
        })
        .collect();
    let mut snapshot = CpuProfileSnapshot {
        capture_id,
        sampling_interval_micros,
        start_time_micros: profile.start_time,
        end_time_micros: profile.end_time,
        nodes,
        samples,
        time_deltas_micros,
        functions: Vec::new(),
        analysis: None,
    };
    aggregate_cpu_profile(&mut snapshot)?;
    Ok(snapshot)
}

async fn project_cpu_profile(
    driver: &mut DebuggerDriver,
    session_key: &SessionKey,
    snapshot: &mut CpuProfileSnapshot,
    source_path: Option<&str>,
    no_cache: bool,
) -> Result<(), TargetDebuggerError> {
    let node_indexes = cpu_profile_node_indexes(snapshot)?;
    let mut script_weights = BTreeMap::<String, u64>::new();
    for (&sample_id, &delta) in snapshot.samples.iter().zip(&snapshot.time_deltas_micros) {
        let node = &snapshot.nodes[*node_indexes.get(&sample_id).ok_or_else(|| {
            TargetDebuggerError::InvalidCpuProfile(format!(
                "sample references missing node {sample_id}"
            ))
        })?];
        *script_weights
            .entry(node.call_frame.script_id.clone())
            .or_default() += delta;
    }
    let mut candidates = script_weights
        .into_iter()
        .filter_map(|(script_id, weight)| {
            let key = ScriptKey {
                session: session_key.clone(),
                script_id,
            };
            let script = driver.state().scripts.get(&key)?;
            matches!(script.source, ScriptSourceState::Unresolved).then_some((
                script.source_map_url.is_some(),
                weight,
                key,
            ))
        })
        .collect::<Vec<_>>();
    candidates.sort_by_key(|(mapped, weight, _)| std::cmp::Reverse((*mapped, *weight)));

    driver.set_source_map_cache_enabled(!no_cache);
    let mut hydration_result = Ok(());
    for (_, _, script) in candidates {
        hydration_result = driver
            .apply(Input::RequestScriptSource {
                script: script.clone(),
            })
            .await
            .map(|_| ());
        if hydration_result.is_err()
            || source_path.is_some_and(|path| script_contains_source(driver, &script, path))
        {
            break;
        }
    }
    driver.set_source_map_cache_enabled(true);
    hydration_result?;

    let state = driver.state().clone();
    let source_effects = driver.source_effects();
    snapshot.nodes.par_iter_mut().for_each(|node| {
        let Ok(line) = u32::try_from(node.call_frame.line_number) else {
            return;
        };
        let Ok(column) = u32::try_from(node.call_frame.column_number) else {
            return;
        };
        let script = ScriptKey {
            session: session_key.clone(),
            script_id: node.call_frame.script_id.clone(),
        };
        if let Some((source_url, position, content)) =
            source_effects.project_generated_position(&state, &script, Position { line, column })
        {
            let location = source_location(source_url.clone(), position.line, position.column);
            node.breadcrumb = callback_aware_breadcrumb(
                source_effects.breadcrumb(
                    &state,
                    &script,
                    &source_url,
                    location.line,
                    location.column,
                    &content,
                ),
                &node.call_frame.function_name,
            );
            node.authored_location = Some(location);
        }
        if node.breadcrumb.is_none() {
            node.breadcrumb = generated_script_callback_breadcrumb(
                source_effects,
                &state,
                &script,
                &cpu_profile_generated_location(node),
                &node.call_frame.function_name,
            );
        }
    });
    aggregate_cpu_profile(snapshot)
}

fn cpu_profile_node_indexes(
    snapshot: &CpuProfileSnapshot,
) -> Result<BTreeMap<i64, usize>, TargetDebuggerError> {
    let mut indexes = BTreeMap::new();
    for (index, node) in snapshot.nodes.iter().enumerate() {
        if indexes.insert(node.id, index).is_some() {
            return Err(TargetDebuggerError::InvalidCpuProfile(format!(
                "profile contains duplicate node {}",
                node.id
            )));
        }
    }
    Ok(indexes)
}

pub(crate) fn aggregate_cpu_profile(
    snapshot: &mut CpuProfileSnapshot,
) -> Result<(), TargetDebuggerError> {
    let node_indexes = cpu_profile_node_indexes(snapshot)?;
    let mut parents = BTreeMap::<i64, i64>::new();
    for node in &snapshot.nodes {
        for child in &node.children {
            if !node_indexes.contains_key(child) {
                return Err(TargetDebuggerError::InvalidCpuProfile(format!(
                    "node {} references missing child {child}",
                    node.id
                )));
            }
            if let Some(previous) = parents.insert(*child, node.id)
                && previous != node.id
            {
                return Err(TargetDebuggerError::InvalidCpuProfile(format!(
                    "node {child} has multiple parents"
                )));
            }
        }
    }

    for node in &mut snapshot.nodes {
        node.self_time_micros = 0;
        node.total_time_micros = 0;
        node.sample_count = 0;
    }
    let mut function_templates =
        BTreeMap::<CpuProfileFunctionKey, CpuProfileFunctionSnapshot>::new();
    for node in &snapshot.nodes {
        let key = cpu_profile_function_key(node);
        function_templates
            .entry(key)
            .or_insert_with(|| cpu_profile_function(node));
    }
    let mut functions = function_templates;

    for (&sample_id, &delta) in snapshot.samples.iter().zip(&snapshot.time_deltas_micros) {
        let sample_index = *node_indexes.get(&sample_id).ok_or_else(|| {
            TargetDebuggerError::InvalidCpuProfile(format!(
                "sample references missing node {sample_id}"
            ))
        })?;
        let leaf_key = cpu_profile_function_key(&snapshot.nodes[sample_index]);
        snapshot.nodes[sample_index].self_time_micros = snapshot.nodes[sample_index]
            .self_time_micros
            .saturating_add(delta);
        snapshot.nodes[sample_index].sample_count =
            snapshot.nodes[sample_index].sample_count.saturating_add(1);
        if let Some(function) = functions.get_mut(&leaf_key) {
            function.self_time_micros = function.self_time_micros.saturating_add(delta);
            function.sample_count = function.sample_count.saturating_add(1);
        }

        let mut current = Some(sample_id);
        let mut visited_nodes = BTreeSet::new();
        let mut visited_functions = BTreeSet::new();
        while let Some(node_id) = current {
            if !visited_nodes.insert(node_id) {
                return Err(TargetDebuggerError::InvalidCpuProfile(
                    "profile node graph contains a cycle".to_owned(),
                ));
            }
            let index = *node_indexes.get(&node_id).ok_or_else(|| {
                TargetDebuggerError::InvalidCpuProfile(format!(
                    "profile stack references missing node {node_id}"
                ))
            })?;
            snapshot.nodes[index].total_time_micros = snapshot.nodes[index]
                .total_time_micros
                .saturating_add(delta);
            let key = cpu_profile_function_key(&snapshot.nodes[index]);
            if visited_functions.insert(key.clone())
                && let Some(function) = functions.get_mut(&key)
            {
                function.total_time_micros = function.total_time_micros.saturating_add(delta);
            }
            current = parents.get(&node_id).copied();
        }
    }
    snapshot.functions = functions
        .into_values()
        .filter(|function| function.total_time_micros > 0)
        .collect();
    Ok(())
}

fn cpu_profile_function_key(node: &CpuProfileNodeSnapshot) -> CpuProfileFunctionKey {
    let location = node
        .authored_location
        .as_ref()
        .cloned()
        .unwrap_or_else(|| cpu_profile_generated_location(node));
    CpuProfileFunctionKey {
        name: node.call_frame.function_name.clone(),
        source_url: location.source_url,
        line: location.line,
        column: location.column,
    }
}

fn cpu_profile_function(node: &CpuProfileNodeSnapshot) -> CpuProfileFunctionSnapshot {
    CpuProfileFunctionSnapshot {
        name: node.call_frame.function_name.clone(),
        breadcrumb: node.breadcrumb.clone(),
        generated_location: cpu_profile_generated_location(node),
        authored_location: node.authored_location.clone(),
        self_time_micros: 0,
        total_time_micros: 0,
        sample_count: 0,
    }
}

fn cpu_profile_generated_location(node: &CpuProfileNodeSnapshot) -> SourceLocation {
    SourceLocation {
        source_url: node.call_frame.url.clone(),
        line: u32::try_from(node.call_frame.line_number)
            .map(|line| line.saturating_add(1))
            .unwrap_or(0),
        column: u32::try_from(node.call_frame.column_number)
            .map(|column| column.saturating_add(1))
            .unwrap_or(0),
    }
}

#[derive(Clone, Default)]
struct CoverageRecording {
    timestamp_micros: u64,
    scripts: BTreeMap<String, AccumulatedScriptCoverage>,
    captures: BTreeMap<String, CoverageSnapshot>,
}

#[derive(Clone, Default)]
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

    fn exclude_coverage(
        mut selected: CoverageSnapshot,
        baseline: &CoverageSnapshot,
    ) -> CoverageSnapshot {
        for source in &mut selected.sources {
            let Some(baseline_source) = baseline.sources.iter().find(|candidate| {
                candidate.script_id == source.script_id
                    && candidate.generated_url == source.generated_url
            }) else {
                continue;
            };
            for function in &mut source.functions {
                let identity = function.ranges.first().map(|range| {
                    (
                        function.name.as_str(),
                        function.block_coverage,
                        range.start_offset,
                        range.end_offset,
                    )
                });
                let Some(baseline_function) = baseline_source.functions.iter().find(|candidate| {
                    candidate.ranges.first().map(|range| {
                        (
                            candidate.name.as_str(),
                            candidate.block_coverage,
                            range.start_offset,
                            range.end_offset,
                        )
                    }) == identity
                }) else {
                    continue;
                };
                function.ranges.retain(|range| {
                    !baseline_function.ranges.iter().any(|candidate| {
                        candidate.start_offset == range.start_offset
                            && candidate.end_offset == range.end_offset
                            && candidate.count > 0
                    })
                });
            }

            source
                .functions
                .retain(|function| !function.ranges.is_empty());
        }
        selected
            .sources
            .retain(|source| !source.functions.is_empty());
        selected
    }

    fn exclude_baseline(&mut self, baseline: &CoverageSnapshot) {
        for source in baseline.sources.iter() {
            let Some(script) = self.scripts.get_mut(&source.script_id) else {
                continue;
            };
            for function in &source.functions {
                let Some(root) = function.ranges.first() else {
                    continue;
                };
                let key = (
                    function.name.clone(),
                    function.block_coverage,
                    root.start_offset,
                    root.end_offset,
                );
                let Some(ranges) = script.functions.get_mut(&key) else {
                    continue;
                };
                for range in &function.ranges {
                    if range.count > 0 {
                        ranges.remove(&(range.start_offset, range.end_offset));
                    }
                }
            }
            script.functions.retain(|_, ranges| !ranges.is_empty());
        }
        self.scripts
            .retain(|_, script| !script.functions.is_empty());
    }

    fn snapshot(&self) -> CoverageSnapshot {
        CoverageSnapshot {
            timestamp_micros: self.timestamp_micros,
            analysis: None,
            sources: self
                .scripts
                .iter()
                .filter_map(|(script_id, script)| {
                    let functions = script
                        .functions
                        .iter()
                        .map(|((name, block_coverage, root_start, root_end), ranges)| {
                            let ranges = ranges
                                .iter()
                                .map(|((start, end), count)| CoverageRangeSnapshot {
                                    start_offset: *start,
                                    end_offset: *end,
                                    count: *count,
                                    authored_start: None,
                                    authored_end: None,
                                })
                                .collect::<Vec<_>>();
                            CoverageFunctionSnapshot {
                                name: if name.is_empty() {
                                    "(anonymous)".to_owned()
                                } else {
                                    name.clone()
                                },
                                block_coverage: *block_coverage,
                                root_start_offset: *root_start,
                                root_end_offset: *root_end,
                                ranges,
                                effective_ranges: Vec::new(),
                                authored_location: None,
                                breadcrumb: None,
                                generated_location: None,
                            }
                        })
                        .collect::<Vec<_>>();
                    if functions.is_empty() {
                        return None;
                    }
                    Some(CoverageSourceSnapshot {
                        script_id: script_id.clone(),
                        generated_url: script.url.clone(),
                        associated_authored_source: None,
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

async fn remove_breakpoint(
    driver: &mut DebuggerDriver,
    client_id: &str,
    breakpoint_id: &str,
) -> Result<(), TargetDebuggerError> {
    driver
        .apply(Input::RemoveBreakpoint {
            key: BreakpointKey {
                client_id: client_id.to_owned(),
                breakpoint_id: breakpoint_id.to_owned(),
            },
        })
        .await?;
    Ok(())
}

fn breakpoint_spec(state: &DebuggerState, key: &BreakpointKey) -> Option<TargetBreakpointSpec> {
    state
        .breakpoints
        .get(key)
        .map(|breakpoint| TargetBreakpointSpec {
            id: key.breakpoint_id.clone(),
            source_url: breakpoint.source_url.clone(),
            line: breakpoint.position.line.saturating_add(1),
            column: breakpoint.position.column.saturating_add(1),
            condition: breakpoint.condition.clone(),
        })
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

async fn release_waiting_target(
    driver: &mut DebuggerDriver,
    session_key: &SessionKey,
) -> Result<(), TargetDebuggerError> {
    let was_waiting = driver
        .state()
        .sessions
        .get(session_key)
        .is_some_and(|session| session.waiting_for_debugger);
    driver
        .apply(Input::ReleaseIfWaiting {
            session: session_key.clone(),
        })
        .await?;
    if !was_waiting {
        return Ok(());
    }

    let startup_pause = tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            let session = driver
                .state()
                .sessions
                .get(session_key)
                .ok_or(TargetDebuggerError::SessionMissing)?;
            if let SessionPhase::Paused { epoch } = session.phase {
                return Ok::<_, TargetDebuggerError>((epoch, session.pause.clone()));
            }
            driver.process_next_event().await?;
        }
    })
    .await;
    let Ok(startup_pause) = startup_pause else {
        return Ok(());
    };
    let (pause_epoch, pause) = startup_pause?;
    if pause
        .as_ref()
        .is_some_and(|pause| pause.reason == "Break on start")
    {
        resume_and_settle(driver, session_key, pause_epoch).await?;
    }
    Ok(())
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
    const OBJECT_GROUP: &str = "jsdbg-ephemeral-evaluation";
    let result = evaluate_remote(
        driver,
        session_key,
        pause_epoch,
        frame_index,
        expression.clone(),
        true,
        crate::promise_debugging::DEFAULT_VALUE_PREVIEW_LENGTH,
        Some(OBJECT_GROUP),
    )
    .await;
    let snapshot = result.map(|result| {
        let mut preview = remote_value_snapshot(
            &result.remote,
            crate::promise_debugging::DEFAULT_VALUE_PREVIEW_LENGTH,
        );
        preview.truncated |= result.preview_truncated;
        preview.reference = None;
        let kind = remote_object_kind(&result.remote);
        EvaluationSnapshot {
            expression,
            kind,
            value: result.remote.value,
            unserializable_value: result.remote.unserializable_value,
            description: result.remote.description,
            object_id: None,
            preview,
        }
    });
    let release = driver
        .client()
        .runtime_release_object_group(RuntimeReleaseObjectGroupParams {
            object_group: OBJECT_GROUP.to_owned(),
        })
        .await
        .map_err(|error| {
            TargetDebuggerError::Properties(format!(
                "failed to release ephemeral evaluation values: {error:?}"
            ))
        });
    match (snapshot, release) {
        (Ok(value), Ok(_)) => Ok(value),
        (Ok(_), Err(error)) | (Err(error), _) => Err(error),
    }
}

struct EvaluatedRemote {
    remote: RuntimeRemoteObject,
    preview_truncated: bool,
}

async fn evaluate_remote(
    driver: &DebuggerDriver,
    session_key: &SessionKey,
    pause_epoch: Option<u64>,
    frame_index: u32,
    expression: String,
    allow_side_effects: bool,
    max_preview_length: u32,
    object_group: Option<&str>,
) -> Result<EvaluatedRemote, TargetDebuggerError> {
    let container_expression = evaluation_container_expression(&expression);
    let result = if let Some(pause_epoch) = pause_epoch {
        let pause = require_pause(driver, session_key, pause_epoch)?;
        let frame = pause
            .frames
            .get(frame_index as usize)
            .ok_or(TargetDebuggerError::FrameNotFound(frame_index))?;
        let mut params = DebuggerEvaluateOnCallFrameParams::new(
            frame.call_frame_id.clone(),
            container_expression.clone(),
        );
        params.return_by_value = Some(false);
        params.generate_preview = Some(false);
        params.throw_on_side_effect = Some(!allow_side_effects);
        params.object_group = object_group.map(str::to_owned);
        let evaluated = driver
            .client()
            .debugger_evaluate_on_call_frame(params)
            .await
            .map_err(|error| TargetDebuggerError::Evaluation(format!("{error:?}")))?;
        if let Some(exception) = evaluated.exception_details {
            return Err(TargetDebuggerError::Evaluation(exception_message(
                &exception,
            )));
        }
        evaluated.result
    } else {
        let mut params = crate::cdp::RuntimeEvaluateParams::new(container_expression);
        params.return_by_value = Some(false);
        params.generate_preview = Some(false);
        params.throw_on_side_effect = Some(!allow_side_effects);
        params.object_group = object_group.map(str::to_owned);
        let evaluated = driver
            .client()
            .runtime_evaluate(params)
            .await
            .map_err(|error| TargetDebuggerError::Evaluation(format!("{error:?}")))?;
        if let Some(exception) = evaluated.exception_details {
            return Err(TargetDebuggerError::Evaluation(exception_message(
                &exception,
            )));
        }
        evaluated.result
    };
    let container_id = result.object_id.ok_or_else(|| {
        TargetDebuggerError::Evaluation("target did not return the evaluation container".to_owned())
    })?;
    let mut projection_params =
        RuntimeCallFunctionOnParams::new(bounded_projection_function(max_preview_length));
    projection_params.object_id = Some(container_id.clone());
    projection_params.return_by_value = Some(false);
    projection_params.generate_preview = Some(false);
    projection_params.object_group = object_group.map(str::to_owned);
    let projected = driver
        .client()
        .runtime_call_function_on(projection_params)
        .await
        .map_err(|error| TargetDebuggerError::Evaluation(format!("{error:?}")))?;
    if let Some(exception) = projected.exception_details {
        return Err(TargetDebuggerError::Evaluation(exception_message(
            &exception,
        )));
    }
    let envelope_id = projected.result.object_id.ok_or_else(|| {
        TargetDebuggerError::Evaluation(
            "target did not return the bounded evaluation envelope".to_owned(),
        )
    })?;
    let projection =
        get_object_property_descriptors(driver, session_key, pause_epoch, envelope_id.clone())
            .await
            .and_then(|(properties, _)| evaluated_remote_from_envelope(&properties));
    if object_group.is_none() {
        let release =
            release_evaluation_objects(driver, [container_id, envelope_id].into_iter()).await;
        match (projection, release) {
            (Ok(value), Ok(_)) => Ok(value),
            (Ok(_), Err(error)) | (Err(error), _) => Err(error),
        }
    } else {
        projection
    }
}

fn evaluation_container_expression(expression: &str) -> String {
    format!(
        r#"({{
  __jsdbgValue: (
{expression}
  )
}})"#
    )
}

fn bounded_projection_function(max_preview_length: u32) -> String {
    format!(
        r#"function() {{
  const __jsdbgValue = this.__jsdbgValue;
  const __jsdbgMaxLength = {max_preview_length};
  const __jsdbgKind = typeof __jsdbgValue;
  let __jsdbgText;
  if (__jsdbgKind === "string") {{
    __jsdbgText = __jsdbgValue;
  }} else if (__jsdbgKind === "bigint") {{
    __jsdbgText = `${{__jsdbgValue}}n`;
  }} else if (__jsdbgKind === "symbol") {{
    // A Symbol description is only exposed through a mutable prototype getter.
    return {{ __jsdbgKind, __jsdbgTruncated: true }};
  }} else if (__jsdbgKind === "number" && __jsdbgValue !== __jsdbgValue) {{
    __jsdbgText = "NaN";
  }} else if (__jsdbgKind === "number" && __jsdbgValue === 1 / 0) {{
    __jsdbgText = "Infinity";
  }} else if (__jsdbgKind === "number" && __jsdbgValue === -1 / 0) {{
    __jsdbgText = "-Infinity";
  }} else if (
    __jsdbgKind === "number"
    && __jsdbgValue === 0
    && 1 / __jsdbgValue === -1 / 0
  ) {{
    __jsdbgText = "-0";
  }} else {{
    return {{ __jsdbgKind: "remote", __jsdbgValue }};
  }}
  let __jsdbgPreview = "";
  let __jsdbgLength = 0;
  let __jsdbgOffset = 0;
  // In-range string index and length reads use own exotic data, not prototype hooks.
  while (
    __jsdbgOffset < __jsdbgText.length
    && __jsdbgLength < __jsdbgMaxLength
  ) {{
    const __jsdbgFirst = __jsdbgText[__jsdbgOffset];
    __jsdbgPreview += __jsdbgFirst;
    __jsdbgOffset++;
    if (
      __jsdbgFirst >= "\uD800"
      && __jsdbgFirst <= "\uDBFF"
      && __jsdbgOffset < __jsdbgText.length
    ) {{
      const __jsdbgSecond = __jsdbgText[__jsdbgOffset];
      if (__jsdbgSecond >= "\uDC00" && __jsdbgSecond <= "\uDFFF") {{
        __jsdbgPreview += __jsdbgSecond;
        __jsdbgOffset++;
      }}
    }}
    __jsdbgLength++;
  }}
  return {{
    __jsdbgKind,
    __jsdbgText: __jsdbgPreview,
    __jsdbgTruncated: __jsdbgOffset < __jsdbgText.length
  }};
}}"#
    )
}

async fn release_evaluation_objects(
    driver: &DebuggerDriver,
    object_ids: impl Iterator<Item = String>,
) -> Result<(), TargetDebuggerError> {
    for object_id in object_ids {
        driver
            .client()
            .runtime_release_object(RuntimeReleaseObjectParams::new(object_id))
            .await
            .map_err(|error| {
                TargetDebuggerError::Properties(format!(
                    "failed to release evaluation envelope: {error:?}"
                ))
            })?;
    }
    Ok(())
}

fn evaluated_remote_from_envelope(
    properties: &[RuntimePropertyDescriptor],
) -> Result<EvaluatedRemote, TargetDebuggerError> {
    let property = |name: &str| {
        properties
            .iter()
            .find(|property| property.name == name)
            .and_then(|property| property.value.as_ref())
    };
    let kind = property("__jsdbgKind")
        .and_then(|value| value.value.as_ref())
        .and_then(serde_json::Value::as_str)
        .ok_or_else(|| {
            TargetDebuggerError::Evaluation(
                "target returned an invalid bounded evaluation envelope".to_owned(),
            )
        })?;
    if kind == "remote" {
        let value = property("__jsdbgValue").ok_or_else(|| {
            TargetDebuggerError::Evaluation(
                "target bounded evaluation envelope omitted its value".to_owned(),
            )
        })?;
        return Ok(EvaluatedRemote {
            remote: value.clone(),
            preview_truncated: false,
        });
    }
    let mut remote = match kind {
        "string" => RuntimeRemoteObject::new(RuntimeRemoteObjectType::String),
        "bigint" => RuntimeRemoteObject::new(RuntimeRemoteObjectType::Bigint),
        "symbol" => RuntimeRemoteObject::new(RuntimeRemoteObjectType::Symbol),
        "number" => RuntimeRemoteObject::new(RuntimeRemoteObjectType::Number),
        _ => {
            return Err(TargetDebuggerError::Evaluation(format!(
                "target returned unknown bounded evaluation kind '{kind}'"
            )));
        }
    };
    let preview = property("__jsdbgText")
        .and_then(|value| value.value.as_ref())
        .and_then(serde_json::Value::as_str);
    if kind != "symbol" && preview.is_none() {
        return Err(TargetDebuggerError::Evaluation(
            "target bounded primitive projection omitted its preview".to_owned(),
        ));
    }
    match &remote.r#type {
        RuntimeRemoteObjectType::String => {
            remote.value = Some(serde_json::Value::String(
                preview.expect("string preview was required").to_owned(),
            ));
        }
        RuntimeRemoteObjectType::Bigint => {
            let preview = preview.expect("BigInt preview was required");
            remote.unserializable_value = Some(preview.to_owned());
            remote.description = Some(preview.to_owned());
        }
        RuntimeRemoteObjectType::Symbol => {}
        RuntimeRemoteObjectType::Number => {
            let preview = preview.expect("number preview was required");
            remote.unserializable_value = Some(preview.to_owned());
            remote.description = Some(preview.to_owned());
        }
        _ => unreachable!("bounded primitive kinds are exhaustive"),
    }
    Ok(EvaluatedRemote {
        remote,
        preview_truncated: property("__jsdbgTruncated")
            .and_then(|value| value.value.as_ref())
            .and_then(serde_json::Value::as_bool)
            .unwrap_or(false),
    })
}

async fn scope_variables(
    driver: &DebuggerDriver,
    session_key: &SessionKey,
    pause_epoch: u64,
    frame_index: u32,
    scope_index: u32,
) -> Result<Vec<VariableSnapshot>, TargetDebuggerError> {
    let pause = require_pause(driver, session_key, pause_epoch)?;
    let frame = pause
        .frames
        .get(frame_index as usize)
        .ok_or(TargetDebuggerError::FrameNotFound(frame_index))?;
    let scope = frame
        .scopes
        .get(scope_index as usize)
        .ok_or(TargetDebuggerError::ScopeNotFound(scope_index))?;
    object_properties(
        driver,
        session_key,
        Some(pause_epoch),
        scope.object_id.clone(),
    )
    .await
}

async fn object_properties(
    driver: &DebuggerDriver,
    session_key: &SessionKey,
    pause_epoch: Option<u64>,
    object_id: String,
) -> Result<Vec<VariableSnapshot>, TargetDebuggerError> {
    let (properties, _) =
        get_object_property_descriptors(driver, session_key, pause_epoch, object_id).await?;
    Ok(properties
        .into_iter()
        .filter_map(|property| {
            property
                .value
                .map(|value| variable_snapshot(property.name, value))
        })
        .collect())
}

async fn get_object_property_descriptors(
    driver: &DebuggerDriver,
    session_key: &SessionKey,
    pause_epoch: Option<u64>,
    object_id: String,
) -> Result<
    (
        Vec<RuntimePropertyDescriptor>,
        Vec<RuntimeInternalPropertyDescriptor>,
    ),
    TargetDebuggerError,
> {
    if let Some(pause_epoch) = pause_epoch {
        require_pause(driver, session_key, pause_epoch)?;
    }
    let mut params = RuntimeGetPropertiesParams::new(object_id);
    params.own_properties = Some(true);
    params.generate_preview = Some(true);
    let result = driver
        .client()
        .runtime_get_properties(params)
        .await
        .map_err(|error| TargetDebuggerError::Properties(format!("{error:?}")))?;
    if let Some(exception) = result.exception_details {
        return Err(TargetDebuggerError::Properties(exception_message(
            &exception,
        )));
    }
    Ok((
        result.result,
        result.internal_properties.unwrap_or_default(),
    ))
}

async fn inspect_value(
    driver: &DebuggerDriver,
    session_key: &SessionKey,
    pause_epoch: Option<u64>,
    selector: ValueSelector,
    options: &ValueInspectionOptions,
    object_group: Option<&str>,
) -> Result<ValueSnapshot, TargetDebuggerError> {
    let (remote, selector) = match selector {
        ValueSelector::Expression {
            expression,
            allow_side_effects,
        } => {
            let remote = evaluate_remote(
                driver,
                session_key,
                pause_epoch,
                0,
                expression.clone(),
                allow_side_effects,
                options.max_preview_length,
                object_group,
            )
            .await?;
            (
                Some(remote),
                ValueSelector::Expression {
                    expression,
                    allow_side_effects,
                },
            )
        }
        ValueSelector::RemoteObject { object_id } => {
            if let Some(pause_epoch) = pause_epoch {
                require_pause(driver, session_key, pause_epoch)?;
            }
            (None, ValueSelector::RemoteObject { object_id })
        }
    };
    let object_id = remote
        .as_ref()
        .and_then(|value| value.remote.object_id.clone())
        .or_else(|| match &selector {
            ValueSelector::RemoteObject { object_id } => Some(object_id.clone()),
            ValueSelector::Expression { .. } => None,
        });
    let (properties, internal_properties) = match (&object_id, object_group) {
        (Some(_), Some(_)) => (Vec::new(), Vec::new()),
        (Some(object_id), None) => {
            get_object_property_descriptors(driver, session_key, pause_epoch, object_id.clone())
                .await?
        }
        (None, _) => (Vec::new(), Vec::new()),
    };
    let is_promise = remote
        .as_ref()
        .is_some_and(|value| value.remote.subtype == Some(RuntimeRemoteObjectSubtype::Promise))
        || has_live_promise_evidence(&internal_properties);
    let promise = if is_promise {
        object_id.as_ref().map(|object_id| {
            inspect_live_promise(
                object_id.clone(),
                internal_properties,
                options.max_preview_length,
            )
        })
    } else {
        None
    };
    let subtype = remote
        .as_ref()
        .and_then(|value| value.remote.subtype.as_ref())
        .and_then(serialized_enum_name);
    let class_name = remote
        .as_ref()
        .and_then(|value| value.remote.class_name.clone());
    let mut preview = remote.as_ref().map_or_else(
        || ValuePreviewSnapshot {
            kind: "object".to_owned(),
            preview: None,
            truncated: false,
            reference: object_id.clone(),
        },
        |value| remote_value_snapshot(&value.remote, options.max_preview_length),
    );
    preview.truncated |= remote.as_ref().is_some_and(|value| value.preview_truncated);
    let remote_preview = remote
        .as_ref()
        .and_then(|value| value.remote.preview.as_ref());
    let property_references = properties
        .iter()
        .filter_map(|property| {
            property
                .value
                .as_ref()
                .and_then(|value| value.object_id.clone())
                .map(|reference| (property.name.clone(), reference))
        })
        .collect::<BTreeMap<_, _>>();
    let (mut properties, preview_overflow) = if let Some(remote_preview) = remote_preview {
        let properties = remote_preview
            .properties
            .iter()
            .map(|property| {
                let raw_preview = property.value.clone().or_else(|| {
                    property
                        .value_preview
                        .as_ref()
                        .and_then(|preview| preview.description.clone())
                });
                let (preview, truncated) = raw_preview.map_or((None, false), |preview| {
                    let protocol_truncated = preview.contains('…');
                    let (preview, length_truncated) =
                        bounded_preview_text(&preview, options.max_preview_length);
                    (Some(preview), protocol_truncated || length_truncated)
                });
                ValuePropertySnapshot {
                    name: property.name.clone(),
                    value: ValuePreviewSnapshot {
                        kind: serialized_enum_name(&property.r#type)
                            .unwrap_or_else(|| "unknown".to_owned()),
                        preview,
                        truncated,
                        reference: property_references.get(&property.name).cloned(),
                    },
                }
            })
            .collect::<Vec<_>>();
        (properties, remote_preview.overflow)
    } else {
        (
            properties
                .into_iter()
                .filter_map(|property| {
                    property.value.map(|value| ValuePropertySnapshot {
                        name: property.name,
                        value: remote_value_snapshot(&value, options.max_preview_length),
                    })
                })
                .collect::<Vec<_>>(),
            false,
        )
    };
    let omitted_property_count = if preview_overflow {
        0
    } else {
        properties
            .len()
            .saturating_sub(options.max_properties as usize) as u64
    };
    let properties_truncated =
        preview_overflow || properties.len() > options.max_properties as usize;
    properties.truncate(options.max_properties as usize);
    Ok(ValueSnapshot {
        selector,
        subtype,
        class_name,
        preview,
        properties,
        omitted_property_count,
        properties_truncated,
        promise,
    })
}

fn bounded_preview_text(value: &str, max_length: u32) -> (String, bool) {
    let end = value
        .char_indices()
        .nth(max_length as usize)
        .map_or(value.len(), |(index, _)| index);
    (value[..end].to_owned(), end < value.len())
}

fn remote_object_kind(value: &RuntimeRemoteObject) -> String {
    serialized_enum_name(&value.r#type).unwrap_or_else(|| "unknown".to_owned())
}

fn exception_message(exception: &RuntimeExceptionDetails) -> String {
    exception
        .exception
        .as_ref()
        .and_then(|value| value.description.clone())
        .unwrap_or_else(|| format_exception_details(exception))
}

fn serialized_enum_name(value: &impl serde::Serialize) -> Option<String> {
    serde_json::to_value(value)
        .ok()
        .and_then(|value| value.as_str().map(ToOwned::to_owned))
}

fn format_exception_details<T>(exception: &T) -> String
where
    T: serde::Serialize + std::fmt::Debug,
{
    serde_json::to_string(exception).unwrap_or_else(|_| format!("{exception:?}"))
}

fn variable_snapshot(name: String, value: RuntimeRemoteObject) -> VariableSnapshot {
    let mut preview = remote_value_snapshot(
        &value,
        crate::promise_debugging::DEFAULT_VALUE_PREVIEW_LENGTH,
    );
    preview.reference = None;
    let kind = remote_object_kind(&value);
    VariableSnapshot {
        name,
        kind,
        value: value.value,
        unserializable_value: value.unserializable_value,
        description: value.description,
        object_id: value.object_id,
        preview,
    }
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
        .map(|(index, values)| ConsoleMessageSnapshot {
            index: *index,
            values: values.clone(),
        })
        .collect();
    for result_breakpoint in &mut result.breakpoints {
        let Some((_, breakpoint)) = driver.state().breakpoints.iter().find(|(key, _)| {
            key.client_id == context_id && key.breakpoint_id == result_breakpoint.id
        }) else {
            continue;
        };
        result_breakpoint.source = breakpoint.bindings.keys().find_map(|physical| {
            let confirmed_position = driver
                .state()
                .physical_breakpoints
                .get(physical)
                .and_then(|physical| physical.confirmed_position)
                .unwrap_or(physical.position);
            let (source_url, position, content) =
                driver.project_generated_position(&physical.script, confirmed_position)?;
            let location = SourceLocation {
                source_url: source_url.clone(),
                line: position.line.saturating_add(1),
                column: position.column.saturating_add(1),
            };
            let breadcrumb = driver.breadcrumb(
                &physical.script,
                &source_url,
                location.line,
                location.column,
                &content,
            );
            Some(source_excerpt(&source_url, &location, &content, breadcrumb))
        });
    }
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
                frame.breadcrumb = callback_aware_breadcrumb(
                    crate::language_intelligence::breadcrumb(
                        &location.source_url,
                        &content,
                        location.line,
                        location.column,
                    ),
                    &raw_frame.function_name,
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
            if frame.breadcrumb.is_none()
                && let Some(content) = driver.generated_source_content(&raw_frame.raw_script)
            {
                frame.breadcrumb = callback_aware_breadcrumb(
                    driver.breadcrumb(
                        &raw_frame.raw_script,
                        &frame.raw.source_url,
                        frame.raw.line,
                        frame.raw.column,
                        &content,
                    ),
                    &raw_frame.function_name,
                );
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
    let raw_highlight_length = remainder
        .chars()
        .take_while(|character| character.is_alphanumeric() || *character == '_')
        .count()
        .max(1) as u32;
    let current_display_text = expand_tabs(current_text, 4);
    let (current_excerpt, highlight_start, available_highlight) =
        window_highlighted_line(&current_display_text, display_column, 200);
    let highlight_length = raw_highlight_length.min(available_highlight.max(1));
    SourceExcerpt {
        source_url: source_url.to_owned(),
        breadcrumb,
        current_line: location.line,
        lines: (start..end)
            .map(|index| SourceExcerptLine {
                line: (index + 1) as u32,
                text: if index == current {
                    current_excerpt.clone()
                } else {
                    truncate_line(&expand_tabs(lines[index], 4), 200)
                },
            })
            .collect(),
        highlight_start,
        highlight_length,
    }
}

fn window_highlighted_line(
    line: &str,
    display_column: usize,
    maximum: usize,
) -> (String, u32, u32) {
    const OMISSION_MARK: &str = "...";

    let total_chars = line.chars().count();
    let start = display_column.saturating_sub(maximum / 2);
    let prefix = if start > 0 { OMISSION_MARK } else { "" };

    // Reserve room for a trailing marker only when the line actually keeps
    // going past what the remaining budget can show, so short/ordinary
    // lines (and lines truncated only by hitting the very end) stay
    // untouched.
    let budget_without_suffix = maximum.saturating_sub(prefix.len());
    let remaining_after_start = total_chars.saturating_sub(start);
    let needs_suffix = remaining_after_start > budget_without_suffix;
    let content_budget = if needs_suffix {
        budget_without_suffix.saturating_sub(OMISSION_MARK.len())
    } else {
        budget_without_suffix
    };

    let visible = line
        .chars()
        .skip(start)
        .take(content_budget)
        .collect::<String>();
    let suffix = if needs_suffix { OMISSION_MARK } else { "" };

    let highlight = prefix.len() + display_column.saturating_sub(start);
    let available = maximum
        .saturating_sub(suffix.len())
        .saturating_sub(highlight);
    (
        format!("{prefix}{visible}{suffix}"),
        highlight.saturating_add(1) as u32,
        available as u32,
    )
}

fn utf16_to_byte_and_display(line: &str, utf16_column: usize) -> (usize, usize) {
    let mut utf16 = 0;
    let mut display = 0;
    for (byte, character) in line.char_indices() {
        if utf16 >= utf16_column {
            return (byte, display);
        }
        utf16 += character.len_utf16();
        display += if character == '\t' {
            4 - display % 4
        } else {
            1
        };
    }

    (line.len(), display)
}

fn expand_tabs(line: &str, tab_width: usize) -> String {
    let mut expanded = String::with_capacity(line.len());
    let mut display = 0;
    for character in line.chars() {
        if character == '\t' {
            let spaces = tab_width - display % tab_width;
            expanded.extend(std::iter::repeat_n(' ', spaces));
            display += spaces;
        } else {
            expanded.push(character);
            display += 1;
        }
    }
    expanded
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
        TargetWaitPredicate::Changed { after_revision } => snapshot.revision > *after_revision,
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

fn breakpoint_wait_failure(
    snapshot: &TargetDebuggerSnapshot,
    predicate: &TargetWaitPredicate,
) -> Option<TargetDebuggerError> {
    let TargetWaitPredicate::BreakpointInstalled { breakpoint_id } = predicate else {
        return None;
    };
    let TargetBreakpointSnapshot {
        status: TargetBreakpointStatus::Failed { message },
        ..
    } = snapshot
        .breakpoints
        .iter()
        .find(|breakpoint| breakpoint.id == *breakpoint_id)?
    else {
        return None;
    };
    Some(TargetDebuggerError::BreakpointFailed {
        breakpoint_id: breakpoint_id.clone(),
        message: message.clone(),
    })
}

fn breakpoint_candidate_snapshot(
    candidate: &BreakpointSourceCandidate,
) -> BreakpointSourceCandidateSnapshot {
    BreakpointSourceCandidateSnapshot {
        source_url: candidate.source_url.clone(),
        content_hash: format!("{:?}", candidate.content.content),
        provenance: format!("{:?}", candidate.content.provenance),
    }
}

fn breakpoint_mapping_snapshot(
    source_url: &str,
    requested: Position,
    mapping: &BreakpointMapping,
) -> BreakpointMappingSnapshot {
    BreakpointMappingSnapshot {
        source_url: source_url.to_owned(),
        requested_line: requested.line.saturating_add(1),
        requested_column: requested.column.saturating_add(1),
        generated_url: mapping.generated_url.clone(),
        generated_line: mapping.generated_position.line.saturating_add(1),
        generated_column: mapping.generated_position.column.saturating_add(1),
        quality: mapping.quality.clone(),
        projection: mapping.projection.clone(),
    }
}

fn target_breakpoint_assessment_status(
    assessments: &[BreakpointScriptAssessmentSnapshot],
) -> TargetBreakpointStatus {
    if let Some((candidates, omitted_candidate_count)) =
        assessments
            .iter()
            .find_map(|assessment| match &assessment.status {
                BreakpointScriptAssessmentStatus::AmbiguousSource {
                    candidates,
                    omitted_candidate_count,
                } => Some((candidates.clone(), *omitted_candidate_count)),
                _ => None,
            })
    {
        return TargetBreakpointStatus::AmbiguousSource {
            candidates,
            omitted_candidate_count,
        };
    }
    let unmapped = assessments
        .iter()
        .filter_map(|assessment| match &assessment.status {
            BreakpointScriptAssessmentStatus::Unmapped { diagnostics, .. } => {
                Some(diagnostics.iter().cloned())
            }
            _ => None,
        })
        .flatten()
        .collect::<Vec<_>>();
    if !unmapped.is_empty() {
        return TargetBreakpointStatus::Unmapped {
            diagnostics: unmapped,
        };
    }
    let mapping_count = assessments
        .iter()
        .map(|assessment| match &assessment.status {
            BreakpointScriptAssessmentStatus::Applicable { mappings, .. } => mappings.len(),
            BreakpointScriptAssessmentStatus::Mapping { .. } => 1,
            _ => 0,
        })
        .sum::<usize>();
    if mapping_count > 0 {
        return TargetBreakpointStatus::Applicable {
            mapping_count: u32::try_from(mapping_count).unwrap_or(u32::MAX),
        };
    }
    if assessments.is_empty()
        || assessments.iter().any(|assessment| {
            matches!(
                assessment.status,
                BreakpointScriptAssessmentStatus::WaitingForScript
                    | BreakpointScriptAssessmentStatus::Mapping { .. }
            )
        })
    {
        return TargetBreakpointStatus::WaitingForScript;
    }
    if let Some(message) = assessments
        .iter()
        .find_map(|assessment| match &assessment.status {
            BreakpointScriptAssessmentStatus::Failed { message } => Some(message.clone()),
            _ => None,
        })
    {
        return TargetBreakpointStatus::Failed { message };
    }
    let diagnostics = assessments
        .iter()
        .filter_map(|assessment| match &assessment.status {
            BreakpointScriptAssessmentStatus::SourceNotFound { diagnostics } => {
                Some(diagnostics.iter().cloned())
            }
            _ => None,
        })
        .flatten()
        .collect();
    TargetBreakpointStatus::SourceNotFound { diagnostics }
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
            let assessments = breakpoint
                .assessments
                .iter()
                .filter_map(|(script_key, assessment)| {
                    let script = state.scripts.get(script_key)?;
                    Some(BreakpointScriptAssessmentSnapshot {
                        connection_id: connection_id.to_owned(),
                        target_id: target_id.to_owned(),
                        connection_generation,
                        script_id: script_key.script_id.clone(),
                        script_url: script.url.clone(),
                        script_version: assessment.script_version,
                        status: match &assessment.status {
                            BreakpointAssessmentStatus::WaitingForScript => {
                                BreakpointScriptAssessmentStatus::WaitingForScript
                            }
                            BreakpointAssessmentStatus::SourceNotFound { diagnostics } => {
                                BreakpointScriptAssessmentStatus::SourceNotFound {
                                    diagnostics: diagnostics.as_ref().clone(),
                                }
                            }
                            BreakpointAssessmentStatus::AmbiguousSource {
                                candidates,
                                omitted_candidate_count,
                            } => BreakpointScriptAssessmentStatus::AmbiguousSource {
                                candidates: candidates
                                    .iter()
                                    .map(breakpoint_candidate_snapshot)
                                    .collect(),
                                omitted_candidate_count: u32::try_from(*omitted_candidate_count)
                                    .unwrap_or(u32::MAX),
                            },
                            BreakpointAssessmentStatus::Mapping { candidate, .. } => {
                                BreakpointScriptAssessmentStatus::Mapping {
                                    candidate: breakpoint_candidate_snapshot(candidate),
                                }
                            }
                            BreakpointAssessmentStatus::Unmapped {
                                candidate,
                                diagnostics,
                            } => BreakpointScriptAssessmentStatus::Unmapped {
                                candidate: breakpoint_candidate_snapshot(candidate),
                                diagnostics: diagnostics.as_ref().clone(),
                            },
                            BreakpointAssessmentStatus::Applicable {
                                candidate,
                                mappings,
                            } => BreakpointScriptAssessmentStatus::Applicable {
                                candidate: breakpoint_candidate_snapshot(candidate),
                                mappings: mappings
                                    .iter()
                                    .map(|mapping| {
                                        breakpoint_mapping_snapshot(
                                            &breakpoint.source_url,
                                            breakpoint.position,
                                            mapping,
                                        )
                                    })
                                    .collect(),
                            },
                            BreakpointAssessmentStatus::Failed { message } => {
                                BreakpointScriptAssessmentStatus::Failed {
                                    message: message.clone(),
                                }
                            }
                        },
                    })
                })
                .collect::<Vec<_>>();
            let applications = breakpoint
                .bindings
                .iter()
                .filter_map(|(physical, binding)| {
                    let script = state.scripts.get(&physical.script)?;
                    let confirmed_position = state
                        .physical_breakpoints
                        .get(physical)
                        .and_then(|physical| physical.confirmed_position)
                        .unwrap_or(physical.position);
                    let mapping =
                        breakpoint
                            .assessments
                            .get(&physical.script)
                            .and_then(|assessment| match &assessment.status {
                                BreakpointAssessmentStatus::Applicable { mappings, .. } => mappings
                                    .iter()
                                    .find(|mapping| mapping.generated_position == physical.position)
                                    .map(|mapping| {
                                        breakpoint_mapping_snapshot(
                                            &breakpoint.source_url,
                                            breakpoint.position,
                                            mapping,
                                        )
                                    }),
                                _ => None,
                            });
                    Some(BreakpointApplicationSnapshot {
                        connection_id: connection_id.to_owned(),
                        target_id: target_id.to_owned(),
                        connection_generation,
                        script_id: physical.script.script_id.clone(),
                        script_url: script.url.clone(),
                        script_version: physical.script_version,
                        generated_line: confirmed_position.line.saturating_add(1),
                        generated_column: confirmed_position.column.saturating_add(1),
                        mapping,
                        status: match binding {
                            BreakpointBinding::WaitingForRemoval(_) => {
                                BreakpointApplicationStatus::Removing
                            }
                            BreakpointBinding::PendingInstall(_) => {
                                BreakpointApplicationStatus::Installing
                            }
                            BreakpointBinding::Installed { backend_id } => {
                                BreakpointApplicationStatus::Installed {
                                    backend_id: backend_id.clone(),
                                }
                            }
                            BreakpointBinding::Failed { message } => {
                                BreakpointApplicationStatus::Failed {
                                    message: message.clone(),
                                }
                            }
                        },
                    })
                })
                .collect::<Vec<_>>();
            let desired = breakpoint
                .assessments
                .iter()
                .flat_map(|(script, assessment)| match &assessment.status {
                    BreakpointAssessmentStatus::Applicable { mappings, .. } => mappings
                        .iter()
                        .map(|mapping| PhysicalBreakpointKey {
                            script: script.clone(),
                            script_version: assessment.script_version,
                            position: mapping.generated_position,
                            condition: breakpoint.condition.clone(),
                        })
                        .collect::<Vec<_>>(),
                    _ => Vec::new(),
                })
                .collect::<BTreeSet<_>>();
            let desired_installed = desired
                .iter()
                .filter(|physical| {
                    matches!(
                        breakpoint.bindings.get(*physical),
                        Some(BreakpointBinding::Installed { .. })
                    )
                })
                .count();
            let desired_failure =
                desired
                    .iter()
                    .find_map(|physical| match breakpoint.bindings.get(physical) {
                        Some(BreakpointBinding::Failed { message }) => Some(message.clone()),
                        _ => None,
                    });
            let desired_installing = desired.iter().any(|physical| {
                !matches!(
                    breakpoint.bindings.get(physical),
                    Some(BreakpointBinding::Installed { .. } | BreakpointBinding::Failed { .. })
                )
            });
            let assessment_pending = assessments.iter().any(|assessment| {
                matches!(
                    assessment.status,
                    BreakpointScriptAssessmentStatus::WaitingForScript
                        | BreakpointScriptAssessmentStatus::Mapping { .. }
                )
            });
            let status = if desired_installing {
                TargetBreakpointStatus::Installing {
                    application_count: u32::try_from(desired.len()).unwrap_or(u32::MAX),
                }
            } else if desired_installed > 0 {
                TargetBreakpointStatus::Installed {
                    binding_count: u32::try_from(desired_installed).unwrap_or(u32::MAX),
                }
            } else if assessment_pending {
                target_breakpoint_assessment_status(&assessments)
            } else if let Some(message) = desired_failure {
                TargetBreakpointStatus::Failed { message }
            } else {
                target_breakpoint_assessment_status(&assessments)
            };
            TargetBreakpointSnapshot {
                id: key.breakpoint_id.clone(),
                source_url: breakpoint.source_url.clone(),
                line: breakpoint.position.line.saturating_add(1),
                column: breakpoint.position.column.saturating_add(1),
                status,
                source: None,
                assessments,
                applications,
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
                        scopes: frame
                            .scopes
                            .iter()
                            .enumerate()
                            .map(|(index, scope)| ScopeSnapshot {
                                index: u32::try_from(index).unwrap_or(u32::MAX),
                                kind: scope.kind.clone(),
                                name: scope.name.clone(),
                            })
                            .collect(),
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
    #[error(transparent)]
    SourceSearch(#[from] SearchError),
    #[error("breakpoint line and column must be one-based")]
    InvalidBreakpointPosition,
    #[error("the debugger session is no longer available")]
    SessionMissing,
    #[error("pause epoch {0} is stale")]
    StalePause(u64),
    #[error("frame {0} does not exist in the current pause")]
    FrameNotFound(u32),
    #[error("scope {0} does not exist in the selected frame")]
    ScopeNotFound(u32),
    #[error("evaluation failed: {0}")]
    Evaluation(String),
    #[error("property inspection failed: {0}")]
    Properties(String),
    #[error("invalid value inspection: {0}")]
    InvalidValueInspection(String),
    #[error("interaction failed: {0}")]
    Interaction(String),
    #[error("screenshot capture failed: {0}")]
    Screenshot(String),
    #[error("selector '{0}' did not match an element")]
    SelectorNotFound(String),
    #[error("unsupported key chord '{0}'")]
    UnsupportedKeyChord(String),
    #[error("coverage failed: {0}")]
    Coverage(String),
    #[error("heap snapshot failed: {0}")]
    HeapSnapshot(String),
    #[error("heap capture '{0}' does not exist")]
    HeapCaptureNotFound(String),
    #[error("invalid heap class filter: {0}")]
    InvalidHeapFilter(String),
    #[error("invalid heap selector: {0}")]
    InvalidHeapSelector(String),
    #[error("invalid heap reference: {0}")]
    InvalidHeapReference(String),
    #[error("heap object id '{0}' does not exist in the capture")]
    HeapNodeNotFound(String),
    #[error("heap analysis failed: {0}")]
    HeapAnalysis(String),
    #[error("heap captures '{older}' and '{newer}' are incompatible")]
    IncompatibleHeapCaptures { older: String, newer: String },
    #[error("coverage recording is already active")]
    CoverageAlreadyActive,
    #[error("coverage recording is not active")]
    CoverageNotActive,
    #[error("coverage capture '{0}' does not exist in the active recording")]
    CoverageCaptureNotFound(String),
    #[error("coverage capture '{0}' already exists in the active recording")]
    CoverageCaptureAlreadyExists(String),
    #[error("CPU profiling failed: {0}")]
    CpuProfile(String),
    #[error("CPU profile recording is already active")]
    CpuProfileAlreadyActive,
    #[error("CPU profile recording is not active")]
    CpuProfileNotActive,
    #[error("CPU profile capture '{0}' does not exist")]
    CpuProfileCaptureNotFound(String),
    #[error("CPU profile capture '{0}' already exists")]
    CpuProfileCaptureAlreadyExists(String),
    #[error("CPU profile sampling interval must be between 1us and 2147483647us")]
    InvalidCpuProfileSamplingInterval,
    #[error("invalid CPU profile: {0}")]
    InvalidCpuProfile(String),
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
    #[error("batch installation failed ({install}) and rollback also failed ({rollback})")]
    BatchRollback { install: String, rollback: String },
    #[error("target wait timeout must be between 1ms and 5 minutes")]
    InvalidTimeout,
}

fn callback_aware_breadcrumb(
    breadcrumb: Option<String>,
    runtime_function_name: &str,
) -> Option<String> {
    breadcrumb.map(|breadcrumb| {
        if (runtime_function_name.is_empty() || runtime_function_name == "(anonymous)")
            && !breadcrumb.ends_with(" → callback")
        {
            format!("{breadcrumb} → callback")
        } else {
            breadcrumb
        }
    })
}

fn generated_script_callback_breadcrumb(
    source_effects: &SourceEffectInterpreter,
    state: &DebuggerState,
    script: &ScriptKey,
    location: &SourceLocation,
    runtime_function_name: &str,
) -> Option<String> {
    let content = source_effects.generated_source_content(state, script)?;
    callback_aware_breadcrumb(
        source_effects.breadcrumb(
            state,
            script,
            &location.source_url,
            location.line,
            location.column,
            &content,
        ),
        runtime_function_name,
    )
}

#[cfg(test)]
mod tests {
    use super::{
        TargetDebuggerError, TargetDebuggerHandle, aggregate_cpu_profile, bounded_heap_text,
        bounded_projection_function, breakpoint_wait_failure, callback_aware_breadcrumb,
        complete_source_search_batch, effective_coverage_ranges, evaluated_remote_from_envelope,
        heap_class_display_name, predicate_matches, publish_snapshot, snapshot, source_excerpt,
        window_highlighted_line,
    };
    use crate::cdp::{
        RuntimePropertyDescriptor, RuntimeRemoteObject, RuntimeRemoteObjectType,
        TargetAttachToTargetParams, TargetCloseTargetParams, TargetCreateTargetParams,
    };
    use crate::cdp_runtime::CdpConnection;
    use crate::content_store::ContentStore;
    use crate::context_source_model::ContextSourceModel;
    use crate::debugger_engine::{
        self, BreakpointAssessment, BreakpointAssessmentStatus, BreakpointBinding, BreakpointKey,
        BreakpointMapping, BreakpointSourceCandidate, BreakpointState, DebuggerState, EffectId,
        Input, PhysicalBreakpointKey, ScriptKey, ScriptSourceState, ScriptState, SessionKey,
    };
    use crate::service_api::{
        BreakpointApplicationStatus, CoverageRangeSnapshot, CpuProfileCallFrameSnapshot,
        CpuProfileNodeSnapshot, CpuProfileSnapshot, SourceExcerpt, SourceLocation,
        TargetBreakpointStatus, TargetDebuggerPhase, TargetDebuggerSnapshot, TargetWaitPredicate,
        ValueInspectionOptions, ValueSelector, ValueSnapshot,
    };
    use crate::source_search::{SearchControl, SearchError};
    use crate::source_view::{ContentCandidate, Position, Provenance};
    use crate::websocket_transport::CdpWebSocketTransport;
    use std::collections::BTreeMap;
    use std::sync::Arc;

    #[test]
    fn rejects_non_retained_existing_remote_object_inspection() {
        let error = TargetDebuggerHandle::validate_value_inspection(
            &ValueSelector::RemoteObject {
                object_id: "remote-1".to_owned(),
            },
            &ValueInspectionOptions {
                max_preview_length: 120,
                max_properties: 20,
                retain_references: false,
            },
        )
        .unwrap_err();
        assert!(matches!(
            error,
            TargetDebuggerError::InvalidValueInspection(_)
        ));
    }
    use std::time::Duration;

    fn range(start_offset: u32, end_offset: u32, count: u64) -> CoverageRangeSnapshot {
        CoverageRangeSnapshot {
            start_offset,
            end_offset,
            count,
            authored_start: None,
            authored_end: None,
        }
    }

    fn target_snapshot(phase: TargetDebuggerPhase) -> TargetDebuggerSnapshot {
        TargetDebuggerSnapshot {
            context_id: "test".to_owned(),
            connection_id: "browser".to_owned(),
            target_id: "page".to_owned(),
            connection_generation: 1,
            revision: 1,
            phase,
            scripts: Vec::new(),
            breakpoints: Vec::new(),
            logs: Vec::new(),
            pause: None,
        }
    }

    #[test]
    fn bounded_primitive_envelopes_restore_remote_object_shape() {
        let cases = [
            (
                "string",
                RuntimeRemoteObjectType::String,
                Some("hello"),
                None,
                None,
            ),
            (
                "bigint",
                RuntimeRemoteObjectType::Bigint,
                None,
                Some("99999"),
                Some("99999"),
            ),
            (
                "number",
                RuntimeRemoteObjectType::Number,
                None,
                Some("-Infinity"),
                Some("-Infinity"),
            ),
        ];
        for (kind, expected_type, value, unserializable, description) in cases {
            let projection = evaluated_remote_from_envelope(&[
                envelope_property("__jsdbgKind", serde_json::json!(kind)),
                envelope_property(
                    "__jsdbgText",
                    serde_json::json!(value.or(unserializable).or(description).unwrap()),
                ),
                envelope_property("__jsdbgTruncated", serde_json::json!(true)),
            ])
            .unwrap();
            assert_eq!(projection.remote.r#type, expected_type);
            assert_eq!(
                projection
                    .remote
                    .value
                    .as_ref()
                    .and_then(|value| value.as_str()),
                value
            );
            assert_eq!(
                projection.remote.unserializable_value.as_deref(),
                unserializable
            );
            assert_eq!(projection.remote.description.as_deref(), description);
            assert!(projection.preview_truncated);
        }
    }

    #[test]
    fn bounded_symbol_envelope_omits_untrusted_description() {
        let projection = evaluated_remote_from_envelope(&[
            envelope_property("__jsdbgKind", serde_json::json!("symbol")),
            envelope_property("__jsdbgTruncated", serde_json::json!(true)),
        ])
        .unwrap();
        assert_eq!(projection.remote.r#type, RuntimeRemoteObjectType::Symbol);
        assert!(projection.remote.description.is_none());
        assert!(projection.preview_truncated);
    }

    #[test]
    fn bounded_projection_avoids_mutable_primitive_dispatch() {
        let source = bounded_projection_function(120);
        assert!(!source.contains(" for "));
        assert!(!source.contains(" of "));
        assert!(!source.contains(".description"));
        assert!(!source.contains(".charAt"));
        assert!(!source.contains(".charCodeAt"));
        assert!(!source.contains(".codePointAt"));
        assert!(!source.contains(".slice"));
        assert!(source.contains("__jsdbgText[__jsdbgOffset]"));
        assert!(source.contains(r#""\uD800""#));
        assert!(source.contains(r#""\uDC00""#));
    }

    #[test]
    fn remote_envelope_preserves_safe_primitive_payload() {
        let projection = evaluated_remote_from_envelope(&[
            envelope_property("__jsdbgKind", serde_json::json!("remote")),
            envelope_property("__jsdbgValue", serde_json::json!(true)),
        ])
        .unwrap();
        assert_eq!(projection.remote.r#type, RuntimeRemoteObjectType::Boolean);
        assert_eq!(projection.remote.value, Some(serde_json::json!(true)));
        assert!(!projection.preview_truncated);
    }

    fn envelope_property(name: &str, value: serde_json::Value) -> RuntimePropertyDescriptor {
        let mut remote = RuntimeRemoteObject::new(match &value {
            serde_json::Value::Bool(_) => RuntimeRemoteObjectType::Boolean,
            _ => RuntimeRemoteObjectType::String,
        });
        remote.value = Some(value);
        let mut property = RuntimePropertyDescriptor::new(name.to_owned(), true, true);
        property.value = Some(remote);
        property
    }

    fn replacement_snapshot(desired_binding: BreakpointBinding) -> TargetDebuggerSnapshot {
        let connected =
            debugger_engine::reduce(&Arc::new(DebuggerState::default()), Input::Connected);
        let attached = debugger_engine::reduce(
            &connected.state,
            Input::SessionAttached {
                session_id: "session".into(),
                target_id: "target".into(),
                parent_session_id: None,
                waiting_for_debugger: false,
            },
        );
        let session = attached.state.sessions.keys().next().unwrap().clone();
        let configured = debugger_engine::reduce(
            &attached.state,
            Input::SessionConfigured {
                effect_id: attached.effects[0].effect_id(),
            },
        );
        let mut state = (*configured.state).clone();
        let script = ScriptKey {
            session: session.clone(),
            script_id: "script".into(),
        };
        Arc::make_mut(&mut state.scripts).insert(
            script.clone(),
            Arc::new(ScriptState {
                url: "bundle.js".into(),
                hash: "hash".into(),
                source_map_url: Some("bundle.js.map".into()),
                version: 1,
                source: ScriptSourceState::Unresolved,
            }),
        );
        let old_physical = PhysicalBreakpointKey {
            script: script.clone(),
            script_version: 1,
            position: Position { line: 1, column: 1 },
            condition: None,
        };
        let new_position = Position { line: 2, column: 2 };
        let new_physical = PhysicalBreakpointKey {
            script: script.clone(),
            script_version: 1,
            position: new_position,
            condition: None,
        };
        let content = ContentCandidate {
            content: ContentStore::default().intern("source"),
            provenance: Provenance::Workspace {
                logical_url: "app.ts".into(),
            },
        };
        Arc::make_mut(&mut state.breakpoints).insert(
            BreakpointKey {
                client_id: "context".into(),
                breakpoint_id: "replacement".into(),
            },
            Arc::new(BreakpointState {
                generation: 1,
                source_url: "app.ts".into(),
                position: Position::ZERO,
                condition: None,
                friendly_candidate_selected: false,
                candidate_index: Arc::new(Default::default()),
                pending_mappings: Arc::new(BTreeMap::new()),
                assessments: Arc::new(
                    BTreeMap::from([(
                        script,
                        BreakpointAssessment {
                            script_version: 1,
                            status: BreakpointAssessmentStatus::Applicable {
                                candidate: BreakpointSourceCandidate {
                                    source_url: "app.ts".into(),
                                    content,
                                },
                                mappings: Arc::new(vec![BreakpointMapping {
                                    generated_position: new_position,
                                    quality: "exact".into(),
                                    generated_url: "bundle.js".into(),
                                    projection: vec!["source map".into()],
                                }]),
                            },
                        },
                    )])
                    .into(),
                ),
                bindings: Arc::new(BTreeMap::from([
                    (
                        old_physical,
                        BreakpointBinding::Installed {
                            backend_id: "backend-fallback".into(),
                        },
                    ),
                    (new_physical, desired_binding),
                ])),
            }),
        );
        snapshot("context", "connection", "target", 1, &session, &state)
    }

    #[derive(Clone, Copy)]
    enum MixedCandidateState {
        WaitingForScript,
        Mapping,
        Installing,
        Installed,
    }

    fn mixed_failure_snapshot(candidate_state: MixedCandidateState) -> TargetDebuggerSnapshot {
        let connected =
            debugger_engine::reduce(&Arc::new(DebuggerState::default()), Input::Connected);
        let attached = debugger_engine::reduce(
            &connected.state,
            Input::SessionAttached {
                session_id: "session".into(),
                target_id: "target".into(),
                parent_session_id: None,
                waiting_for_debugger: false,
            },
        );
        let session = attached.state.sessions.keys().next().unwrap().clone();
        let configured = debugger_engine::reduce(
            &attached.state,
            Input::SessionConfigured {
                effect_id: attached.effects[0].effect_id(),
            },
        );
        let mut state = (*configured.state).clone();
        let failed_script = ScriptKey {
            session: session.clone(),
            script_id: "failed".into(),
        };
        let candidate_script = ScriptKey {
            session: session.clone(),
            script_id: "candidate".into(),
        };
        for script in [&failed_script, &candidate_script] {
            Arc::make_mut(&mut state.scripts).insert(
                script.clone(),
                Arc::new(ScriptState {
                    url: format!("{}.js", script.script_id),
                    hash: format!("{}-hash", script.script_id),
                    source_map_url: Some(format!("{}.js.map", script.script_id)),
                    version: 1,
                    source: ScriptSourceState::Unresolved,
                }),
            );
        }
        let failed_position = Position { line: 1, column: 1 };
        let candidate_position = Position { line: 2, column: 2 };
        let content = ContentStore::default().intern("source");
        let candidate = |script: &ScriptKey| BreakpointSourceCandidate {
            source_url: "app.ts".into(),
            content: ContentCandidate {
                content: content.clone(),
                provenance: Provenance::Workspace {
                    logical_url: format!("{}:app.ts", script.script_id),
                },
            },
        };
        let mapping = |position| {
            Arc::new(vec![BreakpointMapping {
                generated_position: position,
                quality: "exact".into(),
                generated_url: "bundle.js".into(),
                projection: vec!["source map".into()],
            }])
        };
        let (candidate_status, candidate_binding, pending_mapping) = match candidate_state {
            MixedCandidateState::WaitingForScript => {
                (BreakpointAssessmentStatus::WaitingForScript, None, None)
            }
            MixedCandidateState::Mapping => (
                BreakpointAssessmentStatus::Mapping {
                    effect_id: EffectId(12),
                    candidate: candidate(&candidate_script),
                },
                None,
                Some(EffectId(12)),
            ),
            MixedCandidateState::Installing => (
                BreakpointAssessmentStatus::Applicable {
                    candidate: candidate(&candidate_script),
                    mappings: mapping(candidate_position),
                },
                Some(BreakpointBinding::PendingInstall(EffectId(13))),
                None,
            ),
            MixedCandidateState::Installed => (
                BreakpointAssessmentStatus::Applicable {
                    candidate: candidate(&candidate_script),
                    mappings: mapping(candidate_position),
                },
                Some(BreakpointBinding::Installed {
                    backend_id: "backend-success".into(),
                }),
                None,
            ),
        };
        let failed_physical = PhysicalBreakpointKey {
            script: failed_script.clone(),
            script_version: 1,
            position: failed_position,
            condition: None,
        };
        let candidate_physical = PhysicalBreakpointKey {
            script: candidate_script.clone(),
            script_version: 1,
            position: candidate_position,
            condition: None,
        };
        let mut bindings = BTreeMap::from([(
            failed_physical,
            BreakpointBinding::Failed {
                message: "first script failed".into(),
            },
        )]);
        if let Some(binding) = candidate_binding {
            bindings.insert(candidate_physical, binding);
        }
        Arc::make_mut(&mut state.breakpoints).insert(
            BreakpointKey {
                client_id: "context".into(),
                breakpoint_id: "mixed".into(),
            },
            Arc::new(BreakpointState {
                generation: 1,
                source_url: "app.ts".into(),
                position: Position::ZERO,
                condition: None,
                friendly_candidate_selected: false,
                candidate_index: Arc::new(Default::default()),
                pending_mappings: Arc::new(
                    pending_mapping
                        .map(|effect_id| BTreeMap::from([(candidate_script.clone(), effect_id)]))
                        .unwrap_or_default(),
                ),
                assessments: Arc::new(
                    BTreeMap::from([
                        (
                            failed_script.clone(),
                            BreakpointAssessment {
                                script_version: 1,
                                status: BreakpointAssessmentStatus::Applicable {
                                    candidate: candidate(&failed_script),
                                    mappings: mapping(failed_position),
                                },
                            },
                        ),
                        (
                            candidate_script,
                            BreakpointAssessment {
                                script_version: 1,
                                status: candidate_status,
                            },
                        ),
                    ])
                    .into(),
                ),
                bindings: Arc::new(bindings),
            }),
        );
        snapshot("context", "connection", "target", 1, &session, &state)
    }

    #[test]
    fn replacement_status_ignores_installed_fallback_while_desired_binding_is_pending() {
        let snapshot = replacement_snapshot(BreakpointBinding::PendingInstall(EffectId(99)));
        let breakpoint = &snapshot.breakpoints[0];
        assert!(matches!(
            breakpoint.status,
            TargetBreakpointStatus::Installing {
                application_count: 1
            }
        ));
        assert!(breakpoint.applications.iter().any(|application| matches!(
            application.status,
            BreakpointApplicationStatus::Installed { ref backend_id }
                if backend_id == "backend-fallback"
        )));
        let predicate = TargetWaitPredicate::BreakpointInstalled {
            breakpoint_id: "replacement".into(),
        };
        assert!(!predicate_matches(&snapshot, &predicate));
        assert!(breakpoint_wait_failure(&snapshot, &predicate).is_none());
    }

    #[test]
    fn replacement_failure_wakes_waiter_as_failure_despite_installed_fallback() {
        let snapshot = replacement_snapshot(BreakpointBinding::Failed {
            message: "replacement failed".into(),
        });
        assert!(matches!(
            snapshot.breakpoints[0].status,
            TargetBreakpointStatus::Failed { ref message }
                if message == "replacement failed"
        ));
        let predicate = TargetWaitPredicate::BreakpointInstalled {
            breakpoint_id: "replacement".into(),
        };
        assert!(!predicate_matches(&snapshot, &predicate));
        assert!(matches!(
            breakpoint_wait_failure(&snapshot, &predicate),
            Some(super::TargetDebuggerError::BreakpointFailed {
                breakpoint_id,
                message,
            }) if breakpoint_id == "replacement" && message == "replacement failed"
        ));

        let installed = replacement_snapshot(BreakpointBinding::Installed {
            backend_id: "backend-new".into(),
        });
        assert!(predicate_matches(&installed, &predicate));
        assert!(breakpoint_wait_failure(&installed, &predicate).is_none());
    }

    #[test]
    fn failed_candidate_does_not_end_wait_while_another_candidate_can_succeed() {
        let predicate = TargetWaitPredicate::BreakpointInstalled {
            breakpoint_id: "mixed".into(),
        };
        for candidate_state in [
            MixedCandidateState::WaitingForScript,
            MixedCandidateState::Mapping,
            MixedCandidateState::Installing,
        ] {
            let snapshot = mixed_failure_snapshot(candidate_state);
            assert!(!predicate_matches(&snapshot, &predicate));
            assert!(
                breakpoint_wait_failure(&snapshot, &predicate).is_none(),
                "pending or viable candidate must keep the waiter alive: {:?}",
                snapshot.breakpoints[0].status
            );
        }

        let installed = mixed_failure_snapshot(MixedCandidateState::Installed);
        assert!(matches!(
            installed.breakpoints[0].status,
            TargetBreakpointStatus::Installed { binding_count: 1 }
        ));
        assert!(predicate_matches(&installed, &predicate));
        assert!(breakpoint_wait_failure(&installed, &predicate).is_none());
    }

    #[tokio::test]
    async fn abandoned_and_cancelled_search_batches_skip_hydration() {
        let control = SearchControl::default();
        let (response, receiver) = tokio::sync::oneshot::channel();
        drop(receiver);
        let mut hydrated = false;
        complete_source_search_batch(response, &control, || {
            hydrated = true;
            Ok(())
        });
        assert!(!hydrated, "a closed response must skip queued hydration");

        let cancelled = SearchControl::default();
        cancelled.cancel();
        let (response, receiver) = tokio::sync::oneshot::channel();
        let mut hydrated = false;
        complete_source_search_batch(response, &cancelled, || {
            hydrated = true;
            Ok(())
        });
        assert!(!hydrated, "a cancelled request must skip queued hydration");
        assert!(matches!(
            receiver.await.unwrap(),
            Err(super::TargetDebuggerError::SourceSearch(
                SearchError::Cancelled
            ))
        ));

        let expired = SearchControl::with_deadline(std::time::Instant::now());
        let (response, receiver) = tokio::sync::oneshot::channel();
        let mut hydrated = false;
        complete_source_search_batch(response, &expired, || {
            hydrated = true;
            Ok(())
        });
        assert!(!hydrated, "an expired request must skip queued hydration");
        assert!(matches!(
            receiver.await.unwrap(),
            Err(super::TargetDebuggerError::SourceSearch(
                SearchError::DeadlineExceeded
            ))
        ));
    }

    #[tokio::test(flavor = "multi_thread")]
    #[ignore = "requires CDP_WS_ENDPOINT for Playwright-launched Chromium"]
    async fn live_evaluation_bounds_large_primitive_transfer() {
        tokio::time::timeout(Duration::from_secs(120), async {
            let endpoint =
                std::env::var("CDP_WS_ENDPOINT").expect("Playwright provides CDP_WS_ENDPOINT");
            let transport = Arc::new(
                CdpWebSocketTransport::connect(&endpoint)
                    .await
                    .expect("connect WebSocket transport"),
            );
            let connection = CdpConnection::connect_transport(transport.clone())
                .await
                .expect("connect to Chromium");
            let root = connection.root();
            let created = root
                .target_create_target(TargetCreateTargetParams::new("about:blank".into()))
                .await
                .expect("create target");
            let mut attach = TargetAttachToTargetParams::new(created.target_id.clone());
            attach.flatten = Some(true);
            let attached = root
                .target_attach_to_target(attach)
                .await
                .expect("attach target");
            let session_key = SessionKey {
                connection_generation: 1,
                session_id: attached.session_id,
            };
            let debugger = TargetDebuggerHandle::start(
                "bounded-evaluation".into(),
                "browser".into(),
                created.target_id.clone(),
                1,
                connection
                    .open_session(session_key.clone())
                    .expect("open target session"),
                session_key,
                false,
                Arc::new(ContextSourceModel::new()),
            )
            .await
            .expect("start target debugger");

            let number = inspect_live(&debugger, "6 * 7", true)
                .await
                .expect("number evaluates");
            assert_eq!(number.preview.kind, "number");
            assert_eq!(number.preview.preview.as_deref(), Some("42"));

            let string = inspect_live(&debugger, "'ordinary string'", true)
                .await
                .expect("string evaluates");
            assert_eq!(string.preview.kind, "string");
            assert_eq!(string.preview.preview.as_deref(), Some("ordinary string"));
            assert!(!string.preview.truncated);

            let nan = inspect_live(&debugger, "NaN", true)
                .await
                .expect("NaN evaluates");
            assert_eq!(nan.preview.kind, "number");
            assert_eq!(nan.preview.preview.as_deref(), Some("NaN"));
            let bigint = inspect_live(&debugger, "12345678901234567890n", true)
                .await
                .expect("bigint evaluates");
            assert_eq!(bigint.preview.kind, "bigint");
            assert_eq!(
                bigint.preview.preview.as_deref(),
                Some("12345678901234567890n")
            );

            inspect_live(&debugger, "setTimeout(() => { debugger; }, 0)", true)
                .await
                .expect("schedule debugger pause");
            let paused = debugger
                .wait(
                    TargetWaitPredicate::Paused { after_epoch: 0 },
                    Duration::from_secs(5),
                )
                .await
                .expect("target pauses");
            let pause_epoch = match paused.phase {
                TargetDebuggerPhase::Paused { epoch } => epoch,
                phase => panic!("expected paused target, got {phase:?}"),
            };
            let frame_value = debugger
                .inspect_value(
                    Some(pause_epoch),
                    ValueSelector::Expression {
                        expression: "21 * 2".to_owned(),
                        allow_side_effects: true,
                    },
                    ValueInspectionOptions {
                        max_preview_length: 120,
                        max_properties: 20,
                        retain_references: false,
                    },
                )
                .await
                .expect("pause-frame expression evaluates");
            assert_eq!(frame_value.preview.preview.as_deref(), Some("42"));
            debugger.resume(pause_epoch).await.expect("target resumes");

            inspect_live(&debugger, "globalThis.__jsdbgEvaluationCount = 0", true)
                .await
                .expect("counter initializes");
            transport.reset_largest_received_message_size();
            let huge = inspect_live(
                &debugger,
                "(globalThis.__jsdbgEvaluationCount++, 'x'.repeat(16 * 1024 * 1024))",
                true,
            )
            .await
            .expect("huge string evaluates");
            let expected_preview = "x".repeat(120);
            assert_eq!(huge.preview.kind, "string");
            assert_eq!(
                huge.preview.preview.as_deref(),
                Some(expected_preview.as_str())
            );
            assert!(huge.preview.truncated);
            assert!(huge.properties.is_empty());
            assert!(transport.largest_received_message_size() < 64 * 1024);
            assert!(serde_json::to_vec(&huge).unwrap().len() < 2_048);
            assert_no_references(&huge);
            let count = inspect_live(&debugger, "globalThis.__jsdbgEvaluationCount", true)
                .await
                .expect("counter reads");
            assert_eq!(count.preview.preview.as_deref(), Some("1"));

            inspect_live(&debugger, "globalThis.__jsdbgEvaluationCount = 0", true)
                .await
                .expect("counter resets");
            transport.reset_largest_received_message_size();
            let huge_bigint = inspect_live(
                &debugger,
                "(globalThis.__jsdbgEvaluationCount++, BigInt('9'.repeat(1_000_000)))",
                true,
            )
            .await
            .expect("million-digit BigInt evaluates");
            let expected_bigint_preview = "9".repeat(120);
            assert_eq!(huge_bigint.preview.kind, "bigint");
            assert_eq!(
                huge_bigint.preview.preview.as_deref(),
                Some(expected_bigint_preview.as_str())
            );
            assert!(huge_bigint.preview.truncated);
            assert!(transport.largest_received_message_size() < 64 * 1024);
            assert!(serde_json::to_vec(&huge_bigint).unwrap().len() < 2_048);
            assert_no_references(&huge_bigint);
            let count = inspect_live(&debugger, "globalThis.__jsdbgEvaluationCount", true)
                .await
                .expect("counter reads");
            assert_eq!(count.preview.preview.as_deref(), Some("1"));

            transport.reset_largest_received_message_size();
            let huge_symbol = inspect_live(&debugger, "Symbol('s'.repeat(1_000_000))", true)
                .await
                .expect("huge Symbol description evaluates");
            assert_eq!(huge_symbol.preview.kind, "symbol");
            assert!(huge_symbol.preview.preview.is_none());
            assert!(huge_symbol.preview.truncated);
            assert!(transport.largest_received_message_size() < 64 * 1024);
            assert!(serde_json::to_vec(&huge_symbol).unwrap().len() < 2_048);
            assert_no_references(&huge_symbol);

            let object = inspect_live(&debugger, "({ answer: 42, label: 'ok' })", true)
                .await
                .expect("object evaluates");
            assert_eq!(object.preview.kind, "object");
            assert!(
                object
                    .properties
                    .iter()
                    .any(|property| property.name == "answer")
            );
            assert_no_references(&object);

            let promise = inspect_live(&debugger, "Promise.resolve(42)", true)
                .await
                .expect("promise evaluates");
            assert_eq!(promise.subtype.as_deref(), Some("promise"));
            assert_eq!(promise.class_name.as_deref(), Some("Promise"));
            assert!(promise.promise.is_some());
            assert_no_references(&promise);

            let thrown = inspect_live(
                &debugger,
                "(() => { throw new Error('bounded-evaluation-error') })()",
                true,
            )
            .await
            .expect_err("thrown expression fails");
            assert!(thrown.to_string().contains("bounded-evaluation-error"));

            let pure = inspect_live(&debugger, "'side-effect-free'", false)
                .await
                .expect("pure generic value evaluation succeeds");
            assert_eq!(pure.preview.preview.as_deref(), Some("side-effect-free"));
            assert!(
                inspect_live(
                    &debugger,
                    "globalThis.__jsdbgForbiddenSideEffect = true",
                    false,
                )
                .await
                .is_err()
            );

            inspect_live(
                &debugger,
                r#"(() => {
  globalThis.__jsdbgEvaluationCount = 0;
  globalThis.__jsdbgPoisonedUnicode = "😀".repeat(1_000_000);
  globalThis.__jsdbgPoisonedBigInt = BigInt("8".repeat(1_000_000));
  globalThis.__jsdbgPoisonedSymbol = Symbol("z".repeat(1_000_000));
  const poison = function() { for (;;) {} };
  for (const name of [
    "charAt", "charCodeAt", "codePointAt", "slice", "substring", "substr",
    "toString", "valueOf"
  ]) {
    Object.defineProperty(String.prototype, name, {
      value: poison,
      configurable: true
    });
  }
  Object.defineProperty(String.prototype, Symbol.iterator, {
    value: poison,
    configurable: true
  });
  Object.defineProperty(String.prototype, Symbol.toPrimitive, {
    value: poison,
    configurable: true
  });
  Object.defineProperty(String.prototype, "0", {
    get: poison,
    configurable: true
  });
  for (const name of ["toString", "valueOf"]) {
    Object.defineProperty(BigInt.prototype, name, {
      value: poison,
      configurable: true
    });
    Object.defineProperty(Symbol.prototype, name, {
      value: poison,
      configurable: true
    });
  }
  Object.defineProperty(BigInt.prototype, Symbol.toPrimitive, {
    value: poison,
    configurable: true
  });
  Object.defineProperty(Symbol.prototype, Symbol.toPrimitive, {
    value: poison,
    configurable: true
  });
  Object.defineProperty(Symbol.prototype, "description", {
    get: poison,
    configurable: true
  });
  return true;
})()"#,
                true,
            )
            .await
            .expect("install nonterminating primitive prototype poison");

            transport.reset_largest_received_message_size();
            let poisoned_unicode = inspect_live(
                &debugger,
                "(globalThis.__jsdbgEvaluationCount++, globalThis.__jsdbgPoisonedUnicode)",
                true,
            )
            .await
            .expect("poisoned Unicode string evaluates");
            assert_eq!(poisoned_unicode.preview.kind, "string");
            assert_eq!(
                poisoned_unicode.preview.preview.as_deref(),
                Some("😀".repeat(120).as_str())
            );
            assert!(poisoned_unicode.preview.truncated);
            assert!(transport.largest_received_message_size() < 64 * 1024);
            assert!(serde_json::to_vec(&poisoned_unicode).unwrap().len() < 2_048);

            transport.reset_largest_received_message_size();
            let poisoned_bigint = inspect_live(
                &debugger,
                "(globalThis.__jsdbgEvaluationCount++, globalThis.__jsdbgPoisonedBigInt)",
                true,
            )
            .await
            .expect("poisoned BigInt evaluates");
            assert_eq!(poisoned_bigint.preview.kind, "bigint");
            assert_eq!(
                poisoned_bigint.preview.preview.as_deref(),
                Some("8".repeat(120).as_str())
            );
            assert!(poisoned_bigint.preview.truncated);
            assert!(transport.largest_received_message_size() < 64 * 1024);
            assert!(serde_json::to_vec(&poisoned_bigint).unwrap().len() < 2_048);

            transport.reset_largest_received_message_size();
            let poisoned_symbol = inspect_live(
                &debugger,
                "(globalThis.__jsdbgEvaluationCount++, globalThis.__jsdbgPoisonedSymbol)",
                true,
            )
            .await
            .expect("poisoned Symbol evaluates");
            assert_eq!(poisoned_symbol.preview.kind, "symbol");
            assert!(poisoned_symbol.preview.preview.is_none());
            assert!(poisoned_symbol.preview.truncated);
            assert!(transport.largest_received_message_size() < 64 * 1024);
            assert!(serde_json::to_vec(&poisoned_symbol).unwrap().len() < 2_048);

            let count = inspect_live(&debugger, "globalThis.__jsdbgEvaluationCount", true)
                .await
                .expect("poisoned evaluation count reads");
            assert_eq!(count.preview.preview.as_deref(), Some("3"));

            print!(
                "{}",
                include_str!("../tests/transcripts/bounded-evaluation.txt")
            );
            root.target_close_target(TargetCloseTargetParams::new(created.target_id))
                .await
                .expect("close target");
        })
        .await
        .expect("live bounded evaluation scenario timed out");
    }

    async fn inspect_live(
        debugger: &TargetDebuggerHandle,
        expression: &str,
        allow_side_effects: bool,
    ) -> Result<ValueSnapshot, TargetDebuggerError> {
        debugger
            .inspect_value(
                None,
                ValueSelector::Expression {
                    expression: expression.to_owned(),
                    allow_side_effects,
                },
                ValueInspectionOptions {
                    max_preview_length: 120,
                    max_properties: 20,
                    retain_references: false,
                },
            )
            .await
    }

    fn assert_no_references(value: &ValueSnapshot) {
        assert!(value.preview.reference.is_none());
        assert!(
            value
                .properties
                .iter()
                .all(|property| property.value.reference.is_none())
        );
        assert!(value.promise.as_ref().is_none_or(|promise| {
            promise.reference.is_none()
                && promise
                    .settlement
                    .as_ref()
                    .is_none_or(|settlement| settlement.reference.is_none())
        }));
    }

    #[tokio::test]
    async fn pause_events_survive_a_later_running_snapshot() {
        let (snapshots, _) =
            tokio::sync::watch::channel(target_snapshot(TargetDebuggerPhase::Running));
        let (pause_events, _) = tokio::sync::broadcast::channel(4);
        let mut pauses = pause_events.subscribe();

        publish_snapshot(
            &snapshots,
            &pause_events,
            target_snapshot(TargetDebuggerPhase::Paused { epoch: 2 }),
        );
        publish_snapshot(
            &snapshots,
            &pause_events,
            target_snapshot(TargetDebuggerPhase::Running),
        );

        assert!(matches!(
            snapshots.borrow().phase,
            TargetDebuggerPhase::Running
        ));
        let pause = pauses.recv().await.unwrap();
        assert!(predicate_matches(
            &pause,
            &TargetWaitPredicate::Paused { after_epoch: 1 }
        ));
    }

    #[test]
    fn heap_text_preview_truncates_on_character_boundaries() {
        assert_eq!(
            bounded_heap_text("ab\u{00e9}def", Some(3)),
            ("ab\u{00e9}".to_owned(), true)
        );
        assert_eq!(
            bounded_heap_text("ab\u{00e9}", Some(3)),
            ("ab\u{00e9}".to_owned(), false)
        );
        assert_eq!(
            bounded_heap_text("secret", None),
            ("secret".to_owned(), false)
        );
    }

    fn profile_node(id: i64, name: &str, children: Vec<i64>) -> CpuProfileNodeSnapshot {
        CpuProfileNodeSnapshot {
            id,
            call_frame: CpuProfileCallFrameSnapshot {
                function_name: name.to_owned(),
                script_id: "1".to_owned(),
                url: "app.js".to_owned(),
                line_number: id,
                column_number: 0,
            },
            hit_count: None,
            children,
            deopt_reason: None,
            position_ticks: Vec::new(),
            authored_location: None,
            breadcrumb: None,
            self_time_micros: 0,
            total_time_micros: 0,
            sample_count: 0,
        }
    }

    #[test]
    fn cpu_profile_aggregation_attributes_self_and_total_time() {
        let mut profile = CpuProfileSnapshot {
            capture_id: "test".to_owned(),
            sampling_interval_micros: Some(1_000),
            start_time_micros: 0.0,
            end_time_micros: 150.0,
            nodes: vec![
                profile_node(1, "(root)", vec![2]),
                profile_node(2, "outer", vec![3]),
                profile_node(3, "inner", vec![]),
            ],
            samples: vec![3, 2],
            time_deltas_micros: vec![100, 50],
            functions: Vec::new(),
            analysis: None,
        };

        aggregate_cpu_profile(&mut profile).unwrap();

        assert_eq!(profile.nodes[0].self_time_micros, 0);
        assert_eq!(profile.nodes[0].total_time_micros, 150);
        assert_eq!(profile.nodes[1].self_time_micros, 50);
        assert_eq!(profile.nodes[1].total_time_micros, 150);
        assert_eq!(profile.nodes[2].self_time_micros, 100);
        assert_eq!(profile.nodes[2].total_time_micros, 100);
        assert_eq!(
            profile
                .functions
                .iter()
                .find(|function| function.name == "inner")
                .unwrap()
                .self_time_micros,
            100
        );
    }

    #[test]
    fn heap_class_names_omit_the_constructor_breadcrumb_segment() {
        assert_eq!(
            heap_class_display_name("PieceTreeTextBuffer.constructor"),
            "PieceTreeTextBuffer"
        );
        assert_eq!(
            heap_class_display_name("Namespace.constructorFactory"),
            "Namespace.constructorFactory"
        );
    }

    #[test]
    fn anonymous_frames_are_identified_as_callbacks_of_the_authored_container() {
        assert_eq!(
            callback_aware_breadcrumb(Some("NativeEditContext.constructor".to_owned()), ""),
            Some("NativeEditContext.constructor → callback".to_owned())
        );
        assert_eq!(
            callback_aware_breadcrumb(Some("NativeEditContext._onType".to_owned()), "_onType"),
            Some("NativeEditContext._onType".to_owned())
        );
        assert_eq!(
            callback_aware_breadcrumb(Some("Emitter.fire".to_owned()), "(anonymous)"),
            Some("Emitter.fire → callback".to_owned())
        );
        let generated = "function load() { queueMicrotask(() => work()); }";
        let column = generated.find("work").unwrap() as u32 + 1;
        assert_eq!(
            callback_aware_breadcrumb(
                crate::language_intelligence::breadcrumb("bundle.js", generated, 1, column,),
                "",
            ),
            Some("load → callback".to_owned())
        );
    }

    #[test]
    fn effective_ranges_respect_nested_zero_and_positive_overrides() {
        let effective =
            effective_coverage_ranges(&[range(0, 100, 1), range(20, 80, 0), range(40, 60, 1)]);
        assert_eq!(
            effective
                .iter()
                .map(|range| (range.start_offset, range.end_offset))
                .collect::<Vec<_>>(),
            vec![(0, 20), (40, 60), (80, 100)]
        );
    }

    #[test]
    fn source_excerpt_expands_tabs_before_positioning_the_caret() {
        let source = "\tpublic type(value: string): void {}\n";
        let excerpt = source_excerpt(
            "example.ts",
            &SourceLocation {
                source_url: "example.ts".to_owned(),
                line: 1,
                column: 9,
            },
            source,
            None,
        );
        assert_eq!(
            excerpt.lines[0].text,
            "    public type(value: string): void {}"
        );
        assert_eq!(excerpt.highlight_start, 12);
        assert_eq!(excerpt.highlight_length, 4);
    }

    /// Builds a single, very long source line made of non-alphanumeric
    /// filler (so it never gets mistaken for part of `marker`) with
    /// `marker` spliced in at `marker_offset` characters. `total_length`
    /// controls the overall line length so tests can force windowing.
    fn long_line_with_marker(marker: &str, marker_offset: usize, total_length: usize) -> String {
        let mut line: String = std::iter::repeat_n('-', marker_offset).collect();
        line.push_str(marker);
        let remaining = total_length.saturating_sub(line.chars().count());
        // Use a filler character distinct from both `.` (so it can never
        // be mistaken for the `...` omission marker) and `_` (so it never
        // extends the alphanumeric identifier run past `marker`).
        line.extend(std::iter::repeat_n('#', remaining));
        line
    }

    /// Extracts the highlighted slice of `text` addressed by the 1-based
    /// `highlight_start`/`highlight_length` pair, mirroring how
    /// `print_source_excerpt` positions the `^^^` caret under the text.
    fn highlighted_slice(text: &str, highlight_start: u32, highlight_length: u32) -> String {
        text.chars()
            .skip(highlight_start.saturating_sub(1) as usize)
            .take(highlight_length as usize)
            .collect()
    }

    fn excerpt_for_single_line(line: &str, one_based_column: u32) -> SourceExcerpt {
        source_excerpt(
            "minified.js",
            &SourceLocation {
                source_url: "minified.js".to_owned(),
                line: 1,
                column: one_based_column,
            },
            line,
            None,
        )
    }

    #[test]
    fn source_excerpt_leaves_ordinary_lines_unchanged() {
        let source = "const a = 1;\nconst b = a + 1;\nconsole.log(b);\n";
        let excerpt = source_excerpt(
            "app.js",
            &SourceLocation {
                source_url: "app.js".to_owned(),
                line: 2,
                column: 7,
            },
            source,
            None,
        );
        let rendered = excerpt
            .lines
            .iter()
            .map(|line| line.text.as_str())
            .collect::<Vec<_>>();
        assert_eq!(
            rendered,
            vec!["const a = 1;", "const b = a + 1;", "console.log(b);"]
        );
        assert!(
            !rendered
                .iter()
                .any(|line| line.contains('…') || line.contains("..."))
        );
        assert_eq!(excerpt.highlight_start, 7);
        assert_eq!(excerpt.highlight_length, 1);
    }

    #[test]
    fn source_excerpt_bounds_long_line_with_column_near_start() {
        let marker_offset = 10;
        let line = long_line_with_marker("TARGET", marker_offset, 1_000);
        let excerpt = excerpt_for_single_line(&line, marker_offset as u32 + 1);
        let text = &excerpt.lines[0].text;

        // Near the start of the window: no leading omission, but the tail
        // is far too long to show in full and must be marked as omitted.
        assert!(
            !text.starts_with("..."),
            "unexpected leading omission: {text}"
        );
        assert!(text.ends_with("..."), "missing trailing omission: {text}");
        assert!(text.len() < line.len(), "line should be bounded: {text}");
        assert_eq!(
            highlighted_slice(text, excerpt.highlight_start, excerpt.highlight_length),
            "TARGET"
        );
    }

    #[test]
    fn source_excerpt_bounds_long_line_with_column_near_middle() {
        let marker_offset = 500;
        let line = long_line_with_marker("TARGET", marker_offset, 1_000);
        let excerpt = excerpt_for_single_line(&line, marker_offset as u32 + 1);
        let text = &excerpt.lines[0].text;

        // Centered in a huge line: both ends must be marked as omitted.
        assert!(text.starts_with("..."), "missing leading omission: {text}");
        assert!(text.ends_with("..."), "missing trailing omission: {text}");
        assert_eq!(
            highlighted_slice(text, excerpt.highlight_start, excerpt.highlight_length),
            "TARGET"
        );
    }

    #[test]
    fn source_excerpt_bounds_long_line_with_column_near_end() {
        let marker_offset = 990;
        let line = long_line_with_marker("TARGET", marker_offset, 1_000);
        let excerpt = excerpt_for_single_line(&line, marker_offset as u32 + 1);
        let text = &excerpt.lines[0].text;

        // Near the true end of the line: leading omission, but nothing
        // trails past the marker, so no trailing marker should appear.
        assert!(text.starts_with("..."), "missing leading omission: {text}");
        assert!(
            !text.ends_with("..."),
            "unexpected trailing omission: {text}"
        );
        assert_eq!(
            highlighted_slice(text, excerpt.highlight_start, excerpt.highlight_length),
            "TARGET"
        );
    }

    #[test]
    fn source_excerpt_bounds_long_unicode_line_around_the_caret() {
        // 5 astral emoji (2 UTF-16 units, 1 char each) plus ASCII filler
        // ahead of the marker, exercising UTF-16 column mapping together
        // with the character-based windowing.
        let emoji_prefix = "😀".repeat(5);
        let ascii_prefix = "-".repeat(300);
        let marker = "MÄRK";
        let suffix = "#".repeat(300);
        let line = format!("{emoji_prefix}{ascii_prefix}{marker}{suffix}");

        let utf16_before_marker = (emoji_prefix.chars().count() * 2) + ascii_prefix.chars().count();
        let excerpt = excerpt_for_single_line(&line, utf16_before_marker as u32 + 1);
        let text = &excerpt.lines[0].text;

        assert!(text.starts_with("..."), "missing leading omission: {text}");
        assert!(text.ends_with("..."), "missing trailing omission: {text}");
        assert_eq!(
            highlighted_slice(text, excerpt.highlight_start, excerpt.highlight_length),
            marker
        );
    }

    #[test]
    fn source_excerpt_bounds_nearby_long_lines_without_misleading_columns() {
        let long_neighbor = "z".repeat(500);
        let source = format!("{long_neighbor}\nlet value = 1;\n{long_neighbor}\n");
        let excerpt = source_excerpt(
            "app.js",
            &SourceLocation {
                source_url: "app.js".to_owned(),
                line: 2,
                column: 5,
            },
            &source,
            None,
        );
        assert_eq!(excerpt.lines.len(), 3);
        // Non-active long lines are bounded too, but since they carry no
        // caret they only need a trailing omission marker, never a
        // misleading leading offset.
        assert_eq!(excerpt.lines[0].text.chars().count(), 201);
        assert!(excerpt.lines[0].text.ends_with('…'));
        assert!(!excerpt.lines[0].text.starts_with('…'));
        assert_eq!(excerpt.lines[2].text, excerpt.lines[0].text);
        assert_eq!(excerpt.lines[1].text, "let value = 1;");
    }

    #[test]
    fn window_highlighted_line_marks_only_the_omissions_actually_present() {
        // Short line: fits entirely, no markers at all.
        let (text, start, _) = window_highlighted_line("let value = 1;", 4, 200);
        assert_eq!(text, "let value = 1;");
        assert_eq!(start, 5);

        // Long line, caret at the very start: only a trailing marker.
        let long_tail = format!("{}{}", "a", "b".repeat(400));
        let (text, _, _) = window_highlighted_line(&long_tail, 0, 200);
        assert!(!text.starts_with("..."));
        assert!(text.ends_with("..."));

        // Long line, caret at the very end: only a leading marker.
        let long_head = format!("{}{}", "b".repeat(400), "a");
        let (text, _, _) = window_highlighted_line(&long_head, 400, 200);
        assert!(text.starts_with("..."));
        assert!(!text.ends_with("..."));

        // Long line, caret in the middle: markers on both sides.
        let long_middle = format!("{}{}{}", "b".repeat(400), "a", "b".repeat(400));
        let (text, _, _) = window_highlighted_line(&long_middle, 400, 200);
        assert!(text.starts_with("..."));
        assert!(text.ends_with("..."));
    }
}
