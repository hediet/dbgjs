//! Compact, immutable access to the graph in a V8 `.heapsnapshot`.
//!
//! The large flattened `nodes`, `edges`, `locations`, and `strings` arrays are
//! consumed with serde visitors rather than materialized as `serde_json::Value`.

use std::cmp::Reverse;
use std::collections::{BTreeMap, BinaryHeap, HashMap, HashSet, VecDeque};
use std::fmt;
use std::io::{BufReader, Read};
use std::sync::OnceLock;

use regex::Regex;
use serde::Deserialize;
use serde::de::{DeserializeSeed, IgnoredAny, MapAccess, SeqAccess, Visitor};

const SNAPSHOT_READ_BUFFER_SIZE: usize = 1024 * 1024;
const NO_NODE: u32 = u32::MAX;
const MAX_STRING_RECONSTRUCTION_DEPTH: usize = 64;
const MAX_RECONSTRUCTED_STRING_BYTES: usize = 1024 * 1024;

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct NodeIndex(pub u32);

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct EdgeIndex(pub u32);

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Location {
    pub node: NodeIndex,
    pub script_id: i64,
    pub line: u32,
    pub column: u32,
}

#[derive(Debug)]
pub struct HeapGraph {
    node_types: Vec<String>,
    edge_types: Vec<String>,
    strings: Vec<String>,

    // Nodes (SoA).
    node_type: Vec<u32>,
    node_name: Vec<u32>,
    node_id: Vec<u64>,
    node_shallow_size: Vec<u64>,
    edge_starts: Vec<u32>,

    // Edges (SoA, grouped by source node).
    edge_type: Vec<u32>,
    edge_name_or_index: Vec<u64>,
    edge_target: Vec<u32>,

    // Locations (SoA, in snapshot order).
    location_node: Vec<u32>,
    location_script_id: Vec<i64>,
    location_line: Vec<u32>,
    location_column: Vec<u32>,

    id_index: OnceLock<HashMap<u64, u32>>,
    reverse: OnceLock<ReverseIndex>,
    dominators: OnceLock<Result<DominatorAnalysis, AnalysisError>>,
}

#[derive(Debug)]
struct ReverseIndex {
    starts: Vec<u32>,
    sources: Vec<u32>,
    edges: Vec<u32>,
}

/// Parses one V8 heap snapshot. All indexes whose cardinality contributes to
/// graph storage are checked before conversion to `u32`.
pub fn parse_heap_graph(reader: impl Read) -> Result<HeapGraph, HeapGraphParseError> {
    HeapGraph::parse(reader)
}

impl HeapGraph {
    pub fn parse(reader: impl Read) -> Result<Self, HeapGraphParseError> {
        let mut builder = GraphBuilder::default();
        let reader = BufReader::with_capacity(SNAPSHOT_READ_BUFFER_SIZE, reader);
        let mut deserializer = serde_json::Deserializer::from_reader(reader);
        let result = HeapGraphSeed {
            builder: &mut builder,
        }
        .deserialize(&mut deserializer);
        if let Err(json) = result {
            if let Some(error) = builder.error.take() {
                return Err(error);
            }
            return Err(HeapGraphParseError::Json(json));
        }
        deserializer.end().map_err(HeapGraphParseError::Json)?;
        builder.finish()
    }

    pub fn node_count(&self) -> usize {
        self.node_id.len()
    }

    pub fn edge_count(&self) -> usize {
        self.edge_type.len()
    }

    pub fn strings(&self) -> &[String] {
        &self.strings
    }

    pub fn node_kinds(&self) -> &[String] {
        &self.node_types
    }

    pub fn edge_kinds(&self) -> &[String] {
        &self.edge_types
    }

