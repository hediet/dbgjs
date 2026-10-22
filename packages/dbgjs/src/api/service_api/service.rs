use super::*;

#[link_rpc_interface(id = "dev.dbgjs.cdp-debugger")]
pub trait ServiceApi {
    async fn service_info() -> Result<ServiceInfo, JsonRpcError>;

    async fn discover_vscode_process_trees() -> Result<Vec<ProcessTreeSnapshot>, JsonRpcError>;

    /// Discovers process trees on the service's machine: trees of `root_kind`, or of every
    /// recognized root kind when it is omitted. `include_stats` requires a `root_kind`;
    /// `include_targets` also queries the runtime targets of every root.
    async fn discover_process_trees(
        root_kind: Option<ProcessRootKind>,
        include_stats: bool,
        include_targets: bool,
    ) -> Result<Vec<ProcessTreeSnapshot>, JsonRpcError>;

    /// Returns a process-oriented resource projection. Runtime target discovery is performed only
    /// for the roots named in `expanded_root_process_ids`.
    async fn get_process_projection(
        context_id: String,
        expanded_root_process_ids: Vec<u32>,
    ) -> Result<Vec<ProcessTreeSnapshot>, JsonRpcError>;

    async fn shutdown() -> Result<bool, JsonRpcError>;
}
