use super::*;

#[async_trait::async_trait]
impl HeapProfilerApi for DebuggerService {
    async fn take_heap_snapshot(
        &self,
        ctx: &CallCtx,
        target_ref: TargetRef,
        path: String,
        capture_numeric_value: bool,
        expose_internals: bool,
        progress: StreamSender<HeapSnapshotProgress>,
    ) -> Result<HeapSnapshotResult, JsonRpcError> {
        let TargetRef {
            connection:
                ConnectionRef {
                    context_id,
                    connection_id,
                },
            target_id,
        } = target_ref;
        let debugger = self
            .target_debugger(&context_id, &connection_id, &target_id)
            .await?;
        let (progress_tx, progress_rx) = mpsc::channel(16);
        finish_heap_streamed_call(
            ctx,
            drain_heap_progress(
                ctx,
                progress,
                progress_rx,
                debugger.take_heap_snapshot(
                    path,
                    capture_numeric_value,
                    expose_internals,
                    progress_tx,
                ),
            )
            .await,
        )
        .await
    }

    async fn capture_heap_snapshot(
        &self,
        ctx: &CallCtx,
        target_ref: TargetRef,
        capture_id: Option<String>,
        capture_numeric_value: bool,
        expose_internals: bool,
        progress: StreamSender<HeapSnapshotProgress>,
    ) -> Result<HeapCaptureResult, JsonRpcError> {
        let TargetRef {
            connection:
                ConnectionRef {
                    context_id,
                    connection_id,
                },
            target_id,
        } = target_ref;
        if let Some(name) = capture_id.as_ref()
            && let Some(completed) = self
                .promote_completed_capture(
                    &context_id,
                    &connection_id,
                    &target_id,
                    name,
                    CaptureKind::HeapSnapshot,
                )
                .await?
        {
            let CapturePayload::HeapSnapshot { .. } = completed.payload else {
                return Err(invalid_state(
                    "completed capture kind does not match reservation",
                ));
            };
            return completed
                .heap_result
                .ok_or_else(|| invalid_state("completed heap capture result is missing"));
        }
        let debugger = self
            .target_debugger(&context_id, &connection_id, &target_id)
            .await?;
        let owner = debugger.snapshot();
        let reservation = CaptureReservationGuard::new(
            self.clone(),
            self.reserve_capture_optional(
                &context_id,
                &owner.connection_id,
                &owner.target_id,
                owner.connection_generation,
                capture_id,
                CaptureKind::HeapSnapshot,
            )
            .await?,
        )
        .with_heap_debugger(debugger.clone());
        let name = reservation.reservation.metadata.name.clone();
        if reservation.reservation.completed.is_some() {
            let completed = self
                .promote_completed_capture(
                    &reservation.reservation.metadata.context_id,
                    &reservation.reservation.metadata.connection_id,
                    &reservation.reservation.metadata.target_id,
                    &reservation.reservation.metadata.name,
                    CaptureKind::HeapSnapshot,
                )
                .await?
                .ok_or_else(|| invalid_state("completed capture reservation disappeared"))?;
            let CapturePayload::HeapSnapshot { .. } = completed.payload else {
                return Err(invalid_state(
                    "completed capture kind does not match reservation",
                ));
            };
            let result = completed
                .heap_result
                .ok_or_else(|| invalid_state("completed heap capture result is missing"))?;
            return Ok(result);
        }
        let (progress_tx, progress_rx) = mpsc::channel(16);
        let outcome = drain_heap_progress(
            ctx,
            progress,
            progress_rx,
            debugger.capture_heap_snapshot(
                Some(name.clone()),
                capture_numeric_value,
                expose_internals,
                progress_tx,
            ),
        )
        .await;
        let cancellation = outcome.cancellation;
        let delivery_error = outcome.delivery_error;
        let result = match outcome.result.map_err(target_debugger_rpc_error) {
            Ok(result) => result,
            Err(error) => {
                self.abandon_capture(&reservation.reservation).await;
                if cancellation.is_some() || ctx.is_cancelled() {
                    let reason = match &cancellation {
                        Some(reason) => reason.clone(),
                        None => ctx.cancelled().await,
                    };
                    return Err(cancelled_heap_call(reason));
                }
                return Err(error);
            }
        };
        let interruption = if cancellation.is_some() || ctx.is_cancelled() {
            Some(cancelled_heap_call(match &cancellation {
                Some(reason) => reason.clone(),
                None => ctx.cancelled().await,
            }))
        } else {
            delivery_error
        };
        if let Some(error) = interruption {
            debugger
                .delete_stored_capture(name)
                .await
                .map_err(target_debugger_rpc_error)?;
            self.abandon_capture(&reservation.reservation).await;
            return Err(error);
        }
        let (staging_path, final_path) = self.heap_capture_paths(&reservation.reservation);
        if let Err(error) = debugger
            .copy_heap_capture(name, staging_path.to_string_lossy().into_owned())
            .await
            .map_err(target_debugger_rpc_error)
        {
            self.abandon_capture(&reservation.reservation).await;
            remove_capture_payload_files([staging_path]);
            return Err(error);
        }
        if let Err(error) = self.capture_storage.sync_file(&staging_path) {
            self.abandon_capture(&reservation.reservation).await;
            remove_capture_payload_files([staging_path]);
            return Err(internal_error(format!(
                "failed to synchronize heap capture storage before publication: {error}"
            )));
        }
        if let Err(error) = fs::rename(&staging_path, &final_path) {
            self.abandon_capture(&reservation.reservation).await;
            remove_capture_payload_files([staging_path, final_path]);
            return Err(internal_error(format!(
                "failed to publish heap capture storage: {error}"
            )));
        }
        if let Err(error) = self.capture_storage.sync_parent(&final_path) {
            self.abandon_capture(&reservation.reservation).await;
            remove_capture_payload_files([final_path]);
            return Err(internal_error(format!(
                "failed to synchronize heap capture storage publication: {error}"
            )));
        }
        if let Err(error) = self
            .store_heap_capture(
                &reservation.reservation,
                CapturePayload::HeapSnapshot {
                    path: final_path.to_string_lossy().into_owned(),
                },
                result.clone(),
            )
            .await
        {
            if !self
                .completed_capture_is_retained(&reservation.reservation)
                .await
            {
                remove_capture_payload_files([final_path]);
            }
            return Err(error);
        }
        if cancellation.is_some() || ctx.is_cancelled() {
            let reason = match &cancellation {
                Some(reason) => reason.clone(),
                None => ctx.cancelled().await,
            };
            self.delete_capture(&CallCtx::default(), context_id, result.capture_id.clone())
                .await?;
            return Err(cancelled_heap_call(reason));
        }
        Ok(result)
    }