    pub fn node_summary(&self, node: NodeIndex) -> Result<NodeSummary<'_>, AnalysisError> {
        let index = self.checked_node(node)?;
        let raw_name_index = self.node_name[index];
        let raw_name = self.string(raw_name_index).ok_or_else(|| {
            AnalysisError::CorruptGraph(format!(
                "node {} has invalid string index {}",
                node.0, raw_name_index
            ))
        })?;
        let node_type = self.node_type_name(index)?;
        let outgoing_references =
            usize::try_from(self.edge_starts[index + 1] - self.edge_starts[index])
                .expect("u32 fits usize");
        let reverse = self.reverse_index();
        let incoming_references =
            usize::try_from(reverse.starts[index + 1] - reverse.starts[index])
                .expect("u32 fits usize");
        Ok(NodeSummary {
            index: node,
            node_type,
            heap_object_id: self.node_id[index],
            raw_name_index,
            raw_name,
            string_value: self.is_string_type(index).then_some(raw_name),
            shallow_size: self.node_shallow_size[index],
            outgoing_references,
            incoming_references,
        })
    }

    pub fn node_summaries(
        &self,
    ) -> impl ExactSizeIterator<Item = Result<NodeSummary<'_>, AnalysisError>> + '_ {
        (0..self.node_count()).map(|index| {
            self.node_summary(NodeIndex(
                u32::try_from(index).expect("node count was checked while parsing"),
            ))
        })
    }

    /// Flattens V8 cons/sliced string nodes with cycle and depth protection.
    /// Results never exceed 1 MiB, and `max_chars` can impose a smaller display
    /// bound. Regular snapshots omit slice offsets, so those return a truncated
    /// preview of the reconstructed backing string.
    pub fn reconstructed_string(
        &self,
        node: NodeIndex,
        max_chars: Option<usize>,
    ) -> Result<Option<ReconstructedString>, AnalysisError> {
        let index = self.checked_node(node)?;
        if !self.is_string_type(index) {
            return Ok(None);
        }
        let mut state = StringReconstruction {
            graph: self,
            max_chars: max_chars.unwrap_or(usize::MAX),
            max_bytes: MAX_RECONSTRUCTED_STRING_BYTES,
            active: HashSet::new(),
            truncated: false,
            uncertain: false,
        };
        let mut value = String::new();
        state.append(node, 0, &mut value)?;
        Ok(Some(ReconstructedString {
            value,
            truncated: state.truncated,
            exact_prefix: !state.uncertain,
        }))
    }

    /// Looks up an object id within this capture. The index is intentionally
    /// owned by the graph, so ids from different captures are never conflated.
    pub fn node_by_heap_object_id(&self, heap_object_id: u64) -> Option<NodeIndex> {
        self.id_index
            .get_or_init(|| {
                self.node_id
                    .iter()
                    .enumerate()
                    .map(|(index, &id)| {
                        (
                            id,
                            u32::try_from(index).expect("node count was checked while parsing"),
                        )
                    })
                    .fold(HashMap::new(), |mut ids, (id, index)| {
                        ids.entry(id).or_insert(index);
                        ids
                    })
            })
            .get(&heap_object_id)
            .copied()
            .map(NodeIndex)
    }

    pub fn locations(&self) -> impl ExactSizeIterator<Item = Location> + '_ {
        (0..self.location_node.len()).map(|index| Location {
            node: NodeIndex(self.location_node[index]),
            script_id: self.location_script_id[index],
            line: self.location_line[index],
            column: self.location_column[index],
        })
    }

    pub fn locations_for_node(
        &self,
        node: NodeIndex,
    ) -> Result<impl Iterator<Item = Location> + '_, AnalysisError> {
        self.checked_node(node)?;
        Ok(self
            .locations()
            .filter(move |location| location.node == node))
    }

    pub fn outgoing_references(
        &self,
        node: NodeIndex,
    ) -> Result<OutgoingReferences<'_>, AnalysisError> {
        let index = self.checked_node(node)?;
        Ok(OutgoingReferences {
            graph: self,
            source: node,
            next: self.edge_starts[index],
            end: self.edge_starts[index + 1],
        })
    }

    /// Returns incoming references backed by a compact CSR reverse index. The
    /// reverse index is constructed only on the first incoming-edge operation.
    pub fn incoming_references(
        &self,
        node: NodeIndex,
    ) -> Result<IncomingReferences<'_>, AnalysisError> {
        let index = self.checked_node(node)?;
        let reverse = self.reverse_index();
        Ok(IncomingReferences {
            graph: self,
            reverse,
            next: reverse.starts[index],
            end: reverse.starts[index + 1],
        })
    }

    pub fn select(&self, selector: &NodeSelector<'_>) -> Vec<NodeIndex> {
        let limit = selector.limit.unwrap_or(usize::MAX);
        if limit == 0 {
            return Vec::new();
        }
        let mut result = Vec::new();
        for index in 0..self.node_count() {
            if selector
                .heap_object_id
                .is_some_and(|expected| self.node_id[index] != expected)
            {
                continue;
            }
            let Ok(node_type) = self.node_type_name(index) else {
                continue;
            };
            if selector
                .node_type
                .is_some_and(|expected| expected != node_type)
            {
                continue;
            }
            let Some(raw_name) = self.string(self.node_name[index]) else {
                continue;
            };
            if selector
                .raw_name
                .as_ref()
                .is_some_and(|matcher| !matcher.matches(raw_name))
            {
                continue;
            }
            if let Some(matcher) = &selector.string_value {
                let matches = match self.node_type_name(index) {
                    Ok("string") => matcher.matches(raw_name),
                    Ok("concatenated string" | "sliced string") => self
                        .reconstructed_string(
                            NodeIndex(
                                u32::try_from(index).expect("node count was checked while parsing"),
                            ),
                            None,
                        )
                        .ok()
                        .flatten()
                        .is_some_and(|value| {
                            (!value.truncated
                                || (value.exact_prefix
                                    && matches!(matcher, TextMatcher::Contains(_))))
                                && matcher.matches(&value.value)
                        }),
                    _ => false,
                };
                if !matches {
                    continue;
                }
            }
            let size = self.node_shallow_size[index];
            if selector
                .min_shallow_size
                .is_some_and(|minimum| size < minimum)
                || selector
                    .max_shallow_size
                    .is_some_and(|maximum| size > maximum)
            {
                continue;
            }
            result.push(NodeIndex(
                u32::try_from(index).expect("node count was checked while parsing"),
            ));
            if result.len() == limit {
                break;
            }
        }
        result
    }

    pub fn shortest_path(
        &self,
        from: NodeIndex,
        to: NodeIndex,
        options: PathOptions,
    ) -> Result<Option<GraphPath>, AnalysisError> {
        self.checked_node(from)?;
        self.checked_node(to)?;
        if from == to {
            return Ok(Some(GraphPath {
                nodes: vec![from],
                steps: Vec::new(),
                cost: 0,
            }));
        }
        match options.cost {
            CostPolicy::Edges => self.shortest_path_bfs(from, to, options),
            CostPolicy::Readable => self.shortest_path_dijkstra(from, to, options),
        }
    }

    pub fn root_path(
        &self,
        target: NodeIndex,
        edge_policy: EdgePolicy,
        cost: CostPolicy,
    ) -> Result<Option<GraphPath>, AnalysisError> {
        if self.node_count() == 0 {
            return Err(AnalysisError::EmptyGraph);
        }
        self.shortest_path(
            NodeIndex(0),
            target,
            PathOptions {
                direction: PathDirection::Outgoing,
                edge_policy,
                cost,
            },
        )
    }

    /// Computes immediate dominators and retained sizes over strong outgoing
    /// edges rooted at node index 0. Lengauer-Tarjan is used, with iterative
    /// DFS and path compression; unreachable nodes have no result.
    pub fn dominators(&self) -> Result<&DominatorAnalysis, AnalysisError> {
        let result = self.dominators.get_or_init(|| self.compute_dominators());
        match result {
            Ok(analysis) => Ok(analysis),
            Err(error) => Err(error.clone()),
        }
    }

    pub fn aggregate(&self, by: AggregateBy) -> SnapshotAggregate {
        let mut groups = BTreeMap::<String, AggregateValue>::new();
        for index in 0..self.node_count() {
            let key = match by {
                AggregateBy::NodeType => self.node_type_name(index).ok().map(str::to_owned),
                AggregateBy::RawName => self.string(self.node_name[index]).map(str::to_owned),
                AggregateBy::StringValue if self.is_string_type(index) => self
                    .reconstructed_string(
                        NodeIndex(
                            u32::try_from(index).expect("node count was checked while parsing"),
                        ),
                        None,
                    )
                    .ok()
                    .flatten()
                    .and_then(|value| (!value.truncated).then_some(value.value)),
                AggregateBy::StringValue => None,
            };
            let Some(key) = key else {
                continue;
            };
            let value = groups.entry(key).or_default();
            value.count += 1;
            value.shallow_size += u128::from(self.node_shallow_size[index]);
        }
        SnapshotAggregate { by, groups }
    }

    pub fn diff(&self, newer: &HeapGraph, by: AggregateBy) -> SnapshotDiff {
        diff_aggregates(&self.aggregate(by), &newer.aggregate(by))
    }

    fn checked_node(&self, node: NodeIndex) -> Result<usize, AnalysisError> {
        let index = usize::try_from(node.0).expect("u32 fits usize");
        if index >= self.node_count() {
            return Err(AnalysisError::InvalidNodeIndex(node));
        }
        Ok(index)
    }

    fn string(&self, index: u32) -> Option<&str> {
        self.strings
            .get(usize::try_from(index).expect("u32 fits usize"))
            .map(String::as_str)
    }

    fn node_type_name(&self, node: usize) -> Result<&str, AnalysisError> {
        self.node_types
            .get(usize::try_from(self.node_type[node]).expect("u32 fits usize"))
            .map(String::as_str)
            .ok_or_else(|| AnalysisError::CorruptGraph("invalid node type index".to_owned()))
    }

    fn edge_type_name(&self, edge: usize) -> Result<&str, AnalysisError> {
        self.edge_types
            .get(usize::try_from(self.edge_type[edge]).expect("u32 fits usize"))
            .map(String::as_str)
            .ok_or_else(|| AnalysisError::CorruptGraph("invalid edge type index".to_owned()))
    }

    fn is_string_type(&self, node: usize) -> bool {
        matches!(
            self.node_type_name(node),
            Ok("string" | "concatenated string" | "sliced string")
        )
    }

    fn is_weak_edge(&self, edge: usize) -> bool {
        self.edge_type_name(edge) == Ok("weak")
    }

    fn edge_string_name(&self, edge: usize) -> Option<&str> {
        let edge_type = self.edge_type_name(edge).ok()?;
        if matches!(edge_type, "element" | "hidden") {
            return None;
        }
        let index = u32::try_from(self.edge_name_or_index[edge]).ok()?;
        self.string(index)
    }

    fn named_edge_target(&self, node: NodeIndex, name: &str) -> Option<NodeIndex> {
        self.outgoing_references(node)
            .ok()?
            .find(|reference| reference.name == Some(name))
            .map(|reference| reference.target)
    }

    fn numeric_edge_value(&self, node: NodeIndex, name: &str) -> Option<usize> {
        let target = self.named_edge_target(node, name)?;
        let index = self.checked_node(target).ok()?;
        self.string(self.node_name[index])?.parse().ok()
    }

    fn reference(
        &self,
        source: NodeIndex,
        edge: EdgeIndex,
    ) -> Result<HeapReference<'_>, AnalysisError> {
        let edge_index = usize::try_from(edge.0).expect("u32 fits usize");
        if edge_index >= self.edge_count() {
            return Err(AnalysisError::InvalidEdgeIndex(edge));
        }
        Ok(HeapReference {
            edge,
            source,
            target: NodeIndex(self.edge_target[edge_index]),
            edge_type: self.edge_type_name(edge_index)?,
            name_or_index: self.edge_name_or_index[edge_index],
            name: self.edge_string_name(edge_index),
        })
    }

    fn reverse_index(&self) -> &ReverseIndex {
        self.reverse.get_or_init(|| {
            let node_count = self.node_count();
            let mut counts = vec![0_u32; node_count];
            for &target in &self.edge_target {
                let count = &mut counts[usize::try_from(target).expect("u32 fits usize")];
                *count = count.checked_add(1).expect("total edge count fits u32");
            }
            let mut starts = vec![0_u32; node_count + 1];
            for index in 0..node_count {
                starts[index + 1] = starts[index]
                    .checked_add(counts[index])
                    .expect("total edge count fits u32");
            }
            let mut cursors = starts[..node_count].to_vec();
            let mut sources = vec![0_u32; self.edge_count()];
            let mut edges = vec![0_u32; self.edge_count()];
            for source in 0..node_count {
                let edge_start = usize::try_from(self.edge_starts[source]).expect("u32 fits usize");
                let edge_end =
                    usize::try_from(self.edge_starts[source + 1]).expect("u32 fits usize");
                for edge in edge_start..edge_end {
                    let target = usize::try_from(self.edge_target[edge]).expect("u32 fits usize");
                    let slot = usize::try_from(cursors[target]).expect("u32 fits usize");
                    sources[slot] =
                        u32::try_from(source).expect("node count was checked while parsing");
                    edges[slot] =
                        u32::try_from(edge).expect("edge count was checked while parsing");
                    cursors[target] += 1;
                }
            }
            ReverseIndex {
                starts,
                sources,
                edges,
            }
        })
    }

    fn visit_neighbors(
        &self,
        node: NodeIndex,
        options: PathOptions,
        mut visit: impl FnMut(NodeIndex, EdgeIndex, TraversalDirection),
    ) {
        let index = usize::try_from(node.0).expect("u32 fits usize");
        if matches!(
            options.direction,
            PathDirection::Outgoing | PathDirection::Either
        ) {
            for edge in self.edge_starts[index]..self.edge_starts[index + 1] {
                let edge_index = usize::try_from(edge).expect("u32 fits usize");
                if options.edge_policy == EdgePolicy::Strong && self.is_weak_edge(edge_index) {
                    continue;
                }
                visit(
                    NodeIndex(self.edge_target[edge_index]),
                    EdgeIndex(edge),
                    TraversalDirection::Outgoing,
                );
            }
        }
        if matches!(
            options.direction,
            PathDirection::Incoming | PathDirection::Either
        ) {
            let reverse = self.reverse_index();
            for slot in reverse.starts[index]..reverse.starts[index + 1] {
                let slot = usize::try_from(slot).expect("u32 fits usize");
                let edge = reverse.edges[slot];
                if options.edge_policy == EdgePolicy::Strong
                    && self.is_weak_edge(usize::try_from(edge).expect("u32 fits usize"))
                {
                    continue;
                }
                visit(
                    NodeIndex(reverse.sources[slot]),
                    EdgeIndex(edge),
                    TraversalDirection::Incoming,
                );
            }
        }
    }

    fn shortest_path_bfs(
        &self,
        from: NodeIndex,
        to: NodeIndex,
        options: PathOptions,
    ) -> Result<Option<GraphPath>, AnalysisError> {
        let mut predecessor = vec![None; self.node_count()];
        let mut seen = vec![false; self.node_count()];
        let mut queue = VecDeque::new();
        seen[usize::try_from(from.0).expect("u32 fits usize")] = true;
        queue.push_back(from);
        while let Some(node) = queue.pop_front() {
            let mut found = false;
            self.visit_neighbors(node, options, |neighbor, edge, direction| {
                let index = usize::try_from(neighbor.0).expect("u32 fits usize");
                if !seen[index] {
                    seen[index] = true;
                    predecessor[index] = Some((node, edge, direction));
                    queue.push_back(neighbor);
                    found |= neighbor == to;
                }
            });
            if found {
                return Ok(Some(reconstruct_path(
                    from,
                    to,
                    &predecessor,
                    path_edge_count(&predecessor, from, to),
                )));
            }
        }
        Ok(None)
    }

    fn shortest_path_dijkstra(
        &self,
        from: NodeIndex,
        to: NodeIndex,
        options: PathOptions,
    ) -> Result<Option<GraphPath>, AnalysisError> {
        let mut distance = vec![u64::MAX; self.node_count()];
        let mut predecessor = vec![None; self.node_count()];
        let mut heap = BinaryHeap::new();
        distance[usize::try_from(from.0).expect("u32 fits usize")] = 0;
        heap.push(Reverse((0_u64, from.0)));
        while let Some(Reverse((cost, raw_node))) = heap.pop() {
            let node = NodeIndex(raw_node);
            let node_index = usize::try_from(raw_node).expect("u32 fits usize");
            if cost != distance[node_index] {
                continue;
            }
            if node == to {
                return Ok(Some(reconstruct_path(from, to, &predecessor, cost)));
            }
            self.visit_neighbors(node, options, |neighbor, edge, direction| {
                let weight = self.readable_edge_weight(edge);
                let Some(next_cost) = cost.checked_add(weight) else {
                    return;
                };
                let neighbor_index = usize::try_from(neighbor.0).expect("u32 fits usize");
                if next_cost < distance[neighbor_index] {
                    distance[neighbor_index] = next_cost;
                    predecessor[neighbor_index] = Some((node, edge, direction));
                    heap.push(Reverse((next_cost, neighbor.0)));
                }
            });
        }
        Ok(None)
    }

    /// Readability weights prefer ordinary named references. Named
    /// property/context/shortcut edges cost 1, elements cost 2, internal edges
    /// cost 3, hidden edges cost 4, and weak edges cost 5. Other edge kinds cost
    /// 2 when named and 3 otherwise.
    fn readable_edge_weight(&self, edge: EdgeIndex) -> u64 {
        let edge = usize::try_from(edge.0).expect("u32 fits usize");
        match self.edge_type_name(edge).unwrap_or("") {
            "property" | "context" | "context variable" | "shortcut"
                if self.edge_string_name(edge).is_some() =>
            {
                1
            }
            "element" => 2,
            "internal" => 3,
            "hidden" => 4,
            "weak" => 5,
            _ if self.edge_string_name(edge).is_some() => 2,
            _ => 3,
        }
    }

    fn compute_dominators(&self) -> Result<DominatorAnalysis, AnalysisError> {
        if self.node_count() == 0 {
            return Err(AnalysisError::EmptyGraph);
        }

        // Iterative DFS numbering. Index zero in the DFS arrays is a sentinel.
        let mut dfs_number = vec![0_u32; self.node_count()];
        let mut vertex = vec![NO_NODE];
        let mut parent = vec![0_u32];
        dfs_number[0] = 1;
        vertex.push(0);
        parent.push(0);
        let mut stack = vec![(0_u32, self.edge_starts[0])];
        while let Some((node, next_edge)) = stack.last_mut() {
            let node_index = usize::try_from(*node).expect("u32 fits usize");
            let end = self.edge_starts[node_index + 1];
            if *next_edge == end {
                stack.pop();
                continue;
            }
            let edge = *next_edge;
            *next_edge += 1;
            let edge_index = usize::try_from(edge).expect("u32 fits usize");
            if self.is_weak_edge(edge_index) {
                continue;
            }
            let target = self.edge_target[edge_index];
            let target_index = usize::try_from(target).expect("u32 fits usize");
            if dfs_number[target_index] != 0 {
                continue;
            }
            let number = u32::try_from(vertex.len())
                .map_err(|_| AnalysisError::Overflow("DFS node count"))?;
            dfs_number[target_index] = number;
            vertex.push(target);
            parent.push(dfs_number[node_index]);
            stack.push((target, self.edge_starts[target_index]));
        }

        let reachable = vertex.len() - 1;
        let mut semi: Vec<u32> = (0..=u32::try_from(reachable)
            .map_err(|_| AnalysisError::Overflow("reachable node count"))?)
            .collect();
        let mut idom = vec![0_u32; reachable + 1];
        let mut ancestor = vec![0_u32; reachable + 1];
        let mut label = semi.clone();
        let mut bucket_head = vec![0_u32; reachable + 1];
        let mut bucket_next = vec![0_u32; reachable + 1];
        let reverse = self.reverse_index();
        let mut eval_stack = Vec::new();

        for raw_w in (2..=reachable).rev() {
            let w = u32::try_from(raw_w).expect("reachable count fits u32");
            let node = usize::try_from(vertex[raw_w]).expect("u32 fits usize");
            for slot in reverse.starts[node]..reverse.starts[node + 1] {
                let slot = usize::try_from(slot).expect("u32 fits usize");
                let edge = usize::try_from(reverse.edges[slot]).expect("u32 fits usize");
                if self.is_weak_edge(edge) {
                    continue;
                }
                let predecessor = usize::try_from(reverse.sources[slot]).expect("u32 fits usize");
                let v = dfs_number[predecessor];
                if v == 0 {
                    continue;
                }
                let u = dominator_eval(v, &mut ancestor, &mut label, &semi, &mut eval_stack);
                let u_index = usize::try_from(u).expect("u32 fits usize");
                if semi[u_index] < semi[raw_w] {
                    semi[raw_w] = semi[u_index];
                }
            }

            let semi_w = usize::try_from(semi[raw_w]).expect("u32 fits usize");
            bucket_next[raw_w] = bucket_head[semi_w];
            bucket_head[semi_w] = w;
            ancestor[raw_w] = parent[raw_w];

            let parent_w = usize::try_from(parent[raw_w]).expect("u32 fits usize");
            let mut v = bucket_head[parent_w];
            bucket_head[parent_w] = 0;
            while v != 0 {
                let v_index = usize::try_from(v).expect("u32 fits usize");
                let next = bucket_next[v_index];
                let u = dominator_eval(v, &mut ancestor, &mut label, &semi, &mut eval_stack);
                let u_index = usize::try_from(u).expect("u32 fits usize");
                idom[v_index] = if semi[u_index] < semi[v_index] {
                    u
                } else {
                    parent[raw_w]
                };
                v = next;
            }
        }

        for w in 2..=reachable {
            if idom[w] != semi[w] {
                idom[w] = idom[usize::try_from(idom[w]).expect("u32 fits usize")];
            }
        }

        let mut immediate_dominator = vec![NO_NODE; self.node_count()];
        let mut retained_size = vec![0_u64; self.node_count()];
        for number in 1..=reachable {
            let node = usize::try_from(vertex[number]).expect("u32 fits usize");
            retained_size[node] = self.node_shallow_size[node];
            if number > 1 {
                immediate_dominator[node] =
                    vertex[usize::try_from(idom[number]).expect("u32 fits usize")];
            }
        }
        for number in (2..=reachable).rev() {
            let node = usize::try_from(vertex[number]).expect("u32 fits usize");
            let dominator = usize::try_from(immediate_dominator[node]).expect("u32 fits usize");
            retained_size[dominator] = retained_size[dominator]
                .checked_add(retained_size[node])
                .ok_or(AnalysisError::Overflow("retained size"))?;
        }

        Ok(DominatorAnalysis {
            root: NodeIndex(0),
            immediate_dominator,
            retained_size,
            dfs_number,
        })
    }
}

