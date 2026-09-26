use super::*;

#[async_trait::async_trait]
impl CaptureApi for DebuggerService {
    async fn list_captures(
        &self,
        _ctx: &CallCtx,
        context_id: String,
    ) -> Result<Vec<CaptureSnapshot>, CaptureError> {
        let state = self.state.lock().await;
        if !state.contexts.contains_key(&context_id) {
            return Err(CaptureError::ContextNotFound { context_id });
        }
        Ok(state
            .captures
            .range((context_id.clone(), String::new())..=(context_id, char::MAX.to_string()))
            .map(|(_, capture)| capture.metadata.clone())
            .collect())
    }

    async fn get_capture(
        &self,
        _ctx: &CallCtx,
        context_id: String,
        capture_name: String,
    ) -> Result<CaptureSnapshot, CaptureError> {
        let state = self.state.lock().await;
        Ok(
            select_stored_capture(&state, &context_id, &capture_name, None, None, None)?
                .metadata
                .clone(),
        )
    }

    async fn delete_capture(
        &self,
        _ctx: &CallCtx,
        context_id: String,
        capture_name: String,
    ) -> Result<bool, CaptureError> {
        let mut state = self.state.lock().await;
        let key = (context_id, capture_name.clone());
        let (capture, completed_reservation) =
            if let Some(capture) = state.captures.get(&key).cloned() {
                (capture, false)
            } else if let Some(reservation) = state.capture_reservations.get(&key) {
                if reservation.deleting {
                    return Err(CaptureError::CaptureDeleting {
                        context_id: key.0.clone(),
                        capture_id: capture_name,
                    });
                }
                let Some(completed) = &reservation.completed else {
                    return Err(CaptureError::CaptureBusy {
                        context_id: key.0.clone(),
                        capture_id: capture_name,
                    });
                };
                (
                    StoredCapture {
                        metadata: reservation.metadata.clone(),
                        payload: completed.payload.clone(),
                        publication_order: 0,
                        heap_mapping: completed
                            .heap_result
                            .as_ref()
                            .and_then(|result| result.mapping.clone()),
                    },
                    true,
                )
            } else {
                return Err(CaptureError::CaptureNotFound {
                    context_id: key.0.clone(),
                    selector: capture_name,
                });
            };
        let storage_id = capture.metadata.storage_id.clone();
        let debugger = state
            .target_debuggers
            .get(&(
                capture.metadata.context_id.clone(),
                capture.metadata.connection_id.clone(),
                capture.metadata.target_id.clone(),
            ))
            .filter(|debugger| {
                debugger.snapshot().connection_generation == capture.metadata.connection_generation
            })
            .cloned();
        if completed_reservation {
            state
                .capture_reservations
                .get_mut(&key)
                .expect("completed capture reservation disappeared while locked")
                .deleting = true;
            drop(state);
        } else {
            let previous = state.clone();
            state.captures.remove(&key);
            self.persist_or_restore(&mut state, previous)?;
            drop(state);
        }
        let mut deletion_guard = completed_reservation
            .then(|| CaptureDeletionGuard::new(self.clone(), key.clone(), storage_id.clone()));
        if let Some(debugger) = debugger {
            if let Err(error) = debugger.delete_stored_capture(capture_name.clone()).await {
                if let Some(guard) = &mut deletion_guard {
                    guard.restore().await;
                }
                return Err(target_debugger_rpc_error(error).into());
            }
        }
        let payload_path = capture.payload_path();
        if let Err(error) = self.capture_storage.remove_for_delete(&payload_path).await {
            if completed_reservation {
                deletion_guard
                    .as_mut()
                    .expect("completed capture deletion guard is missing")
                    .restore()
                    .await;
                return Err(internal_error(format!(
                    "failed to discard completed capture '{capture_name}'; its payload '{}' and retryable reservation were retained: {error}",
                    payload_path.display()
                )).into());
            }
            return Err(internal_error(format!(
                "capture '{capture_name}' was removed from the catalog but its payload '{}' could not be deleted and will be retried during startup cleanup: {error}",
                payload_path.display()
            )).into());
        }
        if completed_reservation {
            let mut state = self.state.lock().await;
            if state.capture_reservations.get(&key).is_some_and(|current| {
                current.metadata.storage_id == storage_id && current.completed.is_some()
            }) {
                state.capture_reservations.remove(&key);
            }
            deletion_guard
                .as_mut()
                .expect("completed capture deletion guard is missing")
                .disarm();
        }
        Ok(true)
    }