    async fn get_heap_classes(
        &self,
        _ctx: &CallCtx,
        target_ref: TargetRef,
        capture_id: String,
        filter: Option<String>,
        no_cache: bool,
    ) -> Result<HeapClassSnapshot, JsonRpcError> {
        let TargetRef {
            connection:
                ConnectionRef {
                    context_id,
                    connection_id,
                },
            target_id,
        } = target_ref;
        self.target_debugger(&context_id, &connection_id, &target_id)
            .await?
            .get_heap_classes(capture_id, filter, no_cache)
            .await
            .map_err(target_debugger_rpc_error)
    }

    async fn select_promises(
        &self,
        _ctx: &CallCtx,
        target_ref: TargetRef,
        capture_id: String,
        state: Option<PromiseState>,
        limit: u32,
        max_preview_length: u32,
    ) -> Result<PromiseSelectionSnapshot, JsonRpcError> {
        let TargetRef {
            connection:
                ConnectionRef {
                    context_id,
                    connection_id,
                },
            target_id,
        } = target_ref;
        self.target_debugger(&context_id, &connection_id, &target_id)
            .await?
            .select_promises(capture_id, state, limit, max_preview_length)
            .await
            .map_err(target_debugger_rpc_error)
    }

    async fn select_heap_nodes(
        &self,
        _ctx: &CallCtx,
        target_ref: TargetRef,
        capture_id: String,
        selector: HeapNodeSelector,
        max_string_length: Option<u32>,
        include_dominators: bool,
    ) -> Result<HeapNodeSelectionSnapshot, JsonRpcError> {
        let TargetRef {
            connection:
                ConnectionRef {
                    context_id,
                    connection_id,
                },
            target_id,
        } = target_ref;
        self.target_debugger(&context_id, &connection_id, &target_id)
            .await?
            .select_heap_nodes(capture_id, selector, max_string_length, include_dominators)
            .await
            .map_err(target_debugger_rpc_error)
    }

