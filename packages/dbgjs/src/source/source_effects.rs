use std::collections::{BTreeMap, BTreeSet};
use std::sync::Arc;
use std::sync::{Mutex, OnceLock};

use rayon::prelude::*;

use crate::capture::content_store::ContentStore;
use crate::service::context_source_model::{ContextSourceModel, SourceContributionId, SourceSnapshotRole};
use crate::debugger::debugger_engine::{
    BreakpointMapping, DebuggerState, Effect, EffectId, Input, ScriptKey, ScriptSourceState,
};
use crate::api::service_api::{SourceGraphViewSnapshot, SourceProjectionPathSnapshot};
use crate::source::source_graph::{RevisionNamespace, SourceRevision, SourceUri};
use crate::source::source_location::{ResolvedSourcePosition, resolve_source_position_with_breadcrumb};
use crate::source::source_search::{HydratedSource, HydratedSourceBatch, SearchControl, SearchError};
use crate::source::source_view::{
    GeneratedSourceInput, LineIndex, MappingQuality, Position, ProjectionStep, Provenance,
    ResolutionPolicy, ResolvedSourceView, SourceViewError, appears_minified, canonical_source_uri,
};

pub struct SourceEffectOptions {
    pub policy: ResolutionPolicy,
    pub workspace: BTreeMap<String, String>,
    pub format_unmapped_sources: bool,
}

impl Default for SourceEffectOptions {
    fn default() -> Self {
        Self {
            policy: ResolutionPolicy::PreferSourcesContent,
            workspace: BTreeMap::new(),
            format_unmapped_sources: true,
        }
    }
}

struct RetainedView {
    script: ScriptKey,
    generated_url: String,
    source_map_url: Option<String>,
    logical_to_canonical: BTreeMap<String, String>,
    canonical_to_logical: BTreeMap<String, String>,
    generated_content: Arc<str>,
    generated_index: Arc<LineIndex>,
    projection_cache: Mutex<BTreeMap<u32, Option<ProjectedOffset>>>,
    symbol_indexes: SymbolIndexCache,
    view: Arc<ResolvedSourceView>,
}

impl RetainedView {
    fn positions_for(&self, content: &str) -> Option<&Arc<LineIndex>> {
        std::ptr::eq(content, self.generated_content.as_ref()).then_some(&self.generated_index)
    }
}

type CachedSymbolIndex = Arc<OnceLock<Option<crate::source::language_intelligence::SymbolIndex>>>;

/// Symbol indexes for one immutable source view. Failed parses are cached too.
#[derive(Default)]
pub struct SymbolIndexCache {
    entries: Mutex<BTreeMap<String, CachedSymbolIndex>>,
}

impl SymbolIndexCache {
    pub(crate) fn breadcrumb(
        &self,
        source_url: &str,
        content: &str,
        line: u32,
        column: u32,
        positions: Option<&Arc<LineIndex>>,
    ) -> Option<String> {
        self.get_or_create(source_url, content, positions)
            .get()
            .and_then(Option::as_ref)
            .and_then(|index| index.breadcrumb(line, column))
    }

    fn get_or_create(
        &self,
        source_url: &str,
        content: &str,
        positions: Option<&Arc<LineIndex>>,
    ) -> CachedSymbolIndex {
        let index = {
            let mut indexes = self.entries.lock().unwrap();
            indexes
                .entry(source_url.to_owned())
                .or_insert_with(|| Arc::new(OnceLock::new()))
                .clone()
        };
        index.get_or_init(|| {
            crate::source::language_intelligence::SymbolIndex::with_positions(
                source_url,
                content,
                positions.cloned(),
            )
        });
        index
    }

    pub fn clear(&self) {
        self.entries.lock().unwrap().clear();
    }
}

#[derive(Clone)]
struct ProjectedOffset {
    source_url: String,
    position: Position,
    content: Arc<str>,
}

struct RuntimeSourceObservation {
    contribution: SourceContributionId,
    uri: SourceUri,
    revision: SourceRevision,
}

pub struct SourceEffectInterpreter {
    options: SourceEffectOptions,
    model: Arc<ContextSourceModel>,
    contribution_prefix: String,
    store: Arc<ContentStore>,
    views: BTreeMap<EffectId, RetainedView>,
    runtime_sources: BTreeMap<ScriptKey, RuntimeSourceObservation>,
}

impl SourceEffectInterpreter {
    pub fn new(
        options: SourceEffectOptions,
        model: Arc<ContextSourceModel>,
        contribution_prefix: impl Into<String>,
    ) -> Self {
        let store = model.content_store().clone();
        Self {
            options,
            model,
            contribution_prefix: contribution_prefix.into(),
            store,
            views: BTreeMap::new(),
            runtime_sources: BTreeMap::new(),
        }
    }

