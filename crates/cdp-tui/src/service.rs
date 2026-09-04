use std::collections::BTreeMap;
use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

use cdp_client::context_identity::{
    normalize_absolute_path, path_and_parents, resolve_context_expression,
};
use cdp_client::local_rpc::{default_state_file, ensure_service};
use cdp_client::service_api::{
    ConnectionStatus, ContextSnapshot, ContextSummary, DebuggerServiceApiClient, MutationOptions,
    ObservationCursor, ObservationResult, ProcessTreeSnapshot, ResourceGraphSnapshot,
    SourceContentSnapshot, SourceDisplayOptions, SourceTreeKind, SourceTreeSnapshot,
    SourceViewPreference, TargetAttachOptions, TargetDebuggerSnapshot,
};
use tokio::sync::mpsc;
use tokio::task::JoinHandle;

use crate::app::{ConnectionPathRef, Section, TargetRef, UiAction, connection_path_spec};

pub struct Bootstrap {
    pub client: Arc<DebuggerServiceApiClient>,
    pub cwd: String,
    pub contexts: Vec<ContextSummary>,
    pub context_index: usize,
    pub context: ContextSnapshot,
}

impl Bootstrap {
    pub async fn connect(context_expression: Option<&str>) -> Result<Self, String> {
        let state_file = default_state_file();
        let client = Arc::new(
            ensure_service(&state_file)
                .await
                .map_err(|error| error.to_string())?,
        );
        let cwd = std::env::current_dir().map_err(|error| error.to_string())?;
        let normalized_cwd = normalize_absolute_path(&cwd).map_err(|error| error.to_string())?;
        let contexts = client
            .list_contexts(Some(normalized_cwd.clone()))
            .await
            .map_err(rpc_error)?;
        if contexts.is_empty() {
            return Err(
                "no debugger contexts exist; create one with `jsdbg context create`".to_owned(),
            );
        }
        let configured_context = if context_expression.is_none() {
            configured_context_id(
                &state_file.with_extension("selection.json"),
                &normalized_cwd,
            )?
        } else {
            None
        };
        let context_index = if let Some(expression) = context_expression {
            let id = resolve_context_expression(expression, Path::new(&cwd))
                .map_err(|error| error.to_string())?
                .id;
            contexts
                .iter()
                .position(|context| context.id == id)
                .ok_or_else(|| format!("context '{id}' does not exist"))?
        } else if let Some(context_id) = configured_context {
            contexts
                .iter()
                .position(|context| context.id == context_id)
                .ok_or_else(|| {
                    format!(
                        "configured context '{context_id}' does not exist; select another context with `jsdbg set context --context <expression>`"
                    )
                })?
        } else {
            contexts
                .iter()
                .position(|context| {
                    context.path_ancestor == Some(true)
                        && context.path_distance
                            == contexts
                                .iter()
                                .filter(|candidate| candidate.path_ancestor == Some(true))
                                .filter_map(|candidate| candidate.path_distance)
                                .min()
                })
                .unwrap_or(0)
        };
        let context = client
            .get_context(contexts[context_index].id.clone())
            .await
            .map_err(rpc_error)?;
        Ok(Self {
            client,
            cwd: normalized_cwd,
            contexts,
            context_index,
            context,
        })
    }
}

pub enum Data {
    Contexts(Vec<ContextSummary>),
    Processes(Vec<ProcessTreeSnapshot>),
    Resources(ResourceGraphSnapshot),
    Sources(SourceTreeSnapshot),
    Captures(Vec<cdp_client::service_api::CaptureSnapshot>),
}

pub enum ServiceEvent {
    Context(ContextSnapshot),
    ContextError(String),
    TargetDetail {
        generation: u64,
        target: TargetRef,
        detail: Option<TargetDebuggerSnapshot>,
    },
    DataLoaded {
        section: Section,
        context_id: String,
        generation: u64,
        result: Result<Data, String>,
    },
    SourceLoaded {
        context_id: String,
        path: String,
        generation: u64,
        result: Result<SourceContentSnapshot, String>,
    },
    ActionFinished(Result<(String, ContextSnapshot), String>),
}

