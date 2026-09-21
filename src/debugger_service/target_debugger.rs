use super::*;

#[async_trait::async_trait]
impl TargetDebuggerApi for DebuggerService {
    async fn resolve_target(
        &self,
        _ctx: &CallCtx,
        context_id: String,
        selector: String,
    ) -> Result<CanonicalTargetSnapshot, JsonRpcError> {
        let state = self.state.lock().await;
        Self::resolve_canonical_target(&state, &context_id, &selector)
    }

    async fn attach_target(
        &self,
        ctx: &CallCtx,
        context_id: String,
        connection_id: String,
        target_id: String,
        options: TargetAttachOptions,
    ) -> Result<TargetAttachmentResult, JsonRpcError> {
        let _relay_lifecycle_guard = self.relay_lifecycle_lock.lock().await;
        ensure_context_not_relayed(&*self.state.lock().await, &context_id)?;
        self.attach_target_internal(ctx, context_id, connection_id, target_id, options)
            .await
    }

    async fn get_target(
        &self,
        _ctx: &CallCtx,
        context_id: String,
        connection_id: String,
        target_id: String,
    ) -> Result<TargetDebuggerSnapshot, JsonRpcError> {
        Ok(self
            .target_debugger(&context_id, &connection_id, &target_id)
            .await?
            .snapshot())
    }

    async fn get_logs(
        &self,
        _ctx: &CallCtx,
        context_id: String,
        connection_id: String,
        target_id: String,
    ) -> Result<crate::service_api::TargetLogSnapshot, JsonRpcError> {
        use crate::service_api::{LogCaptureSnapshot, LogCaptureStatus, TargetLogSnapshot};
        let state = self.state.lock().await;
        ensure_context_not_relayed(&state, &context_id)?;
        let target_id =
            Self::resolve_target_id_in_state(&state, &context_id, &connection_id, &target_id)?;
        let context = state
            .contexts
            .get(&context_id)
            .ok_or_else(|| not_found("context", &context_id))?;
        let connection = context
            .connections
            .get(&connection_id)
            .ok_or_else(|| not_found("connection", &connection_id))?;
        if !context_connection_has_target(
            &state,
            &context_id,
            &connection_id,
            connection.generation,
            &target_id,
        ) {
            return Err(not_found("target", &target_id));
        }
        let snapshot = state
            .target_debuggers
            .get(&(context_id.clone(), connection_id.clone(), target_id.clone()))
            .map(TargetDebuggerHandle::snapshot);
        Ok(TargetLogSnapshot {
            context_id,
            connection_id,
            target_id,
            connection_generation: connection.generation,
            capture: snapshot.as_ref().map_or_else(
                || LogCaptureSnapshot {
                    status: LogCaptureStatus::Inactive,
                    ..Default::default()
                },
                |snapshot| snapshot.log_capture.clone(),
            ),
            messages: snapshot.map_or_else(Vec::new, |snapshot| snapshot.logs),
        })
    }

    async fn wait_target(
        &self,
        _ctx: &CallCtx,
        context_id: String,
        connection_id: String,
        target_id: String,
        predicate: TargetWaitPredicate,
        timeout_ms: u64,
    ) -> Result<TargetDebuggerSnapshot, JsonRpcError> {
        self.target_debugger(&context_id, &connection_id, &target_id)
            .await?
            .wait(predicate, Duration::from_millis(timeout_ms))
            .await
            .map_err(target_debugger_rpc_error)
    }

    async fn observe_target(
        &self,
        _ctx: &CallCtx,
        context_id: String,
        connection_id: String,
        target_id: String,
        after_revision: u64,
        timeout_ms: u64,
    ) -> Result<Option<TargetDebuggerSnapshot>, JsonRpcError> {
        match self
            .target_debugger(&context_id, &connection_id, &target_id)
            .await?
            .wait(
                TargetWaitPredicate::Changed { after_revision },
                Duration::from_millis(timeout_ms),
            )
            .await
        {
            Ok(snapshot) => Ok(Some(snapshot)),
            Err(TargetDebuggerError::WaitTimedOut) => Ok(None),
            Err(error) => Err(target_debugger_rpc_error(error)),
        }
    }