    pub fn interpret(&mut self, effect: &Effect) -> Result<Option<Input>, SourceEffectError> {
        match effect {
            Effect::BuildSourceView {
                effect_id,
                script,
                generated_url,
                content,
                source_map,
                source_map_url,
                ..
            } => {
                let mut view = ResolvedSourceView::new(
                    self.options.policy,
                    self.model.clone(),
                    SourceContributionId::new(format!(
                        "{}/{}/{}/{}",
                        self.contribution_prefix,
                        script.session.session_id,
                        script.script_id,
                        effect_id.0
                    )),
                    self.options.workspace.clone(),
                );
                view.add_generated_with_prepared_map(
                    GeneratedSourceInput {
                        url: generated_url,
                        content,
                        source_map: None,
                        source_map_url: source_map_url.as_deref(),
                        minified: source_map.is_none()
                            && self.options.format_unmapped_sources
                            && appears_minified(generated_url, content),
                    },
                    source_map.as_ref(),
                )?;
                let logical_sources = view
                    .files()
                    .iter()
                    .map(|(url, file)| (url.clone(), file.primary.clone()))
                    .collect();
                let logical_to_canonical = view
                    .files()
                    .keys()
                    .map(|path| {
                        (
                            path.clone(),
                            canonical_source_uri(source_map_url.as_deref(), path).display(),
                        )
                    })
                    .collect::<BTreeMap<_, _>>();
                let canonical_to_logical = logical_to_canonical
                    .iter()
                    .map(|(logical, canonical)| (canonical.clone(), logical.clone()))
                    .collect();
                self.views.insert(
                    *effect_id,
                    RetainedView {
                        script: script.clone(),
                        generated_url: generated_url.clone(),
                        source_map_url: source_map_url.clone(),
                        logical_to_canonical,
                        canonical_to_logical,
                        generated_content: content.clone(),
                        generated_index: Arc::new(LineIndex::new(content)),
                        projection_cache: Mutex::new(BTreeMap::new()),
                        symbol_indexes: SymbolIndexCache::default(),
                        view: Arc::new(view),
                    },
                );
                Ok(Some(Input::SourceViewBuilt {
                    effect_id: *effect_id,
                    logical_sources,
                }))
            }
            Effect::MapBreakpoint {
                effect_id,
                script,
                view_id,
                source_url,
                position,
                ..
            } => {
                let retained = self.view_for(*view_id, script)?;
                let mappings = if source_url == &retained.generated_url {
                    vec![BreakpointMapping {
                        generated_position: *position,
                        quality: "exact".to_owned(),
                        generated_url: retained.generated_url.clone(),
                        projection: vec!["identity".to_owned()],
                    }]
                } else {
                    retained
                    .view
                    .reverse(source_url, *position)
                    .into_iter()
                    .filter(|candidate| candidate.source_url == retained.generated_url)
                    .map(|candidate| BreakpointMapping {
                        generated_position: candidate.position,
                        quality: mapping_quality_label(candidate.quality).to_owned(),
                        generated_url: candidate.projection.generated_url.clone(),
                        projection: candidate
                            .projection
                            .steps
                            .iter()
                            .map(projection_step_label)
                            .collect(),
                    })
                    .collect::<Vec<_>>()
                };
                let mappings = mappings
                    .into_iter()
                    .fold(BTreeMap::new(), |mut result, mapping| {
                        result.entry(mapping.generated_position).or_insert(mapping);
                        result
                    })
                    .into_values()
                    .collect();
                Ok(Some(Input::BreakpointMappingAssessed {
                    effect_id: *effect_id,
                    mappings,
                }))
            }
            Effect::MapFrame {
                effect_id,
                script,
                view_id,
                position,
                ..
            } => {
                let retained = self.view_for(*view_id, script)?;
                let mapped = retained
                    .view
                    .forward(&retained.generated_url, *position)
                    .into_iter()
                    .next()
                    .ok_or_else(|| {
                        retained
                            .view
                            .source_map_location(&retained.generated_url, *position)
                            .map_or(
                                SourceEffectError::UnmappedPosition {
                                    view_id: *view_id,
                                    position: *position,
                                },
                                |(source_url, source_position)| {
                                    SourceEffectError::UnavailableMappedSource {
                                        view_id: *view_id,
                                        position: *position,
                                        source_url,
                                        source_position,
                                    }
                                },
                            )
                    })?;
                Ok(Some(Input::FrameMapped {
                    effect_id: *effect_id,
                    source_url: mapped.source_url,
                    position: mapped.position,
                }))
            }
            _ => Ok(None),
        }
    }

    pub fn retain_for_state(&mut self, state: &DebuggerState) {
        let retained_ids: BTreeSet<_> = state
            .scripts
            .values()
            .filter_map(|script| match &script.source {
                ScriptSourceState::Resolved(view) => Some(view.view_id),
                _ => None,
            })
            .collect();
        self.views
            .retain(|view_id, _| retained_ids.contains(view_id));

        let desired = state
            .scripts
            .iter()
            .filter(|(_, script)| !matches!(script.source, ScriptSourceState::Resolved(_)))
            .map(|(key, script)| {
                let uri = source_uri(&script.url, key);
                let revision = SourceRevision::Version {
                    namespace: RevisionNamespace::new("cdp-script")
                        .expect("static revision namespace is valid"),
                    value: if script.hash.is_empty() {
                        format!(
                            "anonymous:{}:{}:{}:{}",
                            key.session.connection_generation,
                            key.session.session_id,
                            key.script_id,
                            script.version
                        )
                    } else {
                        script.hash.clone()
                    },
                };
                (key.clone(), (uri, revision))
            })
            .collect::<BTreeMap<_, _>>();

        let stale = self
            .runtime_sources
            .iter()
            .filter(|(key, observation)| {
                desired.get(*key).is_none_or(|(uri, revision)| {
                    observation.uri != *uri || observation.revision != *revision
                })
            })
            .map(|(key, _)| key.clone())
            .collect::<Vec<_>>();
        for key in stale {
            if let Some(observation) = self.runtime_sources.remove(&key) {
                self.model.release(&observation.contribution);
            }
        }

        for (key, (uri, revision)) in desired {
            if self.runtime_sources.contains_key(&key) {
                continue;
            }
            let contribution = SourceContributionId::new(format!(
                "{}/runtime/{}/{}",
                self.contribution_prefix, key.session.session_id, key.script_id
            ));
            let SourceRevision::Version { namespace, value } = revision.clone() else {
                unreachable!("runtime observations always use provider versions");
            };
            let snapshot = self
                .model
                .intern_version(&contribution, uri.clone(), namespace, value)
                .expect("runtime source revisions are non-empty");
            self.model
                .mark_snapshot_role(&contribution, snapshot, SourceSnapshotRole::Loaded)
                .expect("runtime source snapshot is registered");
            self.runtime_sources.insert(
                key,
                RuntimeSourceObservation {
                    contribution,
                    uri,
                    revision,
                },
            );
        }
    }

    pub fn retained_view_count(&self) -> usize {
        self.views.len()
    }

    pub fn project_generated_offset(
        &self,
        state: &DebuggerState,
        script_key: &ScriptKey,
        utf16_offset: u32,
    ) -> Option<(String, Position, Arc<str>)> {
        let ScriptSourceState::Resolved(source_state) = &state.scripts.get(script_key)?.source
        else {
            return None;
        };
        let retained = self.views.get(&source_state.view_id)?;
        if let Some(cached) = retained
            .projection_cache
            .lock()
            .unwrap()
            .get(&utf16_offset)
            .cloned()
        {
            return cached
                .map(|projected| (projected.source_url, projected.position, projected.content));
        }

        let mapped = std::iter::once_with(|| {
            retained
                .generated_index
                .clamped_utf16_position(utf16_offset)
        })
        .chain(std::iter::once_with(|| {
            retained.generated_index.clamped_byte_position(utf16_offset)
        }))
        .find_map(|position| {
            retained
                .view
                .forward(&retained.generated_url, position)
                .into_iter()
                .next()
        })?;
        let projected = retained
            .view
            .files()
            .get(&mapped.source_url)
            .and_then(|authored| self.store.get(authored.primary.content))
            .map(|content| ProjectedOffset {
                source_url: mapped.source_url,
                position: mapped.position,
                content,
            });
        retained
            .projection_cache
            .lock()
            .unwrap()
            .insert(utf16_offset, projected.clone());
        projected.map(|projected| (projected.source_url, projected.position, projected.content))
    }