pub struct ServiceController {
    client: Arc<DebuggerServiceApiClient>,
    events: mpsc::Sender<ServiceEvent>,
    context_task: Option<JoinHandle<()>>,
    target_tasks: BTreeMap<(String, String), JoinHandle<()>>,
    target_generation: u64,
    source_task: Option<JoinHandle<()>>,
    source_generation: u64,
    load_generations: [u64; Section::COUNT],
    cwd: String,
}

impl ServiceController {
    pub fn new(
        client: Arc<DebuggerServiceApiClient>,
        events: mpsc::Sender<ServiceEvent>,
        cwd: String,
    ) -> Self {
        Self {
            client,
            events,
            context_task: None,
            target_tasks: BTreeMap::new(),
            target_generation: 0,
            source_task: None,
            source_generation: 0,
            load_generations: [0; Section::COUNT],
            cwd,
        }
    }

    pub fn observe_context_after(&mut self, context_id: String, revision: u64) {
        self.start_context_observer(context_id, ObservationCursor::After { revision });
    }

    pub fn switch_context(&mut self, context_id: String) {
        self.set_targets(Vec::new());
        self.set_source(None);
        self.start_context_observer(context_id, ObservationCursor::Current);
    }

    pub fn set_targets(&mut self, targets: Vec<TargetRef>) {
        for (_, task) in std::mem::take(&mut self.target_tasks) {
            task.abort();
        }
        self.target_generation = self.target_generation.wrapping_add(1);
        let generation = self.target_generation;
        for target in targets {
            let client = self.client.clone();
            let events = self.events.clone();
            let task_target = target.clone();
            let task = tokio::spawn(async move {
                let mut snapshot = match client
                    .get_target(
                        task_target.context_id.clone(),
                        task_target.connection_id.clone(),
                        task_target.target_id.clone(),
                    )
                    .await
                {
                    Ok(snapshot) => snapshot,
                    Err(_) => {
                        let _ = events
                            .send(ServiceEvent::TargetDetail {
                                generation,
                                target: task_target,
                                detail: None,
                            })
                            .await;
                        return;
                    }
                };
                if events
                    .send(ServiceEvent::TargetDetail {
                        generation,
                        target: task_target.clone(),
                        detail: Some(snapshot.clone()),
                    })
                    .await
                    .is_err()
                {
                    return;
                }
                loop {
                    match client
                        .observe_target(
                            task_target.context_id.clone(),
                            task_target.connection_id.clone(),
                            task_target.target_id.clone(),
                            snapshot.revision,
                            1_000,
                        )
                        .await
                    {
                        Ok(Some(next)) => {
                            snapshot = next;
                            if events
                                .send(ServiceEvent::TargetDetail {
                                    generation,
                                    target: task_target.clone(),
                                    detail: Some(snapshot.clone()),
                                })
                                .await
                                .is_err()
                            {
                                return;
                            }
                        }
                        Ok(None) => {}
                        Err(_) => {
                            let _ = events
                                .send(ServiceEvent::TargetDetail {
                                    generation,
                                    target: task_target.clone(),
                                    detail: None,
                                })
                                .await;
                            return;
                        }
                    }
                }
            });
            self.target_tasks.insert(
                (target.connection_id.clone(), target.target_id.clone()),
                task,
            );
        }
    }

    pub fn target_generation(&self) -> u64 {
        self.target_generation
    }

    pub fn load(
        &mut self,
        section: Section,
        context_id: String,
        source_kind: SourceTreeKind,
        expanded_process_roots: Vec<u32>,
    ) {
        let generation = self.load_generations[section.index()].wrapping_add(1);
        self.load_generations[section.index()] = generation;
        let client = self.client.clone();
        let events = self.events.clone();
        let cwd = self.cwd.clone();
        tokio::spawn(async move {
            let load = async {
                match section {
                    Section::Contexts => client
                        .list_contexts(Some(cwd))
                        .await
                        .map(Data::Contexts)
                        .map_err(rpc_error),
                    Section::Processes => client
                        .get_process_projection(context_id.clone(), expanded_process_roots)
                        .await
                        .map(Data::Processes)
                        .map_err(rpc_error),
                    Section::Connections => client
                        .get_resource_graph(context_id.clone())
                        .await
                        .map(Data::Resources)
                        .map_err(rpc_error),
                    Section::Sources => client
                        .show_source_tree(context_id.clone(), source_kind)
                        .await
                        .map(Data::Sources)
                        .map_err(rpc_error),
                    Section::Captures => client
                        .list_captures(context_id.clone())
                        .await
                        .map(Data::Captures)
                        .map_err(rpc_error),
                    Section::Targets
                    | Section::Attention
                    | Section::Breakpoints
                    | Section::CallStacks => unreachable!("derived sections are not loaded"),
                }
            };
            let timeout_seconds = if section == Section::Sources { 30 } else { 15 };
            let result =
                match tokio::time::timeout(Duration::from_secs(timeout_seconds), load).await {
                    Ok(result) => result,
                    Err(_) => Err(format!(
                        "{} query timed out after {timeout_seconds} seconds",
                        section.title(),
                    )),
                };
            let _ = events
                .send(ServiceEvent::DataLoaded {
                    section,
                    context_id,
                    generation,
                    result,
                })
                .await;
        });
    }

