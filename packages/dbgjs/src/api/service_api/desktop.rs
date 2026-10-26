use super::*;

/// Native screen and window capture on the machine that runs the service.
///
/// Unlike [`BrowserAutomationApi::capture_screenshot`], these operations do not use a debug
/// target: they capture what the service host's interactive desktop shows, including OS
/// permission prompts, native dialogs, and other applications.
#[link_rpc_interface(id = "dev.dbgjs.desktop")]
pub trait DesktopApi {
    async fn list_displays() -> Result<Vec<DisplaySnapshot>, DesktopError>;

    /// Visible top-level windows, front to back.
    async fn list_windows() -> Result<Vec<DesktopWindowSnapshot>, DesktopError>;

    /// Captures one display; `None` selects the primary display.
    async fn capture_display(display_id: Option<u32>) -> Result<ScreenshotSnapshot, DesktopError>;

    async fn capture_window(window_id: u32) -> Result<ScreenshotSnapshot, DesktopError>;
}

/// A display in the host's desktop coordinate space.
///
/// Windows reports physical pixels; macOS reports points, so its captures are
/// `scale_factor` times larger.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct DisplaySnapshot {
    pub id: u32,
    pub name: String,
    pub x: i32,
    pub y: i32,
    pub width: u32,
    pub height: u32,
    pub scale_factor: f32,
    pub is_primary: bool,
}

/// A visible top-level native window, in the same coordinate space as [`DisplaySnapshot`].
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct DesktopWindowSnapshot {
    /// Native handle: an `HWND` on Windows, a `CGWindowID` on macOS. Valid while the window lives.
    pub id: u32,
    pub pid: u32,
    pub app_name: String,
    pub title: String,
    pub x: i32,
    pub y: i32,
    pub width: u32,
    pub height: u32,
    pub is_minimized: bool,
    pub is_focused: bool,
}