    pub fn project_generated_position(
        &self,
        state: &DebuggerState,
        script_key: &ScriptKey,
        position: Position,
    ) -> Option<(String, Position, Arc<str>)> {
        let ScriptSourceState::Resolved(source_state) = &state.scripts.get(script_key)?.source
        else {
            return None;
        };
        let retained = self.views.get(&source_state.view_id)?;
        let (source_url, position, _) = retained
            .view
            .preferred_generated_location(&retained.generated_url, position)?;
        retained
            .view
            .files()
            .get(&source_url)
            .and_then(|authored| self.store.get(authored.primary.content))
            .map(|content| (source_url, position, content))
    }

    pub fn resolve_generated_position(
        &self,
        state: &DebuggerState,
        script_key: &ScriptKey,
        position: Position,
    ) -> Option<ResolvedSourcePosition> {
        let ScriptSourceState::Resolved(source_state) = &state.scripts.get(script_key)?.source
        else {
            return None;
        };
        let retained = self.views.get(&source_state.view_id)?;
        Some(resolve_source_position_with_breadcrumb(
            &retained.view,
            &retained.generated_url,
            retained.source_map_url.as_deref(),
            position,
            |source_url, content, line, column| {
                retained.symbol_indexes.breadcrumb(
                    source_url,
                    content,
                    line,
                    column,
                    retained.positions_for(content),
                )
            },
        ))
    }

    pub fn generated_position(
        &self,
        state: &DebuggerState,
        script_key: &ScriptKey,
        utf16_offset: u32,
    ) -> Option<Position> {
        let ScriptSourceState::Resolved(source_state) = &state.scripts.get(script_key)?.source
        else {
            return None;
        };
        let retained = self.views.get(&source_state.view_id)?;
        Some(
            retained
                .generated_index
                .clamped_utf16_position(utf16_offset),
        )
    }

    pub fn breadcrumb(
        &self,
        state: &DebuggerState,
        script_key: &ScriptKey,
        source_url: &str,
        line: u32,
        column: u32,
        content: &str,
    ) -> Option<String> {
        let ScriptSourceState::Resolved(source_state) = &state.scripts.get(script_key)?.source
        else {
            return None;
        };
        let retained = self.views.get(&source_state.view_id)?;
        retained
            .symbol_indexes
            .breadcrumb(source_url, content, line, column, retained.positions_for(content))
    }

    pub fn prepare_breadcrumbs(
        &self,
        state: &DebuggerState,
        sources: &[(ScriptKey, String, Arc<str>)],
    ) {
        let mut unique = BTreeMap::<(EffectId, String), Arc<str>>::new();
        for (script, source_url, content) in sources {
            let Some(ScriptSourceState::Resolved(source_state)) =
                state.scripts.get(script).map(|script| &script.source)
            else {
                continue;
            };
            unique
                .entry((source_state.view_id, source_url.clone()))
                .or_insert_with(|| content.clone());
        }

        unique
            .into_par_iter()
            .for_each(|((view_id, source_url), content)| {
                if let Some(retained) = self.views.get(&view_id) {
                    retained.symbol_indexes.get_or_create(
                        &source_url,
                        &content,
                        retained.positions_for(&content),
                    );
                }
            });
    }

    pub fn logical_source_content(
        &self,
        state: &DebuggerState,
        script_key: &ScriptKey,
        source_url: &str,
    ) -> Option<Arc<str>> {
        state
            .scripts
            .get(script_key)
            .and_then(|script| match &script.source {
                ScriptSourceState::Resolved(view) => {
                    let retained = self.views.get(&view.view_id)?;
                    let logical_url = authored_lookup_path(retained, source_url)?;
                    view.logical_sources
                        .get(&logical_url)
                        .and_then(|candidate| self.store.get(candidate.content))
                }
                _ => None,
            })
    }

    pub fn explain_source(&self, path: &str) -> Vec<SourceGraphViewSnapshot> {
        let mut explanations = Vec::new();
        for retained in self.views.values() {
            let diagnostics = retained
                .view
                .diagnostics()
                .iter()
                .map(|diagnostic| format!("{diagnostic:?}"))
                .collect::<Vec<_>>();
            if retained.generated_url == path {
                explanations.push(SourceGraphViewSnapshot {
                    connection_id: String::new(),
                    target_id: String::new(),
                    generated_url: retained.generated_url.clone(),
                    source_path: path.to_owned(),
                    role: "generated".to_owned(),
                    kind: "runtime".to_owned(),
                    primary_provenance: format!("runtime source {}", retained.generated_url),
                    alternative_provenance: Vec::new(),
                    projection_paths: Vec::new(),
                    resolved_source_count: retained.view.files().len() as u32,
                    diagnostics: diagnostics.clone(),
                });
            }
            let Some(logical_path) = authored_lookup_path(retained, path) else {
                continue;
            };
            let Some(file) = retained.view.files().get(&logical_path) else {
                continue;
            };
            explanations.push(SourceGraphViewSnapshot {
                connection_id: String::new(),
                target_id: String::new(),
                generated_url: retained.generated_url.clone(),
                source_path: canonical_authored_url(retained, &file.logical_url),
                role: "authored".to_owned(),
                kind: format!("{:?}", file.kind).to_ascii_lowercase(),
                primary_provenance: provenance_label(&file.primary.provenance),
                alternative_provenance: file
                    .alternatives
                    .iter()
                    .map(|candidate| provenance_label(&candidate.provenance))
                    .collect(),
                projection_paths: file
                    .projection_paths
                    .iter()
                    .map(|projection| SourceProjectionPathSnapshot {
                        generated_url: projection.generated_url.clone(),
                        steps: projection.steps.iter().map(projection_step_label).collect(),
                    })
                    .collect(),
                resolved_source_count: retained.view.files().len() as u32,
                diagnostics,
            });
        }
        explanations
    }

    pub fn resolved_source_paths(&self) -> Vec<(String, String)> {
        let mut paths = BTreeSet::new();
        for retained in self.views.values() {
            paths.insert((retained.generated_url.clone(), "runtime".to_owned()));
            paths.extend(
                retained
                    .view
                    .files()
                    .keys()
                    .map(|path| canonical_authored_url(retained, path))
                    .map(|path| (path, "authored".to_owned())),
            );
        }
        paths.into_iter().collect()
    }

    pub fn authored_source_paths(
        &self,
        state: &DebuggerState,
        script_key: &ScriptKey,
    ) -> Vec<String> {
        let Some(script) = state.scripts.get(script_key) else {
            return Vec::new();
        };
        let ScriptSourceState::Resolved(view) = &script.source else {
            return Vec::new();
        };
        let Some(retained) = self.views.get(&view.view_id) else {
            return view.logical_sources.keys().cloned().collect();
        };
        view.logical_sources
            .keys()
            .map(|path| canonical_authored_url(retained, path))
            .collect()
    }