    async fn release_target(
        &self,
        _ctx: &CallCtx,
        context_id: String,
        connection_id: String,
        target_id: String,
    ) -> Result<TargetDebuggerSnapshot, JsonRpcError> {
        self.target_debugger(&context_id, &connection_id, &target_id)
            .await?
            .release_if_waiting()
            .await
            .map_err(target_debugger_rpc_error)
    }

    async fn detach_target(
        &self,
        ctx: &CallCtx,
        context_id: String,
        connection_id: String,
        target_id: String,
        expected_connection_generation: Option<u64>,
    ) -> Result<ContextSnapshot, JsonRpcError> {
        let _relay_lifecycle_guard = self.relay_lifecycle_lock.lock().await;
        ensure_context_not_relayed(&*self.state.lock().await, &context_id)?;
        let _attachment_guard = self.attachment_lock.lock().await;
        let (key, debugger, runtime, attachment, attempt) = {
            let state = self.state.lock().await;
            let target_id =
                Self::resolve_target_id_in_state(&state, &context_id, &connection_id, &target_id)?;
            let key = (context_id.clone(), connection_id.clone(), target_id.clone());
            let context = state
                .contexts
                .get(&context_id)
                .ok_or_else(|| not_found("context", &context_id))?;
            let connection = context
                .connections
                .get(&connection_id)
                .ok_or_else(|| not_found("connection", &connection_id))?;
            if expected_connection_generation
                .is_some_and(|expected| expected != connection.generation)
            {
                return Err(invalid_state(
                    "connection changed before the target could be detached",
                ));
            }
            let debugger = state
                .target_debuggers
                .get(&key)
                .cloned()
                .ok_or_else(|| not_found("attached target", &target_id))?;
            let runtime = state
                .runtimes
                .get(&(context_id.clone(), connection_id.clone()))
                .cloned()
                .ok_or_else(|| invalid_state("connection is not connected"))?;
            (
                key.clone(),
                debugger,
                runtime,
                state.debug_attachments.get(&key).cloned(),
                ConnectionAttempt {
                    configuration_version: connection.configuration_version,
                    generation: connection.generation,
                },
            )
        };

        if runtime.is_direct_debugger() {
            return self
                .disconnect_connection(ctx, context_id, connection_id)
                .await;
        }
        let close_error = if let Some(attachment) = attachment {
            attachment
                .capability
                .close(&attachment.handle)
                .await
                .err()
                .map(|error| error.to_string())
        } else {
            detach_session(&runtime, debugger.session_id()).await;
            None
        };

        {
            let mut state = self.state.lock().await;
            if state
                .target_debuggers
                .get(&key)
                .is_some_and(|current| current.same_instance(&debugger))
            {
                remove_debugger_registration(&mut state, &key);
            } else if state.target_debuggers.contains_key(&key) {
                return Err(invalid_state(
                    "target attachment changed while detach was pending",
                ));
            }
        }
        self.publish_target_attachment_change(&key, attempt).await;

        if let Some(error) = close_error {
            return Err(internal_error(error));
        }
        let state = self.state.lock().await;
        service_snapshot(&state, &self.agent_instance_id, &context_id)
            .ok_or_else(|| not_found("context", &context_id))
    }

    async fn resume_target(
        &self,
        _ctx: &CallCtx,
        context_id: String,
        connection_id: String,
        target_id: String,
        pause_epoch: u64,
    ) -> Result<TargetDebuggerSnapshot, JsonRpcError> {
        self.target_debugger(&context_id, &connection_id, &target_id)
            .await?
            .resume(pause_epoch)
            .await
            .map_err(target_debugger_rpc_error)
    }

    async fn step_target(
        &self,
        _ctx: &CallCtx,
        context_id: String,
        connection_id: String,
        target_id: String,
        pause_epoch: u64,
        kind: ApiStepKind,
    ) -> Result<TargetDebuggerSnapshot, JsonRpcError> {
        self.target_debugger(&context_id, &connection_id, &target_id)
            .await?
            .step(
                pause_epoch,
                match kind {
                    ApiStepKind::Into => StepKind::Into,
                    ApiStepKind::Over => StepKind::Over,
                    ApiStepKind::Out => StepKind::Out,
                },
            )
            .await
            .map_err(target_debugger_rpc_error)
    }

