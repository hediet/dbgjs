use super::*;

#[async_trait::async_trait]
impl ContextApi for DebuggerService {
    async fn list_contexts(
        &self,
        _ctx: &CallCtx,
        cwd: Option<String>,
    ) -> Result<Vec<ContextSummary>, JsonRpcError> {
        let state = self.state.lock().await;
        let mut contexts = state
            .contexts
            .iter()
            .map(|(id, context)| ContextSummary {
                agent_instance_id: self.agent_instance_id.clone(),
                id: id.clone(),
                kind: state
                    .context_kinds
                    .get(id)
                    .copied()
                    .unwrap_or(ContextKind::Named),
                path_distance: cwd.as_deref().and_then(|cwd| {
                    (state.context_kinds.get(id) == Some(&ContextKind::Path))
                        .then(|| path_relation(cwd, id))
                        .flatten()
                        .map(|relation| relation.distance)
                }),
                path_ancestor: cwd.as_deref().and_then(|cwd| {
                    (state.context_kinds.get(id) == Some(&ContextKind::Path))
                        .then(|| path_relation(cwd, id))
                        .flatten()
                        .map(|relation| relation.ancestor)
                }),
                display_name: context.display_name.clone(),
                revision: context.revision,
                connection_count: context.connections.len() as u32,
                breakpoint_count: context.breakpoints.len() as u32,
            })
            .collect::<Vec<_>>();
        contexts.sort_by(|left, right| {
            left.kind
                .cmp(&right.kind)
                .then_with(|| match (cwd.as_deref(), left.kind) {
                    (Some(cwd), ContextKind::Path) => {
                        compare_context_paths(cwd, &left.id, &right.id)
                    }
                    _ => left.id.cmp(&right.id),
                })
        });
        Ok(contexts)
    }

    async fn put_context(
        &self,
        _ctx: &CallCtx,
        context_id: String,
        kind: ContextKind,
        display_name: Option<String>,
    ) -> Result<ContextSnapshot, JsonRpcError> {
        validate_context_identity(&context_id, kind)?;
        let mut state = self.state.lock().await;
        if let Some(existing) = state.context_kinds.get(&context_id)
            && existing != &kind
        {
            return Err(invalid_state(&format!(
                "context '{context_id}' is already registered as {existing:?}"
            )));
        }
        let previous = state.clone();
        let context = state
            .contexts
            .get(&context_id)
            .cloned()
            .unwrap_or_else(|| ContextState::new(context_id.clone()));
        let transition = reduce_context(
            &context,
            ContextInput::UserCommand(UserCommand::PutContext { display_name }),
        )
        .map_err(transition_rpc_error)?;
        let result = self.commit_context(&mut state, &context_id, transition);
        state.context_kinds.insert(context_id.clone(), kind);
        self.persist_or_restore(&mut state, previous)?;
        Ok(result)
    }

    async fn get_context(
        &self,
        _ctx: &CallCtx,
        context_id: String,
    ) -> Result<ContextSnapshot, JsonRpcError> {
        let state = self.state.lock().await;
        if !state.contexts.contains_key(&context_id) {
            return Err(not_found("context", &context_id));
        }
        Ok(
            service_snapshot(&state, &self.agent_instance_id, &context_id)
                .expect("context was checked above"),
        )
    }

    async fn get_resource_graph(
        &self,
        _ctx: &CallCtx,
        context_id: String,
    ) -> Result<ResourceGraphSnapshot, JsonRpcError> {
        let state = self.state.lock().await;
        if !state.contexts.contains_key(&context_id) {
            return Err(not_found("context", &context_id));
        }
        let snapshot = state
            .resource_graphs
            .get(&context_id)
            .map(GraphSink::snapshot)
            .unwrap_or_else(|| ResourceGraph::default().snapshot());
        Ok(resource_graph_api_snapshot(snapshot))
    }