    pub fn script_contains_authored_source(
        &self,
        state: &DebuggerState,
        script_key: &ScriptKey,
        source_path: &str,
    ) -> bool {
        let Some(script) = state.scripts.get(script_key) else {
            return false;
        };
        let ScriptSourceState::Resolved(view) = &script.source else {
            return false;
        };
        let normalized = normalize_source_path(source_path);
        let Some(retained) = self.views.get(&view.view_id) else {
            return view
                .logical_sources
                .keys()
                .any(|path| normalize_source_path(path).starts_with(normalized));
        };
        retained
            .logical_to_canonical
            .iter()
            .any(|(logical, canonical)| {
                normalize_source_path(logical).starts_with(normalized)
                    || normalize_source_path(canonical).starts_with(normalized)
            })
    }

    pub fn search_source_batch(
        &self,
        state: &DebuggerState,
        path_selector: Option<&str>,
        control: &SearchControl,
    ) -> Result<HydratedSourceBatch, SearchError> {
        let mut sources = BTreeMap::new();
        let mut skipped = BTreeMap::new();
        for (script_key, script) in state.scripts.iter() {
            control.check()?;
            let authored_error = script
                .captured_source
                .as_ref()
                .and_then(|source| source.source_map_error.as_deref())
                .or_else(|| match &script.source {
                    ScriptSourceState::Failed(reason) if script.source_map_url.is_some() => {
                        Some(reason.as_str())
                    }
                    _ => None,
                });
            if let Some(reason) = authored_error {
                skipped.insert(
                    (script.url.clone(), "source-map".to_owned()),
                    format!("could not discover authored sources: {reason}"),
                );
            }
            if path_selector.is_none_or(|selector| script.url.contains(selector)) {
                let identity = (script.url.clone(), "runtime".to_owned());
                if let Some(content) = self.generated_source_content(state, script_key) {
                    let content_hash = crate::capture::content_store::ContentHash::try_of_bytes(
                        content.as_bytes(),
                        || control.check(),
                    )?;
                    sources
                        .entry((
                            identity.0.clone(),
                            identity.1.clone(),
                            content_hash,
                            format!("runtime source {}", script.url),
                        ))
                        .or_insert_with(|| HydratedSource {
                            path: identity.0.clone(),
                            kind: identity.1.clone(),
                            provenance: format!("runtime source {}", script.url),
                            content_hash,
                            content,
                        });
                } else {
                    skipped.insert(identity, Self::script_source_skip_reason(&script.source));
                }
            }
            let ScriptSourceState::Resolved(view) = &script.source else {
                continue;
            };
            let retained = self.views.get(&view.view_id);
            if let Some(retained) = retained {
                for diagnostic in retained.view.diagnostics() {
                    match diagnostic {
                        crate::source::source_view::SourceDiagnostic::SourceMapFailed { error, .. } => {
                            skipped.insert(
                                (script.url.clone(), "source-map".to_owned()),
                                format!("could not discover authored sources: {error}"),
                            );
                        }
                        crate::source::source_view::SourceDiagnostic::MissingContent { logical_url } => {
                            let path = canonical_authored_url(retained, logical_url);
                            if path_selector.is_none_or(|selector| {
                                logical_url.contains(selector) || path.contains(selector)
                            }) {
                                skipped.insert(
                                    (path, "authored".to_owned()),
                                    "source map supplies no content and no workspace content is available".to_owned(),
                                );
                            }
                        }
                        _ => {}
                    }
                }
            }
            for (logical_url, candidate) in view.logical_sources.iter() {
                control.check()?;
                let source_url = retained.map_or_else(
                    || logical_url.clone(),
                    |retained| canonical_authored_url(retained, logical_url),
                );
                if path_selector.is_some_and(|selector| {
                    !logical_url.contains(selector) && !source_url.contains(selector)
                }) {
                    continue;
                }
                let provenance = provenance_label(&candidate.provenance);
                let Some(content) = self.store.get(candidate.content) else {
                    skipped.insert(
                        (source_url, "authored".to_owned()),
                        "source content is not available in the content store".to_owned(),
                    );
                    continue;
                };
                sources
                    .entry((
                        source_url.clone(),
                        "authored".to_owned(),
                        candidate.content,
                        provenance.clone(),
                    ))
                    .or_insert_with(|| HydratedSource {
                        path: source_url,
                        kind: "authored".to_owned(),
                        provenance,
                        content_hash: candidate.content,
                        content,
                    });
            }
        }
        let hydrated = sources
            .values()
            .map(|source| (source.path.clone(), source.kind.clone()))
            .collect::<BTreeSet<_>>();
        skipped.retain(|identity, _| !hydrated.contains(identity));
        Ok(HydratedSourceBatch {
            sources: sources.into_values().collect(),
            skipped_sources: skipped.len().min(u32::MAX as usize) as u32,
            skipped: skipped
                .into_iter()
                .map(|((path, kind), reason)| crate::api::service_api::SourceSearchSkip {
                    path,
                    kind,
                    connection_id: None,
                    target_id: None,
                    reason,
                })
                .collect(),
        })
    }

    fn script_source_skip_reason(source: &ScriptSourceState) -> String {
        match source {
            ScriptSourceState::Unresolved => "script content has not been requested".to_owned(),
            ScriptSourceState::Pending(_) => "script content acquisition is pending".to_owned(),
            ScriptSourceState::Failed(reason) => format!("script content acquisition failed: {reason}"),
            ScriptSourceState::Loaded { .. } | ScriptSourceState::Resolved(_) => {
                "script content is no longer retained".to_owned()
            }
        }
    }

    pub fn map_source_position(
        &self,
        path: &str,
        position: Position,
    ) -> Vec<(String, Position, String, String)> {
        let mut mappings = Vec::new();
        for retained in self.views.values() {
            if retained.generated_url == path {
                mappings.extend(retained.view.forward(path, position).into_iter().map(
                    |candidate| {
                        (
                            canonical_authored_url(retained, &candidate.source_url),
                            candidate.position,
                            "generated-to-authored".to_owned(),
                            mapping_quality_label(candidate.quality).to_owned(),
                        )
                    },
                ));
            }
            if let Some(logical_path) = authored_lookup_path(retained, path) {
                mappings.extend(
                    retained
                        .view
                        .reverse(&logical_path, position)
                        .into_iter()
                        .map(|candidate| {
                            (
                                candidate.source_url,
                                candidate.position,
                                "authored-to-generated".to_owned(),
                                mapping_quality_label(candidate.quality).to_owned(),
                            )
                        }),
                );
            }
        }
        mappings
    }

