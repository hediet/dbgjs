use super::*;

#[async_trait::async_trait]
impl SourceApi for DebuggerService {
    async fn set_source_formatting(
        &self,
        _ctx: &CallCtx,
        context_id: String,
        mode: SourceFormattingMode,
    ) -> Result<SourceFormattingSettings, JsonRpcError> {
        self.update_source_formatting(&context_id, UserCommand::SetSourceFormatting { mode })
            .await
    }

    async fn add_source_formatting_rule(
        &self,
        _ctx: &CallCtx,
        context_id: String,
        mode: SourceFormattingMode,
        target_pattern: Option<String>,
        url_pattern: Option<String>,
    ) -> Result<SourceFormattingSettings, JsonRpcError> {
        if target_pattern.is_none() && url_pattern.is_none() {
            return Err(invalid_params(
                "a formatting rule requires --target, --url, or both",
            ));
        }
        validate_formatting_pattern(target_pattern.as_deref())?;
        validate_formatting_pattern(url_pattern.as_deref())?;
        let mut state = self.state.lock().await;
        let previous = state.clone();
        let context = state
            .contexts
            .get(&context_id)
            .cloned()
            .ok_or_else(|| not_found("context", &context_id))?;
        let mut index = 1_u64;
        let rule_id = loop {
            let candidate = format!("fmt-{index}");
            if context
                .source_formatting
                .rules
                .iter()
                .all(|rule| rule.id != candidate)
            {
                break candidate;
            }
            index = index.saturating_add(1);
        };
        let transition = reduce_context(
            &context,
            ContextInput::UserCommand(UserCommand::AddSourceFormattingRule {
                rule: SourceFormattingRule {
                    id: rule_id,
                    mode,
                    target_pattern,
                    url_pattern,
                },
            }),
        )
        .map_err(transition_rpc_error)?;
        let result = self.commit_context(&mut state, &context_id, transition);
        self.persist_or_restore(&mut state, previous)?;
        Ok(result.source_formatting)
    }

    async fn delete_source_formatting_rule(
        &self,
        _ctx: &CallCtx,
        context_id: String,
        rule_id: String,
    ) -> Result<SourceFormattingSettings, JsonRpcError> {
        self.update_source_formatting(
            &context_id,
            UserCommand::RemoveSourceFormattingRule { rule_id },
        )
        .await
    }

    async fn list_sources(
        &self,
        _ctx: &CallCtx,
        context_id: String,
        path: Option<String>,
    ) -> Result<Vec<SourceSnapshotInfo>, JsonRpcError> {
        let state = self.state.lock().await;
        let context = source_context(&state, &context_id)?;
        let mut sources = BTreeMap::new();
        let mut live_debuggers = Vec::new();
        for ((_, connection_id, target_id), debugger) in
            scoped_source_debuggers(&state, &context_id)
        {
            live_debuggers.push((connection_id.clone(), target_id.clone(), debugger.clone()));
            for script in debugger.snapshot().scripts {
                if path.as_ref().is_some_and(|path| !script.url.contains(path)) {
                    continue;
                }
                let authored_sources = match &script.status {
                    crate::api::service_api::TargetScriptStatus::Resolved { authored_sources } => {
                        authored_sources.clone()
                    }
                    _ => Vec::new(),
                };
                sources.insert(
                    (script.url.clone(), connection_id.clone(), target_id.clone()),
                    SourceSnapshotInfo {
                        path: script.url,
                        kind: "runtime".into(),
                        status: match script.status {
                            crate::api::service_api::TargetScriptStatus::Unresolved => "loaded",
                            crate::api::service_api::TargetScriptStatus::Pending => "loading",
                            crate::api::service_api::TargetScriptStatus::Resolved { .. } => "resolved",
                            crate::api::service_api::TargetScriptStatus::Failed { .. } => "failed",
                        }
                        .into(),
                        connection_id: Some(connection_id.clone()),
                        target_id: Some(target_id.clone()),
                        source_map_url: script.source_map_url,
                    },
                );
                for authored in authored_sources {
                    let kind = if authored.ends_with("?formatted") {
                        "formatted"
                    } else {
                        "authored"
                    };
                    sources.insert(
                        (authored.clone(), connection_id.clone(), target_id.clone()),
                        SourceSnapshotInfo {
                            path: authored,
                            kind: kind.into(),
                            status: "resolved".into(),
                            connection_id: Some(connection_id.clone()),
                            target_id: Some(target_id.clone()),
                            source_map_url: None,
                        },
                    );
                }
            }
        }
        for breakpoint in context.breakpoints.values() {
            if path
                .as_ref()
                .is_some_and(|path| !breakpoint.source_path.contains(path))
            {
                continue;
            }
            sources
                .entry((breakpoint.source_path.clone(), String::new(), String::new()))
                .or_insert_with(|| SourceSnapshotInfo {
                    path: breakpoint.source_path.clone(),
                    kind: "intent".into(),
                    status: "known".into(),
                    connection_id: None,
                    target_id: None,
                    source_map_url: None,
                });
        }
        drop(state);
        for (connection_id, target_id, debugger) in live_debuggers {
            for (source_path, kind) in debugger
                .resolved_source_paths()
                .await
                .map_err(target_debugger_rpc_error)?
            {
                if path
                    .as_ref()
                    .is_some_and(|path| !source_path.contains(path))
                {
                    continue;
                }
                sources
                    .entry((
                        source_path.clone(),
                        connection_id.clone(),
                        target_id.clone(),
                    ))
                    .and_modify(|source| {
                        source.kind = kind.clone();
                        source.status = "resolved".to_owned();
                    })
                    .or_insert(SourceSnapshotInfo {
                        path: source_path,
                        kind,
                        status: "resolved".to_owned(),
                        connection_id: Some(connection_id.clone()),
                        target_id: Some(target_id.clone()),
                        source_map_url: None,
                    });
            }
        }
        Ok(sources.into_values().collect())
    }

