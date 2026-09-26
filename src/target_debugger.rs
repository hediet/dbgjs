use std::collections::{BTreeMap, BTreeSet};
use std::fs::File;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

use rayon::prelude::*;
use sha2::{Digest, Sha256};
use tokio::sync::{broadcast, mpsc, oneshot, watch};

use crate::cdp::{
    DebuggerEvaluateOnCallFrameParams, InputDispatchMouseEventParams,
    InputDispatchMouseEventParamsType, InputMouseButton, PageCaptureScreenshotParams,
    PageCaptureScreenshotParamsFormat, ProfilerProfile, ProfilerScriptCoverage,
    RuntimeCallFunctionOnParams, RuntimeExceptionDetails, RuntimeInternalPropertyDescriptor,
    RuntimePropertyDescriptor, RuntimeRemoteObject, RuntimeRemoteObjectSubtype,
    RuntimeRemoteObjectType,
};
use crate::cdp_runtime::CdpDebuggerSession;
use crate::context_source_model::ContextSourceModel;
use crate::context_source_model::SourceContributionId;
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
    BreakpointSourceCandidateSnapshot, CoverageAnalysisSnapshot, CoverageFunctionSnapshot,
    CoverageRangeSnapshot, CoverageSnapshot, CoverageSourceSnapshot, CaptureScriptProvenance, CpuProfileAnalysisSnapshot,
    CpuProfileCallFrameSnapshot, CpuProfileFunctionSnapshot, CpuProfileNodeSnapshot,
    CpuProfilePositionTickSnapshot, CpuProfileSnapshot, EvaluationSnapshot,
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
use crate::service_api::{
    HeapMappingSnapshot, HeapMappingStatus, HeapScriptMappingDiagnostic, HeapScriptSnapshot,
    HeapSourceMapSupply, ScriptProvenance,
};
use crate::source_effects::{SourceEffectInterpreter, SourceEffectOptions};
use crate::source_search::{HydratedSourceBatch, SearchControl, SearchError};
use crate::source_view::Position;
use crate::source_view::{GeneratedSourceInput, ResolutionPolicy, ResolvedSourceView};

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
    #[cfg(test)]
    pub(crate) fn ownership_stub_for_tests(
        snapshot: TargetDebuggerSnapshot,
        owned: Arc<std::sync::atomic::AtomicBool>,
        context_owned: Arc<std::sync::atomic::AtomicBool>,
    ) -> Self {
        let (commands, mut receiver) = mpsc::channel(8);
        let mut stub = Self::stub_for_tests(snapshot.clone());
        stub.commands = commands;
        tokio::spawn(async move {
            while let Some(command) = receiver.recv().await {
                match command {
                    TargetCommand::OwnsLogpoint { response, .. } => {
                        let _ = response.send(owned.load(Ordering::SeqCst));
                    }
                    TargetCommand::SetBreakpoints { lifetime, breakpoints, response } => {
                        let result = if matches!(lifetime, BreakpointLifetime::TargetGeneration)
                            && context_owned.load(Ordering::SeqCst)
                        {
                            Err(TargetDebuggerError::BreakpointOwnedByContext(
                                breakpoints[0].id.clone(),
                            ))
                        } else if matches!(lifetime, BreakpointLifetime::TargetGeneration) {
                            owned.store(true, Ordering::SeqCst);
                            Ok(snapshot.clone())
                        } else if owned.load(Ordering::SeqCst) {
                            Err(TargetDebuggerError::BreakpointOwnedByTarget(
                                breakpoints[0].id.clone(),
                            ))
                        } else {
                            context_owned.store(true, Ordering::SeqCst);
                            Ok(snapshot.clone())
                        };
                        let _ = response.send(result);
                    }
                    _ => {}
                }
            }
        });
        stub
    }

    #[cfg(test)]
    pub(crate) fn stub_for_tests(snapshot: TargetDebuggerSnapshot) -> Self {
        let (commands, _) = mpsc::channel(1);
        let (_, snapshots) = watch::channel(snapshot);
        let (pause_events, _) = broadcast::channel(1);
        let (_, heap_snapshot_progress) = watch::channel(None);
        let (raw_events, _) = broadcast::channel(1);
        Self {
            commands,
            snapshots,
            pause_events,
            session_id: "native-session".to_owned(),
            heap_snapshot_progress,
            raw_events,
            raw_event_history: Arc::new(std::sync::Mutex::new(Vec::new())),
        }
    }

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
            lifetime: BreakpointLifetime::ContextIntent(context_revision),
            breakpoints,
            response,
        })
        .await
    }

    pub async fn set_logpoints(
        &self,
        breakpoints: Vec<TargetBreakpointSpec>,
    ) -> Result<TargetDebuggerSnapshot, TargetDebuggerError> {
        self.command(|response| TargetCommand::SetBreakpoints {
            lifetime: BreakpointLifetime::TargetGeneration,
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

    pub async fn remove_logpoint(
        &self,
        breakpoint_id: String,
    ) -> Result<crate::service_api::LogpointRemovalResult, TargetDebuggerError> {
        let (response, receiver) = oneshot::channel();
        self.commands
            .send(TargetCommand::RemoveLogpoint {
                breakpoint_id,
                response,
            })
            .await
            .map_err(|_| TargetDebuggerError::Stopped)?;
        receiver.await.map_err(|_| TargetDebuggerError::Stopped)?
    }

    pub(crate) async fn owns_logpoint(
        &self,
        breakpoint_id: String,
    ) -> Result<bool, TargetDebuggerError> {
        let (response, receiver) = oneshot::channel();
        self.commands
            .send(TargetCommand::OwnsLogpoint { breakpoint_id, response })
            .await
            .map_err(|_| TargetDebuggerError::Stopped)?;
        receiver.await.map_err(|_| TargetDebuggerError::Stopped)
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
    ) -> Result<serde_json::Value, linkrpc::prelude::JsonRpcError> {
        let (response, receiver) = oneshot::channel();
        self.commands
            .send(TargetCommand::RawCdpRequest {
                method,
                params,
                response,
            })
            .await
            .map_err(|_| {
                linkrpc::prelude::JsonRpcError::new(
                    linkrpc::prelude::error_codes::PEER_DISCONNECTED,
                    "target debugger stopped before CDP request was sent",
                )
            })?;
        receiver.await.map_err(|_| {
            linkrpc::prelude::JsonRpcError::new(
                linkrpc::prelude::error_codes::PEER_DISCONNECTED,
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

    pub async fn hydrate_sources(&self, include_unmapped: bool) -> Result<(), TargetDebuggerError> {
        let (response, receiver) = oneshot::channel();
        self.commands
            .send(TargetCommand::HydrateSources {
                include_unmapped,
                response,
            })
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
        progress: mpsc::Sender<HeapSnapshotProgress>,
    ) -> Result<HeapSnapshotResult, TargetDebuggerError> {
        let (response, receiver) = oneshot::channel();
        self.commands
            .send(TargetCommand::TakeHeapSnapshot {
                path,
                capture_numeric_value,
                expose_internals,
                progress,
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
        progress: mpsc::Sender<HeapSnapshotProgress>,
    ) -> Result<HeapCaptureResult, TargetDebuggerError> {
        let (response, receiver) = oneshot::channel();
        self.commands
            .send(TargetCommand::CaptureHeapSnapshot {
                capture_id,
                capture_numeric_value,
                expose_internals,
                progress,
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
            .map(heap_snapshot_progress)
    }

    pub async fn start_coverage(&self) -> Result<(), TargetDebuggerError> {
        let (response, receiver) = oneshot::channel();
        self.commands
            .send(TargetCommand::StartCoverage { response })
            .await
            .map_err(|_| TargetDebuggerError::Stopped)?;
        receiver.await.map_err(|_| TargetDebuggerError::Stopped)?
    }

    pub async fn take_coverage(&self, capture_id: Option<String>) -> Result<CoverageSnapshot, TargetDebuggerError> {
        let (response, receiver) = oneshot::channel();
        self.commands
            .send(TargetCommand::TakeCoverage {
                capture_id,
                response,
            })
            .await
            .map_err(|_| TargetDebuggerError::Stopped)?;
        receiver.await.map_err(|_| TargetDebuggerError::Stopped)?
    }

    pub async fn stop_coverage(&self) -> Result<CoverageSnapshot, TargetDebuggerError> {
        self.stop_coverage_with_projection().await
    }

    pub async fn finish_coverage(&self) -> Result<(), TargetDebuggerError> {
        let (response, receiver) = oneshot::channel();
        self.commands
            .send(TargetCommand::FinishCoverage { response })
            .await
            .map_err(|_| TargetDebuggerError::Stopped)?;
        receiver.await.map_err(|_| TargetDebuggerError::Stopped)??;
        Ok(())
    }

    async fn stop_coverage_with_projection(&self) -> Result<CoverageSnapshot, TargetDebuggerError> {
        let (response, receiver) = oneshot::channel();
        self.commands
            .send(TargetCommand::StopCoverage { response })
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

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum BreakpointLifetime {
    ContextIntent(u64),
    TargetGeneration,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
struct BreakpointOwnership {
    // Retain the context watermark even while a target-local binding occupies this ID.
    context_revision: Option<u64>,
    owner: Option<BreakpointOwner>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum BreakpointOwner {
    Context,
    TargetGeneration,
}

impl BreakpointOwnership {
    fn accepts(self, lifetime: BreakpointLifetime, id: &str) -> Result<bool, TargetDebuggerError> {
        match lifetime {
            BreakpointLifetime::ContextIntent(revision) => {
                if self.context_revision.is_some_and(|current| current > revision) {
                    return Ok(false);
                }
                if self.owner == Some(BreakpointOwner::TargetGeneration) {
                    return Err(TargetDebuggerError::BreakpointOwnedByTarget(id.to_owned()));
                }
            }
            BreakpointLifetime::TargetGeneration => {
                if self.owner == Some(BreakpointOwner::Context) {
                    return Err(TargetDebuggerError::BreakpointOwnedByContext(id.to_owned()));
                }
            }
        }
        Ok(true)
    }

    fn installed(mut self, lifetime: BreakpointLifetime) -> Self {
        match lifetime {
            BreakpointLifetime::ContextIntent(revision) => {
                self.context_revision = Some(revision);
                self.owner = Some(BreakpointOwner::Context);
            }
            BreakpointLifetime::TargetGeneration => {
                self.owner = Some(BreakpointOwner::TargetGeneration)
            }
        }
        self
    }

    fn context_removed(mut self, revision: u64) -> Self {
        self.context_revision = Some(revision);
        if self.owner == Some(BreakpointOwner::Context) {
            self.owner = None;
        }
        self
    }
}

enum TargetCommand {
    SetBreakpoints {
        lifetime: BreakpointLifetime,
        breakpoints: Vec<TargetBreakpointSpec>,
        response: CommandResponse,
    },
    RemoveBreakpoint {
        context_revision: u64,
        breakpoint_id: String,
        response: CommandResponse,
    },
    RemoveLogpoint {
        breakpoint_id: String,
        response: oneshot::Sender<Result<crate::service_api::LogpointRemovalResult, TargetDebuggerError>>,
    },
    OwnsLogpoint {
        breakpoint_id: String,
        response: oneshot::Sender<bool>,
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
        response: oneshot::Sender<Result<serde_json::Value, linkrpc::prelude::JsonRpcError>>,
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
    HydrateSources {
        include_unmapped: bool,
        response: oneshot::Sender<Result<(), TargetDebuggerError>>,
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
        progress: mpsc::Sender<HeapSnapshotProgress>,
        response: oneshot::Sender<Result<HeapSnapshotResult, TargetDebuggerError>>,
    },
    CaptureHeapSnapshot {
        capture_id: Option<String>,
        capture_numeric_value: bool,
        expose_internals: bool,
        progress: mpsc::Sender<HeapSnapshotProgress>,
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
        response: oneshot::Sender<Result<CoverageSnapshot, TargetDebuggerError>>,
    },
    StopCoverage {
        response: oneshot::Sender<Result<CoverageSnapshot, TargetDebuggerError>>,
    },
    FinishCoverage {
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
    let mut breakpoint_owners = BTreeMap::<String, BreakpointOwnership>::new();
    let mut coverage = None::<CoverageRecording>;
    let mut coverage_objects = BTreeMap::<String, CoverageSnapshot>::new();
    let mut completed_recordings = BTreeMap::<String, CoverageRecording>::new();
    let mut pending_stopped_coverage = None::<CoverageSnapshot>;
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
                lifetime,
                breakpoints,
                response,
            })) => {
                let result = async {
                    let applicable = breakpoints
                        .into_iter()
                        .filter_map(|breakpoint| {
                            let owned = breakpoint_owners
                                .get(&breakpoint.id)
                                .copied()
                                .unwrap_or_default();
                            match owned.accepts(lifetime, &breakpoint.id) {
                                Ok(true) => Some(Ok(breakpoint)),
                                Ok(false) => None,
                                Err(error) => Some(Err(error)),
                            }
                        })
                        .collect::<Result<Vec<_>, _>>()?;
                    if matches!(lifetime, BreakpointLifetime::TargetGeneration)
                        && !applicable.is_empty()
                    {
                        driver.ensure_logpoint_binding().await.map_err(|error| {
                            TargetDebuggerError::LogpointTransport(error.message)
                        })?;
                    }
                    let mut applied = Vec::new();
                    for breakpoint in &applicable {
                        let previous = breakpoint_spec(
                            driver.state(),
                            &BreakpointKey {
                                client_id: context_id.clone(),
                                breakpoint_id: breakpoint.id.clone(),
                            },
                        );
                        applied.push((breakpoint.id.clone(), previous));
                        if let Err(install) =
                            apply_breakpoint(&mut driver, &context_id, breakpoint.clone()).await
                        {
                            let mut rollback_failures = Vec::new();
                            for (applied_id, prior) in applied.into_iter().rev() {
                                let rollback = match prior {
                                    Some(prior) => {
                                        apply_breakpoint(&mut driver, &context_id, prior).await
                                    }
                                    None => {
                                        remove_breakpoint(&mut driver, &context_id, &applied_id)
                                            .await
                                    }
                                };
                                if let Err(rollback) = rollback {
                                    rollback_failures.push(format!("{applied_id}: {rollback}"));
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
                    for breakpoint in &applicable {
                        let owner = breakpoint_owners.entry(breakpoint.id.clone()).or_default();
                        *owner = owner.installed(lifetime);
                    }
                    if matches!(lifetime, BreakpointLifetime::TargetGeneration) {
                        let ids = applicable
                            .iter()
                            .map(|breakpoint| &breakpoint.id)
                            .filter_map(|id| id.strip_prefix("log:").map(str::to_owned))
                            .collect::<Vec<_>>();
                        driver.register_logpoints(&ids);
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
                            linkrpc::prelude::JsonRpcError::new(
                                linkrpc::prelude::error_codes::REQUEST_TIMEOUT,
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
                    let owner = breakpoint_owners.get(&breakpoint_id).copied().unwrap_or_default();
                    if owner
                        .context_revision
                        .is_none_or(|current| current <= context_revision)
                    {
                        if owner.owner != Some(BreakpointOwner::TargetGeneration) {
                            remove_breakpoint(&mut driver, &context_id, &breakpoint_id).await?;
                        }
                        breakpoint_owners
                            .insert(breakpoint_id, owner.context_removed(context_revision));
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
            Next::Command(Some(TargetCommand::RemoveLogpoint {
                breakpoint_id,
                response,
            })) => {
                let result = async {
                    let previous = snapshot_from_driver(
                        &context_id,
                        &connection_id,
                        &target_id,
                        connection_generation,
                        &session_key,
                        &driver,
                    );
                    let owned = breakpoint_owners.get(&breakpoint_id).is_some_and(|state| {
                        state.owner == Some(BreakpointOwner::TargetGeneration)
                    });
                    let existing = previous
                        .breakpoints
                        .iter()
                        .find(|breakpoint| breakpoint.id == breakpoint_id);
                    let removed_bindings = existing.filter(|_| owned).map_or(0, |breakpoint| match breakpoint.status {
                        TargetBreakpointStatus::Installed { binding_count } => binding_count,
                        _ => 0,
                    });
                    if owned {
                        remove_breakpoint(&mut driver, &context_id, &breakpoint_id).await?;
                        let owner = breakpoint_owners
                            .get_mut(&breakpoint_id)
                            .expect("owned breakpoint");
                        owner.owner = None;
                        if let Some(id) = breakpoint_id.strip_prefix("log:") {
                            driver.unregister_logpoint(id);
                        }
                    }
                    Ok(crate::service_api::LogpointRemovalResult {
                        existed: owned,
                        removed_bindings,
                        target: snapshot_from_driver(
                            &context_id,
                            &connection_id,
                            &target_id,
                            connection_generation,
                            &session_key,
                            &driver,
                        ),
                    })
                }
                .await;
                if let Ok(result) = &result {
                    publish_snapshot(&snapshots, &pause_events, result.target.clone());
                }
                let _ = response.send(result);
            }
            Next::Command(Some(TargetCommand::OwnsLogpoint { breakpoint_id, response })) => {
                let _ = response.send(breakpoint_owners.get(&breakpoint_id).is_some_and(|state| {
                    state.owner == Some(BreakpointOwner::TargetGeneration)
                }));
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
                let result = async {
                    hydrate_source_for_path(&mut driver, &path).await?;
                    Ok(driver.state().scripts.iter().find_map(|(key, script)| {
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
                        driver.logical_source_content(key, &path).map(|content| {
                            SourceContentSnapshot {
                                path: path.clone(),
                                content: content.to_string(),
                                start_line: 1,
                                end_line: content.lines().count() as u32,
                                total_lines: content.lines().count() as u32,
                            }
                        })
                    }))
                }
                .await;
                if result.is_ok() {
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
                let _ = response.send(result);
            }
            Next::Command(Some(TargetCommand::ResolvedSourcePaths { response })) => {
                let paths = driver.source_effects().resolved_source_paths();
                let _ = response.send(Ok(paths));
            }
            Next::Command(Some(TargetCommand::HydrateSources {
                include_unmapped,
                response,
            })) => {
                let result = hydrate_sources(&mut driver, include_unmapped).await;
                if result.is_ok() {
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
                let _ = response.send(result);
            }
            Next::Command(Some(TargetCommand::SourceSearchBatch {
                path_selector,
                control,
                response,
            })) => {
                if !response.is_closed() && control.check().is_ok() {
                    let result = acquire_sources(
                        &mut driver,
                        SourceAcquisition::Search(path_selector.as_deref()),
                        Some(&control),
                    )
                    .await;
                    if let Err(error) = result {
                        let _ = response.send(Err(error));
                        continue;
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
                }
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
                let result = async {
                    hydrate_source_for_path(&mut driver, &path).await?;
                    let position = Position {
                        line: line.saturating_sub(1),
                        column: column.saturating_sub(1),
                    };
                    Ok(driver
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
                        .collect())
                }
                .await;
                if result.is_ok() {
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
                let _ = response.send(result);
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
                let result = evaluate(
                    &mut driver,
                    &session_key,
                    pause_epoch,
                    frame_index,
                    expression,
                )
                .await;
                let _ = response.send(result);
            }
            Next::Command(Some(TargetCommand::ScopeVariables {
                pause_epoch,
                frame_index,
                scope_index,
                response,
            })) => {
                let result = scope_variables(
                    &mut driver,
                    &session_key,
                    pause_epoch,
                    frame_index,
                    scope_index,
                )
                .await;
                let _ = response.send(result);
            }
            Next::Command(Some(TargetCommand::ObjectProperties {
                pause_epoch,
                object_id,
                response,
            })) => {
                let result =
                    object_properties(&mut driver, &session_key, pause_epoch, object_id).await;
                let _ = response.send(result);
            }
            Next::Command(Some(TargetCommand::InspectValue {
                pause_epoch,
                selector,
                options,
                response,
            })) => {
                let object_group =
                    (!options.retain_references).then(|| "dbgjs-ephemeral-value".to_owned());
                let result = inspect_value(
                    &mut driver,
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
                        .runtime()
                        .release_object_group(object_group)
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
                    .page()
                    .capture_screenshot(params)
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
                response,
            })) => {
                let result = match coverage.as_mut() {
                    Some(recording) => {
                        capture_coverage(&mut driver, &session_key, recording, capture_id)
                            .await
                    }
                    None => Err(TargetDebuggerError::CoverageNotActive),
                };
                let _ = response.send(result);
            }
            Next::Command(Some(TargetCommand::StopCoverage { response })) => {
                let result = async {
                    if coverage.is_some() {
                        let completed = finish_coverage_recording(&driver, &mut coverage).await?;
                        let mut stored = completed.snapshot();
                        attach_coverage_provenance(&driver, &session_key, &mut stored);
                        completed_recordings.remove(".");
                        coverage_objects.insert(".".to_owned(), stored.clone());
                        pending_stopped_coverage = Some(stored);
                    }
                    pending_stopped_coverage.take().ok_or(TargetDebuggerError::CoverageNotActive)
                }
                .await;
                let _ = response.send(result);
            }
            Next::Command(Some(TargetCommand::FinishCoverage { response })) => {
                let result = async {
                    let completed = finish_coverage_recording(&driver, &mut coverage).await?;
                    completed_recordings.insert(".".to_owned(), completed);
                    coverage_objects.remove(".");
                    pending_stopped_coverage = None;
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
                    let stopped =
                        driver.client().profiler().stop().await.map_err(|error| {
                            TargetDebuggerError::CpuProfile(format!("{error:?}"))
                        })?;
                    cpu_profile = None;
                    let mut snapshot = cpu_profile_snapshot(
                        capture_id.clone(),
                        recording.sampling_interval_micros,
                        stopped.profile,
                    )?;
                    snapshot.script_provenance = capture_cpu_script_provenance(&snapshot.nodes, |script_id, url| {
                        let key = ScriptKey { session: session_key.clone(), script_id: script_id.to_owned() };
                        driver.state().scripts.get(&key)
                            .filter(|script| cheap_capture_url(&script.url) == url)
                            .map(|script| capture_script_provenance(script))
                    });
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
                progress,
                response,
            })) => {
                let result = async {
                    let written = take_heap_snapshot(
                        &driver,
                        PathBuf::from(&path),
                        capture_numeric_value,
                        expose_internals,
                        &progress,
                    )
                    .await?;
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
                progress,
                response,
            })) => {
                let capture_id = capture_id.unwrap_or_else(|| ".".to_owned());
                let path = temporary_heap_snapshot_path();
                let mapping = capture_heap_mapping(&driver, &session_key, connection_generation);
                let result = take_heap_snapshot(
                    &driver,
                    path.clone(),
                    capture_numeric_value,
                    expose_internals,
                    &progress,
                )
                .await
                .map(|written| HeapCaptureResult {
                    capture_id: capture_id.clone(),
                    bytes_written: written.bytes_written,
                    timing: heap_snapshot_timing(&written),
                    mapping: Some(mapping.clone()),
                });
                if let Ok(capture) = &result {
                    let timing = capture.timing.clone();
                    if let Some(previous) = heap_captures.insert(
                        capture_id.clone(),
                        StoredHeapCapture {
                            path,
                            timing,
                            mapping,
                            source_resolver: Default::default(),
                        },
                    ) {
                        let _ = tokio::fs::remove_file(previous.path).await;
                    }
                    heap_constructor_groups.remove(&capture_id);
                    heap_graphs.remove(&capture_id);
                    heap_aliases.retain(|(stored_capture, _), _| stored_capture != &capture_id);
                }
                if let Err(Ok(_)) = response.send(result) {
                    if let Some(capture) = heap_captures.remove(&capture_id)
                        && let Err(error) = tokio::fs::remove_file(&capture.path).await
                        && error.kind() != std::io::ErrorKind::NotFound
                    {
                        eprintln!(
                            "failed to remove orphaned heap capture '{}': {error}",
                            capture.path.display()
                        );
                    }
                }
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
                    if no_cache {
                        return Err(TargetDebuggerError::HeapAnalysis(
                            "--no-cache is not supported for captured heaps; capture a new snapshot to refresh mapping metadata".into(),
                        ));
                    }
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
                    let mut snapshot = project_heap_classes(
                        capture_id.clone(),
                        &groups,
                        filter.as_ref(),
                        heap_captures.get(&capture_id).map(|capture| &capture.mapping),
                    )?;
                    snapshot.analysis = HeapClassAnalysisSnapshot {
                        snapshot_timing: heap_captures
                            .get(&capture_id)
                            .map(|capture| capture.timing.clone()),
                        parse_duration_micros: parse_duration.as_micros() as u64,
                        projection_duration_micros: projection_started.elapsed().as_micros() as u64,
                        constructor_group_count: groups.len() as u64,
                        used_cached_groups,
                        ..snapshot.analysis
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
                    live_heap_graph_view(&graph, &capture_id, &heap_captures,
                        graph_parse_duration, used_cached_graph)
                        .promises(state, limit, max_preview_length)
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
                    let mut selection = live_heap_graph_view(&graph, &capture_id,
                        &heap_captures, graph_parse_duration, used_cached_graph)
                        .select(selector, max_string_length, include_dominators)?;
                    let mut inspector = crate::object_inspection::LiveSourceInspector::default();
                    for node in &mut selection.nodes {
                        enrich_heap_live_source(&mut driver, &session_key, node, &mut inspector)
                            .await;
                    }
                    Ok(selection)
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
                    let (capture_id, _) = parse_heap_reference(&reference)?;
                    let (graph, _, _) =
                        load_heap_graph(&capture_id, &heap_captures, &mut heap_graphs).await?;
                    let mut snapshot = live_heap_graph_view(&graph, &capture_id,
                        &heap_captures, Duration::ZERO, false)
                        .references(&reference, direction, edge_policy, limit, max_string_length)?;
                    enrich_heap_live_source(
                        &mut driver,
                        &session_key,
                        &mut snapshot.node,
                        &mut crate::object_inspection::LiveSourceInspector::default(),
                    )
                    .await;
                    Ok(snapshot)
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
                    let (from_capture, _) = parse_heap_reference(&from)?;
                    let (to_capture, _) = parse_heap_reference(&to)?;
                    if from_capture != to_capture {
                        return Err(TargetDebuggerError::IncompatibleHeapCaptures {
                            older: from_capture,
                            newer: to_capture,
                        });
                    }
                    let capture_id = from_capture;
                    let (graph, _, _) =
                        load_heap_graph(&capture_id, &heap_captures, &mut heap_graphs).await?;
                    live_heap_graph_view(&graph, &capture_id, &heap_captures,
                        Duration::ZERO, false).path(from, to, options, max_string_length)
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
                    let (capture_id, _) = parse_heap_reference(&reference)?;
                    let (graph, _, _) =
                        load_heap_graph(&capture_id, &heap_captures, &mut heap_graphs).await?;
                    live_heap_graph_view(&graph, &capture_id, &heap_captures,
                        Duration::ZERO, false).dominators(&reference, max_string_length)
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
                    live_heap_graph_view(&graph, &capture_id, &heap_captures,
                        Duration::ZERO, false).aggregate(by, limit, max_string_length)
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
                    live_heap_graph_view(&older, &older_capture_id, &heap_captures,
                        Duration::ZERO, false).diff(
                        &live_heap_graph_view(&newer, &newer_capture_id, &heap_captures,
                            Duration::ZERO, false),
                        by, limit, max_string_length)
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
                failed.log_capture.status = crate::service_api::LogCaptureStatus::Stopped;
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
    let directory = if let Some(state_file) = std::env::var_os("DBGJS_SERVICE_STATE") {
        PathBuf::from(state_file)
            .parent()
            .map(|parent| parent.join("heap-captures"))
            .unwrap_or_else(|| std::env::temp_dir().join("dbgjs-heap-captures"))
    } else if let Some(local_app_data) = std::env::var_os("LOCALAPPDATA") {
        PathBuf::from(local_app_data)
            .join("dbgjs")
            .join("heap-captures")
    } else if let Some(home) = std::env::var_os("HOME") {
        PathBuf::from(home)
            .join(".cache")
            .join("dbgjs")
            .join("heap-captures")
    } else {
        std::env::temp_dir().join(format!("dbgjs-heap-captures-{}", std::process::id()))
    };
    directory.join(format!(
        "dbgjs-heap-{}-{}.heapsnapshot",
        std::process::id(),
        TEMPORARY_ID.fetch_add(1, Ordering::Relaxed)
    ))
}

#[derive(Clone)]
struct StoredHeapCapture {
    path: PathBuf,
    timing: HeapSnapshotTiming,
    mapping: HeapMappingSnapshot,
    source_resolver: Arc<std::sync::Mutex<crate::heap_locations::HeapSourceResolver>>,
}

pub(crate) struct StoredHeapGraph {
    capture_id: String,
    capture: StoredHeapCapture,
    graph: HeapGraph,
    parse_duration: Duration,
}

impl StoredHeapGraph {
    pub(crate) fn open(
        path: &Path,
        capture_id: String,
        mapping: Option<HeapMappingSnapshot>,
    ) -> Result<Self, TargetDebuggerError> {
        let started = Instant::now();
        let graph = parse_heap_graph(
            File::open(path).map_err(|error| TargetDebuggerError::HeapSnapshot(error.to_string()))?,
        )
        .map_err(|error| TargetDebuggerError::HeapSnapshot(error.to_string()))?;
        Ok(Self {
            capture_id,
            capture: StoredHeapCapture {
                path: path.to_path_buf(),
                timing: HeapSnapshotTiming::default(),
                mapping: mapping.unwrap_or(HeapMappingSnapshot {
                    connection_generation: 0,
                    scripts: Vec::new(),
                    hydration_duration_micros: 0,
                }),
                source_resolver: Default::default(),
            },
            graph,
            parse_duration: started.elapsed(),
        })
    }

    fn view(&self) -> HeapGraphView<'_> {
        HeapGraphView {
            capture_id: &self.capture_id,
            capture: Some(&self.capture),
            graph: &self.graph,
            parse_duration: self.parse_duration,
            used_cached_graph: false,
            canonicalize_path_endpoints: true,
        }
    }

    pub(crate) fn select(&self, selector: HeapNodeSelector, max_string_length: Option<u32>,
        include_dominators: bool) -> Result<HeapNodeSelectionSnapshot, TargetDebuggerError> {
        self.view().select(selector, max_string_length, include_dominators)
    }

    pub(crate) fn promises(&self, state: Option<PromiseState>, limit: u32,
        max_preview_length: u32) -> Result<PromiseSelectionSnapshot, TargetDebuggerError> {
        self.view().promises(state, limit, max_preview_length)
    }

    pub(crate) fn references(&self, reference: &str, direction: HeapReferenceDirection,
        edge_policy: HeapEdgePolicy, limit: u32, max_string_length: Option<u32>,
    ) -> Result<HeapReferencesSnapshot, TargetDebuggerError> {
        self.view().references(reference, direction, edge_policy, limit, max_string_length)
    }

    pub(crate) fn path(&self, from: String, to: String, options: HeapPathOptions,
        max_string_length: Option<u32>,
    ) -> Result<Option<HeapPathSnapshot>, TargetDebuggerError> {
        self.view().path(from, to, options, max_string_length)
    }

    pub(crate) fn dominators(&self, reference: &str, max_string_length: Option<u32>,
    ) -> Result<HeapDominatorSnapshot, TargetDebuggerError> {
        self.view().dominators(reference, max_string_length)
    }

    pub(crate) fn aggregate(&self, by: HeapAggregateBy, limit: u32,
        max_string_length: Option<u32>,
    ) -> Result<HeapAggregateSnapshot, TargetDebuggerError> {
        self.view().aggregate(by, limit, max_string_length)
    }

    pub(crate) fn diff(&self, newer: &Self, by: HeapAggregateBy, limit: u32,
        max_string_length: Option<u32>,
    ) -> Result<HeapDiffSnapshot, TargetDebuggerError> {
        self.view().diff(&newer.view(), by, limit, max_string_length)
    }
}

struct HeapGraphView<'a> {
    capture_id: &'a str,
    capture: Option<&'a StoredHeapCapture>,
    graph: &'a HeapGraph,
    parse_duration: Duration,
    used_cached_graph: bool,
    canonicalize_path_endpoints: bool,
}

fn live_heap_graph_view<'a>(
    graph: &'a HeapGraph,
    capture_id: &'a str,
    captures: &'a BTreeMap<String, StoredHeapCapture>,
    parse_duration: Duration,
    used_cached_graph: bool,
) -> HeapGraphView<'a> {
    HeapGraphView {
        capture_id,
        capture: captures.get(capture_id),
        graph,
        parse_duration,
        used_cached_graph,
        canonicalize_path_endpoints: false,
    }
}

impl HeapGraphView<'_> {
    fn select(
        &self,
        selector: HeapNodeSelector,
        max_string_length: Option<u32>,
        include_dominators: bool,
    ) -> Result<HeapNodeSelectionSnapshot, TargetDebuggerError> {
        let heap_object_id = selector
            .heap_object_id.as_deref().map(parse_heap_object_id).transpose()?;
        if selector.name.is_some() && selector.name_regex.is_some() {
            return Err(TargetDebuggerError::InvalidHeapSelector(
                "--name and --name-regex are mutually exclusive".into(),
            ));
        }
        if selector.string_contains.is_some() && selector.string_regex.is_some() {
            return Err(TargetDebuggerError::InvalidHeapSelector(
                "stringContains and stringRegex are mutually exclusive".into(),
            ));
        }
        let name_regex = selector.name_regex.as_deref().map(regex::Regex::new)
            .transpose().map_err(|error| TargetDebuggerError::InvalidHeapSelector(error.to_string()))?;
        let string_regex = selector.string_regex.as_deref().map(regex::Regex::new)
            .transpose().map_err(|error| TargetDebuggerError::InvalidHeapSelector(error.to_string()))?;
        let mut graph_selector = NodeSelector::new();
        if let Some(id) = heap_object_id {
            graph_selector = graph_selector.heap_object_id(id);
        }
        if let Some(kind) = selector.node_type.as_deref() {
            graph_selector = graph_selector.node_type(kind);
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
        let selection = self.graph.select_with_stats(&graph_selector);
        let dominators = include_dominators.then(|| self.graph.dominators())
            .transpose().map_err(heap_analysis_error)?;
        let nodes = selection.nodes.into_iter().map(|node| {
            heap_node_snapshot(self.graph, self.capture_id, node, max_string_length,
                dominators, self.capture)
        }).collect::<Result<Vec<_>, _>>()?;
        Ok(HeapNodeSelectionSnapshot {
            capture_id: self.capture_id.to_owned(),
            total_nodes: self.graph.node_count() as u64,
            total_edges: self.graph.edge_count() as u64,
            nodes,
            incomplete_string_count: selection.incomplete_string_count,
            graph_parse_duration_micros: self.parse_duration.as_micros() as u64,
            used_cached_graph: self.used_cached_graph,
        })
    }

    fn node(&self, node: NodeIndex, max_string_length: Option<u32>,
        dominators: Option<&crate::heap_graph::DominatorAnalysis>,
    ) -> Result<HeapNodeSnapshot, TargetDebuggerError> {
        heap_node_snapshot(self.graph, self.capture_id, node, max_string_length,
            dominators, self.capture)
    }

    fn reference_node(&self, reference: &str) -> Result<(NodeIndex, String), TargetDebuggerError> {
        let (capture, id) = parse_heap_reference(reference)?;
        if capture != self.capture_id && capture != "." {
            return Err(TargetDebuggerError::IncompatibleHeapCaptures {
                older: self.capture_id.to_owned(),
                newer: capture,
            });
        }
        Ok((
            heap_node_by_id(self.graph, id)?,
            heap_node_reference(self.capture_id, id),
        ))
    }

    fn promises(&self, state: Option<PromiseState>, limit: u32,
        max_preview_length: u32,
    ) -> Result<PromiseSelectionSnapshot, TargetDebuggerError> {
        let (promises, total_promises) = inspect_heap_promises(
            self.graph, self.capture_id, state, limit, max_preview_length)
            .map_err(heap_analysis_error)?;
        Ok(PromiseSelectionSnapshot {
            capture_id: self.capture_id.to_owned(),
            omitted_promise_count: total_promises.saturating_sub(promises.len() as u64),
            total_promises,
            promises,
            graph_parse_duration_micros: self.parse_duration.as_micros() as u64,
            used_cached_graph: self.used_cached_graph,
        })
    }

    fn references(&self, reference: &str, direction: HeapReferenceDirection,
        edge_policy: HeapEdgePolicy, limit: u32, max_string_length: Option<u32>,
    ) -> Result<HeapReferencesSnapshot, TargetDebuggerError> {
        let (node, _) = self.reference_node(reference)?;
        let mut references = Vec::new();
        if matches!(direction, HeapReferenceDirection::Outgoing | HeapReferenceDirection::Both) {
            references.extend(self.graph.outgoing_references(node).map_err(heap_analysis_error)?
                .filter(|edge| edge_policy == HeapEdgePolicy::All || edge.edge_type != "weak"));
        }
        if matches!(direction, HeapReferenceDirection::Incoming | HeapReferenceDirection::Both) {
            references.extend(self.graph.incoming_references(node).map_err(heap_analysis_error)?
                .filter(|edge| edge_policy == HeapEdgePolicy::All || edge.edge_type != "weak"));
        }
        references.sort_by_key(|edge| edge.edge.0);
        let omitted_reference_count = references.len().saturating_sub(limit as usize) as u64;
        references.truncate(limit as usize);
        let references = references.into_iter().map(|edge| {
            heap_reference_snapshot(self.graph, self.capture_id, edge, self.capture)
        }).collect::<Result<Vec<_>, _>>()?;
        Ok(HeapReferencesSnapshot {
            capture_id: self.capture_id.to_owned(),
            node: self.node(node, max_string_length, None)?,
            direction,
            edge_policy,
            references,
            omitted_reference_count,
        })
    }

    fn path(&self, from: String, to: String, options: HeapPathOptions,
        max_string_length: Option<u32>,
    ) -> Result<Option<HeapPathSnapshot>, TargetDebuggerError> {
        let (from_node, canonical_from) = self.reference_node(&from)?;
        let (to_node, canonical_to) = self.reference_node(&to)?;
        let (from, to) = if self.canonicalize_path_endpoints {
            (canonical_from, canonical_to)
        } else {
            (from, to)
        };
        self.graph.shortest_path(from_node, to_node, PathOptions {
            direction: heap_path_direction(options.direction),
            edge_policy: heap_edge_policy(options.edge_policy),
            cost: heap_path_cost(options.cost),
        }).map_err(heap_analysis_error)?.map(|path| {
            let nodes = path.nodes.into_iter().map(|node| self.node(node, max_string_length, None))
                .collect::<Result<Vec<_>, _>>()?;
            let steps = path.steps.into_iter().map(|step| {
                heap_path_step_snapshot(self.graph, self.capture_id, step)
            }).collect::<Result<Vec<_>, _>>()?;
            Ok(HeapPathSnapshot {
                capture_id: self.capture_id.to_owned(), from, to, cost: path.cost, nodes, steps,
            })
        }).transpose()
    }

    fn dominators(&self, reference: &str, max_string_length: Option<u32>,
    ) -> Result<HeapDominatorSnapshot, TargetDebuggerError> {
        let (node, _) = self.reference_node(reference)?;
        let analysis = self.graph.dominators().map_err(heap_analysis_error)?;
        let mut chain = Vec::new();
        let mut current = node;
        while let Some(parent) = analysis.immediate_dominator(current) {
            chain.push(self.node(parent, max_string_length, Some(analysis))?);
            current = parent;
        }
        Ok(HeapDominatorSnapshot {
            capture_id: self.capture_id.to_owned(),
            node: self.node(node, max_string_length, Some(analysis))?,
            chain,
        })
    }

    fn aggregate(&self, by: HeapAggregateBy, limit: u32,
        max_string_length: Option<u32>,
    ) -> Result<HeapAggregateSnapshot, TargetDebuggerError> {
        let aggregate = self.graph.aggregate(heap_aggregate_by(by));
        let mut entries = aggregate.groups.into_iter()
            .filter(|(_, value)| value.count != 0 || value.shallow_size != 0)
            .map(|(key, value)| {
                let (key, key_truncated) = bounded_heap_text(&key, max_string_length);
                Ok(HeapAggregateEntrySnapshot {
                    key, key_truncated, count: value.count,
                    shallow_size: u64::try_from(value.shallow_size).map_err(|_| {
                        TargetDebuggerError::HeapAnalysis("aggregate shallow size exceeds u64".into())
                    })?,
                })
            }).collect::<Result<Vec<_>, TargetDebuggerError>>()?;
        entries.sort_by_key(|entry| std::cmp::Reverse((entry.shallow_size, entry.count)));
        let omitted_entry_count = entries.len().saturating_sub(limit as usize) as u64;
        entries.truncate(limit as usize);
        Ok(HeapAggregateSnapshot {
            capture_id: self.capture_id.to_owned(), by, entries, omitted_entry_count,
            incomplete_string_count: aggregate.incomplete_string_count,
        })
    }

    fn diff(&self, newer: &Self, by: HeapAggregateBy, limit: u32,
        max_string_length: Option<u32>,
    ) -> Result<HeapDiffSnapshot, TargetDebuggerError> {
        let diff = self.graph.diff(newer.graph, heap_aggregate_by(by));
        let mut entries = diff.groups.into_iter()
            .filter(|(_, value)| value.count != 0 || value.shallow_size != 0)
            .map(|(key, value)| {
                let (key, key_truncated) = bounded_heap_text(&key, max_string_length);
                Ok(HeapDiffEntrySnapshot {
                    key, key_truncated,
                    count_delta: i64::try_from(value.count).map_err(|_| {
                        TargetDebuggerError::HeapAnalysis("aggregate count delta exceeds i64".into())
                    })?,
                    shallow_size_delta: i64::try_from(value.shallow_size).map_err(|_| {
                        TargetDebuggerError::HeapAnalysis("aggregate shallow size delta exceeds i64".into())
                    })?,
                })
            }).collect::<Result<Vec<_>, TargetDebuggerError>>()?;
        entries.sort_by_key(|entry| std::cmp::Reverse(entry.shallow_size_delta.unsigned_abs()));
        entries.truncate(limit as usize);
        Ok(HeapDiffSnapshot {
            older_capture_id: self.capture_id.to_owned(),
            newer_capture_id: newer.capture_id.to_owned(), by, entries,
            older_incomplete_string_count: diff.older_incomplete_string_count,
            newer_incomplete_string_count: diff.newer_incomplete_string_count,
        })
    }
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
    capture: Option<&StoredHeapCapture>,
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
        preview: Some(crate::heap_preview::heap_preview(graph, node).map_err(heap_analysis_error)?),
        shallow_size: summary.shallow_size,
        outgoing_reference_count: summary.outgoing_references as u64,
        incoming_reference_count: summary.incoming_references as u64,
        locations,
        source: heap_object_source(graph, node, capture)?,
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

fn heap_object_source(
    graph: &HeapGraph,
    node: NodeIndex,
    capture: Option<&StoredHeapCapture>,
) -> Result<crate::object_inspection::ObjectSourceSnapshot, TargetDebuggerError> {
    capture
        .map(|capture| {
            capture
                .source_resolver
                .lock()
                .unwrap()
                .inspect(graph, &capture.mapping, node)
        })
        .transpose()
        .map_err(heap_analysis_error)
        .map(Option::unwrap_or_default)
}

async fn enrich_heap_live_source(
    driver: &mut DebuggerDriver,
    session: &SessionKey,
    node: &mut HeapNodeSnapshot,
    inspector: &mut crate::object_inspection::LiveSourceInspector,
) {
    if !matches!(node.node_type.as_str(), "closure" | "object") {
        return;
    }
    if !inspector.take_request() {
        node.source
            .diagnostics
            .push("live comparison skipped: location lookup budget exhausted".into());
        return;
    }
    let materialized = tokio::time::timeout(
        inspector.remaining_time(),
        driver.raw_cdp_request(
            "HeapProfiler.getObjectByHeapObjectId",
            serde_json::json!({
                "objectId": node.heap_object_id,
                "objectGroup": "dbgjs-heap-location",
            }),
        ),
    )
    .await;
    match materialized {
        Ok(Ok(value)) => {
            if let Some(id) = value
                .pointer("/result/objectId")
                .and_then(serde_json::Value::as_str)
            {
                node.source
                    .merge(inspector.inspect(driver, session, id, None).await);
            } else {
                node.source
                    .diagnostics
                    .push("heap object is not available for live location comparison".into());
            }
        }
        Ok(Err(error)) => node.source.diagnostics.push(format!(
            "live heap location comparison unavailable: {error:?}"
        )),
        Err(_) => node
            .source
            .diagnostics
            .push("live heap location comparison timed out".into()),
    }
    if let Err(error) = driver
        .client()
        .runtime()
        .release_object_group("dbgjs-heap-location".into())
        .await
    {
        node.source.diagnostics.push(format!(
            "failed to release live heap location object: {error:?}"
        ));
    }
}

fn heap_reference_snapshot(
    graph: &HeapGraph,
    capture_id: &str,
    reference: crate::heap_graph::HeapReference<'_>,
    capture: Option<&StoredHeapCapture>,
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
        source_preview: Some(
            crate::heap_preview::heap_preview(graph, reference.source)
                .map_err(heap_analysis_error)?,
        ),
        target_preview: Some(
            crate::heap_preview::heap_preview(graph, reference.target)
                .map_err(heap_analysis_error)?,
        ),
        source_locations: heap_object_source(graph, reference.source, capture)?,
        target_locations: heap_object_source(graph, reference.target, capture)?,
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
    progress: &mpsc::Sender<HeapSnapshotProgress>,
) -> Result<crate::cdp_runtime::HeapSnapshotWriteResult, TargetDebuggerError> {
    driver
        .begin_heap_snapshot(path)
        .await
        .map_err(|error| TargetDebuggerError::HeapSnapshot(error.to_string()))?;
    let mut progress_updates = driver.heap_snapshot_progress();
    let heap_profiler = driver.client().heap_profiler();
    let snapshot = heap_profiler.take_heap_snapshot(
        Some(true),
        None,
        capture_numeric_value.then_some(true),
        expose_internals.then_some(true),
    );
    tokio::pin!(snapshot);
    if let Err(error) =
        forward_heap_snapshot_progress(&mut progress_updates, progress, &mut snapshot).await
    {
        driver.abort_heap_snapshot().await;
        return Err(TargetDebuggerError::HeapSnapshot(format!("{error:?}")));
    }
    let written = driver
        .finish_heap_snapshot()
        .await
        .map_err(|error| TargetDebuggerError::HeapSnapshot(error.to_string()))?;
    send_finished_heap_snapshot_progress(&progress_updates, progress, written.bytes_written)
        .await?;
    Ok(written)
}

async fn forward_heap_snapshot_progress<F, T, E>(
    updates: &mut watch::Receiver<Option<crate::cdp_runtime::HeapSnapshotStreamProgress>>,
    output: &mpsc::Sender<HeapSnapshotProgress>,
    operation: &mut std::pin::Pin<&mut F>,
) -> Result<T, E>
where
    F: std::future::Future<Output = Result<T, E>>,
{
    let mut last = None;
    loop {
        let current = updates.borrow().clone().map(heap_snapshot_progress);
        if current != last {
            if let Some(current) = current.clone() {
                send_heap_progress(output, current).await;
            }
            last = current;
        }
        tokio::select! {
            result = operation.as_mut() => {
                let current = updates.borrow().clone().map(heap_snapshot_progress);
                if current != last
                    && let Some(current) = current
                {
                    send_heap_progress(output, current).await;
                }
                return result;
            }
            changed = updates.changed() => {
                if changed.is_err() {
                    return operation.as_mut().await;
                }
            }
        }
    }
}

fn heap_snapshot_progress(
    progress: crate::cdp_runtime::HeapSnapshotStreamProgress,
) -> HeapSnapshotProgress {
    HeapSnapshotProgress {
        done: progress.done,
        total: progress.total,
        finished: progress.finished,
        bytes_written: progress.bytes_written,
    }
}

async fn send_heap_progress(
    output: &mpsc::Sender<HeapSnapshotProgress>,
    progress: HeapSnapshotProgress,
) {
    // Losing the RPC observer must not interrupt CDP chunk ingestion or writer cleanup.
    if !output.is_closed() && output.send(progress).await.is_err() {
        eprintln!("heap progress receiver disconnected; completing the CDP snapshot");
    }
}

async fn send_finished_heap_snapshot_progress(
    updates: &watch::Receiver<Option<crate::cdp_runtime::HeapSnapshotStreamProgress>>,
    output: &mpsc::Sender<HeapSnapshotProgress>,
    bytes_written: u64,
) -> Result<(), TargetDebuggerError> {
    let current = updates.borrow().clone();
    let mut finished = current.map(heap_snapshot_progress).ok_or_else(|| {
        TargetDebuggerError::HeapSnapshot("completed snapshot has no progress state".to_owned())
    })?;
    finished.finished = Some(true);
    finished.bytes_written = bytes_written;
    send_heap_progress(output, finished).await;
    Ok(())
}

struct ProjectedHeapClass {
    script_id: String,
    provenance: ScriptProvenance,
    name: String,
    source_url: String,
    location: SourceLocation,
    generated_name: String,
    instance_count: u64,
    shallow_size: u64,
    instances: Vec<crate::heap_snapshot::HeapInstanceRecord>,
}

fn capture_heap_mapping(
    driver: &DebuggerDriver,
    session: &SessionKey,
    connection_generation: u64,
) -> HeapMappingSnapshot {
    let scripts = driver
        .state()
        .scripts
        .iter()
        .filter(|(key, _)| &key.session == session)
        .map(|(key, script)| captured_heap_script(key, script))
        .collect();
    HeapMappingSnapshot {
        connection_generation,
        scripts,
        hydration_duration_micros: 0,
    }
}

fn captured_heap_script(
    key: &ScriptKey,
    script: &crate::debugger_engine::ScriptState,
) -> HeapScriptSnapshot {
    const MAX_PROVENANCE_URL_BYTES: usize = 2048;
    let cheap_url = |url: &str| {
        url.len() <= MAX_PROVENANCE_URL_BYTES
            && !url.get(..5).is_some_and(|prefix| prefix.eq_ignore_ascii_case("data:"))
            && !url.get(..11).is_some_and(|prefix| prefix.eq_ignore_ascii_case("javascript:"))
    };
    let url = cheap_url(&script.url)
        .then(|| script.url.clone())
        .unwrap_or_else(|| format!("script:{}", key.script_id));
    let source_map_url = script.source_map_url.as_ref()
        .filter(|url| cheap_url(url)).cloned();
    let omitted_source = url != script.url;
    let omitted_map = script.source_map_url.is_some() && source_map_url.is_none();
    let mapping_status = if script.source_map_url.is_none() {
        HeapMappingStatus::NoMapSupplied
    } else {
        HeapMappingStatus::NotAttempted
    };
    HeapScriptSnapshot {
        script_id: key.script_id.clone(),
        url,
        hash: script.hash.clone(),
        provenance: script.provenance.clone(),
        source_map_url,
        generated_source: None,
        source_map: None,
        mapping_status,
        diagnostic: (omitted_source || omitted_map).then(|| {
            "inline or oversized source/map URL omitted from capture; generated location retained"
                .to_owned()
        }),
    }
}

fn heap_source_view(script: &HeapScriptSnapshot, map: &str) -> Result<ResolvedSourceView, String> {
    sourcemap::decode_slice(map.as_bytes())
        .map_err(|error| format!("invalid source map: {error}"))?;
    let mut view = ResolvedSourceView::new(
        ResolutionPolicy::PreferSourcesContent,
        Arc::new(ContextSourceModel::new()),
        SourceContributionId::new("stored-heap"),
        BTreeMap::new(),
    );
    view.add_generated(GeneratedSourceInput {
        url: &script.url,
        content: script.generated_source.as_deref().unwrap_or(""),
        source_map: Some(map.as_bytes()),
        source_map_url: script.source_map_url.as_deref(),
        minified: false,
    })
    .map_err(|error| error.to_string())?;
    if let Some(error) = view
        .diagnostics()
        .iter()
        .find_map(|diagnostic| match diagnostic {
            crate::source_view::SourceDiagnostic::SourceMapFailed { error, .. } => Some(error),
            _ => None,
        })
    {
        return Err(error.clone());
    }
    Ok(view)
}

pub(crate) fn supply_heap_source_map(
    mapping: &mut HeapMappingSnapshot,
    supply: HeapSourceMapSupply,
) -> Result<(), TargetDebuggerError> {
    let script = mapping
        .scripts
        .iter_mut()
        .find(|script| script.script_id == supply.script_id)
        .ok_or_else(|| {
            TargetDebuggerError::HeapAnalysis(format!(
                "captured script '{}' not found",
                supply.script_id
            ))
        })?;
    if script.hash.is_empty() || script.hash != supply.script_hash {
        return Err(TargetDebuggerError::HeapAnalysis(
            "source map script hash does not match the captured script".into(),
        ));
    }
    let json: serde_json::Value = serde_json::from_str(&supply.source_map).map_err(|error| {
        TargetDebuggerError::HeapAnalysis(format!("invalid source map: {error}"))
    })?;
    if let Some(file) = json.get("file").and_then(serde_json::Value::as_str) {
        let basename = |url: &str| {
            url.split(['?', '#'])
                .next()
                .unwrap_or(url)
                .rsplit(['/', '\\'])
                .next()
                .unwrap_or(url)
                .to_owned()
        };
        let source_url_was_omitted = script.url == format!("script:{}", script.script_id)
            && script.diagnostic.as_deref().is_some_and(|diagnostic| {
                diagnostic.starts_with("inline or oversized source/map URL omitted")
            });
        if !source_url_was_omitted && basename(file) != basename(&script.url) {
            return Err(TargetDebuggerError::HeapAnalysis(
                "source map file does not match the captured script URL".into(),
            ));
        }
    }
    let mut updated = script.clone();
    updated.source_map_url = Some(supply.source_map_url);
    heap_source_view(&updated, &supply.source_map).map_err(TargetDebuggerError::HeapAnalysis)?;
    updated.source_map = Some(supply.source_map);
    updated.mapping_status = HeapMappingStatus::Mapped;
    updated.diagnostic = None;
    *script = updated;
    Ok(())
}

pub fn stored_heap_classes(
    path: &Path,
    capture_id: String,
    filter: Option<&str>,
    mapping: Option<&HeapMappingSnapshot>,
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
    let projection_started = Instant::now();
    let mut snapshot = project_heap_classes(capture_id, &groups, filter.as_ref(), mapping)?;
    snapshot.analysis.parse_duration_micros = parse_duration.as_micros() as u64;
    snapshot.analysis.projection_duration_micros = projection_started.elapsed().as_micros() as u64;
    Ok(snapshot)
}

fn heap_script_needs_map(script: &HeapScriptSnapshot) -> bool {
    script.source_map.is_none()
        && (script.source_map_url.is_some()
            || script.diagnostic.as_deref().is_some_and(|diagnostic| {
                diagnostic.starts_with("inline or oversized source/map URL omitted")
            }))
}

fn heap_script_sha256(script: &HeapScriptSnapshot) -> Option<&str> {
    (script.hash.len() == 64 && script.hash.bytes().all(|byte| byte.is_ascii_hexdigit()))
        .then_some(script.hash.as_str())
}

pub(crate) async fn prepare_heap_view_sources(
    mapping: Option<&HeapMappingSnapshot>,
) -> (crate::capture_projection::PreparedViewSources, Vec<String>) {
    let scripts = mapping.into_iter().flat_map(|mapping| mapping.scripts.iter())
        .filter(|script| heap_script_needs_map(script))
        .map(|script| (
            script.script_id.clone(),
            CaptureScriptProvenance {
                url: script.url.clone(),
                source_map_url: script.source_map_url.clone(),
                source_sha256: heap_script_sha256(script).map(str::to_owned),
            },
        ))
        .collect::<Vec<_>>();
    crate::capture_projection::prepare_view_sources(
        scripts.iter().map(|(id, provenance)| (id.as_str(), provenance)),
        false,
    ).await
}

pub(crate) fn recover_heap_mapping_for_view(
    mapping: Option<HeapMappingSnapshot>,
    prepared: &crate::capture_projection::PreparedViewSources,
    diagnostics: &[String],
) -> Option<HeapMappingSnapshot> {
    let mut mapping = mapping?;
    for index in 0..mapping.scripts.len() {
        let script = &mapping.scripts[index];
        if !heap_script_needs_map(script) {
            continue;
        }
        let recovered = prepared.get(&(script.script_id.clone(), script.url.clone()))
            .map(|sources| Ok((sources.map_bytes.clone(), sources.map_url.clone())))
            .unwrap_or_else(|| crate::capture_projection::recover_source_map_for_view(
                &script.url,
                script.source_map_url.as_deref(),
                heap_script_sha256(script),
                &script.hash,
            ));
        let (bytes, map_url) = match recovered {
            Ok(recovered) => recovered,
            Err(error) => {
                let view_error = diagnostics.iter().find(|diagnostic| {
                    diagnostic.starts_with(&format!("{}:", script.url))
                });
                mapping.scripts[index].mapping_status = HeapMappingStatus::MapLoadingFailed;
                mapping.scripts[index].diagnostic = Some(match view_error {
                    Some(view_error) => format!("{error}; {view_error}"),
                    None => error,
                });
                continue;
            }
        };
        let source_map = match String::from_utf8(bytes) {
            Ok(source_map) => source_map,
            Err(error) => {
                mapping.scripts[index].mapping_status = HeapMappingStatus::MapLoadingFailed;
                mapping.scripts[index].diagnostic =
                    Some(format!("source map is not UTF-8: {error}"));
                continue;
            }
        };
        let supply = HeapSourceMapSupply {
            script_id: mapping.scripts[index].script_id.clone(),
            script_hash: mapping.scripts[index].hash.clone(),
            source_map_url: map_url,
            source_map,
        };
        if let Err(error) = supply_heap_source_map(&mut mapping, supply) {
            mapping.scripts[index].mapping_status = HeapMappingStatus::MapLoadingFailed;
            mapping.scripts[index].diagnostic = Some(error.to_string());
        }
    }
    Some(mapping)
}

fn project_heap_classes(
    capture_id: String,
    groups: &[HeapConstructorGroup],
    filter: Option<&regex::Regex>,
    mapping: Option<&HeapMappingSnapshot>,
) -> Result<HeapClassSnapshot, TargetDebuggerError> {
    let scripts = mapping
        .map(|mapping| {
            mapping
                .scripts
                .iter()
                .map(|script| (script.script_id.clone(), script))
                .collect::<BTreeMap<_, _>>()
        })
        .unwrap_or_default();
    let mut views = BTreeMap::new();
    let mut diagnostics = Vec::new();
    for script_id in groups
        .iter()
        .map(|group| group.script_id.to_string())
        .collect::<BTreeSet<_>>()
    {
        let Some(script) = scripts.get(&script_id) else {
            diagnostics.push(HeapScriptMappingDiagnostic {
                script_id: script_id.clone(),
                url: format!("script:{script_id}"),
                hash: String::new(),
                provenance: Default::default(),
                status: HeapMappingStatus::NotAttempted,
                diagnostic: Some(
                    "capture has no script mapping metadata (legacy capture or unobserved script)"
                        .into(),
                ),
            });
            continue;
        };
        let mut diagnostic = HeapScriptMappingDiagnostic {
            script_id: script_id.clone(),
            url: script.url.clone(),
            hash: script.hash.clone(),
            provenance: script.provenance.clone(),
            status: script.mapping_status.clone(),
            diagnostic: script.diagnostic.clone().or_else(|| {
                (script.source_map.is_none() && script.source_map_url.is_some()).then(|| {
                    format!(
                        "source map '{}' unavailable for this view; generated location retained",
                        script.source_map_url.as_deref().unwrap_or_default()
                    )
                })
            }),
        };
        if let Some(map) = &script.source_map {
            match heap_source_view(script, map) {
                Ok(view) => {
                    diagnostic.status = HeapMappingStatus::Mapped;
                    diagnostic.diagnostic = None;
                    views.insert(script_id.clone(), view);
                }
                Err(error) => {
                    diagnostic.status = HeapMappingStatus::MapLoadingFailed;
                    diagnostic.diagnostic = Some(error);
                }
            }
        }
        diagnostics.push(diagnostic);
    }
    let mut projected = BTreeMap::<(String, String, u32, u32, String), ProjectedHeapClass>::new();
    let mut symbol_indexes = BTreeMap::<String, crate::source_location::SymbolIndexCache>::new();
    for group in groups {
        let script_id = group.script_id.to_string();
        let script = scripts.get(&script_id);
        let generated_url = script
            .map(|script| script.url.clone())
            .filter(|url| !url.is_empty())
            .unwrap_or_else(|| format!("script:{script_id}"));
        let mapped = views.get(&script_id).map(|view| {
            let resolved = crate::source_location::resolve_source_position(
                view,
                &generated_url,
                script.and_then(|script| script.source_map_url.as_deref()),
                Position {
                    line: group.line,
                    column: group.column,
                },
                symbol_indexes.entry(script_id.clone()).or_default(),
            );
            (
                resolved.resolved.source_url,
                Position {
                    line: resolved.resolved.line.saturating_sub(1),
                    column: resolved.resolved.column.saturating_sub(1),
                },
                resolved.breadcrumb,
            )
        });
        let (source_url, location, name) = match mapped {
            Some((url, position, name)) => (
                url.clone(),
                source_location(url, position.line, position.column),
                name.map(|name| heap_class_display_name(&name).to_owned())
                    .unwrap_or_else(|| group.generated_name.clone()),
            ),
            None => (
                generated_url.clone(),
                source_location(generated_url, group.line, group.column),
                heap_class_display_name(&group.generated_name).to_owned(),
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
                script_id.clone(),
                source_url.clone(),
                location.line,
                location.column,
                name.clone(),
            ))
            .or_insert_with(|| ProjectedHeapClass {
                script_id,
                provenance: script
                    .map(|script| script.provenance.clone())
                    .unwrap_or_default(),
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
                script_id: class.script_id,
                provenance: class.provenance,
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
    let mapping_status = if diagnostics
        .iter()
        .any(|d| d.status == HeapMappingStatus::Mapped)
    {
        HeapMappingStatus::Mapped
    } else if diagnostics
        .iter()
        .any(|d| d.status == HeapMappingStatus::MapLoadingFailed)
    {
        HeapMappingStatus::MapLoadingFailed
    } else if mapping.is_none()
        || diagnostics
            .iter()
            .any(|d| d.status == HeapMappingStatus::NotAttempted)
    {
        HeapMappingStatus::NotAttempted
    } else {
        HeapMappingStatus::NoMapSupplied
    };
    Ok(HeapClassSnapshot {
        capture_id,
        total_instances,
        total_shallow_size,
        classes,
        analysis: HeapClassAnalysisSnapshot {
            snapshot_timing: None,
            parse_duration_micros: 0,
            projection_duration_micros: 0,
            source_map_hydration_duration_micros: mapping
                .map_or(0, |mapping| mapping.hydration_duration_micros),
            constructor_group_count: groups.len() as u64,
            used_cached_groups: false,
            mapping_status,
            script_mappings: diagnostics,
        },
    })
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
        .dom()
        .get_document(None, None)
        .await
        .map_err(|error| TargetDebuggerError::Interaction(format!("{error:?}")))?;
    let node = driver
        .client()
        .dom()
        .query_selector(document.root.node_id, selector.clone())
        .await
        .map_err(|error| TargetDebuggerError::Interaction(format!("{error:?}")))?;
    if node.node_id == 0 {
        return Err(TargetDebuggerError::SelectorNotFound(selector));
    }

    let model = driver
        .client()
        .dom()
        .get_box_model(Some(node.node_id), None, None)
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
                .input()
                .dispatch_mouse_event(event)
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
            .input()
            .insert_text(text)
            .await
            .map(|_| ())
            .map_err(|error| TargetDebuggerError::Interaction(format!("{error:?}")))
    })
}

async fn capture_coverage(
    driver: &mut DebuggerDriver,
    session_key: &SessionKey,
    recording: &mut CoverageRecording,
    capture_id: Option<String>,
) -> Result<CoverageSnapshot, TargetDebuggerError> {
    if let Some(capture_id) = &capture_id
        && recording.captures.contains_key(capture_id)
    {
        return Err(TargetDebuggerError::CoverageCaptureAlreadyExists(
            capture_id.clone(),
        ));
    }
    let mut snapshot = take_coverage(driver, recording).await?;
    attach_coverage_provenance(driver, session_key, &mut snapshot);
    if let Some(capture_id) = capture_id {
        recording.captures.insert(capture_id, snapshot.clone());
    }
    Ok(snapshot)
}

fn capture_script_provenance(script: &crate::debugger_engine::ScriptState) -> CaptureScriptProvenance {
    CaptureScriptProvenance {
        url: cheap_capture_url(&script.url),
        source_map_url: script.captured_source.as_ref()
            .and_then(|captured| captured.source_map_url.clone())
            .or_else(|| script.source_map_url.clone())
            .filter(|url| url.len() <= 2048 && !is_inline_source_url(url)),
        source_sha256: script.captured_source.as_ref()
            .map(|captured| format!("{:x}", Sha256::digest(captured.content.as_bytes())))
            .or_else(|| {
                (script.hash.len() == 64 && script.hash.bytes().all(|byte| byte.is_ascii_hexdigit()))
                    .then(|| script.hash.to_ascii_lowercase())
            }),
    }
}

fn capture_cpu_script_provenance(
    nodes: &[CpuProfileNodeSnapshot],
    mut capture: impl FnMut(&str, &str) -> Option<CaptureScriptProvenance>,
) -> BTreeMap<String, CaptureScriptProvenance> {
    let mut provenance = BTreeMap::new();
    let mut seen = BTreeSet::new();
    for node in nodes {
        if provenance.contains_key(&node.call_frame.script_id)
            || !seen.insert((node.call_frame.script_id.clone(), node.call_frame.url.clone()))
        {
            continue;
        }
        if let Some(script) = capture(&node.call_frame.script_id, &node.call_frame.url) {
            provenance.insert(node.call_frame.script_id.clone(), script);
        }
    }
    provenance
}

fn is_inline_source_url(url: &str) -> bool {
    url.get(..5)
        .is_some_and(|prefix| prefix.eq_ignore_ascii_case("data:"))
}

fn cheap_capture_url(url: &str) -> String {
    if is_inline_source_url(url) {
        "<inline-script-url>".to_owned()
    } else if url.len() > 2048 {
        "<oversized-script-url>".to_owned()
    } else {
        url.to_owned()
    }
}

fn attach_coverage_provenance(driver: &DebuggerDriver, session: &SessionKey, snapshot: &mut CoverageSnapshot) {
    for source in &mut snapshot.sources {
        let key = ScriptKey { session: session.clone(), script_id: source.script_id.clone() };
        source.provenance = Some(if let Some(script) = driver.state().scripts.get(&key)
            && cheap_capture_url(&script.url) == source.generated_url {
            capture_script_provenance(script)
        } else {
            CaptureScriptProvenance {
                url: source.generated_url.clone(),
                source_map_url: None,
                source_sha256: None,
            }
        });
    }
}

async fn finish_coverage_recording(
    driver: &DebuggerDriver,
    recording: &mut Option<CoverageRecording>,
) -> Result<CoverageRecording, TargetDebuggerError> {
    let active = recording
        .as_mut()
        .ok_or(TargetDebuggerError::CoverageNotActive)?;
    update_coverage(driver, active).await?;
    driver
        .client()
        .profiler()
        .stop_precise_coverage()
        .await
        .map_err(|error| TargetDebuggerError::Coverage(format!("{error:?}")))?;
    let completed = recording
        .take()
        .ok_or(TargetDebuggerError::CoverageNotActive)?;
    Ok(completed)
}

async fn start_coverage(
    driver: &mut DebuggerDriver,
    _session_key: &SessionKey,
) -> Result<(), TargetDebuggerError> {
    driver
        .client()
        .profiler()
        .enable()
        .await
        .map_err(|error| TargetDebuggerError::Coverage(format!("{error:?}")))?;
    driver
        .client()
        .profiler()
        .start_precise_coverage(Some(true), Some(true), None)
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
        .profiler()
        .take_precise_coverage()
        .await
        .map_err(|error| TargetDebuggerError::Coverage(format!("{error:?}")))?;
    recording.timestamp_micros = (coverage.timestamp * 1_000_000.0).max(0.0) as u64;
    for script in coverage.result {
        recording.merge(script);
    }
    Ok(())
}

pub(crate) fn effective_coverage_ranges(ranges: &[CoverageRangeSnapshot]) -> Vec<CoverageRangeSnapshot> {
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
    driver
        .source_effects()
        .script_contains_authored_source(driver.state(), script, source_path)
}

async fn hydrate_source_for_path(
    driver: &mut DebuggerDriver,
    source_path: &str,
) -> Result<(), TargetDebuggerError> {
    acquire_sources(driver, SourceAcquisition::Exact(source_path), None).await
}

async fn hydrate_sources(
    driver: &mut DebuggerDriver,
    include_unmapped: bool,
) -> Result<(), TargetDebuggerError> {
    acquire_sources(driver, SourceAcquisition::All { include_unmapped }, None).await
}

#[derive(Clone, Copy)]
enum SourceAcquisition<'a> {
    Exact(&'a str),
    Search(Option<&'a str>),
    All { include_unmapped: bool },
}

fn original_source_path(path: &str) -> &str {
    path.strip_suffix("?formatted").unwrap_or(path)
}

fn source_acquisition_candidates(
    state: &DebuggerState,
    request: SourceAcquisition<'_>,
) -> Vec<ScriptKey> {
    let exact_runtime = match request {
        SourceAcquisition::Exact(path) => state
            .scripts
            .values()
            .any(|script| script.url == original_source_path(path)),
        _ => false,
    };
    state
        .scripts
        .iter()
        .filter_map(|(key, script)| {
            let selected = match request {
                SourceAcquisition::Exact(path) if exact_runtime => {
                    script.url == original_source_path(path)
                }
                SourceAcquisition::Exact(_) => script.source_map_url.is_some(),
                SourceAcquisition::Search(selector) => {
                    selector
                        .is_none_or(|selector| script.url.contains(original_source_path(selector)))
                        || script.source_map_url.is_some()
                }
                SourceAcquisition::All { include_unmapped } => {
                    include_unmapped || script.source_map_url.is_some()
                }
            };
            (selected
                && matches!(
                    script.source,
                    ScriptSourceState::Unresolved | ScriptSourceState::Failed(_)
                ))
            .then(|| key.clone())
        })
        .collect()
}

async fn acquire_sources(
    driver: &mut DebuggerDriver,
    request: SourceAcquisition<'_>,
    control: Option<&SearchControl>,
) -> Result<(), TargetDebuggerError> {
    let authored_path = match request {
        SourceAcquisition::Exact(path)
            if !driver
                .state()
                .scripts
                .values()
                .any(|script| script.url == original_source_path(path)) =>
        {
            Some(original_source_path(path))
        }
        _ => None,
    };
    if let Some(path) = authored_path
        && driver
            .state()
            .scripts
            .keys()
            .any(|script| script_contains_source(driver, script, path))
    {
        return Ok(());
    }
    for script in source_acquisition_candidates(driver.state(), request) {
        if let Some(control) = control {
            control.check()?;
        }
        driver
            .acquire_script_source(script.clone(), control)
            .await?;
        if let Some(control) = control {
            control.check()?;
        }
        if let Some(path) = authored_path
            && script_contains_source(driver, &script, path)
        {
            break;
        }
    }
    Ok(())
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
        .profiler()
        .enable()
        .await
        .map_err(|error| TargetDebuggerError::CpuProfile(format!("{error:?}")))?;
    if let Some(interval) = sampling_interval_micros {
        driver
            .client()
            .profiler()
            .set_sampling_interval(interval as i64)
            .await
            .map_err(|error| TargetDebuggerError::CpuProfile(format!("{error:?}")))?;
    }
    driver
        .client()
        .profiler()
        .start()
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
    let time_deltas_micros = profile.time_deltas.unwrap_or_default();
    let nodes = profile
        .nodes
        .into_iter()
        .map(|node| CpuProfileNodeSnapshot {
            id: node.id,
            call_frame: CpuProfileCallFrameSnapshot {
                function_name: node.call_frame.function_name,
                script_id: node.call_frame.script_id,
                url: cheap_capture_url(&node.call_frame.url),
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
    let snapshot = CpuProfileSnapshot {
        capture_id,
        sampling_interval_micros,
        start_time_micros: profile.start_time,
        end_time_micros: profile.end_time,
        nodes,
        samples,
        time_deltas_micros,
        functions: Vec::new(),
        analysis: None,
        script_provenance: BTreeMap::new(),
        projection_diagnostics: Vec::new(),
    };
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
    for (sample_id, delta) in cpu_profile_sample_durations(snapshot)? {
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

fn cpu_profile_sample_durations(
    snapshot: &CpuProfileSnapshot,
) -> Result<Vec<(i64, u64)>, TargetDebuggerError> {
    if snapshot.samples.len() != snapshot.time_deltas_micros.len() {
        return Err(TargetDebuggerError::InvalidCpuProfile(format!(
            "received {} samples but {} time deltas",
            snapshot.samples.len(),
            snapshot.time_deltas_micros.len()
        )));
    }

    // V8 processes VM and sampler queues separately, so CDP deltas can be negative.
    // Like DevTools, sort reconstructed timestamps with their samples, not the deltas.
    // Use offsets from startTime to keep microsecond precision without f64 arithmetic.
    let mut timestamp = 0_u64;
    let mut samples = Vec::with_capacity(snapshot.samples.len());
    for (index, (&sample, &delta)) in snapshot
        .samples
        .iter()
        .zip(&snapshot.time_deltas_micros)
        .enumerate()
    {
        timestamp = timestamp.checked_add_signed(delta).ok_or_else(|| {
            TargetDebuggerError::InvalidCpuProfile(format!(
                "sample {index} timestamp offset is outside 0..=u64::MAX ({timestamp} + {delta})"
            ))
        })?;
        samples.push((sample, timestamp));
    }
    samples.sort_by_key(|&(_, timestamp)| timestamp);
    let mut previous = 0;
    for (_, timestamp) in &mut samples {
        let current = *timestamp;
        *timestamp = current - previous;
        previous = current;
    }
    Ok(samples)
}

pub(crate) fn aggregate_cpu_profile(
    snapshot: &mut CpuProfileSnapshot,
) -> Result<(), TargetDebuggerError> {
    let sample_durations = cpu_profile_sample_durations(snapshot)?;
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

    for (sample_id, delta) in sample_durations {
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
        accumulated.url = cheap_capture_url(&script.url);
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
}

pub(crate) fn exclude_coverage(
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
            let identity = (
                function.name.as_str(),
                function.block_coverage,
                function.root_start_offset,
                function.root_end_offset,
            );
            let Some(baseline_function) = baseline_source.functions.iter().find(|candidate| {
                (
                    candidate.name.as_str(),
                    candidate.block_coverage,
                    candidate.root_start_offset,
                    candidate.root_end_offset,
                ) == identity
            }) else {
                continue;
            };
            let original_ranges = function.ranges.clone();
            function.ranges.retain(|range| {
                !baseline_function.ranges.iter().any(|candidate| {
                    candidate.start_offset == range.start_offset
                        && candidate.end_offset == range.end_offset
                        && candidate.count > 0
                })
            });
            if function.ranges.len() == original_ranges.len() {
                continue;
            }
            // Rebuild derived ranges using persisted mappings, never a live target.
            if !function.effective_ranges.is_empty() {
                let projected = original_ranges
                    .iter()
                    .chain(&function.effective_ranges)
                    .collect::<Vec<_>>();
                let mut effective = effective_coverage_ranges(&function.ranges);
                for range in &mut effective {
                    range.authored_start = projected
                        .iter()
                        .find(|original| {
                            original.start_offset == range.start_offset
                                && original.authored_start.is_some()
                        })
                        .and_then(|original| original.authored_start.clone());
                    range.authored_end = projected
                        .iter()
                        .find(|original| {
                            original.end_offset == range.end_offset
                                && original.authored_end.is_some()
                        })
                        .and_then(|original| original.authored_end.clone());
                }
                function.authored_location = effective
                    .iter()
                    .find_map(|range| range.authored_start.clone());
                function.effective_ranges = effective;
            }
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

impl CoverageRecording {
    fn snapshot(&self) -> CoverageSnapshot {
        CoverageSnapshot {
            capture_id: None,
            timestamp_micros: self.timestamp_micros,
            analysis: None,
            projection_diagnostics: Vec::new(),
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
                        provenance: None,
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
    driver: &mut DebuggerDriver,
    session_key: &SessionKey,
    pause_epoch: Option<u64>,
    frame_index: u32,
    expression: String,
) -> Result<EvaluationSnapshot, TargetDebuggerError> {
    const OBJECT_GROUP: &str = "dbgjs-ephemeral-evaluation";
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
    let snapshot = match result {
        Ok(result) => {
            let mut preview = remote_value_snapshot(
                &result.remote,
                crate::promise_debugging::DEFAULT_VALUE_PREVIEW_LENGTH,
            );
            preview.truncated |= result.preview_truncated;
            if let Some(object_id) = &result.remote.object_id {
                preview.source = crate::object_inspection::LiveSourceInspector::default()
                    .inspect(driver, session_key, object_id, None)
                    .await;
            }
            preview.reference = None;
            let kind = remote_object_kind(&result.remote);
            Ok(EvaluationSnapshot {
                expression,
                kind,
                value: result.remote.value,
                unserializable_value: result.remote.unserializable_value,
                description: result.remote.description,
                object_id: None,
                preview,
            })
        }
        Err(error) => Err(error),
    };
    let release = driver
        .client()
        .runtime()
        .release_object_group(OBJECT_GROUP.to_owned())
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
            .debugger()
            .evaluate_on_call_frame(
                params.call_frame_id,
                params.expression,
                params.object_group,
                params.include_command_line_api,
                params.silent,
                params.return_by_value,
                params.generate_preview,
                params.throw_on_side_effect,
                params.timeout,
                params.scope_number,
            )
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
            .runtime()
            .evaluate(
                params.expression,
                params.object_group,
                params.include_command_line_api,
                params.silent,
                params.context_id,
                params.return_by_value,
                params.generate_preview,
                params.user_gesture,
                params.await_promise,
                params.throw_on_side_effect,
                params.timeout,
                params.disable_breaks,
                params.repl_mode,
                params.allow_unsafe_eval_blocked_by_csp,
                params.unique_context_id,
                params.serialization_options,
            )
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
        .runtime()
        .call_function_on(
            projection_params.function_declaration,
            projection_params.object_id,
            projection_params.arguments,
            projection_params.silent,
            projection_params.return_by_value,
            projection_params.generate_preview,
            projection_params.user_gesture,
            projection_params.await_promise,
            projection_params.execution_context_id,
            projection_params.object_group,
            projection_params.throw_on_side_effect,
            projection_params.unique_context_id,
            projection_params.serialization_options,
        )
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
  __dbgjsValue: (
{expression}
  )
}})"#
    )
}

fn bounded_projection_function(max_preview_length: u32) -> String {
    format!(
        r#"function() {{
  const __dbgjsValue = this.__dbgjsValue;
  const __dbgjsMaxLength = {max_preview_length};
  const __dbgjsKind = typeof __dbgjsValue;
  let __dbgjsText;
  if (__dbgjsKind === "string") {{
    __dbgjsText = __dbgjsValue;
  }} else if (__dbgjsKind === "bigint") {{
    __dbgjsText = `${{__dbgjsValue}}n`;
  }} else if (__dbgjsKind === "symbol") {{
    // A Symbol description is only exposed through a mutable prototype getter.
    return {{ __dbgjsKind, __dbgjsTruncated: true }};
  }} else if (__dbgjsKind === "number" && __dbgjsValue !== __dbgjsValue) {{
    __dbgjsText = "NaN";
  }} else if (__dbgjsKind === "number" && __dbgjsValue === 1 / 0) {{
    __dbgjsText = "Infinity";
  }} else if (__dbgjsKind === "number" && __dbgjsValue === -1 / 0) {{
    __dbgjsText = "-Infinity";
  }} else if (
    __dbgjsKind === "number"
    && __dbgjsValue === 0
    && 1 / __dbgjsValue === -1 / 0
  ) {{
    __dbgjsText = "-0";
  }} else {{
    return {{ __dbgjsKind: "remote", __dbgjsValue }};
  }}
  if (__dbgjsMaxLength >= __dbgjsText.length) {{
    return {{ __dbgjsKind, __dbgjsText, __dbgjsTruncated: false }};
  }}
  let __dbgjsPreview = "";
  let __dbgjsLength = 0;
  let __dbgjsOffset = 0;
  // In-range string index and length reads use own exotic data, not prototype hooks.
  while (
    __dbgjsOffset < __dbgjsText.length
    && __dbgjsLength < __dbgjsMaxLength
  ) {{
    const __dbgjsFirst = __dbgjsText[__dbgjsOffset];
    __dbgjsPreview += __dbgjsFirst;
    __dbgjsOffset++;
    if (
      __dbgjsFirst >= "\uD800"
      && __dbgjsFirst <= "\uDBFF"
      && __dbgjsOffset < __dbgjsText.length
    ) {{
      const __dbgjsSecond = __dbgjsText[__dbgjsOffset];
      if (__dbgjsSecond >= "\uDC00" && __dbgjsSecond <= "\uDFFF") {{
        __dbgjsPreview += __dbgjsSecond;
        __dbgjsOffset++;
      }}
    }}
    __dbgjsLength++;
  }}
  return {{
    __dbgjsKind,
    __dbgjsText: __dbgjsPreview,
    __dbgjsTruncated: __dbgjsOffset < __dbgjsText.length
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
            .runtime()
            .release_object(object_id)
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
    let kind = property("__dbgjsKind")
        .and_then(|value| value.value.as_ref())
        .and_then(serde_json::Value::as_str)
        .ok_or_else(|| {
            TargetDebuggerError::Evaluation(
                "target returned an invalid bounded evaluation envelope".to_owned(),
            )
        })?;
    if kind == "remote" {
        let value = property("__dbgjsValue").ok_or_else(|| {
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
    let preview = property("__dbgjsText")
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
        preview_truncated: property("__dbgjsTruncated")
            .and_then(|value| value.value.as_ref())
            .and_then(serde_json::Value::as_bool)
            .unwrap_or(false),
    })
}

async fn scope_variables(
    driver: &mut DebuggerDriver,
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
    driver: &mut DebuggerDriver,
    session_key: &SessionKey,
    pause_epoch: Option<u64>,
    object_id: String,
) -> Result<Vec<VariableSnapshot>, TargetDebuggerError> {
    let (properties, _) =
        get_object_property_descriptors(driver, session_key, pause_epoch, object_id).await?;
    let mut variables = properties
        .into_iter()
        .filter_map(|property| {
            property
                .value
                .map(|value| variable_snapshot(property.name, value))
        })
        .collect::<Vec<_>>();
    let mut inspector = crate::object_inspection::LiveSourceInspector::default();
    for variable in &mut variables {
        if let Some(object_id) = &variable.object_id {
            variable.preview.source = inspector
                .inspect(driver, session_key, object_id, None)
                .await;
        }
    }
    Ok(variables)
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
    let result = driver
        .client()
        .runtime()
        .get_properties(object_id, Some(true), None, Some(true), None)
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
    driver: &mut DebuggerDriver,
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
    let mut inspector = crate::object_inspection::LiveSourceInspector::default();
    let source = if let Some(object_id) = &object_id {
        let known = object_group
            .is_none()
            .then_some((properties.as_slice(), internal_properties.as_slice()));
        inspector
            .inspect(driver, session_key, object_id, known)
            .await
    } else {
        Default::default()
    };
    let is_promise = remote
        .as_ref()
        .is_some_and(|value| value.remote.subtype == Some(RuntimeRemoteObjectSubtype::Promise))
        || has_live_promise_evidence(&internal_properties);
    let mut promise = if is_promise {
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
            source: Default::default(),
        },
        |value| remote_value_snapshot(&value.remote, options.max_preview_length),
    );
    preview.truncated |= remote.as_ref().is_some_and(|value| value.preview_truncated);
    preview.source = source;
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
                        source: Default::default(),
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
    for property in &mut properties {
        if let Some(object_id) = &property.value.reference {
            property.value.source = inspector
                .inspect(driver, session_key, object_id, None)
                .await;
        }
    }
    if let Some(settlement) = promise
        .as_mut()
        .and_then(|promise| promise.settlement.as_mut())
        && let Some(object_id) = &settlement.reference
    {
        settlement.source = inspector
            .inspect(driver, session_key, object_id, None)
            .await;
    }
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
    for (script_snapshot, (script_key, script)) in result.scripts.iter_mut().zip(
        driver
            .state()
            .scripts
            .iter()
            .filter(|(script_key, _)| script_key.session == *session_key),
    ) {
        if matches!(script.source, ScriptSourceState::Resolved(_)) {
            script_snapshot.status = TargetScriptStatus::Resolved {
                authored_sources: driver
                    .source_effects()
                    .authored_source_paths(driver.state(), script_key),
            };
        }
    }
    result.logs = driver.console_messages().iter().cloned().collect();
    result.log_capture = driver.log_capture();
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
    source_map_url: Option<&str>,
) -> BreakpointSourceCandidateSnapshot {
    let source_url = if source_map_url.is_some()
        && url::Url::parse(&candidate.source_url).is_err()
    {
        crate::source_view::canonical_source_uri(source_map_url, &candidate.source_url).display()
    } else {
        candidate.source_url.clone()
    };
    BreakpointSourceCandidateSnapshot {
        source_url,
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
            log_capture: Default::default(),
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
                    let source_map_url = script.captured_source.as_ref()
                        .and_then(|captured| captured.source_map_url.as_deref())
                        .or(script.source_map_url.as_deref());
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
                                    .map(|candidate| breakpoint_candidate_snapshot(
                                        candidate, source_map_url
                                    ))
                                    .collect(),
                                omitted_candidate_count: u32::try_from(*omitted_candidate_count)
                                    .unwrap_or(u32::MAX),
                            },
                            BreakpointAssessmentStatus::Mapping { candidate, .. } => {
                                BreakpointScriptAssessmentStatus::Mapping {
                                    candidate: breakpoint_candidate_snapshot(
                                        candidate, source_map_url
                                    ),
                                }
                            }
                            BreakpointAssessmentStatus::Unmapped {
                                candidate,
                                diagnostics,
                            } => BreakpointScriptAssessmentStatus::Unmapped {
                                candidate: breakpoint_candidate_snapshot(
                                    candidate, source_map_url
                                ),
                                diagnostics: diagnostics.as_ref().clone(),
                            },
                            BreakpointAssessmentStatus::Applicable {
                                candidate,
                                mappings,
                            } => BreakpointScriptAssessmentStatus::Applicable {
                                candidate: breakpoint_candidate_snapshot(
                                    candidate, source_map_url
                                ),
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
        log_capture: Default::default(),
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
    #[error("logpoint capture transport unavailable: {0}")]
    LogpointTransport(String),
    #[error("breakpoint {0} belongs to the context, not a target logpoint")]
    BreakpointOwnedByContext(String),
    #[error("breakpoint {0} belongs to a target logpoint, not the context")]
    BreakpointOwnedByTarget(String),
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
#[path = "source_acquisition_tests.rs"]
mod source_acquisition_tests;

#[cfg(test)]
#[path = "heap_mapping_retry_tests.rs"]
mod heap_mapping_retry_tests;

#[cfg(test)]
#[path = "coverage_finalization_regression_tests.rs"]
mod coverage_finalization_regression_tests;

#[cfg(test)]
mod tests {
    fn representative_stored_graph() -> super::StoredHeapGraph {
        use super::{StoredHeapCapture, StoredHeapGraph};
        use crate::heap_graph::parse_heap_graph;
        let json = r#"{
            "snapshot":{"meta":{
                "node_fields":["type","name","id","self_size","edge_count"],
                "node_types":[["synthetic","object","string"],"string","number","number","number"],
                "edge_fields":["type","name_or_index","to_node"],
                "edge_types":[["property","weak"],"string_or_number","node"]
            },"node_count":3,"edge_count":2},
            "nodes":[0,0,1,1,2, 1,1,3,10,0, 2,2,5,20,0],
            "edges":[0,3,5, 1,4,10],
            "strings":["root","Object","hello","strong","weak"],
            "locations":[]
        }"#;
        StoredHeapGraph {
            capture_id: "sample".into(),
            capture: StoredHeapCapture {
                path: Default::default(),
                timing: Default::default(),
                mapping: crate::service_api::HeapMappingSnapshot {
                    connection_generation: 0,
                    scripts: Vec::new(),
                    hydration_duration_micros: 0,
                },
                source_resolver: Default::default(),
            },
            graph: parse_heap_graph(json.as_bytes()).unwrap(),
            parse_duration: Default::default(),
        }
    }

    #[test]
    fn stored_heap_graph_selection_and_traversal_contract() {
        use super::TargetDebuggerError;
        use crate::service_api::{
            HeapAggregateBy, HeapEdgePolicy, HeapNodeSelector, HeapPathOptions,
            HeapReferenceDirection,
        };
        let graph = representative_stored_graph();
        let selected = graph.select(
            HeapNodeSelector { name_regex: Some("Object".into()), ..Default::default() },
            None, true,
        ).unwrap();
        assert_eq!((selected.total_nodes, selected.total_edges), (3, 2));
        assert_eq!(selected.nodes[0].reference, "sample#3");
        assert_eq!(selected.nodes.len(), 1);
        assert!(matches!(
            graph.select(HeapNodeSelector {
                name: Some("Object".into()), name_regex: Some("Object".into()),
                ..Default::default()
            }, None, false),
            Err(TargetDebuggerError::InvalidHeapSelector(_))
        ));
        let refs = graph.references(".#1", HeapReferenceDirection::Both,
            HeapEdgePolicy::Strong, 1, None).unwrap();
        assert_eq!(refs.node.reference, "sample#1");
        assert_eq!(refs.references.len(), 1);
        assert_eq!(refs.omitted_reference_count, 0);
        assert_eq!(refs.references[0].target, "sample#3");
        assert_eq!(graph.references(".#1", HeapReferenceDirection::Outgoing,
            HeapEdgePolicy::All, 10, None).unwrap().references.len(), 2);
        assert_eq!(graph.references(".#1", HeapReferenceDirection::Outgoing,
            HeapEdgePolicy::All, 1, None).unwrap().omitted_reference_count, 1);
        let path = graph.path(".#1".into(), ".#3".into(),
            HeapPathOptions::default(), None).unwrap().unwrap();
        assert_eq!((path.from.as_str(), path.to.as_str()), ("sample#1", "sample#3"));
        assert_eq!(path.steps.len(), 1);
        assert_eq!(graph.dominators(".#3", None).unwrap().chain[0].reference, "sample#1");
        let aggregate = graph.aggregate(HeapAggregateBy::NodeType, 1, None).unwrap();
        assert_eq!(aggregate.entries.len(), 1);
        assert_eq!(aggregate.omitted_entry_count, 2);
        assert!(graph.diff(&graph, HeapAggregateBy::Name, 10, None).unwrap().entries.is_empty());
    }

    #[test]
    fn live_and_stored_heap_graph_operations_match() {
        use crate::service_api::{
            HeapAggregateBy, HeapEdgePolicy, HeapNodeSelector, HeapPathOptions,
            HeapReferenceDirection,
        };
        let stored = representative_stored_graph();
        let mut captures = std::collections::BTreeMap::new();
        captures.insert(stored.capture_id.clone(), stored.capture.clone());
        let live = super::live_heap_graph_view(
            &stored.graph, &stored.capture_id, &captures, stored.parse_duration, false,
        );
        let selector = HeapNodeSelector { node_type: Some("object".into()), ..Default::default() };
        assert_eq!(live.select(selector.clone(), Some(3), true).unwrap(),
            stored.select(selector, Some(3), true).unwrap());
        assert_eq!(live.promises(None, 1, 3).unwrap(), stored.promises(None, 1, 3).unwrap());
        assert_eq!(live.references("sample#1", HeapReferenceDirection::Both,
            HeapEdgePolicy::All, 1, Some(3)).unwrap(),
            stored.references("sample#1", HeapReferenceDirection::Both,
                HeapEdgePolicy::All, 1, Some(3)).unwrap());
        assert_eq!(live.path("sample#1".into(), "sample#3".into(),
            HeapPathOptions::default(), Some(3)).unwrap(),
            stored.path("sample#1".into(), "sample#3".into(),
                HeapPathOptions::default(), Some(3)).unwrap());
        let live_path = live.path("sample#01".into(), "sample#03".into(),
            HeapPathOptions::default(), None).unwrap().unwrap();
        assert_eq!((live_path.from.as_str(), live_path.to.as_str()),
            ("sample#01", "sample#03"));
        let stored_path = stored.path("sample#01".into(), "sample#03".into(),
            HeapPathOptions::default(), None).unwrap().unwrap();
        assert_eq!((stored_path.from.as_str(), stored_path.to.as_str()),
            ("sample#1", "sample#3"));
        let stored_alias = stored.path(".#01".into(), ".#03".into(),
            HeapPathOptions::default(), None).unwrap().unwrap();
        assert_eq!((stored_alias.from.as_str(), stored_alias.to.as_str()),
            ("sample#1", "sample#3"));
        assert_eq!(live.dominators("sample#3", Some(3)).unwrap(),
            stored.dominators("sample#3", Some(3)).unwrap());
        assert_eq!(live.aggregate(HeapAggregateBy::Name, 1, Some(3)).unwrap(),
            stored.aggregate(HeapAggregateBy::Name, 1, Some(3)).unwrap());
        assert_eq!(live.diff(&stored.view(), HeapAggregateBy::Name, 1, Some(3)).unwrap(),
            stored.diff(&stored, HeapAggregateBy::Name, 1, Some(3)).unwrap());
    }

    use super::{BreakpointLifetime, BreakpointOwner, BreakpointOwnership};
    use super::{
        TargetDebuggerError, TargetDebuggerHandle, aggregate_cpu_profile, bounded_heap_text,
        bounded_projection_function, breakpoint_wait_failure, callback_aware_breadcrumb,
        capture_cpu_script_provenance,
        complete_source_search_batch, cpu_profile_sample_durations, cpu_profile_snapshot,
        effective_coverage_ranges, evaluated_remote_from_envelope, forward_heap_snapshot_progress,
        heap_class_display_name, predicate_matches, publish_snapshot, snapshot, source_excerpt,
        window_highlighted_line,
    };
    use super::{captured_heap_script, project_heap_classes, supply_heap_source_map};
    use crate::cdp::{RuntimePropertyDescriptor, RuntimeRemoteObject, RuntimeRemoteObjectType};
    use crate::cdp_runtime::CdpConnection;
    use crate::content_store::ContentStore;
    use crate::context_source_model::ContextSourceModel;
    use crate::debugger_engine::{
        self, BreakpointAssessment, BreakpointAssessmentStatus, BreakpointBinding, BreakpointKey,
        BreakpointMapping, BreakpointSourceCandidate, BreakpointState, DebuggerState, EffectId,
        Input, PhysicalBreakpointKey, ScriptKey, ScriptSourceState, ScriptState, SessionKey,
    };
    use crate::heap_snapshot::HeapConstructorGroup;
    use crate::service_api::{
        BreakpointApplicationStatus, CaptureScriptProvenance, CoverageRangeSnapshot, CpuProfileCallFrameSnapshot,
        CpuProfileNodeSnapshot, CpuProfileSnapshot, SourceExcerpt, SourceLocation,
        TargetBreakpointStatus, TargetDebuggerPhase, TargetDebuggerSnapshot, TargetWaitPredicate,
        ValueInspectionOptions, ValueSelector, ValueSnapshot,
    };
    use sha2::{Digest, Sha256};
    use crate::service_api::{
        HeapMappingSnapshot, HeapMappingStatus, HeapScriptSnapshot, HeapSourceMapSupply,
        ScriptProvenance,
    };
    use crate::source_search::{SearchControl, SearchError};
    use crate::source_view::{ContentCandidate, Position, Provenance};
    use crate::websocket_transport::CdpWebSocketTransport;
    use std::collections::BTreeMap;
    use std::sync::Arc;

    #[test]
    fn breakpoint_lifetimes_keep_context_revisions_separate_from_target_generation() {
        let context = BreakpointLifetime::ContextIntent;
        let target = BreakpointLifetime::TargetGeneration;
        let id = "log:collision";
        let mut owner = BreakpointOwnership::default();
        owner = owner.installed(context(5));
        assert!(owner.accepts(target, id).is_err());
        assert!(!owner.accepts(context(4), id).unwrap());
        owner = owner.context_removed(6);
        owner = owner.installed(target);
        assert_eq!(owner.owner, Some(BreakpointOwner::TargetGeneration));
        assert!(owner.accepts(context(7), id).is_err());
        owner = owner.context_removed(7);
        assert_eq!(owner.owner, Some(BreakpointOwner::TargetGeneration));
        assert!(!owner.accepts(context(6), id).unwrap());
        owner.owner = None;
        assert!(!owner.accepts(context(6), id).unwrap());
        owner = owner.installed(context(8));
        assert!(owner.accepts(target, id).is_err());
        owner = owner.context_removed(9);
        assert!(owner.accepts(target, id).unwrap());
        owner = owner.installed(target);
        assert_eq!(owner.context_revision, Some(9));
        assert!(owner.accepts(context(u64::MAX), id).is_err(),
            "even the largest context revision must not override target instrumentation");
    }

    #[tokio::test]
    async fn heap_progress_forwarding_is_command_scoped_and_flushes_final_update() {
        use crate::cdp_runtime::HeapSnapshotStreamProgress;
        use tokio::sync::{mpsc, watch};

        let (updates_tx, mut updates) = watch::channel(Some(HeapSnapshotStreamProgress {
            done: 99,
            total: 100,
            finished: Some(false),
            bytes_written: 99,
        }));
        updates_tx.send_replace(Some(HeapSnapshotStreamProgress::default()));
        let (output, mut received) = mpsc::channel(16);
        let operation = async {
            updates_tx.send_replace(Some(HeapSnapshotStreamProgress {
                done: 5,
                total: 10,
                finished: Some(false),
                bytes_written: 50,
            }));
            tokio::time::sleep(std::time::Duration::from_millis(1)).await;
            updates_tx.send_replace(Some(HeapSnapshotStreamProgress {
                done: 10,
                total: 10,
                finished: Some(true),
                bytes_written: 100,
            }));
            Ok::<_, ()>(())
        };
        tokio::pin!(operation);
        forward_heap_snapshot_progress(&mut updates, &output, &mut operation)
            .await
            .unwrap();
        drop(output);
        let mut progress = Vec::new();
        while let Some(update) = received.recv().await {
            progress.push(update);
        }

        assert_eq!(progress.first().unwrap().bytes_written, 0);
        assert!(!progress.iter().any(|update| update.bytes_written == 99));
        assert_eq!(progress.last().unwrap().bytes_written, 100);
        assert_eq!(progress.last().unwrap().finished, Some(true));
    }

    #[tokio::test]
    async fn heap_progress_always_emits_completion_after_writer_finalization() {
        use crate::cdp_runtime::HeapSnapshotStreamProgress;
        use tokio::sync::{mpsc, watch};

        for finished in [None, Some(true)] {
            let (_updates_tx, updates) = watch::channel(Some(HeapSnapshotStreamProgress {
                done: 10,
                total: 10,
                finished,
                bytes_written: 100,
            }));
            let (output, mut received) = mpsc::channel(1);
            super::send_finished_heap_snapshot_progress(&updates, &output, 100)
                .await
                .unwrap();
            let progress = received.try_recv().expect("terminal progress must be sent");
            assert_eq!(progress.finished, Some(true));
            assert_eq!(progress.bytes_written, 100);
        }
    }

    #[tokio::test]
    async fn heap_progress_receiver_disconnect_does_not_abort_or_mask_command_error() {
        use crate::cdp_runtime::HeapSnapshotStreamProgress;
        use tokio::sync::{mpsc, watch};

        let (updates_tx, mut updates) = watch::channel(Some(HeapSnapshotStreamProgress::default()));
        let (output, received) = mpsc::channel(16);
        drop(received);
        let operation = async {
            updates_tx.send_replace(Some(HeapSnapshotStreamProgress {
                done: 1,
                total: 1,
                finished: Some(true),
                bytes_written: 12,
            }));
            Err::<(), _>("expected command failure")
        };
        tokio::pin!(operation);

        let result = tokio::time::timeout(
            std::time::Duration::from_secs(1),
            forward_heap_snapshot_progress(&mut updates, &output, &mut operation),
        )
        .await
        .expect("disconnected progress receiver must not block CDP cleanup");
        assert_eq!(result, Err("expected command failure"));
    }

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
            log_capture: Default::default(),
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
                envelope_property("__dbgjsKind", serde_json::json!(kind)),
                envelope_property(
                    "__dbgjsText",
                    serde_json::json!(value.or(unserializable).or(description).unwrap()),
                ),
                envelope_property("__dbgjsTruncated", serde_json::json!(true)),
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
            envelope_property("__dbgjsKind", serde_json::json!("symbol")),
            envelope_property("__dbgjsTruncated", serde_json::json!(true)),
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
        assert!(source.contains("__dbgjsText[__dbgjsOffset]"));
        assert!(source.contains(r#""\uD800""#));
        assert!(source.contains(r#""\uDC00""#));
    }

    #[test]
    fn remote_envelope_preserves_safe_primitive_payload() {
        let projection = evaluated_remote_from_envelope(&[
            envelope_property("__dbgjsKind", serde_json::json!("remote")),
            envelope_property("__dbgjsValue", serde_json::json!(true)),
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
                provenance: Default::default(),
                captured_source: None,
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
                    provenance: Default::default(),
                    captured_source: None,
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
                .target()
                .create_target(
                    "about:blank".into(),
                    None,
                    None,
                    None,
                    None,
                    None,
                    None,
                    None,
                    None,
                    None,
                    None,
                    None,
                    None,
                )
                .await
                .expect("create target");
            let attached = root
                .target()
                .attach_to_target(created.target_id.clone(), Some(true), None)
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

            inspect_live(&debugger, "globalThis.__dbgjsEvaluationCount = 0", true)
                .await
                .expect("counter initializes");
            transport.reset_largest_received_message_size();
            let huge = inspect_live(
                &debugger,
                "(globalThis.__dbgjsEvaluationCount++, 'x'.repeat(16 * 1024 * 1024))",
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
            let count = inspect_live(&debugger, "globalThis.__dbgjsEvaluationCount", true)
                .await
                .expect("counter reads");
            assert_eq!(count.preview.preview.as_deref(), Some("1"));

            inspect_live(&debugger, "globalThis.__dbgjsEvaluationCount = 0", true)
                .await
                .expect("counter resets");
            transport.reset_largest_received_message_size();
            let huge_bigint = inspect_live(
                &debugger,
                "(globalThis.__dbgjsEvaluationCount++, BigInt('9'.repeat(1_000_000)))",
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
            let count = inspect_live(&debugger, "globalThis.__dbgjsEvaluationCount", true)
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
                    "globalThis.__dbgjsForbiddenSideEffect = true",
                    false,
                )
                .await
                .is_err()
            );

            inspect_live(
                &debugger,
                r#"(() => {
  globalThis.__dbgjsEvaluationCount = 0;
  globalThis.__dbgjsPoisonedUnicode = "😀".repeat(1_000_000);
  globalThis.__dbgjsPoisonedBigInt = BigInt("8".repeat(1_000_000));
  globalThis.__dbgjsPoisonedSymbol = Symbol("z".repeat(1_000_000));
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
                "(globalThis.__dbgjsEvaluationCount++, globalThis.__dbgjsPoisonedUnicode)",
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
                "(globalThis.__dbgjsEvaluationCount++, globalThis.__dbgjsPoisonedBigInt)",
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
                "(globalThis.__dbgjsEvaluationCount++, globalThis.__dbgjsPoisonedSymbol)",
                true,
            )
            .await
            .expect("poisoned Symbol evaluates");
            assert_eq!(poisoned_symbol.preview.kind, "symbol");
            assert!(poisoned_symbol.preview.preview.is_none());
            assert!(poisoned_symbol.preview.truncated);
            assert!(transport.largest_received_message_size() < 64 * 1024);
            assert!(serde_json::to_vec(&poisoned_symbol).unwrap().len() < 2_048);

            let count = inspect_live(&debugger, "globalThis.__dbgjsEvaluationCount", true)
                .await
                .expect("poisoned evaluation count reads");
            assert_eq!(count.preview.preview.as_deref(), Some("3"));

            print!(
                "{}",
                include_str!("../tests/transcripts/bounded-evaluation.txt")
            );
            root.target()
                .close_target(created.target_id)
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

    fn raw_cpu_profile(samples: Vec<i64>, time_deltas: Vec<i64>) -> crate::cdp::ProfilerProfile {
        serde_json::from_value(serde_json::json!({
            "startTime": 18_014_398_509_481_984.0,
            "endTime": 18_014_398_509_482_984.0,
            "nodes": [
                {
                    "id": 1,
                    "callFrame": {
                        "functionName": "(root)", "scriptId": "0", "url": "",
                        "lineNumber": -1, "columnNumber": -1
                    },
                    "children": [2, 3]
                },
                {
                    "id": 2,
                    "callFrame": {
                        "functionName": "outer", "scriptId": "1", "url": "app.js",
                        "lineNumber": 0, "columnNumber": 0
                    }
                },
                {
                    "id": 3,
                    "callFrame": {
                        "functionName": "inner", "scriptId": "1", "url": "app.js",
                        "lineNumber": 1, "columnNumber": 0
                    }
                }
            ],
            "samples": samples,
            "timeDeltas": time_deltas
        }))
        .unwrap()
    }

    #[test]
    fn cpu_profile_preserves_signed_stream_and_aggregates_chronologically() {
        let raw = raw_cpu_profile(vec![2, 3, 2, 3, 2], vec![100, -28, 0, 78, -10]);
        let mut snapshot = cpu_profile_snapshot("typing-cpu".into(), Some(1_000), raw).unwrap();
        assert!(snapshot.functions.is_empty());
        aggregate_cpu_profile(&mut snapshot).unwrap();

        assert_eq!(snapshot.samples, vec![2, 3, 2, 3, 2]);
        assert_eq!(snapshot.time_deltas_micros, vec![100, -28, 0, 78, -10]);
        assert_eq!(snapshot.sampling_interval_micros, Some(1_000));
        assert_eq!(
            cpu_profile_sample_durations(&snapshot).unwrap(),
            vec![(3, 72), (2, 0), (2, 28), (2, 40), (3, 10)]
        );
        assert_eq!(snapshot.nodes[0].total_time_micros, 150);
        assert_eq!(snapshot.nodes[1].self_time_micros, 68);
        assert_eq!(snapshot.nodes[1].sample_count, 3);
        assert_eq!(snapshot.nodes[2].self_time_micros, 82);
        assert_eq!(snapshot.nodes[2].sample_count, 2);

        let mut ordered = cpu_profile_snapshot(
            "ordered".into(),
            None,
            raw_cpu_profile(vec![3, 2, 2, 2, 3], vec![72, 0, 28, 40, 10]),
        )
        .unwrap();
        aggregate_cpu_profile(&mut ordered).unwrap();
        assert_eq!(snapshot.nodes, ordered.nodes);
        assert_eq!(snapshot.functions, ordered.functions);

        let mut restored: CpuProfileSnapshot =
            serde_json::from_slice(&serde_json::to_vec(&snapshot).unwrap()).unwrap();
        aggregate_cpu_profile(&mut restored).unwrap();
        assert_eq!(restored, snapshot);
    }

    #[test]
    fn cpu_capture_does_not_aggregate_or_reject_unprojectable_samples() {
        let raw = raw_cpu_profile(vec![99], vec![12]);
        let snapshot = cpu_profile_snapshot("raw".into(), None, raw).unwrap();
        assert_eq!(snapshot.samples, vec![99]);
        assert_eq!(snapshot.time_deltas_micros, vec![12]);
        assert!(snapshot.functions.is_empty());
        assert!(aggregate_cpu_profile(&mut snapshot.clone()).is_err());
    }

    #[test]
    fn cpu_capture_builds_provenance_once_per_distinct_script() {
        let nodes = (0..500)
            .map(|id| profile_node(id, "work", Vec::new()))
            .collect::<Vec<_>>();
        let mut builds = 0;
        let provenance = capture_cpu_script_provenance(&nodes, |script_id, url| {
            builds += 1;
            assert_eq!(script_id, "1");
            assert_eq!(url, "app.js");
            Some(CaptureScriptProvenance {
                url: url.to_owned(),
                source_map_url: None,
                source_sha256: Some(format!("{:x}", Sha256::digest(b"already available source"))),
            })
        });
        assert_eq!(builds, 1);
        assert_eq!(provenance.len(), 1);
        let mut mismatched = profile_node(1000, "work", Vec::new());
        mismatched.call_frame.url = "other.js".into();
        let mut builds = 0;
        let provenance = capture_cpu_script_provenance(&[mismatched, nodes[0].clone()], |_, url| {
            builds += 1;
            (url == "app.js").then(|| CaptureScriptProvenance {
                url: url.to_owned(),
                source_map_url: None,
                source_sha256: None,
            })
        });
        assert_eq!(builds, 2);
        assert_eq!(provenance["1"].url, "app.js");
    }

    #[test]
    fn cpu_profile_keeps_empty_ordered_and_equal_timestamp_samples() {
        for (samples, deltas) in [
            (vec![], vec![]),
            (vec![2], vec![0]),
            (vec![2, 3, 2], vec![100, 50, 25]),
            (vec![3, 2, 3], vec![0, 0, 0]),
        ] {
            let mut snapshot = cpu_profile_snapshot(
                "test".into(),
                None,
                raw_cpu_profile(samples.clone(), deltas.clone()),
            )
            .unwrap();
            aggregate_cpu_profile(&mut snapshot).unwrap();
            let expected = samples
                .iter()
                .copied()
                .zip(deltas.iter().map(|&delta| u64::try_from(delta).unwrap()))
                .collect::<Vec<_>>();
            assert_eq!(cpu_profile_sample_durations(&snapshot).unwrap(), expected);
            assert_eq!(
                snapshot
                    .nodes
                    .iter()
                    .map(|node| node.sample_count)
                    .sum::<u64>(),
                samples.len() as u64
            );
            assert_eq!(snapshot.samples, samples);
            assert_eq!(snapshot.time_deltas_micros, deltas);
        }
    }

    #[test]
    fn cpu_profile_rejects_invalid_timestamp_offsets_and_mismatched_streams() {
        for deltas in [vec![-1], vec![10, -11], vec![i64::MAX, i64::MAX, 2]] {
            let mut snapshot = cpu_profile_snapshot(
                "test".into(),
                None,
                raw_cpu_profile(vec![2; deltas.len()], deltas),
            )
            .unwrap();
            let error = aggregate_cpu_profile(&mut snapshot).unwrap_err();
            assert!(error.to_string().contains("timestamp offset is outside"));
        }
        for (samples, deltas) in [(vec![2], vec![]), (vec![], vec![10])] {
            let mut snapshot = cpu_profile_snapshot("test".into(), None, raw_cpu_profile(samples, deltas))
                .unwrap();
            let error = aggregate_cpu_profile(&mut snapshot).unwrap_err();
            assert!(error.to_string().contains("samples but"));
        }

        let mut snapshot = cpu_profile_snapshot(
            "test".into(),
            None,
            raw_cpu_profile(vec![2, 99], vec![100, 0]),
        )
        .unwrap();
        let error = aggregate_cpu_profile(&mut snapshot).unwrap_err();
        assert!(
            error
                .to_string()
                .contains("sample references missing node 99")
        );
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
            script_provenance: BTreeMap::new(),
            projection_diagnostics: Vec::new(),
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
    fn heap_mapping_survives_serialization_and_keeps_frame_groups_separate() {
        let mapping = heap_mapping_fixture();
        let restored: HeapMappingSnapshot =
            serde_json::from_slice(&serde_json::to_vec(&mapping).unwrap()).unwrap();
        let groups = heap_mapping_groups();
        let snapshot =
            project_heap_classes("offline".into(), &groups, None, Some(&restored)).unwrap();
        assert_eq!(snapshot.analysis.mapping_status, HeapMappingStatus::Mapped);
        assert_eq!(snapshot.classes.len(), 2);
        assert_eq!(snapshot.classes[0].name, "Original");
        assert_eq!(snapshot.classes[0].location.line, 1);
        assert_eq!(
            snapshot.classes[0].source_url,
            "https://example.test/original.ts"
        );
        assert_ne!(
            snapshot.classes[0].provenance.frame_id,
            snapshot.classes[1].provenance.frame_id
        );
        assert_eq!(snapshot.analysis.script_mappings[0].hash, "captured-hash");
        assert_eq!(restored.connection_generation, 42);
    }

    #[test]
    fn heap_mapping_reports_legacy_absent_and_invalid_maps() {
        let groups = heap_mapping_groups();
        let legacy = project_heap_classes("old".into(), &groups, None, None).unwrap();
        assert_eq!(
            legacy.analysis.mapping_status,
            HeapMappingStatus::NotAttempted
        );
        assert!(
            legacy
                .analysis
                .script_mappings
                .iter()
                .all(|script| script.diagnostic.is_some())
        );
        let mut mapping = heap_mapping_fixture();
        for script in &mut mapping.scripts {
            script.source_map = None;
            script.mapping_status = HeapMappingStatus::NoMapSupplied;
        }

        let absent = project_heap_classes("absent".into(), &groups, None, Some(&mapping)).unwrap();
        assert_eq!(
            absent.analysis.mapping_status,
            HeapMappingStatus::NoMapSupplied
        );
        assert_eq!(absent.classes[0].source_url, "https://example.test/app.js");
        mapping.scripts[0].source_map = Some("{broken".into());
        let invalid =
            project_heap_classes("invalid".into(), &groups, None, Some(&mapping)).unwrap();
        assert_eq!(
            invalid.analysis.script_mappings[0].status,
            HeapMappingStatus::MapLoadingFailed
        );
        assert!(
            invalid.analysis.script_mappings[0]
                .diagnostic
                .as_ref()
                .unwrap()
                .contains("invalid source map")
        );
    }

    #[test]
    fn heap_capture_records_only_cheap_script_provenance() {
        let key = ScriptKey {
            session: SessionKey {
                connection_generation: 7,
                session_id: "session".into(),
            },
            script_id: "42".into(),
        };
        let mut script = ScriptState {
            url: "https://example.test/bundle.js".into(),
            hash: "hash".into(),
            source_map_url: Some("bundle.js.map".into()),
            version: 1,
            source: ScriptSourceState::Unresolved,
            provenance: Default::default(),
            captured_source: Some(crate::debugger_engine::CapturedScriptSource {
                content: Arc::from("large generated source"),
                source_map: Some(crate::source_view::SourceMapData::new(
                    br#"{"version":3,"sources":[],"mappings":""}"#.as_slice(),
                )),
                source_map_url: Some("bundle.js.map".into()),
                source_map_error: None,
            }),
        };
        let captured = captured_heap_script(&key, &script);
        assert_eq!(captured.script_id, "42");
        assert_eq!(captured.url, script.url);
        assert_eq!(captured.hash, script.hash);
        assert_eq!(captured.source_map_url.as_deref(), Some("bundle.js.map"));
        assert!(captured.generated_source.is_none());
        assert!(captured.source_map.is_none());
        assert_eq!(captured.mapping_status, HeapMappingStatus::NotAttempted);
        let small_payload = serde_json::to_vec(&captured).unwrap();
        let source_marker = "generated-content-must-not-be-persisted";
        let map_marker = "map-content-must-not-be-persisted";
        let large_source = source_marker.repeat(32_768);
        let large_map = map_marker.repeat(32_768);
        let source = script.captured_source.as_mut().unwrap();
        source.content = Arc::from(large_source);
        source.source_map = Some(crate::source_view::SourceMapData::new(large_map.into_bytes()));
        let large_payload = serde_json::to_vec(&captured_heap_script(&key, &script)).unwrap();
        assert_eq!(large_payload, small_payload);
        assert!(large_payload.len() < 1024);
        assert!(!large_payload.windows(source_marker.len()).any(|part| part == source_marker.as_bytes()));
        assert!(!large_payload.windows(map_marker.len()).any(|part| part == map_marker.as_bytes()));
        let mut inline = script.clone();
        inline.url = format!("data:text/javascript,{}", source_marker.repeat(32_768));
        inline.source_map_url = Some(format!("data:application/json,{}", map_marker.repeat(32_768)));
        let inline_metadata = captured_heap_script(&key, &inline);
        let serialized_inline = serde_json::to_vec(&inline_metadata).unwrap();
        assert!(serialized_inline.len() < 1024);
        assert_eq!(inline_metadata.url, "script:42");
        assert!(inline_metadata.source_map_url.is_none());
        assert_eq!(inline_metadata.mapping_status, HeapMappingStatus::NotAttempted);
        assert!(!serialized_inline.windows(source_marker.len())
            .any(|part| part == source_marker.as_bytes()));
        assert!(!serialized_inline.windows(map_marker.len())
            .any(|part| part == map_marker.as_bytes()));
        assert!(inline_metadata.diagnostic.as_deref().unwrap().contains("omitted"));
        let mut supplied = HeapMappingSnapshot {
            connection_generation: 7,
            hydration_duration_micros: 0,
            scripts: vec![inline_metadata.clone()],
        };
        supply_heap_source_map(&mut supplied, HeapSourceMapSupply {
            script_id: "42".into(),
            script_hash: "hash".into(),
            source_map_url: "file:///maps/app.js.map".into(),
            source_map: r#"{"version":3,"file":"app.js","sources":["original.ts"],"names":[],"mappings":"AAAA"}"#.into(),
        }).unwrap();
        assert_eq!(supplied.scripts[0].mapping_status, HeapMappingStatus::Mapped);
        inline.url = format!("https://example.test/app.js?source={}", source_marker.repeat(32_768));
        inline.source_map_url =
            Some(format!("https://example.test/app.js.map?map={}", map_marker.repeat(32_768)));
        let oversized_metadata = captured_heap_script(&key, &inline);
        assert!(serde_json::to_vec(&oversized_metadata).unwrap().len() < 1024);
        assert!(oversized_metadata.diagnostic.as_deref().unwrap().contains("omitted"));
        let mapping = HeapMappingSnapshot {
            connection_generation: 7,
            hydration_duration_micros: 0,
            scripts: vec![captured],
        };
        let serialized_mapping = serde_json::to_vec(&mapping).unwrap();
        assert!(serialized_mapping.len() < 1024);
        assert!(!serialized_mapping.windows(source_marker.len())
            .any(|part| part == source_marker.as_bytes()));
        assert!(!serialized_mapping.windows(map_marker.len())
            .any(|part| part == map_marker.as_bytes()));
        let viewed = project_heap_classes(
            "cheap".into(),
            &[HeapConstructorGroup {
                script_id: 42,
                line: 0,
                column: 0,
                generated_name: "a".into(),
                instance_count: 1,
                shallow_size: 16,
                instances: Vec::new(),
            }],
            None,
            Some(&mapping),
        )
        .unwrap();
        assert_eq!(viewed.analysis.mapping_status, HeapMappingStatus::NotAttempted);
        assert_eq!(viewed.analysis.script_mappings[0].status, HeapMappingStatus::NotAttempted);
        assert!(viewed.analysis.script_mappings[0]
            .diagnostic.as_deref().unwrap().contains("unavailable"));
    }

    #[test]
    fn heap_maps_without_sources_content_still_project_locations() {
        let mut mapping = heap_mapping_fixture();
        for script in &mut mapping.scripts {
            let mut map: serde_json::Value =
                serde_json::from_str(script.source_map.as_ref().unwrap()).unwrap();
            map.as_object_mut().unwrap().remove("sourcesContent");
            script.source_map = Some(map.to_string());
        }
        let snapshot = project_heap_classes(
            "without-content".into(),
            &heap_mapping_groups(),
            None,
            Some(&mapping),
        )
        .unwrap();
        assert_eq!(snapshot.analysis.mapping_status, HeapMappingStatus::Mapped);
        assert_eq!(
            snapshot.classes[0].source_url,
            "https://example.test/original.ts"
        );
        assert_eq!(snapshot.classes[0].name, "a");
    }

    #[test]
    fn heap_supplied_maps_require_captured_hash_and_matching_file() {
        let mut mapping = heap_mapping_fixture();
        let original = mapping.clone();
        let mut supply = HeapSourceMapSupply {
            script_id: "7".into(),
            script_hash: "wrong-hash".into(),
            source_map_url: "file:///maps/app.js.map".into(),
            source_map: mapping.scripts[0].source_map.clone().unwrap(),
        };
        assert!(
            supply_heap_source_map(&mut mapping, supply.clone())
                .unwrap_err()
                .to_string()
                .contains("hash")
        );
        assert_eq!(mapping, original);
        supply.script_hash = "captured-hash".into();
        supply.source_map = supply.source_map.replace("\"app.js\"", "\"other.js\"");
        assert!(
            supply_heap_source_map(&mut mapping, supply.clone())
                .unwrap_err()
                .to_string()
                .contains("file")
        );
        assert_eq!(mapping, original);
        supply.source_map = "{invalid".into();
        assert!(supply_heap_source_map(&mut mapping, supply.clone()).is_err());
        assert_eq!(mapping, original);
        supply.source_map = original.scripts[0].source_map.clone().unwrap();
        supply_heap_source_map(&mut mapping, supply).unwrap();
        assert_eq!(
            mapping.scripts[0].source_map_url.as_deref(),
            Some("file:///maps/app.js.map")
        );
        assert_eq!(mapping.scripts[0].hash, "captured-hash");
    }

    fn heap_mapping_fixture() -> HeapMappingSnapshot {
        HeapMappingSnapshot {
            connection_generation: 42, hydration_duration_micros: 12,
            scripts: ["7", "8"].into_iter().map(|id| HeapScriptSnapshot {
                script_id: id.into(), url: "https://example.test/app.js".into(),
                hash: "captured-hash".into(),
                provenance: ScriptProvenance {
                    execution_context_id: Some(id.parse().unwrap()),
                    execution_context_aux_data: Some(serde_json::json!({"frameId": format!("frame-{id}"), "isDefault": true})),
                    frame_id: Some(format!("frame-{id}")),
                },
                generated_source: Some("class a {}".into()),
                source_map_url: Some("https://example.test/app.js.map".into()),
                source_map: Some(r#"{"version":3,"file":"app.js","sources":["original.ts"],"sourcesContent":["class Original { constructor() {} }"],"names":[],"mappings":"AAAA"}"#.into()),
                mapping_status: HeapMappingStatus::Mapped, diagnostic: None,
            }).collect(),
        }
    }

    fn heap_mapping_groups() -> Vec<HeapConstructorGroup> {
        [7, 8]
            .into_iter()
            .map(|script_id| HeapConstructorGroup {
                script_id,
                generated_name: "a".into(),
                line: 0,
                column: 0,
                instance_count: 1,
                shallow_size: 16,
                instances: vec![crate::heap_snapshot::HeapInstanceRecord {
                    heap_object_id: script_id as u64,
                    shallow_size: 16,
                }],
            })
            .collect()
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