fn dominator_eval(
    vertex: u32,
    ancestor: &mut [u32],
    label: &mut [u32],
    semi: &[u32],
    stack: &mut Vec<u32>,
) -> u32 {
    let vertex_index = usize::try_from(vertex).expect("u32 fits usize");
    if ancestor[vertex_index] == 0 {
        return label[vertex_index];
    }
    stack.clear();
    let mut current = vertex;
    loop {
        let current_index = usize::try_from(current).expect("u32 fits usize");
        let parent = ancestor[current_index];
        let parent_index = usize::try_from(parent).expect("u32 fits usize");
        if parent == 0 || ancestor[parent_index] == 0 {
            break;
        }
        stack.push(current);
        current = parent;
    }
    while let Some(item) = stack.pop() {
        let item_index = usize::try_from(item).expect("u32 fits usize");
        let parent = ancestor[item_index];
        let parent_index = usize::try_from(parent).expect("u32 fits usize");
        let parent_label = usize::try_from(label[parent_index]).expect("u32 fits usize");
        let item_label = usize::try_from(label[item_index]).expect("u32 fits usize");
        if semi[parent_label] < semi[item_label] {
            label[item_index] = label[parent_index];
        }
        ancestor[item_index] = ancestor[parent_index];
    }
    label[vertex_index]
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ReconstructedString {
    pub value: String,
    pub truncated: bool,
    pub exact_prefix: bool,
}

struct StringReconstruction<'a> {
    graph: &'a HeapGraph,
    max_chars: usize,
    max_bytes: usize,
    active: HashSet<NodeIndex>,
    truncated: bool,
    uncertain: bool,
}

impl StringReconstruction<'_> {
    fn append(
        &mut self,
        node: NodeIndex,
        depth: usize,
        output: &mut String,
    ) -> Result<(), AnalysisError> {
        if !output.is_empty()
            && (output.len() >= self.max_bytes
                || (self.max_chars != usize::MAX && output.chars().count() >= self.max_chars))
        {
            self.truncated = true;
            return Ok(());
        }
        if depth >= MAX_STRING_RECONSTRUCTION_DEPTH || !self.active.insert(node) {
            self.truncated = true;
            self.uncertain = true;
            return Ok(());
        }
        let result = self.append_inner(node, depth, output);
        self.active.remove(&node);
        result
    }

    fn append_inner(
        &mut self,
        node: NodeIndex,
        depth: usize,
        output: &mut String,
    ) -> Result<(), AnalysisError> {
        let index = self.graph.checked_node(node)?;
        let node_type = self.graph.node_type_name(index)?;
        let raw_name = self
            .graph
            .string(self.graph.node_name[index])
            .ok_or_else(|| {
                AnalysisError::CorruptGraph(format!("node {} has an invalid string index", node.0))
            })?;
        let placeholder = match node_type {
            "concatenated string" => Some("(concatenated string)"),
            "sliced string" => Some("(sliced string)"),
            _ => None,
        };
        if placeholder.is_some_and(|placeholder| raw_name != placeholder) {
            self.append_text(output, raw_name);
            return Ok(());
        }
        match node_type {
            "string" => {
                self.append_text(output, raw_name);
            }
            "concatenated string" => {
                for part in ["first", "second"] {
                    let Some(target) = self.graph.named_edge_target(node, part) else {
                        self.truncated = true;
                        self.uncertain = true;
                        continue;
                    };
                    self.append(target, depth + 1, output)?;
                }
            }
            "sliced string" => {
                let Some(parent) = self.graph.named_edge_target(node, "parent") else {
                    self.truncated = true;
                    self.uncertain = true;
                    return Ok(());
                };
                let mut parent_value = String::new();
                let slice_bounds = (
                    self.graph.numeric_edge_value(node, "offset"),
                    self.graph.numeric_edge_value(node, "length"),
                );
                if let (Some(offset), Some(length)) = slice_bounds {
                    let original_max_chars = self.max_chars;
                    let original_truncated = self.truncated;
                    self.max_chars = offset
                        .saturating_add(length.min(original_max_chars))
                        .min(MAX_RECONSTRUCTED_STRING_BYTES);
                    self.append(parent, depth + 1, &mut parent_value)?;
                    self.max_chars = original_max_chars;
                    self.truncated = original_truncated;
                    if parent_value.chars().count()
                        < offset.saturating_add(length.min(original_max_chars))
                    {
                        self.truncated = true;
                    }
                    let slice = parent_value
                        .chars()
                        .skip(offset)
                        .take(length)
                        .collect::<String>();
                    if slice.chars().count() < length {
                        self.truncated = true;
                    }
                    self.append_text(output, &slice);
                } else {
                    self.append(parent, depth + 1, &mut parent_value)?;
                    // Regular V8 snapshots omit the slice offset. The backing
                    // string is useful as an explicitly incomplete preview.
                    self.truncated = true;
                    self.uncertain = true;
                    self.append_text(output, &parent_value);
                }
            }
            _ => {
                self.truncated = true;
                self.uncertain = true;
            }
        }
        Ok(())
    }

    fn append_text(&mut self, output: &mut String, value: &str) {
        let remaining_chars = if self.max_chars == usize::MAX {
            usize::MAX
        } else {
            self.max_chars.saturating_sub(output.chars().count())
        };
        let remaining_bytes = self.max_bytes.saturating_sub(output.len());
        if remaining_chars == 0 || remaining_bytes == 0 {
            self.truncated |= !value.is_empty();
            return;
        }

        let mut end = 0;
        let mut chars = 0;
        for (index, character) in value.char_indices() {
            let next = index + character.len_utf8();
            if chars == remaining_chars || next > remaining_bytes {
                break;
            }
            end = next;
            chars += 1;
        }
        output.push_str(&value[..end]);
        self.truncated |= end < value.len();
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct NodeSummary<'a> {
    pub index: NodeIndex,
    pub node_type: &'a str,
    pub heap_object_id: u64,
    pub raw_name_index: u32,
    pub raw_name: &'a str,
    /// The name-table text when the node kind is `string`, `concatenated
    /// string`, or `sliced string`; `None` for every other node kind.
    pub string_value: Option<&'a str>,
    pub shallow_size: u64,
    pub outgoing_references: usize,
    pub incoming_references: usize,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct HeapReference<'a> {
    pub edge: EdgeIndex,
    pub source: NodeIndex,
    pub target: NodeIndex,
    pub edge_type: &'a str,
    /// The unmodified V8 `name_or_index` field.
    pub name_or_index: u64,
    /// Resolved for named edge kinds; `element` and `hidden` retain only their
    /// numeric `name_or_index`.
    pub name: Option<&'a str>,
}