    pub fn generated_source_content(
        &self,
        state: &DebuggerState,
        script_key: &ScriptKey,
    ) -> Option<Arc<str>> {
        let script = state.scripts.get(script_key)?;
        match &script.source {
            ScriptSourceState::Loaded { content, .. } => Some(content.clone()),
            ScriptSourceState::Resolved(view) => self
                .views
                .get(&view.view_id)
                .map(|view| view.generated_content.clone()),
            _ => None,
        }
        .or_else(|| script.captured_source.as_ref().map(|source| source.content.clone()))
    }

    pub fn clear_caches(&self) {
        for view in self.views.values() {
            view.projection_cache.lock().unwrap().clear();
            view.symbol_indexes.clear();
        }
    }

    fn view_for(
        &self,
        view_id: EffectId,
        script: &ScriptKey,
    ) -> Result<&RetainedView, SourceEffectError> {
        let retained = self
            .views
            .get(&view_id)
            .ok_or(SourceEffectError::UnknownView(view_id))?;
        if &retained.script != script {
            return Err(SourceEffectError::ScriptMismatch {
                view_id,
                expected: retained.script.clone(),
                actual: script.clone(),
            });
        }

        Ok(retained)
    }
}

impl Drop for SourceEffectInterpreter {
    fn drop(&mut self) {
        for observation in self.runtime_sources.values() {
            self.model.release(&observation.contribution);
        }
    }
}

fn source_uri(url: &str, key: &ScriptKey) -> SourceUri {
    if url.is_empty() {
        return SourceUri::embedded(
            "runtime",
            &format!(
                "anonymous/{}/{}/{}",
                key.session.connection_generation, key.session.session_id, key.script_id
            ),
        )
        .expect("runtime source identities can be embedded");
    }
    SourceUri::parse(url)
        .or_else(|_| SourceUri::embedded("runtime", url))
        .expect("runtime source values can be represented")
}

fn mapping_quality_label(quality: MappingQuality) -> &'static str {
    match quality {
        MappingQuality::Exact => "exact",
        MappingQuality::GreatestLowerBound => "greatest-lower-bound",
    }
}

fn provenance_label(provenance: &Provenance) -> String {
    match provenance {
        Provenance::RuntimeSource { url } => format!("runtime source {url}"),
        Provenance::SourcesContent { map_id } => {
            format!("source-map sourcesContent ({map_id:?})")
        }
        Provenance::Workspace { logical_url } => format!("workspace {logical_url}"),
        Provenance::VerifiedWorkspaceAndSourcesContent {
            map_id,
            logical_url,
        } => format!("verified workspace {logical_url} and sourcesContent ({map_id:?})"),
        Provenance::Formatted { generated_url } => {
            format!("formatted fallback from {generated_url}")
        }
    }
}

fn authored_lookup_path(retained: &RetainedView, path: &str) -> Option<String> {
    retained
        .view
        .files()
        .contains_key(path)
        .then(|| path.to_owned())
        .or_else(|| retained.canonical_to_logical.get(path).cloned())
}

fn canonical_authored_url(retained: &RetainedView, path: &str) -> String {
    retained
        .logical_to_canonical
        .get(path)
        .cloned()
        .unwrap_or_else(|| path.to_owned())
}

fn normalize_source_path(path: &str) -> &str {
    path.trim_start_matches("../").trim_start_matches("./")
}

fn projection_step_label(step: &ProjectionStep) -> String {
    match step {
        ProjectionStep::Identity => "identity".to_owned(),
        ProjectionStep::SourceMap { map_id, shape } => {
            format!("source map {map_id:?} ({shape:?})")
        }
        ProjectionStep::Format { formatter } => format!("format with {formatter}"),
        ProjectionStep::Edit { edit_id } => format!("edit {edit_id}"),
    }
}

#[derive(Debug, thiserror::Error)]
pub enum SourceEffectError {
    #[error(transparent)]
    SourceView(#[from] SourceViewError),
    #[error("source view {0:?} is not retained")]
    UnknownView(EffectId),
    #[error("source view {view_id:?} belongs to {expected:?}, not {actual:?}")]
    ScriptMismatch {
        view_id: EffectId,
        expected: ScriptKey,
        actual: ScriptKey,
    },
    #[error("source view {view_id:?} cannot map generated position {position:?}")]
    UnmappedPosition {
        view_id: EffectId,
        position: crate::source::source_view::Position,
    },
    #[error(
        "source view {view_id:?} maps generated position {position:?} to {source_url} at \
         {source_position:?}, but the authored source content is unavailable"
    )]
    UnavailableMappedSource {
        view_id: EffectId,
        position: crate::source::source_view::Position,
        source_url: String,
        source_position: crate::source::source_view::Position,
    },
}

#[cfg(test)]
mod tests {
    use sourcemap::SourceMapBuilder;

    use super::*;

    #[test]
    fn symbol_indexes_are_shared_and_lookups_do_not_hold_the_cache_lock() {
        let cache = SymbolIndexCache::default();
        let source = "class Example { method() { return 1; } }";
        let positions = Arc::new(LineIndex::new(source));
        let cells = std::thread::scope(|scope| {
            let threads = (0..8)
                .map(|_| {
                    scope.spawn(|| cache.get_or_create("fixture.js", source, Some(&positions)))
                })
                .collect::<Vec<_>>();
            threads
                .into_iter()
                .map(|thread| thread.join().unwrap())
                .collect::<Vec<_>>()
        });
        assert_eq!(Arc::strong_count(&positions), 2);
        assert!(cells.iter().all(|cell| Arc::ptr_eq(cell, &cells[0])));
        for _ in 0..3 {
            assert_eq!(
                cache.breadcrumb("fixture.js", source, 1, 30, Some(&positions)).as_deref(),
                Some("Example.method")
            );
        }
        assert_eq!(Arc::strong_count(&positions), 2);
        let entries = cache.entries.lock().unwrap();
        assert_eq!(entries.len(), 1);
        assert_eq!(
            cells[0]
                .get()
                .unwrap()
                .as_ref()
                .unwrap()
                .breadcrumb(1, 30)
                .as_deref(),
            Some("Example.method")
        );
        drop(entries);
        cache.clear();
        assert!(cache.entries.lock().unwrap().is_empty());
        assert_eq!(
            cells[0]
                .get()
                .unwrap()
                .as_ref()
                .unwrap()
                .breadcrumb(1, 30)
                .as_deref(),
            Some("Example.method")
        );
    }

