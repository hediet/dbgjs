use std::collections::{BTreeMap, BTreeSet, VecDeque};
use std::fs;
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, OnceLock};
use std::time::Duration;

use atomic_write_file::AtomicWriteFile;
use futures_util::{StreamExt, future::join_all, stream};
use globset::{Glob, GlobMatcher};
use hubrpc::prelude::{CallCtx, JsonRpcError, error_codes};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use tokio::sync::{Mutex, watch};
use tokio::time::{Instant, timeout_at};

use crate::capability::{
    CapabilityKind, CapabilityObject, DebugCapability, DebugOpenRequest, DebugSessionHandle,
};
use crate::cdp::{
    BrowserGetVersionParams, TargetDetachFromTargetParams, TargetGetTargetsParams,
    TargetSetDiscoverTargetsParams,
};
use crate::connection_provider::{ConnectionRuntime, validate_configuration};
use crate::context_engine::{
    BreakpointState, ConnectionAttempt, ConnectionState, ContextEffect, ContextInput, ContextState,
    ContextTransitionError, EffectCompletion, RuntimeObservation, TargetGraphChange, UserCommand,
    reduce_context,
};
use crate::context_identity::{
    ContextKind, compare_context_paths, normalize_absolute_path, path_relation,
    synthetic_node_target_id,
};
use crate::context_source_model::{
    CompactedProjectionKind, ContextSourceGraphSnapshot, ContextSourceModel,
};
use crate::debugger_engine::{SessionKey, StepKind};
use crate::discovery::{DiscoveryState, PauseChildrenLease};
use crate::resource_graph::{
    GraphDelta, GraphSink, RelationKind, ResourceFacts, ResourceGraph, ResourceId, ResourceKind,
    ResourceUpsert, SharedResourceGraph, SourceId,
};
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
    ProcessTreeSnapshot, PromiseSelectionSnapshot, PromiseState, RelayEndpoint,
    ResourceCapabilitySnapshot, ResourceFrontierSnapshot, ResourceGraphSnapshot,
    ResourceRelationSnapshot, ResourceSnapshot, ScreenshotSnapshot, ServiceInfo,
    SourceContentSnapshot, SourceDisplayOptions, SourceFormattingMode, SourceFormattingRule,
    SourceFormattingSettings, SourceGraphViewSnapshot, SourceMappingSnapshot, SourceMatchSnapshot,
    SourceSearchOptions, SourceSearchSnapshot, SourceSnapshotInfo, SourceSuffixRewriteSnapshot,
    SourceTreeKind, SourceTreeSnapshot, SourceViewPreference, StepKind as ApiStepKind,
    TargetAttachOptions, TargetAttachmentOutcome, TargetAttachmentResult, TargetAttachmentState,
    TargetDebuggerSnapshot, TargetSnapshot, TargetWaitPredicate, UncompactedProjectionSnapshot,
    UncompactedSourceEdgeSnapshot, UncompactedSourceGraphSnapshot, UncompactedSourceNodeSnapshot,
    UncompactedSourceRevisionSnapshot, ValueInspectionOptions, ValueSelector, ValueSnapshot,
    VariableSnapshot, breakpoint_applies_to_target,
};
use crate::source_graph::{IdentityBasis, ProjectionKind, SourceRevision};
use crate::source_search::{
    HydratedSource, SearchControl, SearchDocument, SearchError, SearchQuery, SourceIdentity,
};
use crate::source_view::appears_minified;
use crate::target_debugger::{
    TargetBreakpointSpec, TargetDebuggerError, TargetDebuggerHandle, stored_heap_classes,
};
use crate::target_domain::target_snapshot_from_info;

#[derive(Clone)]
pub struct DebuggerService {
    agent_instance_id: String,
    state: Arc<Mutex<ServiceState>>,
    attachment_lock: Arc<Mutex<()>>,
    relay_lifecycle_lock: Arc<Mutex<()>>,
    relay_attachment_lock: Arc<Mutex<()>>,
    relay_attachments: Arc<Mutex<BTreeMap<(String, String, String, String), RelayDebugAttachment>>>,
    persistence_path: PathBuf,
    shutdown: watch::Sender<bool>,
    revision_signal: watch::Sender<u64>,
    capture_storage: CaptureStorage,
}

#[derive(Clone, Default)]
struct CaptureStorage {
    #[cfg(test)]
    hooks: Arc<CaptureStorageTestHooks>,
}

#[cfg(test)]
#[derive(Default)]
struct CaptureStorageTestHooks {
    fail_next_sync: AtomicBool,
    fail_next_parent_sync: AtomicBool,
    pause_next_remove: AtomicBool,
    fail_next_remove: AtomicBool,
    remove_started: tokio::sync::Notify,
    continue_remove: tokio::sync::Notify,
}

impl CaptureStorage {
    fn sync_file(&self, path: &Path) -> std::io::Result<()> {
        #[cfg(test)]
        if self.hooks.fail_next_sync.swap(false, Ordering::SeqCst) {
            return Err(std::io::Error::other(
                "injected capture storage sync failure",
            ));
        }
        fs::OpenOptions::new().write(true).open(path)?.sync_all()
    }

    fn sync_parent(&self, _path: &Path) -> std::io::Result<()> {
        #[cfg(test)]
        if self
            .hooks
            .fail_next_parent_sync
            .swap(false, Ordering::SeqCst)
            || self.hooks.fail_next_sync.swap(false, Ordering::SeqCst)
        {
            return Err(std::io::Error::other(
                "injected capture storage sync failure",
            ));
        }
        #[cfg(unix)]
        if let Some(parent) = _path.parent() {
            fs::File::open(parent)?.sync_all()?;
        }
        Ok(())
    }

    async fn remove_for_delete(&self, path: &Path) -> std::io::Result<()> {
        #[cfg(test)]
        if self.hooks.pause_next_remove.swap(false, Ordering::SeqCst) {
            self.hooks.remove_started.notify_one();
            self.hooks.continue_remove.notified().await;
        }
        #[cfg(test)]
        if self.hooks.fail_next_remove.swap(false, Ordering::SeqCst) {
            return Err(std::io::Error::other(
                "injected capture storage deletion failure",
            ));
        }
        remove_capture_payload_file(path)
    }

    #[cfg(test)]
    fn fail_next_sync(&self) {
        self.hooks.fail_next_sync.store(true, Ordering::SeqCst);
    }

    #[cfg(test)]
    fn fail_next_parent_sync(&self) {
        self.hooks
            .fail_next_parent_sync
            .store(true, Ordering::SeqCst);
    }

    #[cfg(test)]
    fn pause_next_remove_with_failure(&self) {
        self.hooks.pause_next_remove.store(true, Ordering::SeqCst);
        self.hooks.fail_next_remove.store(true, Ordering::SeqCst);
    }

    #[cfg(test)]
    async fn wait_for_remove(&self) {
        self.hooks.remove_started.notified().await;
    }

    #[cfg(test)]
    fn continue_remove(&self) {
        self.hooks.continue_remove.notify_one();
    }
}

impl DebuggerService {
    pub fn load(
        shutdown: watch::Sender<bool>,
        persistence_path: PathBuf,
    ) -> Result<Self, ServicePersistenceError> {
        let state = load_state(&persistence_path)?;
        scavenge_capture_storage(&persistence_path, &state);
        let (revision_signal, _) = watch::channel(0);
        Ok(Self {
            agent_instance_id: random_instance_id()?,
            state: Arc::new(Mutex::new(state)),
            attachment_lock: Arc::new(Mutex::new(())),
            relay_lifecycle_lock: Arc::new(Mutex::new(())),
            relay_attachment_lock: Arc::new(Mutex::new(())),
            relay_attachments: Arc::new(Mutex::new(BTreeMap::new())),
            persistence_path,
            shutdown,
            revision_signal,
            capture_storage: CaptureStorage::default(),
        })
    }

