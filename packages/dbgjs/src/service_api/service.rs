use super::*;

#[link_rpc_interface(id = "dev.dbgjs.cdp-debugger")]
pub trait ServiceApi {
    async fn service_info() -> Result<ServiceInfo, JsonRpcError>;

    async fn discover_vscode_process_trees() -> Result<Vec<ProcessTreeSnapshot>, JsonRpcError>;

    /// Returns a process-oriented resource projection. Runtime target discovery is performed only
    /// for the roots named in `expanded_root_process_ids`.
    async fn get_process_projection(
        context_id: String,
        expanded_root_process_ids: Vec<u32>,
    ) -> Result<Vec<ProcessTreeSnapshot>, JsonRpcError>;

    async fn shutdown() -> Result<bool, JsonRpcError>;
}
