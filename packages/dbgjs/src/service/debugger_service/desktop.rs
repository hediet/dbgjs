use super::*;
use crate::api::service_api::{
    ConnectionStatus, ContextKind, ContextSnapshot, DesktopApi, DesktopAttachResult, DesktopClientMessage, DesktopInfo,
    DesktopOptions, DesktopProtocol, DesktopServerMessage, IdleTimeout, MachineTarget, MachineTargetRef, MutationOptions,
    ProcessRootKind, ServiceApi,
};
use linkrpc::prelude::StreamReceiver;

/// The service-managed context that holds one process-tree connection per root for machine targets.
const MACHINE_CONTEXT: &str = "dbgjs.machine";
/// How long a newly connected process tree may take to report a requested target.
const MACHINE_TARGET_TIMEOUT: Duration = Duration::from_secs(5);
/// Machine connections close after this long without discovery or an attached desktop.
const MACHINE_IDLE_TIMEOUT_MS: u64 = 10 * 60 * 1000;

/// Serializes setup per root, so concurrent callers do not configure or connect one tree twice.
static MACHINE_SETUP: std::sync::LazyLock<std::sync::Mutex<BTreeMap<u32, Arc<tokio::sync::Mutex<()>>>>> =
    std::sync::LazyLock::new(Default::default);
/// Roots that could not be connected recently (e.g. WebView2 hosts without an inspector), so
/// repeated discovery reports them without spending another connection attempt.
static MACHINE_FAILURES: std::sync::LazyLock<std::sync::Mutex<BTreeMap<u32, (Instant, String)>>> =
    std::sync::LazyLock::new(Default::default);
/// How long a failed connection attempt is reported instead of retried.
const MACHINE_FAILURE_TTL: Duration = Duration::from_secs(60);

/// Serializes creating the machine context itself.
static MACHINE_CONTEXT_SETUP: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());
#[async_trait::async_trait]
impl DesktopApi for DebuggerService {
    async fn info(&self, ctx: &CallCtx, target: MachineTargetRef) -> Result<DesktopInfo, JsonRpcError> {
        let (target, _, title) = self.machine_target(ctx, &target).await?;
        Ok(DesktopInfo {
            title,
            available: true,
            protocol: DesktopProtocol::Rfb,
            target,
        })
    }

    async fn attach(
        &self,
        ctx: &CallCtx,
        target: MachineTargetRef,
        options: DesktopOptions,
        mut input: StreamReceiver<DesktopClientMessage>,
        output: StreamSender<DesktopServerMessage>,
    ) -> Result<DesktopAttachResult, JsonRpcError> {
        let (_, target_ref, title) = self.machine_target(ctx, &target).await?;
        let _activity = self.connection_activity(&target_ref.connection).await;
        let (debugger, owned) = self.stream_target(&target_ref, "desktop").await?;
        let result = crate::desktop::serve(debugger.clone(), title, options, &mut input, output, ctx.cancelled()).await;
        self.release_stream_target(&target_ref, &debugger, owned).await;
        result
    }
}

impl DebuggerService {
    /// Connects `root_pid`'s process tree in the machine context unless it is connected; returns
    /// whether this call connected it.
    pub(super) async fn ensure_machine_connection(&self, ctx: &CallCtx, root_pid: u32) -> Result<bool, JsonRpcError> {
        let context = self.machine_context(ctx).await?;
        let root_setup = MACHINE_SETUP.lock().unwrap().entry(root_pid).or_default().clone();
        let _setup = root_setup.lock().await;
        let connection = machine_connection(root_pid);
        let context = match ContextApi::get_context(self, ctx, MACHINE_CONTEXT.to_owned()).await {
            Ok(current) => current,
            Err(_) => context,
        };
        let existing = context.connections.iter().find(|candidate| candidate.id == connection.connection_id);
        if matches!(existing.map(|candidate| &candidate.status), Some(ConnectionStatus::Connected { .. })) {
            return Ok(false);
        }
        if let Some((failed_at, message)) = MACHINE_FAILURES.lock().unwrap().get(&root_pid)
            && failed_at.elapsed() < MACHINE_FAILURE_TTL
        {
            return Err(JsonRpcError::new(error_codes::INVALID_PARAMS, message.clone()));
        }
        let configuration = ConnectionConfiguration::ProcessTree { root_pid };
        if existing.map(|candidate| &candidate.configuration) != Some(&configuration) {
            ContextApi::put_connection(self, ctx, connection.clone(), configuration).await?;
        }
        let context = ContextApi::connect_connection(self, ctx, connection.clone()).await?;
        match context
            .connections
            .iter()
            .find(|candidate| candidate.id == connection.connection_id)
            .map(|candidate| &candidate.status)
        {
            Some(ConnectionStatus::Failed { message }) => {
                let message = format!("cannot connect to process tree {root_pid}: {message}");
                MACHINE_FAILURES.lock().unwrap().insert(root_pid, (Instant::now(), message.clone()));
                // Process IDs are not reused soon, so a failed root's entry would only accumulate.
                let _ = ContextApi::delete_connection(self, ctx, connection, MutationOptions::default()).await;
                Err(JsonRpcError::new(error_codes::INVALID_PARAMS, message))
            }
            _ => {
                MACHINE_FAILURES.lock().unwrap().remove(&root_pid);
                Ok(true)
            }
        }
    }

