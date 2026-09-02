use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

use cdp_client::context_identity::{
    normalize_absolute_path, resolve_context_expression, synthetic_node_target_id,
};
use cdp_client::local_rpc::{default_state_file, ensure_service};
use cdp_client::service_api::{
    ConnectionConfiguration, ConnectionStatus, ContextSnapshot, ContextSummary,
    DebuggerServiceApiClient, MutationOptions, ObservationCursor, ObservationResult, ProcessRole,
    ProcessTreeSnapshot, SourceTreeKind, SourceTreeSnapshot, TargetAttachOptions,
    TargetAttachmentState, TargetDebuggerSnapshot,
};
use tokio::sync::mpsc;
use tokio::task::JoinHandle;

use crate::app::{ProcessRef, Tab, TargetRef, UiAction};

pub struct Bootstrap {
    pub client: Arc<DebuggerServiceApiClient>,
    pub contexts: Vec<ContextSummary>,
    pub context_index: usize,
    pub context: ContextSnapshot,
}

impl Bootstrap {
    pub async fn connect(context_expression: Option<&str>) -> Result<Self, String> {
        let client = Arc::new(
            ensure_service(&default_state_file())
                .await
                .map_err(|error| error.to_string())?,
        );
        let cwd = std::env::current_dir().map_err(|error| error.to_string())?;
        let normalized_cwd = normalize_absolute_path(&cwd).map_err(|error| error.to_string())?;
        let contexts = client
            .list_contexts(Some(normalized_cwd))
            .await
            .map_err(rpc_error)?;
        if contexts.is_empty() {
            return Err(
                "no debugger contexts exist; create one with `jsdbg context create`".to_owned(),
            );
        }
        let context_index = if let Some(expression) = context_expression {
            let id = resolve_context_expression(expression, Path::new(&cwd))
                .map_err(|error| error.to_string())?
                .id;
            contexts
                .iter()
                .position(|context| context.id == id)
                .ok_or_else(|| format!("context '{id}' does not exist"))?
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
            contexts,
            context_index,
            context,
        })
    }
}

pub enum Data {
    Processes(Vec<ProcessTreeSnapshot>),
    Sources(SourceTreeSnapshot),
    Captures(Vec<cdp_client::service_api::CaptureSnapshot>),
}

pub enum ServiceEvent {
    Context(ContextSnapshot),
    ContextError(String),
    TargetDetail {
        generation: u64,
        detail: Option<TargetDebuggerSnapshot>,
    },
    DataLoaded {
        tab: Tab,
        context_id: String,
        generation: u64,
        result: Result<Data, String>,
    },
    ActionFinished(Result<(String, ContextSnapshot), String>),
}

pub struct ServiceController {
    client: Arc<DebuggerServiceApiClient>,
    events: mpsc::Sender<ServiceEvent>,
    context_task: Option<JoinHandle<()>>,
    target_task: Option<JoinHandle<()>>,
    target_generation: u64,
    load_generations: [u64; 6],
}

impl ServiceController {
    pub fn new(client: Arc<DebuggerServiceApiClient>, events: mpsc::Sender<ServiceEvent>) -> Self {
        Self {
            client,
            events,
            context_task: None,
            target_task: None,
            target_generation: 0,
            load_generations: [0; 6],
        }
    }

    pub fn observe_context_after(&mut self, context_id: String, revision: u64) {
        self.start_context_observer(context_id, ObservationCursor::After { revision });
    }

    pub fn switch_context(&mut self, context_id: String) {
        self.set_target(None);
        self.start_context_observer(context_id, ObservationCursor::Current);
    }