    async fn observe_context(
        &self,
        _ctx: &CallCtx,
        context_id: String,
        cursor: ObservationCursor,
        timeout_ms: u64,
    ) -> Result<ObservationResult, JsonRpcError> {
        let requested_revision = match cursor {
            ObservationCursor::Current => None,
            ObservationCursor::After { revision } => Some(revision),
        };
        let deadline = Instant::now() + Duration::from_millis(timeout_ms);
        let mut signal = self.revision_signal.subscribe();
        loop {
            {
                let state = self.state.lock().await;
                if !state.contexts.contains_key(&context_id) {
                    return Err(not_found("context", &context_id));
                }
                if requested_revision.is_none() {
                    return Ok(ObservationResult::Items {
                        items: vec![ContextObservation {
                            snapshot: service_snapshot(
                                &state,
                                &self.agent_instance_id,
                                &context_id,
                            )
                            .expect("context was checked above"),
                            events: Vec::new(),
                        }],
                    });
                }
                let requested = requested_revision.expect("checked above");
                let current = service_snapshot(&state, &self.agent_instance_id, &context_id)
                    .expect("context was checked above");
                let history = state.history.get(&context_id);
                let oldest_available = history
                    .and_then(|history| history.front())
                    .map(|observation| observation.snapshot.revision);
                let history_gap = oldest_available
                    .is_some_and(|oldest| requested.saturating_add(1) < oldest)
                    || (oldest_available.is_none() && requested < current.revision);
                if history_gap {
                    return Ok(ObservationResult::HistoryGap {
                        requested_revision: requested,
                        oldest_available_revision: oldest_available.unwrap_or(current.revision),
                        current,
                    });
                }
                let items = history
                    .into_iter()
                    .flatten()
                    .filter(|item| item.snapshot.revision > requested)
                    .cloned()
                    .collect::<Vec<_>>();
                if !items.is_empty() || timeout_ms == 0 {
                    return Ok(ObservationResult::Items { items });
                }
            }
            if timeout_at(deadline, signal.changed()).await.is_err() {
                return Ok(ObservationResult::Items { items: Vec::new() });
            }
        }
    }

    async fn delete_context(
        &self,
        _ctx: &CallCtx,
        context_id: String,
        options: MutationOptions,
    ) -> Result<bool, JsonRpcError> {
        let (runtimes, capture_paths, proxy_cancellations, relay_cancellations) = {
            let mut state = self.state.lock().await;
            if options.request_id.as_ref().is_some_and(|request_id| {
                state
                    .completed_requests
                    .contains_key(&(context_id.clone(), request_id.clone()))
            }) {
                return Ok(true);
            }
            if let Some(existing) = self.check_mutation_options(&state, &context_id, &options)? {
                return Ok(existing.id == context_id);
            }
            let mut capture_paths = state
                .captures
                .iter()
                .filter(|((candidate_context, _), _)| candidate_context == &context_id)
                .map(|(_, capture)| capture.payload_path())
                .collect::<Vec<_>>();
            for reservation in state
                .capture_reservations
                .iter()
                .filter(|((candidate_context, _), _)| candidate_context == &context_id)
                .map(|(_, reservation)| reservation)
            {
                let (staging, final_path) = self.heap_capture_paths(reservation);
                capture_paths.extend([staging, final_path]);
                if let Some(completed) = &reservation.completed {
                    capture_paths.push(completed.payload.path.as_str().into());
                }
            }
            let previous = state.clone();
            if state.contexts.remove(&context_id).is_none() {
                return Err(not_found("context", &context_id));
            }
            state.resource_graphs.remove(&context_id);
            state.process_projections.remove(&context_id);
            state.context_kinds.remove(&context_id);
            self.complete_request(&mut state, &context_id, &options, 0);
            state.history.remove(&context_id);
            state.source_models.remove(&context_id);
            state
                .target_debuggers
                .retain(|(candidate_context, _, _), _| candidate_context != &context_id);
            state
                .debug_attachments
                .retain(|(candidate_context, _, _), _| candidate_context != &context_id);
            state
                .pause_children_leases
                .retain(|(candidate_context, _), _| candidate_context != &context_id);
            let proxy_cancellations = state
                .playwright_proxies
                .values()
                .filter(|proxy| proxy.context_id == context_id)
                .map(|proxy| proxy.cancel.clone())
                .collect::<Vec<_>>();
            state
                .playwright_proxies
                .retain(|_, proxy| proxy.context_id != context_id);
            let relay_cancellations = state
                .relays
                .values()
                .filter(|relay| relay.context_id == context_id)
                .map(|relay| relay.cancel.clone())
                .collect::<Vec<_>>();
            state
                .relays
                .retain(|_, relay| relay.context_id != context_id);
            let runtime_keys = state
                .runtimes
                .keys()
                .filter(|(candidate_context, _)| candidate_context == &context_id)
                .cloned()
                .collect::<Vec<_>>();
            let runtimes = runtime_keys
                .into_iter()
                .filter_map(|key| state.runtimes.remove(&key))
                .collect::<Vec<_>>();
            state
                .captures
                .retain(|(candidate_context, _), _| candidate_context != &context_id);
            state
                .capture_reservations
                .retain(|(candidate_context, _), _| candidate_context != &context_id);
            self.persist_or_restore(&mut state, previous)?;
            (
                runtimes,
                capture_paths,
                proxy_cancellations,
                relay_cancellations,
            )
        };
        for cancellation in proxy_cancellations {
            let _ = cancellation.send(true);
        }
        for cancellation in relay_cancellations {
            let _ = cancellation.send(true);
        }
        for runtime in runtimes {
            runtime.close().await;
        }
        remove_capture_payload_files(capture_paths);
        Ok(true)
    }