    pub fn load_generation(&self, section: Section) -> u64 {
        self.load_generations[section.index()]
    }

    pub fn set_source(&mut self, source: Option<(String, String, SourceTreeKind)>) {
        if let Some(task) = self.source_task.take() {
            task.abort();
        }
        self.source_generation = self.source_generation.wrapping_add(1);
        let generation = self.source_generation;
        let Some((context_id, path, source_kind)) = source else {
            return;
        };
        let view = match source_kind {
            SourceTreeKind::Formatted if path.ends_with("?formatted") => {
                SourceViewPreference::Formatted
            }
            SourceTreeKind::Formatted => SourceViewPreference::Original,
            SourceTreeKind::Loaded | SourceTreeKind::SourceMapped => SourceViewPreference::Original,
            SourceTreeKind::Resolved => SourceViewPreference::Policy,
        };
        let client = self.client.clone();
        let events = self.events.clone();
        self.source_task = Some(tokio::spawn(async move {
            let result = match tokio::time::timeout(
                Duration::from_secs(30),
                client.show_source(
                    context_id.clone(),
                    path.clone(),
                    SourceDisplayOptions {
                        line: None,
                        context_lines: 0,
                        view,
                    },
                ),
            )
            .await
            {
                Ok(result) => result.map_err(rpc_error),
                Err(_) => Err("Source query timed out after 30 seconds".to_owned()),
            };
            let _ = events
                .send(ServiceEvent::SourceLoaded {
                    context_id,
                    path,
                    generation,
                    result,
                })
                .await;
        }));
    }

    pub fn source_generation(&self) -> u64 {
        self.source_generation
    }

    pub fn perform(&self, action: UiAction) {
        let client = self.client.clone();
        let events = self.events.clone();
        tokio::spawn(async move {
            let result = perform_action(&client, action).await;
            let _ = events.send(ServiceEvent::ActionFinished(result)).await;
        });
    }

    fn start_context_observer(&mut self, context_id: String, mut cursor: ObservationCursor) {
        if let Some(task) = self.context_task.take() {
            task.abort();
        }
        let client = self.client.clone();
        let events = self.events.clone();
        self.context_task = Some(tokio::spawn(async move {
            loop {
                match client
                    .observe_context(context_id.clone(), cursor.clone(), 1_000)
                    .await
                {
                    Ok(ObservationResult::Items { items }) => {
                        for observation in items {
                            cursor = ObservationCursor::After {
                                revision: observation.snapshot.revision,
                            };
                            if events
                                .send(ServiceEvent::Context(observation.snapshot))
                                .await
                                .is_err()
                            {
                                return;
                            }
                        }
                    }
                    Ok(ObservationResult::HistoryGap { current, .. }) => {
                        cursor = ObservationCursor::After {
                            revision: current.revision,
                        };
                        if events.send(ServiceEvent::Context(current)).await.is_err() {
                            return;
                        }
                    }
                    Err(error) => {
                        if events
                            .send(ServiceEvent::ContextError(rpc_error(error)))
                            .await
                            .is_err()
                        {
                            return;
                        }
                        tokio::time::sleep(Duration::from_secs(1)).await;
                        cursor = ObservationCursor::Current;
                    }
                }
            }
        }));
    }
}