    async fn get_heap_references(
        &self,
        _ctx: &CallCtx,
        target_ref: TargetRef,
        reference: String,
        direction: HeapReferenceDirection,
        edge_policy: HeapEdgePolicy,
        limit: u32,
        max_string_length: Option<u32>,
    ) -> Result<HeapReferencesSnapshot, JsonRpcError> {
        let TargetRef {
            connection:
                ConnectionRef {
                    context_id,
                    connection_id,
                },
            target_id,
        } = target_ref;
        self.target_debugger(&context_id, &connection_id, &target_id)
            .await?
            .get_heap_references(reference, direction, edge_policy, limit, max_string_length)
            .await
            .map_err(target_debugger_rpc_error)
    }

    async fn get_heap_path(
        &self,
        _ctx: &CallCtx,
        target_ref: TargetRef,
        from: String,
        to: String,
        options: HeapPathOptions,
        max_string_length: Option<u32>,
    ) -> Result<Option<HeapPathSnapshot>, JsonRpcError> {
        let TargetRef {
            connection:
                ConnectionRef {
                    context_id,
                    connection_id,
                },
            target_id,
        } = target_ref;
        self.target_debugger(&context_id, &connection_id, &target_id)
            .await?
            .get_heap_path(from, to, options, max_string_length)
            .await
            .map_err(target_debugger_rpc_error)
    }

    async fn get_heap_dominator_chain(
        &self,
        _ctx: &CallCtx,
        target_ref: TargetRef,
        reference: String,
        max_string_length: Option<u32>,
    ) -> Result<HeapDominatorSnapshot, JsonRpcError> {
        let TargetRef {
            connection:
                ConnectionRef {
                    context_id,
                    connection_id,
                },
            target_id,
        } = target_ref;
        self.target_debugger(&context_id, &connection_id, &target_id)
            .await?
            .get_heap_dominator_chain(reference, max_string_length)
            .await
            .map_err(target_debugger_rpc_error)
    }

    async fn aggregate_heap_snapshot(
        &self,
        _ctx: &CallCtx,
        target_ref: TargetRef,
        capture_id: String,
        by: HeapAggregateBy,
        limit: u32,
        max_string_length: Option<u32>,
    ) -> Result<HeapAggregateSnapshot, JsonRpcError> {
        let TargetRef {
            connection:
                ConnectionRef {
                    context_id,
                    connection_id,
                },
            target_id,
        } = target_ref;
        self.target_debugger(&context_id, &connection_id, &target_id)
            .await?
            .aggregate_heap_snapshot(capture_id, by, limit, max_string_length)
            .await
            .map_err(target_debugger_rpc_error)
    }

    async fn diff_heap_snapshots(
        &self,
        _ctx: &CallCtx,
        target_ref: TargetRef,
        older_capture_id: String,
        newer_capture_id: String,
        by: HeapAggregateBy,
        limit: u32,
        max_string_length: Option<u32>,
    ) -> Result<HeapDiffSnapshot, JsonRpcError> {
        let TargetRef {
            connection:
                ConnectionRef {
                    context_id,
                    connection_id,
                },
            target_id,
        } = target_ref;
        self.target_debugger(&context_id, &connection_id, &target_id)
            .await?
            .diff_heap_snapshots(
                older_capture_id,
                newer_capture_id,
                by,
                limit,
                max_string_length,
            )
            .await
            .map_err(target_debugger_rpc_error)
    }

    async fn get_heap_snapshot_progress(
        &self,
        _ctx: &CallCtx,
        target_ref: TargetRef,
    ) -> Result<Option<HeapSnapshotProgress>, JsonRpcError> {
        let TargetRef {
            connection:
                ConnectionRef {
                    context_id,
                    connection_id,
                },
            target_id,
        } = target_ref;
        Ok(self
            .target_debugger(&context_id, &connection_id, &target_id)
            .await?
            .heap_snapshot_progress())
    }
}
