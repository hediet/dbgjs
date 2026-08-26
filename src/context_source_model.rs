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
        let retained_content = retained_content_hashes(&state);
        drop(state);
        self.content
            .retain(|content| retained_content.contains(&content));
        self.decoded_maps
            .lock()
            .unwrap()
            .retain(|content, map| retained_content.contains(content) && map.strong_count() > 0);
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
}