    async fn show_source_graph(
        &self,
        _ctx: &CallCtx,
        context_id: String,
    ) -> Result<CompactedSourceGraphSnapshot, JsonRpcError> {
        let model = {
            let state = self.state.lock().await;
            source_model_for_context(&state, &context_id)?
        };
        let Some(model) = model else {
            return Ok(CompactedSourceGraphSnapshot {
                roots: Vec::new(),
                nodes: Vec::new(),
                edges: Vec::new(),
            });
        };
        let graph = model.compacted_graph();
        Ok(CompactedSourceGraphSnapshot {
            roots: graph.roots,
            nodes: graph
                .nodes
                .into_iter()
                .map(|node| {
                    let listed_source_paths = if node.sources.len() <= 10 {
                        node.sources
                            .into_iter()
                            .map(|source| {
                                source
                                    .relative_path_from(&node.prefix)
                                    .filter(|path| !path.is_empty())
                                    .unwrap_or_else(|| source.display())
                            })
                            .collect()
                    } else {
                        Vec::new()
                    };
                    CompactedSourceNodeSnapshot {
                        id: node.id,
                        prefix: node.prefix.display(),
                        source_count: u32::try_from(node.source_count).unwrap_or(u32::MAX),
                        snapshot_count: u32::try_from(node.snapshot_count).unwrap_or(u32::MAX),
                        listed_source_paths,
                        runtime_internal: node.runtime_internal,
                    }
                })
                .collect(),
            edges: graph
                .edges
                .into_iter()
                .map(|edge| CompactedSourceEdgeSnapshot {
                    derived: edge.derived,
                    basis: edge.basis,
                    kind: compacted_projection_label(&edge.kind),
                    mapping_count: u32::try_from(edge.mapping_count).unwrap_or(u32::MAX),
                    fan_out: edge.fan_out,
                    suffix_rewrite: edge.suffix_rewrite.map(|rewrite| {
                        SourceSuffixRewriteSnapshot {
                            from: rewrite.from,
                            to: rewrite.to,
                        }
                    }),
                })
                .collect(),
        })
    }

    async fn show_uncompacted_source_graph(
        &self,
        _ctx: &CallCtx,
        context_id: String,
    ) -> Result<UncompactedSourceGraphSnapshot, JsonRpcError> {
        let model = {
            let state = self.state.lock().await;
            source_model_for_context(&state, &context_id)?
        };
        let Some(model) = model else {
            return Ok(UncompactedSourceGraphSnapshot {
                roots: Vec::new(),
                nodes: Vec::new(),
                edges: Vec::new(),
            });
        };
        let graph = model.graph_snapshot();
        let referenced = graph
            .projections
            .iter()
            .map(|projection| projection.basis)
            .collect::<BTreeSet<_>>();
        let mut roots = graph
            .sources
            .iter()
            .map(|source| source.id)
            .filter(|source| !referenced.contains(source))
            .map(|source| source.0)
            .collect::<Vec<_>>();
        if roots.is_empty() {
            roots.extend(graph.sources.iter().map(|source| source.id.0));
        }
        Ok(uncompacted_graph_snapshot(graph, roots))
    }