    async fn put_connection(
        &self,
        _ctx: &CallCtx,
        connection_ref: ConnectionRef,
        configuration: ConnectionConfiguration,
    ) -> Result<ContextSnapshot, JsonRpcError> {
        let ConnectionRef {
            context_id,
            connection_id,
        } = connection_ref;
        validate_id("connection", &connection_id)?;
        validate_connection_configuration(&configuration)?;

        let mut state = self.state.lock().await;
        let previous = state.clone();
        let context = state
            .contexts
            .get(&context_id)
            .cloned()
            .ok_or_else(|| not_found("context", &context_id))?;
        let transition = reduce_context(
            &context,
            ContextInput::UserCommand(UserCommand::PutConnection {
                connection_id,
                configuration,
            }),
        )
        .map_err(transition_rpc_error)?;
        let result = self.commit_context(&mut state, &context_id, transition);
        self.persist_or_restore(&mut state, previous)?;
        Ok(result)
    }

    async fn connect_connection(
        &self,
        _ctx: &CallCtx,
        connection_ref: ConnectionRef,
    ) -> Result<ContextSnapshot, JsonRpcError> {
        let ConnectionRef {
            context_id,
            connection_id,
        } = connection_ref;
        let (configuration, attempt) = {
            let mut state = self.state.lock().await;
            let context = state
                .contexts
                .get(&context_id)
                .cloned()
                .ok_or_else(|| not_found("context", &context_id))?;
            let transition = reduce_context(
                &context,
                ContextInput::UserCommand(UserCommand::ConnectConnection {
                    connection_id: connection_id.clone(),
                }),
            )
            .map_err(transition_rpc_error)?;
            let (configuration, attempt) = match transition.effects.as_slice() {
                [
                    ContextEffect::Connect {
                        configuration,
                        attempt,
                        ..
                    },
                ] => (configuration.clone(), *attempt),
                effects => panic!("connect command emitted unexpected effects: {effects:?}"),
            };
            self.commit_context(&mut state, &context_id, transition);
            (configuration, attempt)
        };

        let connected = connect_runtime(&configuration, &connection_id, attempt.generation).await;
        let mut state = self.state.lock().await;
        let context = state
            .contexts
            .get(&context_id)
            .cloned()
            .ok_or_else(|| not_found("context", &context_id))?;
        let runtime_key = (context_id.clone(), connection_id.clone());
        let (completion, runtime, targets) = match connected {
            Ok((runtime, product, protocol_version, targets)) => (
                EffectCompletion::ConnectionOpened {
                    connection_id: connection_id.clone(),
                    attempt,
                    product,
                    protocol_version,
                },
                Some(runtime),
                targets
                    .into_iter()
                    .map(|target| (target.target_id.clone(), target))
                    .collect::<BTreeMap<_, _>>(),
            ),
            Err(message) => (
                EffectCompletion::ConnectionOpenFailed {
                    connection_id: connection_id.clone(),
                    attempt,
                    message,
                },
                None,
                BTreeMap::new(),
            ),
        };
        let targets = filter_target_snapshot_scope(Some(&configuration), targets);
        let transition = match reduce_context(&context, ContextInput::EffectCompletion(completion))
        {
            Ok(transition) => transition,
            Err(error) => {
                drop(state);
                if let Some(runtime) = runtime {
                    runtime.close().await;
                }
                return Err(transition_rpc_error(error));
            }
        };
        if let Some(runtime) = &runtime {
            if let Err(error) = stage_connection_resource_graph(
                &mut state,
                &context_id,
                &connection_id,
                &transition.state,
                &targets,
                runtime,
                &[],
            ) {
                let failure = reduce_context(
                    &context,
                    ContextInput::EffectCompletion(EffectCompletion::ConnectionOpenFailed {
                        connection_id: connection_id.clone(),
                        attempt,
                        message: format!("failed to publish resource graph: {error}"),
                    }),
                )
                .map_err(transition_rpc_error)?;
                self.commit_context(&mut state, &context_id, failure);
                let runtime = runtime.clone();
                drop(state);
                runtime.close().await;
                return Err(internal_error(format!(
                    "failed to publish resource graph: {error}"
                )));
            }
        }
        let auto_attach_targets = if runtime
            .as_ref()
            .is_some_and(|runtime| runtime.is_direct_debugger() || runtime.is_virtual_root())
        {
            Vec::new()
        } else {
            targets
                .values()
                .filter(|target| matches!(target.target_type.as_str(), "page" | "node"))
                .map(|target| target.target_id.clone())
                .collect::<Vec<_>>()
        };
        let result = self.commit_context(&mut state, &context_id, transition);
        if let Some(runtime) = runtime {
            state.runtimes.insert(runtime_key, runtime.clone());
            self.supervise_runtime(
                context_id.clone(),
                connection_id.clone(),
                attempt.configuration_version,
                attempt.generation,
                runtime,
            );
            self.supervise_target_events(
                context_id.clone(),
                connection_id.clone(),
                attempt.configuration_version,
                attempt.generation,
                state
                    .runtimes
                    .get(&(context_id.clone(), connection_id.clone()))
                    .expect("runtime was inserted above")
                    .clone(),
            )
            .await;
            self.supervise_provider_target_events(
                context_id.clone(),
                connection_id.clone(),
                attempt.configuration_version,
                attempt.generation,
                state
                    .runtimes
                    .get(&(context_id.clone(), connection_id.clone()))
                    .expect("runtime was inserted above")
                    .clone(),
            )
            .await;
        } else {
            state.runtimes.remove(&runtime_key);
        }
        drop(state);
        for target_id in auto_attach_targets {
            let _ = self
                .attach_target_internal(
                    _ctx,
                    context_id.clone(),
                    connection_id.clone(),
                    target_id,
                    TargetAttachOptions::default(),
                )
                .await;
        }
        Ok(result)
    }