    pub fn set_target(&mut self, target: Option<TargetRef>) {
        if let Some(task) = self.target_task.take() {
            task.abort();
        }
        self.target_generation = self.target_generation.wrapping_add(1);
        let generation = self.target_generation;
        let Some(target) = target else {
            let events = self.events.clone();
            tokio::spawn(async move {
                let _ = events
                    .send(ServiceEvent::TargetDetail {
                        generation,
                        detail: None,
                    })
                    .await;
            });
            return;
        };
        let client = self.client.clone();
        let events = self.events.clone();
        self.target_task = Some(tokio::spawn(async move {
            let mut snapshot = match client
                .get_target(
                    target.context_id.clone(),
                    target.connection_id.clone(),
                    target.target_id.clone(),
                )
                .await
            {
                Ok(snapshot) => snapshot,
                Err(_) => {
                    let _ = events
                        .send(ServiceEvent::TargetDetail {
                            generation,
                            detail: None,
                        })
                        .await;
                    return;
                }
            };
            if events
                .send(ServiceEvent::TargetDetail {
                    generation,
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
                        target.context_id.clone(),
                        target.connection_id.clone(),
                        target.target_id.clone(),
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
                                detail: None,
                            })
                            .await;
                        return;
                    }
                }
            }
        }));
    }

    pub fn target_generation(&self) -> u64 {
        self.target_generation
    }

    pub fn load(&mut self, tab: Tab, context_id: String, source_kind: SourceTreeKind) {
        let generation = self.load_generations[tab.index()].wrapping_add(1);
        self.load_generations[tab.index()] = generation;
        let client = self.client.clone();
        let events = self.events.clone();
        tokio::spawn(async move {
            let result = match tab {
                Tab::Processes => client
                    .discover_vscode_process_trees()
                    .await
                    .map(Data::Processes)
                    .map_err(rpc_error),
                Tab::Sources => client
                    .show_source_tree(context_id.clone(), source_kind)
                    .await
                    .map(Data::Sources)
                    .map_err(rpc_error),
                Tab::Captures => client
                    .list_captures(context_id.clone())
                    .await
                    .map(Data::Captures)
                    .map_err(rpc_error),
                Tab::Connections | Tab::Targets | Tab::Breakpoints => return,
            };
            let _ = events
                .send(ServiceEvent::DataLoaded {
                    tab,
                    context_id,
                    generation,
                    result,
                })
                .await;
        });
    }

    pub fn load_generation(&self, tab: Tab) -> u64 {
        self.load_generations[tab.index()]
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
        if let Some(task) = self.target_task.take() {
            task.abort();
        }
    }
}

async fn perform_action(
    client: &DebuggerServiceApiClient,
    action: UiAction,
) -> Result<(String, ContextSnapshot), String> {
    match action {
        UiAction::AttachProcess { process } => attach_process(client, process).await,
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
    }
}