    async fn show_source_tree(
        &self,
        _ctx: &CallCtx,
        context_id: String,
        kind: SourceTreeKind,
    ) -> Result<SourceTreeSnapshot, JsonRpcError> {
        let (model, debuggers) = {
            let state = self.state.lock().await;
            (
                source_model_for_context(&state, &context_id)?,
                scoped_source_debuggers(&state, &context_id)
                    .map(|(_, debugger)| debugger.clone())
                    .collect::<Vec<_>>(),
            )
        };
        if matches!(
            kind,
            SourceTreeKind::SourceMapped | SourceTreeKind::Formatted | SourceTreeKind::Resolved
        ) {
            let include_unmapped = kind != SourceTreeKind::SourceMapped;
            for result in join_all(
                debuggers
                    .iter()
                    .map(|debugger| debugger.hydrate_sources(include_unmapped)),
            )
            .await
            {
                if let Err(error) = result
                    && !matches!(error, TargetDebuggerError::Stopped)
                {
                    return Err(target_debugger_rpc_error(error));
                }
            }
        }
        let sources = model.map_or_else(Vec::new, |model| match kind {
            SourceTreeKind::Loaded => model.loaded_sources(),
            SourceTreeKind::SourceMapped => model.source_mapped_loaded_sources(),
            SourceTreeKind::Formatted => model.formatted_loaded_sources(),
            SourceTreeKind::Resolved => model.resolved_loaded_sources(),
        });
        Ok(SourceTreeSnapshot {
            kind,
            sources: sources
                .into_iter()
                .map(uncompacted_source_node_snapshot)
                .collect(),
        })
    }

    async fn resolve_sources(
        &self,
        _ctx: &CallCtx,
        context_id: String,
        source: String,
    ) -> Result<UncompactedSourceGraphSnapshot, JsonRpcError> {
        let model = {
            let state = self.state.lock().await;
            source_model_for_context(&state, &context_id)?
        };
        let Some(model) = model else {
            return Ok(UncompactedSourceGraphSnapshot {
                roots: Vec::new(),
                nodes: Vec::new(),
                edges: Vec::new(),
            });
        };
        let selection = model
            .resolve_sources(&source)
            .map_err(|error| invalid_params(error.to_string()))?;
        Ok(uncompacted_graph_snapshot(
            selection.graph,
            selection.roots.into_iter().map(|source| source.0).collect(),
        ))
    }

    async fn show_source(
        &self,
        _ctx: &CallCtx,
        context_id: String,
        path: String,
        options: SourceDisplayOptions,
    ) -> Result<SourceContentSnapshot, JsonRpcError> {
        let (debuggers, formatting) = {
            let state = self.state.lock().await;
            let context = source_context(&state, &context_id)?;
            let debuggers = scoped_source_debuggers(&state, &context_id)
                .map(|((_, _, target_id), debugger)| (target_id.clone(), debugger.clone()))
                .collect::<Vec<_>>();
            (
                debuggers,
                compile_formatting_settings(&context.source_formatting)?,
            )
        };
        let mut original_fallback = None;
        for (target_id, debugger) in debuggers {
            let base_path = path.strip_suffix("?formatted").unwrap_or(&path);
            if options.view == SourceViewPreference::Formatted {
                if let Some(content) = debugger
                    .source_content(format!("{base_path}?formatted"))
                    .await
                    .map_err(target_debugger_rpc_error)?
                {
                    return source_content_range(content, &options);
                }
                continue;
            }
            let original = debugger
                .source_content(base_path.to_owned())
                .await
                .map_err(target_debugger_rpc_error)?;
            let selected_path = match options.view {
                SourceViewPreference::Original => base_path.to_owned(),
                SourceViewPreference::Formatted => unreachable!(),
                SourceViewPreference::Policy if path.ends_with("?formatted") => path.clone(),
                SourceViewPreference::Policy => {
                    let mode = effective_formatting_mode(&formatting, &target_id, base_path);
                    if mode == SourceFormattingMode::On
                        || mode == SourceFormattingMode::Auto
                            && original
                                .as_ref()
                                .is_some_and(|source| appears_minified(base_path, &source.content))
                    {
                        format!("{base_path}?formatted")
                    } else {
                        base_path.to_owned()
                    }
                }
            };
            if selected_path == base_path {
                if let Some(content) = original {
                    return source_content_range(content, &options);
                }
                continue;
            }
            if let Some(content) = debugger
                .source_content(selected_path)
                .await
                .map_err(target_debugger_rpc_error)?
            {
                return source_content_range(content, &options);
            }
            if options.view == SourceViewPreference::Policy && original_fallback.is_none() {
                original_fallback = original;
            }
        }
        if let Some(content) = original_fallback {
            return source_content_range(content, &options);
        }
        if options.view == SourceViewPreference::Formatted {
            return Err(not_found("formatted source", &path));
        }
        if path.is_empty() {
            return Err(not_found("source", "<empty>"));
        }
        let file_path = source_file_path(&path)?;
        let content = fs::read_to_string(&file_path).map_err(|error| {
            internal_error(format!(
                "failed to read source '{}': {error}",
                file_path.display()
            ))
        })?;
        source_content_range(
            SourceContentSnapshot {
                path,
                total_lines: content.lines().count() as u32,
                start_line: 1,
                end_line: content.lines().count() as u32,
                content,
            },
            &options,
        )
    }