    async fn disconnect_connection(
        &self,
        _ctx: &CallCtx,
        connection_ref: ConnectionRef,
    ) -> Result<ContextSnapshot, JsonRpcError> {
        let ConnectionRef {
            context_id,
            connection_id,
        } = connection_ref;
        let (runtime, attempt) = {
            let mut state = self.state.lock().await;
            let context = state
                .contexts
                .get(&context_id)
                .cloned()
                .ok_or_else(|| not_found("context", &context_id))?;
            let transition = reduce_context(
                &context,
                ContextInput::UserCommand(UserCommand::DisconnectConnection {
                    connection_id: connection_id.clone(),
                }),
            )
            .map_err(transition_rpc_error)?;
            let attempt = match transition.effects.as_slice() {
                [] => {
                    return Ok(
                        service_snapshot(&state, &self.agent_instance_id, &context_id)
                            .expect("context was checked above"),
                    );
                }
                [ContextEffect::Disconnect { attempt, .. }] => *attempt,
                effects => panic!("disconnect command emitted unexpected effects: {effects:?}"),
            };
            self.commit_context(&mut state, &context_id, transition);
            let runtime = state
                .runtimes
                .remove(&(context_id.clone(), connection_id.clone()));
            retract_connection_resource_graph(
                &mut state,
                &context_id,
                &connection_id,
                attempt.generation,
            );
            remove_connection_debugger_registrations(&mut state, &context_id, &connection_id);
            state
                .pause_children_leases
                .remove(&(context_id.clone(), connection_id.clone()));
            cancel_playwright_proxies(
                &mut state,
                &context_id,
                &connection_id,
                Some(attempt.generation),
            );
            (runtime, attempt)
        };

        self.release_relay_attachments_for_connection(&context_id, &connection_id)
            .await;
        if let Some(runtime) = runtime {
            runtime.close().await;
        }

        let mut state = self.state.lock().await;
        let context = state
            .contexts
            .get(&context_id)
            .cloned()
            .ok_or_else(|| not_found("context", &context_id))?;
        let transition = reduce_context(
            &context,
            ContextInput::EffectCompletion(EffectCompletion::ConnectionClosed {
                connection_id,
                attempt,
            }),
        )
        .map_err(transition_rpc_error)?;
        let result = self.commit_context(&mut state, &context_id, transition);
        Ok(result)
    }