    #[test]
    fn symbol_index_cache_reuses_negative_entries_until_cleared() {
        let cache = SymbolIndexCache::default();
        let failed = Arc::new(OnceLock::new());
        assert!(failed.set(None).is_ok());
        cache.entries.lock().unwrap().insert("fixture.js".into(), failed.clone());
        assert_eq!(
            cache.breadcrumb("fixture.js", "function example() {}", 1, 1, None),
            None
        );
        assert!(Arc::ptr_eq(
            &failed,
            &cache.get_or_create("fixture.js", "function example() {}", None)
        ));
        cache.clear();
        assert_eq!(
            cache.breadcrumb("fixture.js", "function example() {}", 1, 1, None).as_deref(),
            Some("example")
        );
    }

    #[test]
    fn source_position_resolution_shares_projection_and_cache_without_changing_frame_errors() {
        for authored_content in [Some("function example() { return 1; }"), None] {
            let model = Arc::new(ContextSourceModel::new());
            let mut interpreter = SourceEffectInterpreter::new(
                SourceEffectOptions::default(),
                model,
                "source-position-test",
            );
            let script = ScriptKey {
                session: crate::debugger::debugger_engine::SessionKey {
                    connection_generation: 1,
                    session_id: "session-1".into(),
                },
                script_id: "script-1".into(),
            };
            let mut builder = SourceMapBuilder::new(Some("bundle.js"));
            let source = builder.add_source("../src/example.ts");
            builder.set_source_contents(source, authored_content);
            builder.add(0, 0, 0, 24, Some("../src/example.ts"), None, false);
            let mut map = Vec::new();
            builder.into_sourcemap().to_writer(&mut map).unwrap();
            let built = interpreter.interpret(&Effect::BuildSourceView {
                effect_id: EffectId(1),
                script: script.clone(),
                script_version: 1,
                generated_url: "file:///workspace/dist/bundle.js".into(),
                content: Arc::from("function a(){return 1;}"),
                source_map: Some(crate::source::source_view::SourceMapData::new(map)),
                source_map_url: Some("file:///workspace/dist/bundle.js.map".into()),
            }).unwrap().unwrap();
            let Input::SourceViewBuilt { logical_sources, .. } = built else {
                panic!("expected source view");
            };
            let mut state = DebuggerState::default();
            Arc::make_mut(&mut state.scripts).insert(script.clone(), Arc::new(
                crate::debugger::debugger_engine::ScriptState {
                    url: "file:///workspace/dist/bundle.js".into(),
                    hash: "runtime-hash".into(),
                    source_map_url: Some("file:///workspace/dist/bundle.js.map".into()),
                    version: 1,
                    source: ScriptSourceState::Resolved(crate::debugger::debugger_engine::SourceViewState {
                        view_id: EffectId(1),
                        logical_sources: Arc::new(logical_sources),
                    }),
                    provenance: Default::default(),
                    captured_source: None,
                },
            ));
            for _ in 0..3 {
                let resolved = interpreter.resolve_generated_position(
                    &state, &script, Position::ZERO,
                ).unwrap();
                assert_eq!(resolved.mapping, "authored");
                assert_eq!(resolved.resolved.source_url, "file:///workspace/src/example.ts");
                assert_eq!((resolved.resolved.line, resolved.resolved.column), (1, 25));
                assert_eq!(resolved.diagnostic.is_some(), authored_content.is_none());
                assert_eq!(resolved.breadcrumb.as_deref(), authored_content.map(|_| "example"));
                let projected = interpreter.project_generated_position(
                    &state, &script, Position::ZERO,
                );
                assert_eq!(projected.is_some(), authored_content.is_some());
                if let Some((url, position, content)) = projected {
                    assert_eq!(position, Position { line: 0, column: 24 });
                    assert_eq!(
                        interpreter.breadcrumb(&state, &script, &url, 1, 25, &content),
                        resolved.breadcrumb
                    );
                }
            }
            let retained = &interpreter.views[&EffectId(1)];
            assert_eq!(
                retained.symbol_indexes.entries.lock().unwrap().len(),
                usize::from(authored_content.is_some())
            );
            let frame = interpreter.interpret(&Effect::MapFrame {
                effect_id: EffectId(2),
                session: script.session.clone(),
                pause_epoch: 1,
                frame_index: 0,
                script,
                view_id: EffectId(1),
                position: Position::ZERO,
            });
            if authored_content.is_none() {
                assert!(matches!(frame, Err(SourceEffectError::UnavailableMappedSource { .. })));
            } else {
                assert!(matches!(frame, Ok(Some(Input::FrameMapped { .. }))));
            }
        }
    }

    #[test]
    fn generated_offset_index_maps_utf16_and_byte_offsets() {
        let content = "a😀b\nsecond";
        let index = LineIndex::new(content);
        assert_eq!(
            index.clamped_utf16_position(3),
            Position { line: 0, column: 3 }
        );
        assert_eq!(
            index.clamped_byte_position(5),
            Position { line: 0, column: 3 }
        );
        assert_eq!(
            index.clamped_utf16_position(5),
            Position { line: 1, column: 0 }
        );
    }

    #[test]
    fn observes_unresolved_runtime_scripts_and_releases_them() {
        let model = Arc::new(ContextSourceModel::new());
        let mut interpreter = SourceEffectInterpreter::new(
            SourceEffectOptions::default(),
            model.clone(),
            "test-target",
        );
        let state = reduce(&Arc::new(DebuggerState::default()), Input::Connected).state;
        let state = reduce(
            &state,
            Input::SessionAttached {
                session_id: "session-1".into(),
                target_id: "target-1".into(),
                parent_session_id: None,
                waiting_for_debugger: false,
            },
        )
        .state;
        let session = state.sessions.keys().next().unwrap().clone();
        let state = reduce(
            &state,
            Input::ScriptParsed {
                session: session.clone(),
                script_id: "script-1".into(),
                url: "https://example.test/app.js".into(),
                hash: "runtime-hash".into(),
                source_map_url: None,
            },
        )
        .state;

        interpreter.retain_for_state(&state);
        let snapshot = model.graph_snapshot();
        assert_eq!(snapshot.sources.len(), 1);
        assert_eq!(
            snapshot.sources[0].revision,
            SourceRevision::Version {
                namespace: RevisionNamespace::new("cdp-script").unwrap(),
                value: "runtime-hash".into(),
            }
        );
        assert_eq!(model.compacted_graph().nodes.len(), 1);

        let state = reduce(&state, Input::SessionDetached { session }).state;
        interpreter.retain_for_state(&state);
        assert!(model.graph_snapshot().sources.is_empty());
    }