    async fn get_stored_coverage(
        &self,
        _ctx: &CallCtx,
        context_id: String,
        capture_name: String,
        source_path: Option<String>,
        target_id: Option<String>,
        connection_id: Option<String>,
        path_glob: Option<String>,
        exclude_capture_id: Option<String>,
    ) -> Result<CoverageSnapshot, CaptureError> {
        let (payload, name, baseline_payload) = {
            let state = self.state.lock().await;
            let capture = select_stored_capture(
                &state,
                &context_id,
                &capture_name,
                Some(CaptureKind::Coverage),
                target_id.as_deref(),
                connection_id.as_deref(),
            )?;
            let baseline = exclude_capture_id.as_ref().map(|selector| {
                let baseline = select_stored_capture(
                    &state, &context_id, selector, Some(CaptureKind::Coverage),
                    target_id.as_deref(), connection_id.as_deref(),
                )?;
                if baseline.metadata.target_id != capture.metadata.target_id
                    || baseline.metadata.connection_id != capture.metadata.connection_id
                    || baseline.metadata.connection_generation != capture.metadata.connection_generation
                {
                    return Err(invalid_params(
                        "coverage exclusion requires captures from the same target and connection generation",
                    ));
                }
                Ok(baseline.payload.clone())
            }).transpose()?;
            (
                capture.payload.clone(),
                capture.metadata.name.clone(),
                baseline,
            )
        };
        let CapturePayload::Coverage(mut snapshot) =
            load_capture_payload(&payload, CaptureKind::Coverage)
                .map_err(capture_payload_rpc_error)?
        else {
            return Err(invalid_params(&format!(
                "capture '{capture_name}' is not a coverage capture"
            )).into());
        };
        if let Some(payload) = baseline_payload {
            let CapturePayload::Coverage(baseline) =
                load_capture_payload(&payload, CaptureKind::Coverage)
                    .map_err(capture_payload_rpc_error)?
            else {
                return Err(invalid_params("baseline is not a coverage capture").into());
            };
            snapshot = crate::debugger::target_debugger::exclude_coverage(snapshot, &baseline);
        }
        snapshot.capture_id = Some(name);
        let (prepared, diagnostics) = crate::capture::capture_projection::prepare_view_sources(
            snapshot.sources.iter().filter_map(|source| {
                source.provenance.as_ref().map(|provenance| (source.script_id.as_str(), provenance))
            }),
            true,
        ).await;
        snapshot.projection_diagnostics.extend(diagnostics);
        crate::capture::capture_projection::project_stored_coverage_with_sources(&mut snapshot, &prepared);
        let diagnostics = snapshot.projection_diagnostics.clone();
        crate::capture::coverage::coverage_filter::filter_coverage(
            &mut snapshot,
            source_path.as_deref(),
            path_glob.as_deref(),
        )
        .map_err(|error| {
            let details = if diagnostics.is_empty() {
                String::new()
            } else {
                format!(" Projection unavailable: {}", diagnostics.join("; "))
            };
            invalid_params(&format!("{error}{details}"))
        })?;
        Ok(snapshot)
    }

