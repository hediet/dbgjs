use super::*;

#[async_trait::async_trait]
impl CdpAccessApi for DebuggerService {
    async fn raw_cdp_request(
        &self,
        _ctx: &CallCtx,
        target_ref: TargetRef,
        method: String,
        params: serde_json::Value,
        validate: bool,
    ) -> Result<serde_json::Value, JsonRpcError> {
        let TargetRef {
            connection:
                ConnectionRef {
                    context_id,
                    connection_id,
                },
            target_id,
        } = target_ref;
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
        let detached_id = (method == "Target.detachFromTarget")
            .then(|| params.get("sessionId").and_then(serde_json::Value::as_str).map(str::to_owned))
            .flatten();
        let raw_events = (method == "Target.attachToTarget")
            .then(|| debugger.subscribe_raw_events());
        let result = debugger.raw_cdp_request(method.clone(), params).await;
        let state = self.state.lock().await;
        let is_current = state.target_debuggers
            .get(&(identity.context_id.clone(), identity.connection_id.clone(), identity.target_id.clone()))
            .is_some_and(|current| current.same_instance(&debugger)
                && current.snapshot().connection_generation == identity.connection_generation)
            && state.runtimes.get(&(context_id.clone(), connection_id.clone()))
                .is_some_and(|runtime| runtime.generation() == identity.connection_generation);
        let runtime = state.runtimes.get(&(context_id, connection_id)).cloned();
        drop(state);
        let attached_id = if method == "Target.attachToTarget" {
            result.as_ref().ok().and_then(|value| value.get("sessionId")).and_then(serde_json::Value::as_str)
        } else {
            None
        };
        if !is_current {
            if let Some(session_id) = attached_id {
                detach_unregistered_raw_session(&debugger, session_id).await;
            }
            return Err(invalid_state(
                "target connection changed while the CDP request was in flight",
            ));
        }
        let result = result?;
        if method == "Target.attachToTarget" {
            let session_id = result.get("sessionId").and_then(serde_json::Value::as_str)
                .ok_or_else(|| invalid_state("Target.attachToTarget did not return a sessionId"))?;
            let Some(runtime) = runtime else {
                detach_unregistered_raw_session(&debugger, session_id).await;
                return Err(invalid_state("connection is not connected"));
            };
            if let Err(error) = runtime.register_raw_session(
                &identity.target_id, session_id, raw_events.unwrap(),
            ) {
                if !runtime.has_raw_session(&identity.target_id, session_id) {
                    detach_unregistered_raw_session(&debugger, session_id).await;
                }
                return Err(invalid_state(&error));
            }
        } else if let Some(session_id) = detached_id {
            if let Some(runtime) = runtime {
                runtime.retire_raw_session(&identity.target_id, &session_id);
            }
        }
        Ok(result)
    }

    async fn raw_cdp_session_request(
        &self,
        _ctx: &CallCtx,
        target_ref: TargetRef,
        session_id: String,
        method: String,
        params: serde_json::Value,
        validate: bool,
    ) -> Result<serde_json::Value, JsonRpcError> {
        let TargetRef {
            connection:
                ConnectionRef {
                    context_id,
                    connection_id,
                },
            target_id,
        } = target_ref;
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
        let result = runtime.raw_session_request(&identity.target_id, &session_id, &method, params).await;
        let is_current = self
            .state
            .lock()
            .await
            .target_debuggers
            .get(&(identity.context_id.clone(), identity.connection_id.clone(), identity.target_id.clone()))
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

async fn detach_unregistered_raw_session(debugger: &TargetDebuggerHandle, session_id: &str) {
    let _ = tokio::time::timeout(
        Duration::from_secs(2),
        debugger.raw_cdp_request(
            "Target.detachFromTarget".to_owned(),
            serde_json::json!({"sessionId": session_id}),
        ),
    ).await;
}
