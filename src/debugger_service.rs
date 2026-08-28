use std::collections::{BTreeMap, BTreeSet, VecDeque};
use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, OnceLock};
use std::time::Duration;

use atomic_write_file::AtomicWriteFile;
use futures_util::{StreamExt, stream};
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
use crate::context_identity::{
    ContextKind, compare_context_paths, normalize_absolute_path, path_relation,
    synthetic_node_target_id,
};
use crate::context_source_model::{
    CompactedProjectionKind, ContextSourceGraphSnapshot, ContextSourceModel,
};
use crate::debugger_engine::{SessionKey, StepKind};
use crate::service_api::{
    BreakpointPendingReason, BreakpointSnapshot, BreakpointSpec, BreakpointStatus,
    CanonicalTargetSnapshot, CaptureKind, CaptureSnapshot, CompactedSourceEdgeSnapshot,
    CompactedSourceGraphSnapshot, CompactedSourceNodeSnapshot, ConnectionConfiguration,
    ConnectionSnapshot, ConnectionStatus, ContextEventSnapshot, ContextObservation,
    ContextSnapshot, ContextSummary, CoverageSnapshot, CpuProfileSnapshot, DebuggerServiceApi,
    EvaluationSnapshot, HeapAggregateBy, HeapAggregateSnapshot, HeapCaptureResult,
    HeapClassSnapshot, HeapDiffSnapshot, HeapDominatorSnapshot, HeapEdgePolicy,
    HeapNodeSelectionSnapshot, HeapNodeSelector, HeapPathOptions, HeapPathSnapshot,
    HeapReferenceDirection, HeapReferencesSnapshot, HeapSnapshotProgress, HeapSnapshotResult,
    LogpointSpec, MutationOptions, ObservationCursor, ObservationResult, PlaywrightProxyEndpoint,
    ProcessTreeSnapshot, PromiseSelectionSnapshot, PromiseState, ScreenshotSnapshot, ServiceInfo,
    SourceContentSnapshot, SourceDisplayOptions, SourceGraphViewSnapshot, SourceMappingSnapshot,
    SourceMatchSnapshot, SourceSearchOptions, SourceSearchSnapshot, SourceSnapshotInfo,
    SourceSuffixRewriteSnapshot, SourceTreeKind, SourceTreeSnapshot, StepKind as ApiStepKind,
    TargetAttachOptions, TargetAttachmentOutcome, TargetAttachmentResult, TargetDebuggerSnapshot,
    TargetSnapshot, TargetWaitPredicate, UncompactedProjectionSnapshot,
    UncompactedSourceEdgeSnapshot, UncompactedSourceGraphSnapshot, UncompactedSourceNodeSnapshot,
    UncompactedSourceRevisionSnapshot, ValueInspectionOptions, ValueSelector, ValueSnapshot,
    VariableSnapshot,
};
use crate::source_graph::{IdentityBasis, ProjectionKind, SourceRevision};
use crate::source_search::{
    SearchControl, SearchDocument, SearchError, SearchQuery, SourceIdentity,
};
use crate::target_debugger::{
    TargetBreakpointSpec, TargetDebuggerError, TargetDebuggerHandle, stored_heap_classes,
};

