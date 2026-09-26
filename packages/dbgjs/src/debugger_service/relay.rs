use super::*;

#[async_trait::async_trait]
impl RelayApi for DebuggerService {
    async fn open_playwright_proxy(
        &self,
        _ctx: &CallCtx,
        target_ref: TargetRef,
        expected_generation: u64,
    ) -> Result<PlaywrightProxyEndpoint, JsonRpcError> {
        let TargetRef {
            connection:
                ConnectionRef {
                    context_id,
                    connection_id,
                },
            target_id,
        } = target_ref;
        let (target_id, runtime, browser_context_id) = {
            let state = self.state.lock().await;
            let target_id =
                Self::resolve_target_id_in_state(&state, &context_id, &connection_id, &target_id)?;
            let connection = state
                .contexts
                .get(&context_id)
                .ok_or_else(|| not_found("context", &context_id))?
                .connections
                .get(&connection_id)
                .ok_or_else(|| not_found("connection", &connection_id))?;
            if connection.generation != expected_generation {
                return Err(invalid_state(
                    "selected connection generation is stale; resolve the target again",
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
            if target.target.target_type != "page" {
                return Err(invalid_params(format!(
                    "Playwright requires a page target, but '{target_id}' has type '{}'",
                    target.target.target_type
                )));
            }
            let runtime = state
                .runtimes
                .get(&(context_id.clone(), connection_id.clone()))
                .cloned()
                .ok_or_else(|| invalid_state("connection is not connected"))?;
            (target_id, runtime, target.target.browser_context_id)
        };

        let id = random_instance_id().map_err(|error| internal_error(error.to_string()))?;
        let relay = crate::context_relay::start_connection_relay(
            self.clone(),
            context_id.clone(),
            connection_id.clone(),
            format!("{id}-relay"),
        )
        .await
        .map_err(|error| internal_error(error.to_string()))?;
        let proxy = match crate::playwright_proxy::start(
            crate::playwright_proxy::PlaywrightCdpSource::BrowserRoot {
                endpoint: relay.websocket_url.clone(),
            },
            crate::playwright_proxy::PlaywrightPageScope {
                target_id: target_id.clone(),
                browser_context_id,
            },
            id.clone(),
        )
        .await
        {
            Ok(proxy) => proxy,
            Err(error) => {
                relay.cancel.send_replace(true);
                return Err(internal_error(error.to_string()));
            }
        };
        let crate::context_relay::RelaySession {
            cancel: relay_cancel,
            completion: relay_completion,
            ..
        } = relay;
        let crate::playwright_proxy::PlaywrightProxy {
            websocket_url,
            cancel,
            completion,
        } = proxy;
        let mut cancellation = cancel.subscribe();
        let relay_cancellation = relay_cancel.clone();
        tokio::spawn(async move {
            if cancellation.changed().await.is_ok() && *cancellation.borrow() {
                relay_cancellation.send_replace(true);
            }
        });
        let (closed_sender, closed_receiver) = watch::channel(None);
        {
            let mut state = self.state.lock().await;
            let is_current = state
                .runtimes
                .get(&(context_id.clone(), connection_id.clone()))
                .is_some_and(|current| Arc::ptr_eq(current, &runtime))
                && state
                    .contexts
                    .get(&context_id)
                    .and_then(|context| context.connections.get(&connection_id))
                    .is_some_and(|connection| connection.generation == expected_generation)
                && context_connection_target(
                    &state,
                    &context_id,
                    &connection_id,
                    expected_generation,
                    &target_id,
                )
                .is_some();
            if !is_current {
                cancel.send_replace(true);
                return Err(invalid_state(
                    "selected target changed while the Playwright proxy was opening",
                ));
            }
            state.playwright_proxies.insert(
                id.clone(),
                PlaywrightProxyRegistration {
                    context_id: context_id.clone(),
                    connection_id: connection_id.clone(),
                    target_id: target_id.clone(),
                    generation: expected_generation,
                    cancel: cancel.clone(),
                    closed: closed_receiver,
                },
            );
        }
        let service = self.clone();
        let cleanup_id = id.clone();
        tokio::spawn(async move {
            let result = completion
                .await
                .unwrap_or_else(|_| Err("proxy task ended without a result".into()));
            relay_cancel.send_replace(true);
            let result = match tokio::time::timeout(Duration::from_secs(2), relay_completion).await
            {
                Ok(Ok(())) => result,
                _ => Err(match result {
                    Ok(()) => "context relay cleanup exceeded its deadline".to_owned(),
                    Err(error) => format!("{error}; context relay cleanup exceeded its deadline"),
                }),
            };
            closed_sender.send_replace(Some(result.clone()));
            if result.is_err() {
                tokio::time::sleep(Duration::from_secs(30)).await;
            }
            service
                .state
                .lock()
                .await
                .playwright_proxies
                .remove(&cleanup_id);
        });
        Ok(PlaywrightProxyEndpoint {
            id,
            websocket_url,
            connection_generation: expected_generation,
        })
    }

    async fn close_playwright_proxy(
        &self,
        _ctx: &CallCtx,
        proxy_id: String,
    ) -> Result<bool, JsonRpcError> {
        let registration = self.state.lock().await.playwright_proxies.remove(&proxy_id);
        if let Some(mut registration) = registration {
            registration.cancel.send_replace(true);
            tokio::time::timeout(Duration::from_secs(2), async {
                loop {
                    if let Some(result) = registration.closed.borrow_and_update().clone() {
                        break result;
                    }
                    registration
                        .closed
                        .changed()
                        .await
                        .map_err(|_| "proxy task ended without a result".to_owned())?;
                }
            })
            .await
            .map_err(|_| internal_error("Playwright proxy cleanup exceeded its deadline"))?
            .map_err(|error| internal_error(format!("Playwright proxy failed: {error}")))?;
            Ok(true)
        } else {
            Ok(false)
        }
    }

    async fn open_context_relay(
        &self,
        _ctx: &CallCtx,
        context_id: String,
    ) -> Result<RelayEndpoint, JsonRpcError> {
        let relay_lifecycle_guard = self.relay_lifecycle_lock.lock().await;
        {
            let state = self.state.lock().await;
            if !state.contexts.contains_key(&context_id) {
                return Err(not_found("context", &context_id));
            }
            ensure_context_not_relayed(&state, &context_id)?;
        }
        let id = random_instance_id().map_err(|error| internal_error(error.to_string()))?;
        let relay =
            crate::context_relay::start_context_relay(self.clone(), context_id.clone(), id.clone())
                .await
                .map_err(|error| internal_error(error.to_string()))?;
        self.register_relay(&relay_lifecycle_guard, id, context_id, relay)
            .await
    }

    async fn open_target_relay(
        &self,
        ctx: &CallCtx,
        target_ref: TargetRef,
    ) -> Result<RelayEndpoint, JsonRpcError> {
        let TargetRef {
            connection:
                ConnectionRef {
                    context_id,
                    connection_id,
                },
            target_id,
        } = target_ref;
        let relay_lifecycle_guard = self.relay_lifecycle_lock.lock().await;
        {
            let state = self.state.lock().await;
            if !state.contexts.contains_key(&context_id) {
                return Err(not_found("context", &context_id));
            }
            ensure_context_not_relayed(&state, &context_id)?;
        }
        // Attach eagerly (bypassing the guard we are about to install) so an unresolvable
        // target or a provider failure surfaces synchronously, before any listener is bound.
        let attachment = self
            .attach_target_internal(
                ctx,
                context_id.clone(),
                connection_id.clone(),
                target_id,
                TargetAttachOptions::default(),
            )
            .await?;
        let target_id = attachment.target.target_id;

        let id = random_instance_id().map_err(|error| internal_error(error.to_string()))?;
        let relay = crate::context_relay::start_target_relay(
            self.clone(),
            context_id.clone(),
            connection_id,
            target_id,
            id.clone(),
        )
        .await
        .map_err(|error| internal_error(error.to_string()))?;
        self.register_relay(&relay_lifecycle_guard, id, context_id, relay)
            .await
    }

    async fn close_relay(&self, _ctx: &CallCtx, relay_id: String) -> Result<bool, JsonRpcError> {
        let registration = self.state.lock().await.relays.get(&relay_id).cloned();
        if let Some(registration) = registration {
            registration.cancel.send_replace(true);
            Ok(true)
        } else {
            Ok(false)
        }
    }
}
