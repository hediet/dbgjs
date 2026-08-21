use hubrpc::prelude::{JsonRpcError, hub_rpc_interface};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

pub const SERVICE_PROTOCOL_VERSION: u32 = 4;

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
    pub configuration: ConnectionConfiguration,
    pub generation: u64,
    pub status: ConnectionStatus,
    pub targets: Vec<TargetSnapshot>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(tag = "kind", rename_all = "camelCase")]
pub enum ConnectionConfiguration {
    DirectCdp {
        endpoint: String,
    },
    Playwright {
        url: String,
        channel: PlaywrightChannel,
        headless: bool,
    },
}

impl From<&str> for ConnectionConfiguration {
    fn from(endpoint: &str) -> Self {
        Self::DirectCdp {
            endpoint: endpoint.to_owned(),
        }
    }
}

impl From<String> for ConnectionConfiguration {
    fn from(endpoint: String) -> Self {
        Self::DirectCdp { endpoint }
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub enum PlaywrightChannel {
    Bundled,
    Chrome,
    ChromeBeta,
    ChromeDev,
    ChromeCanary,
    Msedge,
    MsedgeBeta,
    MsedgeDev,
    MsedgeCanary,
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

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct TargetDebuggerSnapshot {
    pub context_id: String,
    pub connection_id: String,
    pub target_id: String,
    pub connection_generation: u64,
    pub revision: u64,
    pub phase: TargetDebuggerPhase,
    pub scripts: Vec<TargetScriptSnapshot>,
    pub breakpoints: Vec<TargetBreakpointSnapshot>,
    pub pause: Option<PauseSnapshot>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(tag = "kind", rename_all = "camelCase")]
pub enum TargetDebuggerPhase {
    Running,
    Paused { epoch: u64 },
    Resuming { epoch: u64 },
    Failed { message: String },
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct TargetBreakpointSnapshot {
    pub id: String,
    pub source_url: String,
    pub line: u32,
    pub column: u32,
    pub status: TargetBreakpointStatus,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(tag = "kind", rename_all = "camelCase")]
pub enum TargetBreakpointStatus {
    Pending,
    Installed { binding_count: u32 },
    Failed { message: String },
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct TargetScriptSnapshot {
    pub url: String,
    pub source_map_url: Option<String>,
    pub status: TargetScriptStatus,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(tag = "kind", rename_all = "camelCase")]
pub enum TargetScriptStatus {
    Unresolved,
    Pending,
    Resolved { authored_sources: Vec<String> },
    Failed { message: String },
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct PauseSnapshot {
    pub epoch: u64,
    pub reason: String,
    pub frames: Vec<FrameSnapshot>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct FrameSnapshot {
    pub index: u32,
    pub function_name: String,
    pub raw: SourceLocation,
    pub projected: FrameProjectionSnapshot,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct SourceLocation {
    pub source_url: String,
    pub line: u32,
    pub column: u32,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(tag = "kind", rename_all = "camelCase")]
pub enum FrameProjectionSnapshot {
    Raw,
    Pending,
    Resolved { location: SourceLocation },
    Failed { message: String },
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(tag = "kind", rename_all = "camelCase")]
pub enum TargetWaitPredicate {
    Running,
    BreakpointInstalled { breakpoint_id: String },
    Paused { after_epoch: u64 },
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
        configuration: ConnectionConfiguration,
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

    async fn attach_target(
        context_id: String,
        connection_id: String,
        target_id: String,
    ) -> Result<TargetDebuggerSnapshot, JsonRpcError>;

    async fn get_target(
        context_id: String,
        connection_id: String,
        target_id: String,
    ) -> Result<TargetDebuggerSnapshot, JsonRpcError>;

    async fn wait_target(
        context_id: String,
        connection_id: String,
        target_id: String,
        predicate: TargetWaitPredicate,
        timeout_ms: u64,
    ) -> Result<TargetDebuggerSnapshot, JsonRpcError>;

    async fn resume_target(
        context_id: String,
        connection_id: String,
        target_id: String,
        pause_epoch: u64,
    ) -> Result<TargetDebuggerSnapshot, JsonRpcError>;

    async fn shutdown() -> Result<bool, JsonRpcError>;
}