    fn persist(&self, state: &ServiceState) -> Result<(), ServicePersistenceError> {
        let stored = StoredServiceState::from(state);
        persist_stored_state(&self.persistence_path, &stored)
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
                retract_connection_resource_graph(
                    &mut state,
                    &context_id,
                    &connection_id,
                    generation,
                );
                cancel_playwright_proxies(
                    &mut state,
                    &context_id,
                    &connection_id,
                    Some(generation),
                );
                remove_connection_debugger_registrations(&mut state, &context_id, &connection_id);
                state
                    .pause_children_leases
                    .remove(&(context_id.clone(), connection_id.clone()));
                if let Some(context) = state.contexts.get(&context_id).cloned() {
                    let transition = reduce_context(
                        &context,
                        ContextInput::RuntimeObservation(RuntimeObservation::ConnectionClosed {
                            connection_id: connection_id.clone(),
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
            if is_current_runtime {
                service
                    .release_relay_attachments_for_connection(&context_id, &connection_id)
                    .await;
            }
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
                let update = match event {
                    Ok(crate::cdp_runtime::RootCdpEvent::TargetCreated(params)) => {
                        ConnectionTargetUpdate::Upsert(canonicalize_synthetic_target(
                            target_snapshot_from_info(params.target_info),
                            &connection_id,
                        ))
                    }
                    Ok(crate::cdp_runtime::RootCdpEvent::TargetChanged(params)) => {
                        ConnectionTargetUpdate::Upsert(canonicalize_synthetic_target(
                            target_snapshot_from_info(params.target_info),
                            &connection_id,
                        ))
                    }
                    Ok(crate::cdp_runtime::RootCdpEvent::TargetDestroyed(params)) => {
                        ConnectionTargetUpdate::Remove(canonicalize_synthetic_target_id(
                            &params.target_id,
                            &connection_id,
                        ))
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
                let Some((targets, change, removed_targets)) = prepare_connection_target_update(
                    &state,
                    &context_id,
                    &connection_id,
                    generation,
                    update,
                ) else {
                    continue;
                };
                let transition = reduce_context(
                    &context,
                    ContextInput::RuntimeObservation(RuntimeObservation::TargetGraphChanged {
                        connection_id: connection_id.clone(),
                        attempt,
                        change: change.clone(),
                    }),
                )
                .expect("target graph observations do not fail");
                if transition.change == crate::context_engine::ContextChange::None {
                    continue;
                }
                let retracted_sessions = removed_targets
                    .iter()
                    .flat_map(|target_id| {
                        debug_session_sources_for_target(
                            &state,
                            &context_id,
                            &connection_id,
                            generation,
                            target_id,
                        )
                    })
                    .collect::<Vec<_>>();
                if let Err(error) = stage_connection_resource_graph(
                    &mut state,
                    &context_id,
                    &connection_id,
                    &transition.state,
                    &targets,
                    &runtime,
                    &retracted_sessions,
                ) {
                    eprintln!("rejecting target discovery graph update: {error}");
                    drop(state);
                    runtime.close().await;
                    break;
                }
                service.commit_context(&mut state, &context_id, transition);
                for target_id in &removed_targets {
                    cancel_playwright_proxies_for_target(
                        &mut state,
                        &context_id,
                        &connection_id,
                        target_id,
                    );
                    remove_debugger_registration(
                        &mut state,
                        &(context_id.clone(), connection_id.clone(), target_id.clone()),
                    );
                }
                drop(state);
                for target_id in removed_targets {
                    service
                        .release_relay_attachments_for_target(
                            &context_id,
                            &connection_id,
                            &target_id,
                        )
                        .await;
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
                let (update, target_to_attach) = match event {
                    crate::connection_provider::ProviderTargetEvent::Upsert(target) => {
                        let target = canonicalize_synthetic_target(target, &connection_id);
                        let target_id = target.target_id.clone();
                        (ConnectionTargetUpdate::Upsert(target), Some(target_id))
                    }
                    crate::connection_provider::ProviderTargetEvent::Removed(target_id) => (
                        ConnectionTargetUpdate::Remove(canonicalize_synthetic_target_id(
                            &target_id,
                            &connection_id,
                        )),
                        None,
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
                let Some((targets, change, removed_targets)) = prepare_connection_target_update(
                    &state,
                    &context_id,
                    &connection_id,
                    generation,
                    update,
                ) else {
                    continue;
                };
                let transition = reduce_context(
                    &context,
                    ContextInput::RuntimeObservation(RuntimeObservation::TargetGraphChanged {
                        connection_id: connection_id.clone(),
                        attempt,
                        change,
                    }),
                )
                .expect("provider target graph observations do not fail");
                let retracted_sessions = removed_targets
                    .iter()
                    .flat_map(|target_id| {
                        debug_session_sources_for_target(
                            &state,
                            &context_id,
                            &connection_id,
                            generation,
                            target_id,
                        )
                    })
                    .collect::<Vec<_>>();
                if let Err(error) = stage_connection_resource_graph(
                    &mut state,
                    &context_id,
                    &connection_id,
                    &transition.state,
                    &targets,
                    &runtime,
                    &retracted_sessions,
                ) {
                    eprintln!("rejecting provider target graph update: {error}");
                    drop(state);
                    runtime.close().await;
                    break;
                }
                service.commit_context(&mut state, &context_id, transition);
                for target_id in &removed_targets {
                    cancel_playwright_proxies_for_target(
                        &mut state,
                        &context_id,
                        &connection_id,
                        target_id,
                    );
                    remove_debugger_registration(
                        &mut state,
                        &(context_id.clone(), connection_id.clone(), target_id.clone()),
                    );
                }
                drop(state);
                for target_id in removed_targets {
                    service
                        .release_relay_attachments_for_target(
                            &context_id,
                            &connection_id,
                            &target_id,
                        )
                        .await;
                }

                if !runtime.is_direct_debugger()
                    && let Some(target_id) = target_to_attach
                {
                    let _ = service
                        .attach_target_internal(
                            &CallCtx::default(),
                            context_id.clone(),
                            connection_id.clone(),
                            target_id,
                            TargetAttachOptions::default(),
                        )
                        .await;
                }
            }
        });
    }

    async fn publish_target_attachment_change(
        &self,
        key: &(String, String, String),
        attempt: ConnectionAttempt,
    ) {
        let mut state = self.state.lock().await;
        let Some(context) = state.contexts.get(&key.0).cloned() else {
            return;
        };
        let transition = reduce_context(
            &context,
            ContextInput::RuntimeObservation(RuntimeObservation::TargetGraphChanged {
                connection_id: key.1.clone(),
                attempt,
                change: TargetGraphChange::Changed {
                    target_id: key.2.clone(),
                },
            }),
        )
        .expect("target attachment observations do not fail");
        if transition.change == crate::context_engine::ContextChange::None {
            return;
        }
        self.commit_context(&mut state, &key.0, transition);
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
    resource_graphs: BTreeMap<String, SharedResourceGraph>,
    process_projections: BTreeMap<String, Vec<ProcessTreeSnapshot>>,
    source_models: BTreeMap<String, Arc<ContextSourceModel>>,
    context_kinds: BTreeMap<String, ContextKind>,
    runtimes: BTreeMap<(String, String), Arc<ConnectionRuntime>>,
    target_debuggers: BTreeMap<(String, String, String), TargetDebuggerHandle>,
    debug_attachments: BTreeMap<(String, String, String), DebugAttachment>,
    pause_children_leases: BTreeMap<(String, String), PauseChildrenLease>,
    history: BTreeMap<String, VecDeque<ContextObservation>>,
    completed_requests: BTreeMap<(String, String), u64>,
    playwright_proxies: BTreeMap<String, PlaywrightProxyRegistration>,
    relays: BTreeMap<String, RelayRegistration>,
    captures: BTreeMap<(String, String), StoredCapture>,
    capture_reservations: BTreeMap<(String, String), CaptureReservation>,
    next_capture_id: u64,
    next_publication_order: u64,
}

#[derive(Clone)]
struct CaptureReservation {
    metadata: CaptureSnapshot,
    completed: Option<CompletedCapture>,
    deleting: bool,
}

#[derive(Clone)]
struct CompletedCapture {
    payload: CapturePayloadReference,
    heap_result: Option<HeapCaptureResult>,
}

struct PromotedCapture {
    payload: CapturePayload,
    heap_result: Option<HeapCaptureResult>,
}

struct CaptureReservationGuard {
    service: DebuggerService,
    reservation: CaptureReservation,
}

struct CaptureDeletionGuard {
    service: DebuggerService,
    key: (String, String),
    storage_id: String,
    armed: bool,
}

impl CaptureDeletionGuard {
    fn new(service: DebuggerService, key: (String, String), storage_id: String) -> Self {
        Self {
            service,
            key,
            storage_id,
            armed: true,
        }
    }

    async fn restore(&mut self) {
        self.service
            .restore_failed_capture_deletion(&self.key, &self.storage_id)
            .await;
        self.armed = false;
    }

    fn disarm(&mut self) {
        self.armed = false;
    }
}

impl Drop for CaptureDeletionGuard {
    fn drop(&mut self) {
        if !self.armed {
            return;
        }
        let service = self.service.clone();
        let key = self.key.clone();
        let storage_id = self.storage_id.clone();
        if let Ok(runtime) = tokio::runtime::Handle::try_current() {
            runtime.spawn(async move {
                service
                    .restore_failed_capture_deletion(&key, &storage_id)
                    .await;
            });
        }
    }
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
                    remove_capture_payload_files([staging, final_path]);
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
    closed: watch::Receiver<bool>,
}

/// Tracks one open `dbgjs context relay` or `dbgjs target relay`. Its mere presence for a
/// context is what makes relay ownership exclusive: see `ensure_context_not_relayed`.
#[derive(Clone)]
struct RelayRegistration {
    context_id: String,
    cancel: watch::Sender<bool>,
}

/// Returns a clear, actionable error if `context_id` is currently owned by an active relay.
/// Relay-internal code paths (attach, raw CDP forwarding) call the `_bypassing_relay` /
/// `_internal` siblings of the guarded methods directly instead of going through this check.
fn ensure_context_not_relayed(state: &ServiceState, context_id: &str) -> Result<(), JsonRpcError> {
    if state
        .relays
        .values()
        .any(|registration| registration.context_id == context_id)
    {
        return Err(invalid_state(&format!(
            "context '{context_id}' is exclusively owned by an active relay (dbgjs context relay or dbgjs target relay); local target debugging commands are unavailable until the relay closes"
        )));
    }
    Ok(())
}

struct PhysicalAttachmentOwner {
    key: (String, String, String),
    debugger: TargetDebuggerHandle,
    runtime: Arc<ConnectionRuntime>,
    attempt: ConnectionAttempt,
    attachment: Option<DebugAttachment>,
}

#[derive(Clone)]
struct DebugAttachment {
    capability: Arc<dyn DebugCapability>,
    handle: DebugSessionHandle,
    graph_source: SourceId,
}

#[derive(Clone)]
struct RelayDebugAttachment {
    debugger: TargetDebuggerHandle,
    attachment: DebugAttachment,
}

fn physical_target_key(
    state: &ServiceState,
    key: &(String, String, String),
) -> Result<String, JsonRpcError> {
    let connection = state
        .contexts
        .get(&key.0)
        .ok_or_else(|| not_found("context", &key.0))?
        .connections
        .get(&key.1)
        .ok_or_else(|| not_found("connection", &key.1))?;
    let target = context_connection_target(state, &key.0, &key.1, connection.generation, &key.2)
        .ok_or_else(|| not_found("target", &key.2))?;
    Ok(target.resource_id.to_string())
}

fn ownership_conflict(owner: &(String, String, String)) -> JsonRpcError {
    invalid_state(&format!(
        "target ownership conflict: physical target is already owned by {}/{}; target {}; retry with --force to steal it",
        owner.0, owner.1, owner.2
    ))
}

fn direct_attachment_error(message: String, force: bool) -> JsonRpcError {
    if message.contains("already attached by another debugger")
        || message.contains("already has a dbgjs client")
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
    payload: CapturePayloadReference,
    #[serde(default)]
    publication_order: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    heap_mapping: Option<crate::service_api::HeapMappingSnapshot>,
}

fn capture_prefix(kind: CaptureKind) -> &'static str {
    match kind {
        CaptureKind::Coverage => "cov",
        CaptureKind::CpuProfile => "profile",
        CaptureKind::HeapSnapshot => "heap",
    }
}

fn select_stored_capture<'a>(
    state: &'a ServiceState,
    context_id: &str,
    selector: &str,
    kind: Option<CaptureKind>,
    target_id: Option<&str>,
    connection_id: Option<&str>,
) -> Result<&'a StoredCapture, JsonRpcError> {
    let canonical_target = target_id.and_then(|target| {
        DebuggerService::resolve_canonical_target(state, context_id, target).ok()
    });
    let resolved_selector = canonical_target.as_ref().map(|target| {
        crate::target_selector::resolved_target_selector(
            &target.connection_id, &target.target_id, target.connection_generation, target_id,
        )
    });
    let target_id = resolved_selector.as_deref().or(target_id);
    let connection_id = connection_id.or_else(||
        canonical_target.as_ref().map(|target| target.connection_id.as_str()));
    let matches = |capture: &&StoredCapture| {
        let metadata = &capture.metadata;
        metadata.context_id == context_id
            && kind.is_none_or(|kind| metadata.kind == kind)
            && connection_id.is_none_or(|connection| metadata.connection_id == connection)
            && target_id.is_none_or(|target| {
                crate::target_selector::resolved_target_selector(
                    &metadata.connection_id,
                    &metadata.target_id,
                    metadata.connection_generation,
                    Some(target),
                ) == target
            })
    };
    if let Some(index) =
        crate::service_api::capture_relative_index(selector).map_err(invalid_params)?
    {
        let mut captures = state.captures.values().filter(matches).collect::<Vec<_>>();
        captures.sort_by_key(|capture| std::cmp::Reverse(capture.publication_order));
        captures.get(index - 1).copied().ok_or_else(|| {
            invalid_params(format!(
                "capture selector '{selector}' has no match in context '{context_id}' for the requested kind and target scope"
            ))
        })
    } else {
        state
            .captures
            .get(&(context_id.to_owned(), selector.to_owned()))
            .filter(matches)
            .ok_or_else(|| not_found("capture in requested kind and target scope", selector))
    }
}

#[derive(Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
struct CapturePayloadReference {
    path: String,
    sha256: String,
    byte_len: u64,
}

#[derive(Clone)]
enum CapturePayload {
    Coverage(CoverageSnapshot),
    CpuProfile(CpuProfileSnapshot),
    HeapSnapshot { path: String },
}

impl StoredCapture {
    fn payload_path(&self) -> PathBuf {
        self.payload.path.as_str().into()
    }
}

fn write_capture_payload(
    persistence_path: &Path,
    metadata: &CaptureSnapshot,
    payload: &CapturePayload,
) -> Result<CapturePayloadReference, ServicePersistenceError> {
    write_capture_payload_with_storage(
        persistence_path,
        metadata,
        payload,
        &CaptureStorage::default(),
    )
}

fn write_capture_payload_with_storage(
    persistence_path: &Path,
    metadata: &CaptureSnapshot,
    payload: &CapturePayload,
    storage: &CaptureStorage,
) -> Result<CapturePayloadReference, ServicePersistenceError> {
    match (metadata.kind, payload) {
        (CaptureKind::Coverage, CapturePayload::Coverage(snapshot)) => {
            let bytes = serde_json::to_vec(snapshot)?;
            let (staging, final_path) = capture_payload_paths_for(persistence_path, metadata);
            write_atomic_payload_file(storage, &staging, &final_path, &bytes)?;
            Ok(payload_reference_from_bytes(final_path, &bytes))
        }
        (CaptureKind::CpuProfile, CapturePayload::CpuProfile(snapshot)) => {
            let bytes = serde_json::to_vec(snapshot)?;
            let (staging, final_path) = capture_payload_paths_for(persistence_path, metadata);
            write_atomic_payload_file(storage, &staging, &final_path, &bytes)?;
            Ok(payload_reference_from_bytes(final_path, &bytes))
        }
        (CaptureKind::HeapSnapshot, CapturePayload::HeapSnapshot { path }) => {
            payload_reference_from_file(Path::new(path))
        }
        _ => Err(ServicePersistenceError::CapturePayload(
            "capture metadata kind does not match its payload".to_owned(),
        )),
    }
}

fn write_atomic_payload_file(
    storage: &CaptureStorage,
    staging_path: &Path,
    final_path: &Path,
    bytes: &[u8],
) -> Result<(), ServicePersistenceError> {
    if let Some(parent) = final_path.parent() {
        fs::create_dir_all(parent)?;
    }
    let expected = payload_reference_from_bytes(final_path.to_owned(), bytes);
    if final_path.exists() {
        let existing = payload_reference_from_file(final_path)?;
        if existing.byte_len == expected.byte_len && existing.sha256 == expected.sha256 {
            storage.sync_file(final_path)?;
            storage.sync_parent(final_path)?;
            return Ok(());
        }
        return Err(ServicePersistenceError::CapturePayload(format!(
            "immutable capture payload '{}' already exists with different content",
            final_path.display()
        )));
    }
    let mut file = AtomicWriteFile::open(staging_path)?;
    file.write_all(bytes)?;
    file.commit()?;
    storage.sync_file(staging_path)?;
    match fs::rename(staging_path, final_path) {
        Ok(()) => {
            storage.sync_parent(final_path)?;
            Ok(())
        }
        Err(rename_error) => {
            let published = payload_reference_from_file(final_path).is_ok_and(|existing| {
                existing.byte_len == expected.byte_len && existing.sha256 == expected.sha256
            });
            let _ = remove_capture_payload_file(staging_path);
            if published {
                storage.sync_file(final_path)?;
                storage.sync_parent(final_path)?;
                Ok(())
            } else {
                Err(rename_error.into())
            }
        }
    }
}

fn payload_reference_from_bytes(path: PathBuf, bytes: &[u8]) -> CapturePayloadReference {
    CapturePayloadReference {
        path: path.to_string_lossy().into_owned(),
        sha256: hex_digest(Sha256::digest(bytes)),
        byte_len: bytes.len() as u64,
    }
}

fn payload_reference_from_file(
    path: &Path,
) -> Result<CapturePayloadReference, ServicePersistenceError> {
    let mut file = fs::File::open(path)?;
    let mut digest = Sha256::new();
    let mut byte_len = 0_u64;
    let mut buffer = [0_u8; 64 * 1024];
    loop {
        let read = file.read(&mut buffer)?;
        if read == 0 {
            break;
        }
        digest.update(&buffer[..read]);
        byte_len = byte_len.saturating_add(read as u64);
    }
    Ok(CapturePayloadReference {
        path: path.to_string_lossy().into_owned(),
        sha256: hex_digest(digest.finalize()),
        byte_len,
    })
}

fn hex_digest(digest: impl AsRef<[u8]>) -> String {
    digest
        .as_ref()
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect()
}

fn validate_capture_payload_reference(
    reference: &CapturePayloadReference,
) -> Result<(), ServicePersistenceError> {
    let actual = payload_reference_from_file(Path::new(&reference.path))?;
    if actual.byte_len != reference.byte_len || actual.sha256 != reference.sha256 {
        return Err(ServicePersistenceError::CapturePayload(format!(
            "capture payload '{}' failed integrity validation (expected {} bytes / {}, found {} bytes / {})",
            reference.path, reference.byte_len, reference.sha256, actual.byte_len, actual.sha256
        )));
    }
    Ok(())
}

fn validate_capture_payload_kind(
    reference: &CapturePayloadReference,
    kind: CaptureKind,
) -> Result<(), ServicePersistenceError> {
    let expected_suffix = match kind {
        CaptureKind::Coverage => ".coverage.json",
        CaptureKind::CpuProfile => ".cpuprofile.json",
        CaptureKind::HeapSnapshot => ".heapsnapshot",
    };
    if !reference.path.ends_with(expected_suffix) {
        return Err(ServicePersistenceError::CapturePayload(format!(
            "capture payload '{}' does not match expected {kind:?} storage",
            reference.path
        )));
    }
    Ok(())
}

fn validate_capture_payload_location(
    persistence_path: &Path,
    capture: &StoredCapture,
) -> Result<(), ServicePersistenceError> {
    validate_capture_payload_kind(&capture.payload, capture.metadata.kind)?;
    if capture.metadata.kind != CaptureKind::HeapSnapshot {
        let (_, expected_path) = capture_payload_paths_for(persistence_path, &capture.metadata);
        if storage_path_key(Path::new(&capture.payload.path)) != storage_path_key(&expected_path) {
            return Err(ServicePersistenceError::CapturePayload(format!(
                "capture payload '{}' does not match immutable storage '{}'",
                capture.payload.path,
                expected_path.display()
            )));
        }
    }
    Ok(())
}

fn load_capture_payload(
    reference: &CapturePayloadReference,
    kind: CaptureKind,
) -> Result<CapturePayload, ServicePersistenceError> {
    validate_capture_payload_kind(reference, kind)?;
    match kind {
        CaptureKind::Coverage | CaptureKind::CpuProfile => {
            let bytes = fs::read(&reference.path)?;
            let actual = payload_reference_from_bytes(PathBuf::from(&reference.path), &bytes);
            if actual.byte_len != reference.byte_len || actual.sha256 != reference.sha256 {
                return Err(ServicePersistenceError::CapturePayload(format!(
                    "capture payload '{}' failed integrity validation",
                    reference.path
                )));
            }
            match kind {
                CaptureKind::Coverage => {
                    Ok(CapturePayload::Coverage(serde_json::from_slice(&bytes)?))
                }
                CaptureKind::CpuProfile => {
                    Ok(CapturePayload::CpuProfile(serde_json::from_slice(&bytes)?))
                }
                CaptureKind::HeapSnapshot => unreachable!(),
            }
        }
        CaptureKind::HeapSnapshot => {
            validate_capture_payload_reference(reference)?;
            Ok(CapturePayload::HeapSnapshot {
                path: reference.path.clone(),
            })
        }
    }
}

fn remove_capture_payload_files(paths: impl IntoIterator<Item = PathBuf>) {
    for path in paths {
        if let Err(error) = remove_capture_payload_file(&path) {
            eprintln!(
                "failed to remove capture payload '{}': {error}",
                path.display()
            );
        }
    }
}

fn remove_capture_payload_file(path: &Path) -> std::io::Result<()> {
    match fs::remove_file(path) {
        Ok(()) => Ok(()),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(error),
    }
}

fn heap_capture_paths_for(
    persistence_path: &Path,
    reservation: &CaptureReservation,
) -> (PathBuf, PathBuf) {
    capture_payload_paths_for(persistence_path, &reservation.metadata)
}

fn capture_payload_paths_for(
    persistence_path: &Path,
    metadata: &CaptureSnapshot,
) -> (PathBuf, PathBuf) {
    let directory = persistence_path
        .with_extension("captures")
        .join(format!("{:016x}", stable_name_hash(&metadata.context_id)));
    let suffix = match metadata.kind {
        CaptureKind::Coverage => "coverage.json",
        CaptureKind::CpuProfile => "cpuprofile.json",
        CaptureKind::HeapSnapshot => "heapsnapshot",
    };
    let final_path = directory.join(format!("{}.{}", metadata.storage_id, suffix));
    let staging_path = directory.join(format!("{}.{}.partial", metadata.storage_id, suffix));
    (staging_path, final_path)
}

fn scavenge_capture_storage(persistence_path: &Path, state: &ServiceState) {
    let capture_root = persistence_path.with_extension("captures");
    let mut referenced = state
        .captures
        .values()
        .map(StoredCapture::payload_path)
        .map(|path| storage_path_key(&path))
        .collect::<BTreeSet<_>>();
    for reservation in state.capture_reservations.values() {
        let (staging, final_path) = heap_capture_paths_for(persistence_path, reservation);
        referenced.insert(storage_path_key(&staging));
        referenced.insert(storage_path_key(&final_path));
        if let Some(completed) = &reservation.completed {
            referenced.insert(storage_path_key(Path::new(&completed.payload.path)));
        }
    }
    scavenge_capture_directory(&capture_root, &referenced);
}

fn scavenge_capture_directory(directory: &Path, referenced: &BTreeSet<PathBuf>) {
    match fs::symlink_metadata(directory) {
        Ok(metadata) if metadata.file_type().is_symlink() => return,
        Ok(_) => {}
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return,
        Err(error) => {
            eprintln!(
                "failed to inspect capture storage '{}': {error}",
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
                "failed to inspect capture storage '{}': {error}",
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
                    "failed to inspect an entry in capture storage '{}': {error}",
                    directory.display()
                );
                continue;
            }
        };
        let path = entry.path();
        let name = path
            .file_name()
            .and_then(|name| name.to_str())
            .unwrap_or("");
        let is_capture_storage = name.ends_with(".partial")
            || name.ends_with(".heapsnapshot")
            || name.ends_with(".coverage.json")
            || name.ends_with(".cpuprofile.json");
        let file_type = match entry.file_type() {
            Ok(file_type) => file_type,
            Err(error) => {
                eprintln!(
                    "failed to inspect capture storage '{}': {error}",
                    path.display()
                );
                continue;
            }
        };
        if file_type.is_dir() {
            scavenge_capture_directory(&path, referenced);
            if is_capture_storage
                && !referenced.contains(&storage_path_key(&path))
                && let Err(error) = fs::remove_dir(&path)
            {
                eprintln!(
                    "failed to remove orphan capture storage '{}': {error}",
                    path.display()
                );
            }
            continue;
        }
        if is_capture_storage && !referenced.contains(&storage_path_key(&path)) {
            remove_capture_payload_files([path]);
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

impl DebuggerService {
    async fn update_source_formatting(
        &self,
        context_id: &str,
        command: UserCommand,
    ) -> Result<SourceFormattingSettings, JsonRpcError> {
        let mut state = self.state.lock().await;
        let previous = state.clone();
        let context = state
            .contexts
            .get(context_id)
            .cloned()
            .ok_or_else(|| not_found("context", context_id))?;
        let transition = reduce_context(&context, ContextInput::UserCommand(command))
            .map_err(transition_rpc_error)?;
        let result = self.commit_context(&mut state, context_id, transition);
        self.persist_or_restore(&mut state, previous)?;
        Ok(result.source_formatting)
    }
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

    async fn get_process_projection(
        &self,
        _ctx: &CallCtx,
        context_id: String,
        expanded_root_process_ids: Vec<u32>,
    ) -> Result<Vec<ProcessTreeSnapshot>, JsonRpcError> {
        let expanded = expanded_root_process_ids
            .into_iter()
            .collect::<BTreeSet<_>>();
        let mut trees = crate::process_discovery::discover_recognized_process_trees()
            .await
            .map_err(|error| internal_error(error.to_string()))?;
        let mut covered_processes = BTreeSet::new();
        trees.retain(|tree| {
            if covered_processes.contains(&tree.root_process_id) {
                return false;
            }
            covered_processes.extend(tree.processes.iter().map(|process| process.process_id));
            true
        });
        for tree in &mut trees {
            if expanded.contains(&tree.root_process_id) {
                crate::process_discovery::populate_process_tree_targets(std::slice::from_mut(tree))
                    .await;
            }
        }
        let mut state = self.state.lock().await;
        if !state.contexts.contains_key(&context_id) {
            return Err(not_found("context", &context_id));
        }
        if state.process_projections.get(&context_id) != Some(&trees) {
            stage_process_resource_graph(&mut state, &context_id, &trees)
                .map_err(internal_error)?;
            state.process_projections.insert(context_id, trees.clone());
        }
        Ok(trees)
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

    async fn get_resource_graph(
        &self,
        _ctx: &CallCtx,
        context_id: String,
    ) -> Result<ResourceGraphSnapshot, JsonRpcError> {
        let state = self.state.lock().await;
        if !state.contexts.contains_key(&context_id) {
            return Err(not_found("context", &context_id));
        }
        let snapshot = state
            .resource_graphs
            .get(&context_id)
            .map(GraphSink::snapshot)
            .unwrap_or_else(|| ResourceGraph::default().snapshot());
        Ok(resource_graph_api_snapshot(snapshot))
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
        let (runtimes, capture_paths, proxy_cancellations, relay_cancellations) = {
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
            let mut capture_paths = state
                .captures
                .iter()
                .filter(|((candidate_context, _), _)| candidate_context == &context_id)
                .map(|(_, capture)| capture.payload_path())
                .collect::<Vec<_>>();
            for reservation in state
                .capture_reservations
                .iter()
                .filter(|((candidate_context, _), _)| candidate_context == &context_id)
                .map(|(_, reservation)| reservation)
            {
                let (staging, final_path) = self.heap_capture_paths(reservation);
                capture_paths.extend([staging, final_path]);
                if let Some(completed) = &reservation.completed {
                    capture_paths.push(completed.payload.path.as_str().into());
                }
            }
            let previous = state.clone();
            if state.contexts.remove(&context_id).is_none() {
                return Err(not_found("context", &context_id));
            }
            state.resource_graphs.remove(&context_id);
            state.process_projections.remove(&context_id);
            state.context_kinds.remove(&context_id);
            self.complete_request(&mut state, &context_id, &options, 0);
            state.history.remove(&context_id);
            state.source_models.remove(&context_id);
            state
                .target_debuggers
                .retain(|(candidate_context, _, _), _| candidate_context != &context_id);
            state
                .debug_attachments
                .retain(|(candidate_context, _, _), _| candidate_context != &context_id);
            state
                .pause_children_leases
                .retain(|(candidate_context, _), _| candidate_context != &context_id);
            let proxy_cancellations = state
                .playwright_proxies
                .values()
                .filter(|proxy| proxy.context_id == context_id)
                .map(|proxy| proxy.cancel.clone())
                .collect::<Vec<_>>();
            state
                .playwright_proxies
                .retain(|_, proxy| proxy.context_id != context_id);
            let relay_cancellations = state
                .relays
                .values()
                .filter(|relay| relay.context_id == context_id)
                .map(|relay| relay.cancel.clone())
                .collect::<Vec<_>>();
            state
                .relays
                .retain(|_, relay| relay.context_id != context_id);
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
            (
                runtimes,
                capture_paths,
                proxy_cancellations,
                relay_cancellations,
            )
        };
        for cancellation in proxy_cancellations {
            let _ = cancellation.send(true);
        }
        for cancellation in relay_cancellations {
            let _ = cancellation.send(true);
        }
        for runtime in runtimes {
            runtime.close().await;
        }
        remove_capture_payload_files(capture_paths);
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
        let (completion, runtime, targets) = match connected {
            Ok((runtime, product, protocol_version, targets)) => (
                EffectCompletion::ConnectionOpened {
                    connection_id: connection_id.clone(),
                    attempt,
                    product,
                    protocol_version,
                },
                Some(runtime),
                targets
                    .into_iter()
                    .map(|target| (target.target_id.clone(), target))
                    .collect::<BTreeMap<_, _>>(),
            ),
            Err(message) => (
                EffectCompletion::ConnectionOpenFailed {
                    connection_id: connection_id.clone(),
                    attempt,
                    message,
                },
                None,
                BTreeMap::new(),
            ),
        };
        let targets = filter_target_snapshot_scope(Some(&configuration), targets);
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
        if let Some(runtime) = &runtime {
            if let Err(error) = stage_connection_resource_graph(
                &mut state,
                &context_id,
                &connection_id,
                &transition.state,
                &targets,
                runtime,
                &[],
            ) {
                let failure = reduce_context(
                    &context,
                    ContextInput::EffectCompletion(EffectCompletion::ConnectionOpenFailed {
                        connection_id: connection_id.clone(),
                        attempt,
                        message: format!("failed to publish resource graph: {error}"),
                    }),
                )
                .map_err(transition_rpc_error)?;
                self.commit_context(&mut state, &context_id, failure);
                let runtime = runtime.clone();
                drop(state);
                runtime.close().await;
                return Err(internal_error(format!(
                    "failed to publish resource graph: {error}"
                )));
            }
        }
        let auto_attach_targets = if runtime
            .as_ref()
            .is_some_and(|runtime| runtime.is_direct_debugger() || runtime.is_virtual_root())
        {
            Vec::new()
        } else {
            targets
                .values()
                .filter(|target| matches!(target.target_type.as_str(), "page" | "node"))
                .map(|target| target.target_id.clone())
                .collect::<Vec<_>>()
        };
        let result = self.commit_context(&mut state, &context_id, transition);
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
                .attach_target_internal(
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
                    return Ok(
                        service_snapshot(&state, &self.agent_instance_id, &context_id)
                            .expect("context was checked above"),
                    );
                }
                [ContextEffect::Disconnect { attempt, .. }] => *attempt,
                effects => panic!("disconnect command emitted unexpected effects: {effects:?}"),
            };
            self.commit_context(&mut state, &context_id, transition);
            let runtime = state
                .runtimes
                .remove(&(context_id.clone(), connection_id.clone()));
            retract_connection_resource_graph(
                &mut state,
                &context_id,
                &connection_id,
                attempt.generation,
            );
            remove_connection_debugger_registrations(&mut state, &context_id, &connection_id);
            state
                .pause_children_leases
                .remove(&(context_id.clone(), connection_id.clone()));
            cancel_playwright_proxies(
                &mut state,
                &context_id,
                &connection_id,
                Some(attempt.generation),
            );
            (runtime, attempt)
        };

        self.release_relay_attachments_for_connection(&context_id, &connection_id)
            .await;
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

    async fn set_pause_future_children(
        &self,
        _ctx: &CallCtx,
        context_id: String,
        connection_id: String,
        enabled: bool,
    ) -> Result<bool, JsonRpcError> {
        let key = (context_id.clone(), connection_id.clone());
        if !enabled {
            self.state.lock().await.pause_children_leases.remove(&key);
            return Ok(false);
        }
        let (generation, capability) = {
            let state = self.state.lock().await;
            if state.pause_children_leases.contains_key(&key) {
                return Ok(true);
            }
            let context = state
                .contexts
                .get(&context_id)
                .ok_or_else(|| not_found("context", &context_id))?;
            let connection = context
                .connections
                .get(&connection_id)
                .ok_or_else(|| not_found("connection", &connection_id))?;
            let root = connection_root_resource_id(&connection_id, connection.generation);
            let source = connection_source_id(&connection_id, connection.generation);
            let capability = state
                .resource_graphs
                .get(&context_id)
                .and_then(|graph| {
                    graph.read(|graph| {
                        graph.capability_from_source(
                            &root,
                            &source,
                            &CapabilityKind::PauseFutureChildren,
                        )
                    })
                })
                .and_then(|capability| capability.as_pause_future_children())
                .ok_or_else(|| {
                    invalid_state(&format!(
                        "connection '{connection_id}' cannot pause future child targets"
                    ))
                })?;
            (connection.generation, capability)
        };
        let lease = capability
            .arm()
            .await
            .map_err(|error| invalid_state(&error.to_string()))?;
        let mut state = self.state.lock().await;
        let still_current = state
            .contexts
            .get(&context_id)
            .and_then(|context| context.connections.get(&connection_id))
            .is_some_and(|connection| connection.generation == generation);
        if !still_current {
            drop(lease);
            return Err(invalid_state(
                "connection changed while pause-on-start was being armed",
            ));
        }
        state.pause_children_leases.insert(key, lease);
        Ok(true)
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
        remove_connection_debugger_registrations(&mut state, &context_id, &connection_id);
        state
            .pause_children_leases
            .remove(&(context_id.clone(), connection_id.clone()));
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
            let applies_to_target = breakpoint_applies_to_target(
                specification.enabled,
                specification.target_selector.as_deref(),
                &target_id,
            );
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

    async fn set_source_formatting(
        &self,
        _ctx: &CallCtx,
        context_id: String,
        mode: SourceFormattingMode,
    ) -> Result<SourceFormattingSettings, JsonRpcError> {
        self.update_source_formatting(&context_id, UserCommand::SetSourceFormatting { mode })
            .await
    }

    async fn add_source_formatting_rule(
        &self,
        _ctx: &CallCtx,
        context_id: String,
        mode: SourceFormattingMode,
        target_pattern: Option<String>,
        url_pattern: Option<String>,
    ) -> Result<SourceFormattingSettings, JsonRpcError> {
        if target_pattern.is_none() && url_pattern.is_none() {
            return Err(invalid_params(
                "a formatting rule requires --target, --url, or both",
            ));
        }
        validate_formatting_pattern(target_pattern.as_deref())?;
        validate_formatting_pattern(url_pattern.as_deref())?;
        let mut state = self.state.lock().await;
        let previous = state.clone();
        let context = state
            .contexts
            .get(&context_id)
            .cloned()
            .ok_or_else(|| not_found("context", &context_id))?;
        let mut index = 1_u64;
        let rule_id = loop {
            let candidate = format!("fmt-{index}");
            if context
                .source_formatting
                .rules
                .iter()
                .all(|rule| rule.id != candidate)
            {
                break candidate;
            }
            index = index.saturating_add(1);
        };
        let transition = reduce_context(
            &context,
            ContextInput::UserCommand(UserCommand::AddSourceFormattingRule {
                rule: SourceFormattingRule {
                    id: rule_id,
                    mode,
                    target_pattern,
                    url_pattern,
                },
            }),
        )
        .map_err(transition_rpc_error)?;
        let result = self.commit_context(&mut state, &context_id, transition);
        self.persist_or_restore(&mut state, previous)?;
        Ok(result.source_formatting)
    }

    async fn delete_source_formatting_rule(
        &self,
        _ctx: &CallCtx,
        context_id: String,
        rule_id: String,
    ) -> Result<SourceFormattingSettings, JsonRpcError> {
        self.update_source_formatting(
            &context_id,
            UserCommand::RemoveSourceFormattingRule { rule_id },
        )
        .await
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
                        status: match script.status {
                            crate::service_api::TargetScriptStatus::Unresolved => "loaded",
                            crate::service_api::TargetScriptStatus::Pending => "loading",
                            crate::service_api::TargetScriptStatus::Resolved { .. } => "resolved",
                            crate::service_api::TargetScriptStatus::Failed { .. } => "failed",
                        }
                        .into(),
                        connection_id: Some(connection_id.clone()),
                        target_id: Some(target_id.clone()),
                        source_map_url: script.source_map_url,
                    },
                );
                for authored in authored_sources {
                    let kind = if authored.ends_with("?formatted") {
                        "formatted"
                    } else {
                        "authored"
                    };
                    sources.insert(
                        (authored.clone(), connection_id.clone(), target_id.clone()),
                        SourceSnapshotInfo {
                            path: authored,
                            kind: kind.into(),
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
                    .and_modify(|source| {
                        source.kind = kind.clone();
                        source.status = "resolved".to_owned();
                    })
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
        let (model, debuggers) = {
            let state = self.state.lock().await;
            if !state.contexts.contains_key(&context_id) {
                return Err(not_found("context", &context_id));
            }
            (
                state.source_models.get(&context_id).cloned(),
                state
                    .target_debuggers
                    .iter()
                    .filter(|((candidate_context, _, _), _)| candidate_context == &context_id)
                    .map(|(_, debugger)| debugger.clone())
                    .collect::<Vec<_>>(),
            )
        };
        if matches!(
            kind,
            SourceTreeKind::SourceMapped | SourceTreeKind::Formatted | SourceTreeKind::Resolved
        ) {
            let include_unmapped = kind != SourceTreeKind::SourceMapped;
            for result in join_all(
                debuggers
                    .iter()
                    .map(|debugger| debugger.hydrate_sources(include_unmapped)),
            )
            .await
            {
                if let Err(error) = result
                    && !matches!(error, TargetDebuggerError::Stopped)
                {
                    return Err(target_debugger_rpc_error(error));
                }
            }
        }
        let sources = model.map_or_else(Vec::new, |model| match kind {
            SourceTreeKind::Loaded => model.loaded_sources(),
            SourceTreeKind::SourceMapped => model.source_mapped_loaded_sources(),
            SourceTreeKind::Formatted => model.formatted_loaded_sources(),
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
        let selection = model
            .resolve_sources(&source)
            .map_err(|error| invalid_params(error.to_string()))?;
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
        let (debuggers, formatting) = {
            let state = self.state.lock().await;
            let context = state
                .contexts
                .get(&context_id)
                .ok_or_else(|| not_found("context", &context_id))?;
            let debuggers = state
                .target_debuggers
                .iter()
                .filter(|((candidate_context, _, _), _)| candidate_context == &context_id)
                .map(|((_, _, target_id), debugger)| (target_id.clone(), debugger.clone()))
                .collect::<Vec<_>>();
            (
                debuggers,
                compile_formatting_settings(&context.source_formatting)?,
            )
        };
        let mut original_fallback = None;
        for (target_id, debugger) in debuggers {
            let base_path = path.strip_suffix("?formatted").unwrap_or(&path);
            if options.view == SourceViewPreference::Formatted {
                if let Some(content) = debugger
                    .source_content(format!("{base_path}?formatted"))
                    .await
                    .map_err(target_debugger_rpc_error)?
                {
                    return source_content_range(content, &options);
                }
                continue;
            }
            let original = debugger
                .source_content(base_path.to_owned())
                .await
                .map_err(target_debugger_rpc_error)?;
            let selected_path = match options.view {
                SourceViewPreference::Original => base_path.to_owned(),
                SourceViewPreference::Formatted => unreachable!(),
                SourceViewPreference::Policy if path.ends_with("?formatted") => path.clone(),
                SourceViewPreference::Policy => {
                    let mode = effective_formatting_mode(&formatting, &target_id, base_path);
                    if mode == SourceFormattingMode::On
                        || mode == SourceFormattingMode::Auto
                            && original
                                .as_ref()
                                .is_some_and(|source| appears_minified(base_path, &source.content))
                    {
                        format!("{base_path}?formatted")
                    } else {
                        base_path.to_owned()
                    }
                }
            };
            if selected_path == base_path {
                if let Some(content) = original {
                    return source_content_range(content, &options);
                }
                continue;
            }
            if let Some(content) = debugger
                .source_content(selected_path)
                .await
                .map_err(target_debugger_rpc_error)?
            {
                return source_content_range(content, &options);
            }
            if options.view == SourceViewPreference::Policy && original_fallback.is_none() {
                original_fallback = original;
            }
        }
        if let Some(content) = original_fallback {
            return source_content_range(content, &options);
        }
        if options.view == SourceViewPreference::Formatted {
            return Err(not_found("formatted source", &path));
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
        let (debuggers, local_sources, formatting) = {
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
            (
                debuggers,
                local_sources,
                compile_formatting_settings(&context.source_formatting)?,
            )
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
        let mut skipped = Vec::new();
        for (connection_id, target_id, mut batch) in batches {
            debug_assert_eq!(batch.skipped_sources as usize, batch.skipped.len());
            skipped.extend(batch.skipped.into_iter().map(|mut source| {
                source.connection_id = Some(connection_id.clone());
                source.target_id = Some(target_id.clone());
                source
            }));
            select_source_views(&mut batch.sources, &formatting, &target_id, options.view);
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
            let mut skipped_local = Vec::new();
            for path in local_sources {
                worker_control.check()?;
                let content = match source_file_path(&path)
                    .map_err(|error| error.message)
                    .and_then(|file_path| fs::read_to_string(file_path).map_err(|error| error.to_string()))
                {
                    Ok(content) => Arc::<str>::from(content),
                    Err(reason) => {
                        skipped_local.push(crate::service_api::SourceSearchSkip {
                            path,
                            kind: "intent".to_owned(),
                            connection_id: None,
                            target_id: None,
                            reason,
                        });
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
        skipped.extend(skipped_local);
        let skipped_sources = skipped.len().min(u32::MAX as usize) as u32;
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
            skipped_sources,
            skipped,
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
                        view: SourceViewPreference::Policy,
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
        Ok(select_stored_capture(&state, &context_id, &capture_name, None, None, None)?
            .metadata.clone())
    }

    async fn delete_capture(
        &self,
        _ctx: &CallCtx,
        context_id: String,
        capture_name: String,
    ) -> Result<bool, JsonRpcError> {
        let mut state = self.state.lock().await;
        let key = (context_id, capture_name.clone());
        let (capture, completed_reservation) =
            if let Some(capture) = state.captures.get(&key).cloned() {
                (capture, false)
            } else if let Some(reservation) = state.capture_reservations.get(&key) {
                if reservation.deleting {
                    return Err(invalid_state(&format!(
                        "capture '{capture_name}' is currently being deleted from context '{}'",
                        key.0
                    )));
                }
                let Some(completed) = &reservation.completed else {
                    return Err(invalid_state(&format!(
                        "capture '{capture_name}' is currently being stored in context '{}'",
                        key.0
                    )));
                };
                (
                    StoredCapture {
                        metadata: reservation.metadata.clone(),
                        payload: completed.payload.clone(),
                        publication_order: 0,
                        heap_mapping: completed.heap_result.as_ref().and_then(|result| result.mapping.clone()),
                    },
                    true,
                )
            } else {
                return Err(not_found("capture", &capture_name));
            };
        let storage_id = capture.metadata.storage_id.clone();
        let debugger = state
            .target_debuggers
            .get(&(
                capture.metadata.context_id.clone(),
                capture.metadata.connection_id.clone(),
                capture.metadata.target_id.clone(),
            ))
            .filter(|debugger| {
                debugger.snapshot().connection_generation == capture.metadata.connection_generation
            })
            .cloned();
        if completed_reservation {
            state
                .capture_reservations
                .get_mut(&key)
                .expect("completed capture reservation disappeared while locked")
                .deleting = true;
            drop(state);
        } else {
            let previous = state.clone();
            state.captures.remove(&key);
            self.persist_or_restore(&mut state, previous)?;
            drop(state);
        }
        let mut deletion_guard = completed_reservation
            .then(|| CaptureDeletionGuard::new(self.clone(), key.clone(), storage_id.clone()));
        if let Some(debugger) = debugger {
            if let Err(error) = debugger.delete_stored_capture(capture_name.clone()).await {
                if let Some(guard) = &mut deletion_guard {
                    guard.restore().await;
                }
                return Err(target_debugger_rpc_error(error));
            }
        }
        let payload_path = capture.payload_path();
        if let Err(error) = self.capture_storage.remove_for_delete(&payload_path).await {
            if completed_reservation {
                deletion_guard
                    .as_mut()
                    .expect("completed capture deletion guard is missing")
                    .restore()
                    .await;
                return Err(internal_error(format!(
                    "failed to discard completed capture '{capture_name}'; its payload '{}' and retryable reservation were retained: {error}",
                    payload_path.display()
                )));
            }
            return Err(internal_error(format!(
                "capture '{capture_name}' was removed from the catalog but its payload '{}' could not be deleted and will be retried during startup cleanup: {error}",
                payload_path.display()
            )));
        }
        if completed_reservation {
            let mut state = self.state.lock().await;
            if state.capture_reservations.get(&key).is_some_and(|current| {
                current.metadata.storage_id == storage_id && current.completed.is_some()
            }) {
                state.capture_reservations.remove(&key);
            }
            deletion_guard
                .as_mut()
                .expect("completed capture deletion guard is missing")
                .disarm();
        }
        Ok(true)
    }

    async fn get_stored_coverage(
        &self,
        _ctx: &CallCtx,
        context_id: String,
        capture_name: String,
        source_path: Option<String>,
        target_id: Option<String>,
        connection_id: Option<String>,
        path_glob: Option<String>,
    ) -> Result<CoverageSnapshot, JsonRpcError> {
        let (payload, name) = {
            let state = self.state.lock().await;
            let capture = select_stored_capture(
                &state, &context_id, &capture_name, Some(CaptureKind::Coverage),
                target_id.as_deref(), connection_id.as_deref(),
            )?;
            (capture.payload.clone(), capture.metadata.name.clone())
        };
        let CapturePayload::Coverage(mut snapshot) =
            load_capture_payload(&payload, CaptureKind::Coverage)
                .map_err(capture_payload_rpc_error)?
        else {
            return Err(invalid_params(&format!(
                "capture '{capture_name}' is not a coverage capture"
            )));
        };
        snapshot.capture_id = Some(name);
        crate::coverage_filter::filter_coverage(
            &mut snapshot, source_path.as_deref(), path_glob.as_deref(),
        ).map_err(invalid_params)?;
        Ok(snapshot)
    }

    async fn get_stored_cpu_profile(
        &self,
        _ctx: &CallCtx,
        context_id: String,
        capture_name: String,
        _source_path: Option<String>,
        target_id: Option<String>,
        connection_id: Option<String>,
    ) -> Result<CpuProfileSnapshot, JsonRpcError> {
        let (payload, name) = {
            let state = self.state.lock().await;
            let capture = select_stored_capture(
                &state, &context_id, &capture_name, Some(CaptureKind::CpuProfile),
                target_id.as_deref(), connection_id.as_deref(),
            )?;
            (capture.payload.clone(), capture.metadata.name.clone())
        };
        let CapturePayload::CpuProfile(mut snapshot) =
            load_capture_payload(&payload, CaptureKind::CpuProfile)
                .map_err(capture_payload_rpc_error)?
        else {
            return Err(invalid_params(&format!(
                "capture '{capture_name}' is not a CPU profile capture"
            )));
        };
        snapshot.capture_id = name;
        if snapshot.functions.is_empty() && !snapshot.nodes.is_empty() {
            crate::target_debugger::aggregate_cpu_profile(&mut snapshot)
                .map_err(target_debugger_rpc_error)?;
        }
        Ok(snapshot)
    }

    async fn get_stored_heap_classes(
        &self,
        _ctx: &CallCtx,
        context_id: String,
        capture_name: String,
        filter: Option<String>,
        target_id: Option<String>,
        connection_id: Option<String>,
    ) -> Result<HeapClassSnapshot, JsonRpcError> {
        let (payload, mapping, capture_name) = {
            let state = self.state.lock().await;
            let capture = select_stored_capture(
                &state, &context_id, &capture_name, Some(CaptureKind::HeapSnapshot),
                target_id.as_deref(), connection_id.as_deref(),
            )?;
            (capture.payload.clone(), capture.heap_mapping.clone(), capture.metadata.name.clone())
        };
        let CapturePayload::HeapSnapshot { path } =
            load_capture_payload(&payload, CaptureKind::HeapSnapshot)
                .map_err(capture_payload_rpc_error)?
        else {
            return Err(invalid_state(
                "stored heap payload kind does not match metadata",
            ));
        };
        let capture_for_task = capture_name.clone();
        tokio::task::spawn_blocking(move || {
            stored_heap_classes(Path::new(&path), capture_for_task, filter.as_deref(), mapping.as_ref())
        })
        .await
        .map_err(|error| internal_error(error.to_string()))?
        .map_err(target_debugger_rpc_error)
    }

    async fn supply_stored_heap_source_map(
        &self,
        _ctx: &CallCtx,
        context_id: String,
        capture_name: String,
        supply: crate::service_api::HeapSourceMapSupply,
    ) -> Result<(), JsonRpcError> {
        let mut state = self.state.lock().await;
        let previous = state.clone();
        let capture_name = select_stored_capture(
            &state, &context_id, &capture_name, Some(CaptureKind::HeapSnapshot), None, None,
        )?.metadata.name.clone();
        let capture = state.captures.get_mut(&(context_id, capture_name.clone()))
            .ok_or_else(|| not_found("capture", &capture_name))?;
        if capture.metadata.kind != CaptureKind::HeapSnapshot {
            return Err(invalid_params("capture is not a heap snapshot"));
        }
        let mapping = capture.heap_mapping.as_mut().ok_or_else(||
            invalid_state("legacy capture has no captured script hashes; cannot safely supply a source map"))?;
        crate::target_debugger::supply_heap_source_map(mapping, supply)
            .map_err(target_debugger_rpc_error)?;
        self.persist_or_restore(&mut state, previous)?;
        Ok(())
    }

    async fn attach_target(
        &self,
        ctx: &CallCtx,
        context_id: String,
        connection_id: String,
        target_id: String,
        options: TargetAttachOptions,
    ) -> Result<TargetAttachmentResult, JsonRpcError> {
        let _relay_lifecycle_guard = self.relay_lifecycle_lock.lock().await;
        ensure_context_not_relayed(&*self.state.lock().await, &context_id)?;
        self.attach_target_internal(ctx, context_id, connection_id, target_id, options)
            .await
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

    async fn get_logs(
        &self,
        _ctx: &CallCtx,
        context_id: String,
        connection_id: String,
        target_id: String,
    ) -> Result<crate::service_api::TargetLogSnapshot, JsonRpcError> {
        use crate::service_api::{LogCaptureSnapshot, LogCaptureStatus, TargetLogSnapshot};
        let state = self.state.lock().await;
        ensure_context_not_relayed(&state, &context_id)?;
        let target_id =
            Self::resolve_target_id_in_state(&state, &context_id, &connection_id, &target_id)?;
        let context = state
            .contexts
            .get(&context_id)
            .ok_or_else(|| not_found("context", &context_id))?;
        let connection = context
            .connections
            .get(&connection_id)
            .ok_or_else(|| not_found("connection", &connection_id))?;
        if !context_connection_has_target(
            &state,
            &context_id,
            &connection_id,
            connection.generation,
            &target_id,
        ) {
            return Err(not_found("target", &target_id));
        }
        let snapshot = state
            .target_debuggers
            .get(&(context_id.clone(), connection_id.clone(), target_id.clone()))
            .map(TargetDebuggerHandle::snapshot);
        Ok(TargetLogSnapshot {
            context_id,
            connection_id,
            target_id,
            connection_generation: connection.generation,
            capture: snapshot.as_ref().map_or_else(
                || LogCaptureSnapshot {
                    status: LogCaptureStatus::Inactive,
                    ..Default::default()
                },
                |snapshot| snapshot.log_capture.clone(),
            ),
            messages: snapshot.map_or_else(Vec::new, |snapshot| snapshot.logs),
        })
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

    async fn detach_target(
        &self,
        ctx: &CallCtx,
        context_id: String,
        connection_id: String,
        target_id: String,
        expected_connection_generation: Option<u64>,
    ) -> Result<ContextSnapshot, JsonRpcError> {
        let _relay_lifecycle_guard = self.relay_lifecycle_lock.lock().await;
        ensure_context_not_relayed(&*self.state.lock().await, &context_id)?;
        let _attachment_guard = self.attachment_lock.lock().await;
        let (key, debugger, runtime, attachment, attempt) = {
            let state = self.state.lock().await;
            let target_id =
                Self::resolve_target_id_in_state(&state, &context_id, &connection_id, &target_id)?;
            let key = (context_id.clone(), connection_id.clone(), target_id.clone());
            let context = state
                .contexts
                .get(&context_id)
                .ok_or_else(|| not_found("context", &context_id))?;
            let connection = context
                .connections
                .get(&connection_id)
                .ok_or_else(|| not_found("connection", &connection_id))?;
            if expected_connection_generation
                .is_some_and(|expected| expected != connection.generation)
            {
                return Err(invalid_state(
                    "connection changed before the target could be detached",
                ));
            }
            let debugger = state
                .target_debuggers
                .get(&key)
                .cloned()
                .ok_or_else(|| not_found("attached target", &target_id))?;
            let runtime = state
                .runtimes
                .get(&(context_id.clone(), connection_id.clone()))
                .cloned()
                .ok_or_else(|| invalid_state("connection is not connected"))?;
            (
                key.clone(),
                debugger,
                runtime,
                state.debug_attachments.get(&key).cloned(),
                ConnectionAttempt {
                    configuration_version: connection.configuration_version,
                    generation: connection.generation,
                },
            )
        };

        if runtime.is_direct_debugger() {
            return self
                .disconnect_connection(ctx, context_id, connection_id)
                .await;
        }
        let close_error = if let Some(attachment) = attachment {
            attachment
                .capability
                .close(&attachment.handle)
                .await
                .err()
                .map(|error| error.to_string())
        } else {
            detach_session(&runtime, debugger.session_id()).await;
            None
        };

        {
            let mut state = self.state.lock().await;
            if state
                .target_debuggers
                .get(&key)
                .is_some_and(|current| current.same_instance(&debugger))
            {
                remove_debugger_registration(&mut state, &key);
            } else if state.target_debuggers.contains_key(&key) {
                return Err(invalid_state(
                    "target attachment changed while detach was pending",
                ));
            }
        }
        self.publish_target_attachment_change(&key, attempt).await;

        if let Some(error) = close_error {
            return Err(internal_error(error));
        }
        let state = self.state.lock().await;
        service_snapshot(&state, &self.agent_instance_id, &context_id)
            .ok_or_else(|| not_found("context", &context_id))
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

    async fn raw_cdp_session_request(
        &self,
        _ctx: &CallCtx,
        context_id: String,
        connection_id: String,
        target_id: String,
        session_id: String,
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
        let runtime = self
            .state
            .lock()
            .await
            .runtimes
            .get(&(context_id.clone(), connection_id.clone()))
            .cloned()
            .ok_or_else(|| invalid_state("connection is not connected"))?;
        let session = runtime
            .open_session(SessionKey {
                connection_generation: identity.connection_generation,
                session_id,
            })
            .map_err(|error| invalid_state(&error.to_string()))?;
        let result = session.raw_request(&method, params).await;
        let is_current = self
            .state
            .lock()
            .await
            .target_debuggers
            .get(&(context_id, connection_id, target_id))
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
        let (target_id, runtime, browser_context_id) = {
            let state = self.state.lock().await;
            let target_id =
                Self::resolve_target_id_in_state(&state, &context_id, &connection_id, &target_id)?;
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
            let target = context_connection_target(
                &state,
                &context_id,
                &connection_id,
                connection.generation,
                &target_id,
            )
            .ok_or_else(|| not_found("target", &target_id))?;
            if target.target.target_type != "page" {
                return Err(invalid_params(format!(
                    "Playwright requires a page target, but '{target_id}' has type '{}'",
                    target.target.target_type
                )));
            }
            let runtime = state
                .runtimes
                .get(&(context_id.clone(), connection_id.clone()))
                .cloned()
                .ok_or_else(|| invalid_state("connection is not connected"))?;
            (target_id, runtime, target.target.browser_context_id)
        };

        let id = random_instance_id().map_err(|error| internal_error(error.to_string()))?;
        let relay = crate::context_relay::start_connection_relay(
            self.clone(),
            context_id.clone(),
            connection_id.clone(),
            format!("{id}-relay"),
        )
        .await
        .map_err(|error| internal_error(error.to_string()))?;
        let proxy = match crate::playwright_proxy::start(
            crate::playwright_proxy::PlaywrightCdpSource::BrowserRoot {
                endpoint: relay.websocket_url.clone(),
            },
            crate::playwright_proxy::PlaywrightPageScope {
                target_id: target_id.clone(),
                browser_context_id,
            },
            id.clone(),
        )
        .await
        {
            Ok(proxy) => proxy,
            Err(error) => {
                relay.cancel.send_replace(true);
                return Err(internal_error(error.to_string()));
            }
        };
        let crate::context_relay::RelaySession {
            cancel: relay_cancel,
            completion: relay_completion,
            ..
        } = relay;
        let crate::playwright_proxy::PlaywrightProxy {
            websocket_url,
            cancel,
            completion,
        } = proxy;
        let mut cancellation = cancel.subscribe();
        let relay_cancellation = relay_cancel.clone();
        tokio::spawn(async move {
            if cancellation.changed().await.is_ok() && *cancellation.borrow() {
                relay_cancellation.send_replace(true);
            }
        });
        let (closed_sender, closed_receiver) = watch::channel(false);
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
                    .is_some_and(|connection| connection.generation == expected_generation)
                && context_connection_target(
                    &state,
                    &context_id,
                    &connection_id,
                    expected_generation,
                    &target_id,
                )
                .is_some();
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
                    closed: closed_receiver,
                },
            );
        }
        let service = self.clone();
        let cleanup_id = id.clone();
        tokio::spawn(async move {
            let _ = completion.await;
            relay_cancel.send_replace(true);
            let _ = relay_completion.await;
            closed_sender.send_replace(true);
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
        if let Some(mut registration) = registration {
            registration.cancel.send_replace(true);
            while !*registration.closed.borrow_and_update() {
                if registration.closed.changed().await.is_err() {
                    break;
                }
            }
            Ok(true)
        } else {
            Ok(false)
        }
    }

    async fn open_context_relay(
        &self,
        _ctx: &CallCtx,
        context_id: String,
    ) -> Result<RelayEndpoint, JsonRpcError> {
        let relay_lifecycle_guard = self.relay_lifecycle_lock.lock().await;
        {
            let state = self.state.lock().await;
            if !state.contexts.contains_key(&context_id) {
                return Err(not_found("context", &context_id));
            }
            ensure_context_not_relayed(&state, &context_id)?;
        }
        let id = random_instance_id().map_err(|error| internal_error(error.to_string()))?;
        let relay =
            crate::context_relay::start_context_relay(self.clone(), context_id.clone(), id.clone())
                .await
                .map_err(|error| internal_error(error.to_string()))?;
        self.register_relay(&relay_lifecycle_guard, id, context_id, relay)
            .await
    }

    async fn open_target_relay(
        &self,
        ctx: &CallCtx,
        context_id: String,
        connection_id: String,
        target_id: String,
    ) -> Result<RelayEndpoint, JsonRpcError> {
        let relay_lifecycle_guard = self.relay_lifecycle_lock.lock().await;
        {
            let state = self.state.lock().await;
            if !state.contexts.contains_key(&context_id) {
                return Err(not_found("context", &context_id));
            }
            ensure_context_not_relayed(&state, &context_id)?;
        }
        // Attach eagerly (bypassing the guard we are about to install) so an unresolvable
        // target or a provider failure surfaces synchronously, before any listener is bound.
        let attachment = self
            .attach_target_internal(
                ctx,
                context_id.clone(),
                connection_id.clone(),
                target_id,
                TargetAttachOptions::default(),
            )
            .await?;
        let target_id = attachment.target.target_id;

        let id = random_instance_id().map_err(|error| internal_error(error.to_string()))?;
        let relay = crate::context_relay::start_target_relay(
            self.clone(),
            context_id.clone(),
            connection_id,
            target_id,
            id.clone(),
        )
        .await
        .map_err(|error| internal_error(error.to_string()))?;
        self.register_relay(&relay_lifecycle_guard, id, context_id, relay)
            .await
    }

    async fn close_relay(&self, _ctx: &CallCtx, relay_id: String) -> Result<bool, JsonRpcError> {
        let registration = self.state.lock().await.relays.get(&relay_id).cloned();
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
        let selected = self
            .target_debugger(&context_id, &connection_id, &target_id)
            .await?;
        let owner = selected.snapshot();
        let target_id = owner.target_id;
        let (target_type, parent_id) = {
            let state = self.state.lock().await;
            let connection = state
                .contexts
                .get(&context_id)
                .ok_or_else(|| not_found("context", &context_id))?
                .connections
                .get(&connection_id)
                .ok_or_else(|| not_found("connection", &connection_id))?;
            if connection.generation != owner.connection_generation {
                return Err(invalid_state("connection changed before the screenshot could be captured"));
            }
            let target = context_connection_target(
                &state,
                &context_id,
                &connection_id,
                connection.generation,
                &target_id,
            )
            .ok_or_else(|| not_found("target", &target_id))?;
            (target.target.target_type, target.target.parent_id)
        };
        if target_type != "iframe" {
            return selected
                .capture_screenshot()
                .await
                .map_err(target_debugger_rpc_error);
        }
        let parent_id = parent_id.ok_or_else(|| {
            invalid_state("iframe screenshot requires a discovered embedding page")
        })?;
        let frame_id = target_id
            .rsplit_once("/target/")
            .map(|(_, frame_id)| frame_id)
            .ok_or_else(|| invalid_state("iframe target has no native frame identifier"))?;
        let owner_id = format!(
            "screenshot-{}",
            random_instance_id().map_err(|error| internal_error(error.to_string()))?
        );
        let service = self.clone();
        let frame_id = frame_id.to_owned();
        tokio::spawn(async move {
            let (parent, created) = service
                .relay_ensure_attached(&owner_id, &context_id, &connection_id, &parent_id)
                .await?;
            let result = if parent.snapshot().connection_generation != owner.connection_generation {
                Err(invalid_state("connection changed before the screenshot could be captured"))
            } else {
                capture_embedded_frame_screenshot(&parent, &frame_id).await
            };
            if created {
                service
                    .relay_release_attachment(
                        &owner_id,
                        &context_id,
                        &connection_id,
                        &parent_id,
                        &parent,
                    )
                    .await;
            }
            result
        })
        .await
        .map_err(|error| internal_error(format!("iframe screenshot task failed: {error}")))?
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
        raw: Option<bool>,
    ) -> Result<CoverageSnapshot, JsonRpcError> {
        if let Some(name) = capture_id.as_ref()
            && let Some(completed) = self
                .promote_completed_capture(
                    &context_id,
                    &connection_id,
                    &target_id,
                    name,
                    CaptureKind::Coverage,
                )
                .await?
        {
            let CapturePayload::Coverage(mut snapshot) = completed.payload else {
                return Err(invalid_state(
                    "completed capture kind does not match reservation",
                ));
            };
            snapshot.capture_id = Some(name.clone());
            return Ok(snapshot);
        }
        let debugger = self
            .target_debugger(&context_id, &connection_id, &target_id)
            .await?;
        let owner = debugger.snapshot();
        let reservation = CaptureReservationGuard::new(
                self.clone(),
                self.reserve_capture_optional(
                    &context_id,
                    &owner.connection_id,
                    &owner.target_id,
                    owner.connection_generation,
                    capture_id,
                    CaptureKind::Coverage,
                )
                .await?,
            );
        if reservation.reservation.completed.is_some() {
            let completed = self
                .promote_completed_capture(
                    &reservation.reservation.metadata.context_id,
                    &reservation.reservation.metadata.connection_id,
                    &reservation.reservation.metadata.target_id,
                    &reservation.reservation.metadata.name,
                    CaptureKind::Coverage,
                )
                .await?
                .ok_or_else(|| invalid_state("completed capture reservation disappeared"))?;
            let CapturePayload::Coverage(mut snapshot) = completed.payload else {
                return Err(invalid_state(
                    "completed capture kind does not match reservation",
                ));
            };
            snapshot.capture_id = Some(reservation.reservation.metadata.name.clone());
            return Ok(snapshot);
        }
        let mut snapshot = match debugger
            .take_coverage(
                Some(reservation.reservation.metadata.name.clone()),
                exclude_capture_id,
                raw.unwrap_or(false),
            )
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
            CapturePayload::Coverage(snapshot.clone()),
        )
        .await?;
        snapshot.capture_id = Some(reservation.reservation.metadata.name.clone());
        Ok(snapshot)
    }

    async fn stop_coverage(
        &self,
        _ctx: &CallCtx,
        context_id: String,
        connection_id: String,
        target_id: String,
        exclude_capture_id: Option<String>,
        capture_id: Option<String>,
    ) -> Result<CoverageSnapshot, JsonRpcError> {
        if let Some(name) = capture_id.as_ref()
            && let Some(completed) = self
            .promote_completed_capture(
                &context_id,
                &connection_id,
                &target_id,
                name,
                CaptureKind::Coverage,
            )
            .await?
        {
            let CapturePayload::Coverage(mut snapshot) = completed.payload else {
                return Err(invalid_state(
                    "completed capture kind does not match reservation",
                ));
            };
            snapshot.capture_id = Some(name.clone());
            return Ok(snapshot);
        }
        let debugger = self
            .target_debugger(&context_id, &connection_id, &target_id)
            .await?;
        let owner = debugger.snapshot();
        let reservation = CaptureReservationGuard::new(
            self.clone(),
            self.reserve_capture_optional(
                &context_id,
                &owner.connection_id,
                &owner.target_id,
                owner.connection_generation,
                capture_id,
                CaptureKind::Coverage,
            )
            .await?,
        );
        if reservation.reservation.completed.is_some() {
            let completed = self
                .promote_completed_capture(
                    &reservation.reservation.metadata.context_id,
                    &reservation.reservation.metadata.connection_id,
                    &reservation.reservation.metadata.target_id,
                    &reservation.reservation.metadata.name,
                    CaptureKind::Coverage,
                )
                .await?
                .ok_or_else(|| invalid_state("completed capture reservation disappeared"))?;
            let CapturePayload::Coverage(mut snapshot) = completed.payload else {
                return Err(invalid_state(
                    "completed capture kind does not match reservation",
                ));
            };
            snapshot.capture_id = Some(reservation.reservation.metadata.name.clone());
            return Ok(snapshot);
        }
        let mut snapshot = match debugger
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
            CapturePayload::Coverage(snapshot.clone()),
        )
        .await?;
        snapshot.capture_id = Some(reservation.reservation.metadata.name.clone());
        Ok(snapshot)
    }

    async fn finish_coverage(
        &self,
        _ctx: &CallCtx,
        context_id: String,
        connection_id: String,
        target_id: String,
        exclude_capture_id: Option<String>,
        capture_id: Option<String>,
    ) -> Result<bool, JsonRpcError> {
        self.stop_coverage(
            _ctx,
            context_id,
            connection_id,
            target_id,
            exclude_capture_id,
            capture_id,
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
        if let Some(name) = capture_id.as_ref()
            && let Some(completed) = self
            .promote_completed_capture(
                &context_id,
                &connection_id,
                &target_id,
                name,
                CaptureKind::CpuProfile,
            )
            .await?
        {
            let CapturePayload::CpuProfile(snapshot) = completed.payload else {
                return Err(invalid_state(
                    "completed capture kind does not match reservation",
                ));
            };
            return Ok(snapshot);
        }
        let debugger = self
            .target_debugger(&context_id, &connection_id, &target_id)
            .await?;
        let owner = debugger.snapshot();
        let reservation = CaptureReservationGuard::new(
            self.clone(),
            self.reserve_capture_optional(
                &context_id,
                &owner.connection_id,
                &owner.target_id,
                owner.connection_generation,
                capture_id,
                CaptureKind::CpuProfile,
            )
            .await?,
        );
        let name = reservation.reservation.metadata.name.clone();
        if reservation.reservation.completed.is_some() {
            let completed = self
                .promote_completed_capture(
                    &reservation.reservation.metadata.context_id,
                    &reservation.reservation.metadata.connection_id,
                    &reservation.reservation.metadata.target_id,
                    &reservation.reservation.metadata.name,
                    CaptureKind::CpuProfile,
                )
                .await?
                .ok_or_else(|| invalid_state("completed capture reservation disappeared"))?;
            let CapturePayload::CpuProfile(snapshot) = completed.payload else {
                return Err(invalid_state(
                    "completed capture kind does not match reservation",
                ));
            };
            return Ok(snapshot);
        }
        let snapshot = match debugger
            .stop_cpu_profile(Some(name.clone()))
            .await
            .map_err(target_debugger_rpc_error)
        {
            Ok(snapshot) => snapshot,
            Err(error) => {
                self.abandon_capture(&reservation.reservation).await;
                return Err(error);
            }
        };
        let snapshot = debugger
            .get_cpu_profile(name.clone(), None, false, true)
            .await
            .unwrap_or(snapshot);
        self.store_capture(
            &reservation.reservation,
            CapturePayload::CpuProfile(snapshot.clone()),
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
        if let Some(name) = capture_id.as_ref()
            && let Some(completed) = self
            .promote_completed_capture(
                &context_id,
                &connection_id,
                &target_id,
                name,
                CaptureKind::HeapSnapshot,
            )
            .await?
        {
            let CapturePayload::HeapSnapshot { .. } = completed.payload else {
                return Err(invalid_state(
                    "completed capture kind does not match reservation",
                ));
            };
            return completed
                .heap_result
                .ok_or_else(|| invalid_state("completed heap capture result is missing"));
        }
        let debugger = self
            .target_debugger(&context_id, &connection_id, &target_id)
            .await?;
        let owner = debugger.snapshot();
        let reservation = CaptureReservationGuard::new(
            self.clone(),
            self.reserve_capture_optional(
                &context_id,
                &owner.connection_id,
                &owner.target_id,
                owner.connection_generation,
                capture_id,
                CaptureKind::HeapSnapshot,
            )
            .await?,
        );
        let name = reservation.reservation.metadata.name.clone();
        if reservation.reservation.completed.is_some() {
            let completed = self
                .promote_completed_capture(
                    &reservation.reservation.metadata.context_id,
                    &reservation.reservation.metadata.connection_id,
                    &reservation.reservation.metadata.target_id,
                    &reservation.reservation.metadata.name,
                    CaptureKind::HeapSnapshot,
                )
                .await?
                .ok_or_else(|| invalid_state("completed capture reservation disappeared"))?;
            let CapturePayload::HeapSnapshot { .. } = completed.payload else {
                return Err(invalid_state(
                    "completed capture kind does not match reservation",
                ));
            };
            let result = completed
                .heap_result
                .ok_or_else(|| invalid_state("completed heap capture result is missing"))?;
            return Ok(result);
        }
        let result = match debugger
            .capture_heap_snapshot(Some(name.clone()), capture_numeric_value, expose_internals)
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
            remove_capture_payload_files([staging_path]);
            return Err(error);
        }
        if let Err(error) = self.capture_storage.sync_file(&staging_path) {
            self.abandon_capture(&reservation.reservation).await;
            remove_capture_payload_files([staging_path]);
            return Err(internal_error(format!(
                "failed to synchronize heap capture storage before publication: {error}"
            )));
        }
        if let Err(error) = fs::rename(&staging_path, &final_path) {
            self.abandon_capture(&reservation.reservation).await;
            remove_capture_payload_files([staging_path, final_path]);
            return Err(internal_error(format!(
                "failed to publish heap capture storage: {error}"
            )));
        }
        if let Err(error) = self.capture_storage.sync_parent(&final_path) {
            self.abandon_capture(&reservation.reservation).await;
            remove_capture_payload_files([final_path]);
            return Err(internal_error(format!(
                "failed to synchronize heap capture storage publication: {error}"
            )));
        }
        if let Err(error) = self
            .store_heap_capture(
                &reservation.reservation,
                CapturePayload::HeapSnapshot {
                    path: final_path.to_string_lossy().into_owned(),
                },
                result.clone(),
            )
            .await
        {
            if !self
                .completed_capture_is_retained(&reservation.reservation)
                .await
            {
                remove_capture_payload_files([final_path]);
            }
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
                state.debug_attachments.clear();
                state.pause_children_leases.clear();
                for registration in std::mem::take(&mut state.playwright_proxies).into_values() {
                    registration.cancel.send_replace(true);
                }
                for registration in std::mem::take(&mut state.relays).into_values() {
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
    async fn promote_completed_capture(
        &self,
        context_id: &str,
        connection_id: &str,
        target_id: &str,
        name: &str,
        kind: CaptureKind,
    ) -> Result<Option<PromotedCapture>, JsonRpcError> {
        let reservation = {
            let state = self.state.lock().await;
            let Some(reservation) = state
                .capture_reservations
                .get(&(context_id.to_owned(), name.to_owned()))
            else {
                return Ok(None);
            };
            if reservation.deleting {
                return Err(invalid_state(&format!(
                    "capture '{name}' is currently being deleted from context '{context_id}'"
                )));
            }
            let Some(completed) = reservation.completed.as_ref() else {
                return Ok(None);
            };
            if reservation.metadata.connection_id != connection_id
                || (reservation.metadata.target_id != target_id
                    && crate::target_selector::qualified_target_selector(
                        connection_id,
                        &reservation.metadata.target_id,
                        reservation.metadata.connection_generation,
                    ) != target_id)
                || reservation.metadata.kind != kind
            {
                return Err(invalid_state(&format!(
                    "capture '{name}' completed in context '{context_id}' but catalog persistence failed; retry the same capture request with exact target '{}' or delete it to discard the completed data",
                    reservation.metadata.target_id
                )));
            }
            (reservation.clone(), completed.clone())
        };
        let payload = load_capture_payload(&reservation.1.payload, reservation.0.metadata.kind)
            .map_err(capture_payload_rpc_error)?;
        self.commit_completed_capture(&reservation.0, &reservation.1)
            .await?;
        Ok(Some(PromotedCapture {
            payload,
            heap_result: reservation.1.heap_result,
        }))
    }

    #[cfg(test)]
    async fn reserve_capture(
        &self,
        context_id: &str,
        connection_id: &str,
        target_id: &str,
        connection_generation: u64,
        name: String,
        kind: CaptureKind,
    ) -> Result<CaptureReservation, JsonRpcError> {
        self.reserve_capture_optional(
            context_id, connection_id, target_id, connection_generation, Some(name), kind,
        ).await
    }

    async fn reserve_capture_optional(
        &self,
        context_id: &str,
        connection_id: &str,
        target_id: &str,
        connection_generation: u64,
        name: Option<String>,
        kind: CaptureKind,
    ) -> Result<CaptureReservation, JsonRpcError> {
        let mut state = self.state.lock().await;
        let automatic = name.is_none();
        let mut next_capture_id = state.next_capture_id.max(1);
        let name = match name {
            Some(name) => {
                validate_id("capture", &name)?;
                if crate::service_api::capture_relative_index(&name)
                    .map_err(invalid_params)?.is_some()
                {
                    return Err(invalid_params("relative capture selectors cannot be used as permanent capture IDs"));
                }
                name
            }
            None => loop {
                let candidate = format!("{}-{next_capture_id}", capture_prefix(kind));
                next_capture_id = next_capture_id.checked_add(1)
                    .ok_or_else(|| invalid_state("capture ID sequence exhausted"))?;
                let key = (context_id.to_owned(), candidate.clone());
                if !state.captures.contains_key(&key) && !state.capture_reservations.contains_key(&key) {
                    break candidate;
                }
            },
        };
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
            if existing.deleting {
                return Err(invalid_state(&format!(
                    "capture '{name}' is currently being deleted from context '{context_id}'"
                )));
            }
            if existing.completed.is_some() {
                if existing.metadata.connection_id == connection_id
                    && existing.metadata.target_id == target_id
                    && existing.metadata.connection_generation == connection_generation
                    && existing.metadata.kind == kind
                {
                    return Ok(existing.clone());
                }
                return Err(invalid_state(&format!(
                    "capture '{name}' completed in context '{context_id}' but catalog persistence failed; retry the same capture request or delete it to discard the completed data"
                )));
            }
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
            || !context_connection_has_target(
                &state,
                context_id,
                connection_id,
                connection_generation,
                target_id,
            )
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
        let reservation = CaptureReservation {
            metadata,
            completed: None,
            deleting: false,
        };
        let previous = automatic.then(|| state.clone());
        state.capture_reservations.insert(key, reservation.clone());
        if let Some(previous) = previous {
            state.next_capture_id = next_capture_id;
            self.persist_or_restore(&mut state, previous)?;
        }
        Ok(reservation)
    }

    async fn finalize_capture(
        &self,
        reservation: &CaptureReservation,
        payload: CapturePayload,
        heap_result: Option<HeapCaptureResult>,
    ) -> Result<CaptureSnapshot, JsonRpcError> {
        let metadata = &reservation.metadata;
        let key = (metadata.context_id.clone(), metadata.name.clone());
        {
            let state = self.state.lock().await;
            let current = state
                .capture_reservations
                .get(&key)
                .filter(|current| current.metadata.storage_id == metadata.storage_id)
                .ok_or_else(|| invalid_state("capture reservation is no longer current"))?;
            if let Some(completed) = &current.completed {
                let completed = completed.clone();
                drop(state);
                return self.commit_completed_capture(reservation, &completed).await;
            }
            let connection = state
                .contexts
                .get(&metadata.context_id)
                .and_then(|context| context.connections.get(&metadata.connection_id))
                .ok_or_else(|| invalid_state("capture owner connection no longer exists"))?;
            if connection.generation != metadata.connection_generation
                || !context_connection_has_target(
                    &state,
                    &metadata.context_id,
                    &metadata.connection_id,
                    metadata.connection_generation,
                    &metadata.target_id,
                )
            {
                return Err(invalid_state(
                    "connection generation changed while the capture was being stored",
                ));
            }
        }
        let payload = write_capture_payload_with_storage(
            &self.persistence_path,
            metadata,
            &payload,
            &self.capture_storage,
        )
        .map_err(capture_payload_rpc_error)?;
        let completed = CompletedCapture {
            payload,
            heap_result,
        };
        let mut state = self.state.lock().await;
        let current = state
            .capture_reservations
            .get(&key)
            .filter(|current| current.metadata.storage_id == metadata.storage_id);
        let valid_generation = state
            .contexts
            .get(&metadata.context_id)
            .and_then(|context| context.connections.get(&metadata.connection_id))
            .is_some_and(|connection| connection.generation == metadata.connection_generation)
            && context_connection_has_target(
                &state,
                &metadata.context_id,
                &metadata.connection_id,
                metadata.connection_generation,
                &metadata.target_id,
            );
        if current.is_none() || !valid_generation {
            drop(state);
            let _ = remove_capture_payload_file(Path::new(&completed.payload.path));
            return Err(invalid_state(
                "connection generation changed while the capture was being stored",
            ));
        }
        state
            .capture_reservations
            .get_mut(&key)
            .expect("current capture reservation disappeared while locked")
            .completed = Some(completed.clone());
        let previous = state.clone();
        let publication_order = state.next_publication_order.max(1);
        state.next_publication_order = publication_order.checked_add(1)
            .ok_or_else(|| invalid_state("capture publication sequence exhausted"))?;
        state.capture_reservations.remove(&key);
        state.captures.insert(
            key,
            StoredCapture {
                metadata: metadata.clone(),
                payload: completed.payload,
                publication_order,
                heap_mapping: completed.heap_result.as_ref().and_then(|result| result.mapping.clone()),
            },
        );
        self.persist_or_restore(&mut state, previous)?;
        Ok(metadata.clone())
    }

    async fn commit_completed_capture(
        &self,
        reservation: &CaptureReservation,
        completed: &CompletedCapture,
    ) -> Result<CaptureSnapshot, JsonRpcError> {
        let metadata = &reservation.metadata;
        let key = (metadata.context_id.clone(), metadata.name.clone());
        let mut state = self.state.lock().await;
        if state.capture_reservations.get(&key).is_none_or(|current| {
            current.deleting
                || current.metadata.storage_id != metadata.storage_id
                || current.completed.as_ref().is_none_or(|current| {
                    current.payload.path != completed.payload.path
                        || current.payload.sha256 != completed.payload.sha256
                })
        }) {
            return Err(invalid_state(
                "completed capture reservation is no longer current or is being deleted",
            ));
        }
        let previous = state.clone();
        let publication_order = state.next_publication_order.max(1);
        state.next_publication_order = publication_order.checked_add(1)
            .ok_or_else(|| invalid_state("capture publication sequence exhausted"))?;
        state.capture_reservations.remove(&key);
        state.captures.insert(
            key,
            StoredCapture {
                metadata: metadata.clone(),
                payload: completed.payload.clone(),
                publication_order,
                heap_mapping: completed.heap_result.as_ref().and_then(|result| result.mapping.clone()),
            },
        );
        self.persist_or_restore(&mut state, previous)?;
        Ok(metadata.clone())
    }

    async fn restore_failed_capture_deletion(&self, key: &(String, String), storage_id: &str) {
        let mut state = self.state.lock().await;
        if let Some(reservation) = state.capture_reservations.get_mut(key)
            && reservation.metadata.storage_id == storage_id
            && reservation.deleting
        {
            reservation.deleting = false;
        }
    }

    async fn abandon_capture(&self, reservation: &CaptureReservation) -> bool {
        let metadata = &reservation.metadata;
        let key = (metadata.context_id.clone(), metadata.name.clone());
        let mut state = self.state.lock().await;
        if state.capture_reservations.get(&key).is_some_and(|current| {
            current.metadata.storage_id == metadata.storage_id && current.completed.is_none()
        }) {
            state.capture_reservations.remove(&key);
            true
        } else {
            false
        }
    }

    async fn completed_capture_is_retained(&self, reservation: &CaptureReservation) -> bool {
        let metadata = &reservation.metadata;
        self.state
            .lock()
            .await
            .capture_reservations
            .get(&(metadata.context_id.clone(), metadata.name.clone()))
            .is_some_and(|current| {
                current.metadata.storage_id == metadata.storage_id && current.completed.is_some()
            })
    }

    async fn store_capture(
        &self,
        reservation: &CaptureReservation,
        payload: CapturePayload,
    ) -> Result<CaptureSnapshot, JsonRpcError> {
        self.finalize_capture(reservation, payload, None).await
    }

    async fn store_heap_capture(
        &self,
        reservation: &CaptureReservation,
        payload: CapturePayload,
        result: HeapCaptureResult,
    ) -> Result<CaptureSnapshot, JsonRpcError> {
        self.finalize_capture(reservation, payload, Some(result))
            .await
    }

    fn heap_capture_paths(&self, reservation: &CaptureReservation) -> (PathBuf, PathBuf) {
        heap_capture_paths_for(&self.persistence_path, reservation)
    }

    /// The full attach implementation. Reused directly (bypassing the relay-exclusivity guard
    /// on the public `attach_target` RPC) by system-internal callers: auto-attach on connect,
    /// auto-attach on provider target discovery, and relay lazy/explicit attachment.
    async fn attach_target_internal(
        &self,
        ctx: &CallCtx,
        context_id: String,
        connection_id: String,
        target_id: String,
        options: TargetAttachOptions,
    ) -> Result<TargetAttachmentResult, JsonRpcError> {
        let _attachment_guard = self.attachment_lock.lock().await;
        let (mut target_id, mut debugger_key, prior_owner, mut resolved_generation) = {
            let mut state = self.state.lock().await;
            let target_id =
                Self::resolve_target_id_in_state(&state, &context_id, &connection_id, &target_id)?;
            let debugger_key = (context_id.clone(), connection_id.clone(), target_id.clone());
            let context = state
                .contexts
                .get(&context_id)
                .ok_or_else(|| not_found("context", &context_id))?;
            let connection = context
                .connections
                .get(&connection_id)
                .ok_or_else(|| not_found("connection", &connection_id))?;
            let resolved_generation = connection.generation;
            if options
                .expected_connection_generation
                .is_some_and(|expected| expected != connection.generation)
            {
                return Err(invalid_state(
                    "connection changed before the target could be attached",
                ));
            }
            if !context_connection_has_target(
                &state,
                &context_id,
                &connection_id,
                connection.generation,
                &target_id,
            ) {
                return Err(not_found("target", &target_id));
            }
            if !state
                .runtimes
                .contains_key(&(context_id.clone(), connection_id.clone()))
            {
                return Err(invalid_state("connection is not connected"));
            }
            let physical_key = physical_target_key(&state, &debugger_key)?;
            let prior_owner = state.target_debuggers.iter().find_map(|(key, debugger)| {
                let owner_runtime = state.runtimes.get(&(key.0.clone(), key.1.clone()))?.clone();
                let connection = state.contexts.get(&key.0)?.connections.get(&key.1)?;
                (physical_target_key(&state, key).ok().as_ref() == Some(&physical_key)).then(|| {
                    PhysicalAttachmentOwner {
                        key: key.clone(),
                        debugger: debugger.clone(),
                        runtime: owner_runtime,
                        attempt: ConnectionAttempt {
                            configuration_version: connection.configuration_version,
                            generation: connection.generation,
                        },
                        attachment: state.debug_attachments.get(key).cloned(),
                    }
                })
            });
            if let Some(owner) = &prior_owner {
                if !options.force {
                    return Err(ownership_conflict(&owner.key));
                }
                remove_debugger_registration(&mut state, &owner.key);
            }
            (target_id, debugger_key, prior_owner, resolved_generation)
        };

        let mut outcome = TargetAttachmentOutcome::Created;
        if let Some(owner) = &prior_owner {
            self.publish_target_attachment_change(&owner.key, owner.attempt)
                .await;
        }
        if let Some(owner) = prior_owner {
            outcome = TargetAttachmentOutcome::Stolen;
            let owner_is_requested = owner.key == debugger_key;
            if owner.runtime.is_direct_debugger()
                && owner.key.2 == synthetic_node_target_id(&owner.key.1)
            {
                self.disconnect_connection(ctx, owner.key.0.clone(), owner.key.1.clone())
                    .await?;
                if owner_is_requested {
                    let connected = self
                        .connect_connection(ctx, context_id.clone(), connection_id.clone())
                        .await?;
                    resolved_generation = connected.connections
                        .iter()
                        .find(|connection| connection.id == connection_id)
                        .ok_or_else(|| not_found("connection", &connection_id))?
                        .generation;
                    target_id = self
                        .resolve_target_id(&context_id, &connection_id, &target_id)
                        .await?;
                    debugger_key = (context_id.clone(), connection_id.clone(), target_id.clone());
                }
            } else if let Some(attachment) = owner.attachment {
                attachment
                    .capability
                    .close(&attachment.handle)
                    .await
                    .map_err(|error| internal_error(error.to_string()))?;
            } else if owner.runtime.is_direct_debugger() {
                owner.runtime.close_direct_debugger(&owner.key.2).await;
            } else {
                detach_session(&owner.runtime, owner.debugger.session_id()).await;
            }
        }

        let (runtime, attempt, debug_capability, waiting_for_debugger, source_model) = {
            let mut state = self.state.lock().await;
            let context = state
                .contexts
                .get(&context_id)
                .ok_or_else(|| not_found("context", &context_id))?;
            let connection = context
                .connections
                .get(&connection_id)
                .ok_or_else(|| not_found("connection", &connection_id))?;
            if connection.generation != resolved_generation {
                return Err(invalid_state(
                    "connection changed before the target could be attached",
                ));
            }
            let target = context_connection_target(
                &state,
                &context_id,
                &connection_id,
                connection.generation,
                &target_id,
            )
            .ok_or_else(|| not_found("target", &target_id))?;
            let attempt = ConnectionAttempt {
                configuration_version: connection.configuration_version,
                generation: connection.generation,
            };
            let runtime = state
                .runtimes
                .get(&(context_id.clone(), connection_id.clone()))
                .cloned()
                .ok_or_else(|| invalid_state("connection is not connected"))?;
            let source = connection_source_id(&connection_id, connection.generation);
            let debug_capability = state
                .resource_graphs
                .get(&context_id)
                .and_then(|graph| {
                    graph.read(|graph| {
                        graph.capability_from_source(
                            &target.resource_id,
                            &source,
                            &CapabilityKind::Debug,
                        )
                    })
                })
                .and_then(|capability| capability.as_debug())
                .ok_or_else(|| {
                    invalid_state(&format!(
                        "target '{target_id}' has no available debug capability"
                    ))
                })?;
            let waiting_for_debugger = matches!(
                &connection.configuration,
                ConnectionConfiguration::Node { .. }
            ) || debug_capability.waiting_for_debugger();
            let source_model = state
                .source_models
                .entry(context_id.clone())
                .or_insert_with(|| Arc::new(ContextSourceModel::new()))
                .clone();
            (
                runtime,
                attempt,
                debug_capability,
                waiting_for_debugger,
                source_model,
            )
        };
        let generation = attempt.generation;

        let opened = debug_capability
            .open(DebugOpenRequest {
                pause_on_start: waiting_for_debugger,
                steal_existing_owner: options.force,
            })
            .await
            .map_err(|error| direct_attachment_error(error.to_string(), options.force))?;
        if opened.stole_existing_owner {
            outcome = TargetAttachmentOutcome::Stolen;
        }
        let session = match debug_capability.take_session(&opened) {
            Ok(session) => session,
            Err(error) => {
                let _ = debug_capability.close(&opened).await;
                return Err(internal_error(error.to_string()));
            }
        };
        let session_key = session.key().clone();
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
                let _ = debug_capability.close(&opened).await;
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
            let _ = debug_capability.close(&opened).await;
            return Err(invalid_state(
                "connection changed while the target was being attached",
            ));
        }
        if let Some(existing) = state.target_debuggers.get(&debugger_key) {
            let owner = existing.snapshot();
            drop(state);
            let _ = debug_capability.close(&opened).await;
            return Err(ownership_conflict(&(
                owner.context_id,
                owner.connection_id,
                owner.target_id,
            )));
        }
        let graph_source = match publish_debug_session_resource(
            &state,
            "debugger",
            &context_id,
            &connection_id,
            generation,
            &target_id,
            &opened.session_id,
        ) {
            Ok(source) => source,
            Err(error) => {
                drop(state);
                let _ = debug_capability.close(&opened).await;
                return Err(internal_error(format!(
                    "failed to publish debug session resource: {error}"
                )));
            }
        };
        state
            .target_debuggers
            .insert(debugger_key.clone(), debugger.clone());
        state.debug_attachments.insert(
            debugger_key.clone(),
            DebugAttachment {
                capability: debug_capability.clone(),
                handle: opened.clone(),
                graph_source,
            },
        );
        let context = state
            .contexts
            .get(&context_id)
            .expect("context was validated above");
        let context_revision = context.revision;
        let breakpoints = context
            .breakpoints
            .iter()
            .filter(|(_, breakpoint)| {
                breakpoint_applies_to_target(
                    breakpoint.enabled,
                    breakpoint.target_selector.as_deref(),
                    &target_id,
                )
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
                    remove_debugger_registration(&mut state, &debugger_key);
                }
                drop(state);
                let _ = debug_capability.close(&opened).await;
                return Err(target_debugger_rpc_error(error));
            }
        }
        let snapshot = debugger.settle(Duration::from_millis(200)).await;
        for breakpoint_id in breakpoint_ids {
            self.publish_breakpoint_application(&context_id, &breakpoint_id)
                .await;
        }
        self.publish_target_attachment_change(&debugger_key, attempt)
            .await;
        Ok(TargetAttachmentResult {
            outcome,
            target: snapshot,
        })
    }

    /// Looks up an already-attached target's debugger handle, rejecting the call while its
    /// context is exclusively owned by an active relay. Relay-internal forwarding uses
    /// [`Self::target_debugger_bypassing_relay`] directly instead.
    async fn target_debugger(
        &self,
        context_id: &str,
        connection_id: &str,
        target_id: &str,
    ) -> Result<TargetDebuggerHandle, JsonRpcError> {
        ensure_context_not_relayed(&*self.state.lock().await, context_id)?;
        self.target_debugger_bypassing_relay(context_id, connection_id, target_id)
            .await
    }

    async fn target_debugger_bypassing_relay(
        &self,
        context_id: &str,
        connection_id: &str,
        target_id: &str,
    ) -> Result<TargetDebuggerHandle, JsonRpcError> {
        let state = self.state.lock().await;
        let target_id =
            Self::resolve_target_id_in_state(&state, context_id, connection_id, target_id)?;
        state
            .target_debuggers
            .get(&(
                context_id.to_owned(),
                connection_id.to_owned(),
                target_id.clone(),
            ))
            .cloned()
            .ok_or_else(|| not_found("attached target", &target_id))
    }

    /// Every canonical target currently known in `context_id`, paired with its owning
    /// connection id, for a relay's `Target.getTargets` and discovery diffing.
    pub(crate) async fn relay_targets(
        &self,
        context_id: &str,
    ) -> Result<Vec<(String, TargetSnapshot)>, JsonRpcError> {
        let state = self.state.lock().await;
        if !state.contexts.contains_key(context_id) {
            return Err(not_found("context", context_id));
        }
        let Some(graph) = state.resource_graphs.get(context_id) else {
            return Ok(Vec::new());
        };
        Ok(relay_targets_from_graph(graph.snapshot()))
    }

    /// Atomically seeds a relay's target inventory and subscribes it to every later graph revision.
    pub(crate) async fn relay_target_observation(
        &self,
        context_id: &str,
    ) -> Result<
        (
            Vec<(String, TargetSnapshot)>,
            watch::Receiver<crate::resource_graph::GraphRevision>,
        ),
        JsonRpcError,
    > {
        let mut state = self.state.lock().await;
        if !state.contexts.contains_key(context_id) {
            return Err(not_found("context", context_id));
        }
        let observation = state
            .resource_graphs
            .entry(context_id.to_owned())
            .or_default()
            .observe();
        Ok((
            relay_targets_from_graph(observation.snapshot),
            observation.revisions,
        ))
    }

    /// Fetches an already-attached target's debugger handle for relay forwarding, bypassing the
    /// exclusivity guard (the relay itself is the sole owner while it holds the context).
    pub(crate) async fn relay_target_handle(
        &self,
        context_id: &str,
        connection_id: &str,
        target_id: &str,
    ) -> Result<TargetDebuggerHandle, JsonRpcError> {
        self.target_debugger_bypassing_relay(context_id, connection_id, target_id)
            .await
    }

    pub(crate) async fn relay_open_cdp_session(
        &self,
        context_id: &str,
        connection_id: &str,
        session_id: &str,
    ) -> Result<crate::cdp_runtime::CdpDebuggerSession, JsonRpcError> {
        let (runtime, generation) = {
            let state = self.state.lock().await;
            let generation = state
                .contexts
                .get(context_id)
                .ok_or_else(|| not_found("context", context_id))?
                .connections
                .get(connection_id)
                .ok_or_else(|| not_found("connection", connection_id))?
                .generation;
            let runtime = state
                .runtimes
                .get(&(context_id.to_owned(), connection_id.to_owned()))
                .cloned()
                .ok_or_else(|| invalid_state("connection is not connected"))?;
            (runtime, generation)
        };
        runtime
            .open_session(SessionKey {
                connection_generation: generation,
                session_id: session_id.to_owned(),
            })
            .map_err(|error| invalid_state(&error.to_string()))
    }

    /// Returns the target's debugger handle, attaching it first (bypassing the exclusivity
    /// guard) if it is not already attached. Idempotent: safe to call repeatedly for the same
    /// target, unlike the public `attach_target` RPC which fails on an existing attachment.
    pub(crate) async fn relay_ensure_attached(
        &self,
        owner_id: &str,
        context_id: &str,
        connection_id: &str,
        target_id: &str,
    ) -> Result<(TargetDebuggerHandle, bool), JsonRpcError> {
        let service = self.clone();
        let owner_id = owner_id.to_owned();
        let context_id = context_id.to_owned();
        let connection_id = connection_id.to_owned();
        let target_id = target_id.to_owned();
        tokio::spawn(async move {
            service
                .relay_ensure_attached_inner(&owner_id, &context_id, &connection_id, &target_id)
                .await
        })
        .await
        .map_err(|error| internal_error(format!("relay attachment task failed: {error}")))?
    }

    async fn relay_ensure_attached_inner(
        &self,
        owner_id: &str,
        context_id: &str,
        connection_id: &str,
        target_id: &str,
    ) -> Result<(TargetDebuggerHandle, bool), JsonRpcError> {
        let _relay_attachment_guard = self.relay_attachment_lock.lock().await;
        let resolved_target_id = self
            .resolve_target_id(context_id, connection_id, target_id)
            .await?;
        let key = (
            owner_id.to_owned(),
            context_id.to_owned(),
            connection_id.to_owned(),
            resolved_target_id.clone(),
        );
        if let Some(attachment) = self.relay_attachments.lock().await.get(&key).cloned() {
            return Ok((attachment.debugger, false));
        }
        let ordinary_key = (
            context_id.to_owned(),
            connection_id.to_owned(),
            resolved_target_id.clone(),
        );
        if let Some(debugger) = self
            .state
            .lock()
            .await
            .target_debuggers
            .get(&ordinary_key)
            .cloned()
        {
            return Ok((debugger, false));
        }
        let is_direct_debugger = self
            .state
            .lock()
            .await
            .runtimes
            .get(&(context_id.to_owned(), connection_id.to_owned()))
            .is_some_and(|runtime| runtime.is_direct_debugger());
        if is_direct_debugger {
            self.attach_target_internal(
                &CallCtx::default(),
                context_id.to_owned(),
                connection_id.to_owned(),
                resolved_target_id.clone(),
                TargetAttachOptions::default(),
            )
            .await?;
            let debugger = self
                .state
                .lock()
                .await
                .target_debuggers
                .get(&ordinary_key)
                .cloned()
                .ok_or_else(|| {
                    internal_error("direct relay attachment did not register a debugger")
                })?;
            return Ok((debugger, false));
        }
        let (generation, debug_capability, waiting_for_debugger, source_model) = {
            let mut state = self.state.lock().await;
            let connection = state
                .contexts
                .get(context_id)
                .ok_or_else(|| not_found("context", context_id))?
                .connections
                .get(connection_id)
                .ok_or_else(|| not_found("connection", connection_id))?;
            let target = context_connection_target(
                &state,
                context_id,
                connection_id,
                connection.generation,
                &resolved_target_id,
            )
            .ok_or_else(|| not_found("target", &resolved_target_id))?;
            if !state
                .runtimes
                .contains_key(&(context_id.to_owned(), connection_id.to_owned()))
            {
                return Err(invalid_state("connection is not connected"));
            }
            let source = connection_source_id(connection_id, connection.generation);
            let debug_capability = state
                .resource_graphs
                .get(context_id)
                .and_then(|graph| {
                    graph.read(|graph| {
                        graph.capability_from_source(
                            &target.resource_id,
                            &source,
                            &CapabilityKind::Debug,
                        )
                    })
                })
                .and_then(|capability| capability.as_debug())
                .ok_or_else(|| {
                    invalid_state(&format!(
                        "target '{resolved_target_id}' has no available debug capability"
                    ))
                })?;
            let waiting_for_debugger = debug_capability.waiting_for_debugger();
            let generation = connection.generation;
            let source_model = state
                .source_models
                .entry(context_id.to_owned())
                .or_insert_with(|| Arc::new(ContextSourceModel::new()))
                .clone();
            (
                generation,
                debug_capability,
                waiting_for_debugger,
                source_model,
            )
        };
        let opened = match debug_capability
            .open(DebugOpenRequest {
                pause_on_start: waiting_for_debugger,
                steal_existing_owner: false,
            })
            .await
        {
            Ok(opened) => opened,
            Err(error) => {
                if let Some(debugger) = self
                    .state
                    .lock()
                    .await
                    .target_debuggers
                    .get(&ordinary_key)
                    .cloned()
                {
                    return Ok((debugger, false));
                }
                return Err(direct_attachment_error(error.to_string(), false));
            }
        };
        let session = match debug_capability.take_session(&opened) {
            Ok(session) => session,
            Err(error) => {
                let _ = debug_capability.close(&opened).await;
                return Err(internal_error(error.to_string()));
            }
        };
        let session_key = session.key().clone();
        let debugger = match TargetDebuggerHandle::start(
            context_id.to_owned(),
            connection_id.to_owned(),
            resolved_target_id.clone(),
            generation,
            session,
            session_key,
            waiting_for_debugger,
            source_model,
        )
        .await
        {
            Ok(debugger) => debugger,
            Err(error) => {
                let _ = debug_capability.close(&opened).await;
                return Err(target_debugger_rpc_error(error));
            }
        };
        let graph_source = {
            let state = self.state.lock().await;
            let generation_is_current = state
                .contexts
                .get(context_id)
                .and_then(|context| context.connections.get(connection_id))
                .is_some_and(|connection| connection.generation == generation);
            if !generation_is_current
                || !context_connection_has_target(
                    &state,
                    context_id,
                    connection_id,
                    generation,
                    &resolved_target_id,
                )
            {
                drop(state);
                let _ = debug_capability.close(&opened).await;
                return Err(invalid_state(
                    "connection changed while the relay target was being attached",
                ));
            }
            match publish_debug_session_resource(
                &state,
                &format!("relay:{owner_id}"),
                context_id,
                connection_id,
                generation,
                &resolved_target_id,
                &opened.session_id,
            ) {
                Ok(source) => source,
                Err(error) => {
                    drop(state);
                    let _ = debug_capability.close(&opened).await;
                    return Err(internal_error(format!(
                        "failed to publish relay debug session resource: {error}"
                    )));
                }
            }
        };
        self.relay_attachments.lock().await.insert(
            key,
            RelayDebugAttachment {
                debugger: debugger.clone(),
                attachment: DebugAttachment {
                    capability: debug_capability,
                    handle: opened,
                    graph_source,
                },
            },
        );
        Ok((debugger, true))
    }

    pub(crate) async fn relay_release_attachment(
        &self,
        owner_id: &str,
        context_id: &str,
        connection_id: &str,
        target_id: &str,
        handle: &TargetDebuggerHandle,
    ) {
        let key = (
            owner_id.to_owned(),
            context_id.to_owned(),
            connection_id.to_owned(),
            target_id.to_owned(),
        );
        let attachment = {
            let mut attachments = self.relay_attachments.lock().await;
            if !attachments
                .get(&key)
                .is_some_and(|current| current.debugger.same_instance(handle))
            {
                return;
            }
            attachments.remove(&key).map(|entry| entry.attachment)
        };
        if let Some(attachment) = attachment {
            {
                let state = self.state.lock().await;
                retract_debug_session_resource(&state, context_id, &attachment.graph_source);
            }
            if let Err(error) = attachment.capability.close(&attachment.handle).await {
                eprintln!("failed to close relay-owned target attachment: {error}");
            }
        }
    }

    async fn release_relay_attachments_for_target(
        &self,
        context_id: &str,
        connection_id: &str,
        target_id: &str,
    ) {
        let _relay_attachment_guard = self.relay_attachment_lock.lock().await;
        let attachments = {
            let mut current = self.relay_attachments.lock().await;
            let keys = current
                .keys()
                .filter(|(_, context, connection, target)| {
                    context == context_id && connection == connection_id && target == target_id
                })
                .cloned()
                .collect::<Vec<_>>();
            keys.into_iter()
                .filter_map(|key| current.remove(&key).map(|entry| entry.attachment))
                .collect::<Vec<_>>()
        };
        for attachment in attachments {
            if let Err(error) = attachment.capability.close(&attachment.handle).await {
                eprintln!("failed to close removed target's relay attachment: {error}");
            }
        }
    }

    async fn release_relay_attachments_for_connection(
        &self,
        context_id: &str,
        connection_id: &str,
    ) {
        let _relay_attachment_guard = self.relay_attachment_lock.lock().await;
        let attachments = {
            let mut current = self.relay_attachments.lock().await;
            let keys = current
                .keys()
                .filter(|(_, context, connection, _)| {
                    context == context_id && connection == connection_id
                })
                .cloned()
                .collect::<Vec<_>>();
            keys.into_iter()
                .filter_map(|key| current.remove(&key).map(|entry| entry.attachment))
                .collect::<Vec<_>>()
        };
        for attachment in attachments {
            if let Err(error) = attachment.capability.close(&attachment.handle).await {
                eprintln!("failed to close disconnected relay attachment: {error}");
            }
        }
    }

    pub(crate) async fn relay_release_owned_attachments(&self, owner_id: &str) {
        let _relay_attachment_guard = self.relay_attachment_lock.lock().await;
        let attachments = {
            let mut current = self.relay_attachments.lock().await;
            let keys = current
                .keys()
                .filter(|(owner, _, _, _)| owner == owner_id)
                .cloned()
                .collect::<Vec<_>>();
            keys.into_iter()
                .filter_map(|key| current.remove(&key).map(|entry| (key.1, entry.attachment)))
                .collect::<Vec<_>>()
        };
        for (context_id, attachment) in attachments {
            {
                let state = self.state.lock().await;
                retract_debug_session_resource(&state, &context_id, &attachment.graph_source);
            }
            if let Err(error) = attachment.capability.close(&attachment.handle).await {
                eprintln!("failed to close relay-owned target attachment: {error}");
            }
        }
    }

    /// Finishes opening a relay while the caller holds `relay_lifecycle_lock`, registers it in
    /// `state.relays` so the guard takes effect immediately, and arranges for that registration to
    /// be removed once the relay's dispatch task completes, however it ends.
    async fn register_relay(
        &self,
        _relay_lifecycle_guard: &tokio::sync::MutexGuard<'_, ()>,
        id: String,
        context_id: String,
        relay: crate::context_relay::RelaySession,
    ) -> Result<RelayEndpoint, JsonRpcError> {
        let crate::context_relay::RelaySession {
            websocket_url,
            cancel,
            completion,
        } = relay;
        {
            let mut state = self.state.lock().await;
            if !state.contexts.contains_key(&context_id) {
                cancel.send_replace(true);
                return Err(not_found("context", &context_id));
            }
            if let Err(error) = ensure_context_not_relayed(&state, &context_id) {
                cancel.send_replace(true);
                return Err(error);
            }
            state.relays.insert(
                id.clone(),
                RelayRegistration {
                    context_id: context_id.clone(),
                    cancel: cancel.clone(),
                },
            );
        }
        let service = self.clone();
        let cleanup_id = id.clone();
        tokio::spawn(async move {
            let _ = completion.await;
            service.state.lock().await.relays.remove(&cleanup_id);
        });
        Ok(RelayEndpoint { id, websocket_url })
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
        let mut candidates = Vec::new();
        for (connection_id, connection) in context.connections.iter() {
            for target_resource in
                context_connection_targets(state, context_id, connection_id, connection.generation)
                    .into_values()
            {
                let target = target_resource.target;
                candidates.push(CanonicalTargetSnapshot {
                    context_id: context_id.to_owned(),
                    resource_id: target_resource.resource_id.to_string(),
                    target_id: target.target_id.clone(),
                    connection_id: connection_id.clone(),
                    connection_generation: connection.generation,
                    target,
                });
            }
        }
        let matches = crate::target_selector::select_target_matches(
            &candidates,
            context.connections.iter().map(|(id, connection)| (id.as_str(), connection.generation)),
            Some(selector),
            |target| crate::target_selector::TargetSelectorCandidate {
                target: &target.target,
                connection_id: &target.connection_id,
                generation: target.connection_generation,
            },
        ).map_err(|message| invalid_params(&message))?;
        match matches.as_slice() {
            [target] => Ok((*target).clone()),
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
        Self::resolve_target_id_in_state(&state, context_id, connection_id, selector)
    }

    fn resolve_target_id_in_state(
        state: &ServiceState,
        context_id: &str,
        connection_id: &str,
        selector: &str,
    ) -> Result<String, JsonRpcError> {
        let connection = state
            .contexts
            .get(context_id)
            .ok_or_else(|| not_found("context", context_id))?
            .connections
            .get(connection_id)
            .ok_or_else(|| not_found("connection", connection_id))?;
        let targets =
            context_connection_targets(state, context_id, connection_id, connection.generation);
        let candidates = targets
            .values()
            .map(|target| &target.target)
            .collect::<Vec<_>>();
        let matches = crate::target_selector::select_target_matches(
            &candidates,
            state.contexts[context_id].connections.iter()
                .map(|(id, connection)| (id.as_str(), connection.generation)),
            Some(selector),
            |target| crate::target_selector::TargetSelectorCandidate {
                target,
                connection_id,
                generation: connection.generation,
            },
        ).map_err(|message| invalid_params(&message))?
            .into_iter()
            .map(|target| target.target_id.clone())
            .collect::<Vec<_>>();
        match matches.as_slice() {
            [target_id] => Ok(target_id.clone()),
            [] => Err(not_found("target selector", selector)),
            _ => Err(invalid_params(&format!(
                "target selector '{selector}' is ambiguous across {} targets. Qualified candidates: {}",
                matches.len(),
                matches
                    .iter()
                    .map(|target_id| crate::target_selector::qualified_target_selector(
                        connection_id,
                        target_id,
                        connection.generation,
                    ))
                    .collect::<Vec<_>>()
                    .join(", ")
            ))),
        }
    }
}

async fn capture_embedded_frame_screenshot(
    parent: &TargetDebuggerHandle,
    frame_id: &str,
) -> Result<ScreenshotSnapshot, JsonRpcError> {
    let owner = parent
        .raw_cdp_request(
            "DOM.getFrameOwner".to_owned(),
            serde_json::json!({ "frameId": frame_id }),
        )
        .await?;
    let backend_node_id = owner
        .get("backendNodeId")
        .and_then(serde_json::Value::as_u64)
        .ok_or_else(|| invalid_state("embedding frame owner has no backend node identifier"))?;
    let box_model = parent
        .raw_cdp_request(
            "DOM.getBoxModel".to_owned(),
            serde_json::json!({ "backendNodeId": backend_node_id }),
        )
        .await?;
    let content = box_model
        .pointer("/model/content")
        .and_then(serde_json::Value::as_array)
        .filter(|quad| quad.len() == 8)
        .ok_or_else(|| invalid_state("embedding frame has no valid content box"))?;
    let coordinates = content
        .iter()
        .map(|coordinate| {
            coordinate
                .as_f64()
                .filter(|coordinate| coordinate.is_finite())
                .ok_or_else(|| invalid_state("embedding frame content box is not finite"))
        })
        .collect::<Result<Vec<_>, _>>()?;
    let xs = [
        coordinates[0],
        coordinates[2],
        coordinates[4],
        coordinates[6],
    ];
    let ys = [
        coordinates[1],
        coordinates[3],
        coordinates[5],
        coordinates[7],
    ];
    let x = xs.into_iter().fold(f64::INFINITY, f64::min);
    let y = ys.into_iter().fold(f64::INFINITY, f64::min);
    let width = xs.into_iter().fold(f64::NEG_INFINITY, f64::max) - x;
    let height = ys.into_iter().fold(f64::NEG_INFINITY, f64::max) - y;
    if width <= 0.0 || height <= 0.0 {
        return Err(invalid_state(
            "embedding frame content box has no visible area",
        ));
    }
    let screenshot = parent
        .raw_cdp_request(
            "Page.captureScreenshot".to_owned(),
            serde_json::json!({
                "format": "png",
                "fromSurface": true,
                "captureBeyondViewport": true,
                "clip": {
                    "x": x,
                    "y": y,
                    "width": width,
                    "height": height,
                    "scale": 1
                }
            }),
        )
        .await?;
    let data_base64 = screenshot
        .get("data")
        .and_then(serde_json::Value::as_str)
        .ok_or_else(|| invalid_state("screenshot response contained no image data"))?
        .to_owned();
    Ok(ScreenshotSnapshot {
        media_type: "image/png".to_owned(),
        data_base64,
    })
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

fn canonicalize_synthetic_target_id(target_id: &str, connection_id: &str) -> String {
    if target_id == "$node-root" {
        synthetic_node_target_id(connection_id)
    } else {
        target_id.to_owned()
    }
}

/// Inverse of [`canonicalize_synthetic_target_id`]: maps the id dbgjs exposes back to the id the
/// connection's `Target` domain uses.
fn runtime_target_id(target_id: &str, connection_id: &str) -> String {
    if target_id == synthetic_node_target_id(connection_id) {
        "$node-root".to_owned()
    } else {
        target_id.to_owned()
    }
}

fn connection_source_id(connection_id: &str, generation: u64) -> SourceId {
    SourceId::new(format!("connection:{connection_id}:{generation}"))
}

fn connection_root_resource_id(connection_id: &str, generation: u64) -> ResourceId {
    ResourceId::from_parts(
        "connection",
        [connection_id, generation.to_string().as_str()],
    )
    .expect("connection ids and generations are valid resource-id parts")
}

fn resource_id_for_target(
    connection_id: &str,
    connection: &ConnectionState,
    runtime: &ConnectionRuntime,
    target: &TargetSnapshot,
) -> ResourceId {
    let runtime_target_id = runtime_target_id(&target.target_id, connection_id);
    if target.subtype.as_deref() == Some("electron-renderer") {
        let root = match &connection.configuration {
            ConnectionConfiguration::ProcessTree { root_pid }
            | ConnectionConfiguration::ScopedProcessTree { root_pid, .. } => root_pid.to_string(),
            _ => connection_id.to_owned(),
        };
        return ResourceId::from_parts("electron-web-contents", [root, runtime_target_id])
            .expect("Electron target identities are valid resource-id parts");
    }
    if let Some(endpoint) = match &connection.configuration {
        ConnectionConfiguration::DirectCdp { endpoint }
        | ConnectionConfiguration::NodeInspector { endpoint } => Some(endpoint),
        _ => None,
    } {
        return ResourceId::from_parts(
            "cdp-endpoint-target",
            [endpoint.as_str(), runtime_target_id.as_str()],
        )
        .expect("CDP endpoint target identities are valid resource-id parts");
    }
    let process_id =
        runtime
            .target_process_id(&runtime_target_id)
            .or_else(|| match &connection.configuration {
                ConnectionConfiguration::Process { process_id }
                    if target.target_id == synthetic_node_target_id(connection_id) =>
                {
                    Some(*process_id)
                }
                _ => None,
            });
    if let Some(process_id) = process_id
        && matches!(target.target_type.as_str(), "node" | "process")
    {
        let process_identity = if target
            .target_id
            .starts_with(&format!("process-{process_id}-"))
        {
            target.target_id.clone()
        } else {
            format!("pid-{process_id}")
        };
        return ResourceId::from_parts("process", [process_identity])
            .expect("process ids are valid resource-id parts");
    }
    ResourceId::from_parts(
        "cdp-target",
        [
            connection_id,
            connection.generation.to_string().as_str(),
            runtime_target_id.as_str(),
        ],
    )
    .expect("connection and target ids are valid resource-id parts")
}

fn sync_connection_resource_graph(
    graph: &mut ResourceGraph,
    connection_id: &str,
    connection: &ConnectionState,
    targets: &BTreeMap<String, TargetSnapshot>,
    runtime: &Arc<ConnectionRuntime>,
) -> Result<(), String> {
    let source = connection_source_id(connection_id, connection.generation);
    graph.retract_source(&source);

    let root_id = connection_root_resource_id(connection_id, connection.generation);
    let root_kind = match connection.configuration {
        ConnectionConfiguration::DirectCdp { .. }
        | ConnectionConfiguration::Chrome { .. }
        | ConnectionConfiguration::Playwright { .. } => "browser",
        ConnectionConfiguration::ProcessTree { .. }
        | ConnectionConfiguration::ScopedProcessTree { .. } => "process-tree",
        ConnectionConfiguration::NodeInspector { .. }
        | ConnectionConfiguration::Process { .. }
        | ConnectionConfiguration::Node { .. } => "runtime",
        ConnectionConfiguration::Stdio { topology, .. } => match topology {
            crate::service_api::CdpStdioTopology::Browser => "browser",
            crate::service_api::CdpStdioTopology::Target => "runtime",
        },
    };
    let resources = targets
        .values()
        .map(|target| {
            (
                target.target_id.clone(),
                resource_id_for_target(connection_id, connection, runtime, target),
            )
        })
        .collect::<BTreeMap<_, _>>();
    let mut root_upsert = ResourceUpsert::new(
        root_id.clone(),
        ResourceFacts::of_kind(ResourceKind::new(root_kind))
            .with_label(connection_id)
            .with_attribute(
                "connectionId",
                serde_json::Value::String(connection_id.to_owned()),
            )
            .with_attribute("generation", serde_json::Value::from(connection.generation)),
    )
    .with_frontier(RelationKind::Contains, DiscoveryState::Live);
    if let Some(capability) = runtime.pause_future_children_capability(
        root_id.clone(),
        resources
            .iter()
            .map(|(target_id, resource)| {
                (
                    resource.clone(),
                    runtime_target_id(target_id, connection_id),
                )
            })
            .collect(),
    ) {
        root_upsert =
            root_upsert.with_capability(CapabilityObject::PauseFutureChildren(capability));
    }
    let mut delta = GraphDelta::for_source(source.clone()).upsert(root_upsert);

    for target in targets.values() {
        let resource = resources
            .get(&target.target_id)
            .expect("resource ids were built from the same target map")
            .clone();
        let runtime_id = runtime_target_id(&target.target_id, connection_id);
        let mut facts = ResourceFacts::of_kind(ResourceKind::new(&target.target_type))
            .with_label(if target.title.is_empty() {
                target.target_id.clone()
            } else {
                target.title.clone()
            })
            .with_attribute(
                "targetId",
                serde_json::Value::String(target.target_id.clone()),
            )
            .with_attribute(
                "connectionId",
                serde_json::Value::String(connection_id.to_owned()),
            )
            .with_attribute(
                "targetType",
                serde_json::Value::String(target.target_type.clone()),
            )
            .with_attribute("title", serde_json::Value::String(target.title.clone()))
            .with_attribute("url", serde_json::Value::String(target.url.clone()))
            .with_attribute("attached", serde_json::Value::from(target.attached));
        if let Some(process_id) = runtime.target_process_id(&runtime_id) {
            facts = facts.with_attribute("processId", serde_json::Value::from(process_id));
        }
        if let Some(window_id) = runtime.target_primary_window_id(&runtime_id) {
            facts = facts.with_attribute("primaryWindowId", serde_json::Value::from(window_id));
        }
        if let Some(subtype) = &target.subtype {
            facts = facts.with_attribute("subtype", serde_json::Value::String(subtype.clone()));
        }
        if let Some(parent_id) = &target.parent_id {
            facts = facts.with_attribute(
                "parentTargetId",
                serde_json::Value::String(parent_id.clone()),
            );
        }
        if let Some(opener_id) = &target.opener_id {
            facts = facts.with_attribute(
                "openerTargetId",
                serde_json::Value::String(opener_id.clone()),
            );
        }
        if let Some(browser_context_id) = &target.browser_context_id {
            facts = facts.with_attribute(
                "browserContextId",
                serde_json::Value::String(browser_context_id.clone()),
            );
        }
        delta = delta.upsert(
            ResourceUpsert::new(resource.clone(), facts)
                .with_capability(CapabilityObject::Debug(
                    runtime.debug_capability(resource.clone(), runtime_id),
                ))
                .with_frontier(RelationKind::Contains, DiscoveryState::Unobserved),
        );

        let parent = target
            .parent_id
            .as_ref()
            .and_then(|parent| resources.get(parent))
            .unwrap_or(&root_id);
        if parent != &resource {
            delta = delta.relate(RelationKind::Contains, parent.clone(), resource);
        }
    }
    graph.apply(delta).map_err(|error| error.to_string())?;
    Ok(())
}

fn stage_connection_resource_graph(
    state: &mut ServiceState,
    context_id: &str,
    connection_id: &str,
    context: &ContextState,
    targets: &BTreeMap<String, TargetSnapshot>,
    runtime: &Arc<ConnectionRuntime>,
    retracted_sources: &[SourceId],
) -> Result<(), String> {
    let connection = context
        .connections
        .get(connection_id)
        .ok_or_else(|| format!("connection '{connection_id}' does not exist"))?;
    let graph = state
        .resource_graphs
        .entry(context_id.to_owned())
        .or_default()
        .clone();
    graph.try_update(|graph| {
        for source in retracted_sources {
            graph.retract_source(source);
        }
        sync_connection_resource_graph(graph, connection_id, connection, targets, runtime)
    })?;
    Ok(())
}

fn stage_process_resource_graph(
    state: &mut ServiceState,
    context_id: &str,
    trees: &[ProcessTreeSnapshot],
) -> Result<(), String> {
    let graph = state
        .resource_graphs
        .entry(context_id.to_owned())
        .or_default()
        .clone();
    let source = SourceId::new("process-discovery");
    graph.try_update(|graph| {
        graph.retract_source(&source);
        let mut delta = GraphDelta::for_source(source);
        let mut upserts = BTreeMap::<ResourceId, ResourceUpsert>::new();
        for tree in trees {
            let process_ids = tree
                .processes
                .iter()
                .map(|process| (process.process_id, process_resource_id(process.process_id)))
                .collect::<BTreeMap<_, _>>();
            for process in &tree.processes {
                let process_id = process_ids[&process.process_id].clone();
                let facts = ResourceFacts::of_kind(ResourceKind::process())
                    .with_label(
                        process
                            .display_name
                            .as_deref()
                            .or(process.window_title.as_deref())
                            .unwrap_or(&process.name),
                    )
                    .with_attribute("processId", serde_json::Value::from(process.process_id))
                    .with_attribute(
                        "rootProcessId",
                        serde_json::Value::from(tree.root_process_id),
                    )
                    .with_attribute(
                        "role",
                        serde_json::to_value(&process.role)
                            .expect("process roles are serializable"),
                    )
                    .with_attribute(
                        "commandLine",
                        serde_json::Value::String(process.command_line.clone()),
                    )
                    .with_attribute(
                        "creationDate",
                        serde_json::Value::String(process.creation_date.clone()),
                    )
                    .with_attribute("attachable", serde_json::Value::Bool(process.attachable));
                upserts
                    .entry(process_id.clone())
                    .or_insert_with(|| ResourceUpsert::new(process_id.clone(), facts));
                if let Some(parent_id) = process
                    .parent_process_id
                    .and_then(|parent_id| process_ids.get(&parent_id))
                {
                    delta =
                        delta.relate(RelationKind::Spawned, parent_id.clone(), process_id.clone());
                }
                for session in &process.agent_sessions {
                    let session_id =
                        ResourceId::from_parts("agent-session", [session.internal_id.as_str()])
                            .map_err(|error| error.to_string())?;
                    let mut facts = ResourceFacts::of_kind(ResourceKind::new("agent-session"))
                        .with_label(session.title.as_deref().unwrap_or(&session.internal_id))
                        .with_attribute(
                            "internalId",
                            serde_json::Value::String(session.internal_id.clone()),
                        )
                        .with_attribute(
                            "workingDirectories",
                            serde_json::to_value(&session.working_directories)
                                .expect("working directories are serializable"),
                        );
                    if let Some(chat_uri) = &session.chat_uri {
                        facts = facts
                            .with_attribute("chatUri", serde_json::Value::String(chat_uri.clone()));
                    }
                    if let Some(disconnected) = session.disconnected {
                        facts = facts
                            .with_attribute("disconnected", serde_json::Value::Bool(disconnected));
                    }
                    upserts
                        .entry(session_id.clone())
                        .or_insert_with(|| ResourceUpsert::new(session_id.clone(), facts));
                    delta = delta.relate(RelationKind::Contains, process_id.clone(), session_id);
                }
            }

            let windows = tree
                .processes
                .iter()
                .filter_map(|process| {
                    process
                        .window_id
                        .map(|window_id| (window_id, process.window_title.as_deref()))
                })
                .fold(
                    BTreeMap::<u32, Option<&str>>::new(),
                    |mut windows, (window_id, title)| {
                        windows
                            .entry(window_id)
                            .and_modify(|current| {
                                if current.is_none() {
                                    *current = title;
                                }
                            })
                            .or_insert(title);
                        windows
                    },
                );
            for (window_id, title) in windows {
                let window = ResourceId::from_parts(
                    "vscode-window",
                    [tree.root_process_id.to_string(), window_id.to_string()],
                )
                .map_err(|error| error.to_string())?;
                let facts = ResourceFacts::of_kind(ResourceKind::new("vscode-window"))
                    .with_label(
                        title
                            .map(str::to_owned)
                            .unwrap_or_else(|| format!("window {window_id}")),
                    )
                    .with_attribute("windowId", serde_json::Value::from(window_id))
                    .with_attribute(
                        "rootProcessId",
                        serde_json::Value::from(tree.root_process_id),
                    );
                upserts
                    .entry(window.clone())
                    .or_insert_with(|| ResourceUpsert::new(window.clone(), facts));
                if let Some(root) = process_ids.get(&tree.root_process_id) {
                    delta = delta.relate(RelationKind::Contains, root.clone(), window.clone());
                }
                for process in tree
                    .processes
                    .iter()
                    .filter(|process| process.window_id == Some(window_id))
                {
                    delta = delta.relate(
                        RelationKind::Contains,
                        window.clone(),
                        process_ids[&process.process_id].clone(),
                    );
                }
            }

            let target_ids = tree
                .targets
                .iter()
                .map(|target| {
                    let id = if target.target.subtype.as_deref() == Some("electron-renderer") {
                        ResourceId::from_parts(
                            "electron-web-contents",
                            [
                                tree.root_process_id.to_string(),
                                target.target.target_id.as_str().to_owned(),
                            ],
                        )
                    } else {
                        ResourceId::from_parts(
                            "discovered-target",
                            [
                                tree.root_process_id.to_string(),
                                target.target.target_id.as_str().to_owned(),
                            ],
                        )
                    }
                    .expect("process target identity parts are non-empty");
                    (target.target.target_id.as_str(), id)
                })
                .collect::<BTreeMap<_, _>>();
            for target in &tree.targets {
                let target_id = target_ids[target.target.target_id.as_str()].clone();
                let facts = ResourceFacts::of_kind(ResourceKind::new(&target.target.target_type))
                    .with_label(if target.target.title.is_empty() {
                        &target.target.target_id
                    } else {
                        &target.target.title
                    })
                    .with_attribute(
                        "targetId",
                        serde_json::Value::String(target.target.target_id.clone()),
                    )
                    .with_attribute(
                        "rootProcessId",
                        serde_json::Value::from(tree.root_process_id),
                    );
                let facts = if let Some(process_id) = target.process_id {
                    facts.with_attribute("processId", serde_json::Value::from(process_id))
                } else {
                    facts
                };
                let facts = if let Some(subtype) = &target.target.subtype {
                    facts.with_attribute("subtype", serde_json::Value::String(subtype.clone()))
                } else {
                    facts
                };
                upserts
                    .entry(target_id.clone())
                    .or_insert_with(|| ResourceUpsert::new(target_id.clone(), facts));
                if let Some(parent) = target
                    .target
                    .parent_id
                    .as_deref()
                    .and_then(|parent| target_ids.get(parent))
                {
                    delta = delta.relate(RelationKind::Contains, parent.clone(), target_id.clone());
                } else if let Some(process) = target
                    .process_id
                    .and_then(|process_id| process_ids.get(&process_id))
                {
                    delta = delta.relate(RelationKind::Hosts, process.clone(), target_id.clone());
                }
            }
        }
        delta.upserts.extend(upserts.into_values());
        graph.apply(delta).map_err(|error| error.to_string())?;
        Ok(())
    })
}

fn process_resource_id(process_id: u32) -> ResourceId {
    ResourceId::from_parts("process", [format!("pid-{process_id}")])
        .expect("process identities are non-empty")
}

fn retract_connection_resource_graph(
    state: &mut ServiceState,
    context_id: &str,
    connection_id: &str,
    generation: u64,
) {
    if let Some(graph) = state.resource_graphs.get(context_id) {
        let connection_source = connection_source_id(connection_id, generation);
        let _: Result<(), std::convert::Infallible> = graph.try_update(|graph| {
            let snapshot = graph.snapshot();
            let connection_resources = snapshot
                .resources
                .iter()
                .filter(|(_, resource)| resource.facets.contains_key(&connection_source))
                .map(|(resource_id, _)| resource_id)
                .collect::<BTreeSet<_>>();
            let session_sources = snapshot
                .relations
                .iter()
                .filter(|relation| {
                    relation.relation.kind == RelationKind::Debugs
                        && connection_resources.contains(&relation.relation.to)
                })
                .flat_map(|relation| relation.contributors.iter().cloned())
                .collect::<BTreeSet<_>>();
            for source in session_sources {
                graph.retract_source(&source);
            }
            graph.retract_source(&connection_source);
            Ok(())
        });
    }
}

#[derive(Clone)]
struct ConnectionTargetResource {
    resource_id: ResourceId,
    target: TargetSnapshot,
}

enum ConnectionTargetUpdate {
    Upsert(TargetSnapshot),
    Remove(String),
}

fn target_snapshot_from_facts(facts: &ResourceFacts) -> Option<TargetSnapshot> {
    let string = |name: &str| {
        facts
            .attributes
            .get(name)
            .and_then(serde_json::Value::as_str)
            .map(str::to_owned)
    };
    Some(TargetSnapshot {
        target_id: string("targetId")?,
        target_type: string("targetType").unwrap_or_else(|| "other".to_owned()),
        title: string("title").unwrap_or_default(),
        url: string("url").unwrap_or_default(),
        attached: facts
            .attributes
            .get("attached")
            .and_then(serde_json::Value::as_bool)
            .unwrap_or(false),
        parent_id: string("parentTargetId"),
        opener_id: string("openerTargetId"),
        browser_context_id: string("browserContextId"),
        subtype: string("subtype"),
    })
}

fn connection_targets_from_graph(
    snapshot: &crate::resource_graph::GraphSnapshot,
    connection_id: &str,
    generation: u64,
) -> BTreeMap<String, ConnectionTargetResource> {
    let source = connection_source_id(connection_id, generation);
    snapshot
        .resources
        .iter()
        .filter_map(|(resource_id, resource)| {
            let facts = resource.facets.get(&source)?;
            let target = target_snapshot_from_facts(facts)?;
            Some((
                target.target_id.clone(),
                ConnectionTargetResource {
                    resource_id: resource_id.clone(),
                    target,
                },
            ))
        })
        .collect()
}

fn context_connection_targets(
    state: &ServiceState,
    context_id: &str,
    connection_id: &str,
    generation: u64,
) -> BTreeMap<String, ConnectionTargetResource> {
    let targets = state
        .resource_graphs
        .get(context_id)
        .map(GraphSink::snapshot)
        .map(|snapshot| connection_targets_from_graph(&snapshot, connection_id, generation))
        .unwrap_or_default();
    let configuration = state
        .contexts
        .get(context_id)
        .and_then(|context| context.connections.get(connection_id))
        .map(|connection| &connection.configuration);
    filter_connection_scope(configuration, targets)
}

fn context_connection_target(
    state: &ServiceState,
    context_id: &str,
    connection_id: &str,
    generation: u64,
    target_id: &str,
) -> Option<ConnectionTargetResource> {
    context_connection_targets(state, context_id, connection_id, generation).remove(target_id)
}

fn context_connection_has_target(
    state: &ServiceState,
    context_id: &str,
    connection_id: &str,
    generation: u64,
    target_id: &str,
) -> bool {
    context_connection_target(state, context_id, connection_id, generation, target_id).is_some()
}

fn prepare_connection_target_update(
    state: &ServiceState,
    context_id: &str,
    connection_id: &str,
    generation: u64,
    update: ConnectionTargetUpdate,
) -> Option<(
    BTreeMap<String, TargetSnapshot>,
    TargetGraphChange,
    Vec<String>,
)> {
    let configuration = state
        .contexts
        .get(context_id)
        .and_then(|context| context.connections.get(connection_id))
        .map(|connection| &connection.configuration);
    let mut targets = context_connection_targets(state, context_id, connection_id, generation)
        .into_iter()
        .map(|(target_id, resource)| (target_id, resource.target))
        .collect::<BTreeMap<_, _>>();
    let previous_target_ids = targets.keys().cloned().collect::<BTreeSet<_>>();
    let (targets, change) = match update {
        ConnectionTargetUpdate::Upsert(target) => {
            let target_id = target.target_id.clone();
            let change = match targets.insert(target_id.clone(), target) {
                Some(previous) if previous == targets[&target_id] => return None,
                Some(_) => TargetGraphChange::Changed {
                    target_id: target_id.clone(),
                },
                None => TargetGraphChange::Created {
                    target_id: target_id.clone(),
                },
            };
            let targets = filter_target_snapshot_scope(configuration, targets);
            if !targets.contains_key(&target_id) {
                return None;
            }
            (targets, change)
        }
        ConnectionTargetUpdate::Remove(target_id) => {
            targets.remove(&target_id)?;
            (
                filter_target_snapshot_scope(configuration, targets),
                TargetGraphChange::Removed { target_id },
            )
        }
    };
    let target_ids = targets.keys().cloned().collect::<BTreeSet<_>>();
    let removed_targets = previous_target_ids
        .difference(&target_ids)
        .cloned()
        .collect();
    Some((targets, change, removed_targets))
}

fn debugged_resources(snapshot: &crate::resource_graph::GraphSnapshot) -> BTreeSet<ResourceId> {
    snapshot
        .relations
        .iter()
        .filter(|relation| relation.relation.kind == RelationKind::Debugs)
        .map(|relation| relation.relation.to.clone())
        .collect()
}

fn filter_connection_scope(
    configuration: Option<&ConnectionConfiguration>,
    targets: BTreeMap<String, ConnectionTargetResource>,
) -> BTreeMap<String, ConnectionTargetResource> {
    let parents = targets
        .iter()
        .map(|(id, target)| (id.clone(), target.target.parent_id.clone()))
        .collect();
    let visible = target_ids_in_connection_scope(configuration, parents);
    targets
        .into_iter()
        .filter(|(target_id, _)| visible.contains(target_id))
        .collect()
}

fn filter_target_snapshot_scope(
    configuration: Option<&ConnectionConfiguration>,
    targets: BTreeMap<String, TargetSnapshot>,
) -> BTreeMap<String, TargetSnapshot> {
    let parents = targets
        .iter()
        .map(|(id, target)| (id.clone(), target.parent_id.clone()))
        .collect();
    let visible = target_ids_in_connection_scope(configuration, parents);
    targets
        .into_iter()
        .filter(|(target_id, _)| visible.contains(target_id))
        .collect()
}

fn target_ids_in_connection_scope(
    configuration: Option<&ConnectionConfiguration>,
    parents: BTreeMap<String, Option<String>>,
) -> BTreeSet<String> {
    let Some(ConnectionConfiguration::ScopedProcessTree { target_id, .. }) = configuration else {
        return parents.into_keys().collect();
    };
    parents
        .keys()
        .filter(|candidate| target_is_within_scope(candidate, target_id, &parents))
        .cloned()
        .collect()
}

fn target_is_within_scope(
    candidate: &str,
    scope_root: &str,
    parents: &BTreeMap<String, Option<String>>,
) -> bool {
    let mut current = Some(candidate);
    let mut visited = BTreeSet::new();
    while let Some(target_id) = current.filter(|target_id| visited.insert((*target_id).to_owned()))
    {
        if target_id == scope_root {
            return true;
        }
        current = parents.get(target_id).and_then(Option::as_deref);
    }
    false
}

fn debug_session_sources_for_target(
    state: &ServiceState,
    context_id: &str,
    connection_id: &str,
    generation: u64,
    target_id: &str,
) -> Vec<SourceId> {
    let Some(target) =
        context_connection_target(state, context_id, connection_id, generation, target_id)
    else {
        return Vec::new();
    };
    state
        .resource_graphs
        .get(context_id)
        .map(|graph| {
            graph.read(|graph| {
                graph
                    .snapshot()
                    .relations
                    .into_iter()
                    .filter(|relation| {
                        relation.relation.kind == RelationKind::Debugs
                            && relation.relation.to == target.resource_id
                    })
                    .flat_map(|relation| relation.contributors)
                    .collect()
            })
        })
        .unwrap_or_default()
}

fn debug_session_source_id(
    owner: &str,
    context_id: &str,
    connection_id: &str,
    generation: u64,
    session_id: &str,
) -> SourceId {
    SourceId::new(format!(
        "debug-session:{owner}:{context_id}:{connection_id}:{generation}:{session_id}"
    ))
}

fn debug_session_resource_id(
    owner: &str,
    context_id: &str,
    connection_id: &str,
    generation: u64,
    session_id: &str,
) -> ResourceId {
    ResourceId::from_parts(
        "debug-session",
        [
            owner,
            context_id,
            connection_id,
            generation.to_string().as_str(),
            session_id,
        ],
    )
    .expect("debug session identity parts are non-empty")
}

fn publish_debug_session_resource(
    state: &ServiceState,
    owner: &str,
    context_id: &str,
    connection_id: &str,
    generation: u64,
    target_id: &str,
    session_id: &str,
) -> Result<SourceId, String> {
    let target = context_connection_target(state, context_id, connection_id, generation, target_id)
        .ok_or_else(|| format!("target '{target_id}' is not present in the resource graph"))?;
    let graph = state
        .resource_graphs
        .get(context_id)
        .ok_or_else(|| format!("context '{context_id}' has no resource graph"))?;
    let source = debug_session_source_id(owner, context_id, connection_id, generation, session_id);
    let session =
        debug_session_resource_id(owner, context_id, connection_id, generation, session_id);
    let facts = ResourceFacts::of_kind(ResourceKind::new("debug-session"))
        .with_label(session_id)
        .with_attribute(
            "contextId",
            serde_json::Value::String(context_id.to_owned()),
        )
        .with_attribute(
            "connectionId",
            serde_json::Value::String(connection_id.to_owned()),
        )
        .with_attribute("generation", serde_json::Value::from(generation))
        .with_attribute("targetId", serde_json::Value::String(target_id.to_owned()))
        .with_attribute(
            "sessionId",
            serde_json::Value::String(session_id.to_owned()),
        )
        .with_attribute("owner", serde_json::Value::String(owner.to_owned()));
    graph
        .apply(
            GraphDelta::for_source(source.clone())
                .upsert(ResourceUpsert::new(session.clone(), facts))
                .relate(RelationKind::Debugs, session, target.resource_id),
        )
        .map_err(|error| error.to_string())?;
    Ok(source)
}

fn retract_debug_session_resource(state: &ServiceState, context_id: &str, source: &SourceId) {
    if let Some(graph) = state.resource_graphs.get(context_id) {
        graph.retract_source(source);
    }
}

fn remove_debugger_registration(
    state: &mut ServiceState,
    key: &(String, String, String),
) -> Option<TargetDebuggerHandle> {
    let debugger = state.target_debuggers.remove(key);
    if let Some(attachment) = state.debug_attachments.remove(key) {
        retract_debug_session_resource(state, &key.0, &attachment.graph_source);
    }
    debugger
}

fn remove_connection_debugger_registrations(
    state: &mut ServiceState,
    context_id: &str,
    connection_id: &str,
) {
    let keys = state
        .target_debuggers
        .keys()
        .filter(|(candidate_context, candidate_connection, _)| {
            candidate_context == context_id && candidate_connection == connection_id
        })
        .cloned()
        .collect::<Vec<_>>();
    for key in keys {
        remove_debugger_registration(state, &key);
    }
}

fn resource_graph_api_snapshot(
    snapshot: crate::resource_graph::GraphSnapshot,
) -> ResourceGraphSnapshot {
    let resources = snapshot
        .resources
        .into_iter()
        .map(|(id, resource)| ResourceSnapshot {
            id: id.to_string(),
            kinds: resource
                .kinds
                .into_iter()
                .map(|kind| kind.to_string())
                .collect(),
            label: resource.label,
            attributes: resource.attributes,
            contributors: resource
                .contributors
                .into_iter()
                .map(|source| source.to_string())
                .collect(),
            capabilities: resource
                .capabilities
                .into_iter()
                .map(|capability| ResourceCapabilitySnapshot {
                    source: capability.source.to_string(),
                    handle: capability.handle.0,
                    kind: capability_kind_name(capability.kind).to_owned(),
                    title: capability.summary.title,
                    detail: capability.summary.detail,
                })
                .collect(),
            frontiers: resource
                .frontiers
                .into_iter()
                .map(|frontier| ResourceFrontierSnapshot {
                    relation: relation_kind_name(&frontier.relation),
                    state: serde_json::to_value(frontier.state)
                        .expect("discovery states are serializable"),
                })
                .collect(),
        })
        .collect();
    let relations = snapshot
        .relations
        .into_iter()
        .map(|relation| ResourceRelationSnapshot {
            kind: relation_kind_name(&relation.relation.kind),
            from: relation.relation.from.to_string(),
            to: relation.relation.to.to_string(),
            contributors: relation
                .contributors
                .into_iter()
                .map(|source| source.to_string())
                .collect(),
        })
        .collect();
    ResourceGraphSnapshot {
        revision: snapshot.revision.0,
        resources,
        relations,
    }
}

fn relay_targets_from_graph(
    snapshot: crate::resource_graph::GraphSnapshot,
) -> Vec<(String, TargetSnapshot)> {
    let debugged = debugged_resources(&snapshot);
    let mut targets = BTreeMap::new();
    for (resource_id, resource) in &snapshot.resources {
        for facts in resource.facets.values() {
            let Some(connection_id) = facts
                .attributes
                .get("connectionId")
                .and_then(serde_json::Value::as_str)
            else {
                continue;
            };
            let Some(mut target) = target_snapshot_from_facts(facts) else {
                continue;
            };
            target.attached |= debugged.contains(resource_id);
            targets.insert(
                (connection_id.to_owned(), target.target_id.clone()),
                (connection_id.to_owned(), target),
            );
        }
    }
    targets.into_values().collect()
}

fn capability_kind_name(kind: CapabilityKind) -> &'static str {
    match kind {
        CapabilityKind::Explore => "explore",
        CapabilityKind::Debug => "debug",
        CapabilityKind::Process => "process",
        CapabilityKind::Browser => "browser",
        CapabilityKind::Frame => "frame",
        CapabilityKind::PauseFutureChildren => "pauseFutureChildren",
    }
}

fn relation_kind_name(kind: &RelationKind) -> String {
    match kind {
        RelationKind::Contains => "contains".to_owned(),
        RelationKind::Spawned => "spawned".to_owned(),
        RelationKind::Hosts => "hosts".to_owned(),
        RelationKind::Debugs => "debugs".to_owned(),
        RelationKind::Other(name) => name.clone(),
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
            ConnectionConfiguration::Stdio { command, .. } => command.clone(),
            _ => "Node.js".to_owned(),
        };
        let target_type = if matches!(configuration, ConnectionConfiguration::Stdio { .. }) {
            "runtime"
        } else {
            "node"
        };
        return Ok((
            connection,
            title.clone(),
            "1.3".to_owned(),
            vec![TargetSnapshot {
                target_id: synthetic_node_target_id(connection_id),
                target_type: target_type.to_owned(),
                title,
                url: match configuration {
                    ConnectionConfiguration::Node { program, .. } => program.clone(),
                    ConnectionConfiguration::NodeInspector { endpoint } => endpoint.clone(),
                    ConnectionConfiguration::Process { process_id } => {
                        format!("process:{process_id}")
                    }
                    ConnectionConfiguration::Stdio { command, .. } => {
                        format!("stdio:{command}")
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
        targets
            .into_iter()
            .map(|target| {
                canonicalize_synthetic_target(target_snapshot_from_info(target), connection_id)
            })
            .collect(),
    ))
}

fn snapshot(
    agent_instance_id: &str,
    id: &str,
    context: &ContextState,
    graph: Option<&crate::resource_graph::GraphSnapshot>,
) -> ContextSnapshot {
    let debugged = graph.map(debugged_resources).unwrap_or_default();
    let mut target_resources = BTreeMap::new();
    let connections = context
        .connections
        .iter()
        .map(|(connection_id, connection)| {
            let targets = graph
                .map(|graph| {
                    connection_targets_from_graph(graph, connection_id, connection.generation)
                })
                .unwrap_or_default();
            let targets = filter_connection_scope(Some(&connection.configuration), targets);
            target_resources.extend(targets.iter().map(|(target_id, target)| {
                (
                    (connection_id.clone(), target_id.clone()),
                    target.resource_id.clone(),
                )
            }));
            ConnectionSnapshot {
                id: connection_id.clone(),
                configuration: connection.configuration.clone(),
                generation: connection.generation,
                status: connection.status.clone(),
                targets: targets.into_values().map(|target| target.target).collect(),
            }
        })
        .collect::<Vec<_>>();
    let mut target_forest = connections
        .iter()
        .flat_map(ConnectionSnapshot::target_forest)
        .collect::<Vec<_>>();
    for target in &mut target_forest {
        if target_resources
            .get(&(
                target.connection_id.clone(),
                target.target.target_id.clone(),
            ))
            .is_some_and(|resource| debugged.contains(resource))
        {
            target.attachment = TargetAttachmentState::Debugger;
        }
    }
    ContextSnapshot {
        agent_instance_id: agent_instance_id.to_owned(),
        id: id.to_owned(),
        display_name: context.display_name.clone(),
        revision: context.revision,
        resource_revision: graph.map_or(0, |graph| graph.revision.0),
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
        source_formatting: context.source_formatting.clone(),
    }
}

fn service_snapshot(
    state: &ServiceState,
    agent_instance_id: &str,
    id: &str,
) -> Option<ContextSnapshot> {
    let context = state.contexts.get(id)?;
    let graph = state.resource_graphs.get(id).map(GraphSink::snapshot);
    let mut result = snapshot(agent_instance_id, id, context, graph.as_ref());
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
                    && breakpoint_applies_to_target(
                        specification.enabled,
                        specification.target_selector.as_deref(),
                        target_id,
                    )
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
    #[serde(default)]
    next_capture_id: u64,
    #[serde(default)]
    next_publication_order: u64,
}

#[derive(Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
struct LegacyStoredServiceState {
    schema_version: u32,
    contexts: BTreeMap<String, StoredContextState>,
    #[serde(default)]
    completed_requests: Vec<StoredCompletedRequest>,
    #[serde(default)]
    captures: Vec<LegacyStoredCapture>,
}

#[derive(Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
struct LegacyStoredCapture {
    metadata: CaptureSnapshot,
    payload: LegacyStoredCapturePayload,
}

#[derive(Serialize, Deserialize)]
#[serde(tag = "kind", content = "value", rename_all = "camelCase")]
enum LegacyStoredCapturePayload {
    Coverage(CoverageSnapshot),
    CpuProfile(CpuProfileSnapshot),
    HeapSnapshot { path: String },
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
    #[serde(default)]
    source_formatting: SourceFormattingSettings,
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
            schema_version: 6,
            next_capture_id: state.next_capture_id,
            next_publication_order: state.next_publication_order,
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
                            source_formatting: context.source_formatting.clone(),
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
    let mut stored = match schema_version {
        1 => migrate_embedded_capture_state(path, migrate_v1(serde_json::from_slice(&bytes)?))?,
        2..=4 => migrate_embedded_capture_state(path, serde_json::from_slice(&bytes)?)?,
        5 | 6 => serde_json::from_slice(&bytes)?,
        version => return Err(ServicePersistenceError::UnsupportedSchema(version as u32)),
    };
    if schema_version < 6 {
        migrate_capture_order(&mut stored);
    }
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
    let mut payload_paths = BTreeSet::new();
    for capture in stored.captures {
        validate_capture_payload_location(path, &capture)?;
        validate_capture_payload_reference(&capture.payload)?;
        if !payload_paths.insert(storage_path_key(Path::new(&capture.payload.path))) {
            return Err(ServicePersistenceError::DuplicateCapturePayload(
                capture.payload.path,
            ));
        }
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
    let state = ServiceState {
        next_capture_id: stored.next_capture_id,
        next_publication_order: stored.next_publication_order,
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
                        source_formatting: context.source_formatting,
                    }),
                )
            })
            .collect(),
        source_models: BTreeMap::new(),
        resource_graphs: BTreeMap::new(),
        process_projections: BTreeMap::new(),
        context_kinds,
        runtimes: BTreeMap::new(),
        target_debuggers: BTreeMap::new(),
        debug_attachments: BTreeMap::new(),
        pause_children_leases: BTreeMap::new(),
        history: BTreeMap::new(),
        completed_requests,
        playwright_proxies: BTreeMap::new(),
        relays: BTreeMap::new(),
        captures,
        capture_reservations: BTreeMap::new(),
    };
    if schema_version < 6 {
        persist_stored_state(path, &StoredServiceState::from(&state))?;
    }
    Ok(state)
}

fn migrate_capture_order(stored: &mut StoredServiceState) {
    let mut keys = stored.captures.iter()
        .map(|capture| (capture.metadata.context_id.clone(), capture.metadata.name.clone()))
        .collect::<BTreeSet<_>>();
    let mut next_id = 1_u64;
    for (index, capture) in stored.captures.iter_mut().enumerate() {
        capture.publication_order = index as u64 + 1;
        let metadata = &mut capture.metadata;
        if crate::service_api::capture_relative_index(&metadata.name).is_ok_and(|index| index.is_some()) {
            loop {
                let name = format!("{}-{next_id}", capture_prefix(metadata.kind));
                next_id += 1;
                if keys.insert((metadata.context_id.clone(), name.clone())) {
                    metadata.name = name;
                    break;
                }
            }
        }
    }
    stored.next_capture_id = next_id;
    stored.next_publication_order = stored.captures.len() as u64 + 1;
    stored.schema_version = 6;
}

fn stored_default_true() -> bool {
    true
}

fn default_context_kind() -> ContextKind {
    ContextKind::Named
}

fn migrate_v1(stored: StoredServiceStateV1) -> LegacyStoredServiceState {
    LegacyStoredServiceState {
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
                        source_formatting: SourceFormattingSettings::default(),
                    },
                )
            })
            .collect(),
        completed_requests: Vec::new(),
        captures: Vec::new(),
    }
}

fn migrate_embedded_capture_state(
    path: &Path,
    legacy: LegacyStoredServiceState,
) -> Result<StoredServiceState, ServicePersistenceError> {
    let mut capture_keys = BTreeSet::new();
    let mut payload_paths = BTreeSet::new();
    for capture in &legacy.captures {
        let key = (
            capture.metadata.context_id.clone(),
            capture.metadata.name.clone(),
        );
        if !capture_keys.insert(key.clone()) {
            return Err(ServicePersistenceError::DuplicateCapture {
                context_id: key.0,
                name: key.1,
            });
        }
        let payload_path = match &capture.payload {
            LegacyStoredCapturePayload::HeapSnapshot { path } => storage_path_key(Path::new(path)),
            LegacyStoredCapturePayload::Coverage(_) | LegacyStoredCapturePayload::CpuProfile(_) => {
                let (_, path) = capture_payload_paths_for(path, &capture.metadata);
                storage_path_key(&path)
            }
        };
        if !payload_paths.insert(payload_path.clone()) {
            return Err(ServicePersistenceError::DuplicateCapturePayload(
                payload_path.to_string_lossy().into_owned(),
            ));
        }
    }
    let captures = legacy
        .captures
        .into_iter()
        .map(|capture| {
            let payload = match capture.payload {
                LegacyStoredCapturePayload::Coverage(snapshot) => {
                    CapturePayload::Coverage(snapshot)
                }
                LegacyStoredCapturePayload::CpuProfile(snapshot) => {
                    CapturePayload::CpuProfile(snapshot)
                }
                LegacyStoredCapturePayload::HeapSnapshot { path } => {
                    CapturePayload::HeapSnapshot { path }
                }
            };
            let payload = write_capture_payload(path, &capture.metadata, &payload)?;
            Ok(StoredCapture {
                metadata: capture.metadata,
                payload,
                publication_order: 0,
                heap_mapping: None,
            })
        })
        .collect::<Result<Vec<_>, ServicePersistenceError>>()?;
    let stored = StoredServiceState {
        schema_version: 5,
        next_capture_id: 0,
        next_publication_order: 0,
        contexts: legacy.contexts,
        completed_requests: legacy.completed_requests,
        captures,
    };
    persist_stored_state(path, &stored)?;
    Ok(stored)
}

fn persist_stored_state(
    path: &Path,
    stored: &StoredServiceState,
) -> Result<(), ServicePersistenceError> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    let bytes = serde_json::to_vec_pretty(stored)?;
    let mut file = AtomicWriteFile::open(path)?;
    file.write_all(&bytes)?;
    file.commit()?;
    Ok(())
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
    #[error("debugger state contains duplicate capture payload path '{0}'")]
    DuplicateCapturePayload(String),
    #[error("invalid capture payload: {0}")]
    CapturePayload(String),
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

fn validate_formatting_pattern(pattern: Option<&str>) -> Result<(), JsonRpcError> {
    if let Some(pattern) = pattern {
        Glob::new(pattern).map_err(|error| {
            invalid_params(&format!("invalid glob pattern '{pattern}': {error}"))
        })?;
    }
    Ok(())
}

struct CompiledFormattingRule {
    mode: SourceFormattingMode,
    target_pattern: Option<GlobMatcher>,
    url_pattern: Option<GlobMatcher>,
}

struct CompiledFormattingSettings {
    default_mode: SourceFormattingMode,
    rules: Vec<CompiledFormattingRule>,
}

fn compile_formatting_settings(
    settings: &SourceFormattingSettings,
) -> Result<CompiledFormattingSettings, JsonRpcError> {
    let compile = |pattern: Option<&str>| {
        pattern
            .map(|pattern| {
                Glob::new(pattern)
                    .map(|glob| glob.compile_matcher())
                    .map_err(|error| {
                        invalid_state(&format!(
                            "persisted source formatting glob '{pattern}' is invalid: {error}"
                        ))
                    })
            })
            .transpose()
    };
    let rules = settings
        .rules
        .iter()
        .map(|rule| {
            Ok(CompiledFormattingRule {
                mode: rule.mode,
                target_pattern: compile(rule.target_pattern.as_deref())?,
                url_pattern: compile(rule.url_pattern.as_deref())?,
            })
        })
        .collect::<Result<_, JsonRpcError>>()?;
    Ok(CompiledFormattingSettings {
        default_mode: settings.default_mode,
        rules,
    })
}

fn effective_formatting_mode(
    settings: &CompiledFormattingSettings,
    target_id: &str,
    source_url: &str,
) -> SourceFormattingMode {
    settings
        .rules
        .iter()
        .fold(settings.default_mode, |mode, rule| {
            if rule
                .target_pattern
                .as_ref()
                .is_none_or(|pattern| pattern.is_match(target_id))
                && rule
                    .url_pattern
                    .as_ref()
                    .is_none_or(|pattern| pattern.is_match(source_url))
            {
                rule.mode
            } else {
                mode
            }
        })
}

fn select_source_views(
    sources: &mut Vec<HydratedSource>,
    settings: &CompiledFormattingSettings,
    target_id: &str,
    preference: SourceViewPreference,
) {
    let formatted = sources
        .iter()
        .filter_map(|source| source.path.strip_suffix("?formatted"))
        .map(str::to_owned)
        .collect::<BTreeSet<_>>();
    let minified = sources
        .iter()
        .filter(|source| source.kind == "runtime")
        .map(|source| {
            (
                source.path.clone(),
                appears_minified(&source.path, &source.content),
            )
        })
        .collect::<BTreeMap<_, _>>();
    sources.retain(|source| {
        if let Some(base) = source.path.strip_suffix("?formatted") {
            return match preference {
                SourceViewPreference::Original => false,
                SourceViewPreference::Formatted => true,
                SourceViewPreference::Policy => {
                    let mode = effective_formatting_mode(settings, target_id, base);
                    mode == SourceFormattingMode::On
                        || mode == SourceFormattingMode::Auto
                            && minified.get(base).copied().unwrap_or(false)
                }
            };
        }
        if source.kind != "runtime" {
            return true;
        }
        if !formatted.contains(&source.path) {
            return preference != SourceViewPreference::Formatted;
        }
        match preference {
            SourceViewPreference::Original => true,
            SourceViewPreference::Formatted => false,
            SourceViewPreference::Policy => {
                let mode = effective_formatting_mode(settings, target_id, &source.path);
                !(mode == SourceFormattingMode::On
                    || mode == SourceFormattingMode::Auto
                        && minified.get(&source.path).copied().unwrap_or(false))
            }
        }
    });
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

fn capture_payload_rpc_error(error: ServicePersistenceError) -> JsonRpcError {
    internal_error(format!("failed to access stored capture payload: {error}"))
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
        | TargetDebuggerError::InvalidValueInspection(_)
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
        | ContextTransitionError::ConnectionAlreadyDisconnecting => error_codes::INVALID_PARAMS,
    };
    JsonRpcError::new(code, error.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn process_projection_contributes_windows_sessions_and_targets_to_the_resource_graph() {
        let process = |process_id, parent_process_id, role, window_id, window_title| {
            crate::service_api::ProcessSnapshot {
                process_id,
                parent_process_id,
                attachable: true,
                debug_target_id: Some(format!("process-{process_id}")),
                name: format!("process-{process_id}.exe"),
                command_line: String::new(),
                creation_date: "1".to_owned(),
                role,
                display_name: None,
                window_id,
                window_title,
                cpu_percent: None,
                memory_bytes: None,
                agent_sessions: Vec::new(),
            }
        };
        let mut root = process(
            100,
            None,
            crate::service_api::ProcessRole::VscodeMain,
            None,
            None,
        );
        root.agent_sessions
            .push(crate::service_api::AgentSessionSnapshot {
                internal_id: "session-1".to_owned(),
                chat_uri: Some("agent-host-session://session-1".to_owned()),
                title: Some("Session".to_owned()),
                working_directories: vec!["D:\\work".to_owned()],
                disconnected: Some(false),
            });
        let tree = ProcessTreeSnapshot {
            root_process_id: 100,
            root_kind: crate::service_api::ProcessRootKind::Vscode,
            processes: vec![
                root,
                process(
                    200,
                    Some(100),
                    crate::service_api::ProcessRole::Renderer,
                    Some(7),
                    Some("Project".to_owned()),
                ),
            ],
            runtime_metadata_available: true,
            targets: vec![crate::service_api::ProcessTargetSnapshot {
                process_id: Some(200),
                target: TargetSnapshot {
                    target_id: "renderer-7".to_owned(),
                    target_type: "page".to_owned(),
                    title: "Project".to_owned(),
                    url: "vscode-file://workbench.html".to_owned(),
                    attached: false,
                    parent_id: None,
                    opener_id: None,
                    browser_context_id: None,
                    subtype: Some("electron-renderer".to_owned()),
                },
            }],
            targets_observed: true,
            target_discovery_error: None,
        };
        let overlapping_tree = ProcessTreeSnapshot {
            root_process_id: 200,
            root_kind: crate::service_api::ProcessRootKind::Node,
            processes: vec![process(
                200,
                None,
                crate::service_api::ProcessRole::Node,
                None,
                None,
            )],
            runtime_metadata_available: false,
            targets: Vec::new(),
            targets_observed: false,
            target_discovery_error: None,
        };
        let mut state = ServiceState::default();

        stage_process_resource_graph(&mut state, "test", &[tree, overlapping_tree]).unwrap();

        let snapshot = state.resource_graphs["test"].snapshot();
        let ids = snapshot
            .resources
            .keys()
            .map(ResourceId::as_str)
            .collect::<BTreeSet<_>>();
        assert!(ids.contains("process/pid-100"));
        assert!(ids.contains("process/pid-200"));
        assert!(ids.contains("vscode-window/100/7"));
        assert!(ids.contains("agent-session/session-1"));
        assert!(ids.contains("electron-web-contents/100/renderer-7"));
        assert!(snapshot.relations.iter().any(|relation| {
            relation.relation.kind == RelationKind::Contains
                && relation.relation.from.as_str() == "vscode-window/100/7"
                && relation.relation.to.as_str() == "process/pid-200"
        }));
        assert!(snapshot.relations.iter().any(|relation| {
            relation.relation.kind == RelationKind::Contains
                && relation.relation.from.as_str() == "process/pid-100"
                && relation.relation.to.as_str() == "agent-session/session-1"
        }));
    }

    #[test]
    fn scoped_process_tree_exposes_only_the_selected_target_subtree() {
        let target = |id: &str, parent_id: Option<&str>| ConnectionTargetResource {
            resource_id: ResourceId::from_parts("target", [id]).unwrap(),
            target: TargetSnapshot {
                target_id: id.to_owned(),
                target_type: "page".to_owned(),
                title: id.to_owned(),
                url: String::new(),
                attached: false,
                parent_id: parent_id.map(str::to_owned),
                opener_id: None,
                browser_context_id: None,
                subtype: None,
            },
        };
        let targets = BTreeMap::from([
            ("main".to_owned(), target("main", None)),
            ("renderer-a".to_owned(), target("renderer-a", Some("main"))),
            ("page-a".to_owned(), target("page-a", Some("renderer-a"))),
            ("renderer-b".to_owned(), target("renderer-b", Some("main"))),
            ("page-b".to_owned(), target("page-b", Some("renderer-b"))),
        ]);

        let visible = filter_connection_scope(
            Some(&ConnectionConfiguration::ScopedProcessTree {
                root_pid: 100,
                target_id: "renderer-a".to_owned(),
            }),
            targets,
        );

        assert_eq!(
            visible.keys().cloned().collect::<Vec<_>>(),
            vec!["page-a".to_owned(), "renderer-a".to_owned()]
        );
    }

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
                    .map(|(connection_id, generation, _targets)| {
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
                            }),
                        )
                    })
                    .collect(),
            ),
            breakpoints: Arc::new(BTreeMap::new()),
            source_formatting: SourceFormattingSettings::default(),
        })
    }

    fn insert_context_with_targets(
        state: &mut ServiceState,
        context_id: &str,
        connections: impl IntoIterator<Item = (&'static str, u64, Vec<TargetSnapshot>)>,
    ) {
        let connections = connections.into_iter().collect::<Vec<_>>();
        let context = context_with_targets(connections.clone());
        let graph = SharedResourceGraph::new();
        graph
            .try_update(|graph| {
                for (connection_id, generation, targets) in &connections {
                    let source = connection_source_id(connection_id, *generation);
                    let root = connection_root_resource_id(connection_id, *generation);
                    let mut delta = GraphDelta::for_source(source).upsert(
                        ResourceUpsert::new(
                            root.clone(),
                            ResourceFacts::of_kind(ResourceKind::new("browser"))
                                .with_label(*connection_id)
                                .with_attribute(
                                    "connectionId",
                                    serde_json::Value::String((*connection_id).to_owned()),
                                )
                                .with_attribute("generation", serde_json::Value::from(*generation)),
                        )
                        .with_frontier(RelationKind::Contains, DiscoveryState::Live),
                    );
                    for target in targets {
                        let resource = ResourceId::from_parts(
                            "cdp-target",
                            [
                                *connection_id,
                                generation.to_string().as_str(),
                                target.target_id.as_str(),
                            ],
                        )
                        .unwrap();
                        let mut facts =
                            ResourceFacts::of_kind(ResourceKind::new(&target.target_type))
                                .with_label(if target.title.is_empty() {
                                    target.target_id.clone()
                                } else {
                                    target.title.clone()
                                })
                                .with_attribute(
                                    "targetId",
                                    serde_json::Value::String(target.target_id.clone()),
                                )
                                .with_attribute(
                                    "connectionId",
                                    serde_json::Value::String((*connection_id).to_owned()),
                                )
                                .with_attribute(
                                    "targetType",
                                    serde_json::Value::String(target.target_type.clone()),
                                )
                                .with_attribute(
                                    "title",
                                    serde_json::Value::String(target.title.clone()),
                                )
                                .with_attribute(
                                    "url",
                                    serde_json::Value::String(target.url.clone()),
                                )
                                .with_attribute(
                                    "attached",
                                    serde_json::Value::from(target.attached),
                                );
                        for (name, value) in [
                            ("parentTargetId", target.parent_id.as_ref()),
                            ("openerTargetId", target.opener_id.as_ref()),
                            ("browserContextId", target.browser_context_id.as_ref()),
                            ("subtype", target.subtype.as_ref()),
                        ] {
                            if let Some(value) = value {
                                facts = facts
                                    .with_attribute(name, serde_json::Value::String(value.clone()));
                            }
                        }
                        delta = delta
                            .upsert(ResourceUpsert::new(resource.clone(), facts))
                            .relate(RelationKind::Contains, root.clone(), resource);
                    }
                    graph.apply(delta)?;
                }
                Ok::<_, crate::resource_graph::GraphError>(())
            })
            .unwrap();
        state.contexts.insert(context_id.to_owned(), context);
        state.resource_graphs.insert(context_id.to_owned(), graph);
    }

    fn service_with_state(path: PathBuf, state: ServiceState) -> DebuggerService {
        let (shutdown, _) = watch::channel(false);
        let (revision_signal, _) = watch::channel(0);
        DebuggerService {
            agent_instance_id: "test-agent".into(),
            state: Arc::new(Mutex::new(state)),
            attachment_lock: Arc::new(Mutex::new(())),
            relay_lifecycle_lock: Arc::new(Mutex::new(())),
            relay_attachment_lock: Arc::new(Mutex::new(())),
            relay_attachments: Arc::new(Mutex::new(BTreeMap::new())),
            persistence_path: path,
            shutdown,
            revision_signal,
            capture_storage: CaptureStorage::default(),
        }
    }

    fn heap_capture(
        context_id: &str,
        name: &str,
        target_id: &str,
        connection_id: &str,
        path: &Path,
    ) -> StoredCapture {
        let metadata = capture_metadata(
            context_id,
            name,
            CaptureKind::HeapSnapshot,
            target_id,
            connection_id,
        );
        StoredCapture {
            metadata,
            payload: payload_reference_from_file(path).unwrap(),
            publication_order: 0,
            heap_mapping: None,
        }
    }

    fn capture_metadata(
        context_id: &str,
        name: &str,
        kind: CaptureKind,
        target_id: &str,
        connection_id: &str,
    ) -> CaptureSnapshot {
        CaptureSnapshot {
            context_id: context_id.into(),
            name: name.into(),
            kind,
            target_id: target_id.into(),
            connection_id: connection_id.into(),
            connection_generation: 1,
            storage_id: format!("storage-{name}"),
        }
    }

    fn stored_capture_from_payload(
        persistence_path: &Path,
        metadata: CaptureSnapshot,
        payload: CapturePayload,
    ) -> StoredCapture {
        let reference = write_capture_payload(persistence_path, &metadata, &payload).unwrap();
        StoredCapture {
            metadata,
            payload: reference,
            publication_order: 0,
            heap_mapping: None,
        }
    }

    fn load_stored_capture(capture: &StoredCapture) -> CapturePayload {
        load_capture_payload(&capture.payload, capture.metadata.kind).unwrap()
    }

    fn capture_retry_service(label: &str) -> (PathBuf, PathBuf, DebuggerService) {
        let root = std::env::current_dir()
            .unwrap()
            .join("target")
            .join(format!("{label}-{}", random_instance_id().unwrap()));
        fs::create_dir_all(&root).unwrap();
        let blocker = root.join("service.json");
        fs::create_dir(&blocker).unwrap();
        let mut state = ServiceState::default();
        insert_context_with_targets(
            &mut state,
            "test",
            [(
                "runtime",
                1,
                vec![target("target-a", "A", "https://a.test")],
            )],
        );
        let service = service_with_state(blocker.clone(), state);
        (root, blocker, service)
    }

    fn unblock_capture_persistence(blocker: &Path) {
        fs::remove_dir(blocker).unwrap();
    }

    #[tokio::test]
    async fn capture_name_reservation_is_atomic_across_targets() {
        let mut state = ServiceState::default();
        insert_context_with_targets(
            &mut state,
            "test",
            [
                ("first", 1, vec![target("target-a", "A", "https://a.test")]),
                ("second", 1, vec![target("target-b", "B", "https://b.test")]),
            ],
        );
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

    fn capture_catalog_service() -> (PathBuf, DebuggerService) {
        let root = std::env::current_dir().unwrap().join("target")
            .join(format!("capture-catalog-{}", random_instance_id().unwrap()));
        let mut state = ServiceState::default();
        insert_context_with_targets(
            &mut state, "test",
            [("runtime", 1, vec![
                target("target-a", "A", "https://a.test"),
                target("target-b", "B", "https://b.test"),
            ])],
        );
        (root.clone(), service_with_state(root.join("service.json"), state))
    }

    fn empty_coverage(timestamp_micros: u64) -> CapturePayload {
        CapturePayload::Coverage(CoverageSnapshot {
            capture_id: None,
            timestamp_micros, sources: Vec::new(), analysis: None,
        })
    }

    #[tokio::test]
    async fn generated_capture_ids_and_publication_order_survive_restart() {
        let (root, service) = capture_catalog_service();
        let first = service.reserve_capture_optional(
            "test", "runtime", "target-a", 1, None, CaptureKind::Coverage,
        ).await.unwrap();
        let second = service.reserve_capture_optional(
            "test", "runtime", "target-b", 1, None, CaptureKind::Coverage,
        ).await.unwrap();
        assert_eq!(first.metadata.name, "cov-1");
        assert_eq!(second.metadata.name, "cov-2");
        service.store_capture(&second, empty_coverage(2)).await.unwrap();
        service.store_capture(&first, empty_coverage(1)).await.unwrap();
        let snapshot = service.get_stored_coverage(
            &CallCtx::default(), "test".into(), ".1".into(), None, None, None, None,
        ).await.unwrap();
        assert_eq!(snapshot.capture_id.as_deref(), Some("cov-1"));
        let restored = load_state(&root.join("service.json")).unwrap();
        for selector in [".", ".1"] {
            assert_eq!(select_stored_capture(
                &restored, "test", selector, Some(CaptureKind::Coverage), None, None,
            ).unwrap().metadata.name, "cov-1");
        }
        assert_eq!(select_stored_capture(
            &restored, "test", ".2", Some(CaptureKind::Coverage), None, None,
        ).unwrap().metadata.name, "cov-2");
        assert_eq!(select_stored_capture(
            &restored, "test", ".", Some(CaptureKind::Coverage), Some("target-b"), None,
        ).unwrap().metadata.name, "cov-2");
        for target in ["runtime/target-b", "runtime/target-b@1"] {
            assert_eq!(select_stored_capture(
                &restored, "test", ".", Some(CaptureKind::Coverage), Some(target), None,
            ).unwrap().metadata.name, "cov-2");
        }
        assert!(select_stored_capture(
            &restored, "test", ".", Some(CaptureKind::Coverage), Some("runtime/target-b@2"), None,
        ).is_err());
        assert!(select_stored_capture(
            &restored, "test", ".", Some(CaptureKind::Coverage), None, Some("other-runtime"),
        ).is_err());
        assert!(select_stored_capture(
            &restored, "test", ".2", Some(CaptureKind::Coverage), Some("target-b"), None,
        ).is_err());
        assert!(select_stored_capture(
            &restored, "test", ".", Some(CaptureKind::CpuProfile), None, None,
        ).is_err());
        assert!(select_stored_capture(
            &restored, "test", "cov-1", Some(CaptureKind::Coverage), Some("target-b"), None,
        ).is_err());
        assert_eq!(restored.next_capture_id, 3);
        assert_eq!(restored.next_publication_order, 3);
        drop(service);
        fs::remove_dir_all(root).unwrap();
    }

    #[tokio::test]
    async fn automatic_names_skip_named_reservations_and_are_never_reused_after_deletion() {
        let (root, service) = capture_catalog_service();
        service.reserve_capture(
            "test", "runtime", "target-a", 1, "cov-1".into(), CaptureKind::CpuProfile,
        ).await.unwrap();
        let automatic = service.reserve_capture_optional(
            "test", "runtime", "target-b", 1, None, CaptureKind::Coverage,
        ).await.unwrap();
        assert_eq!(automatic.metadata.name, "cov-2");
        service.store_capture(&automatic, empty_coverage(1)).await.unwrap();
        service.delete_capture(&CallCtx::default(), "test".into(), "cov-2".into()).await.unwrap();
        assert_eq!(load_state(&root.join("service.json")).unwrap().next_capture_id, 3);
        let next = service.reserve_capture_optional(
            "test", "runtime", "target-b", 1, None, CaptureKind::Coverage,
        ).await.unwrap();
        assert_eq!(next.metadata.name, "cov-3");
        for selector in [".", ".1", ".2", ".0"] {
            assert!(service.reserve_capture(
                "test", "runtime", "target-a", 1, selector.into(), CaptureKind::Coverage,
            ).await.is_err());
        }
        assert!(service.reserve_capture(
            "test", "runtime", "target-b", 1, "cov-1".into(), CaptureKind::Coverage,
        ).await.is_err());
        drop(service);
        fs::remove_dir_all(root).unwrap();
    }

    #[tokio::test]
    async fn legacy_dot_capture_migration_keeps_immutable_payloads_and_avoids_named_ids() {
        let (root, service) = capture_catalog_service();
        let payload = empty_coverage(42);
        let legacy = stored_capture_from_payload(
            &root.join("service.json"),
            capture_metadata("test", ".", CaptureKind::Coverage, "target-a", "runtime"),
            payload.clone(),
        );
        let named = stored_capture_from_payload(
            &root.join("service.json"),
            capture_metadata("test", "cov-1", CaptureKind::Coverage, "target-b", "runtime"),
            payload,
        );
        let legacy_payload = legacy.payload.clone();
        let mut stored = StoredServiceState::from(&*service.state.lock().await);
        stored.schema_version = 5;
        stored.captures = vec![legacy, named];
        persist_stored_state(&root.join("service.json"), &stored).unwrap();
        let restored = load_state(&root.join("service.json")).unwrap();
        assert_eq!(restored.captures.len(), 2);
        let migrated = &restored.captures[&("test".into(), "cov-2".into())];
        assert_eq!(migrated.payload.path, legacy_payload.path);
        assert_eq!(migrated.payload.sha256, legacy_payload.sha256);
        assert_eq!(migrated.metadata.storage_id, "storage-.");
        assert_eq!(load_state(&root.join("service.json")).unwrap().captures.len(), 2);
        let persisted: serde_json::Value = serde_json::from_slice(
            &fs::read(root.join("service.json")).unwrap(),
        ).unwrap();
        assert_eq!(persisted["schemaVersion"], 6);
        drop(service);
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn relative_capture_selector_grammar_has_one_positive_index_form() {
        use crate::service_api::capture_relative_index;
        assert_eq!(capture_relative_index(".").unwrap(), Some(1));
        assert_eq!(capture_relative_index(".1").unwrap(), Some(1));
        assert_eq!(capture_relative_index(".2").unwrap(), Some(2));
        assert_eq!(capture_relative_index("before-click").unwrap(), None);
        assert!(capture_relative_index(".0").is_err());
        assert!(capture_relative_index(".999999999999999999999999").is_err());
    }

    #[test]
    fn coverage_capture_id_is_a_backward_compatible_optional_wire_field() {
        let legacy = serde_json::json!({"timestampMicros": 1, "sources": []});
        let mut snapshot: CoverageSnapshot = serde_json::from_value(legacy).unwrap();
        assert_eq!(snapshot.capture_id, None);
        assert!(serde_json::to_value(&snapshot).unwrap().get("captureId").is_none());
        snapshot.capture_id = Some("cov-1".into());
        assert_eq!(serde_json::to_value(snapshot).unwrap()["captureId"], "cov-1");
    }

    #[tokio::test]
    async fn capture_finalization_rechecks_connection_generation() {
        let mut state = ServiceState::default();
        insert_context_with_targets(
            &mut state,
            "test",
            [(
                "runtime",
                1,
                vec![target("target-a", "A", "https://a.test")],
            )],
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
        {
            let mut state = service.state.lock().await;
            insert_context_with_targets(
                &mut state,
                "test",
                [(
                    "runtime",
                    2,
                    vec![target("target-a", "A", "https://a.test")],
                )],
            );
        }

        let error = service
            .finalize_capture(
                &reservation,
                CapturePayload::HeapSnapshot {
                    path: "unused".into(),
                },
                None,
            )
            .await
            .unwrap_err();
        assert!(error.message.contains("generation changed"), "{error:?}");
        let state = service.state.lock().await;
        assert!(state.captures.is_empty());
        assert_eq!(state.capture_reservations.len(), 1);
    }

    #[tokio::test]
    async fn completed_cpu_profile_promotes_after_disconnect_without_live_target() {
        let (root, blocker, service) = capture_retry_service("cpu-profile-finalization");
        let reservation = service
            .reserve_capture(
                "test",
                "runtime",
                "target-a",
                1,
                "profile".into(),
                CaptureKind::CpuProfile,
            )
            .await
            .unwrap();
        let snapshot = CpuProfileSnapshot {
            capture_id: "profile".into(),
            sampling_interval_micros: Some(100),
            start_time_micros: 1.0,
            end_time_micros: 2.0,
            nodes: Vec::new(),
            samples: Vec::new(),
            time_deltas_micros: Vec::new(),
            functions: Vec::new(),
            analysis: None,
        };

        assert!(
            service
                .store_capture(&reservation, CapturePayload::CpuProfile(snapshot.clone()),)
                .await
                .is_err()
        );
        assert!(!service.abandon_capture(&reservation).await);
        {
            let state = service.state.lock().await;
            let pending = &state.capture_reservations[&("test".to_owned(), "profile".to_owned())];
            assert!(matches!(
                pending.completed.as_ref().map(|value| {
                    load_capture_payload(&value.payload, CaptureKind::CpuProfile).unwrap()
                }),
                Some(CapturePayload::CpuProfile(value)) if value == snapshot
            ));
            assert!(state.captures.is_empty());
        }

        {
            let mut state = service.state.lock().await;
            insert_context_with_targets(&mut state, "test", []);
        }
        unblock_capture_persistence(&blocker);
        let retried = service
            .stop_cpu_profile(
                &CallCtx::default(),
                "test".into(),
                "runtime".into(),
                "target-a".into(),
                Some("profile".into()),
            )
            .await
            .unwrap();
        assert_eq!(retried, snapshot);
        let state = service.state.lock().await;
        assert!(state.capture_reservations.is_empty());
        assert!(matches!(
            load_stored_capture(&state.captures[&("test".to_owned(), "profile".to_owned())]),
            CapturePayload::CpuProfile(value) if value == snapshot
        ));
        drop(state);
        drop(service);

        let (shutdown, _) = watch::channel(false);
        let restored = DebuggerService::load(shutdown, blocker).unwrap();
        assert!(matches!(
            load_stored_capture(
                &restored.state.lock().await.captures
                    [&("test".to_owned(), "profile".to_owned())]
            ),
            CapturePayload::CpuProfile(value) if value == snapshot
        ));
        drop(restored);
        let _ = fs::remove_dir_all(root);
    }

    #[tokio::test]
    async fn stored_coverage_path_filters_normalized_directory_descendants() {
        let root = std::env::current_dir()
            .unwrap()
            .join("target")
            .join(format!("coverage-filter-{}", random_instance_id().unwrap()));
        let persistence_path = root.join("service.json");
        let mut state = ServiceState::default();
        insert_context_with_targets(
            &mut state,
            "test",
            [(
                "runtime",
                1,
                vec![target("target-a", "A", "https://a.test")],
            )],
        );
        let source =
            |script_id: &str, generated_url: &str, associated_authored_source: Option<&str>| {
                crate::service_api::CoverageSourceSnapshot {
                    script_id: script_id.into(),
                    generated_url: generated_url.into(),
                    associated_authored_source: associated_authored_source.map(str::to_owned),
                    functions: Vec::new(),
                }
            };
        let snapshot = CoverageSnapshot {
            capture_id: None,
            timestamp_micros: 42,
            sources: vec![
                source("1", "./src/index.ts", None),
                source("2", "dist/bundle.js", Some("src/nested/worker.ts")),
                source("3", "src-other/not-a-child.ts", None),
                source("4", "src", None),
            ],
            analysis: None,
        };
        state.captures.insert(
            ("test".into(), "coverage".into()),
            stored_capture_from_payload(
                &persistence_path,
                capture_metadata(
                    "test",
                    "coverage",
                    CaptureKind::Coverage,
                    "target-a",
                    "runtime",
                ),
                CapturePayload::Coverage(snapshot),
            ),
        );
        let service = service_with_state(persistence_path, state);

        let filtered = service
            .get_stored_coverage(
                &CallCtx::default(),
                "test".into(),
                "coverage".into(),
                Some("../src/".into()),
                None,
                None,
                None,
            )
            .await
            .unwrap();

        assert_eq!(
            filtered
                .sources
                .iter()
                .map(|source| source.script_id.as_str())
                .collect::<Vec<_>>(),
            vec!["1", "2", "4"]
        );
        let _ = fs::remove_dir_all(root);
    }

    #[tokio::test]
    async fn stored_cpu_profile_rebuilds_analysis_without_live_target() {
        let root = std::env::current_dir()
            .unwrap()
            .join("target")
            .join(format!("cpu-offline-{}", random_instance_id().unwrap()));
        let persistence_path = root.join("service.json");
        let snapshot = CpuProfileSnapshot {
            capture_id: "profile".into(),
            sampling_interval_micros: Some(100),
            start_time_micros: 1.0,
            end_time_micros: 2.0,
            nodes: vec![crate::service_api::CpuProfileNodeSnapshot {
                id: 1,
                call_frame: crate::service_api::CpuProfileCallFrameSnapshot {
                    function_name: "work".into(),
                    script_id: "1".into(),
                    url: "src/work.ts".into(),
                    line_number: 4,
                    column_number: 2,
                },
                hit_count: Some(1),
                children: Vec::new(),
                deopt_reason: None,
                position_ticks: Vec::new(),
                authored_location: None,
                breadcrumb: None,
                self_time_micros: 0,
                total_time_micros: 0,
                sample_count: 0,
            }],
            samples: vec![1],
            time_deltas_micros: vec![250],
            functions: Vec::new(),
            analysis: None,
        };
        let mut state = ServiceState::default();
        state.captures.insert(
            ("test".into(), "profile".into()),
            stored_capture_from_payload(
                &persistence_path,
                capture_metadata(
                    "test",
                    "profile",
                    CaptureKind::CpuProfile,
                    "target-a",
                    "runtime",
                ),
                CapturePayload::CpuProfile(snapshot),
            ),
        );
        let service = service_with_state(persistence_path, state);

        let profile = service
            .get_stored_cpu_profile(&CallCtx::default(), "test".into(), "profile".into(), None, None, None)
            .await
            .unwrap();

        assert_eq!(profile.functions.len(), 1);
        assert_eq!(profile.functions[0].name, "work");
        assert_eq!(profile.functions[0].self_time_micros, 250);
        assert_eq!(profile.functions[0].total_time_micros, 250);
        let _ = fs::remove_dir_all(root);
    }

    #[tokio::test]
    async fn completed_coverage_survives_persistence_failure_and_retries() {
        let (root, blocker, service) = capture_retry_service("coverage-finalization");
        let reservation = service
            .reserve_capture(
                "test",
                "runtime",
                "target-a",
                1,
                "coverage".into(),
                CaptureKind::Coverage,
            )
            .await
            .unwrap();
        let snapshot = CoverageSnapshot {
            capture_id: None,
            timestamp_micros: 42,
            sources: Vec::new(),
            analysis: None,
        };

        assert!(
            service
                .store_capture(&reservation, CapturePayload::Coverage(snapshot.clone()),)
                .await
                .is_err()
        );
        unblock_capture_persistence(&blocker);
        let completed = service
            .promote_completed_capture(
                "test",
                "runtime",
                "target-a",
                "coverage",
                CaptureKind::Coverage,
            )
            .await
            .unwrap()
            .unwrap();
        assert!(matches!(
            completed.payload,
            CapturePayload::Coverage(value) if value == snapshot
        ));
        assert!(matches!(
            load_stored_capture(
                &service.state.lock().await.captures
                    [&("test".to_owned(), "coverage".to_owned())]
            ),
            CapturePayload::Coverage(value) if value == snapshot
        ));
        let _ = fs::remove_dir_all(root);
    }

    #[tokio::test]
    async fn completed_heap_capture_survives_persistence_failure_and_retries() {
        let root = std::env::current_dir()
            .unwrap()
            .join("target")
            .join(format!(
                "heap-finalization-{}",
                random_instance_id().unwrap()
            ));
        fs::create_dir_all(&root).unwrap();
        let persistence_path = root.join("service.json");
        fs::create_dir(&persistence_path).unwrap();
        let mut state = ServiceState::default();
        insert_context_with_targets(
            &mut state,
            "test",
            [(
                "runtime",
                1,
                vec![target("target-a", "A", "https://a.test")],
            )],
        );
        let service = service_with_state(persistence_path.clone(), state);
        let reservation = service
            .reserve_capture(
                "test",
                "runtime",
                "target-a",
                1,
                "heap".into(),
                CaptureKind::HeapSnapshot,
            )
            .await
            .unwrap();
        let (_, final_path) = service.heap_capture_paths(&reservation);
        fs::create_dir_all(final_path.parent().unwrap()).unwrap();
        fs::write(&final_path, b"completed heap").unwrap();
        let result = HeapCaptureResult {
            mapping: None,
            capture_id: "heap".into(),
            bytes_written: 14,
            timing: Default::default(),
        };
        let payload = CapturePayload::HeapSnapshot {
            path: final_path.to_string_lossy().into_owned(),
        };

        assert!(
            service
                .store_heap_capture(&reservation, payload.clone(), result.clone())
                .await
                .is_err()
        );
        assert!(final_path.exists());
        fs::remove_dir(&persistence_path).unwrap();
        let completed = service
            .promote_completed_capture(
                "test",
                "runtime",
                "target-a",
                "heap",
                CaptureKind::HeapSnapshot,
            )
            .await
            .unwrap()
            .unwrap();
        assert_eq!(completed.heap_result, Some(result));
        assert!(matches!(
            completed.payload,
            CapturePayload::HeapSnapshot { ref path }
                if path == &final_path.to_string_lossy()
        ));
        assert!(final_path.exists());
        assert!(matches!(
            load_stored_capture(
                &service.state.lock().await.captures
                    [&("test".to_owned(), "heap".to_owned())]
            ),
            CapturePayload::HeapSnapshot { path }
                if path == final_path.to_string_lossy()
        ));
        let _ = fs::remove_dir_all(root);
    }

    #[tokio::test]
    async fn completed_capture_reservation_can_be_explicitly_discarded() {
        let root = std::env::current_dir()
            .unwrap()
            .join("target")
            .join(format!("capture-discard-{}", random_instance_id().unwrap()));
        fs::create_dir_all(&root).unwrap();
        let persistence_path = root.join("service.json");
        fs::create_dir(&persistence_path).unwrap();
        let mut state = ServiceState::default();
        insert_context_with_targets(
            &mut state,
            "test",
            [(
                "runtime",
                1,
                vec![target("target-a", "A", "https://a.test")],
            )],
        );
        let service = service_with_state(persistence_path, state);
        let reservation = service
            .reserve_capture(
                "test",
                "runtime",
                "target-a",
                1,
                "discard".into(),
                CaptureKind::HeapSnapshot,
            )
            .await
            .unwrap();
        let (_, final_path) = service.heap_capture_paths(&reservation);
        fs::create_dir_all(final_path.parent().unwrap()).unwrap();
        fs::write(&final_path, b"completed heap").unwrap();
        let payload = CapturePayload::HeapSnapshot {
            path: final_path.to_string_lossy().into_owned(),
        };
        assert!(
            service
                .store_heap_capture(
                    &reservation,
                    payload,
                    HeapCaptureResult {
                        capture_id: "discard".into(),
                        mapping: None,
                        bytes_written: 14,
                        timing: Default::default(),
                    },
                )
                .await
                .is_err()
        );

        service
            .delete_capture(&CallCtx::default(), "test".into(), "discard".into())
            .await
            .unwrap();
        assert!(!final_path.exists());
        assert!(service.state.lock().await.capture_reservations.is_empty());
        let _ = fs::remove_dir_all(root);
    }

    #[tokio::test]
    async fn completed_capture_delete_blocks_promotion_and_restores_after_failure() {
        let (root, blocker, service) = capture_retry_service("capture-delete-promote-race");
        let reservation = service
            .reserve_capture(
                "test",
                "runtime",
                "target-a",
                1,
                "coverage".into(),
                CaptureKind::Coverage,
            )
            .await
            .unwrap();
        let snapshot = CoverageSnapshot {
            capture_id: None,
            timestamp_micros: 42,
            sources: Vec::new(),
            analysis: None,
        };
        assert!(
            service
                .store_capture(&reservation, CapturePayload::Coverage(snapshot.clone()))
                .await
                .is_err()
        );
        let (completed_reservation, completed) = {
            let state = service.state.lock().await;
            let reservation =
                state.capture_reservations[&("test".to_owned(), "coverage".to_owned())].clone();
            let completed = reservation.completed.as_ref().unwrap().clone();
            (reservation, completed)
        };
        let payload_path = completed.payload.path.clone();

        service.capture_storage.pause_next_remove_with_failure();
        let deleting_service = service.clone();
        let deletion = tokio::spawn(async move {
            deleting_service
                .delete_capture(&CallCtx::default(), "test".into(), "coverage".into())
                .await
        });
        service.capture_storage.wait_for_remove().await;
        assert!(
            service.state.lock().await.capture_reservations
                [&("test".to_owned(), "coverage".to_owned())]
                .deleting
        );

        let commit = service
            .commit_completed_capture(&completed_reservation, &completed)
            .await;
        let Err(commit_error) = commit else {
            panic!("completed capture committed during deletion");
        };
        assert!(
            commit_error.message.contains("being deleted"),
            "{commit_error:?}"
        );
        let promotion = service
            .promote_completed_capture(
                "test",
                "runtime",
                "target-a",
                "coverage",
                CaptureKind::Coverage,
            )
            .await;
        let Err(promotion_error) = promotion else {
            panic!("capture promotion succeeded during deletion");
        };
        assert!(
            promotion_error.message.contains("currently being deleted"),
            "{promotion_error:?}"
        );
        let retry = service
            .reserve_capture(
                "test",
                "runtime",
                "target-a",
                1,
                "coverage".into(),
                CaptureKind::Coverage,
            )
            .await;
        let Err(retry_error) = retry else {
            panic!("capture retry reserved the name during deletion");
        };
        assert!(
            retry_error.message.contains("currently being deleted"),
            "{retry_error:?}"
        );
        service.capture_storage.continue_remove();
        let deletion_error = deletion.await.unwrap().unwrap_err();
        assert!(
            deletion_error.message.contains("retryable reservation"),
            "{deletion_error:?}"
        );
        {
            let state = service.state.lock().await;
            let reservation =
                &state.capture_reservations[&("test".to_owned(), "coverage".to_owned())];
            assert!(!reservation.deleting);
            assert!(reservation.completed.is_some());
            assert!(state.captures.is_empty());
        }
        assert!(Path::new(&payload_path).exists());

        unblock_capture_persistence(&blocker);
        let promoted = service
            .promote_completed_capture(
                "test",
                "runtime",
                "target-a",
                "coverage",
                CaptureKind::Coverage,
            )
            .await
            .unwrap()
            .unwrap();
        assert!(matches!(
            promoted.payload,
            CapturePayload::Coverage(value) if value == snapshot
        ));
        drop(service);

        let (shutdown, _) = watch::channel(false);
        let restored = DebuggerService::load(shutdown, blocker).unwrap();
        let state = restored.state.lock().await;
        assert!(matches!(
            load_stored_capture(
                &state.captures[&("test".to_owned(), "coverage".to_owned())]
            ),
            CapturePayload::Coverage(value) if value == snapshot
        ));
        drop(state);
        drop(restored);
        let _ = fs::remove_dir_all(root);
    }

    #[tokio::test]
    async fn payload_sync_failure_prevents_catalog_publication_and_retries() {
        let root = std::env::current_dir()
            .unwrap()
            .join("target")
            .join(format!(
                "capture-sync-failure-{}",
                random_instance_id().unwrap()
            ));
        fs::create_dir_all(&root).unwrap();
        let persistence_path = root.join("service.json");
        let mut state = ServiceState::default();
        insert_context_with_targets(
            &mut state,
            "test",
            [(
                "runtime",
                1,
                vec![target("target-a", "A", "https://a.test")],
            )],
        );
        let service = service_with_state(persistence_path.clone(), state);
        let reservation = service
            .reserve_capture(
                "test",
                "runtime",
                "target-a",
                1,
                "coverage".into(),
                CaptureKind::Coverage,
            )
            .await
            .unwrap();
        let snapshot = CoverageSnapshot {
            capture_id: None,
            timestamp_micros: 42,
            sources: Vec::new(),
            analysis: None,
        };
        let (staging_path, final_path) =
            capture_payload_paths_for(&persistence_path, &reservation.metadata);

        service.capture_storage.fail_next_sync();
        let error = service
            .store_capture(&reservation, CapturePayload::Coverage(snapshot.clone()))
            .await
            .unwrap_err();
        assert!(
            error
                .message
                .contains("injected capture storage sync failure"),
            "{error:?}"
        );
        assert!(staging_path.exists());
        assert!(!final_path.exists());
        {
            let state = service.state.lock().await;
            assert!(state.captures.is_empty());
            assert!(
                state.capture_reservations[&("test".to_owned(), "coverage".to_owned())]
                    .completed
                    .is_none()
            );
        }

        service.capture_storage.fail_next_parent_sync();
        let error = service
            .store_capture(&reservation, CapturePayload::Coverage(snapshot.clone()))
            .await
            .unwrap_err();
        assert!(
            error
                .message
                .contains("injected capture storage sync failure"),
            "{error:?}"
        );
        assert!(!staging_path.exists());
        assert!(final_path.exists());
        assert!(service.state.lock().await.captures.is_empty());

        service
            .store_capture(&reservation, CapturePayload::Coverage(snapshot.clone()))
            .await
            .unwrap();
        drop(service);

        let (shutdown, _) = watch::channel(false);
        let restored = DebuggerService::load(shutdown, persistence_path).unwrap();
        assert!(matches!(
            load_stored_capture(
                &restored.state.lock().await.captures
                    [&("test".to_owned(), "coverage".to_owned())]
            ),
            CapturePayload::Coverage(value) if value == snapshot
        ));
        drop(restored);
        let _ = fs::remove_dir_all(root);
    }

    #[tokio::test]
    async fn heap_storage_is_unique_and_cleanup_cannot_remove_another_reservation() {
        let mut state = ServiceState::default();
        insert_context_with_targets(
            &mut state,
            "test",
            [(
                "runtime",
                1,
                vec![target("target-a", "A", "https://a.test")],
            )],
        );
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
        remove_capture_payload_files([second_staging, second_final]);
        assert_eq!(fs::read(&first_final).unwrap(), b"winner");

        remove_capture_payload_files([first_final]);
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
        let mut state = ServiceState::default();
        insert_context_with_targets(
            &mut state,
            "test",
            [(
                "runtime",
                1,
                vec![target("target-a", "A", "https://a.test")],
            )],
        );
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
    async fn failed_catalog_persistence_keeps_capture_retryable() {
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

        let error = service
            .delete_capture(&CallCtx::default(), "test".into(), "kept".into())
            .await
            .unwrap_err();
        assert!(
            error
                .message
                .contains("failed to persist debugger context state"),
            "{error:?}"
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
        fs::remove_file(&blocker).unwrap();
        fs::create_dir(&blocker).unwrap();
        service
            .delete_capture(&CallCtx::default(), "test".into(), "kept".into())
            .await
            .unwrap();
        assert!(
            !service
                .state
                .lock()
                .await
                .captures
                .contains_key(&("test".into(), "kept".into()))
        );
        drop(service);

        let (shutdown, _) = watch::channel(false);
        let restored = DebuggerService::load(shutdown, blocker.join("service.json")).unwrap();
        assert!(restored.state.lock().await.captures.is_empty());
        drop(restored);
        let _ = fs::remove_dir_all(root);
    }

    #[tokio::test]
    async fn failed_heap_file_deletion_keeps_capture_retryable() {
        let root = std::env::current_dir()
            .unwrap()
            .join("target")
            .join(format!(
                "capture-file-delete-failure-{}",
                random_instance_id().unwrap()
            ));
        fs::create_dir_all(&root).unwrap();
        let persistence_path = root.join("service.json");
        let heap_path = persistence_path
            .with_extension("captures")
            .join("orphan")
            .join("blocked.heapsnapshot");
        fs::create_dir_all(heap_path.parent().unwrap()).unwrap();
        fs::write(&heap_path, b"blocked").unwrap();
        let mut state = ServiceState::default();
        state.captures.insert(
            ("test".into(), "blocked".into()),
            heap_capture("test", "blocked", "target-a", "runtime", &heap_path),
        );
        let service = service_with_state(persistence_path.clone(), state);
        service.persist(&*service.state.lock().await).unwrap();
        fs::remove_file(&heap_path).unwrap();
        fs::create_dir(&heap_path).unwrap();

        let error = service
            .delete_capture(&CallCtx::default(), "test".into(), "blocked".into())
            .await
            .unwrap_err();
        assert!(
            error
                .message
                .contains("capture 'blocked' was removed from the catalog"),
            "{error:?}"
        );
        assert!(heap_path.is_dir());
        assert!(
            !service
                .state
                .lock()
                .await
                .captures
                .contains_key(&("test".into(), "blocked".into()))
        );
        drop(service);

        let (shutdown, _) = watch::channel(false);
        let restored = DebuggerService::load(shutdown, persistence_path.clone()).unwrap();
        assert!(restored.state.lock().await.captures.is_empty());
        assert!(!heap_path.exists());
        drop(restored);

        let (shutdown, _) = watch::channel(false);
        let reloaded = DebuggerService::load(shutdown, persistence_path).unwrap();
        assert!(reloaded.state.lock().await.captures.is_empty());
        drop(reloaded);
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
        let capture_path = root.join("kept.heapsnapshot");
        fs::write(&capture_path, b"kept").unwrap();
        let mut state = ServiceState::default();
        insert_context_with_targets(
            &mut state,
            "test",
            [(
                "runtime",
                1,
                vec![target("target-a", "A", "https://a.test")],
            )],
        );
        state
            .context_kinds
            .insert("test".into(), ContextKind::Named);
        state.captures.insert(
            ("test".into(), "kept".into()),
            heap_capture("test", "kept", "target-a", "runtime", &capture_path),
        );
        let (cancel, cancelled) = watch::channel(false);
        let (_, closed) = watch::channel(false);
        state.playwright_proxies.insert(
            "proxy".into(),
            PlaywrightProxyRegistration {
                context_id: "test".into(),
                connection_id: "runtime".into(),
                target_id: "target-a".into(),
                generation: 1,
                cancel,
                closed,
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
        assert!(capture_path.exists());
        let state = service.state.lock().await;
        assert!(state.contexts.contains_key("test"));
        assert!(state.playwright_proxies.contains_key("proxy"));
        assert!(state.captures.contains_key(&("test".into(), "kept".into())));
        drop(state);
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn startup_scavenges_only_unreferenced_capture_payload_files() {
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
        let orphan_heap = capture_root.join("orphan").join("lost.heapsnapshot");
        let orphan_coverage = capture_root.join("orphan").join("lost.coverage.json");
        let orphan_cpu = capture_root.join("orphan").join("lost.cpuprofile.json");
        let orphan_partial = capture_root.join("orphan").join("interrupted.partial");
        let unrelated = capture_root.join("orphan").join("notes.txt");
        for path in [
            &cataloged,
            &orphan_heap,
            &orphan_coverage,
            &orphan_cpu,
            &orphan_partial,
            &unrelated,
        ] {
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
        assert!(!orphan_heap.exists());
        assert!(!orphan_coverage.exists());
        assert!(!orphan_cpu.exists());
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
    fn stored_heap_maps_survive_service_restart_without_a_live_target() {
        use crate::service_api::{HeapMappingSnapshot, HeapMappingStatus, HeapScriptSnapshot, ScriptProvenance};
        let root = std::env::current_dir().unwrap().join("target")
            .join(format!("offline-heap-{}", random_instance_id().unwrap()));
        fs::create_dir_all(&root).unwrap();
        let persistence_path = root.join("service.json");
        let heap_path = root.join("capture.heapsnapshot");
        fs::write(&heap_path, r#"{
            "snapshot":{"meta":{
                "node_fields":["type","name","id","self_size","edge_count"],
                "node_types":[["hidden","object"],"string","number","number","number"],
                "location_fields":["object_index","script_id","line","column"]
            }},
            "nodes":[1,0,7,16,0],"locations":[0,7,0,0],"strings":["a"]
        }"#).unwrap();
        let mut capture = heap_capture("test", "heap", "target-a", "runtime", &heap_path);
        capture.heap_mapping = Some(HeapMappingSnapshot {
            connection_generation: 42, hydration_duration_micros: 10,
            scripts: vec![HeapScriptSnapshot {
                script_id: "7".into(), url: "https://example.test/app.js".into(), hash: "captured-hash".into(),
                provenance: ScriptProvenance { execution_context_id: Some(7),
                    execution_context_aux_data: None, frame_id: Some("child-frame".into()) },
                source_map_url: Some("https://example.test/app.js.map".into()),
                generated_source: Some("class a {}".into()),
                source_map: Some(r#"{"version":3,"sources":["original.ts"],"sourcesContent":["class Original {}"],"names":[],"mappings":"AAAA"}"#.into()),
                mapping_status: HeapMappingStatus::Mapped, diagnostic: None,
            }],
        });
        let mut state = ServiceState::default();
        state.captures.insert(("test".into(), "heap".into()), capture);
        let writer = service_with_state(persistence_path.clone(), state);
        writer.persist(&writer.state.blocking_lock()).unwrap();
        drop(writer);
        let (shutdown, _) = watch::channel(false);
        let restored = DebuggerService::load(shutdown, persistence_path.clone()).unwrap();
        let runtime = tokio::runtime::Runtime::new().unwrap();
        runtime.block_on(async {
            let snapshot = restored.get_stored_heap_classes(&CallCtx::default(),
                "test".into(), "heap".into(), None, None, None).await.unwrap();
            assert_eq!(snapshot.classes[0].name, "Original");
            assert_eq!(snapshot.analysis.mapping_status, HeapMappingStatus::Mapped);
            assert_eq!(snapshot.classes[0].provenance.frame_id.as_deref(), Some("child-frame"));
            assert!(restored.state.lock().await.target_debuggers.is_empty());
            let mut supply = crate::service_api::HeapSourceMapSupply {
                script_id: "7".into(), script_hash: "wrong".into(),
                source_map_url: "file:///maps/app.js.map".into(),
                source_map: r#"{"version":3,"file":"app.js","sources":["supplied.ts"],"sourcesContent":["class Supplied {}"],"names":[],"mappings":"AAAA"}"#.into(),
            };
            assert!(restored.supply_stored_heap_source_map(&CallCtx::default(),
                "test".into(), "heap".into(), supply.clone()).await.is_err());
            supply.script_hash = "captured-hash".into();
            restored.supply_stored_heap_source_map(&CallCtx::default(),
                "test".into(), "heap".into(), supply).await.unwrap();
        });
        drop(restored);
        let (shutdown, _) = watch::channel(false);
        let reloaded = DebuggerService::load(shutdown, persistence_path).unwrap();
        runtime.block_on(async {
            let snapshot = reloaded.get_stored_heap_classes(&CallCtx::default(),
                "test".into(), "heap".into(), None, None, None).await.unwrap();
            assert_eq!(snapshot.classes[0].name, "Supplied");
            assert_eq!(snapshot.analysis.script_mappings[0].hash, "captured-hash");
        });
        drop(reloaded);
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn schema_five_validates_payload_integrity_and_kind_on_restart() {
        let root = std::env::current_dir()
            .unwrap()
            .join("target")
            .join(format!(
                "capture-integrity-{}",
                random_instance_id().unwrap()
            ));
        let persistence_path = root.join("service.json");
        let metadata = capture_metadata(
            "test",
            "coverage",
            CaptureKind::Coverage,
            "target-a",
            "runtime",
        );
        let capture = stored_capture_from_payload(
            &persistence_path,
            metadata,
            CapturePayload::Coverage(CoverageSnapshot {
                capture_id: None,
                timestamp_micros: 42,
                sources: Vec::new(),
                analysis: None,
            }),
        );
        let payload_path = capture.payload_path();
        let original_payload = fs::read(&payload_path).unwrap();
        let mut state = ServiceState::default();
        state
            .captures
            .insert(("test".into(), "coverage".into()), capture);
        let writer = service_with_state(persistence_path.clone(), state);
        writer.persist(&writer.state.blocking_lock()).unwrap();
        drop(writer);
        let original_state = fs::read(&persistence_path).unwrap();

        let (shutdown, _) = watch::channel(false);
        let valid = DebuggerService::load(shutdown, persistence_path.clone()).unwrap();
        assert_eq!(valid.state.blocking_lock().captures.len(), 1);
        drop(valid);

        let mut hash_corrupt_payload = original_payload.clone();
        hash_corrupt_payload[0] ^= 1;
        fs::write(&payload_path, hash_corrupt_payload).unwrap();
        let (shutdown, _) = watch::channel(false);
        let error = match DebuggerService::load(shutdown, persistence_path.clone()) {
            Ok(_) => panic!("hash-corrupt capture payload unexpectedly loaded"),
            Err(error) => error,
        };
        assert!(
            error.to_string().contains("integrity validation"),
            "{error}"
        );

        fs::write(&payload_path, &original_payload).unwrap();
        let mut wrong_size_state: serde_json::Value =
            serde_json::from_slice(&original_state).unwrap();
        let byte_len = wrong_size_state["captures"][0]["payload"]["byteLen"]
            .as_u64()
            .unwrap();
        wrong_size_state["captures"][0]["payload"]["byteLen"] =
            serde_json::Value::from(byte_len + 1);
        fs::write(
            &persistence_path,
            serde_json::to_vec_pretty(&wrong_size_state).unwrap(),
        )
        .unwrap();
        let (shutdown, _) = watch::channel(false);
        let error = match DebuggerService::load(shutdown, persistence_path.clone()) {
            Ok(_) => panic!("wrong-size capture payload unexpectedly loaded"),
            Err(error) => error,
        };
        assert!(
            error.to_string().contains("integrity validation"),
            "{error}"
        );

        fs::write(&persistence_path, &original_state).unwrap();
        let wrong_kind_path = payload_path.with_file_name("wrong.cpuprofile.json");
        fs::copy(&payload_path, &wrong_kind_path).unwrap();
        let mut persisted: serde_json::Value =
            serde_json::from_slice(&fs::read(&persistence_path).unwrap()).unwrap();
        persisted["captures"][0]["payload"]["path"] =
            serde_json::Value::String(wrong_kind_path.to_string_lossy().into_owned());
        fs::write(
            &persistence_path,
            serde_json::to_vec_pretty(&persisted).unwrap(),
        )
        .unwrap();
        let (shutdown, _) = watch::channel(false);
        let error = match DebuggerService::load(shutdown, persistence_path.clone()) {
            Ok(_) => panic!("mismatched capture payload unexpectedly loaded"),
            Err(error) => error,
        };
        assert!(
            error.to_string().contains("expected Coverage storage"),
            "{error}"
        );
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn target_selector_resolution_preserves_nested_connection_prefixes() {
        let mut state = ServiceState::default();
        insert_context_with_targets(
            &mut state,
            "test",
            [("browser", 7, vec![
                target("browser/frame", "Nested", "https://nested.test"),
                target("frame", "Other", "https://other.test"),
            ])],
        );
        for selector in ["browser/browser/frame", "browser/browser/frame@7"] {
            let resolved = DebuggerService::resolve_canonical_target(&state, "test", selector).unwrap();
            assert_eq!(resolved.target_id, "browser/frame");
            let request = crate::target_selector::resolved_target_selector(
                &resolved.connection_id, &resolved.target_id, resolved.connection_generation, Some(selector),
            );
            assert_eq!(
                DebuggerService::resolve_target_id_in_state(&state, "test", "browser", &request).unwrap(),
                "browser/frame",
            );
        }
    }

    #[test]
    fn target_selector_stale_identity_never_falls_back_to_friendly_match() {
        let mut state = ServiceState::default();
        insert_context_with_targets(
            &mut state,
            "test",
            [
                ("browser", 2, vec![target("frame/child", "Current", "https://current.test")]),
                ("decoy", 1, vec![target("other", "browser/frame/child@1", "https://other.test")]),
            ],
        );
        let error = DebuggerService::resolve_canonical_target(&state, "test", "browser/frame/child@1")
            .unwrap_err();
        assert!(error.message.contains("stale connection generation"), "{error:?}");
        let missing = DebuggerService::resolve_canonical_target(&state, "test", "browser/missing@2")
            .unwrap_err();
        assert!(missing.message.contains("discovery may be incomplete"), "{missing:?}");
        insert_context_with_targets(
            &mut state,
            "test",
            [
                ("browser", 2, vec![]),
                ("decoy", 1, vec![target("other", "browser/frame/child@1", "https://other.test")]),
            ],
        );
        let error = DebuggerService::resolve_canonical_target(&state, "test", "browser/frame/child@1")
            .unwrap_err();
        assert!(error.message.contains("stale connection generation"), "{error:?}");
    }

    #[test]
    fn target_selector_literal_id_wins_over_qualification_shaped_friendly_text() {
        let mut state = ServiceState::default();
        let literal = "browser/renderer/target/frame@1";
        insert_context_with_targets(
            &mut state,
            "test",
            [
                ("browser", 2, vec![target(literal, "Exact", "https://exact.test")]),
                ("decoy", 1, vec![target("other", literal, "https://other.test")]),
            ],
        );
        let resolved = DebuggerService::resolve_canonical_target(&state, "test", literal).unwrap();
        assert_eq!(resolved.target_id, literal);
        assert_eq!(resolved.connection_id, "browser");
        assert_eq!(
            DebuggerService::resolve_target_id_in_state(&state, "test", "browser", literal).unwrap(),
            literal,
        );
    }

    #[test]
    fn target_selector_missing_discovery_is_not_a_confident_empty_result() {
        let mut state = ServiceState::default();
        insert_context_with_targets(&mut state, "test", [("browser", 1, vec![])]);
        for error in [
            DebuggerService::resolve_canonical_target(&state, "test", "browser/frame@1").unwrap_err(),
            DebuggerService::resolve_target_id_in_state(&state, "test", "browser", "browser/frame@1").unwrap_err(),
        ] {
            assert!(error.message.contains("discovery may be incomplete"), "{error:?}");
        }
    }

    #[test]
    fn qualified_target_selectors_round_trip_nested_ids_and_generations() {
        let mut state = ServiceState::default();
        let target_id = "renderer-11/target/iframe";
        insert_context_with_targets(
            &mut state,
            "test",
            [
                (
                    "process-tree-1",
                    2,
                    vec![target(target_id, "Editor", "https://a.test")],
                ),
                (
                    "process-tree-2",
                    7,
                    vec![target(target_id, "Editor", "https://b.test")],
                ),
            ],
        );
        assert!(DebuggerService::resolve_canonical_target(&state, "test", target_id).is_err());
        for selector in [
            format!("process-tree-2/{target_id}"),
            crate::target_selector::qualified_target_selector("process-tree-2", target_id, 7),
        ] {
            let resolved =
                DebuggerService::resolve_canonical_target(&state, "test", &selector).unwrap();
            assert_eq!(resolved.connection_id, "process-tree-2");
            assert_eq!(resolved.target_id, target_id);
            assert_eq!(resolved.connection_generation, 7);
        }
        assert!(
            DebuggerService::resolve_canonical_target(
                &state,
                "test",
                &format!("process-tree-2/{target_id}@6"),
            )
            .is_err()
        );
    }

    #[tokio::test]
    async fn logs_for_unattached_target_are_inactive_without_attaching() {
        let mut state = ServiceState::default();
        insert_context_with_targets(
            &mut state,
            "test",
            [(
                "browser",
                2,
                vec![target("renderer/target/frame", "Frame", "https://test")],
            )],
        );
        let service = service_with_state(PathBuf::from("unused"), state);
        let logs = service
            .get_logs(
                &CallCtx::default(),
                "test".into(),
                "browser".into(),
                "browser/renderer/target/frame@2".into(),
            )
            .await
            .unwrap();
        assert_eq!(serde_json::to_value(&logs).unwrap(), serde_json::json!({
            "contextId": "test", "connectionId": "browser",
            "targetId": "renderer/target/frame", "connectionGeneration": 2,
            "messages": [],
            "capture": {
                "status": "inactive", "captureId": null, "sessionId": null,
                "startedAtUnixMs": null, "collectedEvents": [],
                "evictedCount": null, "droppedCount": null
            }
        }));
        assert!(service.state.lock().await.target_debuggers.is_empty());
        for selector in ["browser/renderer/target/frame@1", "missing"] {
            assert!(
                service
                    .get_logs(
                        &CallCtx::default(),
                        "test".into(),
                        "browser".into(),
                        selector.into(),
                    )
                    .await
                    .is_err()
            );
        }
    }

    #[tokio::test]
    async fn qualified_target_requests_reject_reconnect_after_resolution() {
        let mut state = ServiceState::default();
        insert_context_with_targets(
            &mut state,
            "test",
            [(
                "browser",
                1,
                vec![target("renderer/target/frame", "Editor", "https://test")],
            )],
        );
        let selector = "browser/renderer/target/frame@1";
        let resolved = DebuggerService::resolve_canonical_target(&state, "test", selector).unwrap();
        let request_target = crate::target_selector::resolved_target_selector(
            &resolved.connection_id,
            &resolved.target_id,
            resolved.connection_generation,
            Some(selector),
        );
        insert_context_with_targets(
            &mut state,
            "test",
            [(
                "browser",
                2,
                vec![target("renderer/target/frame", "Editor", "https://test")],
            )],
        );
        let service = service_with_state(PathBuf::from("unused"), state);
        let ctx = CallCtx::default();
        let errors = [
            service.get_target(
                &ctx,
                "test".to_owned(),
                "browser".to_owned(),
                request_target.clone(),
            ).await.unwrap_err(),
            service.inspect_value(
                &ctx,
                "test".to_owned(),
                "browser".to_owned(),
                request_target.clone(),
                None,
                ValueSelector::Expression {
                    expression: "1".to_owned(),
                    allow_side_effects: true,
                },
                ValueInspectionOptions {
                    max_preview_length: 120,
                    max_properties: 20,
                    retain_references: false,
                },
            ).await.unwrap_err(),
            service.attach_target(
                &ctx,
                "test".to_owned(),
                "browser".to_owned(),
                request_target,
                TargetAttachOptions::default(),
            ).await.unwrap_err(),
        ];
        for error in errors {
            assert!(error.message.contains("target selector"), "{error:?}");
            assert!(error.message.contains(selector), "{error:?}");
        }
    }

    #[test]
    fn canonical_target_id_wins_over_friendly_matches_context_wide() {
        let mut state = ServiceState::default();
        insert_context_with_targets(
            &mut state,
            "test",
            [
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
            ],
        );
        let resolved =
            DebuggerService::resolve_canonical_target(&state, "test", "canonical").unwrap();
        assert_eq!(resolved.connection_id, "first");
        assert_eq!(resolved.target_id, "canonical");
        assert_eq!(resolved.connection_generation, 2);
    }

    #[test]
    fn debug_session_resources_authoritatively_drive_attachment_projection() {
        let mut state = ServiceState::default();
        insert_context_with_targets(
            &mut state,
            "test",
            [(
                "browser",
                3,
                vec![target("page", "Page", "https://example.test")],
            )],
        );
        let source = publish_debug_session_resource(
            &state,
            "debugger",
            "test",
            "browser",
            3,
            "page",
            "session-1",
        )
        .unwrap();
        let graph = state.resource_graphs["test"].snapshot();
        let projected = snapshot("agent", "test", &state.contexts["test"], Some(&graph));
        assert_eq!(projected.resource_revision, graph.revision.0);
        assert_eq!(
            projected.target_forest[0].attachment,
            TargetAttachmentState::Debugger
        );
        let session = debug_session_resource_id("debugger", "test", "browser", 3, "session-1");
        assert!(graph.resources.contains_key(&session));
        assert!(graph.relations.iter().any(|relation| {
            relation.relation.kind == RelationKind::Debugs && relation.relation.from == session
        }));

        retract_debug_session_resource(&state, "test", &source);
        let graph = state.resource_graphs["test"].snapshot();
        let projected = snapshot("agent", "test", &state.contexts["test"], Some(&graph));
        assert_eq!(
            projected.target_forest[0].attachment,
            TargetAttachmentState::Detached
        );
        assert!(!graph.resources.contains_key(&session));
    }

    #[test]
    fn retracting_a_connection_atomically_removes_its_debug_sessions() {
        let mut state = ServiceState::default();
        insert_context_with_targets(
            &mut state,
            "test",
            [(
                "browser",
                3,
                vec![target("page", "Page", "https://example.test")],
            )],
        );
        publish_debug_session_resource(
            &state, "debugger", "test", "browser", 3, "page", "ordinary",
        )
        .unwrap();
        publish_debug_session_resource(
            &state,
            "relay:owner",
            "test",
            "browser",
            3,
            "page",
            "relay",
        )
        .unwrap();

        retract_connection_resource_graph(&mut state, "test", "browser", 3);

        assert!(
            state.resource_graphs["test"]
                .snapshot()
                .resources
                .is_empty()
        );
    }

    #[test]
    fn friendly_target_ambiguity_reports_qualified_candidates() {
        let mut state = ServiceState::default();
        insert_context_with_targets(
            &mut state,
            "test",
            [
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
            ],
        );
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
        let mut state = ServiceState::default();
        insert_context_with_targets(
            &mut state,
            "test",
            [
                ("runtime-a", 1, vec![first]),
                ("runtime-b", 1, vec![second]),
            ],
        );

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
                "dbgjs-persistence-v1-{}-{}.json",
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
    fn schema_four_embedded_capture_catalog_is_migrated() {
        let path = std::env::current_dir()
            .unwrap()
            .join("target")
            .join(format!(
                "dbgjs-persistence-v4-{}-{}.json",
                std::process::id(),
                random_instance_id().unwrap()
            ));
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        let heap_path = path.with_file_name(format!(
            "legacy-{}.heapsnapshot",
            random_instance_id().unwrap()
        ));
        fs::write(&heap_path, b"legacy heap").unwrap();
        let coverage = CoverageSnapshot {
            capture_id: None,
            timestamp_micros: 42,
            sources: Vec::new(),
            analysis: None,
        };
        let cpu_profile = CpuProfileSnapshot {
            capture_id: "profile".into(),
            sampling_interval_micros: Some(100),
            start_time_micros: 1.0,
            end_time_micros: 2.0,
            nodes: Vec::new(),
            samples: Vec::new(),
            time_deltas_micros: Vec::new(),
            functions: Vec::new(),
            analysis: None,
        };
        let heap_metadata = CaptureSnapshot {
            context_id: "context".into(),
            name: "baseline".into(),
            kind: CaptureKind::HeapSnapshot,
            target_id: "canonical-target".into(),
            connection_id: "browser".into(),
            connection_generation: 11,
            storage_id: "immutable-storage".into(),
        };
        let coverage_metadata = capture_metadata(
            "context",
            "coverage",
            CaptureKind::Coverage,
            "canonical-target",
            "browser",
        );
        write_capture_payload(
            &path,
            &coverage_metadata,
            &CapturePayload::Coverage(coverage.clone()),
        )
        .unwrap();
        let legacy = LegacyStoredServiceState {
            schema_version: 4,
            contexts: BTreeMap::new(),
            completed_requests: Vec::new(),
            captures: vec![
                LegacyStoredCapture {
                    metadata: heap_metadata,
                    payload: LegacyStoredCapturePayload::HeapSnapshot {
                        path: heap_path.to_string_lossy().into_owned(),
                    },
                },
                LegacyStoredCapture {
                    metadata: coverage_metadata,
                    payload: LegacyStoredCapturePayload::Coverage(coverage.clone()),
                },
                LegacyStoredCapture {
                    metadata: capture_metadata(
                        "context",
                        "profile",
                        CaptureKind::CpuProfile,
                        "canonical-target",
                        "browser",
                    ),
                    payload: LegacyStoredCapturePayload::CpuProfile(cpu_profile.clone()),
                },
            ],
        };
        fs::write(&path, serde_json::to_vec_pretty(&legacy).unwrap()).unwrap();
        let restored = load_state(&path).unwrap();
        let capture = &restored.captures[&("context".into(), "baseline".into())];
        assert_eq!(capture.metadata.target_id, "canonical-target");
        assert_eq!(capture.metadata.connection_generation, 11);
        assert_eq!(capture.metadata.storage_id, "immutable-storage");
        assert!(matches!(
            load_stored_capture(capture),
            CapturePayload::HeapSnapshot { path } if path == heap_path.to_string_lossy()
        ));
        assert!(matches!(
            load_stored_capture(&restored.captures[&("context".into(), "coverage".into())]),
            CapturePayload::Coverage(value) if value == coverage
        ));
        assert!(matches!(
            load_stored_capture(&restored.captures[&("context".into(), "profile".into())]),
            CapturePayload::CpuProfile(value) if value == cpu_profile
        ));
        let persisted: serde_json::Value =
            serde_json::from_slice(&fs::read(&path).unwrap()).unwrap();
        assert_eq!(persisted["schemaVersion"], 6);
        assert!(
            persisted["captures"]
                .as_array()
                .unwrap()
                .iter()
                .all(|capture| capture["payload"]["sha256"].is_string())
        );
        assert!(
            !String::from_utf8(fs::read(&path).unwrap())
                .unwrap()
                .contains("\"timestampMicros\"")
        );
        let payload_paths = restored
            .captures
            .values()
            .map(StoredCapture::payload_path)
            .collect::<Vec<_>>();
        drop(restored);
        let _ = fs::remove_file(path);
        for path in payload_paths {
            let _ = fs::remove_file(path);
        }
    }

    #[test]
    fn formatting_policy_uses_last_matching_target_and_url_rule() {
        let settings = SourceFormattingSettings {
            default_mode: SourceFormattingMode::Auto,
            rules: vec![
                SourceFormattingRule {
                    id: "fmt-1".into(),
                    mode: SourceFormattingMode::Off,
                    target_pattern: None,
                    url_pattern: Some("**/vendor/**".into()),
                },
                SourceFormattingRule {
                    id: "fmt-2".into(),
                    mode: SourceFormattingMode::On,
                    target_pattern: Some("page-*".into()),
                    url_pattern: Some("**/vendor/special.min.js".into()),
                },
            ],
        };
        let settings = compile_formatting_settings(&settings).unwrap();
        assert_eq!(
            effective_formatting_mode(
                &settings,
                "page-1",
                "https://example.test/vendor/special.min.js"
            ),
            SourceFormattingMode::On
        );
        assert_eq!(
            effective_formatting_mode(
                &settings,
                "worker-1",
                "https://example.test/vendor/special.min.js"
            ),
            SourceFormattingMode::Off
        );
        assert_eq!(
            effective_formatting_mode(&settings, "page-1", "https://example.test/app.js"),
            SourceFormattingMode::Auto
        );
    }

    #[test]
    fn source_view_selection_keeps_exactly_one_unmapped_representation() {
        let content = |path: &str, kind: &str, text: &str| HydratedSource {
            path: path.into(),
            kind: kind.into(),
            provenance: kind.into(),
            content_hash: crate::content_store::ContentHash::of_bytes(text.as_bytes()),
            content: Arc::from(text),
        };
        let settings = compile_formatting_settings(&SourceFormattingSettings {
            default_mode: SourceFormattingMode::On,
            rules: Vec::new(),
        })
        .unwrap();
        let mut sources = vec![
            content("app.js", "runtime", "const x=1;"),
            content("app.js?formatted", "authored", "const x = 1;\n"),
        ];
        select_source_views(
            &mut sources,
            &settings,
            "page-1",
            SourceViewPreference::Policy,
        );
        assert_eq!(sources.len(), 1);
        assert_eq!(sources[0].path, "app.js?formatted");

        let mut sources = vec![
            content("app.js", "runtime", "const x=1;"),
            content("app.js?formatted", "authored", "const x = 1;\n"),
        ];
        select_source_views(
            &mut sources,
            &settings,
            "page-1",
            SourceViewPreference::Original,
        );
        assert_eq!(sources.len(), 1);
        assert_eq!(sources[0].path, "app.js");
    }

    #[test]
    fn auto_formatting_detection_is_bounded_and_explainable() {
        assert!(appears_minified("app.min.js", "x"));
        assert!(appears_minified("app.js", &"x".repeat(1_024)));
        assert!(!appears_minified(
            "app.js",
            "function readable() {\n  return 1;\n}\n"
        ));
    }

    #[test]
    fn ensure_context_not_relayed_rejects_only_the_relayed_context() {
        let (cancel, _) = watch::channel(false);
        let mut state = ServiceState::default();
        state.relays.insert(
            "relay-1".into(),
            RelayRegistration {
                context_id: "ctx-a".into(),
                cancel,
            },
        );

        let error = ensure_context_not_relayed(&state, "ctx-a").unwrap_err();
        assert!(
            error
                .message
                .contains("exclusively owned by an active relay"),
            "{}",
            error.message
        );
        assert!(ensure_context_not_relayed(&state, "ctx-b").is_ok());
    }

    #[tokio::test]
    async fn target_debugger_rejects_while_relayed_but_the_bypass_proceeds() {
        let (cancel, _) = watch::channel(false);
        let mut state = ServiceState::default();
        insert_context_with_targets(
            &mut state,
            "ctx",
            [(
                "conn",
                1,
                vec![target("page-1", "Page", "https://example.com")],
            )],
        );
        state.relays.insert(
            "relay-1".into(),
            RelayRegistration {
                context_id: "ctx".into(),
                cancel,
            },
        );
        let service = service_with_state(PathBuf::from("unused"), state);

        let guarded = match service.target_debugger("ctx", "conn", "page-1").await {
            Ok(_) => panic!("target_debugger must reject while the context is relayed"),
            Err(error) => error,
        };
        assert!(
            guarded
                .message
                .contains("exclusively owned by an active relay"),
            "{}",
            guarded.message
        );

        // The bypass reaches the ordinary "no attached target" failure instead of the relay
        // guard, proving relay-internal forwarding is unaffected by context exclusivity.
        let bypassed = match service
            .target_debugger_bypassing_relay("ctx", "conn", "page-1")
            .await
        {
            Ok(_) => panic!("no target is attached in this fixture"),
            Err(error) => error,
        };
        assert!(!bypassed.message.contains("relay"), "{}", bypassed.message);
        assert!(
            bypassed.message.contains("attached target"),
            "{}",
            bypassed.message
        );
    }

    #[tokio::test]
    async fn attach_target_rejects_while_relayed_but_internal_proceeds() {
        let (cancel, _) = watch::channel(false);
        let mut state = ServiceState::default();
        insert_context_with_targets(
            &mut state,
            "ctx",
            [(
                "conn",
                1,
                vec![target("page-1", "Page", "https://example.com")],
            )],
        );
        state.relays.insert(
            "relay-1".into(),
            RelayRegistration {
                context_id: "ctx".into(),
                cancel,
            },
        );
        let service = service_with_state(PathBuf::from("unused"), state);

        let guarded = service
            .attach_target(
                &CallCtx::default(),
                "ctx".into(),
                "conn".into(),
                "page-1".into(),
                TargetAttachOptions::default(),
            )
            .await
            .unwrap_err();
        assert!(
            guarded
                .message
                .contains("exclusively owned by an active relay"),
            "{}",
            guarded.message
        );

        // `attach_target_internal` reaches the ordinary "connection is not connected" failure
        // (this fixture registers no `ConnectionRuntime`) instead of the relay guard.
        let bypassed = service
            .attach_target_internal(
                &CallCtx::default(),
                "ctx".into(),
                "conn".into(),
                "page-1".into(),
                TargetAttachOptions::default(),
            )
            .await
            .unwrap_err();
        assert!(!bypassed.message.contains("relay"), "{}", bypassed.message);
    }

    #[tokio::test]
    async fn deleting_a_relayed_context_cancels_its_relay() {
        let root = std::env::current_dir()
            .unwrap()
            .join("target")
            .join(format!("relay-delete-{}", random_instance_id().unwrap()));
        let persistence_path = root.join("service.json");
        let (cancel, mut cancelled) = watch::channel(false);
        let mut state = ServiceState::default();
        insert_context_with_targets(&mut state, "ctx", []);
        state.relays.insert(
            "relay-1".into(),
            RelayRegistration {
                context_id: "ctx".into(),
                cancel,
            },
        );
        let service = service_with_state(persistence_path, state);

        service
            .delete_context(
                &CallCtx::default(),
                "ctx".into(),
                MutationOptions::default(),
            )
            .await
            .unwrap();

        assert!(
            *cancelled.borrow_and_update(),
            "deleting the context must cancel its relay"
        );
        assert!(service.state.lock().await.relays.is_empty());
        let _ = fs::remove_dir_all(root);
    }
}