impl Drop for ServiceController {
    fn drop(&mut self) {
        if let Some(task) = self.context_task.take() {
            task.abort();
        }
        for (_, task) in std::mem::take(&mut self.target_tasks) {
            task.abort();
        }
        if let Some(task) = self.source_task.take() {
            task.abort();
        }
    }
}

async fn perform_action(
    client: &DebuggerServiceApiClient,
    action: UiAction,
) -> Result<(String, ContextSnapshot), String> {
    match action {
        UiAction::ConfigureConnectionPath { path } => configure_connection_path(client, path).await,
        UiAction::SetConnectionPathConfigured { path, configured } => {
            set_connection_path_configured(client, path, configured).await
        }
        UiAction::SetConnection {
            context_id,
            connection_id,
            connected,
        } => {
            let snapshot = if connected {
                client
                    .connect_connection(context_id, connection_id.clone())
                    .await
            } else {
                client
                    .disconnect_connection(context_id, connection_id.clone())
                    .await
            }
            .map_err(rpc_error)?;
            Ok((
                format!(
                    "Connection {connection_id} {}",
                    if connected {
                        "connected"
                    } else {
                        "disconnected"
                    }
                ),
                snapshot,
            ))
        }
        UiAction::DeleteConnection {
            context_id,
            connection_id,
            expected_revision,
        } => {
            let snapshot = client
                .delete_connection(
                    context_id,
                    connection_id.clone(),
                    MutationOptions {
                        expected_revision: Some(expected_revision),
                        request_id: None,
                    },
                )
                .await
                .map_err(rpc_error)?;
            Ok((format!("Connection {connection_id} removed"), snapshot))
        }
        UiAction::SetTargetAttachment {
            target,
            attached,
            force,
        } => {
            let snapshot = if attached {
                client
                    .attach_target(
                        target.context_id.clone(),
                        target.connection_id.clone(),
                        target.target_id.clone(),
                        TargetAttachOptions {
                            force,
                            expected_connection_generation: Some(target.connection_generation),
                        },
                    )
                    .await
                    .map_err(rpc_error)?;
                client
                    .get_context(target.context_id.clone())
                    .await
                    .map_err(rpc_error)?
            } else {
                client
                    .detach_target(
                        target.context_id.clone(),
                        target.connection_id.clone(),
                        target.target_id.clone(),
                        Some(target.connection_generation),
                    )
                    .await
                    .map_err(rpc_error)?
            };
            Ok((
                format!(
                    "Target {}/{} {}",
                    target.connection_id,
                    target.target_id,
                    if attached { "attached" } else { "detached" }
                ),
                snapshot,
            ))
        }
        UiAction::PutBreakpoint {
            context_id,
            breakpoint_id,
            source_path,
            line,
        } => {
            let snapshot = client
                .put_breakpoint(context_id, breakpoint_id.clone(), source_path, line, 1)
                .await
                .map_err(rpc_error)?;
            Ok((format!("Breakpoint {breakpoint_id} set"), snapshot))
        }
        UiAction::DeleteBreakpoint {
            context_id,
            breakpoint_id,
            expected_revision,
        } => {
            let snapshot = client
                .delete_breakpoint(
                    context_id,
                    breakpoint_id.clone(),
                    MutationOptions {
                        expected_revision: Some(expected_revision),
                        request_id: None,
                    },
                )
                .await
                .map_err(rpc_error)?;
            Ok((format!("Breakpoint {breakpoint_id} removed"), snapshot))
        }
    }
}

async fn configure_connection_path(
    client: &DebuggerServiceApiClient,
    path: ConnectionPathRef,
) -> Result<(String, ContextSnapshot), String> {
    let (_, configuration) = connection_path_spec(
        path.root_process_id,
        path.process_id,
        path.debug_target_id.as_deref(),
    );
    let connection_id = path.connection_id.clone();
    let context = client
        .get_context(path.context_id.clone())
        .await
        .map_err(rpc_error)?;
    let existing = context
        .connections
        .iter()
        .find(|connection| connection.id == connection_id)
        .cloned();
    let needs_configuration = existing
        .as_ref()
        .is_none_or(|connection| connection.configuration != configuration);
    if let Some(connection) = &existing {
        if connection.status == ConnectionStatus::Disconnecting {
            return Err(format!(
                "connection '{connection_id}' is still disconnecting; retry when it is disconnected"
            ));
        }
        if connection.configuration != configuration
            && matches!(
                connection.status,
                ConnectionStatus::Connected { .. }
                    | ConnectionStatus::Connecting
                    | ConnectionStatus::Disconnecting
            )
        {
            return Err(format!(
                "connection '{connection_id}' is active with a different process configuration"
            ));
        }
    }
    if needs_configuration {
        client
            .put_connection(
                path.context_id.clone(),
                connection_id.clone(),
                configuration.clone(),
            )
            .await
            .map_err(rpc_error)?;
    }
    let snapshot = client
        .connect_connection(path.context_id, connection_id.clone())
        .await
        .map_err(rpc_error)?;
    Ok((
        format!("Process {} connected as {connection_id}", path.process_id),
        snapshot,
    ))
}

