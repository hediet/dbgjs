use super::*;

#[async_trait::async_trait]
impl ServiceApi for DebuggerService {
    async fn service_info(&self, _ctx: &CallCtx) -> Result<ServiceInfo, JsonRpcError> {
        Ok(ServiceInfo {
            process_id: std::process::id(),
            agent_instance_id: self.agent_instance_id.clone(),
        })
    }

    async fn discover_vscode_process_trees(
        &self,
        _ctx: &CallCtx,
    ) -> Result<Vec<ProcessTreeSnapshot>, JsonRpcError> {
        crate::connection::discovery::process_discovery::discover_vscode_process_trees(false)
            .await
            .map_err(|error| internal_error(error.to_string()))
    }

    async fn get_process_projection(
        &self,
        _ctx: &CallCtx,
        context_id: String,
        expanded_root_process_ids: Vec<u32>,
    ) -> Result<Vec<ProcessTreeSnapshot>, JsonRpcError> {
        let expanded = expanded_root_process_ids
            .into_iter()
            .collect::<BTreeSet<_>>();
        let mut trees = crate::connection::discovery::process_discovery::discover_recognized_process_trees()
            .await
            .map_err(|error| internal_error(error.to_string()))?;
        let mut covered_processes = BTreeSet::new();
        trees.retain(|tree| {
            if covered_processes.contains(&tree.root_process_id) {
                return false;
            }
            covered_processes.extend(tree.processes.iter().map(|process| process.process_id));
            true
        });
        for tree in &mut trees {
            if expanded.contains(&tree.root_process_id) {
                crate::connection::discovery::process_discovery::populate_process_tree_targets(std::slice::from_mut(tree))
                    .await;
            }
        }
        let mut state = self.state.lock().await;
        if !state.contexts.contains_key(&context_id) {
            return Err(not_found("context", &context_id));
        }
        if state.process_projections.get(&context_id) != Some(&trees) {
            stage_process_resource_graph(&mut state, &context_id, &trees)
                .map_err(internal_error)?;
            state.process_projections.insert(context_id, trees.clone());
        }
        Ok(trees)
    }

    async fn shutdown(&self, _ctx: &CallCtx) -> Result<bool, JsonRpcError> {
        let service = self.clone();
        tokio::spawn(async move {
            let runtimes = {
                let mut state = service.state.lock().await;
                state.target_debuggers.clear();
                state.debug_attachments.clear();
                state.pause_children_leases.clear();
                for registration in std::mem::take(&mut state.playwright_proxies).into_values() {
                    registration.cancel.send_replace(true);
                }
                for registration in std::mem::take(&mut state.relays).into_values() {
                    registration.cancel.send_replace(true);
                }
                std::mem::take(&mut state.runtimes)
                    .into_values()
                    .collect::<Vec<_>>()
            };
            for runtime in runtimes {
                runtime.close().await;
            }
            tokio::time::sleep(Duration::from_millis(100)).await;
            service.shutdown.send_replace(true);
        });
        Ok(true)
    }
}
