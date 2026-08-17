use std::collections::{BTreeMap, HashMap};
use std::mem::size_of;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, OnceLock};

use serde::{Deserialize, Serialize};
use sourcemap::{DecodedMap, RawToken, SourceMap, decode_slice};

use crate::content_store::{ContentId, ContentStore, ContentStoreStats};
use crate::source_graph::{
    IdentityBasis, ProjectionKind, SourceFileStore, SourceFileStoreError, SourceGraph,
    SourceGraphError, SourcePath, SourceSnapshot, SourceSnapshotId,
};

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub struct MapId(usize);

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub struct Position {
    pub line: u32,
    pub column: u32,
}

impl Position {
    pub const ZERO: Self = Self { line: 0, column: 0 };
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum MappingQuality {
    Exact,
    GreatestLowerBound,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum MapShape {
    Ordinary,
    Indexed {
        section_count: usize,
        max_depth: usize,
    },
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ProjectionStep {
    Identity,
    SourceMap { map_id: MapId, shape: MapShape },
    Format { formatter: String },
    Edit { edit_id: String },
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ProjectionPath {
    pub generated_url: String,
    pub content: ContentId,
    pub steps: Vec<ProjectionStep>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum Provenance {
    RuntimeSource { url: String },
    SourcesContent { map_id: MapId },
    Workspace { logical_url: String },
    VerifiedWorkspaceAndSourcesContent { map_id: MapId, logical_url: String },
    Formatted { generated_url: String },
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ContentCandidate {
    pub content: ContentId,
    pub provenance: Provenance,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SourceKind {
    Identity,
    Authored,
    FormattedFallback,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ResolvedSourceFile {
    pub logical_url: String,
    pub kind: SourceKind,
    pub primary: ContentCandidate,
    pub alternatives: Vec<ContentCandidate>,
    pub projection_paths: Vec<ProjectionPath>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ResolutionPolicy {
    PreferSourcesContent,
    PreferWorkspaceIfMatching,
    PreferWorkspaceAlways,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CandidateLocation {
    pub source_url: String,
    pub position: Position,
    pub quality: MappingQuality,
    pub projection: ProjectionPath,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum SourceDiagnostic {
    SourceMapFailed {
        generated_url: String,
        error: String,
    },
    MissingContent {
        logical_url: String,
    },
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct SourceMemoryReport {
    pub generated_sources: usize,
    pub resolved_files: usize,
    pub maps: usize,
    pub map_tokens: usize,
    pub encoded_map_bytes: usize,
    pub estimated_decoded_token_bytes: usize,
    pub reverse_indexes_built: usize,
    pub reverse_positions: usize,
    pub format_mapping_points: usize,
    pub estimated_format_index_bytes: usize,
    pub content_store: ContentStoreStats,
}

#[derive(Debug, thiserror::Error)]
pub enum SourceViewError {
    #[error("generated source already exists: {0}")]
    DuplicateGeneratedSource(String),
    #[error("source-map decoding failed: {0}")]
    InvalidSourceMap(String),
    #[error("indexed source map could not be flattened: {0}")]
    InvalidIndexedSourceMap(String),
    #[error("source content is not available: {0}")]
    MissingContent(String),
    #[error(transparent)]
    SourceFileStore(#[from] SourceFileStoreError),
    #[error(transparent)]
    SourceGraph(#[from] SourceGraphError),
    #[error("edit projections are representable but not implemented")]
    EditProjectionUnimplemented,
}

pub struct GeneratedSourceInput<'a> {
    pub url: &'a str,
    pub content: &'a str,
    pub source_map: Option<&'a [u8]>,
    pub minified: bool,
}

type ReverseIndex = BTreeMap<(String, Position), Vec<Position>>;

struct MapProjection {
    shape: MapShape,
    map: SourceMap,
    content: ContentId,
    encoded_bytes: usize,
    reverse: OnceLock<ReverseIndex>,
}

enum GeneratedProjection {
    Identity {
        resolved_url: String,
    },
    SourceMap {
        map_id: MapId,
    },
    Format {
        resolved_url: String,
        mapping: FormatProjection,
    },
}

struct FormatProjection {
    generated_index: LineIndex,
    formatted_index: LineIndex,
    points: Vec<(usize, usize)>,
}

pub struct ResolvedSourceView {
    policy: ResolutionPolicy,
    store: Arc<ContentStore>,
    source_files: SourceFileStore,
    source_graph: SourceGraph,
    workspace: BTreeMap<String, ContentId>,
    files: BTreeMap<String, ResolvedSourceFile>,
    generated: BTreeMap<String, GeneratedProjection>,
    generated_snapshots: BTreeMap<String, SourceSnapshotId>,
    resolved_snapshots: BTreeMap<(String, ContentId), SourceSnapshotId>,
    maps: Vec<MapProjection>,
    diagnostics: Vec<SourceDiagnostic>,
    reverse_index_builds: AtomicUsize,
}

impl ResolvedSourceView {
    pub fn new(
        policy: ResolutionPolicy,
        store: Arc<ContentStore>,
        workspace: BTreeMap<String, String>,
    ) -> Self {
        let workspace = workspace
            .into_iter()
            .map(|(logical_url, content)| (logical_url, store.intern(&content)))
            .collect();
        Self {
            policy,
            source_files: SourceFileStore::new(store.clone()),
            source_graph: SourceGraph::new(),
            store,
            workspace,
            files: BTreeMap::new(),
            generated: BTreeMap::new(),
            generated_snapshots: BTreeMap::new(),
            resolved_snapshots: BTreeMap::new(),
            maps: Vec::new(),
            diagnostics: Vec::new(),
            reverse_index_builds: AtomicUsize::new(0),
        }
    }

    pub fn files(&self) -> &BTreeMap<String, ResolvedSourceFile> {
        &self.files
    }

    pub fn diagnostics(&self) -> &[SourceDiagnostic] {
        &self.diagnostics
    }

    pub fn content_store(&self) -> &Arc<ContentStore> {
        &self.store
    }

    pub fn source_snapshot(&self, id: SourceSnapshotId) -> Option<&SourceSnapshot> {
        self.source_files.snapshot(id)
    }

    pub fn source_graph(&self) -> &SourceGraph {
        &self.source_graph
    }

    pub fn generated_snapshot(&self, generated_url: &str) -> Option<SourceSnapshotId> {
        self.generated_snapshots.get(generated_url).copied()
    }

    pub fn resolved_snapshot(&self, logical_url: &str) -> Option<SourceSnapshotId> {
        let file = self.files.get(logical_url)?;
        self.resolved_snapshots
            .get(&(logical_url.to_owned(), file.primary.content))
            .copied()
    }

    pub fn text(&self, logical_url: &str) -> Result<Arc<str>, SourceViewError> {
        let file = self
            .files
            .get(logical_url)
            .ok_or_else(|| SourceViewError::MissingContent(logical_url.into()))?;
        self.store
            .get(file.primary.content)
            .ok_or_else(|| SourceViewError::MissingContent(logical_url.into()))
    }

    pub fn add_generated(
        &mut self,
        input: GeneratedSourceInput<'_>,
    ) -> Result<(), SourceViewError> {
        if self.generated.contains_key(input.url) {
            return Err(SourceViewError::DuplicateGeneratedSource(input.url.into()));
        }

        let generated_content = self.store.intern(input.content);
        let generated_snapshot = self.register_generated_snapshot(input.url, generated_content)?;
        if let Some(raw_map) = input.source_map {
            match self.add_source_map(raw_map) {
                Ok(map_id) => {
                    self.add_mapped_files(input.url, generated_snapshot, map_id)?;
                    self.generated
                        .insert(input.url.into(), GeneratedProjection::SourceMap { map_id });
                    return Ok(());
                }
                Err(error) => {
                    self.diagnostics.push(SourceDiagnostic::SourceMapFailed {
                        generated_url: input.url.into(),
                        error: error.to_string(),
                    });
                }
            }
        }

        if input.minified {
            let (formatted, mapping) = format_minified(input.content);
            let formatted_content = self.store.intern(&formatted);
            let resolved_url = format!("{}?formatted", input.url);
            let candidates = vec![
                ContentCandidate {
                    content: formatted_content,
                    provenance: Provenance::Formatted {
                        generated_url: input.url.into(),
                    },
                },
                ContentCandidate {
                    content: generated_content,
                    provenance: Provenance::RuntimeSource {
                        url: input.url.into(),
                    },
                },
            ];
            let formatted_snapshot =
                self.register_resolved_candidates(&resolved_url, &candidates, formatted_content)?;
            self.merge_file(
                &resolved_url,
                SourceKind::FormattedFallback,
                candidates,
                ProjectionPath {
                    generated_url: input.url.into(),
                    content: formatted_content,
                    steps: vec![ProjectionStep::Format {
                        formatter: "prototype-js-whitespace-v1".into(),
                    }],
                },
            );
            self.source_graph.add_projection(
                formatted_snapshot,
                generated_snapshot,
                ProjectionKind::Format {
                    formatter: "prototype-js-whitespace-v1".into(),
                },
            )?;
            self.generated.insert(
                input.url.into(),
                GeneratedProjection::Format {
                    resolved_url,
                    mapping,
                },
            );
        } else {
            let resolved_url = input.url.to_owned();
            let candidates = vec![ContentCandidate {
                content: generated_content,
                provenance: Provenance::RuntimeSource {
                    url: input.url.into(),
                },
            }];
            let resolved_snapshot =
                self.register_resolved_candidates(&resolved_url, &candidates, generated_content)?;
            self.merge_file(
                &resolved_url,
                SourceKind::Identity,
                candidates,
                ProjectionPath {
                    generated_url: input.url.into(),
                    content: generated_content,
                    steps: vec![ProjectionStep::Identity],
                },
            );
            self.source_graph.add_projection(
                generated_snapshot,
                resolved_snapshot,
                ProjectionKind::Identity {
                    basis: IdentityBasis::EqualContent(generated_content),
                },
            )?;
            self.generated.insert(
                input.url.into(),
                GeneratedProjection::Identity { resolved_url },
            );
        }
        Ok(())
    }

    pub fn forward(&self, generated_url: &str, position: Position) -> Vec<CandidateLocation> {
        let Some(projection) = self.generated.get(generated_url) else {
            return Vec::new();
        };
        match projection {
            GeneratedProjection::Identity { resolved_url } => {
                self.direct_candidate(resolved_url, position)
            }
            GeneratedProjection::SourceMap { map_id } => {
                let map = &self.maps[map_id.0];
                let Some(token) = map.map.lookup_token(position.line, position.column) else {
                    return Vec::new();
                };
                let Some(source_url) = token.get_source() else {
                    return Vec::new();
                };
                let quality = if token.get_dst() == (position.line, position.column) {
                    MappingQuality::Exact
                } else {
                    MappingQuality::GreatestLowerBound
                };
                self.files
                    .get(source_url)
                    .and_then(|file| {
                        file.projection_paths
                            .iter()
                            .find(|path| path.generated_url == generated_url)
                            .map(|path| CandidateLocation {
                                source_url: source_url.into(),
                                position: Position {
                                    line: token.get_src_line(),
                                    column: token.get_src_col(),
                                },
                                quality,
                                projection: path.clone(),
                            })
                    })
                    .into_iter()
                    .collect()
            }
            GeneratedProjection::Format {
                resolved_url,
                mapping,
            } => mapping
                .forward(position)
                .into_iter()
                .flat_map(|position| self.direct_candidate(resolved_url, position))
                .collect(),
        }
    }

    pub fn reverse(&self, logical_url: &str, position: Position) -> Vec<CandidateLocation> {
        let Some(file) = self.files.get(logical_url) else {
            return Vec::new();
        };
        let mut candidates = Vec::new();
        for path in &file.projection_paths {
            let Some(generated) = self.generated.get(&path.generated_url) else {
                continue;
            };
            match generated {
                GeneratedProjection::Identity { .. } => candidates.push(CandidateLocation {
                    source_url: path.generated_url.clone(),
                    position,
                    quality: MappingQuality::Exact,
                    projection: path.clone(),
                }),
                GeneratedProjection::SourceMap { map_id } => {
                    let map = &self.maps[map_id.0];
                    let reverse = map.reverse.get_or_init(|| {
                        self.reverse_index_builds.fetch_add(1, Ordering::Relaxed);
                        build_reverse_index(&map.map)
                    });
                    let start = (logical_url.to_owned(), Position::ZERO);
                    let end = (logical_url.to_owned(), position);
                    if let Some(((source, original), generated_positions)) =
                        reverse.range(start..=end).next_back()
                    {
                        if source == logical_url {
                            let quality = if *original == position {
                                MappingQuality::Exact
                            } else {
                                MappingQuality::GreatestLowerBound
                            };
                            candidates.extend(generated_positions.iter().map(
                                |generated_position| CandidateLocation {
                                    source_url: path.generated_url.clone(),
                                    position: *generated_position,
                                    quality,
                                    projection: path.clone(),
                                },
                            ));
                        }
                    }
                }
                GeneratedProjection::Format { mapping, .. } => {
                    if let Some(generated_position) = mapping.reverse(position) {
                        candidates.push(CandidateLocation {
                            source_url: path.generated_url.clone(),
                            position: generated_position,
                            quality: MappingQuality::Exact,
                            projection: path.clone(),
                        });
                    }
                }
            }
        }
        candidates
    }

    pub fn resolve_edit_projection(&self, _edit_id: &str) -> Result<(), SourceViewError> {
        Err(SourceViewError::EditProjectionUnimplemented)
    }

    pub fn memory_report(&self) -> SourceMemoryReport {
        let mut map_tokens = 0;
        let mut encoded_map_bytes = 0;
        let mut reverse_positions = 0;
        for map in &self.maps {
            map_tokens += map.map.get_token_count() as usize;
            encoded_map_bytes += map.encoded_bytes;
            if let Some(reverse) = map.reverse.get() {
                reverse_positions += reverse.values().map(Vec::len).sum::<usize>();
            }
        }
        let format_mapping_points = self
            .generated
            .values()
            .map(|projection| match projection {
                GeneratedProjection::Format { mapping, .. } => mapping.points.len(),
                _ => 0,
            })
            .sum();
        let estimated_format_index_bytes = self
            .generated
            .values()
            .map(|projection| match projection {
                GeneratedProjection::Format { mapping, .. } => mapping.estimated_bytes(),
                _ => 0,
            })
            .sum();
        SourceMemoryReport {
            generated_sources: self.generated.len(),
            resolved_files: self.files.len(),
            maps: self.maps.len(),
            map_tokens,
            encoded_map_bytes,
            estimated_decoded_token_bytes: map_tokens * size_of::<RawToken>(),
            reverse_indexes_built: self.reverse_index_builds.load(Ordering::Relaxed),
            reverse_positions,
            format_mapping_points,
            estimated_format_index_bytes,
            content_store: self.store.stats(),
        }
    }

    fn add_source_map(&mut self, raw_map: &[u8]) -> Result<MapId, SourceViewError> {
        let decoded = decode_slice(raw_map)
            .map_err(|error| SourceViewError::InvalidSourceMap(error.to_string()))?;
        let shape = map_shape(raw_map);
        let map = match decoded {
            DecodedMap::Regular(map) => map,
            DecodedMap::Index(index) => index
                .flatten()
                .map_err(|error| SourceViewError::InvalidIndexedSourceMap(error.to_string()))?,
            DecodedMap::Hermes(_) => {
                return Err(SourceViewError::InvalidSourceMap(
                    "Hermes source maps are not part of this prototype".into(),
                ));
            }
        };
        let id = MapId(self.maps.len());
        let encoded = std::str::from_utf8(raw_map)
            .map_err(|error| SourceViewError::InvalidSourceMap(error.to_string()))?;
        let content = self.store.intern(encoded);
        self.maps.push(MapProjection {
            shape,
            map,
            content,
            encoded_bytes: raw_map.len(),
            reverse: OnceLock::new(),
        });
        Ok(id)
    }

    fn add_mapped_files(
        &mut self,
        generated_url: &str,
        generated_snapshot: SourceSnapshotId,
        map_id: MapId,
    ) -> Result<(), SourceViewError> {
        let map = &self.maps[map_id.0];
        let mut discovered = Vec::new();
        for source_id in 0..map.map.get_source_count() {
            let Some(logical_url) = map.map.get_source(source_id) else {
                continue;
            };
            let candidates = self.content_candidates(
                logical_url,
                map.map.get_source_contents(source_id),
                map_id,
            );
            if candidates.is_empty() {
                self.diagnostics.push(SourceDiagnostic::MissingContent {
                    logical_url: logical_url.into(),
                });
                continue;
            }
            let content = candidates[0].content;
            discovered.push((
                source_id,
                logical_url.to_owned(),
                candidates,
                ProjectionPath {
                    generated_url: generated_url.into(),
                    content,
                    steps: vec![ProjectionStep::SourceMap {
                        map_id,
                        shape: map.shape.clone(),
                    }],
                },
            ));
        }
        let map_content = self.maps[map_id.0].content;
        for (source_index, logical_url, candidates, path) in discovered {
            let resolved_snapshot =
                self.register_resolved_candidates(&logical_url, &candidates, path.content)?;
            self.merge_file(&logical_url, SourceKind::Authored, candidates, path);
            self.source_graph.add_projection(
                generated_snapshot,
                resolved_snapshot,
                ProjectionKind::SourceMap {
                    map: map_content,
                    source_index,
                },
            )?;
        }
        Ok(())
    }

    fn content_candidates(
        &self,
        logical_url: &str,
        sources_content: Option<&str>,
        map_id: MapId,
    ) -> Vec<ContentCandidate> {
        let embedded = sources_content.map(|content| ContentCandidate {
            content: self.store.intern(content),
            provenance: Provenance::SourcesContent { map_id },
        });
        let workspace = self
            .workspace
            .get(logical_url)
            .map(|content| ContentCandidate {
                content: *content,
                provenance: Provenance::Workspace {
                    logical_url: logical_url.into(),
                },
            });

        match (embedded, workspace) {
            (Some(embedded), Some(workspace)) if embedded.content == workspace.content => {
                match self.policy {
                    ResolutionPolicy::PreferSourcesContent => vec![embedded],
                    ResolutionPolicy::PreferWorkspaceIfMatching => vec![ContentCandidate {
                        content: embedded.content,
                        provenance: Provenance::VerifiedWorkspaceAndSourcesContent {
                            map_id,
                            logical_url: logical_url.into(),
                        },
                    }],
                    ResolutionPolicy::PreferWorkspaceAlways => vec![workspace],
                }
            }
            (Some(embedded), Some(workspace)) => match self.policy {
                ResolutionPolicy::PreferWorkspaceAlways => vec![workspace, embedded],
                ResolutionPolicy::PreferSourcesContent
                | ResolutionPolicy::PreferWorkspaceIfMatching => vec![embedded, workspace],
            },
            (Some(embedded), None) => vec![embedded],
            (None, Some(workspace)) => vec![workspace],
            (None, None) => Vec::new(),
        }
    }

    fn merge_file(
        &mut self,
        logical_url: &str,
        kind: SourceKind,
        candidates: Vec<ContentCandidate>,
        path: ProjectionPath,
    ) {
        let file = self
            .files
            .entry(logical_url.into())
            .or_insert_with(|| ResolvedSourceFile {
                logical_url: logical_url.into(),
                kind,
                primary: candidates[0].clone(),
                alternatives: Vec::new(),
                projection_paths: Vec::new(),
            });

        let mut by_content: HashMap<ContentId, ContentCandidate> =
            std::iter::once(file.primary.clone())
                .chain(file.alternatives.iter().cloned())
                .map(|candidate| (candidate.content, candidate))
                .collect();
        for candidate in candidates {
            by_content.entry(candidate.content).or_insert(candidate);
        }
        let mut all: Vec<_> = by_content.into_values().collect();
        all.sort_by_key(|candidate| {
            (
                provenance_rank(self.policy, &candidate.provenance),
                candidate.content,
            )
        });
        file.primary = all.remove(0);
        file.alternatives = all;
        file.projection_paths.push(path);
    }

    fn register_generated_snapshot(
        &mut self,
        generated_url: &str,
        content: ContentId,
    ) -> Result<SourceSnapshotId, SourceViewError> {
        let snapshot = self.source_files.intern_content(
            SourcePath::new(format!("runtime-source:{generated_url}"))
                .expect("provider prefix makes source path non-empty"),
            content,
        )?;
        self.source_graph.add_source(snapshot);
        self.generated_snapshots
            .insert(generated_url.to_owned(), snapshot);
        Ok(snapshot)
    }

    fn register_resolved_candidates(
        &mut self,
        logical_url: &str,
        candidates: &[ContentCandidate],
        projected_content: ContentId,
    ) -> Result<SourceSnapshotId, SourceViewError> {
        let path = SourcePath::new(format!("resolved-source:{logical_url}"))
            .expect("provider prefix makes source path non-empty");
        let mut projected = None;
        for candidate in candidates {
            let snapshot = self
                .source_files
                .intern_content(path.clone(), candidate.content)?;
            self.source_graph.add_source(snapshot);
            self.resolved_snapshots
                .insert((logical_url.to_owned(), candidate.content), snapshot);
            if candidate.content == projected_content {
                projected = Some(snapshot);
            }
        }
        Ok(projected.expect("projected content comes from candidates"))
    }

    fn direct_candidate(&self, resolved_url: &str, position: Position) -> Vec<CandidateLocation> {
        self.files
            .get(resolved_url)
            .and_then(|file| {
                file.projection_paths
                    .first()
                    .map(|projection| CandidateLocation {
                        source_url: resolved_url.into(),
                        position,
                        quality: MappingQuality::Exact,
                        projection: projection.clone(),
                    })
            })
            .into_iter()
            .collect()
    }
}

fn provenance_rank(policy: ResolutionPolicy, provenance: &Provenance) -> u8 {
    match provenance {
        Provenance::VerifiedWorkspaceAndSourcesContent { .. } => 0,
        Provenance::Workspace { .. } if policy == ResolutionPolicy::PreferWorkspaceAlways => 1,
        Provenance::SourcesContent { .. } => 2,
        Provenance::Workspace { .. } => 3,
        Provenance::Formatted { .. } => 4,
        Provenance::RuntimeSource { .. } => 5,
    }
}

fn build_reverse_index(map: &SourceMap) -> ReverseIndex {
    let mut reverse = BTreeMap::new();
    for token in map.tokens() {
        let Some(source) = token.get_source() else {
            continue;
        };
        reverse
            .entry((
                source.to_owned(),
                Position {
                    line: token.get_src_line(),
                    column: token.get_src_col(),
                },
            ))
            .or_insert_with(Vec::new)
            .push(Position {
                line: token.get_dst_line(),
                column: token.get_dst_col(),
            });
    }
    reverse
}

fn map_shape(raw_map: &[u8]) -> MapShape {
    let Ok(value) = serde_json::from_slice::<serde_json::Value>(raw_map) else {
        return MapShape::Ordinary;
    };
    let Some(sections) = value["sections"].as_array() else {
        return MapShape::Ordinary;
    };
    MapShape::Indexed {
        section_count: sections.len(),
        max_depth: indexed_depth(&value),
    }
}

fn indexed_depth(value: &serde_json::Value) -> usize {
    let Some(sections) = value["sections"].as_array() else {
        return 0;
    };
    1 + sections
        .iter()
        .filter_map(|section| section.get("map"))
        .map(indexed_depth)
        .max()
        .unwrap_or(0)
}

fn format_minified(source: &str) -> (String, FormatProjection) {
    let mut formatted = String::with_capacity(source.len() + source.len() / 4);
    let mut points = Vec::with_capacity(source.chars().count() + 1);
    let mut indent = 0usize;
    let mut at_line_start = true;

    for (input_offset, character) in source.char_indices() {
        if character == '}' {
            indent = indent.saturating_sub(1);
            ensure_newline(&mut formatted);
            at_line_start = true;
        }
        if at_line_start {
            formatted.push_str(&"  ".repeat(indent));
            at_line_start = false;
        }
        points.push((input_offset, formatted.len()));
        formatted.push(character);
        match character {
            '{' => {
                indent += 1;
                formatted.push('\n');
                at_line_start = true;
            }
            '}' | ';' => {
                formatted.push('\n');
                at_line_start = true;
            }
            _ => {}
        }
    }
    points.push((source.len(), formatted.len()));
    let generated_index = LineIndex::new(source);
    let formatted_index = LineIndex::new(&formatted);
    (
        formatted,
        FormatProjection {
            generated_index,
            formatted_index,
            points,
        },
    )
}

fn ensure_newline(output: &mut String) {
    if !output.is_empty() && !output.ends_with('\n') {
        output.push('\n');
    }
}

impl FormatProjection {
    fn forward(&self, position: Position) -> Option<Position> {
        let input = self.generated_index.byte_offset(position)?;
        let point = greatest_lower_bound(&self.points, input, |point| point.0)?;
        self.formatted_index.position(point.1)
    }

    fn reverse(&self, position: Position) -> Option<Position> {
        let output = self.formatted_index.byte_offset(position)?;
        let point = greatest_lower_bound(&self.points, output, |point| point.1)?;
        self.generated_index.position(point.0)
    }

    fn estimated_bytes(&self) -> usize {
        self.points.len() * size_of::<(usize, usize)>()
            + self.generated_index.estimated_bytes()
            + self.formatted_index.estimated_bytes()
    }
}

struct LineIndex {
    lines: Vec<LineRecord>,
}

struct LineRecord {
    start: usize,
    end: usize,
    utf16_len: u32,
    unicode_boundaries: Option<Vec<(u32, usize)>>,
}

impl LineIndex {
    fn new(text: &str) -> Self {
        let mut lines = Vec::new();
        let mut start = 0;
        let mut utf16_len = 0;
        let mut boundaries = Vec::new();
        let mut has_non_ascii = false;
        for (index, character) in text.char_indices() {
            if character == '\n' {
                boundaries.push((utf16_len, index));
                lines.push(LineRecord {
                    start,
                    end: index,
                    utf16_len,
                    unicode_boundaries: has_non_ascii.then_some(boundaries),
                });
                start = index + 1;
                utf16_len = 0;
                boundaries = Vec::new();
                has_non_ascii = false;
            } else {
                boundaries.push((utf16_len, index));
                utf16_len += character.len_utf16() as u32;
                has_non_ascii |= !character.is_ascii();
            }
        }
        boundaries.push((utf16_len, text.len()));
        lines.push(LineRecord {
            start,
            end: text.len(),
            utf16_len,
            unicode_boundaries: has_non_ascii.then_some(boundaries),
        });
        Self { lines }
    }

    fn byte_offset(&self, position: Position) -> Option<usize> {
        let line = self.lines.get(position.line as usize)?;
        if position.column > line.utf16_len {
            return None;
        }
        match &line.unicode_boundaries {
            Some(boundaries) => boundaries
                .binary_search_by_key(&position.column, |(column, _)| *column)
                .ok()
                .map(|index| boundaries[index].1),
            None => Some(line.start + position.column as usize),
        }
    }

    fn position(&self, byte_offset: usize) -> Option<Position> {
        let line_index = self.lines.partition_point(|line| line.start <= byte_offset) - 1;
        let line = self.lines.get(line_index)?;
        if byte_offset > line.end {
            return None;
        }
        let column = match &line.unicode_boundaries {
            Some(boundaries) => boundaries
                .binary_search_by_key(&byte_offset, |(_, byte)| *byte)
                .ok()
                .map(|index| boundaries[index].0)?,
            None => (byte_offset - line.start) as u32,
        };
        Some(Position {
            line: line_index as u32,
            column,
        })
    }

    fn estimated_bytes(&self) -> usize {
        self.lines.len() * size_of::<LineRecord>()
            + self
                .lines
                .iter()
                .filter_map(|line| line.unicode_boundaries.as_ref())
                .map(|boundaries| boundaries.len() * size_of::<(u32, usize)>())
                .sum::<usize>()
    }
}

fn greatest_lower_bound<T, K: Ord + Copy>(
    values: &[T],
    key: K,
    select: impl Fn(&T) -> K,
) -> Option<&T> {
    let index = values.partition_point(|value| select(value) <= key);
    index.checked_sub(1).and_then(|index| values.get(index))
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeSet;

    use super::*;
    use serde_json::json;
    use sourcemap::SourceMapBuilder;

    #[test]
    fn identity_source_is_the_resolved_view() {
        let mut view = empty_view(ResolutionPolicy::PreferSourcesContent);
        view.add_generated(GeneratedSourceInput {
            url: "file:///app.js",
            content: "const answer = 42;",
            source_map: None,
            minified: false,
        })
        .unwrap();

        let file = &view.files()["file:///app.js"];
        assert_eq!(file.kind, SourceKind::Identity);
        assert_eq!(&*view.text("file:///app.js").unwrap(), "const answer = 42;");
        assert_eq!(
            view.forward("file:///app.js", Position::ZERO)[0].position,
            Position::ZERO
        );
        let generated = view.generated_snapshot("file:///app.js").unwrap();
        let resolved = view.resolved_snapshot("file:///app.js").unwrap();
        let routes = view
            .source_graph()
            .find_routes(
                generated,
                &BTreeSet::from([resolved]),
                crate::source_graph::RouteLimits::default(),
            )
            .unwrap();
        let projection = view
            .source_graph()
            .projection(routes.routes[0].hops[0].projection)
            .unwrap();
        assert!(matches!(
            projection.kind,
            ProjectionKind::Identity {
                basis: IdentityBasis::EqualContent(_)
            }
        ));
    }

    #[test]
    fn ordinary_map_resolves_both_directions_lazily() {
        let map = regular_map(
            "src/app.ts",
            Some("let answer: number = 42;"),
            &[(0, 0, 0, 0), (0, 10, 0, 10)],
        );
        let mut view = empty_view(ResolutionPolicy::PreferSourcesContent);
        view.add_generated(GeneratedSourceInput {
            url: "file:///bundle.js",
            content: "var answer=42;",
            source_map: Some(&map),
            minified: false,
        })
        .unwrap();

        assert_eq!(view.memory_report().reverse_indexes_built, 0);
        let original = view.forward(
            "file:///bundle.js",
            Position {
                line: 0,
                column: 10,
            },
        );
        assert_eq!(original[0].source_url, "src/app.ts");
        assert_eq!(original[0].quality, MappingQuality::Exact);
        assert_eq!(view.memory_report().reverse_indexes_built, 0);

        let generated = view.reverse(
            "src/app.ts",
            Position {
                line: 0,
                column: 10,
            },
        );
        assert_eq!(generated[0].position.column, 10);
        assert_eq!(view.memory_report().reverse_indexes_built, 1);

        let runtime = view.generated_snapshot("file:///bundle.js").unwrap();
        let authored = view.resolved_snapshot("src/app.ts").unwrap();
        assert_eq!(
            view.source_snapshot(authored).unwrap().content_id(),
            Some(view.files()["src/app.ts"].primary.content)
        );
        let routes = view
            .source_graph()
            .find_routes(
                runtime,
                &BTreeSet::from([authored]),
                crate::source_graph::RouteLimits::default(),
            )
            .unwrap();
        assert_eq!(routes.routes.len(), 1);
        let projection = view
            .source_graph()
            .projection(routes.routes[0].hops[0].projection)
            .unwrap();
        assert!(matches!(
            projection.kind,
            ProjectionKind::SourceMap {
                source_index: 0,
                ..
            }
        ));
    }

    #[test]
    fn workspace_only_authored_source_is_a_graph_basis() {
        let map = regular_map("src/offline.ts", None, &[(0, 0, 0, 0)]);
        let mut workspace = BTreeMap::new();
        workspace.insert(
            "src/offline.ts".into(),
            "export const offline = true;".into(),
        );
        let mut view = ResolvedSourceView::new(
            ResolutionPolicy::PreferSourcesContent,
            Arc::new(ContentStore::default()),
            workspace,
        );
        view.add_generated(GeneratedSourceInput {
            url: "bundle.js",
            content: "const offline=true;",
            source_map: Some(&map),
            minified: false,
        })
        .unwrap();

        let generated = view.generated_snapshot("bundle.js").unwrap();
        let authored = view.resolved_snapshot("src/offline.ts").unwrap();
        let routes = view
            .source_graph()
            .find_routes(
                authored,
                &BTreeSet::from([generated]),
                crate::source_graph::RouteLimits::default(),
            )
            .unwrap();

        assert_eq!(routes.routes.len(), 1);
        assert!(matches!(
            view.source_graph()
                .projection(routes.routes[0].hops[0].projection)
                .unwrap()
                .kind,
            ProjectionKind::SourceMap { .. }
        ));
        assert_eq!(
            view.forward("bundle.js", Position::ZERO)[0].source_url,
            "src/offline.ts"
        );
    }

    #[test]
    fn approximate_source_map_lookups_are_labeled_as_glb() {
        let map = regular_map(
            "src/app.ts",
            Some("0123456789abcdef"),
            &[(0, 0, 0, 0), (0, 10, 0, 10)],
        );
        let mut view = empty_view(ResolutionPolicy::PreferSourcesContent);
        view.add_generated(GeneratedSourceInput {
            url: "bundle.js",
            content: "0123456789abcdef",
            source_map: Some(&map),
            minified: false,
        })
        .unwrap();

        let forward = view.forward("bundle.js", Position { line: 0, column: 5 });
        assert_eq!(forward[0].quality, MappingQuality::GreatestLowerBound);

        let reverse = view.reverse("src/app.ts", Position { line: 0, column: 5 });
        assert_eq!(reverse[0].quality, MappingQuality::GreatestLowerBound);
        assert_eq!(reverse[0].position, Position::ZERO);
    }

    #[test]
    fn stale_workspace_content_is_an_explicit_alternative() {
        let map = regular_map("src/app.ts", Some("runtime snapshot"), &[(0, 0, 0, 0)]);
        let mut workspace = BTreeMap::new();
        workspace.insert("src/app.ts".into(), "dirty workspace".into());
        let store = Arc::new(ContentStore::default());
        let mut view =
            ResolvedSourceView::new(ResolutionPolicy::PreferSourcesContent, store, workspace);
        view.add_generated(GeneratedSourceInput {
            url: "bundle.js",
            content: "compiled",
            source_map: Some(&map),
            minified: false,
        })
        .unwrap();

        let file = &view.files()["src/app.ts"];
        assert_eq!(&*view.text("src/app.ts").unwrap(), "runtime snapshot");
        assert_eq!(file.alternatives.len(), 1);
        assert!(matches!(
            file.alternatives[0].provenance,
            Provenance::Workspace { .. }
        ));
    }

    #[test]
    fn matching_workspace_policies_have_distinct_provenance() {
        let map = regular_map("src/app.ts", Some("matching"), &[(0, 0, 0, 0)]);
        for (policy, expected) in [
            (ResolutionPolicy::PreferSourcesContent, "sources"),
            (ResolutionPolicy::PreferWorkspaceIfMatching, "verified"),
            (ResolutionPolicy::PreferWorkspaceAlways, "workspace"),
        ] {
            let mut workspace = BTreeMap::new();
            workspace.insert("src/app.ts".into(), "matching".into());
            let mut view =
                ResolvedSourceView::new(policy, Arc::new(ContentStore::default()), workspace);
            view.add_generated(GeneratedSourceInput {
                url: "bundle.js",
                content: "compiled",
                source_map: Some(&map),
                minified: false,
            })
            .unwrap();
            let provenance = &view.files()["src/app.ts"].primary.provenance;
            assert!(
                matches!(
                    (expected, provenance),
                    ("sources", Provenance::SourcesContent { .. })
                        | (
                            "verified",
                            Provenance::VerifiedWorkspaceAndSourcesContent { .. }
                        )
                        | ("workspace", Provenance::Workspace { .. })
                ),
                "unexpected provenance for {policy:?}: {provenance:?}"
            );
        }
    }

    #[test]
    fn indexed_and_nested_indexed_maps_resolve_sections() {
        let first = regular_map_value("src/first.ts", "first");
        let second = regular_map_value("src/second.ts", "second");
        let nested = json!({
            "version": 3,
            "sections": [{
                "offset": { "line": 0, "column": 0 },
                "map": second,
            }]
        });
        let indexed = serde_json::to_vec(&json!({
            "version": 3,
            "sections": [
                {
                    "offset": { "line": 0, "column": 0 },
                    "map": first,
                },
                {
                    "offset": { "line": 1, "column": 0 },
                    "map": nested,
                }
            ]
        }))
        .unwrap();
        let mut view = empty_view(ResolutionPolicy::PreferSourcesContent);
        view.add_generated(GeneratedSourceInput {
            url: "bundle.js",
            content: "first();\nsecond();",
            source_map: Some(&indexed),
            minified: false,
        })
        .unwrap();

        assert_eq!(
            view.forward("bundle.js", Position { line: 0, column: 0 })[0].source_url,
            "src/first.ts"
        );
        assert_eq!(
            view.forward("bundle.js", Position { line: 1, column: 0 })[0].source_url,
            "src/second.ts"
        );
        assert!(matches!(
            view.files()["src/second.ts"].projection_paths[0].steps[0],
            ProjectionStep::SourceMap {
                shape: MapShape::Indexed { max_depth: 2, .. },
                ..
            }
        ));
        assert_eq!(view.source_graph().projection_count(), 2);
    }

    #[test]
    fn minified_fallback_is_a_bidirectional_projection() {
        let mut view = empty_view(ResolutionPolicy::PreferSourcesContent);
        view.add_generated(GeneratedSourceInput {
            url: "min.js",
            content: "function f(){return 42;}",
            source_map: None,
            minified: true,
        })
        .unwrap();

        let formatted = &view.files()["min.js?formatted"];
        assert_eq!(formatted.kind, SourceKind::FormattedFallback);
        assert!(view.text("min.js?formatted").unwrap().contains('\n'));
        let generated = Position {
            line: 0,
            column: 13,
        };
        let resolved = view.forward("min.js", generated)[0].position;
        assert_eq!(
            view.reverse("min.js?formatted", resolved)[0].position,
            generated
        );
        let runtime = view.generated_snapshot("min.js").unwrap();
        let formatted = view.resolved_snapshot("min.js?formatted").unwrap();
        let routes = view
            .source_graph()
            .find_routes(
                runtime,
                &BTreeSet::from([formatted]),
                crate::source_graph::RouteLimits::default(),
            )
            .unwrap();
        let projection = view
            .source_graph()
            .projection(routes.routes[0].hops[0].projection)
            .unwrap();
        assert!(matches!(projection.kind, ProjectionKind::Format { .. }));
        let report = view.memory_report();
        assert_eq!(report.content_store.unique_contents, 2);
        assert!(report.estimated_format_index_bytes > 0);
    }

    #[test]
    fn formatting_indexes_use_utf16_columns_without_retaining_text() {
        let index = LineIndex::new("a😀b");
        assert_eq!(index.byte_offset(Position { line: 0, column: 3 }), Some(5));
        assert_eq!(index.position(5), Some(Position { line: 0, column: 3 }));
        assert!(index.estimated_bytes() < "a😀b".len() + 128);
    }

    #[test]
    fn shared_authored_sources_reuse_content_but_keep_projection_paths() {
        let first = regular_map(
            "src/shared.ts",
            Some("export const x = 1;"),
            &[(0, 0, 0, 0)],
        );
        let second = first.clone();
        let store = Arc::new(ContentStore::default());
        let mut view = ResolvedSourceView::new(
            ResolutionPolicy::PreferSourcesContent,
            store,
            BTreeMap::new(),
        );
        for (url, generated, map) in [
            ("first.js", "const x=1;", first.as_slice()),
            ("second.js", "var x=1;", second.as_slice()),
        ] {
            view.add_generated(GeneratedSourceInput {
                url,
                content: generated,
                source_map: Some(map),
                minified: false,
            })
            .unwrap();
        }

        let file = &view.files()["src/shared.ts"];
        assert_eq!(file.projection_paths.len(), 2);
        assert_eq!(view.memory_report().content_store.unique_contents, 4);
        assert_eq!(view.memory_report().content_store.intern_requests, 6);
    }

    #[test]
    fn edit_projection_is_representable_but_inert() {
        let step = ProjectionStep::Edit {
            edit_id: "future-edit".into(),
        };
        assert!(matches!(step, ProjectionStep::Edit { .. }));
        assert!(matches!(
            empty_view(ResolutionPolicy::PreferSourcesContent)
                .resolve_edit_projection("future-edit"),
            Err(SourceViewError::EditProjectionUnimplemented)
        ));
    }

    #[test]
    fn large_map_reports_memory_without_building_reverse_index() {
        let mut builder = SourceMapBuilder::new(Some("large.js"));
        let source_id = builder.add_source("src/large.ts");
        builder.set_source_contents(source_id, Some(&"x".repeat(40_000)));
        for index in 0..10_000 {
            builder.add(
                index / 100,
                index % 100,
                index / 100,
                index % 100,
                Some("src/large.ts"),
                None,
                false,
            );
        }
        let mut raw = Vec::new();
        builder.into_sourcemap().to_writer(&mut raw).unwrap();
        let mut view = empty_view(ResolutionPolicy::PreferSourcesContent);
        view.add_generated(GeneratedSourceInput {
            url: "large.js",
            content: "x",
            source_map: Some(&raw),
            minified: false,
        })
        .unwrap();

        let before = view.memory_report();
        assert_eq!(before.map_tokens, 10_000);
        assert_eq!(before.reverse_indexes_built, 0);
        assert_eq!(before.reverse_positions, 0);
        assert_eq!(before.content_store.materializations, 0);
        assert!(before.estimated_decoded_token_bytes <= 10_000 * size_of::<RawToken>());
        let _ = view.reverse("src/large.ts", Position::ZERO);
        assert_eq!(view.memory_report().reverse_indexes_built, 1);
    }

    fn empty_view(policy: ResolutionPolicy) -> ResolvedSourceView {
        ResolvedSourceView::new(policy, Arc::new(ContentStore::default()), BTreeMap::new())
    }

    fn regular_map(
        source: &str,
        content: Option<&str>,
        mappings: &[(u32, u32, u32, u32)],
    ) -> Vec<u8> {
        let mut builder = SourceMapBuilder::new(Some("bundle.js"));
        let source_id = builder.add_source(source);
        builder.set_source_contents(source_id, content);
        for &(dst_line, dst_col, src_line, src_col) in mappings {
            builder.add(
                dst_line,
                dst_col,
                src_line,
                src_col,
                Some(source),
                None,
                false,
            );
        }
        let mut raw = Vec::new();
        builder.into_sourcemap().to_writer(&mut raw).unwrap();
        raw
    }

    fn regular_map_value(source: &str, content: &str) -> serde_json::Value {
        serde_json::from_slice(&regular_map(source, Some(content), &[(0, 0, 0, 0)])).unwrap()
    }
}
