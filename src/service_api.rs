use hubrpc::prelude::{JsonRpcError, hub_rpc_interface};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

pub const SERVICE_PROTOCOL_VERSION: u32 = 2;

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct ServiceInfo {
    pub process_id: u32,
    pub protocol_version: u32,
    pub agent_instance_id: String,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct ContextSummary {
    pub agent_instance_id: String,
    pub id: String,
    pub display_name: String,
    pub revision: u64,
    pub connection_count: u32,
    pub breakpoint_count: u32,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct ContextSnapshot {
    pub agent_instance_id: String,
    pub id: String,
    pub display_name: String,
    pub revision: u64,
    pub connections: Vec<ConnectionSnapshot>,
    pub breakpoints: Vec<BreakpointSnapshot>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct ConnectionSnapshot {
    pub id: String,
    pub endpoint: String,
    pub generation: u64,
    pub status: ConnectionStatus,
    pub targets: Vec<TargetSnapshot>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct TargetSnapshot {
    pub target_id: String,
    pub target_type: String,
    pub title: String,
    pub url: String,
    pub attached: bool,
    pub parent_id: Option<String>,
    pub opener_id: Option<String>,
    pub browser_context_id: Option<String>,
    pub subtype: Option<String>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(tag = "kind", rename_all = "camelCase")]
pub enum ConnectionStatus {
    Disconnected,
    Connecting,
    Disconnecting,
    Connected {
        product: String,
        #[serde(rename = "protocolVersion")]
        #[schemars(rename = "protocolVersion")]
        protocol_version: String,
    },
    Failed {
        message: String,
    },
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct BreakpointSnapshot {
    pub id: String,
    pub source_path: String,
    pub line: u32,
    pub column: u32,
    pub status: BreakpointStatus,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub enum BreakpointStatus {
    Unconfirmed,
}

#[hub_rpc_interface(id = "dev.hediet.cdp-debugger")]
pub trait DebuggerServiceApi {
    async fn service_info() -> Result<ServiceInfo, JsonRpcError>;

    async fn list_contexts() -> Result<Vec<ContextSummary>, JsonRpcError>;

    async fn put_context(
        context_id: String,
        display_name: Option<String>,
    ) -> Result<ContextSnapshot, JsonRpcError>;

    async fn get_context(context_id: String) -> Result<ContextSnapshot, JsonRpcError>;

    async fn put_connection(
        context_id: String,
        connection_id: String,
        endpoint: String,
    ) -> Result<ContextSnapshot, JsonRpcError>;

    async fn connect_connection(
        context_id: String,
        connection_id: String,
    ) -> Result<ContextSnapshot, JsonRpcError>;

    async fn disconnect_connection(
        context_id: String,
        connection_id: String,
    ) -> Result<ContextSnapshot, JsonRpcError>;

    async fn put_breakpoint(
        context_id: String,
        breakpoint_id: String,
        source_path: String,
        line: u32,
        column: u32,
    ) -> Result<ContextSnapshot, JsonRpcError>;

    async fn shutdown() -> Result<bool, JsonRpcError>;
}