    async fn set_pause_future_children(
        &self,
        _ctx: &CallCtx,
        connection_ref: ConnectionRef,
        enabled: bool,
    ) -> Result<bool, JsonRpcError> {
        let ConnectionRef {
            context_id,
            connection_id,
        } = connection_ref;
        let key = (context_id.clone(), connection_id.clone());
        if !enabled {
            self.state.lock().await.pause_children_leases.remove(&key);
            return Ok(false);
        }
        let (generation, capability) = {
            let state = self.state.lock().await;
            if state.pause_children_leases.contains_key(&key) {
                return Ok(true);
            }
            let context = state
                .contexts
                .get(&context_id)
                .ok_or_else(|| not_found("context", &context_id))?;
            let connection = context
                .connections
                .get(&connection_id)
                .ok_or_else(|| not_found("connection", &connection_id))?;
            let root = connection_root_resource_id(&connection_id, connection.generation);
            let source = connection_source_id(&connection_id, connection.generation);
            let capability = state
                .resource_graphs
                .get(&context_id)
                .and_then(|graph| {
                    graph.read(|graph| {
                        graph.capability_from_source(
                            &root,
                            &source,
                            &CapabilityKind::PauseFutureChildren,
                        )
                    })
                })
                .and_then(|capability| capability.as_pause_future_children())
                .ok_or_else(|| {
                    invalid_state(&format!(
                        "connection '{connection_id}' cannot pause future child targets"
                    ))
                })?;
            (connection.generation, capability)
        };
        let lease = capability
            .arm()
            .await
            .map_err(|error| invalid_state(&error.to_string()))?;
        let mut state = self.state.lock().await;
        let still_current = state
            .contexts
            .get(&context_id)
            .and_then(|context| context.connections.get(&connection_id))
            .is_some_and(|connection| connection.generation == generation);
        if !still_current {
            drop(lease);
            return Err(invalid_state(
                "connection changed while pause-on-start was being armed",
            ));
        }
        state.pause_children_leases.insert(key, lease);
        Ok(true)
    }

    async fn delete_connection(
        &self,
        _ctx: &CallCtx,
        connection_ref: ConnectionRef,
        options: MutationOptions,
    ) -> Result<ContextSnapshot, JsonRpcError> {
        let ConnectionRef {
            context_id,
            connection_id,
        } = connection_ref;
        let mut state = self.state.lock().await;
        if let Some(existing) = self.check_mutation_options(&state, &context_id, &options)? {
            return Ok(existing);
        }
        let previous = state.clone();
        let context = state
            .contexts
            .get(&context_id)
            .cloned()
            .ok_or_else(|| not_found("context", &context_id))?;
        let transition = reduce_context(
            &context,
            ContextInput::UserCommand(UserCommand::RemoveConnection {
                connection_id: connection_id.clone(),
            }),
        )
        .map_err(transition_rpc_error)?;
        let result = self.commit_context(&mut state, &context_id, transition);
        self.complete_request(&mut state, &context_id, &options, result.revision);
        remove_connection_debugger_registrations(&mut state, &context_id, &connection_id);
        state
            .pause_children_leases
            .remove(&(context_id.clone(), connection_id.clone()));
        self.persist_or_restore(&mut state, previous)?;
        Ok(result)
    }

