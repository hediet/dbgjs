use super::*;

#[async_trait::async_trait]
impl BrowserAutomationApi for DebuggerService {
    async fn click_target(
        &self,
        _ctx: &CallCtx,
        context_id: String,
        connection_id: String,
        target_id: String,
        selector: String,
    ) -> Result<bool, JsonRpcError> {
        self.target_debugger(&context_id, &connection_id, &target_id)
            .await?
            .click(selector)
            .await
            .map_err(target_debugger_rpc_error)?;
        Ok(true)
    }

    async fn type_target(
        &self,
        _ctx: &CallCtx,
        context_id: String,
        connection_id: String,
        target_id: String,
        text: String,
    ) -> Result<bool, JsonRpcError> {
        self.target_debugger(&context_id, &connection_id, &target_id)
            .await?
            .type_text(text)
            .await
            .map_err(target_debugger_rpc_error)?;
        Ok(true)
    }

    async fn capture_screenshot(
        &self,
        _ctx: &CallCtx,
        context_id: String,
        connection_id: String,
        target_id: String,
    ) -> Result<ScreenshotSnapshot, JsonRpcError> {
        let selected = self
            .target_debugger(&context_id, &connection_id, &target_id)
            .await?;
        let owner = selected.snapshot();
        let target_id = owner.target_id;
        let (target_type, parent_id) = {
            let state = self.state.lock().await;
            let connection = state
                .contexts
                .get(&context_id)
                .ok_or_else(|| not_found("context", &context_id))?
                .connections
                .get(&connection_id)
                .ok_or_else(|| not_found("connection", &connection_id))?;
            if connection.generation != owner.connection_generation {
                return Err(invalid_state(
                    "connection changed before the screenshot could be captured",
                ));
            }
            let target = context_connection_target(
                &state,
                &context_id,
                &connection_id,
                connection.generation,
                &target_id,
            )
            .ok_or_else(|| not_found("target", &target_id))?;
            (target.target.target_type, target.target.parent_id)
        };
        if target_type != "iframe" {
            return selected
                .capture_screenshot()
                .await
                .map_err(target_debugger_rpc_error);
        }
        let parent_id = parent_id.ok_or_else(|| {
            invalid_state("iframe screenshot requires a discovered embedding page")
        })?;
        let frame_id = target_id
            .rsplit_once("/target/")
            .map(|(_, frame_id)| frame_id)
            .ok_or_else(|| invalid_state("iframe target has no native frame identifier"))?;
        let owner_id = format!(
            "screenshot-{}",
            random_instance_id().map_err(|error| internal_error(error.to_string()))?
        );
        let service = self.clone();
        let frame_id = frame_id.to_owned();
        tokio::spawn(async move {
            let (parent, created) = service
                .relay_ensure_attached(&owner_id, &context_id, &connection_id, &parent_id)
                .await?;
            let result = if parent.snapshot().connection_generation != owner.connection_generation {
                Err(invalid_state(
                    "connection changed before the screenshot could be captured",
                ))
            } else {
                capture_embedded_frame_screenshot(&parent, &frame_id).await
            };
            if created {
                service
                    .relay_release_attachment(
                        &owner_id,
                        &context_id,
                        &connection_id,
                        &parent_id,
                        &parent,
                    )
                    .await;
            }
            result
        })
        .await
        .map_err(|error| internal_error(format!("iframe screenshot task failed: {error}")))?
    }
}
