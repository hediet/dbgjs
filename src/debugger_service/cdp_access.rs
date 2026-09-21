use super::*;

#[async_trait::async_trait]
impl CdpAccessApi for DebuggerService {
    async fn raw_cdp_request(
        &self,
        _ctx: &CallCtx,
        context_id: String,
        connection_id: String,
        target_id: String,
        method: String,
        params: serde_json::Value,
        validate: bool,
    ) -> Result<serde_json::Value, JsonRpcError> {
        if validate {
            validate_raw_cdp_params(&method, &params).map_err(|message| {
                invalid_params(&format!(
                    "invalid params for CDP method '{method}': {message}"
                ))
            })?;
        }

        let debugger = self
            .target_debugger(&context_id, &connection_id, &target_id)
            .await?;
        let identity = debugger.snapshot();
        let result = debugger.raw_cdp_request(method, params).await;
        let is_current = self
            .state
            .lock()
            .await
            .target_debuggers
            .get(&(
                identity.context_id.clone(),
                identity.connection_id.clone(),
                identity.target_id.clone(),
            ))
            .is_some_and(|current| {
                current.same_instance(&debugger)
                    && current.snapshot().connection_generation == identity.connection_generation
            });
        if !is_current {
            return Err(invalid_state(
                "target connection changed while the CDP request was in flight",
            ));
        }
        result
    }

    async fn raw_cdp_session_request(
        &self,
        _ctx: &CallCtx,
        context_id: String,
        connection_id: String,
        target_id: String,
        session_id: String,
        method: String,
        params: serde_json::Value,
        validate: bool,
    ) -> Result<serde_json::Value, JsonRpcError> {
        if validate {
            validate_raw_cdp_params(&method, &params).map_err(|message| {
                invalid_params(&format!(
                    "invalid params for CDP method '{method}': {message}"
                ))
            })?;
        }
        let debugger = self
            .target_debugger(&context_id, &connection_id, &target_id)
            .await?;
        let identity = debugger.snapshot();
        let runtime = self
            .state
            .lock()
            .await
            .runtimes
            .get(&(context_id.clone(), connection_id.clone()))
            .cloned()
            .ok_or_else(|| invalid_state("connection is not connected"))?;
        let session = runtime
            .open_session(SessionKey {
                connection_generation: identity.connection_generation,
                session_id,
            })
            .map_err(|error| invalid_state(&error.to_string()))?;
        let result = session.raw_request(&method, params).await;
        let is_current = self
            .state
            .lock()
            .await
            .target_debuggers
            .get(&(context_id, connection_id, target_id))
            .is_some_and(|current| {
                current.same_instance(&debugger)
                    && current.snapshot().connection_generation == identity.connection_generation
            });
        if !is_current {
            return Err(invalid_state(
                "target connection changed while the CDP request was in flight",
            ));
        }
        result
    }
}
