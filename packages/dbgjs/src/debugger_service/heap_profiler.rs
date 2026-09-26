use super::*;

impl DebuggerService {
    async fn stored_heap_capture(
        &self,
        target: &TargetRef,
        capture_id: &str,
    ) -> Result<Option<(String, Option<crate::service_api::HeapMappingSnapshot>, String)>, HeapProfilerError> {
        let stored = {
            let state = self.state.lock().await;
            select_stored_capture(
                &state,
                &target.connection.context_id,
                capture_id,
                Some(CaptureKind::HeapSnapshot),
                Some(&target.target_id),
                Some(&target.connection.connection_id),
            )
            .ok()
            .map(|capture| {
                (
                    capture.payload.clone(),
                    capture.heap_mapping.clone(),
                    capture.metadata.name.clone(),
                )
            })
        };
        let Some((payload, mapping, name)) = stored else {
            return Ok(None);
        };
        let CapturePayload::HeapSnapshot { path } =
            load_capture_payload(&payload, CaptureKind::HeapSnapshot)
                .map_err(capture_payload_rpc_error)?
        else {
            return Err(invalid_state("stored heap payload kind does not match metadata").into());
        };
        Ok(Some((path, mapping, name)))
    }

    async fn with_stored_heap_graph<T: Send + 'static>(
        &self,
        target: &TargetRef,
        capture_id: &str,
        enrich_source: bool,
        analyze: impl FnOnce(crate::target_debugger::StoredHeapGraph)
            -> Result<T, TargetDebuggerError> + Send + 'static,
    ) -> Result<Option<T>, HeapProfilerError> {
        let Some((path, mapping, name)) = self.stored_heap_capture(target, capture_id).await?
        else {
            return Ok(None);
        };
        let (prepared, diagnostics) = if enrich_source {
            crate::target_debugger::prepare_heap_view_sources(mapping.as_ref()).await
        } else {
            (Default::default(), Vec::new())
        };
        tokio::task::spawn_blocking(move || {
            let mapping = if enrich_source {
                crate::target_debugger::recover_heap_mapping_for_view(mapping, &prepared, &diagnostics)
            } else {
                mapping
            };
            let graph = crate::target_debugger::StoredHeapGraph::open(
                Path::new(&path), name, mapping)?;
            analyze(graph)
        })
        .await
        .map_err(|error| internal_error(error.to_string()))?
        .map(Some)
        .map_err(HeapProfilerError::from)
    }
}

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
    ) -> Result<HeapSnapshotResult, HeapProfilerError> {
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
    ) -> Result<HeapCaptureResult, HeapProfilerError> {
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
                ).into());
            };
            return completed
                .heap_result
                .ok_or_else(|| invalid_state("completed heap capture result is missing").into());
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
                ).into());
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
        let result = match outcome.result.map_err(HeapProfilerError::from) {
            Ok(result) => result,
            Err(error) => {
                self.abandon_capture(&reservation.reservation).await;
                if cancellation.is_some() || ctx.is_cancelled() {
                    let reason = match &cancellation {
                        Some(reason) => reason.clone(),
                        None => ctx.cancelled().await,
                    };
                    return Err(cancelled_heap_call(reason).into());
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
                .map_err(HeapProfilerError::from)?;
            self.abandon_capture(&reservation.reservation).await;
            return Err(error.into());
        }
        let (staging_path, final_path) = self.heap_capture_paths(&reservation.reservation);
        if let Err(error) = debugger
            .copy_heap_capture(name, staging_path.to_string_lossy().into_owned())
            .await
            .map_err(HeapProfilerError::from)
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
            )).into());
        }
        if let Err(error) = fs::rename(&staging_path, &final_path) {
            self.abandon_capture(&reservation.reservation).await;
            remove_capture_payload_files([staging_path, final_path]);
            return Err(internal_error(format!(
                "failed to publish heap capture storage: {error}"
            )).into());
        }
        if let Err(error) = self.capture_storage.sync_parent(&final_path) {
            self.abandon_capture(&reservation.reservation).await;
            remove_capture_payload_files([final_path]);
            return Err(internal_error(format!(
                "failed to synchronize heap capture storage publication: {error}"
            )).into());
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
            return Err(error.into());
        }
        if cancellation.is_some() || ctx.is_cancelled() {
            let reason = match &cancellation {
                Some(reason) => reason.clone(),
                None => ctx.cancelled().await,
            };
            self.delete_capture(&CallCtx::default(), context_id, result.capture_id.clone())
                .await.map_err(JsonRpcError::from)?;
            return Err(cancelled_heap_call(reason).into());
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
    ) -> Result<HeapClassSnapshot, HeapProfilerError> {
        if let Some((path, mapping, name)) =
            self.stored_heap_capture(&target_ref, &capture_id).await?
        {
            let (prepared, diagnostics) = crate::target_debugger::prepare_heap_view_sources(mapping.as_ref()).await;
            return tokio::task::spawn_blocking(move || {
                let mapping = crate::target_debugger::recover_heap_mapping_for_view(mapping, &prepared, &diagnostics);
                stored_heap_classes(Path::new(&path), name, filter.as_deref(), mapping.as_ref())
            })
            .await
            .map_err(|error| internal_error(error.to_string()))?
            .map_err(HeapProfilerError::from);
        }
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
            .map_err(HeapProfilerError::from)
    }

    async fn select_promises(
        &self,
        _ctx: &CallCtx,
        target_ref: TargetRef,
        capture_id: String,
        state: Option<PromiseState>,
        limit: u32,
        max_preview_length: u32,
    ) -> Result<PromiseSelectionSnapshot, HeapProfilerError> {
        if let Some(result) = self.with_stored_heap_graph(&target_ref, &capture_id, false,
            move |graph| graph.promises(state, limit, max_preview_length)).await? {
            return Ok(result);
        }
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
            .map_err(HeapProfilerError::from)
    }

    async fn select_heap_nodes(
        &self,
        _ctx: &CallCtx,
        target_ref: TargetRef,
        capture_id: String,
        selector: HeapNodeSelector,
        max_string_length: Option<u32>,
        include_dominators: bool,
    ) -> Result<HeapNodeSelectionSnapshot, HeapProfilerError> {
        if self.stored_heap_capture(&target_ref, &capture_id).await?.is_some() {
            return self.with_stored_heap_graph(&target_ref, &capture_id, true,
                move |graph| graph.select(selector, max_string_length, include_dominators))
                .await?.ok_or_else(|| invalid_state("stored heap capture disappeared").into());
        }
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
            .map_err(HeapProfilerError::from)
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
    ) -> Result<HeapReferencesSnapshot, HeapProfilerError> {
        if let Some((name, _)) = reference.rsplit_once('#') {
            let capture_id = name.to_owned();
            if self.stored_heap_capture(&target_ref, &capture_id).await?.is_some() {
                return self.with_stored_heap_graph(&target_ref, &capture_id, true, move |graph| {
                    graph.references(&reference, direction, edge_policy, limit, max_string_length)
                }).await?.ok_or_else(|| invalid_state("stored heap capture disappeared").into());
            }
        }
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
            .map_err(HeapProfilerError::from)
    }

    async fn get_heap_path(
        &self,
        _ctx: &CallCtx,
        target_ref: TargetRef,
        from: String,
        to: String,
        options: HeapPathOptions,
        max_string_length: Option<u32>,
    ) -> Result<Option<HeapPathSnapshot>, HeapProfilerError> {
        if let Some((name, _)) = from.rsplit_once('#') {
            let capture_id = name.to_owned();
            if self.stored_heap_capture(&target_ref, &capture_id).await?.is_some() {
                return self.with_stored_heap_graph(&target_ref, &capture_id, true, move |graph| {
                    graph.path(from, to, options, max_string_length)
                }).await?.ok_or_else(|| invalid_state("stored heap capture disappeared").into());
            }
        }
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
            .map_err(HeapProfilerError::from)
    }

    async fn get_heap_dominator_chain(
        &self,
        _ctx: &CallCtx,
        target_ref: TargetRef,
        reference: String,
        max_string_length: Option<u32>,
    ) -> Result<HeapDominatorSnapshot, HeapProfilerError> {
        if let Some((name, _)) = reference.rsplit_once('#') {
            let capture_id = name.to_owned();
            if self.stored_heap_capture(&target_ref, &capture_id).await?.is_some() {
                return self.with_stored_heap_graph(&target_ref, &capture_id, true, move |graph| {
                    graph.dominators(&reference, max_string_length)
                }).await?.ok_or_else(|| invalid_state("stored heap capture disappeared").into());
            }
        }
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
            .map_err(HeapProfilerError::from)
    }

    async fn aggregate_heap_snapshot(
        &self,
        _ctx: &CallCtx,
        target_ref: TargetRef,
        capture_id: String,
        by: HeapAggregateBy,
        limit: u32,
        max_string_length: Option<u32>,
    ) -> Result<HeapAggregateSnapshot, HeapProfilerError> {
        if let Some(result) = self.with_stored_heap_graph(&target_ref, &capture_id, false,
            move |graph| graph.aggregate(by, limit, max_string_length)).await? {
            return Ok(result);
        }
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
            .map_err(HeapProfilerError::from)
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
    ) -> Result<HeapDiffSnapshot, HeapProfilerError> {
        if let Some((older_path, older_mapping, older_name)) =
            self.stored_heap_capture(&target_ref, &older_capture_id).await?
        {
            let (newer_path, newer_mapping, newer_name) = self
                .stored_heap_capture(&target_ref, &newer_capture_id)
                .await?
                .ok_or_else(|| TargetDebuggerError::HeapCaptureNotFound(newer_capture_id))?;
            return tokio::task::spawn_blocking(move || {
                let older = crate::target_debugger::StoredHeapGraph::open(
                    Path::new(&older_path), older_name, older_mapping)?;
                let newer = crate::target_debugger::StoredHeapGraph::open(
                    Path::new(&newer_path), newer_name, newer_mapping)?;
                older.diff(&newer, by, limit, max_string_length)
            })
            .await
            .map_err(|error| internal_error(error.to_string()))?
            .map_err(HeapProfilerError::from);
        }
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
            .map_err(HeapProfilerError::from)
    }

    async fn get_heap_snapshot_progress(
        &self,
        _ctx: &CallCtx,
        target_ref: TargetRef,
    ) -> Result<Option<HeapSnapshotProgress>, HeapProfilerError> {
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