async fn attach_process(
    client: &DebuggerServiceApiClient,
    process: ProcessRef,
) -> Result<(String, ContextSnapshot), String> {
    let (connection_id, configuration, target_id, renderer_process_id) =
        if let Some(target_id) = process.debug_target_id.clone() {
            (
                format!("process-tree-{}", process.root_process_id),
                ConnectionConfiguration::ProcessTree {
                    root_pid: process.root_process_id,
                },
                target_id,
                (process.role == ProcessRole::Renderer).then_some(process.process_id),
            )
        } else {
            let connection_id = format!("process-{}", process.process_id);
            (
                connection_id.clone(),
                ConnectionConfiguration::Process {
                    process_id: process.process_id,
                },
                synthetic_node_target_id(&connection_id),
                None,
            )
        };

    let context = client
        .get_context(process.context_id.clone())
        .await
        .map_err(rpc_error)?;
    let existing = context
        .connections
        .iter()
        .find(|connection| connection.id == connection_id);
    if let Some(connection) = existing {
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
    let needs_connection = existing.is_none_or(|connection| {
        connection.configuration != configuration
            || matches!(
                connection.status,
                ConnectionStatus::Disconnected | ConnectionStatus::Failed { .. }
            )
    });
    if needs_connection {
        client
            .put_connection(
                process.context_id.clone(),
                connection_id.clone(),
                configuration,
            )
            .await
            .map_err(rpc_error)?;
        client
            .connect_connection(process.context_id.clone(), connection_id.clone())
            .await
            .map_err(rpc_error)?;
    }

    let target_id = if let Some(process_id) = renderer_process_id {
        resolve_renderer_target_id(client, &process.context_id, &connection_id, process_id).await?
    } else if target_id == "$node-root" {
        synthetic_node_target_id(&connection_id)
    } else {
        target_id
    };
    let (context, connection_generation, attachment) =
        wait_for_target(client, &process.context_id, &connection_id, &target_id).await?;
    if attachment == TargetAttachmentState::Debugger {
        return Ok((
            format!(
                "Process {} is already attached as {connection_id}/{target_id}",
                process.process_id
            ),
            context,
        ));
    }

    let attach_result = client
        .attach_target(
            process.context_id.clone(),
            connection_id.clone(),
            target_id.clone(),
            TargetAttachOptions {
                force: false,
                expected_connection_generation: Some(connection_generation),
            },
        )
        .await;
    if let Err(error) = attach_result {
        let attach_error = rpc_error(error);
        let current = client
            .get_context(process.context_id.clone())
            .await
            .map_err(|verification_error| {
                format!(
                    "{}; attachment state could not be verified: {}",
                    attach_error,
                    rpc_error(verification_error)
                )
            })?;
        let attached_concurrently = current.target_forest.iter().any(|target| {
            target.connection_id == connection_id
                && target.connection_generation == connection_generation
                && target.target.target_id == target_id
                && target.attachment == TargetAttachmentState::Debugger
        });
        if !attached_concurrently {
            return Err(attach_error);
        }
    }
    let context = client
        .get_context(process.context_id)
        .await
        .map_err(rpc_error)?;
    Ok((
        format!(
            "Process {} attached as {connection_id}/{target_id}",
            process.process_id
        ),
        context,
    ))
}

async fn resolve_renderer_target_id(
    client: &DebuggerServiceApiClient,
    context_id: &str,
    connection_id: &str,
    process_id: u32,
) -> Result<String, String> {
    let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
    loop {
        let context = client
            .get_context(context_id.to_owned())
            .await
            .map_err(rpc_error)?;
        ensure_connection_can_publish_target(
            &context,
            connection_id,
            &format!("renderer process {process_id}"),
        )?;
        let graph = client
            .get_resource_graph(context_id.to_owned())
            .await
            .map_err(rpc_error)?;
        let mut candidates = graph
            .resources
            .iter()
            .filter(|resource| {
                resource
                    .attributes
                    .get("connectionId")
                    .and_then(serde_json::Value::as_str)
                    == Some(connection_id)
                    && resource
                        .attributes
                        .get("processId")
                        .and_then(serde_json::Value::as_u64)
                        == Some(u64::from(process_id))
                    && resource
                        .attributes
                        .get("subtype")
                        .and_then(serde_json::Value::as_str)
                        == Some("electron-renderer")
            })
            .filter_map(|resource| {
                resource
                    .attributes
                    .get("targetId")
                    .and_then(serde_json::Value::as_str)
                    .map(str::to_owned)
            })
            .collect::<Vec<_>>();
        candidates.sort();
        candidates.dedup();
        match candidates.as_slice() {
            [target_id] => return Ok(target_id.clone()),
            [] if tokio::time::Instant::now() < deadline => {
                tokio::time::sleep(Duration::from_millis(100)).await;
            }
            [] => {
                return Err(format!(
                    "renderer process {process_id} has no live Electron webContents"
                ));
            }
            _ => {
                return Err(format!(
                    "renderer process {process_id} maps to multiple Electron webContents targets: {}",
                    candidates.join(", ")
                ));
            }
        }
    }
}

async fn wait_for_target(
    client: &DebuggerServiceApiClient,
    context_id: &str,
    connection_id: &str,
    target_id: &str,
) -> Result<(ContextSnapshot, u64, TargetAttachmentState), String> {
    let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
    loop {
        let context = client
            .get_context(context_id.to_owned())
            .await
            .map_err(rpc_error)?;
        ensure_connection_can_publish_target(&context, connection_id, target_id)?;
        if let Some(target) = context.target_forest.iter().find(|target| {
            target.connection_id == connection_id && target.target.target_id == target_id
        }) {
            return Ok((
                context.clone(),
                target.connection_generation,
                target.attachment,
            ));
        }
        if tokio::time::Instant::now() >= deadline {
            return Err(format!(
                "target {connection_id}/{target_id} was not published within 10 seconds"
            ));
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
}

fn ensure_connection_can_publish_target(
    context: &ContextSnapshot,
    connection_id: &str,
    target: &str,
) -> Result<(), String> {
    let connection = context
        .connections
        .iter()
        .find(|connection| connection.id == connection_id)
        .ok_or_else(|| format!("connection '{connection_id}' is no longer available"))?;
    match &connection.status {
        ConnectionStatus::Failed { message } => {
            Err(format!("connection '{connection_id}' failed: {message}"))
        }
        ConnectionStatus::Disconnected => Err(format!(
            "connection '{connection_id}' disconnected before {target} was available"
        )),
        ConnectionStatus::Disconnecting => Err(format!(
            "connection '{connection_id}' is disconnecting before {target} was available"
        )),
        ConnectionStatus::Connecting | ConnectionStatus::Connected { .. } => Ok(()),
    }
}

fn rpc_error(error: impl std::fmt::Debug) -> String {
    format!("{error:?}")
}