    async fn put_breakpoint(
        &self,
        _ctx: &CallCtx,
        context_id: String,
        breakpoint_id: String,
        source_path: String,
        line: u32,
        column: u32,
    ) -> Result<ContextSnapshot, JsonRpcError> {
        validate_breakpoint_id(&breakpoint_id)?;
        if source_path.is_empty() {
            return Err(invalid_params("source path must not be empty"));
        }
        if line == 0 || column == 0 {
            return Err(invalid_params("breakpoint lines and columns are one-based"));
        }
        let lock = self.breakpoint_intent_lock(&context_id).await;
        let _ownership_guard = lock.lock().await;
        self.reject_target_logpoint_collision(&context_id, &breakpoint_id, true, None).await?;
        let runtime_breakpoint = TargetBreakpointSpec {
            id: breakpoint_id.clone(),
            source_url: source_path.clone(),
            line,
            column,
            condition: None,
        };
        let (result, target_debuggers) = {
            let mut state = self.state.lock().await;
            let previous = state.clone();
            let context = state
                .contexts
                .get(&context_id)
                .cloned()
                .ok_or_else(|| not_found("context", &context_id))?;
            let transition = reduce_context(
                &context,
                ContextInput::UserCommand(UserCommand::PutBreakpoint {
                    breakpoint_id,
                    source_path,
                    line,
                    column,
                    enabled: true,
                    condition: None,
                    target_selector: None,
                }),
            )
            .map_err(transition_rpc_error)?;
            let result = self.commit_context(&mut state, &context_id, transition);
            self.persist_or_restore(&mut state, previous)?;
            let target_debuggers = state
                .target_debuggers
                .iter()
                .filter(|((candidate_context, _, _), _)| candidate_context == &context_id)
                .map(|(_, debugger)| debugger.clone())
                .collect::<Vec<_>>();
            (result, target_debuggers)
        };
        let has_target_debuggers = !target_debuggers.is_empty();
        for debugger in target_debuggers {
            match debugger
                .set_breakpoint(result.revision, runtime_breakpoint.clone())
                .await
            {
                Ok(_) => {
                    debugger.settle(Duration::from_millis(200)).await;
                }
                Err(TargetDebuggerError::Stopped) => {}
                Err(error) => {
                    return Err(internal_error(format!(
                        "breakpoint intent was persisted, but runtime application failed: {error}"
                    )));
                }
            }
        }
        if has_target_debuggers {
            self.publish_breakpoint_application(&context_id, &runtime_breakpoint.id)
                .await;
        }
        let state = self.state.lock().await;
        Ok(
            service_snapshot(&state, &self.agent_instance_id, &context_id)
                .expect("context still exists after breakpoint application"),
        )
    }

    async fn put_breakpoint_spec(
        &self,
        _ctx: &CallCtx,
        context_id: String,
        breakpoint_id: String,
        specification: BreakpointSpec,
        options: MutationOptions,
    ) -> Result<ContextSnapshot, JsonRpcError> {
        validate_breakpoint_id(&breakpoint_id)?;
        validate_breakpoint_spec(&specification)?;
        {
            let state = self.state.lock().await;
            if let Some(existing) = self.check_mutation_options(&state, &context_id, &options)? {
                return Ok(existing);
            }
        }
        let lock = self.breakpoint_intent_lock(&context_id).await;
        let _ownership_guard = lock.lock().await;
        self.reject_target_logpoint_collision(
            &context_id,
            &breakpoint_id,
            specification.enabled,
            specification.target_selector.as_deref(),
        ).await?;
        let runtime_breakpoint = TargetBreakpointSpec {
            id: breakpoint_id.clone(),
            source_url: specification.source_path.clone(),
            line: specification.line,
            column: specification.column,
            condition: specification.condition.clone(),
        };
        let (result, target_debuggers) = {
            let mut state = self.state.lock().await;
            if let Some(existing) = self.check_mutation_options(&state, &context_id, &options)? {
                return Ok(existing);
            }
            let previous = state.clone();
            let context = state
                .contexts
                .get(&context_id)
                .cloned()
                .ok_or_else(|| not_found("context", &context_id))?;
            let transition = reduce_context(
                &context,
                ContextInput::UserCommand(UserCommand::PutBreakpoint {
                    breakpoint_id,
                    source_path: specification.source_path,
                    line: specification.line,
                    column: specification.column,
                    enabled: specification.enabled,
                    condition: specification.condition,
                    target_selector: specification.target_selector.clone(),
                }),
            )
            .map_err(transition_rpc_error)?;
            let result = self.commit_context(&mut state, &context_id, transition);
            self.complete_request(&mut state, &context_id, &options, result.revision);
            self.persist_or_restore(&mut state, previous)?;
            let target_debuggers = state
                .target_debuggers
                .iter()
                .filter(|((candidate_context, _, _), _)| candidate_context == &context_id)
                .map(|((_, _, target_id), debugger)| (target_id.clone(), debugger.clone()))
                .collect::<Vec<_>>();
            (result, target_debuggers)
        };
        let has_target_debuggers = !target_debuggers.is_empty();
        for (target_id, debugger) in target_debuggers {
            let applies_to_target = breakpoint_applies_to_target(
                specification.enabled,
                specification.target_selector.as_deref(),
                &target_id,
            );
            if applies_to_target {
                debugger
                    .set_breakpoint(result.revision, runtime_breakpoint.clone())
                    .await
                    .map_err(target_debugger_rpc_error)?;
            } else {
                debugger
                    .remove_breakpoint(result.revision, runtime_breakpoint.id.clone())
                    .await
                    .map_err(target_debugger_rpc_error)?;
            }
        }
        if has_target_debuggers {
            self.publish_breakpoint_application(&context_id, &runtime_breakpoint.id)
                .await;
        }
        let state = self.state.lock().await;
        Ok(
            service_snapshot(&state, &self.agent_instance_id, &context_id)
                .expect("context still exists after breakpoint application"),
        )
    }