pub struct OutgoingReferences<'a> {
    graph: &'a HeapGraph,
    source: NodeIndex,
    next: u32,
    end: u32,
}

impl<'a> Iterator for OutgoingReferences<'a> {
    type Item = HeapReference<'a>;

    fn next(&mut self) -> Option<Self::Item> {
        if self.next == self.end {
            return None;
        }
        let edge = EdgeIndex(self.next);
        self.next += 1;
        self.graph.reference(self.source, edge).ok()
    }

    fn size_hint(&self) -> (usize, Option<usize>) {
        let remaining = usize::try_from(self.end - self.next).expect("u32 fits usize");
        (remaining, Some(remaining))
    }
}

impl ExactSizeIterator for OutgoingReferences<'_> {}

pub struct IncomingReferences<'a> {
    graph: &'a HeapGraph,
    reverse: &'a ReverseIndex,
    next: u32,
    end: u32,
}

impl<'a> Iterator for IncomingReferences<'a> {
    type Item = HeapReference<'a>;

    fn next(&mut self) -> Option<Self::Item> {
        if self.next == self.end {
            return None;
        }
        let slot = usize::try_from(self.next).expect("u32 fits usize");
        self.next += 1;
        self.graph
            .reference(
                NodeIndex(self.reverse.sources[slot]),
                EdgeIndex(self.reverse.edges[slot]),
            )
            .ok()
    }

    fn size_hint(&self) -> (usize, Option<usize>) {
        let remaining = usize::try_from(self.end - self.next).expect("u32 fits usize");
        (remaining, Some(remaining))
    }
}

impl ExactSizeIterator for IncomingReferences<'_> {}

#[derive(Clone, Copy)]
pub enum TextMatcher<'a> {
    Exact(&'a str),
    Contains(&'a str),
    Regex(&'a Regex),
}

impl fmt::Debug for TextMatcher<'_> {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Exact(value) => formatter.debug_tuple("Exact").field(value).finish(),
            Self::Contains(value) => formatter.debug_tuple("Contains").field(value).finish(),
            Self::Regex(value) => formatter
                .debug_tuple("Regex")
                .field(&value.as_str())
                .finish(),
        }
    }
}

impl TextMatcher<'_> {
    fn matches(&self, value: &str) -> bool {
        match self {
            Self::Exact(expected) => value == *expected,
            Self::Contains(expected) => value.contains(expected),
            Self::Regex(regex) => regex.is_match(value),
        }
    }
}

#[derive(Clone, Debug, Default)]
pub struct NodeSelector<'a> {
    pub heap_object_id: Option<u64>,
    pub node_type: Option<&'a str>,
    pub raw_name: Option<TextMatcher<'a>>,
    /// Matches name-table text and additionally requires one of V8's three
    /// string node kinds.
    pub string_value: Option<TextMatcher<'a>>,
    pub min_shallow_size: Option<u64>,
    pub max_shallow_size: Option<u64>,
    pub limit: Option<usize>,
}

impl<'a> NodeSelector<'a> {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn heap_object_id(mut self, heap_object_id: u64) -> Self {
        self.heap_object_id = Some(heap_object_id);
        self
    }

    pub fn node_type(mut self, node_type: &'a str) -> Self {
        self.node_type = Some(node_type);
        self
    }

    pub fn raw_name(mut self, matcher: TextMatcher<'a>) -> Self {
        self.raw_name = Some(matcher);
        self
    }

    pub fn string_value(mut self, matcher: TextMatcher<'a>) -> Self {
        self.string_value = Some(matcher);
        self
    }

    pub fn min_shallow_size(mut self, size: u64) -> Self {
        self.min_shallow_size = Some(size);
        self
    }

    pub fn max_shallow_size(mut self, size: u64) -> Self {
        self.max_shallow_size = Some(size);
        self
    }