    async fn evaluate_target(
        &self,
        _ctx: &CallCtx,
        context_id: String,
        connection_id: String,
        target_id: String,
        pause_epoch: Option<u64>,
        frame_index: u32,
        expression: String,
    ) -> Result<EvaluationSnapshot, JsonRpcError> {
        self.target_debugger(&context_id, &connection_id, &target_id)
            .await?
            .evaluate(pause_epoch, frame_index, expression)
            .await
            .map_err(target_debugger_rpc_error)
    }

    async fn get_scope_variables(
        &self,
        _ctx: &CallCtx,
        context_id: String,
        connection_id: String,
        target_id: String,
        pause_epoch: u64,
        frame_index: u32,
        scope_index: u32,
    ) -> Result<Vec<VariableSnapshot>, JsonRpcError> {
        self.target_debugger(&context_id, &connection_id, &target_id)
            .await?
            .scope_variables(pause_epoch, frame_index, scope_index)
            .await
            .map_err(target_debugger_rpc_error)
    }

    async fn get_object_properties(
        &self,
        _ctx: &CallCtx,
        context_id: String,
        connection_id: String,
        target_id: String,
        pause_epoch: Option<u64>,
        object_id: String,
    ) -> Result<Vec<VariableSnapshot>, JsonRpcError> {
        self.target_debugger(&context_id, &connection_id, &target_id)
            .await?
            .object_properties(pause_epoch, object_id)
            .await
            .map_err(target_debugger_rpc_error)
    }

    async fn inspect_value(
        &self,
        _ctx: &CallCtx,
        context_id: String,
        connection_id: String,
        target_id: String,
        pause_epoch: Option<u64>,
        selector: ValueSelector,
        options: ValueInspectionOptions,
    ) -> Result<ValueSnapshot, JsonRpcError> {
        self.target_debugger(&context_id, &connection_id, &target_id)
            .await?
            .inspect_value(pause_epoch, selector, options)
            .await
            .map_err(target_debugger_rpc_error)
    }

    async fn set_logpoint(
        &self,
        _ctx: &CallCtx,
        context_id: String,
        connection_id: String,
        target_id: String,
        logpoint_id: String,
        source_url: String,
        line: u32,
        column: u32,
        expression: String,
    ) -> Result<TargetDebuggerSnapshot, JsonRpcError> {
        self.set_logpoints(
            _ctx,
            context_id,
            connection_id,
            target_id,
            vec![LogpointSpec {
                id: logpoint_id,
                source_url,
                line,
                column,
                expression,
            }],
        )
        .await
    }

    async fn set_logpoints(
        &self,
        _ctx: &CallCtx,
        context_id: String,
        connection_id: String,
        target_id: String,
        logpoints: Vec<LogpointSpec>,
    ) -> Result<TargetDebuggerSnapshot, JsonRpcError> {
        if logpoints.is_empty() {
            return Err(invalid_params("at least one logpoint is required"));
        }
        let breakpoints = logpoints
            .into_iter()
            .map(|logpoint| {
                validate_id("logpoint", &logpoint.id)?;
                if logpoint.line == 0 || logpoint.column == 0 {
                    return Err(invalid_params("logpoint lines and columns are one-based"));
                }
                Ok(TargetBreakpointSpec {
                    id: format!("log:{}", logpoint.id),
                    source_url: logpoint.source_url,
                    line: logpoint.line,
                    column: logpoint.column,
                    condition: Some(format!(
                        "console.log({}, JSON.stringify(({}))), false",
                        serde_json::to_string(&logpoint.id)
                            .map_err(|error| internal_error(error.to_string()))?,
                        logpoint.expression
                    )),
                })
            })
            .collect::<Result<Vec<_>, JsonRpcError>>()?;
        let debugger = self
            .target_debugger(&context_id, &connection_id, &target_id)
            .await?;
        debugger
            .set_breakpoints(u64::MAX, breakpoints)
            .await
            .map_err(target_debugger_rpc_error)
    }
}
