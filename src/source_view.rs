use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::mem::size_of;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, OnceLock};

use oxc_allocator::Allocator;
use oxc_codegen::{Codegen, CodegenOptions};
use oxc_parser::Parser;
use oxc_span::SourceType;
use rayon::prelude::*;
use serde::{Deserialize, Serialize};
use sourcemap::{DecodedMap, RawToken, SourceMap, SourceMapIndex, decode_slice};

use crate::content_store::{ContentHash, ContentStore, ContentStoreStats, HashedBytes};
use crate::context_source_model::{ContextSourceModel, SourceContributionId, SourceSnapshotRole};
use crate::source_graph::{
    IdentityBasis, ProjectionId, ProjectionKind, RouteLimits, RouteSearch, SourceFileStoreError,
    SourceGraphError, SourceProjection, SourceSnapshot, SourceSnapshotId, SourceUri,
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
    pub content: ContentHash,
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
    pub content: ContentHash,
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
    FormattingFailed {
        generated_url: String,
        error: String,
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
    #[error("source formatting failed: {0}")]
    FormattingFailed(String),
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
    pub source_map_url: Option<&'a str>,
    pub minified: bool,
}

/// Serialized as bytes; clones share the digest and a one-shot decoded-map handoff.
#[derive(Clone, Serialize, Deserialize)]
#[serde(transparent)]
pub struct SourceMapData {
    bytes: HashedBytes,
    #[serde(skip)]
    prepared: Arc<Mutex<Option<Result<DecodedSourceMap, SourceViewError>>>>,
}

impl SourceMapData {
    pub fn new(bytes: impl Into<Arc<[u8]>>) -> Self {
        Self {
            bytes: HashedBytes::new(bytes),
            prepared: Arc::default(),
        }
    }

    pub(crate) fn is_supported(&self) -> bool {
        let decode = || {
            self.prepared
                .lock()
                .unwrap()
                .get_or_insert_with(|| DecodedSourceMap::decode(self.bytes.as_ref()))
                .is_ok()
        };
        if self.len() >= 64 * 1024 {
            rayon::join(decode, || self.content_hash()).0
        } else {
            decode()
        }
    }

    pub(crate) fn content_hash(&self) -> ContentHash {
        self.bytes.hash()
    }

    fn take_prepared(&self) -> Option<Result<DecodedSourceMap, SourceViewError>> {
        self.prepared.lock().unwrap().take()
    }
}

impl std::ops::Deref for SourceMapData {
    type Target = [u8];

    fn deref(&self) -> &[u8] {
        self.bytes.as_ref()
    }
}

impl std::fmt::Debug for SourceMapData {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SourceMapData")
            .field("encoded_bytes", &self.len())
            .finish_non_exhaustive()
    }
}

impl PartialEq for SourceMapData {
    fn eq(&self, other: &Self) -> bool {
        self.bytes.as_ref() == other.bytes.as_ref()
    }
}

impl Eq for SourceMapData {}

struct DecodedSourceMap {
    shape: MapShape,
    map: SourceMap,
}

impl DecodedSourceMap {
    fn decode(bytes: &[u8]) -> Result<Self, SourceViewError> {
        match decode_slice(bytes)
            .map_err(|error| SourceViewError::InvalidSourceMap(error.to_string()))?
        {
            DecodedMap::Regular(map) => Ok(Self {
                shape: MapShape::Ordinary,
                map,
            }),
            DecodedMap::Index(index) => Ok(Self {
                shape: MapShape::Indexed {
                    section_count: index.get_section_count() as usize,
                    max_depth: indexed_depth(&index),
                },
                map: index
                    .flatten()
                    .map_err(|error| SourceViewError::InvalidIndexedSourceMap(error.to_string()))?,
            }),
            DecodedMap::Hermes(_) => Err(SourceViewError::InvalidSourceMap(
                "Hermes source maps are not part of this prototype".into(),
            )),
        }
    }
}

pub(crate) fn appears_minified(source_url: &str, content: &str) -> bool {
    if source_url
        .split(['?', '#'])
        .next()
        .is_some_and(|url| url.ends_with(".min.js") || url.ends_with(".min.mjs"))
    {
        return true;
    }
    if content.len() < 256 {
        return false;
    }
    let mut line_count = 1_usize;
    let mut current_line = 0_usize;
    let mut longest_line = 0_usize;
    for byte in content.bytes() {
        if byte == b'\n' {
            line_count += 1;
            longest_line = longest_line.max(current_line);
            current_line = 0;
        } else {
            current_line += 1;
        }
    }
    longest_line = longest_line.max(current_line);
    longest_line >= 500 || line_count <= 2 && content.len() >= 1_024
}

type ReverseIndex = BTreeMap<(String, Position), Vec<Position>>;

pub(crate) struct MapProjection {
    shape: MapShape,
    map: SourceMap,
    content: ContentHash,
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
    model: Arc<ContextSourceModel>,
    contribution: SourceContributionId,
    store: Arc<ContentStore>,
    workspace: BTreeMap<String, ContentHash>,
    files: BTreeMap<String, ResolvedSourceFile>,
    generated: BTreeMap<String, GeneratedProjection>,
    generated_snapshots: BTreeMap<String, SourceSnapshotId>,
    resolved_snapshots: BTreeMap<(String, ContentHash), SourceSnapshotId>,
    maps: Vec<Arc<MapProjection>>,
    diagnostics: Vec<SourceDiagnostic>,
    reverse_index_builds: AtomicUsize,
}

