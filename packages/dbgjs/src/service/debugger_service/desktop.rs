use super::*;
use crate::api::service_api::{
    ConnectionStatus, ContextKind, DesktopApi, DesktopAttachResult, DesktopClientMessage, DesktopInfo,
    DesktopOptions, DesktopProtocol, DesktopServerMessage, MachineTargetRef,
};
use linkrpc::prelude::StreamReceiver;

/// The service-managed context that holds one process-tree connection per root for machine targets.
const MACHINE_CONTEXT: &str = "dbgjs.machine";
/// How long a newly connected process tree may take to report a requested target.
const MACHINE_TARGET_TIMEOUT: Duration = Duration::from_secs(5);

/// Serializes machine-context setup so concurrent attaches do not configure the same connection twice.
static MACHINE_SETUP: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

#[async_trait::async_trait]
impl DesktopApi for DebuggerService {
    async fn info(&self, ctx: &CallCtx, target: MachineTargetRef) -> Result<DesktopInfo, JsonRpcError> {
        let (_, title) = self.machine_target(ctx, &target).await?;
        Ok(DesktopInfo {
            title,
            available: true,
            protocol: DesktopProtocol::Rfb,
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
        let (target_ref, title) = self.machine_target(ctx, &target).await?;
        let _activity = self.connection_activity(&target_ref.connection).await;
        let (debugger, owned) = self.stream_target(&target_ref, "desktop").await?;
        let result = crate::desktop::serve(debugger.clone(), title, options, &mut input, output, ctx.cancelled()).await;
        self.release_stream_target(&target_ref, &debugger, owned).await;
        result
    }
}

impl DebuggerService {
    /// Resolves `target` to a target of the managed machine context, connecting its process tree
    /// if needed, and returns the target's title.
    async fn machine_target(&self, ctx: &CallCtx, target: &MachineTargetRef) -> Result<(TargetRef, String), JsonRpcError> {
        let connection = ConnectionRef {
            context_id: MACHINE_CONTEXT.to_owned(),
            connection_id: format!("root-{}", target.root_pid),
        };
        {
            let _setup = MACHINE_SETUP.lock().await;
            let context = match ContextApi::get_context(self, ctx, MACHINE_CONTEXT.to_owned()).await {
                Ok(context) => context,
                Err(_) => ContextApi::put_context(
                    self,
                    ctx,
                    MACHINE_CONTEXT.to_owned(),
                    ContextKind::Named,
                    Some("Machine targets (dbgjs desktops)".to_owned()),
                ).await?,
            };
            let existing = context.connections.iter().find(|candidate| candidate.id == connection.connection_id);
            let configuration = ConnectionConfiguration::ProcessTree { root_pid: target.root_pid };
            if existing.map(|candidate| &candidate.configuration) != Some(&configuration) {
                ContextApi::put_connection(self, ctx, connection.clone(), configuration).await?;
            }
            if !matches!(existing.map(|candidate| &candidate.status), Some(ConnectionStatus::Connected { .. })) {
                let context = ContextApi::connect_connection(self, ctx, connection.clone()).await?;
                if let Some(ConnectionStatus::Failed { message }) = context
                    .connections
                    .iter()
                    .find(|candidate| candidate.id == connection.connection_id)
                    .map(|candidate| &candidate.status)
                {
                    return Err(JsonRpcError::new(error_codes::INVALID_PARAMS, format!(
                        "cannot connect to process tree {}: {message}", target.root_pid,
                    )));
                }
            }
        }
        let deadline = tokio::time::Instant::now() + MACHINE_TARGET_TIMEOUT;
        loop {
            let context = ContextApi::get_context(self, ctx, MACHINE_CONTEXT.to_owned()).await?;
            if let Some(node) = context.target_forest.iter().find(|node| {
                node.connection_id == connection.connection_id && node.target.target_id == target.target_id
            }) {
                let title = node.target.title.clone();
                return Ok((TargetRef { connection, target_id: target.target_id.clone() }, title));
            }
            if tokio::time::Instant::now() >= deadline {
                return Err(JsonRpcError::new(error_codes::INVALID_PARAMS, format!(
                    "target '{}' was not found in process tree {}", target.target_id, target.root_pid,
                )));
            }
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
    }
}