    #[test]
    fn resolved_content_replaces_the_runtime_observation() {
        let model = Arc::new(ContextSourceModel::new());
        let mut interpreter = SourceEffectInterpreter::new(
            SourceEffectOptions::default(),
            model.clone(),
            "test-target",
        );
        let script = ScriptKey {
            session: crate::debugger::debugger_engine::SessionKey {
                connection_generation: 1,
                session_id: "session-1".into(),
            },
            script_id: "script-1".into(),
        };
        let mut state = DebuggerState::default();
        Arc::make_mut(&mut state.scripts).insert(
            script.clone(),
            Arc::new(crate::debugger::debugger_engine::ScriptState {
                url: "https://example.test/app.js".into(),
                provenance: Default::default(),
                captured_source: None,
                hash: "runtime-hash".into(),
                source_map_url: None,
                version: 1,
                source: ScriptSourceState::Unresolved,
            }),
        );
        interpreter.retain_for_state(&state);
        assert_eq!(model.graph_snapshot().sources.len(), 1);

        let content_owner = SourceContributionId::new("test-target/content");
        let content = model.content_store().intern("const value = 1;");
        model
            .intern_content(
                &content_owner,
                SourceUri::parse("https://example.test/app.js").unwrap(),
                content,
            )
            .unwrap();
        Arc::make_mut(Arc::make_mut(&mut state.scripts).get_mut(&script).unwrap()).source =
            ScriptSourceState::Resolved(crate::debugger::debugger_engine::SourceViewState {
                view_id: EffectId(1),
                logical_sources: Arc::new(BTreeMap::new()),
            });
        interpreter.retain_for_state(&state);

        let snapshot = model.graph_snapshot();
        assert_eq!(snapshot.sources.len(), 1);
        assert_eq!(
            snapshot.sources[0].revision,
            SourceRevision::Content(content)
        );
        model.release(&content_owner);
    }

    #[test]
    fn source_search_selects_paths_before_content_hydration() {
        let model = Arc::new(ContextSourceModel::new());
        let interpreter = SourceEffectInterpreter::new(
            SourceEffectOptions::default(),
            model.clone(),
            "test-target",
        );
        let keep = model.content_store().intern("const keep = true;");
        let skip = model.content_store().intern("const skip = true;");
        let script = ScriptKey {
            session: crate::debugger::debugger_engine::SessionKey {
                connection_generation: 1,
                session_id: "session-1".into(),
            },
            script_id: "script-1".into(),
        };
        let mut state = DebuggerState::default();
        Arc::make_mut(&mut state.scripts).insert(
            script,
            Arc::new(crate::debugger::debugger_engine::ScriptState {
                url: "dist/app.js".into(),
                provenance: Default::default(),
                captured_source: None,
                hash: "runtime-hash".into(),
                source_map_url: None,
                version: 1,
                source: ScriptSourceState::Resolved(crate::debugger::debugger_engine::SourceViewState {
                    view_id: EffectId(1),
                    logical_sources: Arc::new(BTreeMap::from([
                        (
                            "src/keep.ts".into(),
                            crate::source::source_view::ContentCandidate {
                                content: keep,
                                provenance: Provenance::Workspace {
                                    logical_url: "src/keep.ts".into(),
                                },
                            },
                        ),
                        (
                            "src/skip.ts".into(),
                            crate::source::source_view::ContentCandidate {
                                content: skip,
                                provenance: Provenance::Workspace {
                                    logical_url: "src/skip.ts".into(),
                                },
                            },
                        ),
                    ])),
                }),
            }),
        );

        let before = model.content_store().stats().materializations;
        let batch = interpreter
            .search_source_batch(&state, Some("keep"), &SearchControl::default())
            .unwrap();

        assert_eq!(batch.sources.len(), 1);
        assert_eq!(batch.sources[0].path, "src/keep.ts");
        assert_eq!(
            model.content_store().stats().materializations - before,
            1,
            "the unselected source must not be hydrated"
        );

        let cancelled = SearchControl::default();
        cancelled.cancel();
        let before = model.content_store().stats().materializations;
        assert!(matches!(
            interpreter.search_source_batch(&state, None, &cancelled),
            Err(SearchError::Cancelled)
        ));
        assert_eq!(
            model.content_store().stats().materializations,
            before,
            "cancelled batches must stop before content hydration"
        );
    }

    use crate::debugger::debugger_engine::{
        BreakpointBinding, BreakpointKey, Diagnostic, FrameProjection, RawFrame, SessionPhase,
        reduce,
    };
    use crate::source::source_view::Position;

    #[test]
    fn drives_real_source_maps_through_a_complete_debugger_scenario() {
        let (first, first_revisions) = run_scenario();
        let (second, second_revisions) = run_scenario();

        assert_eq!(first_revisions, second_revisions);
        assert_eq!(first.revision, second.revision);
        assert_eq!(first.sessions.len(), 0);
        assert_eq!(first.scripts.len(), 0);
        assert_eq!(first.physical_breakpoints.len(), 0);
        assert!(matches!(
            first.diagnostics.last(),
            Some(Diagnostic::IgnoredStaleEffect { .. })
        ));
    }