    async fn machine_context(&self, ctx: &CallCtx) -> Result<ContextSnapshot, JsonRpcError> {
        let _setup = MACHINE_CONTEXT_SETUP.lock().await;
        if let Ok(context) = ContextApi::get_context(self, ctx, MACHINE_CONTEXT.to_owned()).await {
            return Ok(context);
        }
        ContextApi::put_context(
            self,
            ctx,
            MACHINE_CONTEXT.to_owned(),
            ContextKind::Named,
            Some("Machine targets (dbgjs desktops and discovery)".to_owned()),
        ).await?;
        ContextApi::set_context_idle_timeout(
            self,
            ctx,
            MACHINE_CONTEXT.to_owned(),
            IdleTimeout::After { milliseconds: MACHINE_IDLE_TIMEOUT_MS },
        ).await
    }

    /// Resolves `target` to a target of the machine context, connecting its process tree if
    /// needed, and returns the resolved target and its title.
    async fn machine_target(
        &self,
        ctx: &CallCtx,
        target: &MachineTargetRef,
    ) -> Result<(MachineTarget, TargetRef, String), JsonRpcError> {
        let target = match target {
            MachineTargetRef::Target(target) => target.clone(),
            MachineTargetRef::Window(window) => self.machine_window(ctx, &window.window).await?,
        };
        self.ensure_machine_connection(ctx, target.root_pid).await?;
        let connection = machine_connection(target.root_pid);
        let deadline = tokio::time::Instant::now() + MACHINE_TARGET_TIMEOUT;
        loop {
            let context = ContextApi::get_context(self, ctx, MACHINE_CONTEXT.to_owned()).await?;
            if let Some(node) = context.target_forest.iter().find(|node| {
                node.connection_id == connection.connection_id && node.target.target_id == target.target_id
            }) {
                let title = node.target.title.clone();
                let target_ref = TargetRef { connection, target_id: target.target_id.clone() };
                return Ok((target, target_ref, title));
            }
            if tokio::time::Instant::now() >= deadline {
                return Err(JsonRpcError::new(error_codes::INVALID_PARAMS, format!(
                    "target '{}' was not found in process tree {}", target.target_id, target.root_pid,
                )));
            }
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
    }

    /// The first page titled `title`: among the machine context's connected process trees, else
    /// among all VS Code, Electron, and browser process trees.
    async fn machine_window(&self, ctx: &CallCtx, title: &str) -> Result<MachineTarget, JsonRpcError> {
        if let Ok(context) = ContextApi::get_context(self, ctx, MACHINE_CONTEXT.to_owned()).await {
            let connected = context
                .connections
                .iter()
                .filter(|connection| matches!(connection.status, ConnectionStatus::Connected { .. }))
                .filter_map(|connection| match connection.configuration {
                    ConnectionConfiguration::ProcessTree { root_pid } => Some((connection.id.as_str(), root_pid)),
                    _ => None,
                })
                .collect::<BTreeMap<_, _>>();
            if let Some(found) = context.target_forest.iter().find_map(|node| {
                let root_pid = *connected.get(node.connection_id.as_str())?;
                is_window(&node.target.target_type, &node.target.title, title).then(|| MachineTarget {
                    root_pid,
                    target_id: node.target.target_id.clone(),
                })
            }) {
                return Ok(found);
            }
        }
        let discovered = futures_util::future::join_all(
            [ProcessRootKind::Vscode, ProcessRootKind::Electron, ProcessRootKind::Browser]
                .map(|root_kind| ServiceApi::discover_process_trees(self, ctx, Some(root_kind), false, true)),
        )
        .await;
        let mut failures = Vec::new();
        for result in discovered {
            match result {
                Ok(trees) => {
                    if let Some(found) = trees.iter().find_map(|tree| {
                        tree.targets.iter().find_map(|target| {
                            is_window(&target.target.target_type, &target.target.title, title).then(|| MachineTarget {
                                root_pid: tree.root_process_id,
                                target_id: target.target.target_id.clone(),
                            })
                        })
                    }) {
                        return Ok(found);
                    }
                }
                Err(error) => failures.push(error.message),
            }
        }
        Err(JsonRpcError::new(
            error_codes::INVALID_PARAMS,
            if failures.is_empty() {
                format!("no page is titled \"{title}\"")
            } else {
                format!("no page is titled \"{title}\"; discovery failed: {}", failures.join("; "))
            },
        ))
    }
}

fn is_window(target_type: &str, title: &str, wanted: &str) -> bool {
    target_type == "page" && title.trim().to_lowercase() == wanted.trim().to_lowercase()
}

fn machine_connection(root_pid: u32) -> ConnectionRef {
    ConnectionRef {
        context_id: MACHINE_CONTEXT.to_owned(),
        connection_id: format!("root-{root_pid}"),
    }
}
#[cfg(test)]
mod tests {
    use super::is_window;

    #[test]
    fn windows_match_page_titles_ignoring_case_and_surrounding_whitespace() {
        assert!(is_window("page", " Agents - Visual Studio Code ", "agents - visual studio code"));
        assert!(is_window("page", "Grüße", "GRÜẞE"));
        assert!(!is_window("worker", "Agents", "Agents"));
        assert!(!is_window("page", "Agents 2", "Agents"));
    }
}
