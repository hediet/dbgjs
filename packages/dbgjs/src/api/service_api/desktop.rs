use super::*;

/// A page target served as a remote desktop: the RFB (VNC) protocol, framed like
/// `linkrpc.desktop.hub.v1`, so VNC clients such as noVNC can view and control the page.
#[link_rpc_interface(id = "dev.dbgjs.desktop")]
pub trait DesktopApi {
    /// The target's title, the target a reference resolved to, and that its desktop speaks RFB.
    async fn info(target: MachineTargetRef) -> Result<DesktopInfo, JsonRpcError>;

    /// Serves `target` as an RFB 3.8 desktop (no RFB authentication; Hub consent gates
    /// the call) until the caller cancels or ends its input stream. The server sends `ready`
    /// before data. Data messages carry canonical base64 of at most 16384 bytes. Each direction
    /// starts with 262144 bytes of credit; sending data consumes credit, and the receiver returns
    /// credit only after consuming the bytes. The client sends its viewport (CSS pixels and device
    /// pixel ratio) over the stream; how the page follows it depends on `options.layout`.
    /// `visibility` reports when the browser hides or shows the page (e.g. a minimized window);
    /// a hidden page sends no new frames and handles input only after seconds.
    /// Not exclusive: other debugging of the target continues.
    #[input_stream(DesktopClientMessage)]
    #[output_stream(DesktopServerMessage)]
    async fn attach(
        target: MachineTargetRef,
        options: DesktopOptions,
    ) -> Result<DesktopAttachResult, JsonRpcError>;
}

/// A page target on this machine, independent of contexts. The service connects to process trees
/// itself, in its managed `dbgjs.machine` context, and keeps those connections for later calls
/// and discovery.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(untagged)]
pub enum MachineTargetRef {
    Target(MachineTarget),
    Window(MachineWindowRef),
}

/// A process tree's root process and the target's ID within it, as `discover_process_trees`
/// reports them (e.g. a VS Code window `renderer-1`, an Electron page, or a browser tab). Written
/// as `<rootPid>/<targetId>`.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct MachineTarget {
    pub root_pid: u32,
    pub target_id: String,
}

/// The first page titled `window` (ignoring case and surrounding whitespace) in a VS Code,
/// Electron, or browser process tree. Unlike a target's IDs, it survives restarts of the app.
/// Pages of process trees the service is already connected to are searched first, so a page that
/// was viewed before resolves without discovering processes.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct MachineWindowRef {
    pub window: String,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct DesktopInfo {
    pub title: String,
    pub available: bool,
    pub protocol: DesktopProtocol,
    /// The target the reference resolved to.
    pub target: MachineTarget,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub enum DesktopProtocol {
    Rfb,
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct DesktopOptions {
    #[serde(default)]
    pub layout: DesktopLayout,
}

/// How the page follows the client's viewport.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub enum DesktopLayout {
    /// The page keeps its own layout; the desktop is the page scaled down to fit the viewport.
    #[default]
    Fit,
    /// The page is laid out at the viewport size and pixel ratio until the desktop closes.
    Resize,
}

/// CSS viewport dimensions and the device pixel ratio to apply to them.
#[derive(Clone, Copy, Debug, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct DesktopViewport {
    pub width: u32,
    pub height: u32,
    pub device_pixel_ratio: f64,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(tag = "t", rename_all = "camelCase")]
pub enum DesktopClientMessage {
    Data { data: String },
    Credit { bytes: u32 },
    Viewport { viewport: DesktopViewport },
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(tag = "t", rename_all = "camelCase")]
pub enum DesktopServerMessage {
    Ready,
    Data { data: String },
    Credit { bytes: u32 },
    Visibility { visible: bool },
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct DesktopAttachResult {
    pub reason: DesktopCloseReason,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub enum DesktopCloseReason {
    Closed,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn machine_target_refs_name_a_target_or_a_window() {
        let parse = |json: serde_json::Value| serde_json::from_value::<MachineTargetRef>(json);
        assert_eq!(
            parse(serde_json::json!({ "rootPid": 42, "targetId": "renderer-1" })).unwrap(),
            MachineTargetRef::Target(MachineTarget { root_pid: 42, target_id: "renderer-1".into() }),
        );
        assert_eq!(
            parse(serde_json::json!({ "window": "Agents" })).unwrap(),
            MachineTargetRef::Window(MachineWindowRef { window: "Agents".into() }),
        );
        assert!(parse(serde_json::json!({ "window": "Agents", "rootPid": 42 })).is_err());
        assert!(parse(serde_json::json!({ "rootPid": 42 })).is_err());
    }
}
