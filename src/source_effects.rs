use std::collections::{BTreeMap, BTreeSet};
use std::sync::Arc;
use std::sync::Mutex;

use rayon::prelude::*;

use crate::content_store::ContentStore;
use crate::context_source_model::{ContextSourceModel, SourceContributionId, SourceSnapshotRole};
use crate::debugger_engine::{
    BreakpointMapping, DebuggerState, Effect, EffectId, Input, ScriptKey, ScriptSourceState,
};
use crate::service_api::{SourceGraphViewSnapshot, SourceProjectionPathSnapshot};
use crate::source_graph::{RevisionNamespace, SourceRevision, SourceUri};
use crate::source_search::{HydratedSource, HydratedSourceBatch, SearchControl, SearchError};
use crate::source_view::{
    GeneratedSourceInput, MappingQuality, Position, ProjectionStep, Provenance, ResolutionPolicy,
    ResolvedSourceView, SourceViewError, appears_minified,
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
    generated_content: Arc<str>,
    generated_index: GeneratedOffsetIndex,
    projection_cache: Mutex<BTreeMap<u32, Option<ProjectedOffset>>>,
    symbol_indexes: Mutex<BTreeMap<String, Option<crate::language_intelligence::SymbolIndex>>>,
    view: Arc<ResolvedSourceView>,
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

struct GeneratedOffsetIndex {
    checkpoints: Vec<OffsetCheckpoint>,
}

#[derive(Clone, Copy)]
struct OffsetCheckpoint {
    byte: usize,
    utf16: u32,
    line: u32,
    column: u32,
}

impl GeneratedOffsetIndex {
    const CHECKPOINT_BYTES: usize = 4096;

    fn new(content: &str) -> Self {
        let mut checkpoints = vec![OffsetCheckpoint {
            byte: 0,
            utf16: 0,
            line: 0,
            column: 0,
        }];
        let mut utf16 = 0_u32;
        let mut line = 0_u32;
        let mut column = 0_u32;
        let mut next_checkpoint = Self::CHECKPOINT_BYTES;
        for (byte, character) in content.char_indices() {
            if byte >= next_checkpoint || character == '\n' {
                checkpoints.push(OffsetCheckpoint {
                    byte,
                    utf16,
                    line,
                    column,
                });
                next_checkpoint = byte.saturating_add(Self::CHECKPOINT_BYTES);
            }
            utf16 = utf16.saturating_add(character.len_utf16() as u32);
            if character == '\n' {
                line = line.saturating_add(1);
                column = 0;
            } else {
                column = column.saturating_add(character.len_utf16() as u32);
            }
        }
        Self { checkpoints }
    }

    fn utf16_position(&self, content: &str, target: u32) -> Position {
        let checkpoint = self
            .checkpoints
            .partition_point(|checkpoint| checkpoint.utf16 <= target)
            .saturating_sub(1);
        self.scan(content, self.checkpoints[checkpoint], |state| {
            state.utf16 >= target
        })
    }

    fn byte_position(&self, content: &str, target: u32) -> Position {
        let target = target as usize;
        let checkpoint = self
            .checkpoints
            .partition_point(|checkpoint| checkpoint.byte <= target)
            .saturating_sub(1);
        self.scan(content, self.checkpoints[checkpoint], |state| {
            state.byte >= target
        })
    }

    fn scan(
        &self,
        content: &str,
        mut state: OffsetCheckpoint,
        done: impl Fn(OffsetCheckpoint) -> bool,
    ) -> Position {
        let base = state.byte;
        for (relative_byte, character) in content[base..].char_indices() {
            state.byte = base + relative_byte;
            if done(state) {
                break;
            }
            state.utf16 = state.utf16.saturating_add(character.len_utf16() as u32);
            if character == '\n' {
                state.line = state.line.saturating_add(1);
                state.column = 0;
            } else {
                state.column = state.column.saturating_add(character.len_utf16() as u32);
            }
            state.byte = base + relative_byte + character.len_utf8();
        }
        Position {
            line: state.line,
            column: state.column,
        }
    }
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
                view.add_generated(GeneratedSourceInput {
                    url: generated_url,
                    content,
                    source_map: source_map.as_deref(),
                    source_map_url: source_map_url.as_deref(),
                    minified: source_map.is_none()
                        && self.options.format_unmapped_sources
                        && appears_minified(generated_url, content),
                })?;
                let logical_sources = view
                    .files()
                    .iter()
                    .map(|(url, file)| (url.clone(), file.primary.clone()))
                    .collect();
                self.views.insert(
                    *effect_id,
                    RetainedView {
                        script: script.clone(),
                        generated_url: generated_url.clone(),
                        generated_content: content.clone(),
                        generated_index: GeneratedOffsetIndex::new(content),
                        projection_cache: Mutex::new(BTreeMap::new()),
                        symbol_indexes: Mutex::new(BTreeMap::new()),
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
                let mappings = retained
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
                    .collect::<Vec<_>>();
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

        let mapped = [
            retained
                .generated_index
                .utf16_position(&retained.generated_content, utf16_offset),
            retained
                .generated_index
                .byte_position(&retained.generated_content, utf16_offset),
        ]
        .into_iter()
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
        let mapped = retained
            .view
            .forward(&retained.generated_url, position)
            .into_iter()
            .next()?;
        retained
            .view
            .files()
            .get(&mapped.source_url)
            .and_then(|authored| self.store.get(authored.primary.content))
            .map(|content| (mapped.source_url, mapped.position, content))
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
                .utf16_position(&retained.generated_content, utf16_offset),
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
        let mut indexes = retained.symbol_indexes.lock().unwrap();
        if !indexes.contains_key(source_url) {
            indexes.insert(
                source_url.to_owned(),
                crate::language_intelligence::SymbolIndex::new(source_url, content),
            );
        }
        indexes
            .get(source_url)
            .and_then(Option::as_ref)
            .and_then(|index| index.breadcrumb(content, line, column))
    }

    pub fn prepare_breadcrumbs(
        &self,
        state: &DebuggerState,
        sources: &[(ScriptKey, String, Arc<str>)],
    ) {
        let mut missing = BTreeMap::<(EffectId, String), Arc<str>>::new();
        for (script, source_url, content) in sources {
            let Some(ScriptSourceState::Resolved(source_state)) =
                state.scripts.get(script).map(|script| &script.source)
            else {
                continue;
            };
            let Some(retained) = self.views.get(&source_state.view_id) else {
                continue;
            };
            if !retained
                .symbol_indexes
                .lock()
                .unwrap()
                .contains_key(source_url)
            {
                missing
                    .entry((source_state.view_id, source_url.clone()))
                    .or_insert_with(|| content.clone());
            }
        }

        let indexes = missing
            .into_par_iter()
            .map(|((view_id, source_url), content)| {
                let index = crate::language_intelligence::SymbolIndex::new(&source_url, &content);
                (view_id, source_url, index)
            })
            .collect::<Vec<_>>();
        for (view_id, source_url, index) in indexes {
            if let Some(retained) = self.views.get(&view_id) {
                retained
                    .symbol_indexes
                    .lock()
                    .unwrap()
                    .entry(source_url)
                    .or_insert(index);
            }
        }
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
                ScriptSourceState::Resolved(view) => view
                    .logical_sources
                    .get(source_url)
                    .and_then(|candidate| self.store.get(candidate.content)),
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
            let Some(file) = retained.view.files().get(path) else {
                continue;
            };
            explanations.push(SourceGraphViewSnapshot {
                connection_id: String::new(),
                target_id: String::new(),
                generated_url: retained.generated_url.clone(),
                source_path: file.logical_url.clone(),
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
                    .cloned()
                    .map(|path| (path, "authored".to_owned())),
            );
        }
        paths.into_iter().collect()
    }

    pub fn search_source_batch(
        &self,
        state: &DebuggerState,
        path_selector: Option<&str>,
        control: &SearchControl,
    ) -> Result<HydratedSourceBatch, SearchError> {
        let mut sources = BTreeMap::new();
        let mut skipped = BTreeSet::new();
        for (script_key, script) in state.scripts.iter() {
            control.check()?;
            if path_selector.is_none_or(|selector| script.url.contains(selector)) {
                let identity = (script.url.clone(), "runtime".to_owned());
                if let Some(content) = self.generated_source_content(state, script_key) {
                    let content_hash = crate::content_store::ContentHash::try_of_bytes(
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
                    skipped.insert(identity);
                }
            }
            let ScriptSourceState::Resolved(view) = &script.source else {
                continue;
            };
            for (logical_url, candidate) in view.logical_sources.iter() {
                control.check()?;
                if path_selector.is_some_and(|selector| !logical_url.contains(selector)) {
                    continue;
                }
                let provenance = provenance_label(&candidate.provenance);
                let Some(content) = self.store.get(candidate.content) else {
                    skipped.insert((logical_url.clone(), "authored".to_owned()));
                    continue;
                };
                sources
                    .entry((
                        logical_url.clone(),
                        "authored".to_owned(),
                        candidate.content,
                        provenance.clone(),
                    ))
                    .or_insert_with(|| HydratedSource {
                        path: logical_url.clone(),
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
        skipped.retain(|identity| !hydrated.contains(identity));
        Ok(HydratedSourceBatch {
            sources: sources.into_values().collect(),
            skipped_sources: skipped.len().min(u32::MAX as usize) as u32,
        })
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
                            candidate.source_url,
                            candidate.position,
                            "generated-to-authored".to_owned(),
                            mapping_quality_label(candidate.quality).to_owned(),
                        )
                    },
                ));
            }
            if retained.view.files().contains_key(path) {
                mappings.extend(retained.view.reverse(path, position).into_iter().map(
                    |candidate| {
                        (
                            candidate.source_url,
                            candidate.position,
                            "authored-to-generated".to_owned(),
                            mapping_quality_label(candidate.quality).to_owned(),
                        )
                    },
                ));
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
    }

    pub fn clear_caches(&self) {
        for view in self.views.values() {
            view.projection_cache.lock().unwrap().clear();
            view.symbol_indexes.lock().unwrap().clear();
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
        position: crate::source_view::Position,
    },
    #[error(
        "source view {view_id:?} maps generated position {position:?} to {source_url} at \
         {source_position:?}, but the authored source content is unavailable"
    )]
    UnavailableMappedSource {
        view_id: EffectId,
        position: crate::source_view::Position,
        source_url: String,
        source_position: crate::source_view::Position,
    },
}

#[cfg(test)]
mod tests {
    use sourcemap::SourceMapBuilder;

    use super::*;

    #[test]
    fn generated_offset_index_maps_utf16_and_byte_offsets() {
        let content = "a😀b\nsecond";
        let index = GeneratedOffsetIndex::new(content);
        assert_eq!(
            index.utf16_position(content, 3),
            Position { line: 0, column: 3 }
        );
        assert_eq!(
            index.byte_position(content, 5),
            Position { line: 0, column: 3 }
        );
        assert_eq!(
            index.utf16_position(content, 5),
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
            session: crate::debugger_engine::SessionKey {
                connection_generation: 1,
                session_id: "session-1".into(),
            },
            script_id: "script-1".into(),
        };
        let mut state = DebuggerState::default();
        Arc::make_mut(&mut state.scripts).insert(
            script.clone(),
            Arc::new(crate::debugger_engine::ScriptState {
                url: "https://example.test/app.js".into(),
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
            ScriptSourceState::Resolved(crate::debugger_engine::SourceViewState {
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
            session: crate::debugger_engine::SessionKey {
                connection_generation: 1,
                session_id: "session-1".into(),
            },
            script_id: "script-1".into(),
        };
        let mut state = DebuggerState::default();
        Arc::make_mut(&mut state.scripts).insert(
            script,
            Arc::new(crate::debugger_engine::ScriptState {
                url: "dist/app.js".into(),
                hash: "runtime-hash".into(),
                source_map_url: None,
                version: 1,
                source: ScriptSourceState::Resolved(crate::debugger_engine::SourceViewState {
                    view_id: EffectId(1),
                    logical_sources: Arc::new(BTreeMap::from([
                        (
                            "src/keep.ts".into(),
                            crate::source_view::ContentCandidate {
                                content: keep,
                                provenance: Provenance::Workspace {
                                    logical_url: "src/keep.ts".into(),
                                },
                            },
                        ),
                        (
                            "src/skip.ts".into(),
                            crate::source_view::ContentCandidate {
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

    use crate::debugger_engine::{
        BreakpointBinding, BreakpointKey, Diagnostic, FrameProjection, RawFrame, SessionPhase,
        reduce,
    };
    use crate::source_view::Position;

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
                source_map: Some(Arc::from(source_map())),
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
    ) -> crate::debugger_engine::Transition {
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