    async fn get_stored_cpu_profile(
        &self,
        _ctx: &CallCtx,
        context_id: String,
        capture_name: String,
        source_path: Option<String>,
        target_id: Option<String>,
        connection_id: Option<String>,
    ) -> Result<CpuProfileSnapshot, CaptureError> {
        let (payload, name) = {
            let state = self.state.lock().await;
            let capture = select_stored_capture(
                &state,
                &context_id,
                &capture_name,
                Some(CaptureKind::CpuProfile),
                target_id.as_deref(),
                connection_id.as_deref(),
            )?;
            (capture.payload.clone(), capture.metadata.name.clone())
        };
        let CapturePayload::CpuProfile(mut snapshot) =
            load_capture_payload(&payload, CaptureKind::CpuProfile)
                .map_err(capture_payload_rpc_error)?
        else {
            return Err(invalid_params(&format!(
                "capture '{capture_name}' is not a CPU profile capture"
            )).into());
        };
        snapshot.capture_id = name;
        let (prepared, diagnostics) = if snapshot.samples.is_empty() && !snapshot.functions.is_empty() {
            (Default::default(), Vec::new())
        } else {
            crate::capture::capture_projection::prepare_view_sources(
                snapshot.script_provenance.iter().map(|(id, provenance)| (id.as_str(), provenance)),
                false,
            ).await
        };
        snapshot.projection_diagnostics.extend(diagnostics);
        crate::capture::capture_projection::project_stored_cpu_with_sources(&mut snapshot, source_path.as_deref(), &prepared)
            .map_err(target_debugger_rpc_error)?;
        Ok(snapshot)
    }

    async fn get_stored_heap_classes(
        &self,
        _ctx: &CallCtx,
        context_id: String,
        capture_name: String,
        filter: Option<String>,
        target_id: Option<String>,
        connection_id: Option<String>,
    ) -> Result<HeapClassSnapshot, CaptureError> {
        let (payload, mapping, capture_name) = {
            let state = self.state.lock().await;
            let capture = select_stored_capture(
                &state,
                &context_id,
                &capture_name,
                Some(CaptureKind::HeapSnapshot),
                target_id.as_deref(),
                connection_id.as_deref(),
            )?;
            (
                capture.payload.clone(),
                capture.heap_mapping.clone(),
                capture.metadata.name.clone(),
            )
        };
        let CapturePayload::HeapSnapshot { path } =
            load_capture_payload(&payload, CaptureKind::HeapSnapshot)
                .map_err(capture_payload_rpc_error)?
        else {
            return Err(invalid_state(
                "stored heap payload kind does not match metadata",
            ).into());
        };
        let (prepared, diagnostics) = crate::debugger::target_debugger::prepare_heap_view_sources(mapping.as_ref()).await;
        let capture_for_task = capture_name.clone();
        tokio::task::spawn_blocking(move || {
            let mapping = crate::debugger::target_debugger::recover_heap_mapping_for_view(mapping, &prepared, &diagnostics);
            stored_heap_classes(
                Path::new(&path),
                capture_for_task,
                filter.as_deref(),
                mapping.as_ref(),
            )
        })
        .await
        .map_err(|error| internal_error(error.to_string()))?
        .map_err(target_debugger_rpc_error)
        .map_err(CaptureError::from)
    }

    async fn supply_stored_heap_source_map(
        &self,
        _ctx: &CallCtx,
        context_id: String,
        capture_name: String,
        supply: crate::api::service_api::HeapSourceMapSupply,
    ) -> Result<(), CaptureError> {
        let mut state = self.state.lock().await;
        let previous = state.clone();
        let capture_name = select_stored_capture(
            &state,
            &context_id,
            &capture_name,
            Some(CaptureKind::HeapSnapshot),
            None,
            None,
        )?
        .metadata
        .name
        .clone();
        let capture = state
            .captures
            .get_mut(&(context_id, capture_name.clone()))
            .ok_or_else(|| not_found("capture", &capture_name))?;
        if capture.metadata.kind != CaptureKind::HeapSnapshot {
            return Err(invalid_params("capture is not a heap snapshot").into());
        }
        let mapping = capture.heap_mapping.as_mut().ok_or_else(|| {
            invalid_state(
                "legacy capture has no captured script hashes; cannot safely supply a source map",
            )
        })?;
        crate::debugger::target_debugger::supply_heap_source_map(mapping, supply)
            .map_err(target_debugger_rpc_error)?;
        self.persist_or_restore(&mut state, previous)?;
        Ok(())
    }
}