    async fn grep_sources(
        &self,
        _ctx: &CallCtx,
        context_id: String,
        options: SourceSearchOptions,
    ) -> Result<SourceSearchSnapshot, JsonRpcError> {
        if options.pattern.is_empty() {
            return Err(invalid_params("source grep pattern must not be empty"));
        }
        if options.max_results == 0 {
            return Err(invalid_params("source grep max_results must be positive"));
        }
        if options.timeout_ms == Some(0) {
            return Err(invalid_params("source grep timeout_ms must be positive"));
        }
        let query = SearchQuery {
            pattern: options.pattern.clone(),
            regex: options.regex,
            case_sensitive: options.case_sensitive,
            max_results: options.max_results as usize,
            context_lines: options.context_lines as usize,
        };
        crate::source::source_search::validate(&query).map_err(source_search_error)?;
        let deadline = options.timeout_ms.and_then(|milliseconds| {
            std::time::Instant::now().checked_add(Duration::from_millis(milliseconds))
        });
        let control = deadline.map_or_else(SearchControl::default, SearchControl::with_deadline);
        let mut cancellation = SearchCancellationGuard::new(control.cancellation_flag());
        let (debuggers, local_sources, formatting) = {
            let state = self.state.lock().await;
            let context = source_context(&state, &context_id)?;
            let debuggers = scoped_source_debuggers(&state, &context_id)
                .map(|((_, connection_id, target_id), debugger)| {
                    (connection_id.clone(), target_id.clone(), debugger.clone())
                })
                .collect::<Vec<_>>();
            let local_sources = context
                .breakpoints
                .values()
                .map(|breakpoint| breakpoint.source_path.clone())
                .filter(|path| {
                    options
                        .path
                        .as_ref()
                        .is_none_or(|selector| path.contains(selector))
                })
                .collect::<BTreeSet<_>>();
            (
                debuggers,
                local_sources,
                compile_formatting_settings(&context.source_formatting)?,
            )
        };

        let path_selector = options.path.clone();
        let batches = stream::iter(debuggers.into_iter().map(
            |(connection_id, target_id, debugger)| {
                let path_selector = path_selector.clone();
                let batch_control = control.clone();
                async move {
                    debugger
                        .source_search_batch(path_selector, batch_control)
                        .await
                        .map(|batch| (connection_id, target_id, batch))
                }
            },
        ))
        .buffer_unordered(8)
        .collect::<Vec<_>>();
        let batches = match deadline {
            Some(deadline) => timeout_at(Instant::from_std(deadline), batches)
                .await
                .map_err(|_| source_search_error(SearchError::DeadlineExceeded))?,
            None => batches.await,
        };
        let mut batches = batches
            .into_iter()
            .collect::<Result<Vec<_>, _>>()
            .map_err(target_source_search_rpc_error)?;
        batches.sort_by(|left, right| (&left.0, &left.1).cmp(&(&right.0, &right.1)));

        let mut documents = Vec::new();
        let mut skipped = Vec::new();
        for (connection_id, target_id, mut batch) in batches {
            debug_assert_eq!(batch.skipped_sources as usize, batch.skipped.len());
            skipped.extend(batch.skipped.into_iter().map(|mut source| {
                source.connection_id = Some(connection_id.clone());
                source.target_id = Some(target_id.clone());
                source
            }));
            select_source_views(&mut batch.sources, &formatting, &target_id, options.view);
            documents.extend(batch.sources.into_iter().map(|source| SearchDocument {
                identity: SourceIdentity {
                    path: source.path,
                    connection_id: Some(connection_id.clone()),
                    target_id: Some(target_id.clone()),
                    kind: source.kind,
                    provenance: source.provenance,
                },
                content_hash: source.content_hash,
                content: source.content,
            }));
        }

        let worker_control = control.clone();
        let worker = tokio::task::spawn_blocking(move || {
            let mut skipped_local = Vec::new();
            for path in local_sources {
                worker_control.check()?;
                let content = match source_file_path(&path)
                    .map_err(|error| error.message)
                    .and_then(|file_path| {
                        fs::read_to_string(file_path).map_err(|error| error.to_string())
                    }) {
                    Ok(content) => Arc::<str>::from(content),
                    Err(reason) => {
                        skipped_local.push(crate::api::service_api::SourceSearchSkip {
                            path,
                            kind: "intent".to_owned(),
                            connection_id: None,
                            target_id: None,
                            reason,
                        });
                        continue;
                    }
                };
                documents.push(SearchDocument {
                    identity: SourceIdentity {
                        path,
                        connection_id: None,
                        target_id: None,
                        kind: "intent".to_owned(),
                        provenance: "local file".to_owned(),
                    },
                    content_hash: crate::capture::content_store::ContentHash::try_of_bytes(
                        content.as_bytes(),
                        || worker_control.check(),
                    )?,
                    content,
                });
            }
            crate::source::source_search::search(documents, &query, &worker_control)
                .map(|result| (result, skipped_local))
        });
        let (result, skipped_local) = match deadline {
            Some(deadline) => timeout_at(Instant::from_std(deadline), worker)
                .await
                .map_err(|_| source_search_error(SearchError::DeadlineExceeded))?
                .map_err(|error| internal_error(format!("source search worker failed: {error}")))?
                .map_err(source_search_error)?,
            None => worker
                .await
                .map_err(|error| internal_error(format!("source search worker failed: {error}")))?
                .map_err(source_search_error)?,
        };
        cancellation.disarm();
        skipped.extend(skipped_local);
        let skipped_sources = skipped.len().min(u32::MAX as usize) as u32;
        let matches = result
            .hits
            .into_iter()
            .map(|hit| SourceMatchSnapshot {
                path: hit.identity.path,
                content_hash: hit.content_hash.to_string(),
                kind: hit.identity.kind,
                provenance: hit.identity.provenance,
                connection_id: hit.identity.connection_id,
                target_id: hit.identity.target_id,
                line: hit.line,
                column: hit.column,
                match_length: hit.match_length,
                text: hit.text,
                before_context: hit.before_context,
                after_context: hit.after_context,
                excerpt_start_column: None,
                text_truncated: false,
                context_truncated: false,
            })
            .collect::<Vec<_>>();
        Ok(SourceSearchSnapshot {
            omitted_matches: result.total_matches.saturating_sub(matches.len() as u64),
            output_omitted_matches: 0,
            matches,
            searched_sources: result.searched_sources,
            searched_contents: result.searched_contents,
            skipped_sources,
            output_truncated: false,
            omitted_diagnostics: 0,
            skipped,
        })
    }

