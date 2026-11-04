use super::*;

/// A view-and-control stream of one page target. Clients receive rendered frames and may send
/// pointer and keyboard input; they cannot issue arbitrary CDP commands through this interface.
#[link_rpc_interface(id = "dev.dbgjs.screencast")]
pub trait ScreencastApi {
    /// Streams `Page.startScreencast` frames of `target_ref` until the caller cancels or ends its
    /// input stream. At most `options.window` frames (default 1) are unacknowledged: the client
    /// sends `frameAck` for each frame it has presented to make room for the next. Hidden or
    /// minimized windows may produce no frames; `visibility` reports such changes when the
    /// browser does. Not exclusive: other debugging of the target continues. Fails if the target
    /// detaches or frames are lost.
    #[input_stream(ScreencastInput)]
    #[output_stream(ScreencastMessage)]
    async fn screencast(
        target_ref: TargetRef,
        options: ScreencastOptions,
    ) -> Result<ScreencastSummary, JsonRpcError>;
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct ScreencastOptions {
    pub format: ScreencastFormat,
    /// JPEG quality in `0..=100`; ignored for PNG.
    #[serde(default)]
    pub quality: Option<u8>,
    #[serde(default)]
    pub size: Option<ScreencastSize>,
    /// How many frames may be on the way to the client before it acks one, in `1..=5`
    /// (default 1). Larger windows hide the round trip on slow links: the service acks the
    /// browser as soon as a frame can be sent and keeps at most one newer frame waiting.
    #[serde(default)]
    pub window: Option<u8>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub enum ScreencastFormat {
    Jpeg,
    Png,
}

/// Maximum frame size in device pixels. The page keeps its own viewport; frames are scaled down.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct ScreencastSize {
    pub max_width: u32,
    pub max_height: u32,
}

/// A page viewport in CSS pixels and the device pixel ratio to render it at.
#[derive(Clone, Copy, Debug, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct ScreencastViewport {
    pub width: u32,
    pub height: u32,
    pub device_scale_factor: f64,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(tag = "kind", rename_all = "camelCase")]
pub enum ScreencastMessage {
    #[serde(rename_all = "camelCase")]
    Frame {
        frame_id: u64,
        media_type: String,
        data_base64: String,
        metadata: ScreencastFrameMetadata,
    },
    #[serde(rename_all = "camelCase")]
    Visibility { visible: bool },
}

/// `Page.ScreencastFrameMetadata`. `deviceWidth` × `deviceHeight` is the frame's coordinate
/// space for mouse input, in device-independent pixels.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct ScreencastFrameMetadata {
    pub offset_top: f64,
    pub page_scale_factor: f64,
    pub device_width: f64,
    pub device_height: f64,
    pub scroll_offset_x: f64,
    pub scroll_offset_y: f64,
    #[serde(default)]
    pub timestamp: Option<f64>,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(tag = "kind", rename_all = "camelCase")]
pub enum ScreencastInput {
    /// The client finished presenting `frame_id`; the browser may send the next frame.
    #[serde(rename_all = "camelCase")]
    FrameAck { frame_id: u64 },
    /// Restarts the screencast with a new maximum frame size.
    Resize(ScreencastSize),
    /// Lays the page out at this viewport (`Emulation.setDeviceMetricsOverride`) until
    /// `resetViewport` or the end of the stream, which always removes the override.
    Viewport(ScreencastViewport),
    /// Removes the viewport override set by `viewport`, restoring the page's own layout.
    ResetViewport,
    Mouse(ScreencastMouseInput),
    Key(ScreencastKeyInput),
}

/// `Input.dispatchMouseEvent`. `x`/`y` are in the frame's coordinate space (`deviceWidth` ×
/// `deviceHeight` device-independent pixels); the service converts them to the page's CSS
/// pixels, which differ when the page is zoomed.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct ScreencastMouseInput {
    #[serde(rename = "type")]
    pub event_type: ScreencastMouseEventType,
    pub x: f64,
    pub y: f64,
    #[serde(default)]
    pub button: ScreencastMouseButton,
    /// Bit field of pressed buttons: left 1, right 2, middle 4.
    #[serde(default)]
    pub buttons: u32,
    #[serde(default)]
    pub click_count: u32,
    #[serde(default)]
    pub delta_x: f64,
    #[serde(default)]
    pub delta_y: f64,
    /// Bit field: Alt 1, Ctrl 2, Meta 4, Shift 8.
    #[serde(default)]
    pub modifiers: u32,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub enum ScreencastMouseEventType {
    MousePressed,
    MouseReleased,
    MouseMoved,
    MouseWheel,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub enum ScreencastMouseButton {
    #[default]
    None,
    Left,
    Middle,
    Right,
}

/// `Input.dispatchKeyEvent`.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct ScreencastKeyInput {
    #[serde(rename = "type")]
    pub event_type: ScreencastKeyEventType,
    pub key: String,
    pub code: String,
    #[serde(default)]
    pub text: Option<String>,
    #[serde(default)]
    pub windows_virtual_key_code: Option<u32>,
    #[serde(default)]
    pub auto_repeat: bool,
    #[serde(default)]
    pub modifiers: u32,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub enum ScreencastKeyEventType {
    KeyDown,
    KeyUp,
    RawKeyDown,
    Char,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct ScreencastSummary {
    pub frames: u64,
}
