use super::*;

#[link_rpc_interface(id = "dev.dbgjs.target-debugger")]
pub trait TargetDebuggerApi {
    async fn resolve_target(
        context_id: String,
        selector: String,
    ) -> Result<CanonicalTargetSnapshot, JsonRpcError>;

    async fn attach_target(
        target_ref: TargetRef,
        options: TargetAttachOptions,
    ) -> Result<TargetAttachmentResult, JsonRpcError>;

    async fn get_target(target_ref: TargetRef) -> Result<TargetDebuggerSnapshot, JsonRpcError>;

    async fn get_logs(target_ref: TargetRef) -> Result<TargetLogSnapshot, JsonRpcError>;

    async fn wait_target(
        target_ref: TargetRef,
        predicate: TargetWaitPredicate,
        timeout_ms: u64,
    ) -> Result<TargetDebuggerSnapshot, JsonRpcError>;

    async fn observe_target(
        target_ref: TargetRef,
        after_revision: u64,
        timeout_ms: u64,
    ) -> Result<Option<TargetDebuggerSnapshot>, JsonRpcError>;

    async fn release_target(target_ref: TargetRef) -> Result<TargetDebuggerSnapshot, JsonRpcError>;

    async fn detach_target(
        target_ref: TargetRef,
        expected_connection_generation: Option<u64>,
    ) -> Result<ContextSnapshot, JsonRpcError>;

    async fn resume_target(
        target_ref: TargetRef,
        pause_epoch: u64,
    ) -> Result<TargetDebuggerSnapshot, JsonRpcError>;

    async fn step_target(
        target_ref: TargetRef,
        pause_epoch: u64,
        kind: StepKind,
    ) -> Result<TargetDebuggerSnapshot, JsonRpcError>;

    async fn evaluate_target(
        target_ref: TargetRef,
        pause_epoch: Option<u64>,
        frame_index: u32,
        expression: String,
    ) -> Result<EvaluationSnapshot, JsonRpcError>;

    async fn get_scope_variables(
        target_ref: TargetRef,
        pause_epoch: u64,
        frame_index: u32,
        scope_index: u32,
    ) -> Result<Vec<VariableSnapshot>, JsonRpcError>;

    async fn get_object_properties(
        target_ref: TargetRef,
        pause_epoch: Option<u64>,
        object_id: String,
    ) -> Result<Vec<VariableSnapshot>, JsonRpcError>;

    async fn inspect_value(
        target_ref: TargetRef,
        pause_epoch: Option<u64>,
        selector: ValueSelector,
        options: ValueInspectionOptions,
    ) -> Result<ValueSnapshot, JsonRpcError>;

    async fn set_logpoint(
        target_ref: TargetRef,
        logpoint_id: String,
        source_url: String,
        line: u32,
        column: u32,
        expression: String,
    ) -> Result<TargetDebuggerSnapshot, JsonRpcError>;

    async fn set_logpoints(
        target_ref: TargetRef,
        logpoints: Vec<LogpointSpec>,
    ) -> Result<TargetDebuggerSnapshot, JsonRpcError>;
}
