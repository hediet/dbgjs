//! Scopes: a declarative, serializable question about the resource graph.
//!
//! A scope names where to start ([`Scope::roots`]), which relations may be crossed
//! ([`RelationFilter`] plus [`TraversalDirection`]), how far to go ([`Scope::max_depth`]) and which
//! of the visited resources are actually wanted ([`ResourcePredicate`]). Traversal and selection
//! are deliberately separate: a scope may walk *through* a resource it does not select, which is
//! what makes "all frames under this process" expressible without special casing the process.
//!
//! Every resolution reports its own completeness. If the walk crossed a resource whose discovery
//! frontier is not [`DiscoveryState::is_complete`], or stopped at the depth limit, the result
//! carries the corresponding [`ScopeGap`] instead of pretending the answer is exhaustive.

use std::collections::{BTreeMap, BTreeSet, VecDeque};

use serde::{Deserialize, Serialize};

use crate::capability::CapabilityKind;
use crate::discovery::DiscoveryState;
use crate::resource_graph::{EdgeDirection, RelationKind, ResourceGraph, ResourceId, ResourceKind};

/// Which relations a traversal may cross.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum RelationFilter {
    #[default]
    Any,
    Only(BTreeSet<RelationKind>),
    Excluding(BTreeSet<RelationKind>),
}

impl RelationFilter {
    pub fn only<I: IntoIterator<Item = RelationKind>>(kinds: I) -> Self {
        Self::Only(kinds.into_iter().collect())
    }

    pub fn excluding<I: IntoIterator<Item = RelationKind>>(kinds: I) -> Self {
        Self::Excluding(kinds.into_iter().collect())
    }

    pub fn allows(&self, kind: &RelationKind) -> bool {
        match self {
            Self::Any => true,
            Self::Only(kinds) => kinds.contains(kind),
            Self::Excluding(kinds) => !kinds.contains(kind),
        }
    }
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum TraversalDirection {
    #[default]
    Outgoing,
    Incoming,
    Both,
}

impl TraversalDirection {
    fn edge_directions(self) -> &'static [EdgeDirection] {
        match self {
            Self::Outgoing => &[EdgeDirection::Outgoing],
            Self::Incoming => &[EdgeDirection::Incoming],
            Self::Both => &[EdgeDirection::Outgoing, EdgeDirection::Incoming],
        }
    }
}

/// Which visited resources the scope selects.
///
/// [`ResourcePredicate::Capability`] is the interesting one: it asks what a resource *can do*,
/// which is a fact about registered capability objects, while [`ResourcePredicate::Kind`] only
/// filters on a descriptive label.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum ResourcePredicate {
    #[default]
    Any,
    Kind(ResourceKind),
    Capability(CapabilityKind),
    All(Vec<ResourcePredicate>),
    AnyOf(Vec<ResourcePredicate>),
    Not(Box<ResourcePredicate>),
}

impl ResourcePredicate {
    pub fn kind(kind: ResourceKind) -> Self {
        Self::Kind(kind)
    }

    pub fn capability(kind: CapabilityKind) -> Self {
        Self::Capability(kind)
    }

    pub fn not(predicate: ResourcePredicate) -> Self {
        Self::Not(Box::new(predicate))
    }

    pub fn matches(&self, graph: &ResourceGraph, id: &ResourceId) -> bool {
        match self {
            Self::Any => true,
            Self::Kind(kind) => graph.has_kind(id, kind),
            Self::Capability(kind) => graph.has_capability(id, kind),
            Self::All(predicates) => predicates
                .iter()
                .all(|predicate| predicate.matches(graph, id)),
            Self::AnyOf(predicates) => predicates
                .iter()
                .any(|predicate| predicate.matches(graph, id)),
            Self::Not(predicate) => !predicate.matches(graph, id),
        }
    }
}

/// A serializable question about the graph.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Scope {
    pub roots: BTreeSet<ResourceId>,
    #[serde(default)]
    pub relations: RelationFilter,
    #[serde(default)]
    pub direction: TraversalDirection,
    #[serde(default)]
    pub predicate: ResourcePredicate,
    /// Maximum number of relation hops from a root. `Some(0)` selects the roots only.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_depth: Option<u32>,
}

impl Scope {
    pub fn from_root(root: ResourceId) -> Self {
        Self {
            roots: BTreeSet::from([root]),
            ..Self::default()
        }
    }

    pub fn from_roots<I: IntoIterator<Item = ResourceId>>(roots: I) -> Self {
        Self {
            roots: roots.into_iter().collect(),
            ..Self::default()
        }
    }