    pub fn limit(mut self, limit: usize) -> Self {
        self.limit = Some(limit);
        self
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PathDirection {
    Outgoing,
    Incoming,
    Either,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum EdgePolicy {
    /// Excludes edges whose V8 type is exactly `weak`.
    Strong,
    All,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CostPolicy {
    /// Every traversal has cost one and is solved with BFS.
    Edges,
    /// Uses Dijkstra. Named property/context/shortcut edges cost 1, elements
    /// cost 2, internal edges cost 3, hidden edges cost 4, and weak edges cost
    /// 5. Other edge kinds cost 2 when named and 3 otherwise.
    Readable,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct PathOptions {
    pub direction: PathDirection,
    pub edge_policy: EdgePolicy,
    pub cost: CostPolicy,
}

impl Default for PathOptions {
    fn default() -> Self {
        Self {
            direction: PathDirection::Outgoing,
            edge_policy: EdgePolicy::Strong,
            cost: CostPolicy::Edges,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TraversalDirection {
    Outgoing,
    Incoming,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct PathStep {
    pub from: NodeIndex,
    pub to: NodeIndex,
    pub edge: EdgeIndex,
    pub direction: TraversalDirection,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct GraphPath {
    pub nodes: Vec<NodeIndex>,
    pub steps: Vec<PathStep>,
    pub cost: u64,
}

fn path_edge_count(
    predecessor: &[Option<(NodeIndex, EdgeIndex, TraversalDirection)>],
    from: NodeIndex,
    to: NodeIndex,
) -> u64 {
    let mut current = to;
    let mut count = 0;
    while current != from {
        count += 1;
        current = predecessor[usize::try_from(current.0).expect("u32 fits usize")]
            .expect("the target was reached")
            .0;
    }
    count
}

fn reconstruct_path(
    from: NodeIndex,
    to: NodeIndex,
    predecessor: &[Option<(NodeIndex, EdgeIndex, TraversalDirection)>],
    cost: u64,
) -> GraphPath {
    let mut nodes = vec![to];
    let mut steps = Vec::new();
    let mut current = to;
    while current != from {
        let (previous, edge, direction) = predecessor
            [usize::try_from(current.0).expect("u32 fits usize")]
        .expect("the target was reached");
        steps.push(PathStep {
            from: previous,
            to: current,
            edge,
            direction,
        });
        current = previous;
        nodes.push(current);
    }
    nodes.reverse();
    steps.reverse();
    GraphPath { nodes, steps, cost }
}

#[derive(Clone, Debug)]
pub struct DominatorAnalysis {
    root: NodeIndex,
    immediate_dominator: Vec<u32>,
    retained_size: Vec<u64>,
    dfs_number: Vec<u32>,
}

impl DominatorAnalysis {
    pub fn root(&self) -> NodeIndex {
        self.root
    }

    pub fn is_reachable(&self, node: NodeIndex) -> bool {
        self.dfs_number
            .get(usize::try_from(node.0).expect("u32 fits usize"))
            .is_some_and(|&number| number != 0)
    }

    pub fn immediate_dominator(&self, node: NodeIndex) -> Option<NodeIndex> {
        let index = usize::try_from(node.0).expect("u32 fits usize");
        if !self.is_reachable(node) {
            return None;
        }
        self.immediate_dominator
            .get(index)
            .copied()
            .filter(|&dominator| dominator != NO_NODE)
            .map(NodeIndex)
    }

    pub fn retained_size(&self, node: NodeIndex) -> Option<u64> {
        let index = usize::try_from(node.0).expect("u32 fits usize");
        self.is_reachable(node).then(|| self.retained_size[index])
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum AggregateBy {
    NodeType,
    RawName,
    /// Includes `string`, `concatenated string`, and `sliced string` nodes.
    StringValue,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct AggregateValue {
    pub count: u64,
    pub shallow_size: u128,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SnapshotAggregate {
    pub by: AggregateBy,
    pub groups: BTreeMap<String, AggregateValue>,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct AggregateDelta {
    pub count: i128,
    pub shallow_size: i128,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SnapshotDiff {
    pub by: AggregateBy,
    /// `newer - older`, retaining keys that disappear from either capture.
    pub groups: BTreeMap<String, AggregateDelta>,
}

pub fn diff_aggregates(older: &SnapshotAggregate, newer: &SnapshotAggregate) -> SnapshotDiff {
    assert_eq!(
        older.by, newer.by,
        "cannot diff aggregates with different keys"
    );
    let mut groups = BTreeMap::new();
    for key in older.groups.keys().chain(newer.groups.keys()) {
        let old = older.groups.get(key).copied().unwrap_or_default();
        let new = newer.groups.get(key).copied().unwrap_or_default();
        groups.insert(
            key.clone(),
            AggregateDelta {
                count: i128::from(new.count) - i128::from(old.count),
                shallow_size: i128::try_from(new.shallow_size)
                    .expect("u32 nodes times u64 shallow size fits i128")
                    - i128::try_from(old.shallow_size)
                        .expect("u32 nodes times u64 shallow size fits i128"),
            },
        );
    }
    SnapshotDiff {
        by: older.by,
        groups,
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum AnalysisError {
    EmptyGraph,
    InvalidNodeIndex(NodeIndex),
    InvalidEdgeIndex(EdgeIndex),
    Overflow(&'static str),
    CorruptGraph(String),
}

impl fmt::Display for AnalysisError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::EmptyGraph => formatter.write_str("the heap graph is empty"),
            Self::InvalidNodeIndex(index) => {
                write!(formatter, "invalid heap node index {}", index.0)
            }
            Self::InvalidEdgeIndex(index) => {
                write!(formatter, "invalid heap edge index {}", index.0)
            }
            Self::Overflow(what) => write!(formatter, "{what} overflow"),
            Self::CorruptGraph(message) => formatter.write_str(message),
        }
    }
}

impl std::error::Error for AnalysisError {}

#[derive(Debug)]
pub enum HeapGraphParseError {
    Json(serde_json::Error),
    MetadataMustPrecede(&'static str),
    DuplicateSection(&'static str),
    MissingSection(&'static str),
    MissingField(&'static str),
    InvalidMetadata(String),
    InvalidRecordLength(&'static str),
    Overflow {
        field: &'static str,
        value: u64,
    },
    SignedOverflow {
        field: &'static str,
        value: i64,
    },
    InvalidTypeIndex {
        field: &'static str,
        value: u64,
        type_count: usize,
    },
    InvalidNodeOffset(u64),
    InvalidStringIndex {
        context: &'static str,
        value: u64,
        string_count: usize,
    },
    EdgeCountMismatch {
        expected: u64,
        actual: u64,
    },
    HeaderCountMismatch {
        kind: &'static str,
        expected: u64,
        actual: u64,
    },
}

impl fmt::Display for HeapGraphParseError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Json(error) => write!(formatter, "invalid heap snapshot JSON: {error}"),
            Self::MetadataMustPrecede(section) => write!(
                formatter,
                "heap snapshot metadata must precede the {section} section"
            ),
            Self::DuplicateSection(section) => {
                write!(formatter, "duplicate heap snapshot section '{section}'")
            }
            Self::MissingSection(section) => {
                write!(formatter, "heap snapshot is missing section '{section}'")
            }
            Self::MissingField(field) => {
                write!(
                    formatter,
                    "heap snapshot metadata is missing field '{field}'"
                )
            }
            Self::InvalidMetadata(message) => write!(formatter, "invalid metadata: {message}"),
            Self::InvalidRecordLength(section) => {
                write!(formatter, "{section} array ended inside a record")
            }
            Self::Overflow { field, value } => {
                write!(
                    formatter,
                    "{field} value {value} does not fit compact storage"
                )
            }
            Self::SignedOverflow { field, value } => {
                write!(
                    formatter,
                    "{field} value {value} does not fit compact storage"
                )
            }
            Self::InvalidTypeIndex {
                field,
                value,
                type_count,
            } => write!(
                formatter,
                "{field} type index {value} is outside 0..{type_count}"
            ),
            Self::InvalidNodeOffset(value) => {
                write!(formatter, "node offset {value} is not a valid node")
            }
            Self::InvalidStringIndex {
                context,
                value,
                string_count,
            } => write!(
                formatter,
                "{context} string index {value} is outside 0..{string_count}"
            ),
            Self::EdgeCountMismatch { expected, actual } => write!(
                formatter,
                "node edge counts require {expected} edges, but {actual} were parsed"
            ),
            Self::HeaderCountMismatch {
                kind,
                expected,
                actual,
            } => write!(
                formatter,
                "snapshot header declares {expected} {kind}, but {actual} were parsed"
            ),
        }
    }
}

impl std::error::Error for HeapGraphParseError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Json(error) => Some(error),
            _ => None,
        }
    }
}

#[derive(Clone)]
struct Metadata {
    node_field_count: usize,
    node_type_offset: usize,
    node_name_offset: usize,
    node_id_offset: usize,
    node_self_size_offset: usize,
    node_edge_count_offset: usize,
    edge_field_count: usize,
    edge_type_offset: usize,
    edge_name_or_index_offset: usize,
    edge_to_node_offset: usize,
    location_field_count: usize,
    location_object_index_offset: usize,
    location_script_id_offset: usize,
    location_line_offset: usize,
    location_column_offset: usize,
    node_types: Vec<String>,
    edge_types: Vec<String>,
    expected_nodes: Option<u64>,
    expected_edges: Option<u64>,
}

#[derive(Deserialize)]
struct SnapshotHeader {
    meta: RawMetadata,
    #[serde(default)]
    node_count: Option<u64>,
    #[serde(default)]
    edge_count: Option<u64>,
}

#[derive(Deserialize)]
struct RawMetadata {
    node_fields: Vec<String>,
    node_types: Vec<serde_json::Value>,
    edge_fields: Vec<String>,
    edge_types: Vec<serde_json::Value>,
    #[serde(default)]
    location_fields: Vec<String>,
}

impl TryFrom<SnapshotHeader> for Metadata {
    type Error = HeapGraphParseError;

    fn try_from(header: SnapshotHeader) -> Result<Self, Self::Error> {
        fn field(fields: &[String], name: &'static str) -> Result<usize, HeapGraphParseError> {
            fields
                .iter()
                .position(|field| field == name)
                .ok_or(HeapGraphParseError::MissingField(name))
        }

        fn type_names(
            descriptors: &[serde_json::Value],
            offset: usize,
            kind: &str,
        ) -> Result<Vec<String>, HeapGraphParseError> {
            let values = descriptors
                .get(offset)
                .and_then(serde_json::Value::as_array)
                .ok_or_else(|| {
                    HeapGraphParseError::InvalidMetadata(format!(
                        "{kind} type field must have a string enum"
                    ))
                })?;
            values
                .iter()
                .map(|value| {
                    value.as_str().map(str::to_owned).ok_or_else(|| {
                        HeapGraphParseError::InvalidMetadata(format!(
                            "{kind} type enum contains a non-string value"
                        ))
                    })
                })
                .collect()
        }

        if header.meta.node_fields.is_empty() {
            return Err(HeapGraphParseError::InvalidMetadata(
                "node_fields cannot be empty".to_owned(),
            ));
        }
        if header.meta.edge_fields.is_empty() {
            return Err(HeapGraphParseError::InvalidMetadata(
                "edge_fields cannot be empty".to_owned(),
            ));
        }
        let node_type_offset = field(&header.meta.node_fields, "type")?;
        let edge_type_offset = field(&header.meta.edge_fields, "type")?;
        let location_offsets = if header.meta.location_fields.is_empty() {
            None
        } else {
            Some((
                field(&header.meta.location_fields, "object_index")?,
                field(&header.meta.location_fields, "script_id")?,
                field(&header.meta.location_fields, "line")?,
                field(&header.meta.location_fields, "column")?,
            ))
        };
        let (
            location_object_index_offset,
            location_script_id_offset,
            location_line_offset,
            location_column_offset,
        ) = location_offsets.unwrap_or((0, 0, 0, 0));
        Ok(Self {
            node_field_count: header.meta.node_fields.len(),
            node_type_offset,
            node_name_offset: field(&header.meta.node_fields, "name")?,
            node_id_offset: field(&header.meta.node_fields, "id")?,
            node_self_size_offset: field(&header.meta.node_fields, "self_size")?,
            node_edge_count_offset: field(&header.meta.node_fields, "edge_count")?,
            edge_field_count: header.meta.edge_fields.len(),
            edge_type_offset,
            edge_name_or_index_offset: field(&header.meta.edge_fields, "name_or_index")?,
            edge_to_node_offset: field(&header.meta.edge_fields, "to_node")?,
            location_field_count: header.meta.location_fields.len(),
            location_object_index_offset,
            location_script_id_offset,
            location_line_offset,
            location_column_offset,
            node_types: type_names(&header.meta.node_types, node_type_offset, "node")?,
            edge_types: type_names(&header.meta.edge_types, edge_type_offset, "edge")?,
            expected_nodes: header.node_count,
            expected_edges: header.edge_count,
        })
    }
}

#[derive(Default)]
struct GraphBuilder {
    metadata: Option<Metadata>,
    strings: Option<Vec<String>>,
    node_type: Vec<u32>,
    node_name: Vec<u32>,
    node_id: Vec<u64>,
    node_shallow_size: Vec<u64>,
    node_edge_count: Vec<u32>,
    edge_type: Vec<u32>,
    edge_name_or_index: Vec<u64>,
    edge_target: Vec<u32>,
    location_node: Vec<u32>,
    location_script_id: Vec<i64>,
    location_line: Vec<u32>,
    location_column: Vec<u32>,
    saw_nodes: bool,
    saw_edges: bool,
    saw_locations: bool,
    error: Option<HeapGraphParseError>,
}

impl GraphBuilder {
    fn finish(self) -> Result<HeapGraph, HeapGraphParseError> {
        let metadata = self
            .metadata
            .ok_or(HeapGraphParseError::MissingSection("snapshot"))?;
        if !self.saw_nodes {
            return Err(HeapGraphParseError::MissingSection("nodes"));
        }
        if !self.saw_edges {
            return Err(HeapGraphParseError::MissingSection("edges"));
        }
        let strings = self
            .strings
            .ok_or(HeapGraphParseError::MissingSection("strings"))?;

        let parsed_nodes =
            u64::try_from(self.node_id.len()).map_err(|_| HeapGraphParseError::Overflow {
                field: "node count",
                value: u64::MAX,
            })?;
        let parsed_edges =
            u64::try_from(self.edge_type.len()).map_err(|_| HeapGraphParseError::Overflow {
                field: "edge count",
                value: u64::MAX,
            })?;
        if let Some(expected) = metadata.expected_nodes {
            if expected != parsed_nodes {
                return Err(HeapGraphParseError::HeaderCountMismatch {
                    kind: "nodes",
                    expected,
                    actual: parsed_nodes,
                });
            }
        }
        if let Some(expected) = metadata.expected_edges {
            if expected != parsed_edges {
                return Err(HeapGraphParseError::HeaderCountMismatch {
                    kind: "edges",
                    expected,
                    actual: parsed_edges,
                });
            }
        }
        let expected_edges = self
            .node_edge_count
            .iter()
            .try_fold(0_u64, |total, &count| {
                total
                    .checked_add(u64::from(count))
                    .ok_or(HeapGraphParseError::Overflow {
                        field: "edge count",
                        value: u64::MAX,
                    })
            })?;
        if expected_edges != parsed_edges {
            return Err(HeapGraphParseError::EdgeCountMismatch {
                expected: expected_edges,
                actual: parsed_edges,
            });
        }
        for &name in &self.node_name {
            if usize::try_from(name).expect("u32 fits usize") >= strings.len() {
                return Err(HeapGraphParseError::InvalidStringIndex {
                    context: "node name",
                    value: u64::from(name),
                    string_count: strings.len(),
                });
            }
        }
        for (edge, &raw_name) in self.edge_name_or_index.iter().enumerate() {
            let edge_type = &metadata.edge_types
                [usize::try_from(self.edge_type[edge]).expect("u32 fits usize")];
            if matches!(edge_type.as_str(), "element" | "hidden") {
                continue;
            }
            let Ok(name) = usize::try_from(raw_name) else {
                return Err(HeapGraphParseError::InvalidStringIndex {
                    context: "edge name",
                    value: raw_name,
                    string_count: strings.len(),
                });
            };
            if name >= strings.len() {
                return Err(HeapGraphParseError::InvalidStringIndex {
                    context: "edge name",
                    value: raw_name,
                    string_count: strings.len(),
                });
            }
        }

        let mut edge_starts = Vec::with_capacity(self.node_edge_count.len() + 1);
        edge_starts.push(0_u32);
        for count in self.node_edge_count {
            let next = edge_starts
                .last()
                .copied()
                .expect("edge starts is nonempty")
                .checked_add(count)
                .ok_or(HeapGraphParseError::Overflow {
                    field: "edge count",
                    value: expected_edges,
                })?;
            edge_starts.push(next);
        }
        Ok(HeapGraph {
            node_types: metadata.node_types,
            edge_types: metadata.edge_types,
            strings,
            node_type: self.node_type,
            node_name: self.node_name,
            node_id: self.node_id,
            node_shallow_size: self.node_shallow_size,
            edge_starts,
            edge_type: self.edge_type,
            edge_name_or_index: self.edge_name_or_index,
            edge_target: self.edge_target,
            location_node: self.location_node,
            location_script_id: self.location_script_id,
            location_line: self.location_line,
            location_column: self.location_column,
            id_index: OnceLock::new(),
            reverse: OnceLock::new(),
            dominators: OnceLock::new(),
        })
    }
}

struct HeapGraphSeed<'a> {
    builder: &'a mut GraphBuilder,
}

impl<'de> DeserializeSeed<'de> for HeapGraphSeed<'_> {
    type Value = ();

    fn deserialize<D>(self, deserializer: D) -> Result<Self::Value, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        deserializer.deserialize_map(HeapGraphVisitor {
            builder: self.builder,
        })
    }
}

struct HeapGraphVisitor<'a> {
    builder: &'a mut GraphBuilder,
}

impl<'de> Visitor<'de> for HeapGraphVisitor<'_> {
    type Value = ();

    fn expecting(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("a V8 heap snapshot object")
    }

    fn visit_map<M>(self, mut map: M) -> Result<Self::Value, M::Error>
    where
        M: MapAccess<'de>,
    {
        while let Some(key) = map.next_key::<String>()? {
            match key.as_str() {
                "snapshot" => {
                    if self.builder.metadata.is_some() {
                        return builder_error(
                            self.builder,
                            HeapGraphParseError::DuplicateSection("snapshot"),
                        );
                    }
                    let header = map.next_value::<SnapshotHeader>()?;
                    match Metadata::try_from(header) {
                        Ok(metadata) => self.builder.metadata = Some(metadata),
                        Err(error) => return builder_error(self.builder, error),
                    }
                }
                "nodes" => {
                    if self.builder.saw_nodes {
                        return builder_error(
                            self.builder,
                            HeapGraphParseError::DuplicateSection("nodes"),
                        );
                    }
                    let Some(metadata) = self.builder.metadata.clone() else {
                        return builder_error(
                            self.builder,
                            HeapGraphParseError::MetadataMustPrecede("nodes"),
                        );
                    };
                    self.builder.saw_nodes = true;
                    map.next_value_seed(NodesSeed {
                        metadata,
                        builder: self.builder,
                    })?;
                }
                "edges" => {
                    if self.builder.saw_edges {
                        return builder_error(
                            self.builder,
                            HeapGraphParseError::DuplicateSection("edges"),
                        );
                    }
                    let Some(metadata) = self.builder.metadata.clone() else {
                        return builder_error(
                            self.builder,
                            HeapGraphParseError::MetadataMustPrecede("edges"),
                        );
                    };
                    if !self.builder.saw_nodes {
                        return builder_error(
                            self.builder,
                            HeapGraphParseError::MetadataMustPrecede("nodes before edges"),
                        );
                    }
                    self.builder.saw_edges = true;
                    map.next_value_seed(EdgesSeed {
                        metadata,
                        builder: self.builder,
                    })?;
                }
                "locations" => {
                    if self.builder.saw_locations {
                        return builder_error(
                            self.builder,
                            HeapGraphParseError::DuplicateSection("locations"),
                        );
                    }
                    let Some(metadata) = self.builder.metadata.clone() else {
                        return builder_error(
                            self.builder,
                            HeapGraphParseError::MetadataMustPrecede("locations"),
                        );
                    };
                    if !self.builder.saw_nodes {
                        return builder_error(
                            self.builder,
                            HeapGraphParseError::MetadataMustPrecede("nodes before locations"),
                        );
                    }
                    self.builder.saw_locations = true;
                    map.next_value_seed(LocationsSeed {
                        metadata,
                        builder: self.builder,
                    })?;
                }
                "strings" => {
                    if self.builder.strings.is_some() {
                        return builder_error(
                            self.builder,
                            HeapGraphParseError::DuplicateSection("strings"),
                        );
                    }
                    let mut strings = Vec::new();
                    map.next_value_seed(StringsSeed {
                        strings: &mut strings,
                        error: &mut self.builder.error,
                    })?;
                    self.builder.strings = Some(strings);
                }
                _ => {
                    map.next_value::<IgnoredAny>()?;
                }
            }
        }
        Ok(())
    }
}

fn builder_error<T, E: serde::de::Error>(
    builder: &mut GraphBuilder,
    error: HeapGraphParseError,
) -> Result<T, E> {
    let message = error.to_string();
    builder.error = Some(error);
    Err(E::custom(message))
}

fn sequence_error<T, E: serde::de::Error>(
    slot: &mut Option<HeapGraphParseError>,
    error: HeapGraphParseError,
) -> Result<T, E> {
    let message = error.to_string();
    *slot = Some(error);
    Err(E::custom(message))
}

struct NodesSeed<'a> {
    metadata: Metadata,
    builder: &'a mut GraphBuilder,
}

impl<'de> DeserializeSeed<'de> for NodesSeed<'_> {
    type Value = ();

    fn deserialize<D>(self, deserializer: D) -> Result<Self::Value, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        deserializer.deserialize_seq(NodesVisitor {
            metadata: self.metadata,
            builder: self.builder,
        })
    }
}

struct NodesVisitor<'a> {
    metadata: Metadata,
    builder: &'a mut GraphBuilder,
}

impl<'de> Visitor<'de> for NodesVisitor<'_> {
    type Value = ();

    fn expecting(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("the flattened heap node array")
    }

    fn visit_seq<A>(self, mut sequence: A) -> Result<Self::Value, A::Error>
    where
        A: SeqAccess<'de>,
    {
        let mut record = vec![0_u64; self.metadata.node_field_count];
        loop {
            for (index, value) in record.iter_mut().enumerate() {
                let Some(next) = sequence.next_element::<u64>()? else {
                    if index == 0 {
                        return Ok(());
                    }
                    return sequence_error(
                        &mut self.builder.error,
                        HeapGraphParseError::InvalidRecordLength("nodes"),
                    );
                };
                *value = next;
            }
            let node_count = self.builder.node_id.len();
            if node_count >= u32::MAX as usize {
                return sequence_error(
                    &mut self.builder.error,
                    HeapGraphParseError::Overflow {
                        field: "node count",
                        value: u64::try_from(node_count + 1).unwrap_or(u64::MAX),
                    },
                );
            }
            let raw_type = record[self.metadata.node_type_offset];
            if raw_type >= u64::try_from(self.metadata.node_types.len()).unwrap_or(u64::MAX) {
                return sequence_error(
                    &mut self.builder.error,
                    HeapGraphParseError::InvalidTypeIndex {
                        field: "node",
                        value: raw_type,
                        type_count: self.metadata.node_types.len(),
                    },
                );
            }
            self.builder.node_type.push(checked_u32(
                raw_type,
                "node type",
                &mut self.builder.error,
            )?);
            self.builder.node_name.push(checked_u32(
                record[self.metadata.node_name_offset],
                "node name",
                &mut self.builder.error,
            )?);
            self.builder
                .node_id
                .push(record[self.metadata.node_id_offset]);
            self.builder
                .node_shallow_size
                .push(record[self.metadata.node_self_size_offset]);
            self.builder.node_edge_count.push(checked_u32(
                record[self.metadata.node_edge_count_offset],
                "node edge count",
                &mut self.builder.error,
            )?);
        }
    }
}

struct EdgesSeed<'a> {
    metadata: Metadata,
    builder: &'a mut GraphBuilder,
}

impl<'de> DeserializeSeed<'de> for EdgesSeed<'_> {
    type Value = ();

    fn deserialize<D>(self, deserializer: D) -> Result<Self::Value, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        deserializer.deserialize_seq(EdgesVisitor {
            metadata: self.metadata,
            builder: self.builder,
        })
    }
}

struct EdgesVisitor<'a> {
    metadata: Metadata,
    builder: &'a mut GraphBuilder,
}

impl<'de> Visitor<'de> for EdgesVisitor<'_> {
    type Value = ();