    async fn delete_breakpoint(
        &self,
        _ctx: &CallCtx,
        context_id: String,
        breakpoint_id: String,
        options: MutationOptions,
    ) -> Result<ContextSnapshot, JsonRpcError> {
        let lock = self.breakpoint_intent_lock(&context_id).await;
        let _ownership_guard = lock.lock().await;
        let (result, target_debuggers) = {
            let mut state = self.state.lock().await;
            if let Some(existing) = self.check_mutation_options(&state, &context_id, &options)? {
                return Ok(existing);
            }
            let previous = state.clone();
            let context = state
                .contexts
                .get(&context_id)
                .cloned()
                .ok_or_else(|| not_found("context", &context_id))?;
            if breakpoint_id.starts_with("log:") && !context.breakpoints.contains_key(&breakpoint_id) {
                return Err(invalid_params(
                    "target logpoints are target-scoped; use `target logpoint delete <id>` with target scope",
                ));
            }
            let transition = reduce_context(
                &context,
                ContextInput::UserCommand(UserCommand::RemoveBreakpoint {
                    breakpoint_id: breakpoint_id.clone(),
                }),
            )
            .map_err(transition_rpc_error)?;
            let result = self.commit_context(&mut state, &context_id, transition);
            self.complete_request(&mut state, &context_id, &options, result.revision);
            self.persist_or_restore(&mut state, previous)?;
            let target_debuggers = state
                .target_debuggers
                .iter()
                .filter(|((candidate_context, _, _), _)| candidate_context == &context_id)
                .map(|(_, debugger)| debugger.clone())
                .collect::<Vec<_>>();
            (result, target_debuggers)
        };
        for debugger in target_debuggers {
            debugger
                .remove_breakpoint(result.revision, breakpoint_id.clone())
                .await
                .map_err(target_debugger_rpc_error)?;
        }
        Ok(result)
    }
}

impl DebuggerService {
    pub(super) async fn breakpoint_intent_lock(&self, context_id: &str) -> Arc<Mutex<()>> {
        let mut locks = self.breakpoint_intent_locks.lock().await;
        locks.retain(|_, lock| lock.strong_count() > 0);
        let key = context_id.to_owned();
        if let Some(lock) = locks.get(&key).and_then(std::sync::Weak::upgrade) {
            return lock;
        }
        let lock = Arc::new(Mutex::new(()));
        locks.insert(key, Arc::downgrade(&lock));
        lock
    }

    async fn reject_target_logpoint_collision(
        &self,
        context_id: &str,
        breakpoint_id: &str,
        enabled: bool,
        target_selector: Option<&str>,
    ) -> Result<(), JsonRpcError> {
        if !breakpoint_id.starts_with("log:") || !enabled {
            return Ok(());
        }
        let debuggers = {
            let state = self.state.lock().await;
            state.target_debuggers.iter()
                .filter(|((candidate, _, target_id), _)| candidate == context_id
                    && breakpoint_applies_to_target(enabled, target_selector, target_id))
                .map(|(_, debugger)| debugger.clone())
                .collect::<Vec<_>>()
        };
        for debugger in debuggers {
            if debugger.owns_logpoint(breakpoint_id.to_owned()).await.map_err(target_debugger_rpc_error)? {
                return Err(invalid_params(format!(
                    "breakpoint {breakpoint_id} belongs to a target logpoint"
                )));
            }
        }
        Ok(())
    }
}