    pub fn with_relations(mut self, relations: RelationFilter) -> Self {
        self.relations = relations;
        self
    }

    pub fn with_direction(mut self, direction: TraversalDirection) -> Self {
        self.direction = direction;
        self
    }

    pub fn with_predicate(mut self, predicate: ResourcePredicate) -> Self {
        self.predicate = predicate;
        self
    }

    pub fn with_max_depth(mut self, max_depth: u32) -> Self {
        self.max_depth = Some(max_depth);
        self
    }

    /// Walks the graph breadth first in deterministic order and reports both the selection and
    /// everything that made the answer non-exhaustive.
    pub fn resolve(&self, graph: &ResourceGraph) -> ScopeResolution {
        let mut depths: BTreeMap<ResourceId, u32> = BTreeMap::new();
        let mut visited: Vec<VisitedResource> = Vec::new();
        let mut gaps: Vec<ScopeGap> = Vec::new();
        let mut queue: VecDeque<ResourceId> = VecDeque::new();

        for root in &self.roots {
            if !graph.contains(root) {
                gaps.push(ScopeGap::MissingRoot {
                    resource: root.clone(),
                });
                continue;
            }
            if depths.insert(root.clone(), 0).is_none() {
                visited.push(VisitedResource {
                    resource: root.clone(),
                    depth: 0,
                });
                queue.push_back(root.clone());
            }
        }

        while let Some(id) = queue.pop_front() {
            let depth = depths[&id];
            let neighbors = self.neighbors(graph, &id);
            let frontier_gaps = self.frontier_gaps(graph, &id);
            if self.max_depth.is_some_and(|max| depth >= max) {
                if !neighbors.is_empty() || !frontier_gaps.is_empty() {
                    gaps.push(ScopeGap::DepthLimit {
                        resource: id.clone(),
                    });
                }
                continue;
            }
            gaps.extend(frontier_gaps);
            for neighbor in neighbors {
                if depths.contains_key(&neighbor) {
                    continue;
                }
                depths.insert(neighbor.clone(), depth + 1);
                visited.push(VisitedResource {
                    resource: neighbor.clone(),
                    depth: depth + 1,
                });
                queue.push_back(neighbor);
            }
        }

        let matched = visited
            .iter()
            .filter(|entry| self.predicate.matches(graph, &entry.resource))
            .map(|entry| entry.resource.clone())
            .collect();

        ScopeResolution {
            revision: graph.revision().0,
            matched,
            visited,
            gaps,
        }
    }

    fn neighbors(&self, graph: &ResourceGraph, id: &ResourceId) -> Vec<ResourceId> {
        let mut neighbors = Vec::new();
        for direction in self.direction.edge_directions() {
            for (kind, other) in graph.neighbors(id, *direction) {
                if self.relations.allows(&kind) {
                    neighbors.push(other);
                }
            }
        }
        neighbors.sort();
        neighbors.dedup();
        neighbors
    }