    async fn explain_source(
        &self,
        _ctx: &CallCtx,
        context_id: String,
        path: String,
    ) -> Result<Vec<SourceGraphViewSnapshot>, JsonRpcError> {
        let debuggers = self.source_debuggers(&context_id).await?;
        let mut explanations = Vec::new();
        for (connection_id, target_id, debugger) in debuggers {
            let mut target_explanations = debugger
                .explain_source(path.clone())
                .await
                .map_err(target_debugger_rpc_error)?;
            for explanation in &mut target_explanations {
                explanation.connection_id = connection_id.clone();
                explanation.target_id = target_id.clone();
            }
            explanations.extend(target_explanations);
        }
        explanations.sort_by(|left, right| {
            (
                &left.connection_id,
                &left.target_id,
                &left.generated_url,
                &left.source_path,
            )
                .cmp(&(
                    &right.connection_id,
                    &right.target_id,
                    &right.generated_url,
                    &right.source_path,
                ))
        });
        Ok(explanations)
    }

    async fn map_source(
        &self,
        _ctx: &CallCtx,
        context_id: String,
        path: String,
        line: u32,
        column: u32,
    ) -> Result<Vec<SourceMappingSnapshot>, JsonRpcError> {
        if line == 0 || column == 0 {
            return Err(invalid_params("source locations are one-based"));
        }
        let debuggers = self.source_debuggers(&context_id).await?;
        let mut locations = Vec::new();
        for (connection_id, target_id, debugger) in debuggers {
            let mut target_locations = debugger
                .map_source(path.clone(), line, column)
                .await
                .map_err(target_debugger_rpc_error)?;
            for location in &mut target_locations {
                location.connection_id = connection_id.clone();
                location.target_id = target_id.clone();
            }
            locations.extend(target_locations);
        }
        locations.sort_by(|left, right| {
            (
                &left.connection_id,
                &left.target_id,
                &left.source_url,
                left.line,
                left.column,
                &left.direction,
            )
                .cmp(&(
                    &right.connection_id,
                    &right.target_id,
                    &right.source_url,
                    right.line,
                    right.column,
                    &right.direction,
                ))
        });
        locations.dedup();
        Ok(locations)
    }

