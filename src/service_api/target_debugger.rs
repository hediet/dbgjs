use super::*;

#[link_rpc_interface(id = "dev.dbgjs.target-debugger")]
pub trait TargetDebuggerApi {
    async fn resolve_target(
        context_id: String,
        selector: String,
    ) -> Result<CanonicalTargetSnapshot, JsonRpcError>;

    async fn attach_target(
        context_id: String,
        connection_id: String,
        target_id: String,
        options: TargetAttachOptions,
    ) -> Result<TargetAttachmentResult, JsonRpcError>;

    async fn get_target(
        context_id: String,
        connection_id: String,
        target_id: String,
    ) -> Result<TargetDebuggerSnapshot, JsonRpcError>;

    async fn get_logs(
        context_id: String,
        connection_id: String,
        target_id: String,
    ) -> Result<TargetLogSnapshot, JsonRpcError>;

    async fn wait_target(
        context_id: String,
        connection_id: String,
        target_id: String,
        predicate: TargetWaitPredicate,
        timeout_ms: u64,
    ) -> Result<TargetDebuggerSnapshot, JsonRpcError>;

    async fn observe_target(
        context_id: String,
        connection_id: String,
        target_id: String,
        after_revision: u64,
        timeout_ms: u64,
    ) -> Result<Option<TargetDebuggerSnapshot>, JsonRpcError>;

    async fn release_target(
        context_id: String,
        connection_id: String,
        target_id: String,
    ) -> Result<TargetDebuggerSnapshot, JsonRpcError>;

    async fn detach_target(
        context_id: String,
        connection_id: String,
        target_id: String,
        expected_connection_generation: Option<u64>,
    ) -> Result<ContextSnapshot, JsonRpcError>;

    async fn resume_target(
        context_id: String,
        connection_id: String,
        target_id: String,
        pause_epoch: u64,
    ) -> Result<TargetDebuggerSnapshot, JsonRpcError>;

    async fn step_target(
        context_id: String,
        connection_id: String,
        target_id: String,
        pause_epoch: u64,
        kind: StepKind,
    ) -> Result<TargetDebuggerSnapshot, JsonRpcError>;

    async fn evaluate_target(
        context_id: String,
        connection_id: String,
        target_id: String,
        pause_epoch: Option<u64>,
        frame_index: u32,
        expression: String,
    ) -> Result<EvaluationSnapshot, JsonRpcError>;

    async fn get_scope_variables(
        context_id: String,
        connection_id: String,
        target_id: String,
        pause_epoch: u64,
        frame_index: u32,
        scope_index: u32,
    ) -> Result<Vec<VariableSnapshot>, JsonRpcError>;

    async fn get_object_properties(
        context_id: String,
        connection_id: String,
        target_id: String,
        pause_epoch: Option<u64>,
        object_id: String,
    ) -> Result<Vec<VariableSnapshot>, JsonRpcError>;

    async fn inspect_value(
        context_id: String,
        connection_id: String,
        target_id: String,
        pause_epoch: Option<u64>,
        selector: ValueSelector,
        options: ValueInspectionOptions,
    ) -> Result<ValueSnapshot, JsonRpcError>;

    async fn set_logpoint(
        context_id: String,
        connection_id: String,
        target_id: String,
        logpoint_id: String,
        source_url: String,
        line: u32,
        column: u32,
        expression: String,
    ) -> Result<TargetDebuggerSnapshot, JsonRpcError>;

    async fn set_logpoints(
        context_id: String,
        connection_id: String,
        target_id: String,
        logpoints: Vec<LogpointSpec>,
    ) -> Result<TargetDebuggerSnapshot, JsonRpcError>;
}