async fn set_connection_path_configured(
    client: &DebuggerServiceApiClient,
    path: ConnectionPathRef,
    configured: bool,
) -> Result<(String, ContextSnapshot), String> {
    let (_, configuration) = connection_path_spec(
        path.root_process_id,
        path.process_id,
        path.debug_target_id.as_deref(),
    );
    if configured {
        let snapshot = client
            .put_connection(path.context_id, path.connection_id.clone(), configuration)
            .await
            .map_err(rpc_error)?;
        return Ok((format!("Connection {} added", path.connection_id), snapshot));
    }

    let context = client
        .get_context(path.context_id.clone())
        .await
        .map_err(rpc_error)?;
    let connection = context
        .connections
        .iter()
        .find(|connection| {
            connection.id == path.connection_id && connection.configuration == configuration
        })
        .ok_or_else(|| {
            format!(
                "connection '{}' is no longer configured",
                path.connection_id
            )
        })?;
    let snapshot = match connection.status {
        ConnectionStatus::Connected { .. } | ConnectionStatus::Connecting => client
            .disconnect_connection(path.context_id.clone(), path.connection_id.clone())
            .await
            .map_err(rpc_error)?,
        ConnectionStatus::Disconnecting => {
            return Err(format!(
                "connection '{}' is still disconnecting",
                path.connection_id
            ));
        }
        ConnectionStatus::Disconnected | ConnectionStatus::Failed { .. } => context,
    };
    let snapshot = client
        .delete_connection(
            path.context_id,
            path.connection_id.clone(),
            MutationOptions {
                expected_revision: Some(snapshot.revision),
                request_id: None,
            },
        )
        .await
        .map_err(rpc_error)?;
    Ok((
        format!("Connection {} removed", path.connection_id),
        snapshot,
    ))
}

fn rpc_error(error: impl std::fmt::Debug) -> String {
    format!("{error:?}")
}

fn configured_context_id(selection_file: &Path, cwd: &str) -> Result<Option<String>, String> {
    let bytes = match std::fs::read(selection_file) {
        Ok(bytes) => bytes,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(error.to_string()),
    };
    let value: serde_json::Value =
        serde_json::from_slice(&bytes).map_err(|error| error.to_string())?;
    if value
        .get("schemaVersion")
        .and_then(serde_json::Value::as_u64)
        == Some(2)
    {
        let bindings = value
            .get("cwdBindings")
            .and_then(serde_json::Value::as_object);
        return Ok(path_and_parents(cwd)
            .map_err(|error| error.to_string())?
            .into_iter()
            .find_map(|directory| {
                bindings?
                    .get(&directory)
                    .and_then(serde_json::Value::as_str)
                    .map(str::to_owned)
            }));
    }
    Ok(value
        .get("context")
        .or_else(|| value.get("workspace"))
        .and_then(serde_json::Value::as_str)
        .map(str::to_owned))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn configured_context_uses_the_nearest_cli_cwd_binding() {
        let path =
            std::env::temp_dir().join(format!("jsdbg-tui-selection-{}.json", std::process::id()));
        std::fs::write(
            &path,
            br#"{
                "schemaVersion": 2,
                "cwdBindings": {
                    "d:\\work": "parent",
                    "d:\\work\\project": "project"
                },
                "activeScopes": {},
                "scopes": {}
            }"#,
        )
        .unwrap();

        let selected = configured_context_id(&path, "d:\\work\\project\\src").unwrap();
        let _ = std::fs::remove_file(path);

        assert_eq!(selected.as_deref(), Some("project"));
    }
}