    fn expecting(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("the flattened heap edge array")
    }

    fn visit_seq<A>(self, mut sequence: A) -> Result<Self::Value, A::Error>
    where
        A: SeqAccess<'de>,
    {
        let mut record = vec![0_u64; self.metadata.edge_field_count];
        loop {
            for (index, value) in record.iter_mut().enumerate() {
                let Some(next) = sequence.next_element::<u64>()? else {
                    if index == 0 {
                        return Ok(());
                    }
                    return sequence_error(
                        &mut self.builder.error,
                        HeapGraphParseError::InvalidRecordLength("edges"),
                    );
                };
                *value = next;
            }
            let edge_count = self.builder.edge_type.len();
            if edge_count >= u32::MAX as usize {
                return sequence_error(
                    &mut self.builder.error,
                    HeapGraphParseError::Overflow {
                        field: "edge count",
                        value: u64::try_from(edge_count + 1).unwrap_or(u64::MAX),
                    },
                );
            }
            let raw_type = record[self.metadata.edge_type_offset];
            if raw_type >= u64::try_from(self.metadata.edge_types.len()).unwrap_or(u64::MAX) {
                return sequence_error(
                    &mut self.builder.error,
                    HeapGraphParseError::InvalidTypeIndex {
                        field: "edge",
                        value: raw_type,
                        type_count: self.metadata.edge_types.len(),
                    },
                );
            }
            let raw_target = record[self.metadata.edge_to_node_offset];
            let field_count =
                u64::try_from(self.metadata.node_field_count).expect("node field count fits u64");
            if raw_target % field_count != 0 {
                return sequence_error(
                    &mut self.builder.error,
                    HeapGraphParseError::InvalidNodeOffset(raw_target),
                );
            }
            let target = raw_target / field_count;
            if target >= u64::try_from(self.builder.node_id.len()).unwrap_or(u64::MAX) {
                return sequence_error(
                    &mut self.builder.error,
                    HeapGraphParseError::InvalidNodeOffset(raw_target),
                );
            }
            self.builder.edge_type.push(checked_u32(
                raw_type,
                "edge type",
                &mut self.builder.error,
            )?);
            self.builder
                .edge_name_or_index
                .push(record[self.metadata.edge_name_or_index_offset]);
            self.builder.edge_target.push(checked_u32(
                target,
                "edge target",
                &mut self.builder.error,
            )?);
        }
    }
}

struct LocationsSeed<'a> {
    metadata: Metadata,
    builder: &'a mut GraphBuilder,
}

impl<'de> DeserializeSeed<'de> for LocationsSeed<'_> {
    type Value = ();

    fn deserialize<D>(self, deserializer: D) -> Result<Self::Value, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        deserializer.deserialize_seq(LocationsVisitor {
            metadata: self.metadata,
            builder: self.builder,
        })
    }
}

struct LocationsVisitor<'a> {
    metadata: Metadata,
    builder: &'a mut GraphBuilder,
}

