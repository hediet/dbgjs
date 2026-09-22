use super::*;

#[link_rpc_interface(id = "dev.dbgjs.browser-automation")]
pub trait BrowserAutomationApi {
    async fn click_target(target_ref: TargetRef, selector: String) -> Result<bool, JsonRpcError>;

    async fn type_target(target_ref: TargetRef, text: String) -> Result<bool, JsonRpcError>;

    async fn capture_screenshot(target_ref: TargetRef) -> Result<ScreenshotSnapshot, JsonRpcError>;
}
