use super::*;

#[link_rpc_interface(id = "dev.dbgjs.context")]
pub trait ContextApi {
    async fn list_contexts(cwd: Option<String>) -> Result<Vec<ContextSummary>, JsonRpcError>;

    async fn put_context(
        context_id: String,
        kind: ContextKind,
        display_name: Option<String>,
    ) -> Result<ContextSnapshot, JsonRpcError>;

    async fn get_context(context_id: String) -> Result<ContextSnapshot, JsonRpcError>;

    async fn get_resource_graph(context_id: String) -> Result<ResourceGraphSnapshot, JsonRpcError>;

    async fn observe_context(
        context_id: String,
        cursor: ObservationCursor,
        timeout_ms: u64,
    ) -> Result<ObservationResult, JsonRpcError>;

    async fn delete_context(
        context_id: String,
        options: MutationOptions,
    ) -> Result<bool, JsonRpcError>;

    async fn put_connection(
        connection_ref: ConnectionRef,
        configuration: ConnectionConfiguration,
    ) -> Result<ContextSnapshot, JsonRpcError>;

    async fn connect_connection(
        connection_ref: ConnectionRef,
    ) -> Result<ContextSnapshot, JsonRpcError>;

    async fn disconnect_connection(
        connection_ref: ConnectionRef,
    ) -> Result<ContextSnapshot, JsonRpcError>;

    async fn set_pause_future_children(
        connection_ref: ConnectionRef,
        enabled: bool,
    ) -> Result<bool, JsonRpcError>;

    async fn delete_connection(
        connection_ref: ConnectionRef,
        options: MutationOptions,
    ) -> Result<ContextSnapshot, JsonRpcError>;

    async fn put_breakpoint(
        context_id: String,
        breakpoint_id: String,
        source_path: String,
        line: u32,
        column: u32,
    ) -> Result<ContextSnapshot, JsonRpcError>;

    async fn put_breakpoint_spec(
        context_id: String,
        breakpoint_id: String,
        specification: BreakpointSpec,
        options: MutationOptions,
    ) -> Result<ContextSnapshot, JsonRpcError>;

    async fn delete_breakpoint(
        context_id: String,
        breakpoint_id: String,
        options: MutationOptions,
    ) -> Result<ContextSnapshot, JsonRpcError>;
}