impl<'de> Visitor<'de> for LocationsVisitor<'_> {
    type Value = ();

    fn expecting(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("the flattened heap location array")
    }

    fn visit_seq<A>(self, mut sequence: A) -> Result<Self::Value, A::Error>
    where
        A: SeqAccess<'de>,
    {
        if self.metadata.location_field_count == 0 {
            if sequence.next_element::<IgnoredAny>()?.is_some() {
                return sequence_error(
                    &mut self.builder.error,
                    HeapGraphParseError::InvalidMetadata(
                        "locations exist but location_fields is empty".to_owned(),
                    ),
                );
            }
            return Ok(());
        }
        let mut record = vec![0_i64; self.metadata.location_field_count];
        loop {
            for (index, value) in record.iter_mut().enumerate() {
                let Some(next) = sequence.next_element::<i64>()? else {
                    if index == 0 {
                        return Ok(());
                    }
                    return sequence_error(
                        &mut self.builder.error,
                        HeapGraphParseError::InvalidRecordLength("locations"),
                    );
                };
                *value = next;
            }
            let raw_node = record[self.metadata.location_object_index_offset];
            let Ok(raw_node) = u64::try_from(raw_node) else {
                return sequence_error(
                    &mut self.builder.error,
                    HeapGraphParseError::SignedOverflow {
                        field: "location object index",
                        value: record[self.metadata.location_object_index_offset],
                    },
                );
            };
            let field_count =
                u64::try_from(self.metadata.node_field_count).expect("node field count fits u64");
            if raw_node % field_count != 0 {
                return sequence_error(
                    &mut self.builder.error,
                    HeapGraphParseError::InvalidNodeOffset(raw_node),
                );
            }
            let node = raw_node / field_count;
            if node >= u64::try_from(self.builder.node_id.len()).unwrap_or(u64::MAX) {
                return sequence_error(
                    &mut self.builder.error,
                    HeapGraphParseError::InvalidNodeOffset(raw_node),
                );
            }
            self.builder.location_node.push(checked_u32(
                node,
                "location node",
                &mut self.builder.error,
            )?);
            self.builder
                .location_script_id
                .push(record[self.metadata.location_script_id_offset]);
            self.builder.location_line.push(checked_signed_u32(
                record[self.metadata.location_line_offset],
                "location line",
                &mut self.builder.error,
            )?);
            self.builder.location_column.push(checked_signed_u32(
                record[self.metadata.location_column_offset],
                "location column",
                &mut self.builder.error,
            )?);
        }
    }
}

struct StringsSeed<'a> {
    strings: &'a mut Vec<String>,
    error: &'a mut Option<HeapGraphParseError>,
}

impl<'de> DeserializeSeed<'de> for StringsSeed<'_> {
    type Value = ();

    fn deserialize<D>(self, deserializer: D) -> Result<Self::Value, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        deserializer.deserialize_seq(StringsVisitor {
            strings: self.strings,
            error: self.error,
        })
    }
}

struct StringsVisitor<'a> {
    strings: &'a mut Vec<String>,
    error: &'a mut Option<HeapGraphParseError>,
}

impl<'de> Visitor<'de> for StringsVisitor<'_> {
    type Value = ();

    fn expecting(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("the heap snapshot string table")
    }

    fn visit_seq<A>(self, mut sequence: A) -> Result<Self::Value, A::Error>
    where
        A: SeqAccess<'de>,
    {
        while let Some(value) = sequence.next_element::<String>()? {
            if self.strings.len() > u32::MAX as usize {
                return sequence_error(
                    self.error,
                    HeapGraphParseError::Overflow {
                        field: "string count",
                        value: u64::try_from(self.strings.len() + 1).unwrap_or(u64::MAX),
                    },
                );
            }
            self.strings.push(value);
        }
        Ok(())
    }
}

fn checked_u32<E: serde::de::Error>(
    value: u64,
    field: &'static str,
    error: &mut Option<HeapGraphParseError>,
) -> Result<u32, E> {
    u32::try_from(value).map_err(|_| {
        let parse_error = HeapGraphParseError::Overflow { field, value };
        let message = parse_error.to_string();
        *error = Some(parse_error);
        E::custom(message)
    })
}

