use super::*;
use crate::api::service_api::{DesktopApi, DesktopError, DesktopWindowSnapshot, DisplaySnapshot};
use crate::service::desktop_capture;

#[async_trait::async_trait]
impl DesktopApi for DebuggerService {
    async fn list_displays(&self, _ctx: &CallCtx) -> Result<Vec<DisplaySnapshot>, DesktopError> {
        run_blocking(desktop_capture::list_displays).await
    }

    async fn list_windows(
        &self,
        _ctx: &CallCtx,
    ) -> Result<Vec<DesktopWindowSnapshot>, DesktopError> {
        run_blocking(desktop_capture::list_windows).await
    }

    async fn capture_display(
        &self,
        _ctx: &CallCtx,
        display_id: Option<u32>,
    ) -> Result<ScreenshotSnapshot, DesktopError> {
        run_blocking(move || desktop_capture::capture_display(display_id)).await
    }

    async fn capture_window(
        &self,
        _ctx: &CallCtx,
        window_id: u32,
    ) -> Result<ScreenshotSnapshot, DesktopError> {
        run_blocking(move || desktop_capture::capture_window(window_id)).await
    }
}

async fn run_blocking<T: Send + 'static>(
    operation: impl FnOnce() -> Result<T, DesktopError> + Send + 'static,
) -> Result<T, DesktopError> {
    tokio::task::spawn_blocking(operation)
        .await
        .map_err(|error| DesktopError::CaptureFailed {
            reason: format!("desktop capture task failed: {error}"),
        })?
}