    /// A resource contributes a gap when the relations this scope wants to cross are not fully
    /// discovered - including the case where nobody ever declared a frontier at all.
    fn frontier_gaps(&self, graph: &ResourceGraph, id: &ResourceId) -> Vec<ScopeGap> {
        match &self.relations {
            RelationFilter::Only(kinds) => kinds
                .iter()
                .filter_map(|kind| {
                    let state = graph.frontier(id, kind);
                    (!state.is_complete()).then(|| ScopeGap::Frontier {
                        resource: id.clone(),
                        relation: Some(kind.clone()),
                        state,
                    })
                })
                .collect(),
            filter => {
                let declared = graph
                    .frontiers(id)
                    .into_iter()
                    .filter(|(kind, _)| filter.allows(kind))
                    .collect::<Vec<_>>();
                if declared.is_empty() {
                    return vec![ScopeGap::Frontier {
                        resource: id.clone(),
                        relation: None,
                        state: DiscoveryState::Unobserved,
                    }];
                }
                declared
                    .into_iter()
                    .filter(|(_, state)| !state.is_complete())
                    .map(|(kind, state)| ScopeGap::Frontier {
                        resource: id.clone(),
                        relation: Some(kind),
                        state,
                    })
                    .collect()
            }
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct VisitedResource {
    pub resource: ResourceId,
    pub depth: u32,
}

/// Why a resolution may be missing resources.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "gap", rename_all = "camelCase", rename_all_fields = "camelCase")]
pub enum ScopeGap {
    /// A root the scope names is not in the graph at all.
    MissingRoot { resource: ResourceId },
    /// The neighborhood the walk relied on is not fully discovered. `relation` is `None` when the
    /// resource declared no frontier whatsoever.
    Frontier {
        resource: ResourceId,
        relation: Option<RelationKind>,
        state: DiscoveryState,
    },
    /// Expansion stopped because of [`Scope::max_depth`].
    DepthLimit { resource: ResourceId },
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ScopeCompleteness {
    /// Every resource the scope describes is present in the result.
    Complete,
    /// Resources may be missing; see [`ScopeResolution::gaps`].
    Incomplete,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ScopeResolution {
    /// The graph revision the answer was computed from.
    pub revision: u64,
    /// The visited resources that satisfy the predicate, in traversal order.
    pub matched: Vec<ResourceId>,
    /// Everything the walk touched, including resources the predicate rejected.
    pub visited: Vec<VisitedResource>,
    pub gaps: Vec<ScopeGap>,
}

impl ScopeResolution {
    pub fn completeness(&self) -> ScopeCompleteness {
        if self.gaps.is_empty() {
            ScopeCompleteness::Complete
        } else {
            ScopeCompleteness::Incomplete
        }
    }

    pub fn is_complete(&self) -> bool {
        self.gaps.is_empty()
    }

    pub fn contains(&self, id: &ResourceId) -> bool {
        self.matched.iter().any(|matched| matched == id)
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use super::*;
    use crate::capability::CapabilityObject;
    use crate::capability::testing::{FakeBrowser, FakeFrame, FakeProcess};
    use crate::resource_graph::{
        GraphDelta, ResourceFacts, ResourceGraph, ResourceUpsert, SourceId,
    };

    fn id(namespace: &str, key: &str) -> ResourceId {
        ResourceId::from_parts(namespace, [key]).unwrap()
    }

    struct Fixture {
        graph: ResourceGraph,
        process: ResourceId,
        child: ResourceId,
        browser: ResourceId,
        page: ResourceId,
        frame: ResourceId,
    }

    /// process -contains-> browser -contains-> page -contains-> frame, plus process -spawned->
    /// child. Every frontier is declared, so the fixture is fully discovered by construction.
    fn fixture() -> Fixture {
        let mut graph = ResourceGraph::new();
        let source = SourceId::new("fixture");
        let process = id("process", "1");
        let child = id("process", "2");
        let browser = id("browser", "B");
        let page = id("page", "P");
        let frame = id("frame", "F");

        graph
            .apply(
                GraphDelta::for_source(source)
                    .upsert(
                        ResourceUpsert::new(
                            process.clone(),
                            ResourceFacts::of_kind(ResourceKind::process()),
                        )
                        .with_capability(CapabilityObject::Process(Arc::new(FakeProcess::new(1))))
                        .with_frontier(RelationKind::Contains, DiscoveryState::Live)
                        .with_frontier(RelationKind::Spawned, DiscoveryState::SnapshotComplete),
                    )
                    .upsert(
                        ResourceUpsert::new(
                            child.clone(),
                            ResourceFacts::of_kind(ResourceKind::process()),
                        )
                        .with_capability(CapabilityObject::Process(Arc::new(FakeProcess::new(2))))
                        .with_frontier(RelationKind::Contains, DiscoveryState::SnapshotComplete)
                        .with_frontier(RelationKind::Spawned, DiscoveryState::SnapshotComplete),
                    )
                    .upsert(
                        ResourceUpsert::new(
                            browser.clone(),
                            ResourceFacts::of_kind(ResourceKind::browser()),
                        )
                        .with_capability(CapabilityObject::Browser(Arc::new(FakeBrowser::new(
                            Vec::new(),
                        ))))
                        .with_frontier(RelationKind::Contains, DiscoveryState::Live),
                    )
                    .upsert(
                        ResourceUpsert::new(
                            page.clone(),
                            ResourceFacts::of_kind(ResourceKind::page()),
                        )
                        .with_frontier(RelationKind::Contains, DiscoveryState::Live),
                    )
                    .upsert(
                        ResourceUpsert::new(
                            frame.clone(),
                            ResourceFacts::of_kind(ResourceKind::frame()),
                        )
                        .with_capability(CapabilityObject::Frame(Arc::new(FakeFrame::new("F"))))
                        .with_frontier(RelationKind::Contains, DiscoveryState::SnapshotComplete),
                    )
                    .relate(RelationKind::Contains, process.clone(), browser.clone())
                    .relate(RelationKind::Contains, browser.clone(), page.clone())
                    .relate(RelationKind::Contains, page.clone(), frame.clone())
                    .relate(RelationKind::Spawned, process.clone(), child.clone()),
            )
            .unwrap();

        Fixture {
            graph,
            process,
            child,
            browser,
            page,
            frame,
        }
    }

    #[test]
    fn traversal_visits_everything_reachable_in_breadth_first_order() {
        let fixture = fixture();
        let resolution = Scope::from_root(fixture.process.clone()).resolve(&fixture.graph);

        assert_eq!(
            resolution
                .visited
                .iter()
                .map(|entry| (entry.resource.clone(), entry.depth))
                .collect::<Vec<_>>(),
            vec![
                (fixture.process.clone(), 0),
                (fixture.browser.clone(), 1),
                (fixture.child.clone(), 1),
                (fixture.page.clone(), 2),
                (fixture.frame.clone(), 3),
            ]
        );
        assert_eq!(resolution.completeness(), ScopeCompleteness::Complete);
    }

    #[test]
    fn relation_filter_restricts_which_edges_are_crossed() {
        let fixture = fixture();
        let resolution = Scope::from_root(fixture.process.clone())
            .with_relations(RelationFilter::only([RelationKind::Contains]))
            .resolve(&fixture.graph);

        assert!(!resolution.contains(&fixture.child));
        assert!(resolution.contains(&fixture.frame));

        let resolution = Scope::from_root(fixture.process.clone())
            .with_relations(RelationFilter::excluding([RelationKind::Contains]))
            .resolve(&fixture.graph);
        assert_eq!(
            resolution.matched,
            vec![fixture.process.clone(), fixture.child.clone()]
        );
    }

    #[test]
    fn incoming_traversal_finds_ancestors() {
        let fixture = fixture();
        let resolution = Scope::from_root(fixture.frame.clone())
            .with_direction(TraversalDirection::Incoming)
            .with_relations(RelationFilter::only([RelationKind::Contains]))
            .resolve(&fixture.graph);

        assert_eq!(
            resolution.matched,
            vec![
                fixture.frame.clone(),
                fixture.page.clone(),
                fixture.browser.clone(),
                fixture.process.clone(),
            ]
        );
    }

    #[test]
    fn max_depth_limits_expansion_and_is_reported() {
        let fixture = fixture();
        let resolution = Scope::from_root(fixture.process.clone())
            .with_relations(RelationFilter::only([RelationKind::Contains]))
            .with_max_depth(1)
            .resolve(&fixture.graph);

        assert_eq!(
            resolution.matched,
            vec![fixture.process.clone(), fixture.browser.clone()]
        );
        assert_eq!(
            resolution.gaps,
            vec![ScopeGap::DepthLimit {
                resource: fixture.browser.clone()
            }]
        );
        assert_eq!(resolution.completeness(), ScopeCompleteness::Incomplete);

        let roots_only = Scope::from_root(fixture.process.clone())
            .with_max_depth(0)
            .resolve(&fixture.graph);
        assert_eq!(roots_only.matched, vec![fixture.process.clone()]);
        assert!(!roots_only.is_complete());
    }

    #[test]
    fn a_leaf_with_a_complete_frontier_does_not_produce_a_depth_gap() {
        let fixture = fixture();
        let resolution = Scope::from_root(fixture.frame.clone())
            .with_relations(RelationFilter::only([RelationKind::Contains]))
            .with_max_depth(0)
            .resolve(&fixture.graph);

        assert_eq!(resolution.matched, vec![fixture.frame.clone()]);
        assert!(resolution.is_complete());
    }

    #[test]
    fn predicates_select_by_kind_and_by_capability() {
        let fixture = fixture();
        let by_kind = Scope::from_root(fixture.process.clone())
            .with_predicate(ResourcePredicate::kind(ResourceKind::process()))
            .resolve(&fixture.graph);
        assert_eq!(
            by_kind.matched,
            vec![fixture.process.clone(), fixture.child.clone()]
        );

        let by_capability = Scope::from_root(fixture.process.clone())
            .with_predicate(ResourcePredicate::capability(CapabilityKind::Frame))
            .resolve(&fixture.graph);
        assert_eq!(by_capability.matched, vec![fixture.frame.clone()]);

        let composed = Scope::from_root(fixture.process.clone())
            .with_predicate(ResourcePredicate::All(vec![
                ResourcePredicate::capability(CapabilityKind::Process),
                ResourcePredicate::not(ResourcePredicate::capability(CapabilityKind::Browser)),
            ]))
            .resolve(&fixture.graph);
        assert_eq!(
            composed.matched,
            vec![fixture.process.clone(), fixture.child.clone()]
        );

        let any_of = Scope::from_root(fixture.process.clone())
            .with_predicate(ResourcePredicate::AnyOf(vec![
                ResourcePredicate::kind(ResourceKind::page()),
                ResourcePredicate::kind(ResourceKind::frame()),
            ]))
            .resolve(&fixture.graph);
        assert_eq!(
            any_of.matched,
            vec![fixture.page.clone(), fixture.frame.clone()]
        );
    }

    #[test]
    fn traversal_passes_through_resources_the_predicate_rejects() {
        let fixture = fixture();
        let resolution = Scope::from_root(fixture.process.clone())
            .with_predicate(ResourcePredicate::kind(ResourceKind::frame()))
            .resolve(&fixture.graph);

        assert_eq!(resolution.matched, vec![fixture.frame.clone()]);
        assert_eq!(resolution.visited.len(), 5);
    }

    #[test]
    fn an_incomplete_frontier_makes_the_resolution_incomplete() {
        let mut fixture = fixture();
        fixture
            .graph
            .apply(
                GraphDelta::for_source(SourceId::new("fixture")).upsert(
                    ResourceUpsert::new(
                        fixture.page.clone(),
                        ResourceFacts::of_kind(ResourceKind::page()),
                    )
                    .with_frontier(
                        RelationKind::Contains,
                        DiscoveryState::partial("renderer busy"),
                    ),
                ),
            )
            .unwrap();

        let resolution = Scope::from_root(fixture.process.clone())
            .with_relations(RelationFilter::only([RelationKind::Contains]))
            .resolve(&fixture.graph);

        assert_eq!(resolution.completeness(), ScopeCompleteness::Incomplete);
        assert_eq!(
            resolution.gaps,
            vec![ScopeGap::Frontier {
                resource: fixture.page.clone(),
                relation: Some(RelationKind::Contains),
                state: DiscoveryState::partial("renderer busy"),
            }]
        );
        assert!(
            resolution.contains(&fixture.frame),
            "known children stay visible; only the frontier claims more may exist"
        );
    }

    #[test]
    fn a_more_complete_source_repairs_the_frontier() {
        let mut fixture = fixture();
        fixture
            .graph
            .apply(
                GraphDelta::for_source(SourceId::new("pessimist")).upsert(
                    ResourceUpsert::new(
                        fixture.browser.clone(),
                        ResourceFacts::of_kind(ResourceKind::browser()),
                    )
                    .with_frontier(RelationKind::Contains, DiscoveryState::failed("timeout")),
                ),
            )
            .unwrap();

        let resolution = Scope::from_root(fixture.process.clone())
            .with_relations(RelationFilter::only([RelationKind::Contains]))
            .resolve(&fixture.graph);
        assert!(
            resolution.is_complete(),
            "the live source still knows the browser's children: {:?}",
            resolution.gaps
        );
    }

    #[test]
    fn undeclared_frontiers_and_missing_roots_are_gaps() {
        let mut graph = ResourceGraph::new();
        let source = SourceId::new("s");
        let lonely = id("process", "1");
        graph
            .apply(GraphDelta::for_source(source).upsert(ResourceUpsert::new(
                lonely.clone(),
                ResourceFacts::of_kind(ResourceKind::process()),
            )))
            .unwrap();

        let missing = id("process", "404");
        let resolution = Scope::from_roots([lonely.clone(), missing.clone()]).resolve(&graph);

        assert_eq!(resolution.matched, vec![lonely.clone()]);
        assert_eq!(
            resolution.gaps,
            vec![
                ScopeGap::MissingRoot {
                    resource: missing.clone()
                },
                ScopeGap::Frontier {
                    resource: lonely.clone(),
                    relation: None,
                    state: DiscoveryState::Unobserved,
                },
            ]
        );
    }

    #[test]
    fn scopes_round_trip_as_json() {
        let scope = Scope::from_root(id("process", "1"))
            .with_relations(RelationFilter::only([
                RelationKind::Contains,
                RelationKind::other("attaches"),
            ]))
            .with_direction(TraversalDirection::Both)
            .with_predicate(ResourcePredicate::capability(CapabilityKind::Debug))
            .with_max_depth(3);
        let text = serde_json::to_string(&scope).unwrap();
        assert_eq!(serde_json::from_str::<Scope>(&text).unwrap(), scope);
    }
}