#[derive(Clone)]
pub struct DebuggerService {
    agent_instance_id: String,
    state: Arc<Mutex<ServiceState>>,
    attachment_lock: Arc<Mutex<()>>,
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
        scavenge_heap_capture_storage(&persistence_path, &state);
        let (revision_signal, _) = watch::channel(0);
        Ok(Self {
            agent_instance_id: random_instance_id()?,
            state: Arc::new(Mutex::new(state)),
            attachment_lock: Arc::new(Mutex::new(())),
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
                cancel_playwright_proxies(
                    &mut state,
                    &context_id,
                    &connection_id,
                    Some(generation),
                );
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
                    match reduce_context(&context, ContextInput::RuntimeObservation(observation)) {
                        Ok(transition) => transition,
                        Err(error @ ContextTransitionError::TargetIdentityCollision { .. }) => {
                            eprintln!("rejecting target discovery: {error}");
                            drop(state);
                            runtime.close().await;
                            break;
                        }
                        Err(error) => {
                            eprintln!("rejecting target discovery: {error}");
                            continue;
                        }
                    };
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
                    cancel_playwright_proxies_for_target(
                        &mut state,
                        &context_id,
                        &connection_id,
                        &target_id,
                    );
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
                        let target = canonicalize_synthetic_target(target, &connection_id);
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
                            target_id: canonicalize_synthetic_target_id(&target_id, &connection_id),
                        },
                        None,
                        Some(canonicalize_synthetic_target_id(&target_id, &connection_id)),
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
                    match reduce_context(&context, ContextInput::RuntimeObservation(observation)) {
                        Ok(transition) => transition,
                        Err(error @ ContextTransitionError::TargetIdentityCollision { .. }) => {
                            eprintln!("rejecting provider target discovery: {error}");
                            drop(state);
                            runtime.close().await;
                            break;
                        }
                        Err(error) => {
                            eprintln!("rejecting provider target discovery: {error}");
                            continue;
                        }
                    };
                service.commit_context(&mut state, &context_id, transition);
                if let Some(target_id) = target_to_remove {
                    cancel_playwright_proxies_for_target(
                        &mut state,
                        &context_id,
                        &connection_id,
                        &target_id,
                    );
                    state.target_debuggers.remove(&(
                        context_id.clone(),
                        connection_id.clone(),
                        target_id,
                    ));
                }
                drop(state);

                if !runtime.is_direct_debugger()
                    && let Some(target_id) = target_to_attach
                    && service
                        .attach_target(
                            &CallCtx::default(),
                            context_id.clone(),
                            connection_id.clone(),
                            target_id.clone(),
                            TargetAttachOptions::default(),
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
    source_models: BTreeMap<String, Arc<ContextSourceModel>>,
    context_kinds: BTreeMap<String, ContextKind>,
    runtimes: BTreeMap<(String, String), Arc<ConnectionRuntime>>,
    target_debuggers: BTreeMap<(String, String, String), TargetDebuggerHandle>,
    history: BTreeMap<String, VecDeque<ContextObservation>>,
    completed_requests: BTreeMap<(String, String), u64>,
    playwright_proxies: BTreeMap<String, PlaywrightProxyRegistration>,
    captures: BTreeMap<(String, String), StoredCapture>,
    capture_reservations: BTreeMap<(String, String), CaptureReservation>,
}

#[derive(Clone, Debug)]
struct CaptureReservation {
    metadata: CaptureSnapshot,
}

struct CaptureReservationGuard {
    service: DebuggerService,
    reservation: CaptureReservation,
}

impl CaptureReservationGuard {
    fn new(service: DebuggerService, reservation: CaptureReservation) -> Self {
        Self {
            service,
            reservation,
        }
    }
}

impl Drop for CaptureReservationGuard {
    fn drop(&mut self) {
        let service = self.service.clone();
        let reservation = self.reservation.clone();
        if let Ok(runtime) = tokio::runtime::Handle::try_current() {
            runtime.spawn(async move {
                if service.abandon_capture(&reservation).await
                    && reservation.metadata.kind == CaptureKind::HeapSnapshot
                {
                    let (staging, final_path) = service.heap_capture_paths(&reservation);
                    remove_heap_files([staging, final_path]);
                }
            });
        }
    }
}

#[derive(Clone)]
struct PlaywrightProxyRegistration {
    context_id: String,
    connection_id: String,
    target_id: String,
    generation: u64,
    cancel: watch::Sender<bool>,
}

struct PhysicalAttachmentOwner {
    key: (String, String, String),
    debugger: TargetDebuggerHandle,
    runtime: Arc<ConnectionRuntime>,
}

fn physical_target_key(
    state: &ServiceState,
    key: &(String, String, String),
    runtime: &Arc<ConnectionRuntime>,
) -> Result<String, JsonRpcError> {
    let connection = state
        .contexts
        .get(&key.0)
        .ok_or_else(|| not_found("context", &key.0))?
        .connections
        .get(&key.1)
        .ok_or_else(|| not_found("connection", &key.1))?;
    let target = connection
        .targets
        .get(&key.2)
        .ok_or_else(|| not_found("target", &key.2))?;
    if let Some(process_id) = runtime.renderer_process_id(&key.2) {
        return Ok(format!("process:{process_id}"));
    }
    Ok(match &connection.configuration {
        ConnectionConfiguration::Process { process_id }
            if key.2 == synthetic_node_target_id(&key.1) =>
        {
            format!("process:{process_id}")
        }
        ConnectionConfiguration::ProcessTree { root_pid }
            if key.2 == synthetic_node_target_id(&key.1) =>
        {
            format!("process:{root_pid}")
        }
        ConnectionConfiguration::ProcessTree { .. } if target.url.starts_with("process:") => {
            target.url.clone()
        }
        ConnectionConfiguration::NodeInspector { endpoint } => {
            format!("node-inspector:{endpoint}")
        }
        ConnectionConfiguration::DirectCdp { endpoint } => {
            format!("cdp:{endpoint}#{}", key.2)
        }
        _ => format!("runtime:{:p}#{}", Arc::as_ptr(runtime), key.2),
    })
}

fn ownership_conflict(owner: &(String, String, String)) -> JsonRpcError {
    invalid_state(&format!(
        "target ownership conflict: physical target is already owned by {}/{}; target {}; retry with --force to steal it",
        owner.0, owner.1, owner.2
    ))
}

fn direct_attachment_error(message: String, force: bool) -> JsonRpcError {
    if message.contains("already attached by another debugger")
        || message.contains("already has a jsdbg client")
    {
        invalid_state(&format!(
            "target ownership conflict: {message}{}",
            if force {
                ""
            } else {
                "; retry with --force to steal it"
            }
        ))
    } else {
        internal_error(message)
    }
}

#[derive(Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
struct StoredCapture {
    metadata: CaptureSnapshot,
    payload: StoredCapturePayload,
}

#[derive(Clone, Serialize, Deserialize)]
#[serde(tag = "kind", content = "value", rename_all = "camelCase")]
enum StoredCapturePayload {
    Coverage(CoverageSnapshot),
    CpuProfile(CpuProfileSnapshot),
    HeapSnapshot { path: String },
}

impl StoredCapture {
    fn heap_path(&self) -> Option<PathBuf> {
        match &self.payload {
            StoredCapturePayload::HeapSnapshot { path } => Some(path.into()),
            StoredCapturePayload::Coverage(_) | StoredCapturePayload::CpuProfile(_) => None,
        }
    }
}

fn remove_heap_files(paths: impl IntoIterator<Item = PathBuf>) {
    for path in paths {
        if let Err(error) = fs::remove_file(&path)
            && error.kind() != std::io::ErrorKind::NotFound
        {
            eprintln!(
                "failed to remove deleted heap capture '{}': {error}",
                path.display()
            );
        }
    }
}

fn heap_capture_paths_for(
    persistence_path: &Path,
    reservation: &CaptureReservation,
) -> (PathBuf, PathBuf) {
    let directory = persistence_path.with_extension("captures").join(format!(
        "{:016x}",
        stable_name_hash(&reservation.metadata.context_id)
    ));
    let final_path = directory.join(format!("{}.heapsnapshot", reservation.metadata.storage_id));
    let staging_path = directory.join(format!("{}.partial", reservation.metadata.storage_id));
    (staging_path, final_path)
}

fn scavenge_heap_capture_storage(persistence_path: &Path, state: &ServiceState) {
    let capture_root = persistence_path.with_extension("captures");
    let mut referenced = state
        .captures
        .values()
        .filter_map(StoredCapture::heap_path)
        .map(|path| storage_path_key(&path))
        .collect::<BTreeSet<_>>();
    for reservation in state
        .capture_reservations
        .values()
        .filter(|reservation| reservation.metadata.kind == CaptureKind::HeapSnapshot)
    {
        let (staging, final_path) = heap_capture_paths_for(persistence_path, reservation);
        referenced.insert(storage_path_key(&staging));
        referenced.insert(storage_path_key(&final_path));
    }
    scavenge_heap_capture_directory(&capture_root, &referenced);
}

fn scavenge_heap_capture_directory(directory: &Path, referenced: &BTreeSet<PathBuf>) {
    match fs::symlink_metadata(directory) {
        Ok(metadata) if metadata.file_type().is_symlink() => return,
        Ok(_) => {}
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return,
        Err(error) => {
            eprintln!(
                "failed to inspect heap capture storage '{}': {error}",
                directory.display()
            );
            return;
        }
    }
    let entries = match fs::read_dir(directory) {
        Ok(entries) => entries,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return,
        Err(error) => {
            eprintln!(
                "failed to inspect heap capture storage '{}': {error}",
                directory.display()
            );
            return;
        }
    };
    for entry in entries {
        let entry = match entry {
            Ok(entry) => entry,
            Err(error) => {
                eprintln!(
                    "failed to inspect an entry in heap capture storage '{}': {error}",
                    directory.display()
                );
                continue;
            }
        };
        let path = entry.path();
        let file_type = match entry.file_type() {
            Ok(file_type) => file_type,
            Err(error) => {
                eprintln!(
                    "failed to inspect heap capture storage '{}': {error}",
                    path.display()
                );
                continue;
            }
        };
        if file_type.is_dir() {
            scavenge_heap_capture_directory(&path, referenced);
            continue;
        }
        let is_capture_storage = path
            .extension()
            .and_then(|extension| extension.to_str())
            .is_some_and(|extension| matches!(extension, "partial" | "heapsnapshot"));
        if is_capture_storage && !referenced.contains(&storage_path_key(&path)) {
            remove_heap_files([path]);
        }
    }
}

fn storage_path_key(path: &Path) -> PathBuf {
    let absolute = if path.is_absolute() {
        path.to_owned()
    } else {
        std::env::current_dir()
            .map(|current| current.join(path))
            .unwrap_or_else(|_| path.to_owned())
    };
    absolute
        .components()
        .fold(PathBuf::new(), |mut path, part| {
            match part {
                std::path::Component::CurDir => {}
                std::path::Component::ParentDir => {
                    path.pop();
                }
                _ => path.push(part.as_os_str()),
            }
            path
        })
}

#[async_trait::async_trait]
impl DebuggerServiceApi for DebuggerService {
    async fn service_info(&self, _ctx: &CallCtx) -> Result<ServiceInfo, JsonRpcError> {
        Ok(ServiceInfo {
            process_id: std::process::id(),
            agent_instance_id: self.agent_instance_id.clone(),
        })
    }

    async fn discover_vscode_process_trees(
        &self,
        _ctx: &CallCtx,
    ) -> Result<Vec<ProcessTreeSnapshot>, JsonRpcError> {
        crate::process_discovery::discover_vscode_process_trees(false)
            .await
            .map_err(|error| internal_error(error.to_string()))
    }

    async fn list_contexts(
        &self,
        _ctx: &CallCtx,
        cwd: Option<String>,
    ) -> Result<Vec<ContextSummary>, JsonRpcError> {
        let state = self.state.lock().await;
        let mut contexts = state
            .contexts
            .iter()
            .map(|(id, context)| ContextSummary {
                agent_instance_id: self.agent_instance_id.clone(),
                id: id.clone(),
                kind: state
                    .context_kinds
                    .get(id)
                    .copied()
                    .unwrap_or(ContextKind::Named),
                path_distance: cwd.as_deref().and_then(|cwd| {
                    (state.context_kinds.get(id) == Some(&ContextKind::Path))
                        .then(|| path_relation(cwd, id))
                        .flatten()
                        .map(|relation| relation.distance)
                }),
                path_ancestor: cwd.as_deref().and_then(|cwd| {
                    (state.context_kinds.get(id) == Some(&ContextKind::Path))
                        .then(|| path_relation(cwd, id))
                        .flatten()
                        .map(|relation| relation.ancestor)
                }),
                display_name: context.display_name.clone(),
                revision: context.revision,
                connection_count: context.connections.len() as u32,
                breakpoint_count: context.breakpoints.len() as u32,
            })
            .collect::<Vec<_>>();
        contexts.sort_by(|left, right| {
            left.kind
                .cmp(&right.kind)
                .then_with(|| match (cwd.as_deref(), left.kind) {
                    (Some(cwd), ContextKind::Path) => {
                        compare_context_paths(cwd, &left.id, &right.id)
                    }
                    _ => left.id.cmp(&right.id),
                })
        });
        Ok(contexts)
    }

    async fn put_context(
        &self,
        _ctx: &CallCtx,
        context_id: String,
        kind: ContextKind,
        display_name: Option<String>,
    ) -> Result<ContextSnapshot, JsonRpcError> {
        validate_context_identity(&context_id, kind)?;
        let mut state = self.state.lock().await;
        if let Some(existing) = state.context_kinds.get(&context_id)
            && existing != &kind
        {
            return Err(invalid_state(&format!(
                "context '{context_id}' is already registered as {existing:?}"
            )));
        }
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
        state.context_kinds.insert(context_id.clone(), kind);
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
        let (runtimes, heap_paths, proxy_cancellations) = {
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
            let mut heap_paths = state
                .captures
                .iter()
                .filter(|((candidate_context, _), _)| candidate_context == &context_id)
                .filter_map(|(_, capture)| capture.heap_path())
                .collect::<Vec<_>>();
            for reservation in state
                .capture_reservations
                .iter()
                .filter(|((candidate_context, _), _)| candidate_context == &context_id)
                .map(|(_, reservation)| reservation)
                .filter(|reservation| reservation.metadata.kind == CaptureKind::HeapSnapshot)
            {
                let (staging, final_path) = self.heap_capture_paths(reservation);
                heap_paths.extend([staging, final_path]);
            }
            let previous = state.clone();
            if state.contexts.remove(&context_id).is_none() {
                return Err(not_found("context", &context_id));
            }
            state.context_kinds.remove(&context_id);
            self.complete_request(&mut state, &context_id, &options, 0);
            state.history.remove(&context_id);
            state.source_models.remove(&context_id);
            state
                .target_debuggers
                .retain(|(candidate_context, _, _), _| candidate_context != &context_id);
            let proxy_cancellations = state
                .playwright_proxies
                .values()
                .filter(|proxy| proxy.context_id == context_id)
                .map(|proxy| proxy.cancel.clone())
                .collect::<Vec<_>>();
            state
                .playwright_proxies
                .retain(|_, proxy| proxy.context_id != context_id);
            let runtime_keys = state
                .runtimes
                .keys()
                .filter(|(candidate_context, _)| candidate_context == &context_id)
                .cloned()
                .collect::<Vec<_>>();
            let runtimes = runtime_keys
                .into_iter()
                .filter_map(|key| state.runtimes.remove(&key))
                .collect::<Vec<_>>();
            state
                .captures
                .retain(|(candidate_context, _), _| candidate_context != &context_id);
            state
                .capture_reservations
                .retain(|(candidate_context, _), _| candidate_context != &context_id);
            self.persist_or_restore(&mut state, previous)?;
            (runtimes, heap_paths, proxy_cancellations)
        };
        for cancellation in proxy_cancellations {
            let _ = cancellation.send(true);
        }
        for runtime in runtimes {
            runtime.close().await;
        }
        remove_heap_files(heap_paths);
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

        let connected = connect_runtime(&configuration, &connection_id, attempt.generation).await;
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
                if matches!(
                    error,
                    ContextTransitionError::TargetIdentityCollision { .. }
                ) {
                    let failed = reduce_context(
                        &context,
                        ContextInput::EffectCompletion(EffectCompletion::ConnectionOpenFailed {
                            connection_id: connection_id.clone(),
                            attempt,
                            message: error.to_string(),
                        }),
                    )
                    .expect("the current connecting attempt can be failed");
                    self.commit_context(&mut state, &context_id, failed);
                }
                drop(state);
                if let Some(runtime) = runtime {
                    runtime.close().await;
                }
                return Err(transition_rpc_error(error));
            }
        };
        let result = snapshot(&self.agent_instance_id, &context_id, &transition.state);
        let auto_attach_targets = if runtime
            .as_ref()
            .is_some_and(|runtime| runtime.is_direct_debugger())
        {
            Vec::new()
        } else {
            transition
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
                .unwrap_or_default()
        };
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
                .attach_target(
                    _ctx,
                    context_id.clone(),
                    connection_id.clone(),
                    target_id,
                    TargetAttachOptions::default(),
                )
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
            cancel_playwright_proxies(
                &mut state,
                &context_id,
                &connection_id,
                Some(attempt.generation),
            );
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
        let mut live_debuggers = Vec::new();
        for ((candidate_context, connection_id, target_id), debugger) in &state.target_debuggers {
            if candidate_context != &context_id {
                continue;
            }
            live_debuggers.push((connection_id.clone(), target_id.clone(), debugger.clone()));
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
        drop(state);
        for (connection_id, target_id, debugger) in live_debuggers {
            for (source_path, kind) in debugger
                .resolved_source_paths()
                .await
                .map_err(target_debugger_rpc_error)?
            {
                if path
                    .as_ref()
                    .is_some_and(|path| !source_path.contains(path))
                {
                    continue;
                }
                sources
                    .entry((
                        source_path.clone(),
                        connection_id.clone(),
                        target_id.clone(),
                    ))
                    .or_insert(SourceSnapshotInfo {
                        path: source_path,
                        kind,
                        status: "resolved".to_owned(),
                        connection_id: Some(connection_id.clone()),
                        target_id: Some(target_id.clone()),
                        source_map_url: None,
                    });
            }
        }
        Ok(sources.into_values().collect())
    }

    async fn show_source_graph(
        &self,
        _ctx: &CallCtx,
        context_id: String,
    ) -> Result<CompactedSourceGraphSnapshot, JsonRpcError> {
        let model = {
            let state = self.state.lock().await;
            if !state.contexts.contains_key(&context_id) {
                return Err(not_found("context", &context_id));
            }
            state.source_models.get(&context_id).cloned()
        };
        let Some(model) = model else {
            return Ok(CompactedSourceGraphSnapshot {
                roots: Vec::new(),
                nodes: Vec::new(),
                edges: Vec::new(),
            });
        };
        let graph = model.compacted_graph();
        Ok(CompactedSourceGraphSnapshot {
            roots: graph.roots,
            nodes: graph
                .nodes
                .into_iter()
                .map(|node| {
                    let listed_source_paths = if node.sources.len() <= 10 {
                        node.sources
                            .into_iter()
                            .map(|source| {
                                source
                                    .relative_path_from(&node.prefix)
                                    .filter(|path| !path.is_empty())
                                    .unwrap_or_else(|| source.display())
                            })
                            .collect()
                    } else {
                        Vec::new()
                    };
                    CompactedSourceNodeSnapshot {
                        id: node.id,
                        prefix: node.prefix.display(),
                        source_count: u32::try_from(node.source_count).unwrap_or(u32::MAX),
                        snapshot_count: u32::try_from(node.snapshot_count).unwrap_or(u32::MAX),
                        listed_source_paths,
                        runtime_internal: node.runtime_internal,
                    }
                })
                .collect(),
            edges: graph
                .edges
                .into_iter()
                .map(|edge| CompactedSourceEdgeSnapshot {
                    derived: edge.derived,
                    basis: edge.basis,
                    kind: compacted_projection_label(&edge.kind),
                    mapping_count: u32::try_from(edge.mapping_count).unwrap_or(u32::MAX),
                    fan_out: edge.fan_out,
                    suffix_rewrite: edge.suffix_rewrite.map(|rewrite| {
                        SourceSuffixRewriteSnapshot {
                            from: rewrite.from,
                            to: rewrite.to,
                        }
                    }),
                })
                .collect(),
        })
    }

    async fn show_uncompacted_source_graph(
        &self,
        _ctx: &CallCtx,
        context_id: String,
    ) -> Result<UncompactedSourceGraphSnapshot, JsonRpcError> {
        let model = {
            let state = self.state.lock().await;
            if !state.contexts.contains_key(&context_id) {
                return Err(not_found("context", &context_id));
            }
            state.source_models.get(&context_id).cloned()
        };
        let Some(model) = model else {
            return Ok(UncompactedSourceGraphSnapshot {
                roots: Vec::new(),
                nodes: Vec::new(),
                edges: Vec::new(),
            });
        };
        let graph = model.graph_snapshot();
        let referenced = graph
            .projections
            .iter()
            .map(|projection| projection.basis)
            .collect::<BTreeSet<_>>();
        let mut roots = graph
            .sources
            .iter()
            .map(|source| source.id)
            .filter(|source| !referenced.contains(source))
            .map(|source| source.0)
            .collect::<Vec<_>>();
        if roots.is_empty() {
            roots.extend(graph.sources.iter().map(|source| source.id.0));
        }
        Ok(uncompacted_graph_snapshot(graph, roots))
    }

    async fn show_source_tree(
        &self,
        _ctx: &CallCtx,
        context_id: String,
        kind: SourceTreeKind,
    ) -> Result<SourceTreeSnapshot, JsonRpcError> {
        let model = {
            let state = self.state.lock().await;
            if !state.contexts.contains_key(&context_id) {
                return Err(not_found("context", &context_id));
            }
            state.source_models.get(&context_id).cloned()
        };
        let sources = model.map_or_else(Vec::new, |model| match kind {
            SourceTreeKind::Loaded => model.loaded_sources(),
            SourceTreeKind::Resolved => model.resolved_loaded_sources(),
        });
        Ok(SourceTreeSnapshot {
            kind,
            sources: sources
                .into_iter()
                .map(uncompacted_source_node_snapshot)
                .collect(),
        })
    }

    async fn resolve_sources(
        &self,
        _ctx: &CallCtx,
        context_id: String,
        source: String,
    ) -> Result<UncompactedSourceGraphSnapshot, JsonRpcError> {
        let model = {
            let state = self.state.lock().await;
            if !state.contexts.contains_key(&context_id) {
                return Err(not_found("context", &context_id));
            }
            state.source_models.get(&context_id).cloned()
        };
        let Some(model) = model else {
            return Ok(UncompactedSourceGraphSnapshot {
                roots: Vec::new(),
                nodes: Vec::new(),
                edges: Vec::new(),
            });
        };
        let selection = model.resolve_sources(&source);
        Ok(uncompacted_graph_snapshot(
            selection.graph,
            selection.roots.into_iter().map(|source| source.0).collect(),
        ))
    }

    async fn show_source(
        &self,
        _ctx: &CallCtx,
        context_id: String,
        path: String,
        options: SourceDisplayOptions,
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
                return source_content_range(content, &options);
            }
        }
        if path.is_empty() {
            return Err(not_found("source", "<empty>"));
        }
        let file_path = source_file_path(&path)?;
        let content = fs::read_to_string(&file_path).map_err(|error| {
            internal_error(format!(
                "failed to read source '{}': {error}",
                file_path.display()
            ))
        })?;
        source_content_range(
            SourceContentSnapshot {
                path,
                total_lines: content.lines().count() as u32,
                start_line: 1,
                end_line: content.lines().count() as u32,
                content,
            },
            &options,
        )
    }

    async fn grep_sources(
        &self,
        _ctx: &CallCtx,
        context_id: String,
        options: SourceSearchOptions,
    ) -> Result<SourceSearchSnapshot, JsonRpcError> {
        if options.pattern.is_empty() {
            return Err(invalid_params("source grep pattern must not be empty"));
        }
        if options.max_results == 0 {
            return Err(invalid_params("source grep max_results must be positive"));
        }
        if options.timeout_ms == Some(0) {
            return Err(invalid_params("source grep timeout_ms must be positive"));
        }
        let query = SearchQuery {
            pattern: options.pattern.clone(),
            regex: options.regex,
            case_sensitive: options.case_sensitive,
            max_results: options.max_results as usize,
            context_lines: options.context_lines as usize,
        };
        crate::source_search::validate(&query).map_err(source_search_error)?;
        let deadline = options.timeout_ms.and_then(|milliseconds| {
            std::time::Instant::now().checked_add(Duration::from_millis(milliseconds))
        });
        let control = deadline.map_or_else(SearchControl::default, SearchControl::with_deadline);
        let mut cancellation = SearchCancellationGuard::new(control.cancellation_flag());
        let (debuggers, local_sources) = {
            let state = self.state.lock().await;
            let Some(context) = state.contexts.get(&context_id) else {
                return Err(not_found("context", &context_id));
            };
            let debuggers = state
                .target_debuggers
                .iter()
                .filter(|((candidate_context, _, _), _)| candidate_context == &context_id)
                .map(|((_, connection_id, target_id), debugger)| {
                    (connection_id.clone(), target_id.clone(), debugger.clone())
                })
                .collect::<Vec<_>>();
            let local_sources = context
                .breakpoints
                .values()
                .map(|breakpoint| breakpoint.source_path.clone())
                .filter(|path| {
                    options
                        .path
                        .as_ref()
                        .is_none_or(|selector| path.contains(selector))
                })
                .collect::<BTreeSet<_>>();
            (debuggers, local_sources)
        };

        let path_selector = options.path.clone();
        let batches = stream::iter(debuggers.into_iter().map(
            |(connection_id, target_id, debugger)| {
                let path_selector = path_selector.clone();
                let batch_control = control.clone();
                async move {
                    debugger
                        .source_search_batch(path_selector, batch_control)
                        .await
                        .map(|batch| (connection_id, target_id, batch))
                }
            },
        ))
        .buffer_unordered(8)
        .collect::<Vec<_>>();
        let batches = match deadline {
            Some(deadline) => timeout_at(Instant::from_std(deadline), batches)
                .await
                .map_err(|_| source_search_error(SearchError::DeadlineExceeded))?,
            None => batches.await,
        };
        let mut batches = batches
            .into_iter()
            .collect::<Result<Vec<_>, _>>()
            .map_err(target_source_search_rpc_error)?;
        batches.sort_by(|left, right| (&left.0, &left.1).cmp(&(&right.0, &right.1)));

        let mut documents = Vec::new();
        let mut skipped_sources = 0_u32;
        for (connection_id, target_id, batch) in batches {
            skipped_sources = skipped_sources.saturating_add(batch.skipped_sources);
            documents.extend(batch.sources.into_iter().map(|source| SearchDocument {
                identity: SourceIdentity {
                    path: source.path,
                    connection_id: Some(connection_id.clone()),
                    target_id: Some(target_id.clone()),
                    kind: source.kind,
                    provenance: source.provenance,
                },
                content_hash: source.content_hash,
                content: source.content,
            }));
        }

        let worker_control = control.clone();
        let worker = tokio::task::spawn_blocking(move || {
            let mut skipped_local = 0_u32;
            for path in local_sources {
                worker_control.check()?;
                let file_path = match source_file_path(&path) {
                    Ok(file_path) => file_path,
                    Err(_) => {
                        skipped_local = skipped_local.saturating_add(1);
                        continue;
                    }
                };
                let content = match fs::read_to_string(file_path) {
                    Ok(content) => Arc::<str>::from(content),
                    Err(_) => {
                        skipped_local = skipped_local.saturating_add(1);
                        continue;
                    }
                };
                documents.push(SearchDocument {
                    identity: SourceIdentity {
                        path,
                        connection_id: None,
                        target_id: None,
                        kind: "intent".to_owned(),
                        provenance: "local file".to_owned(),
                    },
                    content_hash: crate::content_store::ContentHash::try_of_bytes(
                        content.as_bytes(),
                        || worker_control.check(),
                    )?,
                    content,
                });
            }
            crate::source_search::search(documents, &query, &worker_control)
                .map(|result| (result, skipped_local))
        });
        let (result, skipped_local) = match deadline {
            Some(deadline) => timeout_at(Instant::from_std(deadline), worker)
                .await
                .map_err(|_| source_search_error(SearchError::DeadlineExceeded))?
                .map_err(|error| internal_error(format!("source search worker failed: {error}")))?
                .map_err(source_search_error)?,
            None => worker
                .await
                .map_err(|error| internal_error(format!("source search worker failed: {error}")))?
                .map_err(source_search_error)?,
        };
        cancellation.disarm();
        let matches = result
            .hits
            .into_iter()
            .map(|hit| SourceMatchSnapshot {
                path: hit.identity.path,
                content_hash: hit.content_hash.to_string(),
                kind: hit.identity.kind,
                provenance: hit.identity.provenance,
                connection_id: hit.identity.connection_id,
                target_id: hit.identity.target_id,
                line: hit.line,
                column: hit.column,
                match_length: hit.match_length,
                text: hit.text,
                before_context: hit.before_context,
                after_context: hit.after_context,
            })
            .collect::<Vec<_>>();
        Ok(SourceSearchSnapshot {
            omitted_matches: result.total_matches.saturating_sub(matches.len() as u64),
            matches,
            searched_sources: result.searched_sources,
            searched_contents: result.searched_contents,
            skipped_sources: skipped_sources.saturating_add(skipped_local),
        })
    }

    async fn explain_source(
        &self,
        _ctx: &CallCtx,
        context_id: String,
        path: String,
    ) -> Result<Vec<SourceGraphViewSnapshot>, JsonRpcError> {
        let debuggers = {
            let state = self.state.lock().await;
            if !state.contexts.contains_key(&context_id) {
                return Err(not_found("context", &context_id));
            }
            state
                .target_debuggers
                .iter()
                .filter(|((candidate_context, _, _), _)| candidate_context == &context_id)
                .map(|((_, connection_id, target_id), debugger)| {
                    (connection_id.clone(), target_id.clone(), debugger.clone())
                })
                .collect::<Vec<_>>()
        };
        let mut explanations = Vec::new();
        for (connection_id, target_id, debugger) in debuggers {
            let mut target_explanations = debugger
                .explain_source(path.clone())
                .await
                .map_err(target_debugger_rpc_error)?;
            for explanation in &mut target_explanations {
                explanation.connection_id = connection_id.clone();
                explanation.target_id = target_id.clone();
            }
            explanations.extend(target_explanations);
        }
        explanations.sort_by(|left, right| {
            (
                &left.connection_id,
                &left.target_id,
                &left.generated_url,
                &left.source_path,
            )
                .cmp(&(
                    &right.connection_id,
                    &right.target_id,
                    &right.generated_url,
                    &right.source_path,
                ))
        });
        Ok(explanations)
    }

    async fn map_source(
        &self,
        _ctx: &CallCtx,
        context_id: String,
        path: String,
        line: u32,
        column: u32,
    ) -> Result<Vec<SourceMappingSnapshot>, JsonRpcError> {
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
                .map(|((_, connection_id, target_id), debugger)| {
                    (connection_id.clone(), target_id.clone(), debugger.clone())
                })
                .collect::<Vec<_>>()
        };
        let mut locations = Vec::new();
        for (connection_id, target_id, debugger) in debuggers {
            let mut target_locations = debugger
                .map_source(path.clone(), line, column)
                .await
                .map_err(target_debugger_rpc_error)?;
            for location in &mut target_locations {
                location.connection_id = connection_id.clone();
                location.target_id = target_id.clone();
            }
            locations.extend(target_locations);
        }
        locations.sort_by(|left, right| {
            (
                &left.connection_id,
                &left.target_id,
                &left.source_url,
                left.line,
                left.column,
                &left.direction,
            )
                .cmp(&(
                    &right.connection_id,
                    &right.target_id,
                    &right.source_url,
                    right.line,
                    right.column,
                    &right.direction,
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
                .show_source(
                    ctx,
                    context_id.clone(),
                    source.path.clone(),
                    SourceDisplayOptions {
                        line: None,
                        context_lines: 0,
                    },
                )
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

    async fn resolve_target(
        &self,
        _ctx: &CallCtx,
        context_id: String,
        selector: String,
    ) -> Result<CanonicalTargetSnapshot, JsonRpcError> {
        let state = self.state.lock().await;
        Self::resolve_canonical_target(&state, &context_id, &selector)
    }

    async fn list_captures(
        &self,
        _ctx: &CallCtx,
        context_id: String,
    ) -> Result<Vec<CaptureSnapshot>, JsonRpcError> {
        let state = self.state.lock().await;
        if !state.contexts.contains_key(&context_id) {
            return Err(not_found("context", &context_id));
        }
        Ok(state
            .captures
            .range((context_id.clone(), String::new())..=(context_id, char::MAX.to_string()))
            .map(|(_, capture)| capture.metadata.clone())
            .collect())
    }

    async fn get_capture(
        &self,
        _ctx: &CallCtx,
        context_id: String,
        capture_name: String,
    ) -> Result<CaptureSnapshot, JsonRpcError> {
        let state = self.state.lock().await;
        state
            .captures
            .get(&(context_id, capture_name.clone()))
            .map(|capture| capture.metadata.clone())
            .ok_or_else(|| not_found("capture", &capture_name))
    }

    async fn delete_capture(
        &self,
        _ctx: &CallCtx,
        context_id: String,
        capture_name: String,
    ) -> Result<bool, JsonRpcError> {
        let (heap_path, debugger) = {
            let mut state = self.state.lock().await;
            if state
                .capture_reservations
                .contains_key(&(context_id.clone(), capture_name.clone()))
            {
                return Err(invalid_state(&format!(
                    "capture '{capture_name}' is currently being stored in context '{context_id}'"
                )));
            }
            let previous = state.clone();
            let capture = state
                .captures
                .remove(&(context_id, capture_name.clone()))
                .ok_or_else(|| not_found("capture", &capture_name))?;
            let heap_path = capture.heap_path();
            let debugger = state
                .target_debuggers
                .get(&(
                    capture.metadata.context_id.clone(),
                    capture.metadata.connection_id.clone(),
                    capture.metadata.target_id.clone(),
                ))
                .filter(|debugger| {
                    debugger.snapshot().connection_generation
                        == capture.metadata.connection_generation
                })
                .cloned();
            self.persist_or_restore(&mut state, previous)?;
            (heap_path, debugger)
        };
        remove_heap_files(heap_path);
        if let Some(debugger) = debugger {
            let _ = debugger.delete_stored_capture(capture_name).await;
        }
        Ok(true)
    }

    async fn get_stored_coverage(
        &self,
        _ctx: &CallCtx,
        context_id: String,
        capture_name: String,
        source_path: Option<String>,
    ) -> Result<CoverageSnapshot, JsonRpcError> {
        let state = self.state.lock().await;
        let capture = state
            .captures
            .get(&(context_id, capture_name.clone()))
            .ok_or_else(|| not_found("capture", &capture_name))?;
        let StoredCapturePayload::Coverage(mut snapshot) = capture.payload.clone() else {
            return Err(invalid_params(&format!(
                "capture '{capture_name}' is not a coverage capture"
            )));
        };
        if let Some(path) = source_path {
            snapshot.sources.retain(|source| {
                source.generated_url == path
                    || source.associated_authored_source.as_deref() == Some(path.as_str())
            });
        }
        Ok(snapshot)
    }

    async fn get_stored_cpu_profile(
        &self,
        _ctx: &CallCtx,
        context_id: String,
        capture_name: String,
        _source_path: Option<String>,
    ) -> Result<CpuProfileSnapshot, JsonRpcError> {
        let state = self.state.lock().await;
        let capture = state
            .captures
            .get(&(context_id, capture_name.clone()))
            .ok_or_else(|| not_found("capture", &capture_name))?;
        let StoredCapturePayload::CpuProfile(snapshot) = capture.payload.clone() else {
            return Err(invalid_params(&format!(
                "capture '{capture_name}' is not a CPU profile capture"
            )));
        };
        Ok(snapshot)
    }

    async fn get_stored_heap_classes(
        &self,
        _ctx: &CallCtx,
        context_id: String,
        capture_name: String,
        filter: Option<String>,
    ) -> Result<HeapClassSnapshot, JsonRpcError> {
        let path = {
            let state = self.state.lock().await;
            let capture = state
                .captures
                .get(&(context_id, capture_name.clone()))
                .ok_or_else(|| not_found("capture", &capture_name))?;
            let StoredCapturePayload::HeapSnapshot { path } = &capture.payload else {
                return Err(invalid_params(&format!(
                    "capture '{capture_name}' is not a heap snapshot"
                )));
            };
            path.clone()
        };
        let capture_for_task = capture_name.clone();
        tokio::task::spawn_blocking(move || {
            stored_heap_classes(Path::new(&path), capture_for_task, filter.as_deref())
        })
        .await
        .map_err(|error| internal_error(error.to_string()))?
        .map_err(target_debugger_rpc_error)
    }

    async fn attach_target(
        &self,
        ctx: &CallCtx,
        context_id: String,
        connection_id: String,
        target_id: String,
        options: TargetAttachOptions,
    ) -> Result<TargetAttachmentResult, JsonRpcError> {
        let _attachment_guard = self.attachment_lock.lock().await;
        let mut target_id = self
            .resolve_target_id(&context_id, &connection_id, &target_id)
            .await?;
        let mut debugger_key = (context_id.clone(), connection_id.clone(), target_id.clone());
        let prior_owner = {
            let mut state = self.state.lock().await;
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
            let physical_key = physical_target_key(&state, &debugger_key, &runtime)?;
            let prior_owner = state.target_debuggers.iter().find_map(|(key, debugger)| {
                let owner_runtime = state.runtimes.get(&(key.0.clone(), key.1.clone()))?.clone();
                (physical_target_key(&state, key, &owner_runtime)
                    .ok()
                    .as_ref()
                    == Some(&physical_key))
                .then(|| PhysicalAttachmentOwner {
                    key: key.clone(),
                    debugger: debugger.clone(),
                    runtime: owner_runtime,
                })
            });
            if let Some(owner) = &prior_owner {
                if !options.force {
                    return Err(ownership_conflict(&owner.key));
                }
                state.target_debuggers.remove(&owner.key);
            } else if !runtime.is_direct_debugger()
                && connection
                    .targets
                    .get(&target_id)
                    .is_some_and(|target| target.attached)
            {
                let message = format!(
                    "target ownership conflict: {connection_id}/{target_id} is already attached by an external debugger"
                );
                return Err(if options.force {
                    invalid_state(&format!(
                        "{message}; jsdbg cannot detach an unknown CDP session"
                    ))
                } else {
                    invalid_state(&format!(
                        "{message}; retry with --force to steal when supported"
                    ))
                });
            }
            prior_owner
        };

        let mut outcome = TargetAttachmentOutcome::Created;
        if let Some(owner) = prior_owner {
            outcome = TargetAttachmentOutcome::Stolen;
            let owner_is_requested = owner.key == debugger_key;
            if owner.runtime.is_direct_debugger()
                && owner.key.2 == synthetic_node_target_id(&owner.key.1)
            {
                self.disconnect_connection(ctx, owner.key.0.clone(), owner.key.1.clone())
                    .await?;
                if owner_is_requested {
                    self.connect_connection(ctx, context_id.clone(), connection_id.clone())
                        .await?;
                    target_id = self
                        .resolve_target_id(&context_id, &connection_id, &target_id)
                        .await?;
                    debugger_key = (context_id.clone(), connection_id.clone(), target_id.clone());
                }
            } else if owner.runtime.is_direct_debugger() {
                owner.runtime.close_direct_debugger(&owner.key.2).await;
            } else {
                detach_session(&owner.runtime, owner.debugger.session_id()).await;
            }
        }

        let (runtime, generation, waiting_for_debugger, source_model) = {
            let mut state = self.state.lock().await;
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
            let generation = connection.generation;
            let waiting_for_debugger = matches!(
                &connection.configuration,
                ConnectionConfiguration::Node { .. }
            );
            let runtime = state
                .runtimes
                .get(&(context_id.clone(), connection_id.clone()))
                .cloned()
                .ok_or_else(|| invalid_state("connection is not connected"))?;
            let source_model = state
                .source_models
                .entry(context_id.clone())
                .or_insert_with(|| Arc::new(ContextSourceModel::new()))
                .clone();
            (runtime, generation, waiting_for_debugger, source_model)
        };

        let direct_runtime_target_id = runtime.is_direct_debugger().then(|| {
            if target_id == synthetic_node_target_id(&connection_id) {
                "$node-root".to_owned()
            } else {
                target_id.clone()
            }
        });
        let (session, session_key) =
            if let Some(runtime_target_id) = direct_runtime_target_id.as_deref() {
                let attachment = runtime
                    .take_direct_debugger_session(runtime_target_id, options.force)
                    .await
                    .map_err(|error| direct_attachment_error(error.to_string(), options.force))?
                    .ok_or_else(|| invalid_state("direct debugger target has no endpoint"))?;
                if attachment.stole_external_owner {
                    outcome = TargetAttachmentOutcome::Stolen;
                }
                let key = attachment.session.key().clone();
                (attachment.session, key)
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
            waiting_for_debugger,
            source_model,
        )
        .await
        {
            Ok(debugger) => debugger,
            Err(error) => {
                discard_attached_session(
                    &runtime,
                    direct_runtime_target_id.as_deref(),
                    &session_key.session_id,
                )
                .await;
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
            discard_attached_session(
                &runtime,
                direct_runtime_target_id.as_deref(),
                &session_key.session_id,
            )
            .await;
            return Err(invalid_state(
                "connection changed while the target was being attached",
            ));
        }
        if let Some(existing) = state.target_debuggers.get(&debugger_key) {
            let owner = existing.snapshot();
            drop(state);
            discard_attached_session(
                &runtime,
                direct_runtime_target_id.as_deref(),
                &session_key.session_id,
            )
            .await;
            return Err(ownership_conflict(&(
                owner.context_id,
                owner.connection_id,
                owner.target_id,
            )));
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
                discard_attached_session(
                    &runtime,
                    direct_runtime_target_id.as_deref(),
                    &session_key.session_id,
                )
                .await;
                return Err(target_debugger_rpc_error(error));
            }
        }
        let snapshot = debugger.settle(Duration::from_millis(200)).await;
        for breakpoint_id in breakpoint_ids {
            self.publish_breakpoint_application(&context_id, &breakpoint_id)
                .await;
        }
        Ok(TargetAttachmentResult {
            outcome,
            target: snapshot,
        })
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

    async fn observe_target(
        &self,
        _ctx: &CallCtx,
        context_id: String,
        connection_id: String,
        target_id: String,
        after_revision: u64,
        timeout_ms: u64,
    ) -> Result<Option<TargetDebuggerSnapshot>, JsonRpcError> {
        match self
            .target_debugger(&context_id, &connection_id, &target_id)
            .await?
            .wait(
                TargetWaitPredicate::Changed { after_revision },
                Duration::from_millis(timeout_ms),
            )
            .await
        {
            Ok(snapshot) => Ok(Some(snapshot)),
            Err(TargetDebuggerError::WaitTimedOut) => Ok(None),
            Err(error) => Err(target_debugger_rpc_error(error)),
        }
    }

    async fn release_target(
        &self,
        _ctx: &CallCtx,
        context_id: String,
        connection_id: String,
        target_id: String,
    ) -> Result<TargetDebuggerSnapshot, JsonRpcError> {
        self.target_debugger(&context_id, &connection_id, &target_id)
            .await?
            .release_if_waiting()
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

    async fn get_scope_variables(
        &self,
        _ctx: &CallCtx,
        context_id: String,
        connection_id: String,
        target_id: String,
        pause_epoch: u64,
        frame_index: u32,
        scope_index: u32,
    ) -> Result<Vec<VariableSnapshot>, JsonRpcError> {
        self.target_debugger(&context_id, &connection_id, &target_id)
            .await?
            .scope_variables(pause_epoch, frame_index, scope_index)
            .await
            .map_err(target_debugger_rpc_error)
    }

    async fn get_object_properties(
        &self,
        _ctx: &CallCtx,
        context_id: String,
        connection_id: String,
        target_id: String,
        pause_epoch: Option<u64>,
        object_id: String,
    ) -> Result<Vec<VariableSnapshot>, JsonRpcError> {
        self.target_debugger(&context_id, &connection_id, &target_id)
            .await?
            .object_properties(pause_epoch, object_id)
            .await
            .map_err(target_debugger_rpc_error)
    }

    async fn raw_cdp_request(
        &self,
        _ctx: &CallCtx,
        context_id: String,
        connection_id: String,
        target_id: String,
        method: String,
        params: serde_json::Value,
        validate: bool,
    ) -> Result<serde_json::Value, JsonRpcError> {
        if validate {
            validate_raw_cdp_params(&method, &params).map_err(|message| {
                invalid_params(&format!(
                    "invalid params for CDP method '{method}': {message}"
                ))
            })?;
        }

        let debugger = self
            .target_debugger(&context_id, &connection_id, &target_id)
            .await?;
        let identity = debugger.snapshot();
        let result = debugger.raw_cdp_request(method, params).await;
        let is_current = self
            .state
            .lock()
            .await
            .target_debuggers
            .get(&(
                identity.context_id.clone(),
                identity.connection_id.clone(),
                identity.target_id.clone(),
            ))
            .is_some_and(|current| {
                current.same_instance(&debugger)
                    && current.snapshot().connection_generation == identity.connection_generation
            });
        if !is_current {
            return Err(invalid_state(
                "target connection changed while the CDP request was in flight",
            ));
        }
        result
    }

    async fn open_playwright_proxy(
        &self,
        _ctx: &CallCtx,
        context_id: String,
        connection_id: String,
        target_id: String,
        expected_generation: u64,
    ) -> Result<PlaywrightProxyEndpoint, JsonRpcError> {
        let target_id = self
            .resolve_target_id(&context_id, &connection_id, &target_id)
            .await?;
        let (runtime, source, browser_context_id) = {
            let state = self.state.lock().await;
            let connection = state
                .contexts
                .get(&context_id)
                .ok_or_else(|| not_found("context", &context_id))?
                .connections
                .get(&connection_id)
                .ok_or_else(|| not_found("connection", &connection_id))?;
            if connection.generation != expected_generation {
                return Err(invalid_state(
                    "selected connection generation is stale; resolve the target again",
                ));
            }
            let target = connection
                .targets
                .get(&target_id)
                .ok_or_else(|| not_found("target", &target_id))?;
            if target.target_type != "page" {
                return Err(invalid_params(format!(
                    "Playwright requires a page target, but '{target_id}' has type '{}'",
                    target.target_type
                )));
            }
            let runtime = state
                .runtimes
                .get(&(context_id.clone(), connection_id.clone()))
                .cloned()
                .ok_or_else(|| invalid_state("connection is not connected"))?;
            let source = runtime
                .playwright_cdp_source()
                .map_err(|error| invalid_state(&error.to_string()))?;
            (runtime, source, target.browser_context_id.clone())
        };

        let id = random_instance_id().map_err(|error| internal_error(error.to_string()))?;
        let proxy = crate::playwright_proxy::start(
            source,
            crate::playwright_proxy::PlaywrightPageScope {
                target_id: target_id.clone(),
                browser_context_id,
            },
            id.clone(),
        )
        .await
        .map_err(|error| internal_error(error.to_string()))?;
        let crate::playwright_proxy::PlaywrightProxy {
            websocket_url,
            cancel,
            completion,
        } = proxy;
        {
            let mut state = self.state.lock().await;
            let is_current = state
                .runtimes
                .get(&(context_id.clone(), connection_id.clone()))
                .is_some_and(|current| Arc::ptr_eq(current, &runtime))
                && state
                    .contexts
                    .get(&context_id)
                    .and_then(|context| context.connections.get(&connection_id))
                    .is_some_and(|connection| {
                        connection.generation == expected_generation
                            && connection.targets.contains_key(&target_id)
                    });
            if !is_current {
                cancel.send_replace(true);
                return Err(invalid_state(
                    "selected target changed while the Playwright proxy was opening",
                ));
            }
            state.playwright_proxies.insert(
                id.clone(),
                PlaywrightProxyRegistration {
                    context_id: context_id.clone(),
                    connection_id: connection_id.clone(),
                    target_id: target_id.clone(),
                    generation: expected_generation,
                    cancel: cancel.clone(),
                },
            );
        }
        let service = self.clone();
        let cleanup_id = id.clone();
        tokio::spawn(async move {
            let _ = completion.await;
            service
                .state
                .lock()
                .await
                .playwright_proxies
                .remove(&cleanup_id);
        });
        Ok(PlaywrightProxyEndpoint {
            id,
            websocket_url,
            connection_generation: expected_generation,
        })
    }

    async fn close_playwright_proxy(
        &self,
        _ctx: &CallCtx,
        proxy_id: String,
    ) -> Result<bool, JsonRpcError> {
        let registration = self.state.lock().await.playwright_proxies.remove(&proxy_id);
        if let Some(registration) = registration {
            registration.cancel.send_replace(true);
            Ok(true)
        } else {
            Ok(false)
        }
    }

    async fn inspect_value(
        &self,
        _ctx: &CallCtx,
        context_id: String,
        connection_id: String,
        target_id: String,
        pause_epoch: Option<u64>,
        selector: ValueSelector,
        options: ValueInspectionOptions,
    ) -> Result<ValueSnapshot, JsonRpcError> {
        self.target_debugger(&context_id, &connection_id, &target_id)
            .await?
            .inspect_value(pause_epoch, selector, options)
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

    async fn capture_screenshot(
        &self,
        _ctx: &CallCtx,
        context_id: String,
        connection_id: String,
        target_id: String,
    ) -> Result<ScreenshotSnapshot, JsonRpcError> {
        self.target_debugger(&context_id, &connection_id, &target_id)
            .await?
            .capture_screenshot()
            .await
            .map_err(target_debugger_rpc_error)
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
        let debugger = self
            .target_debugger(&context_id, &connection_id, &target_id)
            .await?;
        let owner = debugger.snapshot();
        let reservation = if let Some(name) = capture_id.as_ref() {
            Some(CaptureReservationGuard::new(
                self.clone(),
                self.reserve_capture(
                    &context_id,
                    &owner.connection_id,
                    &owner.target_id,
                    owner.connection_generation,
                    name.clone(),
                    CaptureKind::Coverage,
                )
                .await?,
            ))
        } else {
            None
        };
        let snapshot = match debugger
            .take_coverage(capture_id.clone(), exclude_capture_id)
            .await
            .map_err(target_debugger_rpc_error)
        {
            Ok(snapshot) => snapshot,
            Err(error) => {
                if let Some(reservation) = &reservation {
                    self.abandon_capture(&reservation.reservation).await;
                }
                return Err(error);
            }
        };
        if let Some(reservation) = &reservation {
            self.store_capture(
                &reservation.reservation,
                StoredCapturePayload::Coverage(snapshot.clone()),
            )
            .await?;
        }
        Ok(snapshot)
    }

    async fn stop_coverage(
        &self,
        _ctx: &CallCtx,
        context_id: String,
        connection_id: String,
        target_id: String,
        exclude_capture_id: Option<String>,
    ) -> Result<CoverageSnapshot, JsonRpcError> {
        let debugger = self
            .target_debugger(&context_id, &connection_id, &target_id)
            .await?;
        let owner = debugger.snapshot();
        let reservation = CaptureReservationGuard::new(
            self.clone(),
            self.reserve_capture(
                &context_id,
                &owner.connection_id,
                &owner.target_id,
                owner.connection_generation,
                ".".to_owned(),
                CaptureKind::Coverage,
            )
            .await?,
        );
        let snapshot = match debugger
            .stop_coverage(exclude_capture_id)
            .await
            .map_err(target_debugger_rpc_error)
        {
            Ok(snapshot) => snapshot,
            Err(error) => {
                self.abandon_capture(&reservation.reservation).await;
                return Err(error);
            }
        };
        self.store_capture(
            &reservation.reservation,
            StoredCapturePayload::Coverage(snapshot.clone()),
        )
        .await?;
        Ok(snapshot)
    }

    async fn finish_coverage(
        &self,
        _ctx: &CallCtx,
        context_id: String,
        connection_id: String,
        target_id: String,
        exclude_capture_id: Option<String>,
    ) -> Result<bool, JsonRpcError> {
        self.stop_coverage(
            _ctx,
            context_id,
            connection_id,
            target_id,
            exclude_capture_id,
        )
        .await?;
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

    async fn start_cpu_profile(
        &self,
        _ctx: &CallCtx,
        context_id: String,
        connection_id: String,
        target_id: String,
        sampling_interval_micros: Option<u64>,
    ) -> Result<bool, JsonRpcError> {
        self.target_debugger(&context_id, &connection_id, &target_id)
            .await?
            .start_cpu_profile(sampling_interval_micros)
            .await
            .map_err(target_debugger_rpc_error)?;
        Ok(true)
    }

    async fn stop_cpu_profile(
        &self,
        _ctx: &CallCtx,
        context_id: String,
        connection_id: String,
        target_id: String,
        capture_id: Option<String>,
    ) -> Result<CpuProfileSnapshot, JsonRpcError> {
        let name = capture_id.clone().unwrap_or_else(|| ".".to_owned());
        let debugger = self
            .target_debugger(&context_id, &connection_id, &target_id)
            .await?;
        let owner = debugger.snapshot();
        let reservation = CaptureReservationGuard::new(
            self.clone(),
            self.reserve_capture(
                &context_id,
                &owner.connection_id,
                &owner.target_id,
                owner.connection_generation,
                name,
                CaptureKind::CpuProfile,
            )
            .await?,
        );
        let snapshot = match debugger
            .stop_cpu_profile(capture_id)
            .await
            .map_err(target_debugger_rpc_error)
        {
            Ok(snapshot) => snapshot,
            Err(error) => {
                self.abandon_capture(&reservation.reservation).await;
                return Err(error);
            }
        };
        self.store_capture(
            &reservation.reservation,
            StoredCapturePayload::CpuProfile(snapshot.clone()),
        )
        .await?;
        Ok(snapshot)
    }

    async fn get_cpu_profile(
        &self,
        _ctx: &CallCtx,
        context_id: String,
        connection_id: String,
        target_id: String,
        capture_id: String,
        source_path: Option<String>,
        no_cache: bool,
        project: bool,
    ) -> Result<CpuProfileSnapshot, JsonRpcError> {
        self.target_debugger(&context_id, &connection_id, &target_id)
            .await?
            .get_cpu_profile(capture_id, source_path, no_cache, project)
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
        let name = capture_id.clone().unwrap_or_else(|| ".".to_owned());
        let debugger = self
            .target_debugger(&context_id, &connection_id, &target_id)
            .await?;
        let owner = debugger.snapshot();
        let reservation = CaptureReservationGuard::new(
            self.clone(),
            self.reserve_capture(
                &context_id,
                &owner.connection_id,
                &owner.target_id,
                owner.connection_generation,
                name.clone(),
                CaptureKind::HeapSnapshot,
            )
            .await?,
        );
        let result = match debugger
            .capture_heap_snapshot(capture_id, capture_numeric_value, expose_internals)
            .await
            .map_err(target_debugger_rpc_error)
        {
            Ok(result) => result,
            Err(error) => {
                self.abandon_capture(&reservation.reservation).await;
                return Err(error);
            }
        };
        let (staging_path, final_path) = self.heap_capture_paths(&reservation.reservation);
        if let Err(error) = debugger
            .copy_heap_capture(name, staging_path.to_string_lossy().into_owned())
            .await
            .map_err(target_debugger_rpc_error)
        {
            self.abandon_capture(&reservation.reservation).await;
            remove_heap_files([staging_path]);
            return Err(error);
        }
        if let Err(error) = fs::rename(&staging_path, &final_path) {
            self.abandon_capture(&reservation.reservation).await;
            remove_heap_files([staging_path, final_path]);
            return Err(internal_error(format!(
                "failed to publish heap capture storage: {error}"
            )));
        }
        if let Err(error) = self
            .store_capture(
                &reservation.reservation,
                StoredCapturePayload::HeapSnapshot {
                    path: final_path.to_string_lossy().into_owned(),
                },
            )
            .await
        {
            remove_heap_files([final_path]);
            return Err(error);
        }
        Ok(result)
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

    async fn select_promises(
        &self,
        _ctx: &CallCtx,
        context_id: String,
        connection_id: String,
        target_id: String,
        capture_id: String,
        state: Option<PromiseState>,
        limit: u32,
        max_preview_length: u32,
    ) -> Result<PromiseSelectionSnapshot, JsonRpcError> {
        self.target_debugger(&context_id, &connection_id, &target_id)
            .await?
            .select_promises(capture_id, state, limit, max_preview_length)
            .await
            .map_err(target_debugger_rpc_error)
    }

    async fn select_heap_nodes(
        &self,
        _ctx: &CallCtx,
        context_id: String,
        connection_id: String,
        target_id: String,
        capture_id: String,
        selector: HeapNodeSelector,
        max_string_length: Option<u32>,
        include_dominators: bool,
    ) -> Result<HeapNodeSelectionSnapshot, JsonRpcError> {
        self.target_debugger(&context_id, &connection_id, &target_id)
            .await?
            .select_heap_nodes(capture_id, selector, max_string_length, include_dominators)
            .await
            .map_err(target_debugger_rpc_error)
    }

    async fn get_heap_references(
        &self,
        _ctx: &CallCtx,
        context_id: String,
        connection_id: String,
        target_id: String,
        reference: String,
        direction: HeapReferenceDirection,
        edge_policy: HeapEdgePolicy,
        limit: u32,
        max_string_length: Option<u32>,
    ) -> Result<HeapReferencesSnapshot, JsonRpcError> {
        self.target_debugger(&context_id, &connection_id, &target_id)
            .await?
            .get_heap_references(reference, direction, edge_policy, limit, max_string_length)
            .await
            .map_err(target_debugger_rpc_error)
    }

    async fn get_heap_path(
        &self,
        _ctx: &CallCtx,
        context_id: String,
        connection_id: String,
        target_id: String,
        from: String,
        to: String,
        options: HeapPathOptions,
        max_string_length: Option<u32>,
    ) -> Result<Option<HeapPathSnapshot>, JsonRpcError> {
        self.target_debugger(&context_id, &connection_id, &target_id)
            .await?
            .get_heap_path(from, to, options, max_string_length)
            .await
            .map_err(target_debugger_rpc_error)
    }

    async fn get_heap_dominator_chain(
        &self,
        _ctx: &CallCtx,
        context_id: String,
        connection_id: String,
        target_id: String,
        reference: String,
        max_string_length: Option<u32>,
    ) -> Result<HeapDominatorSnapshot, JsonRpcError> {
        self.target_debugger(&context_id, &connection_id, &target_id)
            .await?
            .get_heap_dominator_chain(reference, max_string_length)
            .await
            .map_err(target_debugger_rpc_error)
    }

    async fn aggregate_heap_snapshot(
        &self,
        _ctx: &CallCtx,
        context_id: String,
        connection_id: String,
        target_id: String,
        capture_id: String,
        by: HeapAggregateBy,
        limit: u32,
        max_string_length: Option<u32>,
    ) -> Result<HeapAggregateSnapshot, JsonRpcError> {
        self.target_debugger(&context_id, &connection_id, &target_id)
            .await?
            .aggregate_heap_snapshot(capture_id, by, limit, max_string_length)
            .await
            .map_err(target_debugger_rpc_error)
    }

    async fn diff_heap_snapshots(
        &self,
        _ctx: &CallCtx,
        context_id: String,
        connection_id: String,
        target_id: String,
        older_capture_id: String,
        newer_capture_id: String,
        by: HeapAggregateBy,
        limit: u32,
        max_string_length: Option<u32>,
    ) -> Result<HeapDiffSnapshot, JsonRpcError> {
        self.target_debugger(&context_id, &connection_id, &target_id)
            .await?
            .diff_heap_snapshots(
                older_capture_id,
                newer_capture_id,
                by,
                limit,
                max_string_length,
            )
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
                for registration in std::mem::take(&mut state.playwright_proxies).into_values() {
                    registration.cancel.send_replace(true);
                }
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
    async fn reserve_capture(
        &self,
        context_id: &str,
        connection_id: &str,
        target_id: &str,
        connection_generation: u64,
        name: String,
        kind: CaptureKind,
    ) -> Result<CaptureReservation, JsonRpcError> {
        validate_id("capture", &name)?;
        let mut state = self.state.lock().await;
        let key = (context_id.to_owned(), name.clone());
        if let Some(existing) = state.captures.get(&key) {
            return Err(invalid_state(&format!(
                "capture '{name}' already exists in context '{context_id}' as {:?} from target '{}' (connection '{}', generation {})",
                existing.metadata.kind,
                existing.metadata.target_id,
                existing.metadata.connection_id,
                existing.metadata.connection_generation,
            )));
        }
        if let Some(existing) = state.capture_reservations.get(&key) {
            return Err(invalid_state(&format!(
                "capture '{name}' is already being stored in context '{context_id}' as {:?} from target '{}' (connection '{}', generation {})",
                existing.metadata.kind,
                existing.metadata.target_id,
                existing.metadata.connection_id,
                existing.metadata.connection_generation,
            )));
        }
        let connection = state
            .contexts
            .get(context_id)
            .and_then(|context| context.connections.get(connection_id))
            .ok_or_else(|| invalid_state("capture owner connection no longer exists"))?;
        if connection.generation != connection_generation
            || !connection.targets.contains_key(target_id)
        {
            return Err(invalid_state(
                "connection generation changed while the capture was being stored",
            ));
        }
        let storage_id = loop {
            let candidate =
                random_instance_id().map_err(|error| internal_error(error.to_string()))?;
            let in_use = state
                .captures
                .values()
                .any(|capture| capture.metadata.storage_id == candidate)
                || state
                    .capture_reservations
                    .values()
                    .any(|reservation| reservation.metadata.storage_id == candidate);
            if !in_use {
                break candidate;
            }
        };
        let metadata = CaptureSnapshot {
            context_id: context_id.to_owned(),
            name,
            kind,
            target_id: target_id.to_owned(),
            connection_id: connection_id.to_owned(),
            connection_generation,
            storage_id,
        };
        let reservation = CaptureReservation { metadata };
        state.capture_reservations.insert(key, reservation.clone());
        Ok(reservation)
    }

    async fn finalize_capture(
        &self,
        reservation: &CaptureReservation,
        payload: StoredCapturePayload,
    ) -> Result<CaptureSnapshot, JsonRpcError> {
        let mut state = self.state.lock().await;
        let metadata = &reservation.metadata;
        let key = (metadata.context_id.clone(), metadata.name.clone());
        if state
            .capture_reservations
            .get(&key)
            .is_none_or(|current| current.metadata.storage_id != metadata.storage_id)
        {
            return Err(invalid_state("capture reservation is no longer current"));
        }
        let connection = state
            .contexts
            .get(&metadata.context_id)
            .and_then(|context| context.connections.get(&metadata.connection_id))
            .ok_or_else(|| invalid_state("capture owner connection no longer exists"))?;
        if connection.generation != metadata.connection_generation
            || !connection.targets.contains_key(&metadata.target_id)
        {
            return Err(invalid_state(
                "connection generation changed while the capture was being stored",
            ));
        }
        let previous = state.clone();
        state.capture_reservations.remove(&key);
        state.captures.insert(
            key,
            StoredCapture {
                metadata: metadata.clone(),
                payload,
            },
        );
        self.persist_or_restore(&mut state, previous)?;
        Ok(metadata.clone())
    }

    async fn abandon_capture(&self, reservation: &CaptureReservation) -> bool {
        let metadata = &reservation.metadata;
        let key = (metadata.context_id.clone(), metadata.name.clone());
        let mut state = self.state.lock().await;
        if state
            .capture_reservations
            .get(&key)
            .is_some_and(|current| current.metadata.storage_id == metadata.storage_id)
        {
            state.capture_reservations.remove(&key);
            true
        } else {
            false
        }
    }

    async fn store_capture(
        &self,
        reservation: &CaptureReservation,
        payload: StoredCapturePayload,
    ) -> Result<CaptureSnapshot, JsonRpcError> {
        let result = self.finalize_capture(reservation, payload).await;
        if result.is_err() {
            self.abandon_capture(reservation).await;
        }
        result
    }

    fn heap_capture_paths(&self, reservation: &CaptureReservation) -> (PathBuf, PathBuf) {
        heap_capture_paths_for(&self.persistence_path, reservation)
    }

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

    fn resolve_canonical_target(
        state: &ServiceState,
        context_id: &str,
        selector: &str,
    ) -> Result<CanonicalTargetSnapshot, JsonRpcError> {
        let context = state
            .contexts
            .get(context_id)
            .ok_or_else(|| not_found("context", context_id))?;
        let candidates = context
            .connections
            .iter()
            .flat_map(|(connection_id, connection)| {
                connection.targets.values().filter_map(move |target| {
                    let exact = target.target_id == selector;
                    let friendly = target.target_type.eq_ignore_ascii_case(selector)
                        || target.title.eq_ignore_ascii_case(selector)
                        || target.url == selector
                        || target
                            .title
                            .to_lowercase()
                            .contains(&selector.to_lowercase())
                        || target.url.to_lowercase().contains(&selector.to_lowercase());
                    (exact || friendly).then(|| {
                        (
                            exact,
                            CanonicalTargetSnapshot {
                                context_id: context_id.to_owned(),
                                target_id: target.target_id.clone(),
                                connection_id: connection_id.clone(),
                                connection_generation: connection.generation,
                                target: target.clone(),
                            },
                        )
                    })
                })
            })
            .collect::<Vec<_>>();
        let exact = candidates
            .iter()
            .filter(|(exact, _)| *exact)
            .map(|(_, target)| target.clone())
            .collect::<Vec<_>>();
        let matches = if exact.is_empty() {
            candidates
                .into_iter()
                .map(|(_, target)| target)
                .collect::<Vec<_>>()
        } else {
            exact
        };
        match matches.as_slice() {
            [target] => Ok(target.clone()),
            [] => Err(not_found("target selector", selector)),
            _ => {
                let details = matches
                    .iter()
                    .map(|candidate| {
                        format!(
                            "\n  {}/{}@{}  type={}  title={:?}  url={}",
                            candidate.connection_id,
                            candidate.target_id,
                            candidate.connection_generation,
                            candidate.target.target_type,
                            candidate.target.title,
                            candidate.target.url
                        )
                    })
                    .collect::<String>();
                Err(invalid_params(&format!(
                    "target selector '{selector}' is ambiguous across {} canonical targets in context '{context_id}'. Qualified candidates:{details}",
                    matches.len()
                )))
            }
        }
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

fn cancel_playwright_proxies(
    state: &mut ServiceState,
    context_id: &str,
    connection_id: &str,
    generation: Option<u64>,
) {
    state.playwright_proxies.retain(|_, registration| {
        let matches = registration.context_id == context_id
            && registration.connection_id == connection_id
            && generation.is_none_or(|generation| registration.generation == generation);
        if matches {
            registration.cancel.send_replace(true);
        }
        !matches
    });
}

fn cancel_playwright_proxies_for_target(
    state: &mut ServiceState,
    context_id: &str,
    connection_id: &str,
    target_id: &str,
) {
    state.playwright_proxies.retain(|_, registration| {
        let matches = registration.context_id == context_id
            && registration.connection_id == connection_id
            && registration.target_id == target_id;
        if matches {
            registration.cancel.send_replace(true);
        }
        !matches
    });
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

async fn discard_attached_session(
    runtime: &ConnectionRuntime,
    direct_target_id: Option<&str>,
    session_id: &str,
) {
    if let Some(target_id) = direct_target_id {
        runtime.close_direct_debugger(target_id).await;
    } else {
        detach_session(runtime, session_id).await;
    }
}

fn canonicalize_synthetic_target_id(target_id: &str, connection_id: &str) -> String {
    if target_id == "$node-root" {
        synthetic_node_target_id(connection_id)
    } else {
        target_id.to_owned()
    }
}

fn canonicalize_synthetic_target(
    mut target: TargetSnapshot,
    connection_id: &str,
) -> TargetSnapshot {
    target.target_id = canonicalize_synthetic_target_id(&target.target_id, connection_id);
    target.parent_id = target
        .parent_id
        .map(|id| canonicalize_synthetic_target_id(&id, connection_id));
    target.opener_id = target
        .opener_id
        .map(|id| canonicalize_synthetic_target_id(&id, connection_id));
    target
}

async fn connect_runtime(
    configuration: &ConnectionConfiguration,
    connection_id: &str,
    connection_generation: u64,
) -> Result<(Arc<ConnectionRuntime>, String, String, Vec<TargetSnapshot>), String> {
    let connection = ConnectionRuntime::connect(configuration, connection_generation)
        .await
        .map_err(|error| error.to_string())?;
    if connection.is_direct_debugger() {
        let title = match configuration {
            ConnectionConfiguration::Process { process_id } => {
                format!("Process {process_id}")
            }
            ConnectionConfiguration::ProcessTree { root_pid } => {
                format!("Process {root_pid}")
            }
            _ => "Node.js".to_owned(),
        };
        return Ok((
            connection,
            title.clone(),
            "1.3".to_owned(),
            vec![TargetSnapshot {
                target_id: synthetic_node_target_id(connection_id),
                target_type: "node".to_owned(),
                title,
                url: match configuration {
                    ConnectionConfiguration::Node { program, .. } => program.clone(),
                    ConnectionConfiguration::NodeInspector { endpoint } => endpoint.clone(),
                    ConnectionConfiguration::Process { process_id } => {
                        format!("process:{process_id}")
                    }
                    ConnectionConfiguration::ProcessTree { root_pid } => {
                        format!("process:{root_pid}")
                    }
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
    let connections = context
        .connections
        .iter()
        .map(|(id, connection)| ConnectionSnapshot {
            id: id.clone(),
            configuration: connection.configuration.clone(),
            generation: connection.generation,
            status: connection.status.clone(),
            targets: connection.targets.values().cloned().collect(),
        })
        .collect::<Vec<_>>();
    let target_forest = connections
        .iter()
        .flat_map(ConnectionSnapshot::target_forest)
        .collect();
    ContextSnapshot {
        agent_instance_id: agent_instance_id.to_owned(),
        id: id.to_owned(),
        display_name: context.display_name.clone(),
        revision: context.revision,
        connections,
        target_forest,
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
                pending_reason: breakpoint
                    .enabled
                    .then_some(BreakpointPendingReason::WaitingForTarget),
                enabled: breakpoint.enabled,
                condition: breakpoint.condition.clone(),
                target_selector: breakpoint.target_selector.clone(),
                targets: Vec::new(),
                applications: Vec::new(),
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
        breakpoint.targets = applications.clone();
        breakpoint.applications = applications
            .iter()
            .flat_map(|target| target.applications.iter().cloned())
            .collect();
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
        breakpoint.pending_reason = match breakpoint.status {
            BreakpointStatus::Bound { .. } | BreakpointStatus::Disabled => None,
            BreakpointStatus::Failed { ref message } => Some(BreakpointPendingReason::Failed {
                message: message.clone(),
            }),
            _ => Some(breakpoint_pending_reason(&applications)),
        };
    }
    Some(result)
}

fn breakpoint_pending_reason(
    applications: &[crate::service_api::TargetBreakpointSnapshot],
) -> BreakpointPendingReason {
    use crate::service_api::TargetBreakpointStatus;

    if applications.is_empty() {
        return BreakpointPendingReason::WaitingForTarget;
    }
    if let Some((candidates, omitted_candidate_count)) =
        applications
            .iter()
            .find_map(|application| match &application.status {
                TargetBreakpointStatus::AmbiguousSource {
                    candidates,
                    omitted_candidate_count,
                } => Some((candidates.clone(), *omitted_candidate_count)),
                _ => None,
            })
    {
        return BreakpointPendingReason::AmbiguousSource {
            candidates,
            omitted_candidate_count,
        };
    }
    if applications.iter().any(|application| {
        matches!(
            application.status,
            TargetBreakpointStatus::Installing { .. }
        )
    }) {
        return BreakpointPendingReason::Installing;
    }
    if applications.iter().any(|application| {
        matches!(
            application.status,
            TargetBreakpointStatus::Applicable { .. }
        )
    }) {
        return BreakpointPendingReason::Applicable;
    }
    let unmapped = applications
        .iter()
        .filter_map(|application| match &application.status {
            TargetBreakpointStatus::Unmapped { diagnostics } => Some(diagnostics.iter().cloned()),
            _ => None,
        })
        .flatten()
        .collect::<Vec<_>>();
    if !unmapped.is_empty() {
        return BreakpointPendingReason::Unmapped {
            diagnostics: unmapped,
        };
    }
    if applications
        .iter()
        .any(|application| matches!(application.status, TargetBreakpointStatus::WaitingForScript))
    {
        return BreakpointPendingReason::WaitingForScript;
    }
    let diagnostics = applications
        .iter()
        .filter_map(|application| match &application.status {
            TargetBreakpointStatus::SourceNotFound { diagnostics } => {
                Some(diagnostics.iter().cloned())
            }
            _ => None,
        })
        .flatten()
        .collect::<Vec<_>>();
    if !diagnostics.is_empty() {
        return BreakpointPendingReason::SourceNotFound { diagnostics };
    }
    applications
        .iter()
        .find_map(|application| match &application.status {
            TargetBreakpointStatus::Failed { message } => Some(BreakpointPendingReason::Failed {
                message: message.clone(),
            }),
            _ => None,
        })
        .unwrap_or(BreakpointPendingReason::WaitingForScript)
}

#[derive(Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
struct StoredServiceState {
    schema_version: u32,
    contexts: BTreeMap<String, StoredContextState>,
    #[serde(default)]
    completed_requests: Vec<StoredCompletedRequest>,
    #[serde(default)]
    captures: Vec<StoredCapture>,
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
    #[serde(default = "default_context_kind")]
    kind: ContextKind,
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
            schema_version: 4,
            contexts: state
                .contexts
                .iter()
                .map(|(id, context)| {
                    (
                        id.clone(),
                        StoredContextState {
                            kind: state
                                .context_kinds
                                .get(id)
                                .copied()
                                .unwrap_or(ContextKind::Named),
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
            captures: state.captures.values().cloned().collect(),
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
        2..=4 => serde_json::from_slice(&bytes)?,
        version => return Err(ServicePersistenceError::UnsupportedSchema(version as u32)),
    };
    let completed_requests = stored
        .completed_requests
        .into_iter()
        .map(|request| ((request.context_id, request.request_id), request.revision))
        .collect();
    let context_kinds = stored
        .contexts
        .iter()
        .map(|(id, context)| (id.clone(), context.kind))
        .collect();
    let mut captures = BTreeMap::new();
    for capture in stored.captures {
        let key = (
            capture.metadata.context_id.clone(),
            capture.metadata.name.clone(),
        );
        if captures.insert(key.clone(), capture).is_some() {
            return Err(ServicePersistenceError::DuplicateCapture {
                context_id: key.0,
                name: key.1,
            });
        }
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
        source_models: BTreeMap::new(),
        context_kinds,
        runtimes: BTreeMap::new(),
        target_debuggers: BTreeMap::new(),
        history: BTreeMap::new(),
        completed_requests,
        playwright_proxies: BTreeMap::new(),
        captures,
        capture_reservations: BTreeMap::new(),
    })
}

fn stored_default_true() -> bool {
    true
}

fn default_context_kind() -> ContextKind {
    ContextKind::Named
}

fn migrate_v1(stored: StoredServiceStateV1) -> StoredServiceState {
    StoredServiceState {
        schema_version: 4,
        contexts: stored
            .contexts
            .into_iter()
            .map(|(id, context)| {
                (
                    id,
                    StoredContextState {
                        kind: ContextKind::Named,
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
        captures: Vec::new(),
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
    #[error("debugger state contains duplicate capture '{name}' in context '{context_id}'")]
    DuplicateCapture { context_id: String, name: String },
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

fn validate_context_identity(id: &str, kind: ContextKind) -> Result<(), JsonRpcError> {
    match kind {
        ContextKind::Named => {
            validate_id("context", id)?;
            if id != id.to_ascii_lowercase() {
                return Err(invalid_params("named context id must be lowercase"));
            }
        }
        ContextKind::Path => {
            let normalized = normalize_absolute_path(Path::new(id))
                .map_err(|error| invalid_params(&error.to_string()))?;
            if normalized != id {
                return Err(invalid_params(
                    "path context id must be a lexically normalized lowercase absolute path",
                ));
            }
        }
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
    if Path::new(path).is_absolute() {
        return Ok(PathBuf::from(path));
    }

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

struct SearchCancellationGuard {
    flag: Arc<AtomicBool>,
    armed: bool,
}

impl SearchCancellationGuard {
    fn new(flag: Arc<AtomicBool>) -> Self {
        Self { flag, armed: true }
    }

    fn disarm(&mut self) {
        self.armed = false;
    }
}

impl Drop for SearchCancellationGuard {
    fn drop(&mut self) {
        if self.armed {
            self.flag.store(true, Ordering::Release);
        }
    }
}

fn source_search_error(error: SearchError) -> JsonRpcError {
    match error {
        SearchError::InvalidPattern(message) => {
            invalid_params(format!("invalid source regex: {message}"))
        }
        SearchError::Cancelled => {
            JsonRpcError::new(error_codes::REQUEST_TIMEOUT, "source search was cancelled")
        }
        SearchError::DeadlineExceeded => JsonRpcError::new(
            error_codes::REQUEST_TIMEOUT,
            "source search deadline exceeded",
        ),
        SearchError::Search(message) => internal_error(message),
    }
}

fn compacted_projection_label(kind: &CompactedProjectionKind) -> String {
    match kind {
        CompactedProjectionKind::Identity => "identity".to_owned(),
        CompactedProjectionKind::SourceMap => "source map".to_owned(),
        CompactedProjectionKind::Format(formatter) => format!("format ({formatter})"),
        CompactedProjectionKind::Edit(edit) => format!("edit ({edit})"),
        CompactedProjectionKind::Offset {
            line_delta,
            column_delta,
        } => format!("offset ({line_delta:+} lines, {column_delta:+} columns)"),
    }
}

fn uncompacted_graph_snapshot(
    graph: ContextSourceGraphSnapshot,
    roots: Vec<u64>,
) -> UncompactedSourceGraphSnapshot {
    UncompactedSourceGraphSnapshot {
        roots,
        nodes: graph
            .sources
            .into_iter()
            .map(uncompacted_source_node_snapshot)
            .collect(),
        edges: graph
            .projections
            .into_iter()
            .map(|projection| UncompactedSourceEdgeSnapshot {
                id: projection.id.0,
                derived: projection.derived.0,
                basis: projection.basis.0,
                projection: projection_snapshot(projection.kind),
            })
            .collect(),
    }
}

fn uncompacted_source_node_snapshot(
    source: crate::source_graph::SourceSnapshot,
) -> UncompactedSourceNodeSnapshot {
    UncompactedSourceNodeSnapshot {
        id: source.id.0,
        uri: source.uri.display(),
        revision: source_revision_snapshot(source.revision),
    }
}

fn source_revision_snapshot(revision: SourceRevision) -> UncompactedSourceRevisionSnapshot {
    match revision {
        SourceRevision::Content(content) => UncompactedSourceRevisionSnapshot::Content {
            hash: content_hash_string(content),
        },
        SourceRevision::Version { namespace, value } => {
            UncompactedSourceRevisionSnapshot::Version {
                namespace: namespace.as_str().to_owned(),
                value,
            }
        }
    }
}

fn projection_snapshot(kind: ProjectionKind) -> UncompactedProjectionSnapshot {
    match kind {
        ProjectionKind::Identity {
            basis: IdentityBasis::EqualContent(content),
        } => UncompactedProjectionSnapshot::IdentityEqualContent {
            content_hash: content_hash_string(content),
        },
        ProjectionKind::Identity {
            basis: IdentityBasis::DeclaredByProvider(provider),
        } => UncompactedProjectionSnapshot::IdentityDeclaredByProvider { provider },
        ProjectionKind::SourceMap { map, source_index } => {
            UncompactedProjectionSnapshot::SourceMap {
                map_hash: content_hash_string(map),
                source_index,
            }
        }
        ProjectionKind::Format { formatter } => UncompactedProjectionSnapshot::Format { formatter },
        ProjectionKind::Edit { edit } => UncompactedProjectionSnapshot::Edit { edit },
        ProjectionKind::Offset {
            line_delta,
            column_delta,
        } => UncompactedProjectionSnapshot::Offset {
            line_delta,
            column_delta,
        },
    }
}

fn content_hash_string(content: crate::content_store::ContentHash) -> String {
    content
        .as_bytes()
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect()
}

fn source_content_range(
    mut source: SourceContentSnapshot,
    options: &SourceDisplayOptions,
) -> Result<SourceContentSnapshot, JsonRpcError> {
    let lines = source.content.lines().collect::<Vec<_>>();
    source.total_lines = lines.len() as u32;
    let Some(line) = options.line else {
        source.start_line = 1;
        source.end_line = source.total_lines;
        return Ok(source);
    };
    if line == 0 || line > source.total_lines {
        return Err(invalid_params(format!(
            "source line {line} is outside 1..={}",
            source.total_lines
        )));
    }
    let context = options.context_lines;
    source.start_line = line.saturating_sub(context).max(1);
    source.end_line = line.saturating_add(context).min(source.total_lines);
    source.content = lines[source.start_line as usize - 1..source.end_line as usize].join("\n");
    Ok(source)
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

fn invalid_params(message: impl Into<String>) -> JsonRpcError {
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

fn validate_raw_cdp_params(method: &str, params: &serde_json::Value) -> Result<(), String> {
    static INTERFACE: OnceLock<Result<hubrpc::prelude::HubRpcInterfaceSchema, String>> =
        OnceLock::new();
    let interface = INTERFACE
        .get_or_init(|| {
            crate::protocol_schema::import_typed_cdp_protocol(
                include_str!("../node_modules/devtools-protocol/json/browser_protocol.json"),
                include_str!("../node_modules/devtools-protocol/json/js_protocol.json"),
            )
            .map_err(|error| error.to_string())
        })
        .as_ref()
        .map_err(Clone::clone)?;
    let method_schema = interface
        .methods
        .get(method)
        .filter(|schema| schema.result.is_some())
        .ok_or_else(|| format!("unknown request method '{method}'"))?;
    let mut schema = method_schema.params.clone();
    if let (Some(object), Some(components)) = (schema.as_object_mut(), &interface.components) {
        object.insert(
            "components".into(),
            serde_json::to_value(components).map_err(|error| error.to_string())?,
        );
    }
    jsonschema::validator_for(&schema)
        .map_err(|error| format!("invalid generated schema: {error}"))?
        .validate(params)
        .map_err(|error| error.to_string())
}

fn target_debugger_rpc_error(error: TargetDebuggerError) -> JsonRpcError {
    let code = match error {
        TargetDebuggerError::InvalidBreakpointPosition
        | TargetDebuggerError::StalePause(_)
        | TargetDebuggerError::FrameNotFound(_)
        | TargetDebuggerError::ScopeNotFound(_)
        | TargetDebuggerError::SelectorNotFound(_)
        | TargetDebuggerError::UnsupportedKeyChord(_)
        | TargetDebuggerError::CoverageAlreadyActive
        | TargetDebuggerError::CoverageNotActive
        | TargetDebuggerError::CoverageCaptureNotFound(_)
        | TargetDebuggerError::CoverageCaptureAlreadyExists(_)
        | TargetDebuggerError::CpuProfileAlreadyActive
        | TargetDebuggerError::CpuProfileNotActive
        | TargetDebuggerError::CpuProfileCaptureNotFound(_)
        | TargetDebuggerError::CpuProfileCaptureAlreadyExists(_)
        | TargetDebuggerError::InvalidCpuProfileSamplingInterval
        | TargetDebuggerError::HeapCaptureNotFound(_)
        | TargetDebuggerError::InvalidHeapFilter(_)
        | TargetDebuggerError::InvalidHeapSelector(_)
        | TargetDebuggerError::InvalidHeapReference(_)
        | TargetDebuggerError::HeapNodeNotFound(_)
        | TargetDebuggerError::IncompatibleHeapCaptures { .. }
        | TargetDebuggerError::InvalidTimeout => error_codes::INVALID_PARAMS,
        TargetDebuggerError::WaitTimedOut
        | TargetDebuggerError::SettlementTimedOut
        | TargetDebuggerError::Stopped
        | TargetDebuggerError::SessionMissing
        | TargetDebuggerError::BreakpointFailed { .. }
        | TargetDebuggerError::Evaluation(_)
        | TargetDebuggerError::Properties(_)
        | TargetDebuggerError::Interaction(_)
        | TargetDebuggerError::Screenshot(_)
        | TargetDebuggerError::Coverage(_)
        | TargetDebuggerError::CpuProfile(_)
        | TargetDebuggerError::InvalidCpuProfile(_)
        | TargetDebuggerError::HeapSnapshot(_)
        | TargetDebuggerError::HeapAnalysis(_)
        | TargetDebuggerError::BatchRollback { .. }
        | TargetDebuggerError::DriverFailed(_)
        | TargetDebuggerError::SourceSearch(_)
        | TargetDebuggerError::Driver(_) => error_codes::INTERNAL_ERROR,
    };
    JsonRpcError::new(code, error.to_string())
}

fn target_source_search_rpc_error(error: TargetDebuggerError) -> JsonRpcError {
    match error {
        TargetDebuggerError::SourceSearch(error) => source_search_error(error),
        error => target_debugger_rpc_error(error),
    }
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
        | ContextTransitionError::ConnectionAlreadyDisconnecting
        | ContextTransitionError::TargetIdentityCollision { .. } => error_codes::INVALID_PARAMS,
    };
    JsonRpcError::new(code, error.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn target(target_id: &str, title: &str, url: &str) -> TargetSnapshot {
        TargetSnapshot {
            target_id: target_id.into(),
            target_type: "page".into(),
            title: title.into(),
            url: url.into(),
            attached: false,
            parent_id: None,
            opener_id: None,
            browser_context_id: None,
            subtype: None,
        }
    }

    fn context_with_targets(
        connections: impl IntoIterator<Item = (&'static str, u64, Vec<TargetSnapshot>)>,
    ) -> Arc<ContextState> {
        Arc::new(ContextState {
            display_name: "test".into(),
            revision: 1,
            connections: Arc::new(
                connections
                    .into_iter()
                    .map(|(connection_id, generation, targets)| {
                        (
                            connection_id.into(),
                            Arc::new(crate::context_engine::ConnectionState {
                                configuration: ConnectionConfiguration::DirectCdp {
                                    endpoint: format!("ws://{connection_id}"),
                                },
                                configuration_version: 1,
                                generation,
                                status: ConnectionStatus::Connected {
                                    product: "Chrome".into(),
                                    protocol_version: "1.3".into(),
                                },
                                targets: Arc::new(
                                    targets
                                        .into_iter()
                                        .map(|target| (target.target_id.clone(), target))
                                        .collect(),
                                ),
                            }),
                        )
                    })
                    .collect(),
            ),
            breakpoints: Arc::new(BTreeMap::new()),
        })
    }

    fn service_with_state(path: PathBuf, state: ServiceState) -> DebuggerService {
        let (shutdown, _) = watch::channel(false);
        let (revision_signal, _) = watch::channel(0);
        DebuggerService {
            agent_instance_id: "test-agent".into(),
            state: Arc::new(Mutex::new(state)),
            attachment_lock: Arc::new(Mutex::new(())),
            persistence_path: path,
            shutdown,
            revision_signal,
        }
    }

    fn heap_capture(
        context_id: &str,
        name: &str,
        target_id: &str,
        connection_id: &str,
        path: &Path,
    ) -> StoredCapture {
        StoredCapture {
            metadata: CaptureSnapshot {
                context_id: context_id.into(),
                name: name.into(),
                kind: CaptureKind::HeapSnapshot,
                target_id: target_id.into(),
                connection_id: connection_id.into(),
                connection_generation: 1,
                storage_id: format!("storage-{name}"),
            },
            payload: StoredCapturePayload::HeapSnapshot {
                path: path.to_string_lossy().into_owned(),
            },
        }
    }

    #[tokio::test]
    async fn capture_name_reservation_is_atomic_across_targets() {
        let context = context_with_targets([
            ("first", 1, vec![target("target-a", "A", "https://a.test")]),
            ("second", 1, vec![target("target-b", "B", "https://b.test")]),
        ]);
        let mut state = ServiceState::default();
        state.contexts.insert("test".into(), context);
        let service = service_with_state(PathBuf::from("unused"), state);

        let (first, second) = tokio::join!(
            service.reserve_capture(
                "test",
                "first",
                "target-a",
                1,
                "same-name".into(),
                CaptureKind::Coverage,
            ),
            service.reserve_capture(
                "test",
                "second",
                "target-b",
                1,
                "same-name".into(),
                CaptureKind::HeapSnapshot,
            )
        );

        assert_ne!(first.is_ok(), second.is_ok());
        let loser = first.as_ref().err().or(second.as_ref().err()).unwrap();
        assert!(loser.message.contains("already being stored"), "{loser:?}");
        let state = service.state.lock().await;
        assert_eq!(state.capture_reservations.len(), 1);
        assert!(state.captures.is_empty());
    }

    #[tokio::test]
    async fn capture_finalization_rechecks_connection_generation() {
        let mut state = ServiceState::default();
        state.contexts.insert(
            "test".into(),
            context_with_targets([(
                "runtime",
                1,
                vec![target("target-a", "A", "https://a.test")],
            )]),
        );
        let service = service_with_state(PathBuf::from("unused"), state);
        let reservation = service
            .reserve_capture(
                "test",
                "runtime",
                "target-a",
                1,
                "capture".into(),
                CaptureKind::HeapSnapshot,
            )
            .await
            .unwrap();
        service.state.lock().await.contexts.insert(
            "test".into(),
            context_with_targets([(
                "runtime",
                2,
                vec![target("target-a", "A", "https://a.test")],
            )]),
        );

        let error = service
            .finalize_capture(
                &reservation,
                StoredCapturePayload::HeapSnapshot {
                    path: "unused".into(),
                },
            )
            .await
            .unwrap_err();
        assert!(error.message.contains("generation changed"), "{error:?}");
        let state = service.state.lock().await;
        assert!(state.captures.is_empty());
        assert_eq!(state.capture_reservations.len(), 1);
    }

    #[tokio::test]
    async fn heap_storage_is_unique_and_cleanup_cannot_remove_another_reservation() {
        let context = context_with_targets([(
            "runtime",
            1,
            vec![target("target-a", "A", "https://a.test")],
        )]);
        let mut state = ServiceState::default();
        state.contexts.insert("test".into(), context);
        let root = std::env::current_dir()
            .unwrap()
            .join("target")
            .join(format!("capture-storage-{}", random_instance_id().unwrap()));
        let service = service_with_state(root.join("service.json"), state);
        let first = service
            .reserve_capture(
                "test",
                "runtime",
                "target-a",
                1,
                "first".into(),
                CaptureKind::HeapSnapshot,
            )
            .await
            .unwrap();
        let second = service
            .reserve_capture(
                "test",
                "runtime",
                "target-a",
                1,
                "second".into(),
                CaptureKind::HeapSnapshot,
            )
            .await
            .unwrap();
        let (first_staging, first_final) = service.heap_capture_paths(&first);
        let (second_staging, second_final) = service.heap_capture_paths(&second);
        assert_ne!(first_staging, second_staging);
        assert_ne!(first_final, second_final);
        fs::create_dir_all(first_final.parent().unwrap()).unwrap();
        fs::write(&first_final, b"winner").unwrap();

        service.abandon_capture(&second).await;
        remove_heap_files([second_staging, second_final]);
        assert_eq!(fs::read(&first_final).unwrap(), b"winner");

        remove_heap_files([first_final]);
        let _ = fs::remove_dir_all(root);
    }

    #[tokio::test]
    async fn persisted_heap_files_are_removed_after_catalog_and_context_deletion() {
        let root = std::env::current_dir()
            .unwrap()
            .join("target")
            .join(format!("capture-delete-{}", random_instance_id().unwrap()));
        fs::create_dir_all(&root).unwrap();
        let persistence_path = root.join("service.json");
        let first_path = root.join("first.heapsnapshot");
        let second_path = root.join("second.heapsnapshot");
        fs::write(&first_path, b"first").unwrap();
        fs::write(&second_path, b"second").unwrap();
        let context = context_with_targets([(
            "runtime",
            1,
            vec![target("target-a", "A", "https://a.test")],
        )]);
        let mut state = ServiceState::default();
        state.contexts.insert("test".into(), context);
        state
            .context_kinds
            .insert("test".into(), ContextKind::Named);
        state.captures.insert(
            ("test".into(), "first".into()),
            heap_capture("test", "first", "target-a", "runtime", &first_path),
        );
        state.captures.insert(
            ("test".into(), "second".into()),
            heap_capture("test", "second", "target-a", "runtime", &second_path),
        );
        let service = service_with_state(persistence_path.clone(), state);
        service.persist(&*service.state.lock().await).unwrap();
        drop(service);

        let (shutdown, _) = watch::channel(false);
        let restored = DebuggerService::load(shutdown, persistence_path.clone()).unwrap();
        restored
            .delete_capture(&CallCtx::default(), "test".into(), "first".into())
            .await
            .unwrap();
        assert!(!first_path.exists());
        assert!(second_path.exists());
        restored
            .delete_context(
                &CallCtx::default(),
                "test".into(),
                MutationOptions::default(),
            )
            .await
            .unwrap();
        assert!(!second_path.exists());

        let (shutdown, _) = watch::channel(false);
        let reloaded = DebuggerService::load(shutdown, persistence_path).unwrap();
        let state = reloaded.state.lock().await;
        assert!(!state.contexts.contains_key("test"));
        assert!(state.captures.is_empty());
        drop(state);
        let _ = fs::remove_dir_all(root);
    }

    #[tokio::test]
    async fn failed_catalog_persistence_leaves_heap_file_and_entry_intact() {
        let root = std::env::current_dir()
            .unwrap()
            .join("target")
            .join(format!(
                "capture-delete-failure-{}",
                random_instance_id().unwrap()
            ));
        fs::create_dir_all(&root).unwrap();
        let blocker = root.join("not-a-directory");
        fs::write(&blocker, b"block").unwrap();
        let heap_path = root.join("kept.heapsnapshot");
        fs::write(&heap_path, b"kept").unwrap();
        let mut state = ServiceState::default();
        state.captures.insert(
            ("test".into(), "kept".into()),
            heap_capture("test", "kept", "target-a", "runtime", &heap_path),
        );
        let service = service_with_state(blocker.join("service.json"), state);

        assert!(
            service
                .delete_capture(&CallCtx::default(), "test".into(), "kept".into())
                .await
                .is_err()
        );
        assert!(heap_path.exists());
        assert!(
            service
                .state
                .lock()
                .await
                .captures
                .contains_key(&("test".into(), "kept".into()))
        );
        let _ = fs::remove_dir_all(root);
    }

    #[tokio::test]
    async fn failed_context_persistence_does_not_cancel_playwright_proxies() {
        let root = std::env::current_dir()
            .unwrap()
            .join("target")
            .join(format!(
                "context-delete-failure-{}",
                random_instance_id().unwrap()
            ));
        fs::create_dir_all(&root).unwrap();
        let blocker = root.join("not-a-directory");
        fs::write(&blocker, b"block").unwrap();
        let mut state = ServiceState::default();
        state.contexts.insert(
            "test".into(),
            context_with_targets([(
                "runtime",
                1,
                vec![target("target-a", "A", "https://a.test")],
            )]),
        );
        state
            .context_kinds
            .insert("test".into(), ContextKind::Named);
        let (cancel, cancelled) = watch::channel(false);
        state.playwright_proxies.insert(
            "proxy".into(),
            PlaywrightProxyRegistration {
                context_id: "test".into(),
                connection_id: "runtime".into(),
                target_id: "target-a".into(),
                generation: 1,
                cancel,
            },
        );
        let service = service_with_state(blocker.join("service.json"), state);

        assert!(
            service
                .delete_context(
                    &CallCtx::default(),
                    "test".into(),
                    MutationOptions::default(),
                )
                .await
                .is_err()
        );
        assert!(!*cancelled.borrow());
        let state = service.state.lock().await;
        assert!(state.contexts.contains_key("test"));
        assert!(state.playwright_proxies.contains_key("proxy"));
        drop(state);
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn startup_scavenges_only_unreferenced_heap_capture_files() {
        let root = std::env::current_dir()
            .unwrap()
            .join("target")
            .join(format!(
                "capture-startup-scavenge-{}",
                random_instance_id().unwrap()
            ));
        let persistence_path = root.join("service.json");
        let capture_root = persistence_path.with_extension("captures");
        let cataloged = capture_root.join("cataloged").join("valid.heapsnapshot");
        let orphan_final = capture_root.join("orphan").join("lost.heapsnapshot");
        let orphan_partial = capture_root.join("orphan").join("interrupted.partial");
        let unrelated = capture_root.join("orphan").join("notes.txt");
        for path in [&cataloged, &orphan_final, &orphan_partial, &unrelated] {
            fs::create_dir_all(path.parent().unwrap()).unwrap();
            fs::write(path, path.to_string_lossy().as_bytes()).unwrap();
        }
        let mut state = ServiceState::default();
        state.captures.insert(
            ("test".into(), "valid".into()),
            heap_capture("test", "valid", "target-a", "runtime", &cataloged),
        );
        let writer = service_with_state(persistence_path.clone(), state);
        writer.persist(&writer.state.blocking_lock()).unwrap();
        drop(writer);

        let (shutdown, _) = watch::channel(false);
        let service = DebuggerService::load(shutdown, persistence_path).unwrap();
        assert!(cataloged.exists());
        assert!(!orphan_final.exists());
        assert!(!orphan_partial.exists());
        assert!(unrelated.exists());
        assert!(
            service
                .state
                .blocking_lock()
                .captures
                .contains_key(&("test".into(), "valid".into()))
        );
        drop(service);
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn canonical_target_id_wins_over_friendly_matches_context_wide() {
        let context = context_with_targets([
            (
                "first",
                2,
                vec![target("canonical", "Unrelated", "https://first.test")],
            ),
            (
                "second",
                7,
                vec![target(
                    "another-id",
                    "canonical friendly title",
                    "https://second.test/canonical",
                )],
            ),
        ]);

        let mut state = ServiceState::default();
        state.contexts.insert("test".into(), context);
        let resolved =
            DebuggerService::resolve_canonical_target(&state, "test", "canonical").unwrap();
        assert_eq!(resolved.connection_id, "first");
        assert_eq!(resolved.target_id, "canonical");
        assert_eq!(resolved.connection_generation, 2);
    }

    #[test]
    fn friendly_target_ambiguity_reports_qualified_candidates() {
        let context = context_with_targets([
            (
                "browser-a",
                3,
                vec![target("target-a", "Dashboard", "https://a.test/app")],
            ),
            (
                "browser-b",
                8,
                vec![target("target-b", "Dashboard", "https://b.test/app")],
            ),
        ]);

        let mut state = ServiceState::default();
        state.contexts.insert("test".into(), context);
        let error =
            DebuggerService::resolve_canonical_target(&state, "test", "Dashboard").unwrap_err();
        let error = error.message;
        assert!(error.contains("browser-a/target-a@3"), "{error}");
        assert!(error.contains("browser-b/target-b@8"), "{error}");
    }

    #[test]
    fn synthetic_node_roots_are_unique_but_node_selector_is_friendly() {
        let mut first = target(
            &synthetic_node_target_id("runtime-a"),
            "Node.js",
            "ws://runtime-a",
        );
        first.target_type = "node".into();
        let mut second = target(
            &synthetic_node_target_id("runtime-b"),
            "Node.js",
            "ws://runtime-b",
        );
        second.target_type = "node".into();
        let context = context_with_targets([
            ("runtime-a", 1, vec![first]),
            ("runtime-b", 1, vec![second]),
        ]);
        let mut state = ServiceState::default();
        state.contexts.insert("test".into(), context);

        let exact = DebuggerService::resolve_canonical_target(
            &state,
            "test",
            &synthetic_node_target_id("runtime-b"),
        )
        .unwrap();
        assert_eq!(exact.connection_id, "runtime-b");
        let error = DebuggerService::resolve_canonical_target(&state, "test", "node").unwrap_err();
        assert!(
            error.message.contains("runtime-a/$node-root:runtime-a@1"),
            "{error:?}"
        );
        assert!(
            error.message.contains("runtime-b/$node-root:runtime-b@1"),
            "{error:?}"
        );
    }

    #[test]
    fn validates_raw_cdp_params_against_generated_protocol_schema() {
        validate_raw_cdp_params(
            "Runtime.evaluate",
            &serde_json::json!({ "expression": "1 + 1", "returnByValue": true }),
        )
        .unwrap();
        validate_raw_cdp_params(
            "HeapProfiler.getObjectByHeapObjectId",
            &serde_json::json!({ "objectId": "42" }),
        )
        .unwrap();

        let missing =
            validate_raw_cdp_params("Runtime.evaluate", &serde_json::json!({})).unwrap_err();
        assert!(missing.contains("expression"), "{missing}");
        let unknown =
            validate_raw_cdp_params("Runtime.notACommand", &serde_json::json!({})).unwrap_err();
        assert!(unknown.contains("unknown request method"), "{unknown}");
    }

    #[test]
    fn migrates_endpoint_only_persistence_to_direct_cdp_configuration() {
        let path = std::env::current_dir()
            .unwrap()
            .join("target")
            .join(format!(
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
        assert_eq!(state.context_kinds["legacy"], ContextKind::Named);
        assert!(state.captures.is_empty());
        let connection = &state.contexts["legacy"].connections["browser"];
        assert_eq!(
            connection.configuration,
            ConnectionConfiguration::DirectCdp {
                endpoint: "ws://127.0.0.1:9222".into()
            }
        );
        let _ = fs::remove_file(path);
    }

    #[test]
    fn schema_four_round_trips_capture_catalog() {
        let path = std::env::current_dir()
            .unwrap()
            .join("target")
            .join(format!(
                "jsdbg-persistence-v4-{}-{}.json",
                std::process::id(),
                random_instance_id().unwrap()
            ));
        let state = StoredServiceState {
            schema_version: 4,
            contexts: BTreeMap::new(),
            completed_requests: Vec::new(),
            captures: vec![StoredCapture {
                metadata: CaptureSnapshot {
                    context_id: "context".into(),
                    name: "baseline".into(),
                    kind: CaptureKind::HeapSnapshot,
                    target_id: "canonical-target".into(),
                    connection_id: "browser".into(),
                    connection_generation: 11,
                    storage_id: "immutable-storage".into(),
                },
                payload: StoredCapturePayload::HeapSnapshot {
                    path: "captures/immutable.heapsnapshot".into(),
                },
            }],
        };
        fs::write(&path, serde_json::to_vec_pretty(&state).unwrap()).unwrap();
        let restored = load_state(&path).unwrap();
        let capture = &restored.captures[&("context".into(), "baseline".into())];
        assert_eq!(capture.metadata.target_id, "canonical-target");
        assert_eq!(capture.metadata.connection_generation, 11);
        assert_eq!(capture.metadata.storage_id, "immutable-storage");
        assert!(matches!(
            capture.payload,
            StoredCapturePayload::HeapSnapshot { .. }
        ));
        let _ = fs::remove_file(path);
    }
}