    async fn evict_source_caches(
        &self,
        _ctx: &CallCtx,
        context_id: String,
    ) -> Result<u32, JsonRpcError> {
        let debuggers = self.source_debuggers(&context_id).await?;
        for (_, _, debugger) in &debuggers {
            debugger
                .evict_source_caches()
                .await
                .map_err(target_debugger_rpc_error)?;
        }
        Ok(debuggers.len() as u32)
    }

    async fn export_sources(
        &self,
        ctx: &CallCtx,
        context_id: String,
        destination: String,
    ) -> Result<Vec<String>, JsonRpcError> {
        let destination = PathBuf::from(destination);
        fs::create_dir_all(&destination).map_err(|error| {
            internal_error(format!(
                "failed to create source export directory '{}': {error}",
                destination.display()
            ))
        })?;
        let sources = self.list_sources(ctx, context_id.clone(), None).await?;
        let mut exported = Vec::new();
        for source in sources {
            let Ok(content) = self
                .show_source(
                    ctx,
                    context_id.clone(),
                    source.path.clone(),
                    SourceDisplayOptions {
                        line: None,
                        context_lines: 0,
                        view: SourceViewPreference::Policy,
                    },
                )
                .await
            else {
                continue;
            };
            let source_path = source_file_path(&source.path).unwrap_or_else(|_| {
                PathBuf::from(source.path.rsplit('/').next().unwrap_or("source.js"))
            });
            let name = format!(
                "{:016x}-{}",
                stable_name_hash(&source.path),
                sanitize_file_name(
                    source_path
                        .file_name()
                        .and_then(|name| name.to_str())
                        .unwrap_or("source.js")
                )
            );
            let output = destination.join(name);
            let mut file = AtomicWriteFile::open(&output)
                .map_err(|error| internal_error(error.to_string()))?;
            file.write_all(content.content.as_bytes())
                .map_err(|error| internal_error(error.to_string()))?;
            file.commit()
                .map_err(|error| internal_error(error.to_string()))?;
            exported.push(output.to_string_lossy().into_owned());
        }
        Ok(exported)
    }
}

impl DebuggerService {
    async fn source_debuggers(
        &self,
        context_id: &str,
    ) -> Result<Vec<(String, String, TargetDebuggerHandle)>, JsonRpcError> {
        let state = self.state.lock().await;
        source_context(&state, context_id)?;
        Ok(scoped_source_debuggers(&state, context_id)
            .map(|((_, connection_id, target_id), debugger)| {
                (connection_id.clone(), target_id.clone(), debugger.clone())
            })
            .collect())
    }
}

fn source_context<'a>(
    state: &'a ServiceState,
    context_id: &str,
) -> Result<&'a Arc<ContextState>, JsonRpcError> {
    state
        .contexts
        .get(context_id)
        .ok_or_else(|| not_found("context", context_id))
}

fn source_model_for_context(
    state: &ServiceState,
    context_id: &str,
) -> Result<Option<Arc<ContextSourceModel>>, JsonRpcError> {
    source_context(state, context_id)?;
    Ok(state.source_models.get(context_id).cloned())
}

fn scoped_source_debuggers<'a>(
    state: &'a ServiceState,
    context_id: &'a str,
) -> impl Iterator<Item = (&'a (String, String, String), &'a TargetDebuggerHandle)> + 'a {
    state
        .target_debuggers
        .iter()
        .filter(move |((candidate_context, _, _), _)| candidate_context == context_id)
}
