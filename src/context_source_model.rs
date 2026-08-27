use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::sync::{Arc, Mutex, Weak};

use crate::content_store::{ContentHash, ContentStore, ContentStoreStats};
use crate::source_graph::{
    IdentityBasis, ProjectionId, ProjectionKind, RouteLimits, RouteSearch, SourceFileStore,
    SourceFileStoreError, SourceGraph, SourceGraphError, SourceProjection, SourceSnapshot,
    SourceSnapshotId, SourceUri,
};
use crate::source_view::MapProjection;

#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct SourceContributionId(String);

impl SourceContributionId {
    pub fn new(value: impl Into<String>) -> Self {
        Self(value.into())
    }
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
struct SourceContribution {
    snapshots: BTreeSet<SourceSnapshotId>,
    projections: BTreeSet<ProjectionId>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ContextSourceGraphSnapshot {
    pub sources: Vec<SourceSnapshot>,
    pub projections: Vec<SourceProjection>,
}

#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum CompactedProjectionKind {
    Identity,
    SourceMap,
    Format(String),
    Edit(String),
    Offset { line_delta: i64, column_delta: i64 },
}

#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub struct SuffixRewrite {
    pub from: String,
    pub to: String,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CompactedSourceNode {
    pub id: u32,
    pub prefix: SourceUri,
    pub source_count: usize,
    pub runtime_internal: bool,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CompactedSourceEdge {
    pub derived: u32,
    pub basis: u32,
    pub kind: CompactedProjectionKind,
    pub mapping_count: usize,
    pub fan_out: bool,
    pub suffix_rewrite: Option<SuffixRewrite>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CompactedSourceGraph {
    pub roots: Vec<u32>,
    pub nodes: Vec<CompactedSourceNode>,
    pub edges: Vec<CompactedSourceEdge>,
}

pub struct ContextSourceModel {
    content: Arc<ContentStore>,
    state: Mutex<ContextSourceState>,
    decoded_maps: Mutex<HashMap<ContentHash, Weak<MapProjection>>>,
}

struct ContextSourceState {
    files: SourceFileStore,
    graph: SourceGraph,
    contributions: BTreeMap<SourceContributionId, SourceContribution>,
    snapshot_references: BTreeMap<SourceSnapshotId, usize>,
    projection_references: BTreeMap<ProjectionId, usize>,
}

impl Default for ContextSourceModel {
    fn default() -> Self {
        Self::new()
    }
}

impl ContextSourceModel {
    pub fn new() -> Self {
        let content = Arc::new(ContentStore::default());
        Self {
            state: Mutex::new(ContextSourceState {
                files: SourceFileStore::new(content.clone()),
                graph: SourceGraph::new(),
                contributions: BTreeMap::new(),
                snapshot_references: BTreeMap::new(),
                projection_references: BTreeMap::new(),
            }),
            content,
            decoded_maps: Mutex::new(HashMap::new()),
        }
    }

    pub fn content_store(&self) -> &Arc<ContentStore> {
        &self.content
    }

    pub fn intern_content(
        &self,
        contribution: &SourceContributionId,
        uri: SourceUri,
        content: ContentHash,
    ) -> Result<SourceSnapshotId, SourceFileStoreError> {
        let mut state = self.state.lock().unwrap();
        let snapshot = state.files.intern_content(uri, content)?;
        state.graph.add_source(snapshot);
        let inserted = state
            .contributions
            .entry(contribution.clone())
            .or_default()
            .snapshots
            .insert(snapshot);
        if inserted {
            *state.snapshot_references.entry(snapshot).or_default() += 1;
        }
        Ok(snapshot)
    }

    pub fn add_projection(
        &self,
        contribution: &SourceContributionId,
        derived: SourceSnapshotId,
        basis: SourceSnapshotId,
        kind: ProjectionKind,
    ) -> Result<ProjectionId, SourceGraphError> {
        let mut state = self.state.lock().unwrap();
        if let ProjectionKind::SourceMap { map, .. } = &kind
            && let Some(existing) = state.graph.projections().find_map(|projection| {
                if projection.derived != derived {
                    return None;
                }
                match projection.kind {
                    ProjectionKind::SourceMap {
                        map: existing_map, ..
                    } if existing_map != *map => Some(existing_map),
                    _ => None,
                }
            })
        {
            return Err(SourceGraphError::ConflictingSourceMap {
                snapshot: derived,
                existing,
                contributed: *map,
            });
        }
        let projection = state.graph.add_projection(derived, basis, kind)?;
        let inserted = state
            .contributions
            .entry(contribution.clone())
            .or_default()
            .projections
            .insert(projection);
        if inserted {
            *state.projection_references.entry(projection).or_default() += 1;
        }
        Ok(projection)
    }

    pub fn snapshot(&self, id: SourceSnapshotId) -> Option<SourceSnapshot> {
        self.state.lock().unwrap().files.snapshot(id).cloned()
    }

    pub fn projection(&self, id: ProjectionId) -> Option<SourceProjection> {
        self.state.lock().unwrap().graph.projection(id).cloned()
    }

    pub fn find_routes(
        &self,
        start: SourceSnapshotId,
        targets: &BTreeSet<SourceSnapshotId>,
        limits: RouteLimits,
    ) -> Result<RouteSearch, SourceGraphError> {
        self.state
            .lock()
            .unwrap()
            .graph
            .find_routes(start, targets, limits)
    }

    pub fn graph_snapshot(&self) -> ContextSourceGraphSnapshot {
        let state = self.state.lock().unwrap();
        ContextSourceGraphSnapshot {
            sources: state.files.snapshots().cloned().collect(),
            projections: state.graph.projections().cloned().collect(),
        }
    }

    pub fn compacted_graph(&self) -> CompactedSourceGraph {
        compact_source_graph(&self.graph_snapshot())
    }
}

#[derive(Clone)]
struct ConcreteMapping {
    derived: SourceUri,
    basis: SourceUri,
    kind: CompactedProjectionKind,
}

#[derive(Clone)]
struct MappingGroup {
    mappings: Vec<ConcreteMapping>,
    derived_prefix: SourceUri,
    basis_prefix: SourceUri,
    kind: CompactedProjectionKind,
    fan_out: bool,
    suffix_rewrite: Option<SuffixRewrite>,
}

fn compact_source_graph(graph: &ContextSourceGraphSnapshot) -> CompactedSourceGraph {
    let sources = graph
        .sources
        .iter()
        .map(|source| (source.id, source.uri.clone()))
        .collect::<BTreeMap<_, _>>();
    let concrete = graph
        .projections
        .iter()
        .filter_map(|projection| {
            Some(ConcreteMapping {
                derived: sources.get(&projection.derived)?.clone(),
                basis: sources.get(&projection.basis)?.clone(),
                kind: compacted_projection_kind(&projection.kind),
            })
        })
        .collect::<Vec<_>>();
    let mut groups = concrete
        .iter()
        .cloned()
        .map(|mapping| {
            group_mappings(vec![mapping], &concrete)
                .expect("a concrete source mapping always has a compact representation")
        })
        .collect::<Vec<_>>();

    loop {
        let mut merged = None;
        'outer: for left in 0..groups.len() {
            for right in left + 1..groups.len() {
                if groups[left].kind != groups[right].kind {
                    continue;
                }
                let mappings = groups[left]
                    .mappings
                    .iter()
                    .chain(&groups[right].mappings)
                    .cloned()
                    .collect();
                if let Some(group) = group_mappings(mappings, &concrete) {
                    merged = Some((left, right, group));
                    break 'outer;
                }
            }
        }
        let Some((left, right, group)) = merged else {
            break;
        };
        groups.remove(right);
        groups[left] = group;
    }

    groups.sort_by(|left, right| {
        (
            &left.derived_prefix,
            &left.basis_prefix,
            &left.kind,
            left.fan_out,
            &left.suffix_rewrite,
        )
            .cmp(&(
                &right.derived_prefix,
                &right.basis_prefix,
                &right.kind,
                right.fan_out,
                &right.suffix_rewrite,
            ))
    });
    let prefixes = groups
        .iter()
        .flat_map(|group| [group.derived_prefix.clone(), group.basis_prefix.clone()])
        .collect::<BTreeSet<_>>();
    let prefix_ids = prefixes
        .iter()
        .cloned()
        .enumerate()
        .map(|(index, prefix)| (prefix, index as u32 + 1))
        .collect::<BTreeMap<_, _>>();
    let nodes = prefixes
        .iter()
        .map(|prefix| {
            let source_count = groups
                .iter()
                .flat_map(|group| &group.mappings)
                .flat_map(|mapping| [&mapping.derived, &mapping.basis])
                .filter(|uri| relative_source_path(uri, prefix).is_some())
                .collect::<BTreeSet<_>>()
                .len();
            CompactedSourceNode {
                id: prefix_ids[prefix],
                prefix: prefix.clone(),
                source_count,
                runtime_internal: is_runtime_internal(prefix),
            }
        })
        .collect::<Vec<_>>();
    let edges = groups
        .into_iter()
        .map(|group| CompactedSourceEdge {
            derived: prefix_ids[&group.derived_prefix],
            basis: prefix_ids[&group.basis_prefix],
            kind: group.kind,
            mapping_count: group.mappings.len(),
            fan_out: group.fan_out,
            suffix_rewrite: group.suffix_rewrite,
        })
        .collect::<Vec<_>>();
    let referenced = edges.iter().map(|edge| edge.basis).collect::<BTreeSet<_>>();
    let mut roots = nodes
        .iter()
        .map(|node| node.id)
        .filter(|node| !referenced.contains(node))
        .collect::<Vec<_>>();
    if roots.is_empty() {
        roots.extend(nodes.iter().map(|node| node.id));
    }
    CompactedSourceGraph {
        roots,
        nodes,
        edges,
    }
}

fn group_mappings(
    mappings: Vec<ConcreteMapping>,
    universe: &[ConcreteMapping],
) -> Option<MappingGroup> {
    corresponding_mapping_group(mappings.clone(), universe)
        .or_else(|| fan_out_mapping_group(mappings, universe))
}

fn corresponding_mapping_group(
    mappings: Vec<ConcreteMapping>,
    universe: &[ConcreteMapping],
) -> Option<MappingGroup> {
    let kind = mappings.first()?.kind.clone();
    if mappings.iter().any(|mapping| mapping.kind != kind) {
        return None;
    }
    let mut derived_prefix = common_parent(mappings.iter().map(|mapping| &mapping.derived))?;
    let mut basis_prefix = common_parent(mappings.iter().map(|mapping| &mapping.basis))?;
    let mut suffix_rewrite = mappings[0]
        .derived
        .relative_path_from(&derived_prefix)
        .zip(mappings[0].basis.relative_path_from(&basis_prefix))
        .and_then(|(derived, basis)| mapping_rewrite(&derived, &basis));
    if suffix_rewrite.is_none() && mappings.len() == 1 {
        derived_prefix = mappings[0].derived.clone();
        basis_prefix = mappings[0].basis.clone();
        suffix_rewrite = Some(None);
    }
    let mut suffix_rewrite = suffix_rewrite?;
    if !group_is_valid(
        &mappings,
        universe,
        &kind,
        &derived_prefix,
        &basis_prefix,
        suffix_rewrite.as_ref(),
    ) {
        if mappings.len() != 1 {
            return None;
        }
        derived_prefix = mappings[0].derived.clone();
        basis_prefix = mappings[0].basis.clone();
        suffix_rewrite = None;
    }
    Some(MappingGroup {
        mappings,
        derived_prefix,
        basis_prefix,
        kind,
        fan_out: false,
        suffix_rewrite,
    })
}

fn fan_out_mapping_group(
    mappings: Vec<ConcreteMapping>,
    universe: &[ConcreteMapping],
) -> Option<MappingGroup> {
    if mappings.len() < 2 {
        return None;
    }
    let first = mappings.first()?;
    let derived = first.derived.clone();
    let kind = first.kind.clone();
    if first.kind != CompactedProjectionKind::SourceMap
        || mappings
            .iter()
            .any(|mapping| mapping.kind != first.kind || mapping.derived != first.derived)
    {
        return None;
    }
    let basis_prefix = common_parent(mappings.iter().map(|mapping| &mapping.basis))?;
    if universe.iter().any(|mapping| {
        mapping.kind == first.kind
            && relative_source_path(&mapping.basis, &basis_prefix).is_some()
            && mapping.derived != first.derived
    }) {
        return None;
    }
    Some(MappingGroup {
        mappings,
        derived_prefix: derived,
        basis_prefix,
        kind,
        fan_out: true,
        suffix_rewrite: None,
    })
}

fn group_is_valid(
    mappings: &[ConcreteMapping],
    universe: &[ConcreteMapping],
    kind: &CompactedProjectionKind,
    derived_prefix: &SourceUri,
    basis_prefix: &SourceUri,
    suffix_rewrite: Option<&SuffixRewrite>,
) -> bool {
    mappings
        .iter()
        .all(|mapping| mapping_matches(mapping, derived_prefix, basis_prefix, suffix_rewrite))
        && universe.iter().all(|mapping| {
            mapping.kind != *kind
                || relative_source_path(&mapping.derived, derived_prefix).is_none()
                || relative_source_path(&mapping.basis, basis_prefix).is_none()
                || mapping_matches(mapping, derived_prefix, basis_prefix, suffix_rewrite)
        })
}

fn common_parent<'a>(uris: impl Iterator<Item = &'a SourceUri>) -> Option<SourceUri> {
    let mut uris = uris;
    let first = uris.next()?;
    let mut common = first.parent().unwrap_or_else(|| first.clone());
    for uri in uris {
        let candidate = uri.parent().unwrap_or_else(|| uri.clone());
        common = common.common_ancestor(&candidate)?;
    }
    Some(common)
}

fn mapping_matches(
    mapping: &ConcreteMapping,
    derived_prefix: &SourceUri,
    basis_prefix: &SourceUri,
    rewrite: Option<&SuffixRewrite>,
) -> bool {
    let Some(derived) = relative_source_path(&mapping.derived, derived_prefix) else {
        return false;
    };
    let Some(basis) = relative_source_path(&mapping.basis, basis_prefix) else {
        return false;
    };
    rewrite_relative_path(&derived, rewrite) == basis
}

fn relative_source_path(uri: &SourceUri, prefix: &SourceUri) -> Option<String> {
    if uri == prefix {
        Some(String::new())
    } else {
        uri.relative_path_from(prefix)
    }
}

fn mapping_rewrite(derived: &str, basis: &str) -> Option<Option<SuffixRewrite>> {
    if derived == basis {
        return Some(None);
    }
    let (derived_stem, derived_suffix) = split_suffix(derived)?;
    let (basis_stem, basis_suffix) = split_suffix(basis)?;
    (derived_stem == basis_stem).then(|| {
        Some(SuffixRewrite {
            from: derived_suffix.to_owned(),
            to: basis_suffix.to_owned(),
        })
    })
}

fn split_suffix(path: &str) -> Option<(&str, &str)> {
    let slash = path.rfind('/').map_or(0, |index| index + 1);
    let dot = path[slash..].rfind('.').map(|index| slash + index)?;
    Some((&path[..dot], &path[dot..]))
}

fn rewrite_relative_path(path: &str, rewrite: Option<&SuffixRewrite>) -> String {
    let Some(rewrite) = rewrite else {
        return path.to_owned();
    };
    path.strip_suffix(&rewrite.from)
        .map(|stem| format!("{stem}{}", rewrite.to))
        .unwrap_or_else(|| path.to_owned())
}

fn compacted_projection_kind(kind: &ProjectionKind) -> CompactedProjectionKind {
    match kind {
        ProjectionKind::Identity { .. } => CompactedProjectionKind::Identity,
        ProjectionKind::SourceMap { .. } => CompactedProjectionKind::SourceMap,
        ProjectionKind::Format { formatter } => CompactedProjectionKind::Format(formatter.clone()),
        ProjectionKind::Edit { edit } => CompactedProjectionKind::Edit(edit.clone()),
        ProjectionKind::Offset {
            line_delta,
            column_delta,
        } => CompactedProjectionKind::Offset {
            line_delta: *line_delta,
            column_delta: *column_delta,
        },
    }
}

fn is_runtime_internal(uri: &SourceUri) -> bool {
    matches!(
        uri.as_url().scheme(),
        "node" | "chrome" | "devtools" | "v8" | "node-internal"
    )
}

impl ContextSourceModel {
    pub fn release(&self, contribution: &SourceContributionId) {
        let mut state = self.state.lock().unwrap();
        let Some(contribution) = state.contributions.remove(contribution) else {
            return;
        };

        for projection in contribution.projections {
            decrement_reference(&mut state.projection_references, projection);
            if !state.projection_references.contains_key(&projection) {
                state.graph.remove_projection(projection);
            }
        }
        for snapshot in contribution.snapshots {
            decrement_reference(&mut state.snapshot_references, snapshot);
            if !state.snapshot_references.contains_key(&snapshot)
                && state.graph.remove_source(snapshot).unwrap_or(false)
            {
                state.files.remove_snapshot(snapshot);
            }
        }
        let mut retained_content = retained_content_hashes(&state);
        drop(state);
        let mut maps = self.decoded_maps.lock().unwrap();
        maps.retain(|_, map| map.strong_count() > 0);
        retained_content.extend(maps.keys().copied());
        self.content
            .retain(|content| retained_content.contains(&content));
    }

