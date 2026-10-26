use super::*;
use crate::api::service_api::{
    DesktopApi, DesktopAttachResult, DesktopClientMessage, DesktopInfo, DesktopOptions,
    DesktopProtocol, DesktopServerMessage,
};
use linkrpc::prelude::StreamReceiver;

#[async_trait::async_trait]
impl DesktopApi for DebuggerService {
    async fn info(&self, ctx: &CallCtx, target_ref: TargetRef) -> Result<DesktopInfo, JsonRpcError> {
        Ok(DesktopInfo {
            title: self.target_title(ctx, &target_ref).await?,
            available: true,
            protocol: DesktopProtocol::Rfb,
        })
    }

    async fn attach(
        &self,
        ctx: &CallCtx,
        target_ref: TargetRef,
        options: DesktopOptions,
        mut input: StreamReceiver<DesktopClientMessage>,
        output: StreamSender<DesktopServerMessage>,
    ) -> Result<DesktopAttachResult, JsonRpcError> {
        let _activity = self.connection_activity(&target_ref.connection).await;
        let title = self.target_title(ctx, &target_ref).await?;
        let (debugger, owned) = self.stream_target(&target_ref, "desktop").await?;
        let result = crate::desktop::serve(debugger.clone(), title, options, &mut input, output, ctx.cancelled()).await;
        self.release_stream_target(&target_ref, &debugger, owned).await;
        result
    }
}

impl DebuggerService {
    async fn target_title(&self, ctx: &CallCtx, target_ref: &TargetRef) -> Result<String, JsonRpcError> {
        let context = ContextApi::get_context(self, ctx, target_ref.connection.context_id.clone()).await?;
        context
            .target_forest
            .iter()
            .find(|node| node.connection_id == target_ref.connection.connection_id && node.target.target_id == target_ref.target_id)
            .map(|node| node.target.title.clone())
            .ok_or_else(|| JsonRpcError::new(error_codes::INVALID_PARAMS, format!("target '{}' was not found", target_ref.target_id)))
    }
}
