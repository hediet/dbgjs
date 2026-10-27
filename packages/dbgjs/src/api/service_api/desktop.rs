use super::*;

/// A page target served as a remote desktop: the RFB (VNC) protocol, framed like
/// `linkrpc.desktop.hub.v1`, so VNC clients such as noVNC can view and control the page.
#[link_rpc_interface(id = "dev.dbgjs.desktop")]
pub trait DesktopApi {
    /// The target's title and that its desktop speaks RFB.
    async fn info(target_ref: TargetRef) -> Result<DesktopInfo, JsonRpcError>;

    /// Serves `target_ref` as an RFB 3.8 desktop (no RFB authentication; Hub consent gates
    /// the call) until the caller cancels or ends its input stream. The server sends `ready`
    /// before data. Data messages carry canonical base64 of at most 16384 bytes. Each direction
    /// starts with 262144 bytes of credit; sending data consumes credit, and the receiver returns
    /// credit only after consuming the bytes. The client sends its viewport (CSS pixels and device
    /// pixel ratio) over the stream; how the page follows it depends on `options.layout`.
    /// Not exclusive: other debugging of the target continues.
    #[input_stream(DesktopClientMessage)]
    #[output_stream(DesktopServerMessage)]
    async fn attach(
        target_ref: TargetRef,
        options: DesktopOptions,
    ) -> Result<DesktopAttachResult, JsonRpcError>;
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct DesktopInfo {
    pub title: String,
    pub available: bool,
    pub protocol: DesktopProtocol,
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