    pub(crate) fn cached_source_map<E>(
        &self,
        content: ContentHash,
        create: impl FnOnce() -> Result<MapProjection, E>,
    ) -> Result<Arc<MapProjection>, E> {
        let mut maps = self.decoded_maps.lock().unwrap();
        if let Some(existing) = maps.get(&content).and_then(Weak::upgrade) {
            return Ok(existing);
        }
        let map = Arc::new(create()?);
        maps.insert(content, Arc::downgrade(&map));
        Ok(map)
    }

    pub fn content_stats(&self) -> ContentStoreStats {
        self.content.stats()
    }

    pub fn decoded_source_map_count(&self) -> usize {
        self.decoded_maps
            .lock()
            .unwrap()
            .values()
            .filter(|map| map.strong_count() > 0)
            .count()
    }
}

fn decrement_reference<T: Ord + Copy>(references: &mut BTreeMap<T, usize>, key: T) {
    let remove = if let Some(count) = references.get_mut(&key) {
        *count -= 1;
        *count == 0
    } else {
        false
    };
    if remove {
        references.remove(&key);
    }
}

fn retained_content_hashes(state: &ContextSourceState) -> BTreeSet<ContentHash> {
    let mut hashes = state
        .files
        .snapshots()
        .filter_map(SourceSnapshot::content_hash)
        .collect::<BTreeSet<_>>();
    for projection in state.graph.projections() {
        match projection.kind {
            ProjectionKind::Identity {
                basis: IdentityBasis::EqualContent(content),
            }
            | ProjectionKind::SourceMap { map: content, .. } => {
                hashes.insert(content);
            }
            ProjectionKind::Identity {
                basis: IdentityBasis::DeclaredByProvider(_),
            }
            | ProjectionKind::Format { .. }
            | ProjectionKind::Edit { .. }
            | ProjectionKind::Offset { .. } => {}
        }
    }
    hashes
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn shares_contributions_and_collects_after_last_observer() {
        let model = ContextSourceModel::new();
        let first = SourceContributionId::new("target-a/script-1");
        let second = SourceContributionId::new("target-b/script-1");
        let hash = model.content_store().intern("shared");
        let uri = SourceUri::parse("https://example.test/app.js").unwrap();

        let first_snapshot = model.intern_content(&first, uri.clone(), hash).unwrap();
        let second_snapshot = model.intern_content(&second, uri, hash).unwrap();
        assert_eq!(first_snapshot, second_snapshot);
        assert_eq!(model.graph_snapshot().sources.len(), 1);

        model.release(&first);
        assert_eq!(model.graph_snapshot().sources.len(), 1);
        assert_eq!(model.content_stats().unique_contents, 1);

        model.release(&second);
        assert!(model.graph_snapshot().sources.is_empty());
        assert_eq!(model.content_stats().unique_contents, 0);
    }

    #[test]
    fn deduplicates_projection_contributions() {
        let model = ContextSourceModel::new();
        let first = SourceContributionId::new("target-a");
        let second = SourceContributionId::new("target-b");
        let generated_hash = model.content_store().intern("generated");
        let source_hash = model.content_store().intern("source");
        let generated = model
            .intern_content(
                &first,
                SourceUri::parse("https://example.test/app.js").unwrap(),
                generated_hash,
            )
            .unwrap();
        let source = model
            .intern_content(
                &first,
                SourceUri::parse("file:///workspace/src/app.ts").unwrap(),
                source_hash,
            )
            .unwrap();
        model
            .intern_content(
                &second,
                SourceUri::parse("https://example.test/app.js").unwrap(),
                generated_hash,
            )
            .unwrap();
        model
            .intern_content(
                &second,
                SourceUri::parse("file:///workspace/src/app.ts").unwrap(),
                source_hash,
            )
            .unwrap();
        let map = model.content_store().intern("map");
        let kind = ProjectionKind::SourceMap {
            map,
            source_index: 0,
        };
        let first_projection = model
            .add_projection(&first, generated, source, kind.clone())
            .unwrap();
        let second_projection = model
            .add_projection(&second, generated, source, kind)
            .unwrap();
        assert_eq!(first_projection, second_projection);

        model.release(&first);
        assert_eq!(model.graph_snapshot().projections.len(), 1);
        model.release(&second);
        assert!(model.graph_snapshot().projections.is_empty());
    }

    #[test]
    fn rejects_conflicting_maps_for_the_same_source_revision() {
        let model = ContextSourceModel::new();
        let first = SourceContributionId::new("target-a");
        let second = SourceContributionId::new("target-b");
        let generated_hash = model.content_store().intern("generated");
        let generated_uri = SourceUri::parse("https://example.test/app.js").unwrap();
        let generated = model
            .intern_content(&first, generated_uri.clone(), generated_hash)
            .unwrap();
        model
            .intern_content(&second, generated_uri, generated_hash)
            .unwrap();
        let first_source = model
            .intern_content(
                &first,
                SourceUri::parse("file:///workspace/src/app.ts").unwrap(),
                model.content_store().intern("source"),
            )
            .unwrap();
        let second_source = model
            .intern_content(
                &second,
                SourceUri::parse("file:///workspace/other/app.ts").unwrap(),
                model.content_store().intern("other source"),
            )
            .unwrap();
        model
            .add_projection(
                &first,
                generated,
                first_source,
                ProjectionKind::SourceMap {
                    map: model.content_store().intern("first map"),
                    source_index: 0,
                },
            )
            .unwrap();
        let error = model
            .add_projection(
                &second,
                generated,
                second_source,
                ProjectionKind::SourceMap {
                    map: model.content_store().intern("second map"),
                    source_index: 0,
                },
            )
            .unwrap_err();
        assert!(matches!(
            error,
            SourceGraphError::ConflictingSourceMap {
                snapshot,
                ..
            } if snapshot == generated
        ));
    }

    #[test]
    fn compacts_relative_mappings_with_a_consistent_suffix_rewrite() {
        let model = ContextSourceModel::new();
        let owner = SourceContributionId::new("target");
        let map = model.content_store().intern("map");
        for name in ["foo", "bar"] {
            let generated = model
                .intern_content(
                    &owner,
                    SourceUri::parse(&format!("file:///workspace/out/{name}.js")).unwrap(),
                    model.content_store().intern(&format!("generated {name}")),
                )
                .unwrap();
            let source = model
                .intern_content(
                    &owner,
                    SourceUri::parse(&format!("file:///workspace/src/{name}.ts")).unwrap(),
                    model.content_store().intern(&format!("source {name}")),
                )
                .unwrap();
            model
                .add_projection(
                    &owner,
                    generated,
                    source,
                    ProjectionKind::SourceMap {
                        map,
                        source_index: 0,
                    },
                )
                .unwrap();
        }

        let compacted = model.compacted_graph();
        assert_eq!(compacted.nodes.len(), 2);
        assert_eq!(compacted.edges.len(), 1);
        assert_eq!(compacted.edges[0].mapping_count, 2);
        assert_eq!(
            compacted.edges[0].suffix_rewrite,
            Some(SuffixRewrite {
                from: ".js".into(),
                to: ".ts".into(),
            })
        );
        assert!(compacted.nodes.iter().any(|node| {
            node.prefix.as_str() == "file:///workspace/out/" && node.source_count == 2
        }));
        assert!(compacted.nodes.iter().any(|node| {
            node.prefix.as_str() == "file:///workspace/src/" && node.source_count == 2
        }));
    }

    #[test]
    fn compacts_one_bundle_into_an_exclusive_source_subtree() {
        let model = ContextSourceModel::new();
        let owner = SourceContributionId::new("target");
        let map = model.content_store().intern("map");
        let generated = model
            .intern_content(
                &owner,
                SourceUri::parse("https://example.test/bundle.js").unwrap(),
                model.content_store().intern("bundle"),
            )
            .unwrap();
        for (index, name) in ["foo", "bar"].into_iter().enumerate() {
            let source = model
                .intern_content(
                    &owner,
                    SourceUri::embedded("resolved", &format!("../../../src/{name}.ts")).unwrap(),
                    model.content_store().intern(&format!("source {name}")),
                )
                .unwrap();
            model
                .add_projection(
                    &owner,
                    generated,
                    source,
                    ProjectionKind::SourceMap {
                        map,
                        source_index: index as u32,
                    },
                )
                .unwrap();
        }

        let compacted = model.compacted_graph();
        assert_eq!(compacted.nodes.len(), 2);
        assert_eq!(compacted.edges.len(), 1);
        assert_eq!(compacted.edges[0].mapping_count, 2);
        assert!(compacted.edges[0].fan_out);
        assert_eq!(
            compacted
                .nodes
                .iter()
                .find(|node| node.id == compacted.edges[0].basis)
                .unwrap()
                .prefix
                .as_str(),
            "source://resolved/~up/~up/~up/src/"
        );
    }

    #[test]
    fn fan_out_compaction_stops_at_a_competing_bundle() {
        let model = ContextSourceModel::new();
        let owner = SourceContributionId::new("target");
        let map = model.content_store().intern("map");
        let first_bundle = model
            .intern_content(
                &owner,
                SourceUri::parse("https://example.test/bundle.js").unwrap(),
                model.content_store().intern("bundle"),
            )
            .unwrap();
        let second_bundle = model
            .intern_content(
                &owner,
                SourceUri::parse("https://example.test/bundle2.js").unwrap(),
                model.content_store().intern("bundle 2"),
            )
            .unwrap();
        for (index, (bundle, name)) in [
            (first_bundle, "foo"),
            (first_bundle, "bar"),
            (second_bundle, "baz"),
        ]
        .into_iter()
        .enumerate()
        {
            let source = model
                .intern_content(
                    &owner,
                    SourceUri::embedded("resolved", &format!("../../../src/{name}.ts")).unwrap(),
                    model.content_store().intern(&format!("source {name}")),
                )
                .unwrap();
            model
                .add_projection(
                    &owner,
                    bundle,
                    source,
                    ProjectionKind::SourceMap {
                        map,
                        source_index: index as u32,
                    },
                )
                .unwrap();
        }

        let compacted = model.compacted_graph();
        assert_eq!(compacted.edges.len(), 3);
        assert!(compacted.edges.iter().all(|edge| !edge.fan_out));
    }

    #[test]
    fn compaction_stops_at_known_relative_path_counterexamples() {
        let model = ContextSourceModel::new();
        let owner = SourceContributionId::new("target");
        let map = model.content_store().intern("map");
        for (generated_name, source_name) in [("foo", "foo"), ("bar", "renamed")] {
            let generated = model
                .intern_content(
                    &owner,
                    SourceUri::parse(&format!("file:///workspace/out/{generated_name}.js"))
                        .unwrap(),
                    model
                        .content_store()
                        .intern(&format!("generated {generated_name}")),
                )
                .unwrap();
            let source = model
                .intern_content(
                    &owner,
                    SourceUri::parse(&format!("file:///workspace/src/{source_name}.ts")).unwrap(),
                    model
                        .content_store()
                        .intern(&format!("source {source_name}")),
                )
                .unwrap();
            model
                .add_projection(
                    &owner,
                    generated,
                    source,
                    ProjectionKind::SourceMap {
                        map,
                        source_index: 0,
                    },
                )
                .unwrap();
        }

        let compacted = model.compacted_graph();
        assert_eq!(compacted.edges.len(), 2);
        assert!(compacted.edges.iter().all(|edge| edge.mapping_count == 1));
    }

    #[test]
    fn marks_only_runtime_scheme_sources_as_internal() {
        let model = ContextSourceModel::new();
        let owner = SourceContributionId::new("target");
        let node = model
            .intern_content(
                &owner,
                SourceUri::parse("node:internal/modules/cjs/loader").unwrap(),
                model.content_store().intern("internal"),
            )
            .unwrap();
        let app = model
            .intern_content(
                &owner,
                SourceUri::parse("file:///workspace/node_modules/pkg/index.js").unwrap(),
                model.content_store().intern("dependency"),
            )
            .unwrap();
        model
            .add_projection(
                &owner,
                app,
                node,
                ProjectionKind::Identity {
                    basis: IdentityBasis::DeclaredByProvider("test".into()),
                },
            )
            .unwrap();

        let compacted = model.compacted_graph();
        assert!(compacted.nodes.iter().any(|node| node.runtime_internal));
        assert!(compacted.nodes.iter().any(|node| {
            node.prefix.as_str().contains("node_modules") && !node.runtime_internal
        }));
    }
}
