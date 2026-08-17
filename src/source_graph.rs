use std::collections::{BTreeMap, BTreeSet, VecDeque};
use std::sync::Arc;

use serde::{Deserialize, Serialize};

use crate::content_store::{ContentId, ContentStore};

/// A provider-qualified address such as a file URL or a CDP-script URL.
///
/// Qualification is part of the string so the graph does not need a separate
/// namespace object merely to prevent providers from colliding.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub struct SourcePath(String);

impl SourcePath {
    pub fn new(value: impl Into<String>) -> Result<Self, SourcePathError> {
        let value = value.into();
        if value.is_empty() {
            return Err(SourcePathError::Empty);
        }
        Ok(Self(value))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
pub enum SourcePathError {
    #[error("source path must not be empty")]
    Empty,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub struct SourceSnapshotId(pub u64);

#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub enum SourceRevision {
    Content(ContentId),
    Opaque(String),
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct SourceSnapshot {
    pub id: SourceSnapshotId,
    pub path: SourcePath,
    pub revision: SourceRevision,
}

impl SourceSnapshot {
    pub fn content_id(&self) -> Option<ContentId> {
        match &self.revision {
            SourceRevision::Content(content) => Some(*content),
            SourceRevision::Opaque(_) => None,
        }
    }
}

/// Stores immutable source snapshots while allowing providers to move a path's head.
///
/// Mutable providers publish new snapshots and move heads. Immutable providers
/// only intern snapshots. In both cases, content is retained in the same CAS.
pub struct SourceFileStore {
    content: Arc<ContentStore>,
    next_snapshot_id: u64,
    snapshots: BTreeMap<SourceSnapshotId, SourceSnapshot>,
    by_identity: BTreeMap<(SourcePath, SourceRevision), SourceSnapshotId>,
    heads: BTreeMap<SourcePath, SourceSnapshotId>,
}

impl SourceFileStore {
    pub fn new(content: Arc<ContentStore>) -> Self {
        Self {
            content,
            next_snapshot_id: 1,
            snapshots: BTreeMap::new(),
            by_identity: BTreeMap::new(),
            heads: BTreeMap::new(),
        }
    }

    pub fn intern_text(&mut self, path: SourcePath, text: &str) -> SourceSnapshotId {
        let content = self.content.intern(text);
        self.intern_revision(path, SourceRevision::Content(content))
    }

    pub fn intern_content(
        &mut self,
        path: SourcePath,
        content: ContentId,
    ) -> Result<SourceSnapshotId, SourceFileStoreError> {
        if !self.content.contains(content) {
            return Err(SourceFileStoreError::UnknownContent(content));
        }
        Ok(self.intern_revision(path, SourceRevision::Content(content)))
    }

    pub fn write_text(&mut self, path: SourcePath, text: &str) -> SourceSnapshotId {
        let snapshot = self.intern_text(path.clone(), text);
        self.heads.insert(path, snapshot);
        snapshot
    }

    pub fn intern_opaque(
        &mut self,
        path: SourcePath,
        revision: impl Into<String>,
    ) -> Result<SourceSnapshotId, SourceFileStoreError> {
        let revision = revision.into();
        if revision.is_empty() {
            return Err(SourceFileStoreError::EmptyOpaqueRevision);
        }
        Ok(self.intern_revision(path, SourceRevision::Opaque(revision)))
    }

    pub fn write_opaque(
        &mut self,
        path: SourcePath,
        revision: impl Into<String>,
    ) -> Result<SourceSnapshotId, SourceFileStoreError> {
        let snapshot = self.intern_opaque(path.clone(), revision)?;
        self.heads.insert(path, snapshot);
        Ok(snapshot)
    }

    pub fn set_head(
        &mut self,
        path: &SourcePath,
        snapshot: SourceSnapshotId,
    ) -> Result<(), SourceFileStoreError> {
        let candidate = self
            .snapshots
            .get(&snapshot)
            .ok_or(SourceFileStoreError::UnknownSnapshot(snapshot))?;
        if &candidate.path != path {
            return Err(SourceFileStoreError::PathMismatch {
                snapshot,
                expected: path.clone(),
                actual: candidate.path.clone(),
            });
        }
        self.heads.insert(path.clone(), snapshot);
        Ok(())
    }

    pub fn head(&self, path: &SourcePath) -> Option<SourceSnapshotId> {
        self.heads.get(path).copied()
    }

    pub fn snapshot(&self, id: SourceSnapshotId) -> Option<&SourceSnapshot> {
        self.snapshots.get(&id)
    }

    pub fn content(&self, id: SourceSnapshotId) -> Option<Arc<str>> {
        self.snapshot(id)
            .and_then(SourceSnapshot::content_id)
            .and_then(|content| self.content.get(content))
    }

    pub fn content_store(&self) -> &Arc<ContentStore> {
        &self.content
    }

    fn intern_revision(&mut self, path: SourcePath, revision: SourceRevision) -> SourceSnapshotId {
        let identity = (path.clone(), revision.clone());
        if let Some(existing) = self.by_identity.get(&identity).copied() {
            return existing;
        }

        let id = SourceSnapshotId(self.next_snapshot_id);
        self.next_snapshot_id += 1;
        self.snapshots.insert(
            id,
            SourceSnapshot {
                id,
                path: path.clone(),
                revision,
            },
        );
        self.by_identity.insert(identity, id);
        id
    }
}

#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
pub enum SourceFileStoreError {
    #[error("content {0:?} does not exist in the content store")]
    UnknownContent(ContentId),
    #[error("opaque source revision must not be empty")]
    EmptyOpaqueRevision,
    #[error("source snapshot {0:?} does not exist")]
    UnknownSnapshot(SourceSnapshotId),
    #[error("source snapshot {snapshot:?} belongs to {actual:?}, not expected path {expected:?}")]
    PathMismatch {
        snapshot: SourceSnapshotId,
        expected: SourcePath,
        actual: SourcePath,
    },
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub struct ProjectionId(pub u64);

#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub enum IdentityBasis {
    EqualContent(ContentId),
    DeclaredByProvider(String),
}

#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub enum ProjectionKind {
    Identity { basis: IdentityBasis },
    SourceMap { map: ContentId, source_index: u32 },
    Format { formatter: String },
    Edit { edit: String },
    Offset { line_delta: i64, column_delta: i64 },
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct SourceProjection {
    pub id: ProjectionId,
    /// The source version produced from or observed through `basis`.
    pub derived: SourceSnapshotId,
    /// The source version on which `derived` depends.
    ///
    /// This orientation forms the dependency DAG; location traversal can still
    /// use the projection in either direction.
    pub basis: SourceSnapshotId,
    pub kind: ProjectionKind,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub enum ProjectionDirection {
    DerivedToBasis,
    BasisToDerived,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProjectionHop {
    pub projection: ProjectionId,
    pub direction: ProjectionDirection,
    pub from: SourceSnapshotId,
    pub to: SourceSnapshotId,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProjectionRoute {
    pub start: SourceSnapshotId,
    pub end: SourceSnapshotId,
    pub hops: Vec<ProjectionHop>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct RouteLimits {
    pub max_depth: usize,
    pub max_routes: usize,
    pub max_expansions: usize,
}

impl Default for RouteLimits {
    fn default() -> Self {
        Self {
            max_depth: 16,
            max_routes: 256,
            max_expansions: 4096,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RouteSearchStatus {
    /// Complete relative to the topology currently contributed to the graph.
    /// Providers may still contribute additional sources or projections later.
    Complete,
    Truncated,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RouteSearch {
    pub status: RouteSearchStatus,
    pub routes: Vec<ProjectionRoute>,
}

pub struct SourceGraph {
    next_projection_id: u64,
    sources: BTreeSet<SourceSnapshotId>,
    projections: BTreeMap<ProjectionId, SourceProjection>,
    projection_index: BTreeMap<(SourceSnapshotId, SourceSnapshotId, ProjectionKind), ProjectionId>,
    dependencies: BTreeMap<SourceSnapshotId, BTreeSet<ProjectionId>>,
    dependents: BTreeMap<SourceSnapshotId, BTreeSet<ProjectionId>>,
}

impl Default for SourceGraph {
    fn default() -> Self {
        Self::new()
    }
}

impl SourceGraph {
    pub fn new() -> Self {
        Self {
            next_projection_id: 1,
            sources: BTreeSet::new(),
            projections: BTreeMap::new(),
            projection_index: BTreeMap::new(),
            dependencies: BTreeMap::new(),
            dependents: BTreeMap::new(),
        }
    }

    pub fn add_source(&mut self, source: SourceSnapshotId) -> bool {
        self.sources.insert(source)
    }

    pub fn add_projection(
        &mut self,
        derived: SourceSnapshotId,
        basis: SourceSnapshotId,
        kind: ProjectionKind,
    ) -> Result<ProjectionId, SourceGraphError> {
        self.require_source(derived)?;
        self.require_source(basis)?;
        if derived == basis {
            return Err(SourceGraphError::SelfProjection(derived));
        }

        let identity = (derived, basis, kind.clone());
        if let Some(existing) = self.projection_index.get(&identity) {
            return Ok(*existing);
        }
        if self.depends_on(basis, derived) {
            return Err(SourceGraphError::Cycle { derived, basis });
        }

        let id = ProjectionId(self.next_projection_id);
        self.next_projection_id += 1;
        self.projections.insert(
            id,
            SourceProjection {
                id,
                derived,
                basis,
                kind,
            },
        );
        self.projection_index.insert(identity, id);
        self.dependencies.entry(derived).or_default().insert(id);
        self.dependents.entry(basis).or_default().insert(id);
        Ok(id)
    }

    pub fn source_count(&self) -> usize {
        self.sources.len()
    }

    pub fn projection_count(&self) -> usize {
        self.projections.len()
    }

    pub fn projection(&self, id: ProjectionId) -> Option<&SourceProjection> {
        self.projections.get(&id)
    }

    pub fn find_routes(
        &self,
        start: SourceSnapshotId,
        targets: &BTreeSet<SourceSnapshotId>,
        limits: RouteLimits,
    ) -> Result<RouteSearch, SourceGraphError> {
        self.require_source(start)?;
        for target in targets {
            self.require_source(*target)?;
        }
        if limits.max_routes == 0 {
            return Err(SourceGraphError::ZeroRouteLimit);
        }
        if limits.max_expansions == 0 {
            return Err(SourceGraphError::ZeroExpansionLimit);
        }

        let mut routes = Vec::new();
        let mut status = RouteSearchStatus::Complete;
        let mut expansions = 0;
        let mut queue = VecDeque::from([RouteCandidate {
            current: start,
            hops: Vec::new(),
            visited: BTreeSet::from([start]),
        }]);

        while let Some(candidate) = queue.pop_front() {
            if targets.contains(&candidate.current) {
                routes.push(ProjectionRoute {
                    start,
                    end: candidate.current,
                    hops: candidate.hops.clone(),
                });
                if routes.len() == limits.max_routes {
                    let has_unvisited_neighbors = candidate.hops.len() < limits.max_depth
                        && self
                            .neighbors(candidate.current)
                            .iter()
                            .any(|neighbor| !candidate.visited.contains(&neighbor.to));
                    if !queue.is_empty() || has_unvisited_neighbors {
                        status = RouteSearchStatus::Truncated;
                    }
                    break;
                }
            }

            let neighbors = self.neighbors(candidate.current);
            if candidate.hops.len() == limits.max_depth {
                if neighbors
                    .iter()
                    .any(|neighbor| !candidate.visited.contains(&neighbor.to))
                {
                    status = RouteSearchStatus::Truncated;
                }
                continue;
            }

            let neighbors = neighbors
                .into_iter()
                .filter(|hop| !candidate.visited.contains(&hop.to))
                .collect::<Vec<_>>();
            if neighbors.is_empty() {
                continue;
            }
            if expansions == limits.max_expansions {
                status = RouteSearchStatus::Truncated;
                continue;
            }
            expansions += 1;

            for hop in neighbors {
                let mut hops = candidate.hops.clone();
                hops.push(hop.clone());
                let mut visited = candidate.visited.clone();
                visited.insert(hop.to);
                queue.push_back(RouteCandidate {
                    current: hop.to,
                    hops,
                    visited,
                });
            }
        }

        Ok(RouteSearch { status, routes })
    }

    fn require_source(&self, source: SourceSnapshotId) -> Result<(), SourceGraphError> {
        if self.sources.contains(&source) {
            Ok(())
        } else {
            Err(SourceGraphError::UnknownSource(source))
        }
    }

    fn depends_on(&self, source: SourceSnapshotId, candidate: SourceSnapshotId) -> bool {
        let mut pending = vec![source];
        let mut visited = BTreeSet::new();
        while let Some(current) = pending.pop() {
            if current == candidate {
                return true;
            }
            if !visited.insert(current) {
                continue;
            }
            if let Some(projections) = self.dependencies.get(&current) {
                pending.extend(
                    projections
                        .iter()
                        .filter_map(|id| self.projections.get(id))
                        .map(|projection| projection.basis),
                );
            }
        }
        false
    }

    fn neighbors(&self, source: SourceSnapshotId) -> Vec<ProjectionHop> {
        let mut result = Vec::new();
        if let Some(projections) = self.dependencies.get(&source) {
            result.extend(projections.iter().filter_map(|id| {
                self.projections.get(id).map(|projection| ProjectionHop {
                    projection: projection.id,
                    direction: ProjectionDirection::DerivedToBasis,
                    from: source,
                    to: projection.basis,
                })
            }));
        }
        if let Some(projections) = self.dependents.get(&source) {
            result.extend(projections.iter().filter_map(|id| {
                self.projections.get(id).map(|projection| ProjectionHop {
                    projection: projection.id,
                    direction: ProjectionDirection::BasisToDerived,
                    from: source,
                    to: projection.derived,
                })
            }));
        }
        result.sort_by_key(|hop| (hop.to, hop.projection, hop.direction));
        result
    }
}

struct RouteCandidate {
    current: SourceSnapshotId,
    hops: Vec<ProjectionHop>,
    visited: BTreeSet<SourceSnapshotId>,
}

#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
pub enum SourceGraphError {
    #[error("source snapshot {0:?} is not registered in the graph")]
    UnknownSource(SourceSnapshotId),
    #[error("source snapshot {0:?} cannot project to itself")]
    SelfProjection(SourceSnapshotId),
    #[error("projection from {derived:?} to {basis:?} would create a dependency cycle")]
    Cycle {
        derived: SourceSnapshotId,
        basis: SourceSnapshotId,
    },
    #[error("route search max_routes must be greater than zero")]
    ZeroRouteLimit,
    #[error("route search max_expansions must be greater than zero")]
    ZeroExpansionLimit,
}

#[cfg(test)]
mod tests {
    use super::*;

    fn path(value: &str) -> SourcePath {
        SourcePath::new(value).unwrap()
    }

    #[test]
    fn mutable_heads_preserve_immutable_cas_snapshots() {
        let content = Arc::new(ContentStore::default());
        let mut files = SourceFileStore::new(content);
        let workspace_path = path("file:///workspace/src/app.ts");
        let mirror_path = path("cas-source:///app.ts");

        let first = files.write_text(workspace_path.clone(), "export const value = 1;");
        let mirror = files.intern_text(mirror_path, "export const value = 1;");
        let second = files.write_text(workspace_path.clone(), "export const value = 2;");

        assert_ne!(first, second);
        assert_eq!(
            files.snapshot(first).unwrap().content_id(),
            files.snapshot(mirror).unwrap().content_id()
        );
        assert_eq!(files.head(&workspace_path), Some(second));
        assert_eq!(&*files.content(first).unwrap(), "export const value = 1;");
        assert_eq!(&*files.content(second).unwrap(), "export const value = 2;");

        assert_eq!(
            files.intern_text(workspace_path.clone(), "export const value = 1;"),
            first
        );
        assert_eq!(files.head(&workspace_path), Some(second));

        files.set_head(&workspace_path, first).unwrap();
        assert_eq!(files.head(&workspace_path), Some(first));
    }

    #[test]
    fn existing_cas_content_can_be_registered_without_reinterning_text() {
        let content = Arc::new(ContentStore::default());
        let content_id = content.intern("shared");
        let mut files = SourceFileStore::new(content.clone());

        let snapshot = files
            .intern_content(path("resolved-source:app.js"), content_id)
            .unwrap();

        assert_eq!(
            files.snapshot(snapshot).unwrap().content_id(),
            Some(content_id)
        );
        assert_eq!(content.stats().intern_requests, 1);
    }

    #[test]
    fn routes_offline_sources_to_runtime_scripts_through_dist() {
        let content = Arc::new(ContentStore::default());
        let mut files = SourceFileStore::new(content);
        let authored = files.intern_text(
            path("file:///workspace/src/app.ts"),
            "export const value: number = 1;",
        );
        let dist = files.intern_text(
            path("file:///workspace/dist/app.js"),
            "export const value = 1;",
        );
        let runtime = files.intern_text(
            path("cdp://generation-1/session-7/script-42"),
            "export const value = 1;",
        );
        let source_map = files.content_store().intern(r#"{"version":3}"#);
        let dist_content = files.snapshot(dist).unwrap().content_id().unwrap();

        let mut graph = SourceGraph::new();
        for source in [authored, dist, runtime] {
            graph.add_source(source);
        }
        let map_projection = graph
            .add_projection(
                dist,
                authored,
                ProjectionKind::SourceMap {
                    map: source_map,
                    source_index: 0,
                },
            )
            .unwrap();
        let runtime_link = graph
            .add_projection(
                runtime,
                dist,
                ProjectionKind::Identity {
                    basis: IdentityBasis::EqualContent(dist_content),
                },
            )
            .unwrap();

        let search = graph
            .find_routes(authored, &BTreeSet::from([runtime]), RouteLimits::default())
            .unwrap();

        assert_eq!(search.status, RouteSearchStatus::Complete);
        assert_eq!(search.routes.len(), 1);
        assert_eq!(
            search.routes[0].hops,
            vec![
                ProjectionHop {
                    projection: map_projection,
                    direction: ProjectionDirection::BasisToDerived,
                    from: authored,
                    to: dist,
                },
                ProjectionHop {
                    projection: runtime_link,
                    direction: ProjectionDirection::BasisToDerived,
                    from: dist,
                    to: runtime,
                },
            ]
        );
    }

    #[test]
    fn one_source_can_reach_multiple_runtime_endpoints() {
        let content = Arc::new(ContentStore::default());
        let mut files = SourceFileStore::new(content);
        let authored = files.intern_text(path("file:///src/app.ts"), "source");
        let dist = files.intern_text(path("file:///dist/app.js"), "generated");
        let page = files.intern_text(path("cdp://page/script-1"), "generated");
        let worker = files.intern_text(path("cdp://worker/script-1"), "generated");
        let map = files.content_store().intern("map");
        let dist_content = files.snapshot(dist).unwrap().content_id().unwrap();

        let mut graph = SourceGraph::new();
        for source in [authored, dist, page, worker] {
            graph.add_source(source);
        }
        graph
            .add_projection(
                dist,
                authored,
                ProjectionKind::SourceMap {
                    map,
                    source_index: 0,
                },
            )
            .unwrap();
        for runtime in [page, worker] {
            graph
                .add_projection(
                    runtime,
                    dist,
                    ProjectionKind::Identity {
                        basis: IdentityBasis::EqualContent(dist_content),
                    },
                )
                .unwrap();
        }

        let search = graph
            .find_routes(
                authored,
                &BTreeSet::from([page, worker]),
                RouteLimits::default(),
            )
            .unwrap();

        assert_eq!(
            search
                .routes
                .iter()
                .map(|route| route.end)
                .collect::<Vec<_>>(),
            vec![page, worker]
        );
    }

    #[test]
    fn route_search_continues_through_intermediate_targets() {
        let mut graph = SourceGraph::new();
        let authored = SourceSnapshotId(1);
        let dist = SourceSnapshotId(2);
        let runtime = SourceSnapshotId(3);
        for source in [authored, dist, runtime] {
            graph.add_source(source);
        }
        let declared = || ProjectionKind::Identity {
            basis: IdentityBasis::DeclaredByProvider("test".into()),
        };
        graph.add_projection(dist, authored, declared()).unwrap();
        graph.add_projection(runtime, dist, declared()).unwrap();

        let search = graph
            .find_routes(
                authored,
                &BTreeSet::from([authored, dist, runtime]),
                RouteLimits::default(),
            )
            .unwrap();

        assert_eq!(search.status, RouteSearchStatus::Complete);
        assert_eq!(
            search
                .routes
                .iter()
                .map(|route| route.end)
                .collect::<Vec<_>>(),
            vec![authored, dist, runtime]
        );
    }

    #[test]
    fn projection_dependencies_must_remain_a_dag() {
        let mut graph = SourceGraph::new();
        let authored = SourceSnapshotId(1);
        let dist = SourceSnapshotId(2);
        let runtime = SourceSnapshotId(3);
        for source in [authored, dist, runtime] {
            graph.add_source(source);
        }
        let declared = || ProjectionKind::Identity {
            basis: IdentityBasis::DeclaredByProvider("test".into()),
        };

        graph.add_projection(dist, authored, declared()).unwrap();
        graph.add_projection(runtime, dist, declared()).unwrap();
        let error = graph
            .add_projection(authored, runtime, declared())
            .unwrap_err();

        assert_eq!(
            error,
            SourceGraphError::Cycle {
                derived: authored,
                basis: runtime,
            }
        );
    }

    #[test]
    fn duplicate_projection_contributions_are_idempotent() {
        let mut graph = SourceGraph::new();
        let derived = SourceSnapshotId(1);
        let basis = SourceSnapshotId(2);
        graph.add_source(derived);
        graph.add_source(basis);
        let kind = ProjectionKind::Offset {
            line_delta: 1,
            column_delta: 0,
        };

        let first = graph.add_projection(derived, basis, kind.clone()).unwrap();
        let second = graph.add_projection(derived, basis, kind).unwrap();

        assert_eq!(first, second);
        assert_eq!(graph.projection_count(), 1);
    }

    #[test]
    fn bounded_search_reports_an_incomplete_topology_walk() {
        let mut graph = SourceGraph::new();
        let authored = SourceSnapshotId(1);
        let dist = SourceSnapshotId(2);
        let runtime = SourceSnapshotId(3);
        for source in [authored, dist, runtime] {
            graph.add_source(source);
        }
        let declared = || ProjectionKind::Identity {
            basis: IdentityBasis::DeclaredByProvider("test".into()),
        };
        graph.add_projection(dist, authored, declared()).unwrap();
        graph.add_projection(runtime, dist, declared()).unwrap();

        let search = graph
            .find_routes(
                authored,
                &BTreeSet::from([runtime]),
                RouteLimits {
                    max_depth: 1,
                    max_routes: 4,
                    max_expansions: 4,
                },
            )
            .unwrap();

        assert_eq!(search.status, RouteSearchStatus::Truncated);
        assert!(search.routes.is_empty());
    }

    #[test]
    fn bounded_search_limits_topology_expansion_without_targets() {
        let mut graph = SourceGraph::new();
        let sources = (1..=6).map(SourceSnapshotId).collect::<Vec<_>>();
        for source in &sources {
            graph.add_source(*source);
        }
        let declared = || ProjectionKind::Identity {
            basis: IdentityBasis::DeclaredByProvider("test".into()),
        };
        for pair in sources.windows(2) {
            graph.add_projection(pair[1], pair[0], declared()).unwrap();
        }

        let search = graph
            .find_routes(
                sources[0],
                &BTreeSet::new(),
                RouteLimits {
                    max_depth: 16,
                    max_routes: 4,
                    max_expansions: 2,
                },
            )
            .unwrap();

        assert_eq!(search.status, RouteSearchStatus::Truncated);
        assert!(search.routes.is_empty());
    }
}
