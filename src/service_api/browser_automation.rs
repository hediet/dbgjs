use super::*;

#[link_rpc_interface(id = "dev.dbgjs.browser-automation")]
pub trait BrowserAutomationApi {
    async fn click_target(
        context_id: String,
        connection_id: String,
        target_id: String,
        selector: String,
    ) -> Result<bool, JsonRpcError>;

    async fn type_target(
        context_id: String,
        connection_id: String,
        target_id: String,
        text: String,
    ) -> Result<bool, JsonRpcError>;

    async fn capture_screenshot(
        context_id: String,
        connection_id: String,
        target_id: String,
    ) -> Result<ScreenshotSnapshot, JsonRpcError>;
}