    fn run_scenario() -> (Arc<DebuggerState>, Vec<u64>) {
        let mut revisions = Vec::new();
        let mut state = Arc::new(DebuggerState::default());
        let mut interpreter = SourceEffectInterpreter::new(
            SourceEffectOptions::default(),
            Arc::new(ContextSourceModel::new()),
            "test-target",
        );

        state = apply(&state, Input::Connected, &mut revisions).state;
        let attached = apply(
            &state,
            Input::SessionAttached {
                session_id: "session-1".into(),
                target_id: "target-1".into(),
                parent_session_id: None,
                waiting_for_debugger: true,
            },
            &mut revisions,
        );
        let Effect::ConfigureSession { effect_id, session } = attached.effects[0].clone() else {
            panic!("expected session configuration");
        };
        let configured = apply(
            &attached.state,
            Input::SessionConfigured { effect_id },
            &mut revisions,
        );
        let waiting_for_debugger = configured
            .state
            .sessions
            .get(&session)
            .map(|session| session.waiting_for_debugger)
            .expect("session exists");
        assert!(waiting_for_debugger);
        assert!(configured.effects.is_empty());
        let released = apply(
            &configured.state,
            Input::ReleaseIfWaiting {
                session: session.clone(),
            },
            &mut revisions,
        );
        let Effect::RunIfWaitingForDebugger {
            effect_id: run_effect,
            ..
        } = released.effects[0]
        else {
            panic!("expected run-if-waiting");
        };
        state = apply(
            &released.state,
            Input::CommandAccepted {
                effect_id: run_effect,
            },
            &mut revisions,
        )
        .state;

        let parsed = apply(
            &state,
            Input::ScriptParsed {
                session: session.clone(),
                script_id: "script-1".into(),
                url: "file:///bundle.js".into(),
                hash: "hash-1".into(),
                source_map_url: Some("file:///bundle.js.map".into()),
            },
            &mut revisions,
        );
        assert!(parsed.effects.is_empty());
        let script = parsed.state.scripts.keys().next().unwrap().clone();
        let requested = apply(
            &parsed.state,
            Input::RequestScriptSource {
                script: script.clone(),
            },
            &mut revisions,
        );
        let Effect::FetchScriptSource {
            effect_id: fetch_id,
            script: effect_script,
            ..
        } = requested.effects[0].clone()
        else {
            panic!("expected source fetch");
        };
        assert_eq!(effect_script, script);
        let fetched = apply(
            &requested.state,
            Input::ScriptSourceFetched {
                effect_id: fetch_id,
                content: Arc::from("var answer=42;"),
                source_map: Some(crate::source::source_view::SourceMapData::new(source_map())),
                source_map_url: Some("file:///bundle.js.map".into()),
                source_map_error: None,
            },
            &mut revisions,
        );
        let build = &fetched.effects[0];
        let built_input = interpreter
            .interpret(build)
            .expect("source view builds")
            .expect("build is a source effect");
        state = apply(&fetched.state, built_input, &mut revisions).state;
        interpreter.retain_for_state(&state);
        assert_eq!(interpreter.retained_view_count(), 1);
        assert!(
            interpreter
                .resolved_source_paths()
                .contains(&("file:///src/app.ts".into(), "authored".into()))
        );
        assert_eq!(
            interpreter.authored_source_paths(&state, &script),
            vec!["file:///src/app.ts"]
        );
        assert!(interpreter.script_contains_authored_source(&state, &script, "file:///src/app.ts"));
        assert_eq!(
            interpreter
                .logical_source_content(&state, &script, "file:///src/app.ts")
                .as_deref(),
            Some("let answer: number = 42;")
        );
        assert!(
            interpreter
                .map_source_position(
                    "file:///src/app.ts",
                    Position {
                        line: 0,
                        column: 10,
                    },
                )
                .iter()
                .any(|(url, position, direction, _)| {
                    url == "file:///bundle.js"
                        && *position
                            == Position {
                                line: 0,
                                column: 10,
                            }
                        && direction == "authored-to-generated"
                })
        );

        let breakpoint = BreakpointKey {
            client_id: "client-1".into(),
            breakpoint_id: "breakpoint-1".into(),
        };
        let mapped = apply(
            &state,
            Input::SetBreakpoint {
                key: breakpoint.clone(),
                source_url: "src/app.ts".into(),
                position: Position {
                    line: 0,
                    column: 10,
                },
                condition: None,
            },
            &mut revisions,
        );
        let mapped_input = interpreter
            .interpret(&mapped.effects[0])
            .expect("breakpoint maps")
            .expect("mapping is a source effect");
        let installing = apply(&mapped.state, mapped_input, &mut revisions);
        let Effect::InstallBreakpoint {
            effect_id: install_id,
            physical,
        } = installing.effects[0].clone()
        else {
            panic!("expected physical breakpoint installation");
        };
        assert_eq!(
            physical.position,
            Position {
                line: 0,
                column: 10
            }
        );
        state = apply(
            &installing.state,
            Input::BreakpointInstalled {
                effect_id: install_id,
                backend_id: "chrome-breakpoint-1".into(),
                confirmed_position: physical.position,
            },
            &mut revisions,
        )
        .state;
        assert!(matches!(
            state.breakpoints[&breakpoint].bindings.values().next(),
            Some(BreakpointBinding::Installed { backend_id })
                if backend_id == "chrome-breakpoint-1"
        ));

        let paused = apply(
            &state,
            Input::Paused {
                session: session.clone(),
                reason: "breakpoint".into(),
                frames: vec![RawFrame {
                    call_frame_id: "frame-1".into(),
                    function_name: "main".into(),
                    script_id: script.script_id.clone(),
                    position: Position {
                        line: 0,
                        column: 10,
                    },
                    scopes: vec![],
                }],
            },
            &mut revisions,
        );
        let late_frame_input = interpreter
            .interpret(&paused.effects[0])
            .expect("frame maps")
            .expect("frame mapping is a source effect");
        state = apply(&paused.state, late_frame_input.clone(), &mut revisions).state;
        let pause = state.sessions[&session].pause.as_ref().expect("paused");
        assert!(matches!(
            pause.frames[0].projected,
            FrameProjection::Resolved {
                ref source_url,
                position: Position {
                    line: 0,
                    column: 10
                }
            } if source_url == "src/app.ts"
        ));
        let pause_epoch = pause.epoch;

        let resuming = apply(
            &state,
            Input::ResumeRequested {
                session: session.clone(),
                pause_epoch,
            },
            &mut revisions,
        );
        assert!(matches!(
            resuming.state.sessions[&session].phase,
            SessionPhase::Resuming { epoch } if epoch == pause_epoch
        ));
        let Effect::Resume {
            effect_id: resume_id,
            ..
        } = resuming.effects[0]
        else {
            panic!("expected resume command");
        };
        let resumed = apply(
            &resuming.state,
            Input::CommandAccepted {
                effect_id: resume_id,
            },
            &mut revisions,
        );
        state = apply(
            &resumed.state,
            Input::Resumed {
                session: session.clone(),
                pause_epoch,
            },
            &mut revisions,
        )
        .state;
        assert!(matches!(
            state.sessions[&session].phase,
            SessionPhase::Running
        ));

        state = apply(
            &state,
            Input::SessionDetached {
                session: session.clone(),
            },
            &mut revisions,
        )
        .state;
        interpreter.retain_for_state(&state);
        assert_eq!(interpreter.retained_view_count(), 0);

        state = apply(&state, late_frame_input, &mut revisions).state;
        (state, revisions)
    }

    fn apply(
        state: &Arc<DebuggerState>,
        input: Input,
        revisions: &mut Vec<u64>,
    ) -> crate::debugger::debugger_engine::Transition {
        let transition = reduce(state, input);
        revisions.push(transition.state.revision);
        transition
    }

    fn source_map() -> Vec<u8> {
        let mut builder = SourceMapBuilder::new(Some("bundle.js"));
        let source = builder.add_source("src/app.ts");
        builder.set_source_contents(source, Some("let answer: number = 42;"));
        builder.add(0, 0, 0, 0, Some("src/app.ts"), None, false);
        builder.add(0, 10, 0, 10, Some("src/app.ts"), None, false);
        let mut raw = Vec::new();
        builder
            .into_sourcemap()
            .to_writer(&mut raw)
            .expect("map serializes");
        raw
    }
}