impl ResolvedSourceView {
    pub fn new(
        policy: ResolutionPolicy,
        model: Arc<ContextSourceModel>,
        contribution: SourceContributionId,
        workspace: BTreeMap<String, String>,
    ) -> Self {
        let store = model.content_store().clone();
        let workspace = workspace
            .into_iter()
            .map(|(logical_url, content)| (logical_url, store.intern(&content)))
            .collect();
        Self {
            policy,
            model,
            contribution,
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

    pub fn source_snapshot(&self, id: SourceSnapshotId) -> Option<SourceSnapshot> {
        self.model.snapshot(id)
    }

    pub fn find_routes(
        &self,
        start: SourceSnapshotId,
        targets: &BTreeSet<SourceSnapshotId>,
        limits: RouteLimits,
    ) -> Result<RouteSearch, SourceGraphError> {
        self.model.find_routes(start, targets, limits)
    }

    pub fn projection(&self, id: ProjectionId) -> Option<SourceProjection> {
        self.model.projection(id)
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
        self.add_generated_with_prepared_map(input, None)
    }

    pub(crate) fn add_generated_with_prepared_map<'a>(
        &mut self,
        input: GeneratedSourceInput<'a>,
        source_map: Option<&'a SourceMapData>,
    ) -> Result<(), SourceViewError> {
        let input = GeneratedSourceInput {
            source_map: source_map.map(|map| &**map).or(input.source_map),
            ..input
        };
        if self.generated.contains_key(input.url) {
            return Err(SourceViewError::DuplicateGeneratedSource(input.url.into()));
        }

        let generated_content = self.store.intern(input.content);
        let generated_snapshot = self.register_generated_snapshot(input.url, generated_content)?;
        if let Some(raw_map) = input.source_map {
            match self.add_source_map(raw_map, source_map) {
                Ok(map_id) => {
                    self.add_mapped_files(
                        input.url,
                        generated_snapshot,
                        map_id,
                        input.source_map_url,
                    )?;
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

        let formatted = if input.minified {
            match format_minified(input.url, input.content) {
                Ok(formatted) => Some(formatted),
                Err(error) => {
                    self.diagnostics.push(SourceDiagnostic::FormattingFailed {
                        generated_url: input.url.into(),
                        error: error.to_string(),
                    });
                    None
                }
            }
        } else {
            None
        };
        if let Some((formatted, mapping)) = formatted {
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
            let formatted_snapshot = self.register_resolved_candidates(
                &resolved_url,
                SourceUri::parse(&resolved_url)
                    .or_else(|_| SourceUri::embedded("resolved", &resolved_url))
                    .expect("formatted source values can be represented"),
                &candidates,
                formatted_content,
            )?;
            self.merge_file(
                &resolved_url,
                SourceKind::FormattedFallback,
                candidates,
                ProjectionPath {
                    generated_url: input.url.into(),
                    content: formatted_content,
                    steps: vec![ProjectionStep::Format {
                        formatter: "oxc-codegen-0.146".into(),
                    }],
                },
            );
            self.model.add_projection(
                &self.contribution,
                formatted_snapshot,
                generated_snapshot,
                ProjectionKind::Format {
                    formatter: "oxc-codegen-0.146".into(),
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
            let resolved_snapshot = self.register_resolved_candidates(
                &resolved_url,
                SourceUri::parse(&resolved_url)
                    .or_else(|_| SourceUri::embedded("resolved", &resolved_url))
                    .expect("runtime source values can be represented"),
                &candidates,
                generated_content,
            )?;
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
            if generated_snapshot != resolved_snapshot {
                self.model.add_projection(
                    &self.contribution,
                    generated_snapshot,
                    resolved_snapshot,
                    ProjectionKind::Identity {
                        basis: IdentityBasis::EqualContent(generated_content),
                    },
                )?;
            }
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

    pub fn source_map_location(
        &self,
        generated_url: &str,
        position: Position,
    ) -> Option<(String, Position)> {
        let GeneratedProjection::SourceMap { map_id } = self.generated.get(generated_url)? else {
            return None;
        };
        let token = self.maps[map_id.0]
            .map
            .lookup_token(position.line, position.column)?;
        Some((
            token.get_source()?.to_owned(),
            Position {
                line: token.get_src_line(),
                column: token.get_src_col(),
            },
        ))
    }

    /// Selects the current projection, retaining authored coordinates even when
    /// the source map does not provide the authored content.
    pub(crate) fn preferred_generated_location(
        &self,
        generated_url: &str,
        position: Position,
    ) -> Option<(String, Position, SourceKind)> {
        if let Some(mapped) = self.forward(generated_url, position).into_iter().next() {
            let kind = self.files.get(&mapped.source_url)?.kind;
            return Some((mapped.source_url, mapped.position, kind));
        }
        self.source_map_location(generated_url, position)
            .map(|(url, position)| (url, position, SourceKind::Authored))
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

    fn add_source_map(
        &mut self,
        raw_map: &[u8],
        source_map: Option<&SourceMapData>,
    ) -> Result<MapId, SourceViewError> {
        let prepared = source_map.and_then(SourceMapData::take_prepared);
        let content = match source_map {
            Some(source_map) => self.store.intern_bytes(&source_map.bytes),
            None => std::str::from_utf8(raw_map).map(|encoded| self.store.intern(encoded)),
        }
        .map_err(|error| SourceViewError::InvalidSourceMap(error.to_string()))?;
        let projection =
            self.model
                .cached_source_map(content, || -> Result<_, SourceViewError> {
                    let decoded = prepared.unwrap_or_else(|| DecodedSourceMap::decode(raw_map))?;
                    Ok(MapProjection {
                        shape: decoded.shape,
                        map: decoded.map,
                        content,
                        encoded_bytes: raw_map.len(),
                        reverse: OnceLock::new(),
                    })
                })?;
        let id = MapId(self.maps.len());
        self.maps.push(projection);
        Ok(id)
    }

    fn add_mapped_files(
        &mut self,
        generated_url: &str,
        generated_snapshot: SourceSnapshotId,
        map_id: MapId,
        source_map_url: Option<&str>,
    ) -> Result<(), SourceViewError> {
        let map = &self.maps[map_id.0];
        let discovered = (0..map.map.get_source_count())
            .into_par_iter()
            .map(|source_id| {
                let logical_url = map.map.get_source(source_id)?;
                let candidates = self.content_candidates(
                    logical_url,
                    map.map.get_source_contents(source_id),
                    map_id,
                );
                if candidates.is_empty() {
                    return Some(Err(SourceDiagnostic::MissingContent {
                        logical_url: logical_url.into(),
                    }));
                }
                let content = candidates[0].content;
                Some(Ok((
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
                )))
            })
            .collect::<Vec<_>>();
        let discovered = discovered
            .into_iter()
            .flatten()
            .filter_map(|discovered| match discovered {
                Ok(discovered) => Some(discovered),
                Err(diagnostic) => {
                    self.diagnostics.push(diagnostic);
                    None
                }
            })
            .collect::<Vec<_>>();
        let map_content = self.maps[map_id.0].content;
        for (source_index, logical_url, candidates, path) in discovered {
            let resolved_snapshot = self.register_resolved_candidates(
                &logical_url,
                canonical_source_uri(source_map_url, &logical_url),
                &candidates,
                path.content,
            )?;
            self.merge_file(&logical_url, SourceKind::Authored, candidates, path);
            if generated_snapshot != resolved_snapshot {
                self.model.add_projection(
                    &self.contribution,
                    generated_snapshot,
                    resolved_snapshot,
                    ProjectionKind::SourceMap {
                        map: map_content,
                        source_index,
                    },
                )?;
            }
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

        let mut by_content: HashMap<ContentHash, ContentCandidate> =
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
        content: ContentHash,
    ) -> Result<SourceSnapshotId, SourceViewError> {
        let snapshot = self.model.intern_content(
            &self.contribution,
            SourceUri::parse(generated_url)
                .or_else(|_| SourceUri::embedded("runtime", generated_url))
                .expect("runtime source values can be represented"),
            content,
        )?;
        self.model
            .mark_snapshot_role(&self.contribution, snapshot, SourceSnapshotRole::Loaded)?;
        self.generated_snapshots
            .insert(generated_url.to_owned(), snapshot);
        Ok(snapshot)
    }

    fn register_resolved_candidates(
        &mut self,
        logical_url: &str,
        uri: SourceUri,
        candidates: &[ContentCandidate],
        projected_content: ContentHash,
    ) -> Result<SourceSnapshotId, SourceViewError> {
        let mut projected = None;
        for candidate in candidates {
            let snapshot =
                self.model
                    .intern_content(&self.contribution, uri.clone(), candidate.content)?;
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

pub(crate) fn canonical_source_uri(source_map_url: Option<&str>, logical_url: &str) -> SourceUri {
    if let Ok(uri) = SourceUri::parse(logical_url) {
        return uri;
    }
    if let Some(base) = source_map_url
        && let Ok(base) = url::Url::parse(base)
        && let Ok(uri) = base.join(&logical_url.replace('\\', "/"))
        && let Ok(uri) = SourceUri::parse(uri.as_str())
    {
        return uri;
    }
    SourceUri::embedded("resolved", logical_url).expect("logical source values can be embedded")
}

impl Drop for ResolvedSourceView {
    fn drop(&mut self) {
        self.maps.clear();
        self.model.release(&self.contribution);
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

fn indexed_depth(index: &SourceMapIndex) -> usize {
    1 + index
        .sections()
        .filter_map(|section| match section.get_sourcemap() {
            Some(DecodedMap::Index(index)) => Some(indexed_depth(index)),
            _ => None,
        })
        .max()
        .unwrap_or(0)
}

pub(crate) fn format_runtime_content(url: &str, source: &str) -> Result<String, SourceViewError> {
    format_minified(url, source).map(|(content, _)| content)
}

fn format_minified(url: &str, source: &str) -> Result<(String, FormatProjection), SourceViewError> {
    let allocator = Allocator::default();
    let source_path = url.split(['?', '#']).next().unwrap_or(url);
    let source_type = SourceType::from_path(source_path).unwrap_or_else(|_| SourceType::cjs());
    let parsed = Parser::new(&allocator, source, source_type).parse();
    let parsed = if parsed.panicked || !parsed.errors.is_empty() {
        Parser::new(&allocator, source, source_type.with_script(true)).parse()
    } else {
        parsed
    };
    if parsed.panicked || !parsed.errors.is_empty() {
        let message = parsed
            .errors
            .first()
            .map_or_else(|| "parser panicked".to_owned(), ToString::to_string);
        return Err(SourceViewError::FormattingFailed(message));
    }
    let mut options = CodegenOptions::default();
    options.source_map_path = Some(url.into());
    let generated = Codegen::new().with_options(options).build(&parsed.program);
    let formatted = generated.code;
    let generated_index = LineIndex::new(source);
    let formatted_index = LineIndex::new(&formatted);
    let mut points = vec![(0, 0)];
    if let Some(map) = generated.map {
        for token in map.get_tokens() {
            let Some(input_offset) = generated_index.byte_offset(Position {
                line: token.get_src_line(),
                column: token.get_src_col(),
            }) else {
                continue;
            };
            let Some(output_offset) = formatted_index.byte_offset(Position {
                line: token.get_dst_line(),
                column: token.get_dst_col(),
            }) else {
                continue;
            };
            if points
                .last()
                .is_none_or(|&(input, output)| input_offset >= input && output_offset >= output)
            {
                points.push((input_offset, output_offset));
            }
        }
    }
    points.push((source.len(), formatted.len()));
    points.sort_unstable();
    points.dedup();
    Ok((
        formatted,
        FormatProjection {
            generated_index,
            formatted_index,
            points,
        },
    ))
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

pub(crate) struct LineIndex {
    lines: Vec<LineRecord>,
}

struct LineRecord {
    start: usize,
    end: usize,
    utf16_start: u32,
    utf16_len: u32,
    unicode_characters: Vec<UnicodeCharacter>,
    trailing_cr: bool,
}

struct UnicodeCharacter {
    byte_start: usize,
    utf16_start: u32,
    byte_len: u8,
    utf16_len: u8,
}

impl LineIndex {
    pub(crate) fn new(text: &str) -> Self {
        let mut lines = Vec::new();
        let mut start = 0;
        let mut utf16_start = 0_u32;
        let mut utf16_len = 0;
        let mut unicode_characters = Vec::new();
        for (index, character) in text.char_indices() {
            if character == '\n' {
                lines.push(LineRecord {
                    start,
                    end: index,
                    utf16_start,
                    utf16_len,
                    unicode_characters,
                    trailing_cr: index > start && text.as_bytes()[index - 1] == b'\r',
                });
                start = index + 1;
                utf16_start = utf16_start.saturating_add(utf16_len).saturating_add(1);
                utf16_len = 0;
                unicode_characters = Vec::new();
            } else {
                if !character.is_ascii() {
                    unicode_characters.push(UnicodeCharacter {
                        byte_start: index,
                        utf16_start: utf16_len,
                        byte_len: character.len_utf8() as u8,
                        utf16_len: character.len_utf16() as u8,
                    });
                }
                utf16_len += character.len_utf16() as u32;
            }
        }
        lines.push(LineRecord {
            start,
            end: text.len(),
            utf16_start,
            utf16_len,
            unicode_characters,
            trailing_cr: text.ends_with('\r'),
        });
        Self { lines }
    }

    pub(crate) fn clamped_byte_offset(&self, position: Position) -> Option<usize> {
        let line = self.lines.get(position.line as usize)?;
        let column = position
            .column
            .min(line.utf16_len - u32::from(line.trailing_cr));
        line.byte_offset(column, true)
    }

    pub(crate) fn clamped_utf16_position(&self, offset: u32) -> Position {
        let line_index = self
            .lines
            .partition_point(|line| line.utf16_start <= offset)
            - 1;
        let line = &self.lines[line_index];
        let column = (offset - line.utf16_start).min(line.utf16_len);
        let byte = line
            .byte_offset(column, true)
            .expect("clamped columns have a byte offset");
        Position {
            line: line_index as u32,
            column: line
                .utf16_column(byte, false)
                .expect("rounded offsets are character boundaries"),
        }
    }

    pub(crate) fn clamped_byte_position(&self, offset: u32) -> Position {
        let offset = offset as usize;
        let line_index = self.lines.partition_point(|line| line.start <= offset) - 1;
        let line = &self.lines[line_index];
        Position {
            line: line_index as u32,
            column: line
                .utf16_column(offset.min(line.end), true)
                .expect("clamped byte offsets have a column"),
        }
    }

    fn byte_offset(&self, position: Position) -> Option<usize> {
        let line = self.lines.get(position.line as usize)?;
        line.byte_offset(position.column, false)
    }

    fn position(&self, byte_offset: usize) -> Option<Position> {
        let line_index = self.lines.partition_point(|line| line.start <= byte_offset) - 1;
        let line = self.lines.get(line_index)?;
        if byte_offset > line.end {
            return None;
        }
        let column = line.utf16_column(byte_offset, false)?;
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
                .map(|line| line.unicode_characters.len() * size_of::<UnicodeCharacter>())
                .sum::<usize>()
    }
}

impl LineRecord {
    fn byte_offset(&self, column: u32, round_surrogate_up: bool) -> Option<usize> {
        if column > self.utf16_len {
            return None;
        }
        let index = self
            .unicode_characters
            .partition_point(|c| c.utf16_start <= column);
        let Some(character) = index.checked_sub(1).map(|i| &self.unicode_characters[i]) else {
            return Some(self.start + column as usize);
        };
        let delta = column - character.utf16_start;
        if delta == 0 {
            Some(character.byte_start)
        } else if delta < u32::from(character.utf16_len) {
            round_surrogate_up.then_some(character.byte_start + usize::from(character.byte_len))
        } else {
            Some(
                character.byte_start
                    + usize::from(character.byte_len)
                    + (delta - u32::from(character.utf16_len)) as usize,
            )
        }
    }

    fn utf16_column(&self, byte_offset: usize, round_utf8_up: bool) -> Option<u32> {
        let index = self
            .unicode_characters
            .partition_point(|c| c.byte_start <= byte_offset);
        let Some(character) = index.checked_sub(1).map(|i| &self.unicode_characters[i]) else {
            return Some((byte_offset - self.start) as u32);
        };
        let delta = byte_offset - character.byte_start;
        if delta == 0 {
            Some(character.utf16_start)
        } else if delta < usize::from(character.byte_len) {
            round_utf8_up.then_some(character.utf16_start + u32::from(character.utf16_len))
        } else {
            Some(
                character.utf16_start
                    + u32::from(character.utf16_len)
                    + (delta - usize::from(character.byte_len)) as u32,
            )
        }
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
            source_map_url: None,
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
        assert_eq!(generated, resolved);
        let routes = view
            .find_routes(
                generated,
                &BTreeSet::from([resolved]),
                crate::source_graph::RouteLimits::default(),
            )
            .unwrap();
        assert!(routes.routes[0].hops.is_empty());
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
            source_map_url: None,
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
            view.source_snapshot(authored).unwrap().content_hash(),
            Some(view.files()["src/app.ts"].primary.content)
        );
        let routes = view
            .find_routes(
                runtime,
                &BTreeSet::from([authored]),
                crate::source_graph::RouteLimits::default(),
            )
            .unwrap();
        assert_eq!(routes.routes.len(), 1);
        let projection = view
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
        let mut view = view_with_workspace(ResolutionPolicy::PreferSourcesContent, workspace);
        view.add_generated(GeneratedSourceInput {
            url: "bundle.js",
            content: "const offline=true;",
            source_map: Some(&map),
            source_map_url: None,
            minified: false,
        })
        .unwrap();

        let generated = view.generated_snapshot("bundle.js").unwrap();
        let authored = view.resolved_snapshot("src/offline.ts").unwrap();
        let routes = view
            .find_routes(
                authored,
                &BTreeSet::from([generated]),
                crate::source_graph::RouteLimits::default(),
            )
            .unwrap();

        assert_eq!(routes.routes.len(), 1);
        assert!(matches!(
            view.projection(routes.routes[0].hops[0].projection)
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
            source_map_url: None,
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
    fn source_map_location_survives_missing_authored_content() {
        let mut builder = SourceMapBuilder::new(Some("bundle.js"));
        builder.add(0, 4, 10, 2, Some("src/app.ts"), None::<&str>, false);
        let mut bytes = Vec::new();
        builder.into_sourcemap().to_writer(&mut bytes).unwrap();
        let mut view = empty_view(ResolutionPolicy::PreferSourcesContent);
        view.add_generated(GeneratedSourceInput {
            url: "bundle.js",
            content: "call();",
            source_map: Some(&bytes),
            source_map_url: None,
            minified: false,
        })
        .unwrap();

        let generated = Position { line: 0, column: 4 };
        assert!(view.forward("bundle.js", generated).is_empty());
        assert_eq!(
            view.source_map_location("bundle.js", generated),
            Some((
                "src/app.ts".to_owned(),
                Position {
                    line: 10,
                    column: 2
                }
            ))
        );
    }

    #[test]
    fn stale_workspace_content_is_an_explicit_alternative() {
        let map = regular_map("src/app.ts", Some("runtime snapshot"), &[(0, 0, 0, 0)]);
        let mut workspace = BTreeMap::new();
        workspace.insert("src/app.ts".into(), "dirty workspace".into());
        let mut view = view_with_workspace(ResolutionPolicy::PreferSourcesContent, workspace);
        view.add_generated(GeneratedSourceInput {
            url: "bundle.js",
            content: "compiled",
            source_map: Some(&map),
            source_map_url: None,
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
            let mut view = view_with_workspace(policy, workspace);
            view.add_generated(GeneratedSourceInput {
                url: "bundle.js",
                content: "compiled",
                source_map: Some(&map),
                source_map_url: None,
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
            source_map_url: None,
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
        assert_eq!(view.model.graph_snapshot().projections.len(), 2);
    }

    #[test]
    fn decoded_sections_preserve_shape_for_empty_and_nested_indexes() {
        let empty = json!({ "version": 3, "sections": [] });
        let nested = json!({
            "version": 3,
            "sections": [
                { "offset": { "line": 0, "column": 0 }, "map": empty },
                { "offset": { "line": 1, "column": 0 }, "map": {
                    "version": 3,
                    "sections": [{ "offset": { "line": 0, "column": 0 }, "map": empty }]
                }}
            ]
        });
        assert_eq!(
            DecodedSourceMap::decode(&serde_json::to_vec(&empty).unwrap())
                .unwrap()
                .shape,
            MapShape::Indexed {
                section_count: 0,
                max_depth: 1
            },
        );
        assert_eq!(
            DecodedSourceMap::decode(&serde_json::to_vec(&nested).unwrap())
                .unwrap()
                .shape,
            MapShape::Indexed {
                section_count: 2,
                max_depth: 3
            },
        );
    }

    #[test]
    fn validated_source_maps_transfer_decoded_storage_without_pinning_it_in_captured_state() {
        let bytes = regular_map(
            "src/app.ts",
            Some("export const value = 1;"),
            &[(0, 0, 0, 0)],
        );
        let data = SourceMapData::new(bytes.clone());
        assert!(data.is_supported());
        let captured = data.clone();
        let contents_pointer = data
            .prepared
            .lock()
            .unwrap()
            .as_ref()
            .unwrap()
            .as_ref()
            .unwrap()
            .map
            .get_source_contents(0)
            .unwrap()
            .as_ptr();
        let encoded = serde_json::to_vec(&data).unwrap();
        assert_eq!(encoded, serde_json::to_vec(&bytes).unwrap());
        let restored: SourceMapData = serde_json::from_slice(&encoded).unwrap();
        assert_eq!(restored, data);
        assert!(restored.prepared.lock().unwrap().is_none());

        let model = Arc::new(ContextSourceModel::new());
        let mut views = Vec::new();
        for (index, source_map) in [&data, &restored].into_iter().enumerate() {
            assert!(source_map.is_supported());
            let mut view = ResolvedSourceView::new(
                ResolutionPolicy::PreferSourcesContent,
                model.clone(),
                SourceContributionId::new(format!("handoff-{index}")),
                BTreeMap::new(),
            );
            view.add_generated_with_prepared_map(
                GeneratedSourceInput {
                    url: "bundle.js",
                    content: "const value=1;",
                    source_map: None,
                    source_map_url: None,
                    minified: false,
                },
                Some(source_map),
            )
            .unwrap();
            assert!(source_map.prepared.lock().unwrap().is_none());
            assert_eq!(
                view.maps[0].map.get_source_contents(0).unwrap().as_ptr(),
                contents_pointer
            );
            views.push(view);
        }
        assert!(Arc::ptr_eq(&views[0].maps[0], &views[1].maps[0]));
        assert!(captured.prepared.lock().unwrap().is_none());
        drop(views);
        assert_eq!(model.decoded_source_map_count(), 0);
        assert_eq!(model.content_stats().unique_contents, 0);
        assert_eq!(&*captured, bytes.as_slice());
    }

    #[test]
    fn large_map_preparation_shares_the_digest_with_one_or_multiple_workers() {
        let bytes = regular_map("src/app.ts", Some(&"x".repeat(64 * 1024)), &[(0, 0, 0, 0)]);
        let expected = ContentHash::of_bytes(&bytes);
        for workers in [1, 4] {
            let pool = rayon::ThreadPoolBuilder::new()
                .num_threads(workers)
                .build()
                .unwrap();
            pool.install(|| {
                let data = SourceMapData::new(bytes.clone());
                assert!(data.is_supported());
                assert_eq!(data.content_hash(), expected);
                let mut view = empty_view(ResolutionPolicy::PreferSourcesContent);
                view.add_generated_with_prepared_map(
                    GeneratedSourceInput {
                        url: "bundle.js",
                        content: "x",
                        source_map: None,
                        source_map_url: None,
                        minified: false,
                    },
                    Some(&data),
                )
                .unwrap();
                assert_eq!(view.maps[0].content, expected);
                assert!(data.prepared.lock().unwrap().is_none());

                let invalid = SourceMapData::new(vec![b'!'; 64 * 1024]);
                assert!(!invalid.is_supported());
                assert_eq!(invalid.content_hash(), ContentHash::of_bytes(&invalid));
            });
        }
    }

    #[test]
    fn parallel_source_discovery_preserves_order_mapping_and_content_deduplication() {
        let mut builder = SourceMapBuilder::new(Some("bundle.js"));
        for index in 0..96 {
            let source = format!("src/{index}.ts");
            let source_id = builder.add_source(&source);
            if index % 3 != 0 {
                builder.set_source_contents(source_id, Some("shared authored content"));
            }
            builder.add(index, 0, 0, 0, Some(&source), None, false);
        }
        let mut bytes = Vec::new();
        builder.into_sourcemap().to_writer(&mut bytes).unwrap();
        for workers in [1, 4] {
            let pool = rayon::ThreadPoolBuilder::new()
                .num_threads(workers)
                .build()
                .unwrap();
            pool.install(|| {
                let mut view = empty_view(ResolutionPolicy::PreferSourcesContent);
                view.add_generated(GeneratedSourceInput {
                    url: "bundle.js",
                    content: "compiled",
                    source_map: Some(&bytes),
                    source_map_url: None,
                    minified: false,
                })
                .unwrap();
                assert_eq!(
                    view.diagnostics(),
                    (0..96)
                        .step_by(3)
                        .map(|index| SourceDiagnostic::MissingContent {
                            logical_url: format!("src/{index}.ts"),
                        })
                        .collect::<Vec<_>>(),
                );
                assert_eq!(view.files().len(), 64);
                assert_eq!(view.model.graph_snapshot().projections.len(), 64);
                assert_eq!(view.memory_report().content_store.unique_contents, 3);
                assert_eq!(view.memory_report().content_store.intern_requests, 66);
                for index in 0..96 {
                    assert_eq!(
                        view.source_map_location("bundle.js", Position { line: index, column: 0 }),
                        Some((format!("src/{index}.ts"), Position::ZERO)),
                    );
                }
            });
        }
    }

    #[test]
    fn invalid_prepared_maps_preserve_diagnostics_and_release_the_handoff() {
        let data = SourceMapData::new(b"<html>unavailable</html>".as_slice());
        assert!(!data.is_supported());
        let mut view = empty_view(ResolutionPolicy::PreferSourcesContent);
        view.add_generated_with_prepared_map(
            GeneratedSourceInput {
                url: "bundle.js",
                content: "const value=1;",
                source_map: None,
                source_map_url: None,
                minified: false,
            },
            Some(&data),
        )
        .unwrap();
        assert!(data.prepared.lock().unwrap().is_none());
        assert!(matches!(
            view.diagnostics(),
            [SourceDiagnostic::SourceMapFailed { .. }]
        ));
        assert_eq!(view.files()["bundle.js"].kind, SourceKind::Identity);
    }

    #[test]
    fn identical_maps_and_sources_are_shared_across_views() {
        let model = Arc::new(ContextSourceModel::new());
        let map = regular_map(
            "src/app.ts",
            Some("export const value = 1;"),
            &[(0, 0, 0, 0)],
        );
        let mut first = ResolvedSourceView::new(
            ResolutionPolicy::PreferSourcesContent,
            model.clone(),
            SourceContributionId::new("target-a"),
            BTreeMap::new(),
        );
        let mut second = ResolvedSourceView::new(
            ResolutionPolicy::PreferSourcesContent,
            model.clone(),
            SourceContributionId::new("target-b"),
            BTreeMap::new(),
        );
        for view in [&mut first, &mut second] {
            view.add_generated(GeneratedSourceInput {
                url: "https://example.test/app.js",
                content: "export const value=1;",
                source_map: Some(&map),
                source_map_url: None,
                minified: false,
            })
            .unwrap();
        }

        assert_eq!(model.decoded_source_map_count(), 1);
        assert_eq!(model.graph_snapshot().sources.len(), 2);
        assert_eq!(model.graph_snapshot().projections.len(), 1);

        drop(first);
        assert_eq!(model.graph_snapshot().sources.len(), 2);
        drop(second);
        assert!(model.graph_snapshot().sources.is_empty());
        assert_eq!(model.content_stats().unique_contents, 0);
    }

    #[test]
    fn source_map_identity_does_not_create_a_self_projection() {
        let model = Arc::new(ContextSourceModel::new());
        let content = "export const value = 1;";
        let map = regular_map("file:///app.js", Some(content), &[(0, 0, 0, 0)]);
        let mut view = ResolvedSourceView::new(
            ResolutionPolicy::PreferSourcesContent,
            model.clone(),
            SourceContributionId::new("identity-map"),
            BTreeMap::new(),
        );
        view.add_generated(GeneratedSourceInput {
            url: "file:///app.js",
            content,
            source_map: Some(&map),
            source_map_url: None,
            minified: false,
        })
        .unwrap();

        assert_eq!(
            view.generated_snapshot("file:///app.js"),
            view.resolved_snapshot("file:///app.js")
        );
        assert!(model.graph_snapshot().projections.is_empty());
    }

    #[test]
    fn minified_fallback_is_a_bidirectional_projection() {
        let mut view = empty_view(ResolutionPolicy::PreferSourcesContent);
        view.add_generated(GeneratedSourceInput {
            url: "min.js",
            content: "function f(){return 42;}",
            source_map: None,
            source_map_url: None,
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
            .find_routes(
                runtime,
                &BTreeSet::from([formatted]),
                crate::source_graph::RouteLimits::default(),
            )
            .unwrap();
        let projection = view
            .projection(routes.routes[0].hops[0].projection)
            .unwrap();
        assert!(matches!(projection.kind, ProjectionKind::Format { .. }));
        let report = view.memory_report();
        assert_eq!(report.content_store.unique_contents, 2);
        assert!(report.estimated_format_index_bytes > 0);
    }

    #[test]
    fn sparse_line_indexes_preserve_all_character_and_line_boundaries() {
        let alphabet = ["a", "é", "中", "😀", "\r", "\n"];
        let mut samples = vec![String::new()];
        for _ in 0..3 {
            samples = samples
                .into_iter()
                .flat_map(|prefix| {
                    alphabet
                        .iter()
                        .map(move |suffix| format!("{prefix}{suffix}"))
                })
                .collect();
        }
        samples.push(String::new());
        for text in samples {
            let index = LineIndex::new(&text);
            let mut start = 0;
            let mut utf16_start = 0;
            let mut global_boundaries = Vec::new();
            let mut line_count = 0;
            for (line_number, line) in text.split('\n').enumerate() {
                line_count += 1;
                let mut column = 0;
                let mut boundaries = Vec::new();
                for (byte, character) in line.char_indices() {
                    let position = Position {
                        line: line_number as u32,
                        column,
                    };
                    boundaries.push((column, start + byte));
                    global_boundaries.push((start + byte, utf16_start + column, position));
                    assert_eq!(index.byte_offset(position), Some(start + byte), "{text:?}");
                    assert_eq!(index.position(start + byte), Some(position), "{text:?}");
                    for inside in 1..character.len_utf8() {
                        assert_eq!(index.position(start + byte + inside), None, "{text:?}");
                    }
                    for inside in 1..character.len_utf16() {
                        assert_eq!(
                            index.byte_offset(Position {
                                column: column + inside as u32,
                                ..position
                            }),
                            None,
                            "{text:?}"
                        );
                    }
                    column += character.len_utf16() as u32;
                }
                boundaries.push((column, start + line.len()));
                let end = Position {
                    line: line_number as u32,
                    column,
                };
                global_boundaries.push((start + line.len(), utf16_start + column, end));
                assert_eq!(index.byte_offset(end), Some(start + line.len()), "{text:?}");
                assert_eq!(index.position(start + line.len()), Some(end), "{text:?}");
                let content_columns = line
                    .strip_suffix('\r')
                    .unwrap_or(line)
                    .encode_utf16()
                    .count() as u32;
                for requested in 0..=column + 2 {
                    let expected = boundaries
                        .iter()
                        .find(|(column, _)| *column >= requested.min(content_columns))
                        .unwrap()
                        .1;
                    assert_eq!(
                        index.clamped_byte_offset(Position {
                            column: requested,
                            ..end
                        }),
                        Some(expected),
                        "{text:?}"
                    );
                }
                assert_eq!(
                    index.byte_offset(Position {
                        column: column + 1,
                        ..end
                    }),
                    None
                );
                start += line.len() + 1;
                utf16_start += column + 1;
            }
            let last = global_boundaries.last().unwrap();
            for target in 0..=text.len() + 2 {
                let expected = global_boundaries
                    .iter()
                    .find(|(byte, _, _)| *byte >= target)
                    .unwrap_or(last)
                    .2;
                assert_eq!(
                    index.clamped_byte_position(target as u32),
                    expected,
                    "{text:?}"
                );
            }
            for target in 0..=text.encode_utf16().count() as u32 + 2 {
                let expected = global_boundaries
                    .iter()
                    .find(|(_, utf16, _)| *utf16 >= target)
                    .unwrap_or(last)
                    .2;
                assert_eq!(index.clamped_utf16_position(target), expected, "{text:?}");
            }
            assert_eq!(index.clamped_byte_position(u32::MAX), last.2);
            assert_eq!(index.clamped_utf16_position(u32::MAX), last.2);
            assert_eq!(index.position(text.len() + 1), None);
            assert_eq!(
                index.clamped_byte_offset(Position {
                    line: line_count,
                    column: 0
                }),
                None
            );
        }
    }

    #[test]
    fn sparse_line_index_storage_scales_with_unicode_not_ascii_text_length() {
        let ascii = "a".repeat(200_000);
        let plain = LineIndex::new(&ascii);
        assert_eq!(plain.lines[0].unicode_characters.capacity(), 0);
        let mixed = LineIndex::new(&format!("{ascii}😀{ascii}"));
        assert_eq!(mixed.lines[0].unicode_characters.len(), 1);
        assert!(mixed.estimated_bytes() < 256);
        assert_eq!(
            mixed.byte_offset(Position {
                line: 0,
                column: 200_001
            }),
            None
        );
        assert_eq!(
            mixed.clamped_byte_offset(Position {
                line: 0,
                column: 200_001
            }),
            Some(200_004)
        );
        assert_eq!(
            mixed.byte_offset(Position {
                line: 0,
                column: 400_002
            }),
            Some(400_004)
        );
    }

    #[test]
    fn formatting_indexes_use_utf16_columns_without_retaining_text() {
        let index = LineIndex::new("a😀b");
        assert_eq!(index.byte_offset(Position { line: 0, column: 3 }), Some(5));
        assert_eq!(index.position(5), Some(Position { line: 0, column: 3 }));
        assert!(index.estimated_bytes() < "a😀b".len() + 128);
    }

    #[test]
    fn clamped_positions_round_surrogates_up_and_exclude_crlf() {
        let index = LineIndex::new("a😀b\r\nc\r\n");
        let offsets = (0..=5)
            .map(|column| index.clamped_byte_offset(Position { line: 0, column }))
            .collect::<Vec<_>>();
        assert_eq!(
            offsets,
            vec![Some(0), Some(1), Some(5), Some(5), Some(6), Some(6)]
        );
        assert_eq!(
            index.clamped_byte_offset(Position {
                line: 1,
                column: 99
            }),
            Some(9)
        );
        assert_eq!(
            index.clamped_byte_offset(Position {
                line: 2,
                column: 99
            }),
            Some(11)
        );
        assert_eq!(
            index.clamped_byte_offset(Position { line: 3, column: 0 }),
            None
        );
        assert_eq!(
            LineIndex::new("").clamped_byte_offset(Position {
                line: 0,
                column: 99
            }),
            Some(0)
        );
    }

    #[test]
    fn shared_authored_sources_reuse_content_but_keep_projection_paths() {
        let first = regular_map(
            "src/shared.ts",
            Some("export const x = 1;"),
            &[(0, 0, 0, 0)],
        );
        let second = first.clone();
        let mut view = view_with_workspace(ResolutionPolicy::PreferSourcesContent, BTreeMap::new());
        for (url, generated, map) in [
            ("first.js", "const x=1;", first.as_slice()),
            ("second.js", "var x=1;", second.as_slice()),
        ] {
            view.add_generated(GeneratedSourceInput {
                url,
                content: generated,
                source_map: Some(map),
                source_map_url: None,
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
            source_map_url: None,
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

    #[test]
    fn source_map_paths_use_the_resolved_map_url_as_their_base() {
        let map = regular_map(
            "../../../src/vs/nls.ts",
            Some("export const message = 'hello';"),
            &[(0, 0, 0, 0)],
        );
        let mut view = empty_view(ResolutionPolicy::PreferSourcesContent);
        view.add_generated(GeneratedSourceInput {
            url: "https://main.vscode-cdn.net/stable/commit/out/vs/workbench/workbench.js",
            content: "const message='hello';",
            source_map: Some(&map),
            source_map_url: Some(
                "https://main.vscode-cdn.net/sourcemaps/commit/core/vs/workbench/workbench.js.map",
            ),
            minified: false,
        })
        .unwrap();

        let source = view
            .resolved_snapshot("../../../src/vs/nls.ts")
            .and_then(|snapshot| view.source_snapshot(snapshot))
            .unwrap();
        assert_eq!(
            source.uri.as_str(),
            "https://main.vscode-cdn.net/sourcemaps/commit/src/vs/nls.ts"
        );
    }

    #[test]
    fn oxc_formatting_preserves_javascript_tokens_and_maps_both_directions() {
        let source = r#"const text="};";function add(a,b){return a+b;}console.log(text,add(1,2));"#;
        let mut view = empty_view(ResolutionPolicy::PreferSourcesContent);
        view.add_generated(GeneratedSourceInput {
            url: "bundle.min.js",
            content: source,
            source_map: None,
            source_map_url: None,
            minified: true,
        })
        .unwrap();

        let formatted = view.text("bundle.min.js?formatted").unwrap();
        assert!(formatted.contains(r#"const text = "};";"#));
        assert!(formatted.lines().count() > 1);
        let generated = Position {
            line: 0,
            column: source.find("return").unwrap() as u32,
        };
        let projected = view.forward("bundle.min.js", generated);
        assert_eq!(projected.len(), 1);
        assert_eq!(projected[0].source_url, "bundle.min.js?formatted");
        assert_eq!(
            view.reverse("bundle.min.js?formatted", projected[0].position)[0].position,
            generated
        );
    }

    #[test]
    fn invalid_javascript_falls_back_to_the_original_source() {
        let mut view = empty_view(ResolutionPolicy::PreferSourcesContent);
        view.add_generated(GeneratedSourceInput {
            url: "broken.min.js",
            content: "function {",
            source_map: None,
            source_map_url: None,
            minified: true,
        })
        .unwrap();

        assert_eq!(&*view.text("broken.min.js").unwrap(), "function {");
        assert!(matches!(
            view.diagnostics(),
            [SourceDiagnostic::FormattingFailed { .. }]
        ));
    }

    fn empty_view(policy: ResolutionPolicy) -> ResolvedSourceView {
        view_with_workspace(policy, BTreeMap::new())
    }

    fn view_with_workspace(
        policy: ResolutionPolicy,
        workspace: BTreeMap<String, String>,
    ) -> ResolvedSourceView {
        ResolvedSourceView::new(
            policy,
            Arc::new(ContextSourceModel::new()),
            SourceContributionId::new("test-view"),
            workspace,
        )
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