fn checked_signed_u32<E: serde::de::Error>(
    value: i64,
    field: &'static str,
    error: &mut Option<HeapGraphParseError>,
) -> Result<u32, E> {
    u32::try_from(value).map_err(|_| {
        let parse_error = HeapGraphParseError::SignedOverflow { field, value };
        let message = parse_error.to_string();
        *error = Some(parse_error);
        E::custom(message)
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn snapshot(nodes: &str, edges: &str, strings: &str, locations: &str) -> String {
        let node_count = nodes.split(',').count() / 5;
        let edge_count = if edges.trim().is_empty() {
            0
        } else {
            edges.split(',').count() / 3
        };
        format!(
            r#"{{
              "snapshot": {{
                "meta": {{
                  "node_fields": ["type", "name", "id", "self_size", "edge_count"],
                  "node_types": [["synthetic", "object", "string", "concatenated string", "sliced string", "number"], "string", "number", "number", "number"],
                  "edge_fields": ["type", "name_or_index", "to_node"],
                  "edge_types": [["property", "element", "weak", "internal", "hidden"], "string_or_number", "node"],
                  "location_fields": ["object_index", "script_id", "line", "column"]
                }},
                "node_count": {node_count},
                "edge_count": {edge_count}
              }},
              "nodes": [{nodes}],
              "edges": [{edges}],
              "locations": [{locations}],
              "strings": [{strings}]
            }}"#
        )
    }

    fn representative_graph() -> HeapGraph {
        // root -> object -> string; object weakly references concat; concat -> slice
        let json = snapshot(
            "0,0,1,1,1, 1,1,3,10,2, 2,2,5,20,0, 3,3,7,30,1, 4,4,9,40,0",
            "0,5,5, 0,6,10, 2,7,15, 3,8,20",
            r#""root","Object","hello","hello world","world","obj","text","weakText","parent""#,
            "5,11,12,13, 10,12,20,4",
        );
        parse_heap_graph(json.as_bytes()).unwrap()
    }

    #[test]
    fn streams_and_preserves_graph_fields() {
        let graph = representative_graph();
        assert_eq!(graph.node_count(), 5);
        assert_eq!(graph.edge_count(), 4);
        assert_eq!(graph.node_kinds()[3], "concatenated string");
        assert_eq!(graph.edge_kinds()[2], "weak");
        assert_eq!(graph.strings()[2], "hello");
        assert_eq!(graph.node_by_heap_object_id(7), Some(NodeIndex(3)));

        let summary = graph.node_summary(NodeIndex(1)).unwrap();
        assert_eq!(summary.node_type, "object");
        assert_eq!(summary.raw_name_index, 1);
        assert_eq!(summary.raw_name, "Object");
        assert_eq!(summary.shallow_size, 10);
        assert_eq!(summary.outgoing_references, 2);
        assert_eq!(summary.incoming_references, 1);
        assert_eq!(
            graph.node_summary(NodeIndex(3)).unwrap().string_value,
            Some("hello world")
        );

        let outgoing: Vec<_> = graph.outgoing_references(NodeIndex(1)).unwrap().collect();
        assert_eq!(outgoing[0].edge_type, "property");
        assert_eq!(outgoing[0].name, Some("text"));
        assert_eq!(outgoing[0].name_or_index, 6);
        assert_eq!(outgoing[0].target, NodeIndex(2));
        assert_eq!(outgoing[1].edge_type, "weak");
        assert_eq!(outgoing[1].target, NodeIndex(3));

        let incoming: Vec<_> = graph.incoming_references(NodeIndex(3)).unwrap().collect();
        assert_eq!(incoming.len(), 1);
        assert_eq!(incoming[0].source, NodeIndex(1));
        assert_eq!(incoming[0].edge, EdgeIndex(2));

        assert_eq!(
            graph.locations().collect::<Vec<_>>(),
            vec![
                Location {
                    node: NodeIndex(1),
                    script_id: 11,
                    line: 12,
                    column: 13,
                },
                Location {
                    node: NodeIndex(2),
                    script_id: 12,
                    line: 20,
                    column: 4,
                },
            ]
        );
    }

    #[test]
    fn reconstructs_concatenated_and_sliced_string_content() {
        let concatenated = parse_heap_graph(
            snapshot(
                "3,0,1,0,2, 2,1,3,0,0, 2,2,5,0,0",
                "3,3,5, 3,4,10",
                r#""(concatenated string)","hello ","world","first","second""#,
                "",
            )
            .as_bytes(),
        )
        .unwrap();
        assert_eq!(
            concatenated
                .reconstructed_string(NodeIndex(0), None)
                .unwrap(),
            Some(ReconstructedString {
                value: "hello world".to_owned(),
                truncated: false,
                exact_prefix: true,
            })
        );
        assert_eq!(
            concatenated
                .reconstructed_string(NodeIndex(0), Some(7))
                .unwrap(),
            Some(ReconstructedString {
                value: "hello w".to_owned(),
                truncated: true,
                exact_prefix: true,
            })
        );
        assert_eq!(
            concatenated.select(&NodeSelector::new().string_value(TextMatcher::Contains("lo wo"))),
            vec![NodeIndex(0)]
        );

        let sliced = parse_heap_graph(
            snapshot(
                "4,0,1,0,3, 2,1,3,0,0, 5,2,5,0,0, 5,3,7,0,0",
                "3,4,5, 3,5,10, 3,6,15",
                r#""(sliced string)","abcdefghijklmnopqrstuvwxyz","2","5","parent","offset","length""#,
                "",
            )
            .as_bytes(),
        )
        .unwrap();
        assert_eq!(
            sliced.reconstructed_string(NodeIndex(0), None).unwrap(),
            Some(ReconstructedString {
                value: "cdefg".to_owned(),
                truncated: false,
                exact_prefix: true,
            })
        );

        let regular_v8_slice = parse_heap_graph(
            snapshot(
                "4,0,1,0,1, 2,1,3,0,0",
                "3,2,5",
                r#""(sliced string)","backing string","parent""#,
                "",
            )
            .as_bytes(),
        )
        .unwrap();
        assert_eq!(
            regular_v8_slice
                .reconstructed_string(NodeIndex(0), Some(7))
                .unwrap(),
            Some(ReconstructedString {
                value: "backing".to_owned(),
                truncated: true,
                exact_prefix: false,
            })
        );
        assert_eq!(
            regular_v8_slice
                .select(&NodeSelector::new().string_value(TextMatcher::Contains("backing"))),
            vec![NodeIndex(1)]
        );
        assert_eq!(
            regular_v8_slice.aggregate(AggregateBy::StringValue).groups["backing string"].count,
            1
        );
    }

    #[test]
    fn bounds_string_reconstruction_cycles_depth_and_size() {
        let cyclic = parse_heap_graph(
            snapshot(
                "3,0,1,0,2, 2,1,3,0,0",
                "3,2,0, 3,3,5",
                r#""(concatenated string)","tail","first","second""#,
                "",
            )
            .as_bytes(),
        )
        .unwrap();
        assert_eq!(
            cyclic.reconstructed_string(NodeIndex(0), None).unwrap(),
            Some(ReconstructedString {
                value: "tail".to_owned(),
                truncated: true,
                exact_prefix: false,
            })
        );

        let concat_count = MAX_STRING_RECONSTRUCTION_DEPTH + 1;
        let empty_index = concat_count + 1;
        let nodes = (0..concat_count)
            .map(|index| format!("3,0,{},0,2", index * 2 + 1))
            .chain([
                format!("2,1,{},0,0", concat_count * 2 + 1),
                format!("2,2,{},0,0", concat_count * 2 + 3),
            ])
            .collect::<Vec<_>>()
            .join(",");
        let edges = (0..concat_count)
            .flat_map(|index| {
                let first = if index + 1 < concat_count {
                    index + 1
                } else {
                    concat_count
                };
                [
                    format!("3,3,{}", first * 5),
                    format!("3,4,{}", empty_index * 5),
                ]
            })
            .collect::<Vec<_>>()
            .join(",");
        let deep = parse_heap_graph(
            snapshot(
                &nodes,
                &edges,
                r#""(concatenated string)","x","","first","second""#,
                "",
            )
            .as_bytes(),
        )
        .unwrap();
        let deep_value = deep
            .reconstructed_string(NodeIndex(0), None)
            .unwrap()
            .unwrap();
        assert!(deep_value.truncated);

        let oversized_value = "x".repeat(MAX_RECONSTRUCTED_STRING_BYTES + 1);
        let oversized_strings =
            serde_json::to_string(&vec![oversized_value]).expect("serialize string table");
        let oversized = parse_heap_graph(
            snapshot(
                "2,0,1,0,0",
                "",
                &oversized_strings[1..oversized_strings.len() - 1],
                "",
            )
            .as_bytes(),
        )
        .unwrap();
        let bounded = oversized
            .reconstructed_string(NodeIndex(0), None)
            .unwrap()
            .unwrap();
        assert_eq!(bounded.value.len(), MAX_RECONSTRUCTED_STRING_BYTES);
        assert!(bounded.truncated);
    }

    #[test]
    fn selectors_compose_type_text_size_and_limit() {
        let graph = representative_graph();
        let regex = Regex::new("hello|world").unwrap();
        assert_eq!(
            graph.select(
                &NodeSelector::new()
                    .string_value(TextMatcher::Contains("o"))
                    .min_shallow_size(20)
                    .max_shallow_size(35)
            ),
            vec![NodeIndex(2), NodeIndex(3)]
        );
        assert_eq!(
            graph.select(
                &NodeSelector::new()
                    .node_type("concatenated string")
                    .raw_name(TextMatcher::Exact("hello world"))
                    .limit(1)
            ),
            vec![NodeIndex(3)]
        );
        assert!(
            graph
                .select(
                    &NodeSelector::new()
                        .node_type("object")
                        .string_value(TextMatcher::Regex(&regex))
                )
                .is_empty()
        );
        assert!(graph.select(&NodeSelector::new().limit(0)).is_empty());
    }

    #[test]
    fn paths_support_direction_edge_and_cost_policies() {
        let graph = representative_graph();
        assert!(
            graph
                .shortest_path(NodeIndex(0), NodeIndex(4), PathOptions::default(),)
                .unwrap()
                .is_none()
        );

        let all = graph
            .shortest_path(
                NodeIndex(0),
                NodeIndex(4),
                PathOptions {
                    edge_policy: EdgePolicy::All,
                    ..PathOptions::default()
                },
            )
            .unwrap()
            .unwrap();
        assert_eq!(
            all.nodes,
            vec![NodeIndex(0), NodeIndex(1), NodeIndex(3), NodeIndex(4)]
        );
        assert_eq!(all.cost, 3);

        let incoming = graph
            .shortest_path(
                NodeIndex(4),
                NodeIndex(0),
                PathOptions {
                    direction: PathDirection::Incoming,
                    edge_policy: EdgePolicy::All,
                    cost: CostPolicy::Readable,
                },
            )
            .unwrap()
            .unwrap();
        assert_eq!(
            incoming.nodes,
            vec![NodeIndex(4), NodeIndex(3), NodeIndex(1), NodeIndex(0)]
        );
        assert!(
            incoming
                .steps
                .iter()
                .all(|step| step.direction == TraversalDirection::Incoming)
        );
        assert_eq!(incoming.cost, 9); // internal 3 + weak 5 + named property 1

        let either = graph
            .shortest_path(
                NodeIndex(2),
                NodeIndex(4),
                PathOptions {
                    direction: PathDirection::Either,
                    edge_policy: EdgePolicy::All,
                    cost: CostPolicy::Edges,
                },
            )
            .unwrap()
            .unwrap();
        assert_eq!(
            either.nodes,
            vec![NodeIndex(2), NodeIndex(1), NodeIndex(3), NodeIndex(4)]
        );
    }

    #[test]
    fn readable_paths_prefer_named_edges_over_a_short_hidden_path() {
        let json = snapshot(
            "0,0,1,1,2, 1,1,3,10,1, 1,2,5,20,0",
            "4,0,10, 0,3,5, 0,4,10",
            r#""root","Middle","Target","via","next""#,
            "",
        );
        let graph = parse_heap_graph(json.as_bytes()).unwrap();
        let by_edges = graph
            .shortest_path(
                NodeIndex(0),
                NodeIndex(2),
                PathOptions {
                    cost: CostPolicy::Edges,
                    ..PathOptions::default()
                },
            )
            .unwrap()
            .unwrap();
        assert_eq!(by_edges.nodes, vec![NodeIndex(0), NodeIndex(2)]);
        assert_eq!(by_edges.cost, 1);

        let readable = graph
            .shortest_path(
                NodeIndex(0),
                NodeIndex(2),
                PathOptions {
                    cost: CostPolicy::Readable,
                    ..PathOptions::default()
                },
            )
            .unwrap()
            .unwrap();
        assert_eq!(
            readable.nodes,
            vec![NodeIndex(0), NodeIndex(1), NodeIndex(2)]
        );
        assert_eq!(readable.cost, 2);
    }

    #[test]
    fn computes_strong_dominators_and_retained_sizes() {
        // A diamond rooted at 0, then 1 and 2 converge at 3. Node 4 is only
        // weakly reachable and is excluded from strong dominator analysis.
        let json = snapshot(
            "0,0,1,1,2, 1,1,3,10,1, 1,1,5,20,1, 1,1,7,30,1, 1,1,9,40,0",
            "0,5,5, 0,6,10, 0,7,15, 0,8,15, 2,9,20",
            r#""root","Object","","","","a","b","c","d","weak""#,
            "",
        );
        let graph = parse_heap_graph(json.as_bytes()).unwrap();
        let dominators = graph.dominators().unwrap();
        assert_eq!(
            dominators.immediate_dominator(NodeIndex(1)),
            Some(NodeIndex(0))
        );
        assert_eq!(
            dominators.immediate_dominator(NodeIndex(2)),
            Some(NodeIndex(0))
        );
        assert_eq!(
            dominators.immediate_dominator(NodeIndex(3)),
            Some(NodeIndex(0))
        );
        assert_eq!(dominators.retained_size(NodeIndex(1)), Some(10));
        assert_eq!(dominators.retained_size(NodeIndex(3)), Some(30));
        assert_eq!(dominators.retained_size(NodeIndex(0)), Some(61));
        assert!(!dominators.is_reachable(NodeIndex(4)));
        assert_eq!(dominators.retained_size(NodeIndex(4)), None);
    }

    #[test]
    fn dominators_handle_cycles_and_nested_retention() {
        // 0 -> 1 -> 2 -> 1 is a cycle. Both 0 and 2 reach 3, so 0
        // dominates 3, while 3 alone dominates 4.
        let json = snapshot(
            "0,0,1,1,2, 1,1,3,10,1, 1,1,5,20,2, 1,1,7,30,1, 1,1,9,40,0",
            "0,5,5, 0,6,15, 0,7,10, 0,8,5, 0,9,15, 0,9,20",
            r#""root","Object","","","","a","b","c","d","next""#,
            "",
        );
        let graph = parse_heap_graph(json.as_bytes()).unwrap();
        let dominators = graph.dominators().unwrap();
        assert_eq!(
            dominators.immediate_dominator(NodeIndex(1)),
            Some(NodeIndex(0))
        );
        assert_eq!(
            dominators.immediate_dominator(NodeIndex(2)),
            Some(NodeIndex(1))
        );
        assert_eq!(
            dominators.immediate_dominator(NodeIndex(3)),
            Some(NodeIndex(0))
        );
        assert_eq!(
            dominators.immediate_dominator(NodeIndex(4)),
            Some(NodeIndex(3))
        );
        assert_eq!(dominators.retained_size(NodeIndex(1)), Some(30));
        assert_eq!(dominators.retained_size(NodeIndex(3)), Some(70));
        assert_eq!(dominators.retained_size(NodeIndex(0)), Some(101));
    }

    #[test]
    fn aggregates_and_diffs_all_supported_dimensions() {
        let older = representative_graph();
        let newer_json = snapshot(
            "0,0,1,1,0, 2,2,5,25,0, 4,4,9,50,0",
            "",
            r#""root","Object","hello","hello world","world""#,
            "",
        );
        let newer = parse_heap_graph(newer_json.as_bytes()).unwrap();

        let strings = older.aggregate(AggregateBy::StringValue);
        assert_eq!(
            strings.groups["hello world"],
            AggregateValue {
                count: 1,
                shallow_size: 30,
            }
        );
        assert_eq!(strings.groups.len(), 3);

        let diff = older.diff(&newer, AggregateBy::StringValue);
        assert_eq!(
            diff.groups["hello"],
            AggregateDelta {
                count: 0,
                shallow_size: 5,
            }
        );
        assert_eq!(
            diff.groups["hello world"],
            AggregateDelta {
                count: -1,
                shallow_size: -30,
            }
        );
        assert_eq!(
            diff.groups["world"],
            AggregateDelta {
                count: 0,
                shallow_size: 10,
            }
        );

        assert_eq!(
            older.aggregate(AggregateBy::NodeType).groups["string"].count,
            1
        );
        assert_eq!(
            older.aggregate(AggregateBy::RawName).groups["Object"].count,
            1
        );
    }

    #[test]
    fn reports_typed_format_and_overflow_errors() {
        let overflow = snapshot("0,4294967296,1,0,0", "", r#""root""#, "");
        assert!(matches!(
            parse_heap_graph(overflow.as_bytes()),
            Err(HeapGraphParseError::Overflow {
                field: "node name",
                ..
            })
        ));

        let bad_target = snapshot("0,0,1,0,1", "0,0,1", r#""root""#, "");
        assert!(matches!(
            parse_heap_graph(bad_target.as_bytes()),
            Err(HeapGraphParseError::InvalidNodeOffset(1))
        ));

        let bad_count = snapshot("0,0,1,0,1", "", r#""root""#, "");
        assert!(matches!(
            parse_heap_graph(bad_count.as_bytes()),
            Err(HeapGraphParseError::EdgeCountMismatch {
                expected: 1,
                actual: 0
            })
        ));
    }
}
