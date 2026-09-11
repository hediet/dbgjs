//! The faceted resource graph.
//!
//! Everything dbgjs can observe - an OS process, a browser, a page, a frame, a debug session -
//! is one *resource* identified by an opaque [`ResourceId`]. A resource is not owned by a single
//! provider: every provider contributes a *facet* (descriptive facts, callable capabilities and
//! discovery frontier states) under its own [`SourceId`], and the graph presents the union of all
//! facets. Retracting a source removes exactly that source's contribution and nothing else.
//!
//! Three rules keep the model honest:
//!
//! 1. [`ResourceKind`] and the attribute bag are *descriptive only*. Nothing may branch on them;
//!    behavior lives exclusively in capabilities.
//! 2. A capability is a callable object (see [`crate::capability`]), never a boolean. The
//!    serializable snapshot carries a [`CapabilityHandleId`] plus a summary that is *derived* from
//!    the registered object on demand, so no parallel descriptor model can drift from reality.
//! 3. What is *not* yet known is explicit: [`DiscoveryState`] per resource and relation, so a
//!    traversal can report incompleteness instead of silently returning a truncated answer.

use std::collections::{BTreeMap, BTreeSet};
use std::fmt;
use std::sync::{Arc, Mutex};

use serde::{Deserialize, Serialize};
use serde_json::Value;
use thiserror::Error;
use tokio::sync::watch;

use crate::capability::{
    CapabilityError, CapabilityHandleId, CapabilityKind, CapabilityObject, CapabilityRegistry,
    CapabilitySummary,
};
use crate::discovery::{DiscoveryState, FrontierDeclaration};

/// The canonical, opaque identity of a resource.
///
/// Ids are built from a namespace and key parts that every provider observing the same physical
/// resource can agree on, so two sources contributing the same process produce the same id. The
/// textual form is canonical (one spelling per identity) but consumers must treat it as opaque:
/// nothing outside this module may parse it to recover kind or structure.
#[derive(Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(try_from = "String", into = "String")]
pub struct ResourceId(Arc<str>);

impl ResourceId {
    /// Builds the canonical id for `namespace` and `parts`. The encoding is injective: distinct
    /// part sequences never collide.
    pub fn from_parts<I, S>(namespace: &str, parts: I) -> Result<Self, ResourceIdError>
    where
        I: IntoIterator<Item = S>,
        S: AsRef<str>,
    {
        if namespace.is_empty() {
            return Err(ResourceIdError::EmptySegment);
        }
        let mut text = encode_segment(namespace);
        for part in parts {
            let part = part.as_ref();
            if part.is_empty() {
                return Err(ResourceIdError::EmptySegment);
            }
            text.push('/');
            text.push_str(&encode_segment(part));
        }
        Ok(Self(Arc::from(text.as_str())))
    }

    /// Parses a previously rendered id, rejecting any non-canonical spelling.
    pub fn parse(text: &str) -> Result<Self, ResourceIdError> {
        if text.is_empty() {
            return Err(ResourceIdError::EmptySegment);
        }
        for segment in text.split('/') {
            if segment.is_empty() {
                return Err(ResourceIdError::EmptySegment);
            }
            let decoded = decode_segment(segment).ok_or(ResourceIdError::NotCanonical)?;
            if encode_segment(&decoded) != segment {
                return Err(ResourceIdError::NotCanonical);
            }
        }
        Ok(Self(Arc::from(text)))
    }

    /// The canonical text. Intended for transport, logging and map keys - never for parsing.
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for ResourceId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl fmt::Debug for ResourceId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "ResourceId({})", self.0)
    }
}

impl From<ResourceId> for String {
    fn from(value: ResourceId) -> Self {
        value.0.to_string()
    }
}

impl TryFrom<String> for ResourceId {
    type Error = ResourceIdError;

    fn try_from(value: String) -> Result<Self, Self::Error> {
        Self::parse(&value)
    }
}

#[derive(Clone, Copy, Debug, Error, PartialEq, Eq)]
pub enum ResourceIdError {
    #[error("resource id segments must not be empty")]
    EmptySegment,
    #[error("resource id is not in canonical form")]
    NotCanonical,
}

fn encode_segment(segment: &str) -> String {
    let mut out = String::with_capacity(segment.len());
    for byte in segment.as_bytes() {
        if is_unreserved(*byte) {
            out.push(*byte as char);
        } else {
            out.push('%');
            out.push(hex_digit(byte >> 4));
            out.push(hex_digit(byte & 0x0f));
        }
    }
    out
}

fn decode_segment(segment: &str) -> Option<String> {
    let bytes = segment.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut index = 0;
    while index < bytes.len() {
        match bytes[index] {
            b'%' => {
                if index + 2 >= bytes.len() {
                    return None;
                }
                let high = hex_value(bytes[index + 1])?;
                let low = hex_value(bytes[index + 2])?;
                out.push(high * 16 + low);
                index += 3;
            }
            byte => {
                out.push(byte);
                index += 1;
            }
        }
    }
    String::from_utf8(out).ok()
}

fn is_unreserved(byte: u8) -> bool {
    byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.' | b'~' | b':')
}

fn hex_digit(value: u8) -> char {
    match value {
        0..=9 => (b'0' + value) as char,
        _ => (b'A' + value - 10) as char,
    }
}

fn hex_value(byte: u8) -> Option<u8> {
    match byte {
        b'0'..=b'9' => Some(byte - b'0'),
        b'A'..=b'F' => Some(byte - b'A' + 10),
        _ => None,
    }
}

/// Identifies the provider that contributed a facet. Provenance is per source, not per resource:
/// the same resource can be described by several sources at once.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct SourceId(Arc<str>);

impl SourceId {
    pub fn new(text: impl AsRef<str>) -> Self {
        Self(Arc::from(text.as_ref()))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for SourceId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

/// A purely descriptive classification, e.g. `process`, `browser`, `page`.
///
/// Kinds exist for humans and for coarse filtering. No behavior may depend on them; ask for a
/// capability instead.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct ResourceKind(Arc<str>);

impl ResourceKind {
    pub fn new(text: impl AsRef<str>) -> Self {
        Self(Arc::from(text.as_ref()))
    }

    pub fn process() -> Self {
        Self::new("process")
    }

    pub fn browser() -> Self {
        Self::new("browser")
    }

    pub fn page() -> Self {
        Self::new("page")
    }

    pub fn frame() -> Self {
        Self::new("frame")
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for ResourceKind {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

/// The typed vocabulary of edges. Relations are directed from the subject to the object.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum RelationKind {
    /// Structural containment: browser contains page, page contains frame.
    Contains,
    /// The subject process created the object process.
    Spawned,
    /// The subject runtime hosts the object execution environment.
    Hosts,
    /// The subject debug session observes the object.
    Debugs,
    /// A provider specific relation. Traversal treats it exactly like the typed variants.
    Other(String),
}

impl RelationKind {
    pub fn other(text: impl Into<String>) -> Self {
        Self::Other(text.into())
    }
}

/// One directed edge. Edges carry no payload; the endpoints and the kind are the whole fact.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Relation {
    pub kind: RelationKind,
    pub from: ResourceId,
    pub to: ResourceId,
}

impl Relation {
    pub fn new(kind: RelationKind, from: ResourceId, to: ResourceId) -> Self {
        Self { kind, from, to }
    }
}

/// The descriptive part of one source's facet.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ResourceFacts {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub kind: Option<ResourceKind>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub label: Option<String>,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub attributes: BTreeMap<String, Value>,
}

impl ResourceFacts {
    pub fn of_kind(kind: ResourceKind) -> Self {
        Self {
            kind: Some(kind),
            ..Self::default()
        }
    }

    pub fn with_label(mut self, label: impl Into<String>) -> Self {
        self.label = Some(label.into());
        self
    }

    pub fn with_attribute(mut self, key: impl Into<String>, value: Value) -> Self {
        self.attributes.insert(key.into(), value);
        self
    }
}

/// A source's complete statement about one resource. Applying it replaces that source's previous
/// facet in full, including its capabilities and frontier declarations.
#[derive(Clone)]
pub struct ResourceUpsert {
    pub id: ResourceId,
    pub facts: ResourceFacts,
    pub capabilities: Vec<CapabilityObject>,
    pub frontiers: Vec<FrontierDeclaration>,
}

impl ResourceUpsert {
    pub fn new(id: ResourceId, facts: ResourceFacts) -> Self {
        Self {
            id,
            facts,
            capabilities: Vec::new(),
            frontiers: Vec::new(),
        }
    }

    pub fn with_capability(mut self, capability: CapabilityObject) -> Self {
        self.capabilities.push(capability);
        self
    }

    pub fn with_frontier(mut self, relation: RelationKind, state: DiscoveryState) -> Self {
        self.frontiers.push(FrontierDeclaration { relation, state });
        self
    }
}

impl fmt::Debug for ResourceUpsert {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("ResourceUpsert")
            .field("id", &self.id)
            .field("facts", &self.facts)
            .field("capabilities", &self.capabilities)
            .field("frontiers", &self.frontiers)
            .finish()
    }
}

/// An atomic contribution from exactly one source. Either every part of the delta is applied or
/// none of it is - a rejected delta never leaves half-registered capabilities behind.
#[derive(Clone, Debug)]
pub struct GraphDelta {
    pub source: SourceId,
    pub upserts: Vec<ResourceUpsert>,
    pub retracted_resources: Vec<ResourceId>,
    pub added_relations: Vec<Relation>,
    pub retracted_relations: Vec<Relation>,
}

impl GraphDelta {
    pub fn for_source(source: SourceId) -> Self {
        Self {
            source,
            upserts: Vec::new(),
            retracted_resources: Vec::new(),
            added_relations: Vec::new(),
            retracted_relations: Vec::new(),
        }
    }

    pub fn upsert(mut self, upsert: ResourceUpsert) -> Self {
        self.upserts.push(upsert);
        self
    }

    pub fn retract_resource(mut self, id: ResourceId) -> Self {
        self.retracted_resources.push(id);
        self
    }

    pub fn relate(mut self, kind: RelationKind, from: ResourceId, to: ResourceId) -> Self {
        self.added_relations.push(Relation::new(kind, from, to));
        self
    }

    pub fn unrelate(mut self, kind: RelationKind, from: ResourceId, to: ResourceId) -> Self {
        self.retracted_relations.push(Relation::new(kind, from, to));
        self
    }

    pub fn is_empty(&self) -> bool {
        self.upserts.is_empty()
            && self.retracted_resources.is_empty()
            && self.added_relations.is_empty()
            && self.retracted_relations.is_empty()
    }
}

#[derive(Debug, Error, PartialEq)]
pub enum GraphError {
    #[error("resource {0} is upserted more than once in one delta")]
    DuplicateUpsert(ResourceId),
    #[error("resource {0} is both upserted and retracted in one delta")]
    ConflictingUpsert(ResourceId),
    #[error("relation endpoint {0} does not exist after the delta")]
    UnknownRelationEndpoint(ResourceId),
    #[error("relation from {0} to itself is not allowed")]
    SelfRelation(ResourceId),
    #[error("capability rejected: {0}")]
    Capability(#[from] CapabilityError),
}

/// The revision of the graph. It increases exactly when an applied mutation changed something.
#[derive(
    Clone, Copy, Debug, Default, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize,
)]
#[serde(transparent)]
pub struct GraphRevision(pub u64);

#[derive(Clone, Debug)]
struct Facet {
    facts: ResourceFacts,
    capabilities: BTreeMap<CapabilityKind, CapabilityHandleId>,
    frontiers: BTreeMap<RelationKind, DiscoveryState>,
}

#[derive(Clone, Debug, Default)]
struct ResourceEntry {
    facets: BTreeMap<SourceId, Facet>,
}

/// The union of every source's facets, plus the runtime registry that keeps capability objects
/// callable while the serializable snapshot only carries handles.
#[derive(Clone)]
pub struct ResourceGraph {
    revision: GraphRevision,
    resources: BTreeMap<ResourceId, ResourceEntry>,
    relations: BTreeMap<Relation, BTreeSet<SourceId>>,
    outgoing: BTreeMap<ResourceId, BTreeSet<(RelationKind, ResourceId)>>,
    incoming: BTreeMap<ResourceId, BTreeSet<(RelationKind, ResourceId)>>,
    capabilities: CapabilityRegistry,
}

impl Default for ResourceGraph {
    fn default() -> Self {
        Self::new()
    }
}

impl ResourceGraph {
    pub fn new() -> Self {
        Self {
            revision: GraphRevision(0),
            resources: BTreeMap::new(),
            relations: BTreeMap::new(),
            outgoing: BTreeMap::new(),
            incoming: BTreeMap::new(),
            capabilities: CapabilityRegistry::new(),
        }
    }

    pub fn revision(&self) -> GraphRevision {
        self.revision
    }

    pub fn len(&self) -> usize {
        self.resources.len()
    }

    pub fn is_empty(&self) -> bool {
        self.resources.is_empty()
    }

    pub fn contains(&self, id: &ResourceId) -> bool {
        self.resources.contains_key(id)
    }

    pub fn ids(&self) -> impl Iterator<Item = &ResourceId> {
        self.resources.keys()
    }

    /// The runtime registry. Callers resolve a [`CapabilityHandleId`] from a snapshot back into a
    /// callable object through it.
    pub fn capability_registry(&self) -> &CapabilityRegistry {
        &self.capabilities
    }

    /// Validates the whole delta, then applies it. On error nothing is mutated and the revision is
    /// unchanged.
    pub fn apply(&mut self, delta: GraphDelta) -> Result<GraphRevision, GraphError> {
        self.validate(&delta)?;

        // Register first: this is the only step that allocates handles, so staging it lets us undo
        // the allocation if anything unexpected happens before the graph itself is touched.
        let mut staged: Vec<(usize, CapabilityKind, CapabilityHandleId)> = Vec::new();
        for (index, upsert) in delta.upserts.iter().enumerate() {
            for capability in &upsert.capabilities {
                match self.capabilities.register(capability.clone()) {
                    Ok(handle) => staged.push((index, capability.kind(), handle)),
                    Err(error) => {
                        for (_, _, handle) in staged {
                            self.capabilities.remove(handle);
                        }
                        return Err(error.into());
                    }
                }
            }
        }

        let mut changed = false;
        for id in &delta.retracted_resources {
            changed |= self.remove_facet(id, &delta.source);
            changed |= self.remove_source_relations_for_resource(id, &delta.source);
        }

        for (index, upsert) in delta.upserts.iter().enumerate() {
            let mut capabilities = BTreeMap::new();
            for (staged_index, kind, handle) in &staged {
                if *staged_index == index {
                    if let Some(replaced) = capabilities.insert(kind.clone(), *handle) {
                        self.capabilities.remove(replaced);
                    }
                }
            }
            let frontiers = upsert
                .frontiers
                .iter()
                .map(|declaration| (declaration.relation.clone(), declaration.state.clone()))
                .collect::<BTreeMap<_, _>>();
            let facet = Facet {
                facts: upsert.facts.clone(),
                capabilities,
                frontiers,
            };
            let entry = self.resources.entry(upsert.id.clone()).or_default();
            match entry.facets.insert(delta.source.clone(), facet) {
                Some(previous) => {
                    let replaced = previous.capabilities.values().copied().collect::<Vec<_>>();
                    for handle in replaced {
                        self.capabilities.remove(handle);
                    }
                    changed = true;
                }
                None => changed = true,
            }
        }

        for relation in &delta.retracted_relations {
            changed |= self.remove_relation(relation, &delta.source);
        }
        for relation in &delta.added_relations {
            let sources = self.relations.entry(relation.clone()).or_default();
            changed |= sources.insert(delta.source.clone());
        }

        changed |= self.prune_dangling_relations();
        if changed {
            self.rebuild_indexes();
            self.revision = GraphRevision(self.revision.0 + 1);
        }
        Ok(self.revision)
    }

    /// Removes every contribution of `source` atomically.
    pub fn retract_source(&mut self, source: &SourceId) -> GraphRevision {
        let ids = self.resources.keys().cloned().collect::<Vec<_>>();
        let mut changed = false;
        for id in ids {
            changed |= self.remove_facet(&id, source);
        }
        let relations = self.relations.keys().cloned().collect::<Vec<_>>();
        for relation in relations {
            changed |= self.remove_relation(&relation, source);
        }
        changed |= self.prune_dangling_relations();
        if changed {
            self.rebuild_indexes();
            self.revision = GraphRevision(self.revision.0 + 1);
        }
        self.revision
    }

    fn validate(&self, delta: &GraphDelta) -> Result<(), GraphError> {
        let mut seen = BTreeSet::new();
        for upsert in &delta.upserts {
            if !seen.insert(upsert.id.clone()) {
                return Err(GraphError::DuplicateUpsert(upsert.id.clone()));
            }
            for capability in &upsert.capabilities {
                capability.validate()?;
            }
        }
        let retracted = delta
            .retracted_resources
            .iter()
            .cloned()
            .collect::<BTreeSet<_>>();
        for id in &retracted {
            if seen.contains(id) {
                return Err(GraphError::ConflictingUpsert(id.clone()));
            }
        }

        let exists_after = |id: &ResourceId| -> bool {
            if seen.contains(id) {
                return true;
            }
            match self.resources.get(id) {
                None => false,
                Some(entry) => {
                    if !retracted.contains(id) {
                        return true;
                    }
                    entry.facets.keys().any(|source| *source != delta.source)
                }
            }
        };
        for relation in &delta.added_relations {
            if relation.from == relation.to {
                return Err(GraphError::SelfRelation(relation.from.clone()));
            }
            if !exists_after(&relation.from) {
                return Err(GraphError::UnknownRelationEndpoint(relation.from.clone()));
            }
            if !exists_after(&relation.to) {
                return Err(GraphError::UnknownRelationEndpoint(relation.to.clone()));
            }
        }
        Ok(())
    }

    fn remove_facet(&mut self, id: &ResourceId, source: &SourceId) -> bool {
        let Some(entry) = self.resources.get_mut(id) else {
            return false;
        };
        let Some(facet) = entry.facets.remove(source) else {
            return false;
        };
        for handle in facet.capabilities.values() {
            self.capabilities.remove(*handle);
        }
        if entry.facets.is_empty() {
            self.resources.remove(id);
        }
        true
    }

    fn remove_relation(&mut self, relation: &Relation, source: &SourceId) -> bool {
        let Some(sources) = self.relations.get_mut(relation) else {
            return false;
        };
        if !sources.remove(source) {
            return false;
        }
        if sources.is_empty() {
            self.relations.remove(relation);
        }
        true
    }

    fn remove_source_relations_for_resource(
        &mut self,
        resource: &ResourceId,
        source: &SourceId,
    ) -> bool {
        let relations = self
            .relations
            .keys()
            .filter(|relation| &relation.from == resource || &relation.to == resource)
            .cloned()
            .collect::<Vec<_>>();
        let mut changed = false;
        for relation in relations {
            changed |= self.remove_relation(&relation, source);
        }
        changed
    }

    /// A relation whose endpoint is gone is not a fact anymore; the graph never exposes dangling
    /// edges.
    fn prune_dangling_relations(&mut self) -> bool {
        let dangling = self
            .relations
            .keys()
            .filter(|relation| {
                !self.resources.contains_key(&relation.from)
                    || !self.resources.contains_key(&relation.to)
            })
            .cloned()
            .collect::<Vec<_>>();
        if dangling.is_empty() {
            return false;
        }
        for relation in dangling {
            self.relations.remove(&relation);
        }
        true
    }

    fn rebuild_indexes(&mut self) {
        self.outgoing.clear();
        self.incoming.clear();
        for relation in self.relations.keys() {
            self.outgoing
                .entry(relation.from.clone())
                .or_default()
                .insert((relation.kind.clone(), relation.to.clone()));
            self.incoming
                .entry(relation.to.clone())
                .or_default()
                .insert((relation.kind.clone(), relation.from.clone()));
        }
    }

    /// Every kind any source ascribed to the resource.
    pub fn kinds(&self, id: &ResourceId) -> BTreeSet<ResourceKind> {
        self.resources
            .get(id)
            .into_iter()
            .flat_map(|entry| entry.facets.values())
            .filter_map(|facet| facet.facts.kind.clone())
            .collect()
    }

    pub fn has_kind(&self, id: &ResourceId, kind: &ResourceKind) -> bool {
        self.resources
            .get(id)
            .into_iter()
            .flat_map(|entry| entry.facets.values())
            .any(|facet| facet.facts.kind.as_ref() == Some(kind))
    }

    pub fn contributors(&self, id: &ResourceId) -> BTreeSet<SourceId> {
        self.resources
            .get(id)
            .map(|entry| entry.facets.keys().cloned().collect())
            .unwrap_or_default()
    }

    /// The first label in source order, so the merged view is order independent.
    pub fn label(&self, id: &ResourceId) -> Option<String> {
        self.resources
            .get(id)?
            .facets
            .values()
            .find_map(|facet| facet.facts.label.clone())
    }

    /// Attribute union. On conflict the lowest [`SourceId`] wins, which keeps the merge
    /// deterministic and independent of contribution order.
    pub fn attributes(&self, id: &ResourceId) -> BTreeMap<String, Value> {
        let mut merged = BTreeMap::new();
        let Some(entry) = self.resources.get(id) else {
            return merged;
        };
        for facet in entry.facets.values() {
            for (key, value) in &facet.facts.attributes {
                merged.entry(key.clone()).or_insert_with(|| value.clone());
            }
        }
        merged
    }

    pub fn facets(&self, id: &ResourceId) -> BTreeMap<SourceId, ResourceFacts> {
        self.resources
            .get(id)
            .map(|entry| {
                entry
                    .facets
                    .iter()
                    .map(|(source, facet)| (source.clone(), facet.facts.clone()))
                    .collect()
            })
            .unwrap_or_default()
    }

    pub fn capability_kinds(&self, id: &ResourceId) -> BTreeSet<CapabilityKind> {
        self.resources
            .get(id)
            .into_iter()
            .flat_map(|entry| entry.facets.values())
            .flat_map(|facet| facet.capabilities.keys().cloned())
            .collect()
    }

    pub fn has_capability(&self, id: &ResourceId, kind: &CapabilityKind) -> bool {
        self.resources
            .get(id)
            .into_iter()
            .flat_map(|entry| entry.facets.values())
            .any(|facet| facet.capabilities.contains_key(kind))
    }

    pub fn capability_handles(
        &self,
        id: &ResourceId,
        kind: &CapabilityKind,
    ) -> Vec<CapabilityHandleId> {
        self.resources
            .get(id)
            .into_iter()
            .flat_map(|entry| entry.facets.values())
            .filter_map(|facet| facet.capabilities.get(kind).copied())
            .collect()
    }

    /// The callable object contributed for `kind`, preferring the lowest [`SourceId`].
    pub fn capability(&self, id: &ResourceId, kind: &CapabilityKind) -> Option<CapabilityObject> {
        self.capability_handles(id, kind)
            .into_iter()
            .find_map(|handle| self.capabilities.get(handle))
    }

    pub fn capability_from_source(
        &self,
        id: &ResourceId,
        source: &SourceId,
        kind: &CapabilityKind,
    ) -> Option<CapabilityObject> {
        let handle = self
            .resources
            .get(id)?
            .facets
            .get(source)?
            .capabilities
            .get(kind)?;
        self.capabilities.get(*handle)
    }

    /// Descriptors derived from the registered objects at call time. There is no stored copy that
    /// could disagree with the object.
    pub fn capability_descriptors(&self, id: &ResourceId) -> Vec<ResourceCapability> {
        let Some(entry) = self.resources.get(id) else {
            return Vec::new();
        };
        let mut descriptors = Vec::new();
        for (source, facet) in &entry.facets {
            for (kind, handle) in &facet.capabilities {
                let Some(summary) = self.capabilities.summary(*handle) else {
                    continue;
                };
                descriptors.push(ResourceCapability {
                    source: source.clone(),
                    handle: *handle,
                    kind: kind.clone(),
                    summary,
                });
            }
        }
        descriptors
    }

    /// The most complete state any source reports for expanding `relation` from `id`.
    pub fn frontier(&self, id: &ResourceId, relation: &RelationKind) -> DiscoveryState {
        self.resources
            .get(id)
            .into_iter()
            .flat_map(|entry| entry.facets.values())
            .filter_map(|facet| facet.frontiers.get(relation).cloned())
            .fold(DiscoveryState::Unobserved, DiscoveryState::merge)
    }

    /// Every relation for which some source declared a frontier, merged across sources.
    pub fn frontiers(&self, id: &ResourceId) -> BTreeMap<RelationKind, DiscoveryState> {
        let mut merged: BTreeMap<RelationKind, DiscoveryState> = BTreeMap::new();
        let Some(entry) = self.resources.get(id) else {
            return merged;
        };
        for facet in entry.facets.values() {
            for (relation, state) in &facet.frontiers {
                let combined = match merged.remove(relation) {
                    Some(existing) => existing.merge(state.clone()),
                    None => state.clone(),
                };
                merged.insert(relation.clone(), combined);
            }
        }
        merged
    }

    pub fn neighbors(
        &self,
        id: &ResourceId,
        direction: EdgeDirection,
    ) -> BTreeSet<(RelationKind, ResourceId)> {
        let index = match direction {
            EdgeDirection::Outgoing => &self.outgoing,
            EdgeDirection::Incoming => &self.incoming,
        };
        index.get(id).cloned().unwrap_or_default()
    }

    pub fn relations(&self) -> impl Iterator<Item = (&Relation, &BTreeSet<SourceId>)> {
        self.relations.iter()
    }

    pub fn relation_sources(&self, relation: &Relation) -> BTreeSet<SourceId> {
        self.relations.get(relation).cloned().unwrap_or_default()
    }

    /// A serializable projection. Capability objects stay in the registry; the snapshot carries
    /// their handles plus summaries derived from those same objects.
    pub fn snapshot(&self) -> GraphSnapshot {
        let resources = self
            .resources
            .keys()
            .map(|id| {
                let snapshot = ResourceSnapshot {
                    kinds: self.kinds(id),
                    label: self.label(id),
                    attributes: self.attributes(id),
                    contributors: self.contributors(id),
                    facets: self.facets(id),
                    capabilities: self.capability_descriptors(id),
                    frontiers: self
                        .frontiers(id)
                        .into_iter()
                        .map(|(relation, state)| FrontierSnapshot { relation, state })
                        .collect(),
                };
                (id.clone(), snapshot)
            })
            .collect();
        let relations = self
            .relations
            .iter()
            .map(|(relation, sources)| RelationSnapshot {
                relation: relation.clone(),
                contributors: sources.clone(),
            })
            .collect();
        GraphSnapshot {
            revision: self.revision,
            resources,
            relations,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum EdgeDirection {
    Outgoing,
    Incoming,
}

/// A capability as seen from a resource: which source contributed it, how to call it back
/// ([`CapabilityHandleId`]) and the summary derived from the registered object.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ResourceCapability {
    pub source: SourceId,
    pub handle: CapabilityHandleId,
    pub kind: CapabilityKind,
    pub summary: CapabilitySummary,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct FrontierSnapshot {
    pub relation: RelationKind,
    pub state: DiscoveryState,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RelationSnapshot {
    #[serde(flatten)]
    pub relation: Relation,
    pub contributors: BTreeSet<SourceId>,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ResourceSnapshot {
    pub kinds: BTreeSet<ResourceKind>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub label: Option<String>,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub attributes: BTreeMap<String, Value>,
    pub contributors: BTreeSet<SourceId>,
    pub facets: BTreeMap<SourceId, ResourceFacts>,
    pub capabilities: Vec<ResourceCapability>,
    pub frontiers: Vec<FrontierSnapshot>,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct GraphSnapshot {
    pub revision: GraphRevision,
    pub resources: BTreeMap<ResourceId, ResourceSnapshot>,
    pub relations: Vec<RelationSnapshot>,
}

/// What a provider needs in order to contribute: an object-safe, thread-safe entry point that
/// applies deltas atomically and retracts everything it contributed on shutdown.
pub trait GraphSink: Send + Sync {
    fn apply(&self, delta: GraphDelta) -> Result<GraphRevision, GraphError>;
    fn retract_source(&self, source: &SourceId) -> GraphRevision;
    fn revision(&self) -> GraphRevision;
    fn snapshot(&self) -> GraphSnapshot;
}

/// An atomic graph snapshot and a signal for every later revision. The watch channel retains the
/// latest revision, so a slow consumer can resnapshot instead of silently applying incomplete
/// deltas.
pub struct GraphObservation {
    pub snapshot: GraphSnapshot,
    pub revisions: watch::Receiver<GraphRevision>,
}

struct SharedResourceGraphState {
    graph: ResourceGraph,
    revisions: watch::Sender<GraphRevision>,
}

/// The shared graph handed to providers. Cloning shares one graph and its revision stream.
#[derive(Clone)]
pub struct SharedResourceGraph {
    inner: Arc<Mutex<SharedResourceGraphState>>,
}

impl Default for SharedResourceGraph {
    fn default() -> Self {
        Self::new()
    }
}

impl SharedResourceGraph {
    pub fn new() -> Self {
        let graph = ResourceGraph::new();
        let (revisions, _) = watch::channel(graph.revision());
        Self {
            inner: Arc::new(Mutex::new(SharedResourceGraphState { graph, revisions })),
        }
    }

    /// Reads the graph under the lock, e.g. to resolve a [`crate::scope::Scope`] or to fetch a
    /// callable capability object.
    pub fn read<R>(&self, reader: impl FnOnce(&ResourceGraph) -> R) -> R {
        let state = self.inner.lock().unwrap();
        reader(&state.graph)
    }

    /// Atomically captures the current graph and subscribes to every later revision.
    pub fn observe(&self) -> GraphObservation {
        let state = self.inner.lock().unwrap();
        GraphObservation {
            snapshot: state.graph.snapshot(),
            revisions: state.revisions.subscribe(),
        }
    }

    /// Stages a multi-step graph update on a clone and publishes it as one observable revision.
    pub fn try_update<R, E>(
        &self,
        update: impl FnOnce(&mut ResourceGraph) -> Result<R, E>,
    ) -> Result<R, E> {
        let mut state = self.inner.lock().unwrap();
        let previous = state.graph.revision();
        let mut graph = state.graph.clone();
        let result = update(&mut graph)?;
        let revision = graph.revision();
        state.graph = graph;
        if revision != previous {
            state.revisions.send_replace(revision);
        }
        Ok(result)
    }
}

impl GraphSink for SharedResourceGraph {
    fn apply(&self, delta: GraphDelta) -> Result<GraphRevision, GraphError> {
        let mut state = self.inner.lock().unwrap();
        let previous = state.graph.revision();
        let revision = state.graph.apply(delta)?;
        if revision != previous {
            state.revisions.send_replace(revision);
        }
        Ok(revision)
    }

    fn retract_source(&self, source: &SourceId) -> GraphRevision {
        let mut state = self.inner.lock().unwrap();
        let previous = state.graph.revision();
        let revision = state.graph.retract_source(source);
        if revision != previous {
            state.revisions.send_replace(revision);
        }
        revision
    }

    fn revision(&self) -> GraphRevision {
        self.inner.lock().unwrap().graph.revision()
    }

    fn snapshot(&self) -> GraphSnapshot {
        self.inner.lock().unwrap().graph.snapshot()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::capability::testing::{FakeExplore, FakeProcess};
    use serde_json::json;

    fn id(namespace: &str, key: &str) -> ResourceId {
        ResourceId::from_parts(namespace, [key]).expect("canonical id")
    }

    #[test]
    fn ids_are_canonical_and_injective() {
        let a = ResourceId::from_parts("process", ["pid/1"]).unwrap();
        let b = ResourceId::from_parts("process", ["pid", "1"]).unwrap();
        assert_ne!(a, b);
        assert_eq!(ResourceId::parse(a.as_str()).unwrap(), a);
        assert_eq!(
            ResourceId::from_parts("process", ["1234"])
                .unwrap()
                .as_str(),
            "process/1234"
        );
        assert_eq!(
            ResourceId::parse("process/%2f"),
            Err(ResourceIdError::NotCanonical)
        );
        assert_eq!(
            ResourceId::parse("process/%41"),
            Err(ResourceIdError::NotCanonical)
        );
        assert_eq!(
            ResourceId::parse("process//1"),
            Err(ResourceIdError::EmptySegment)
        );
    }

    #[test]
    fn contributions_from_several_sources_are_unioned() {
        let mut graph = ResourceGraph::new();
        let os = SourceId::new("os-scan");
        let cdp = SourceId::new("cdp");
        let process = id("process", "1234");

        graph
            .apply(
                GraphDelta::for_source(os.clone()).upsert(ResourceUpsert::new(
                    process.clone(),
                    ResourceFacts::of_kind(ResourceKind::process())
                        .with_label("node")
                        .with_attribute("pid", json!(1234)),
                )),
            )
            .unwrap();
        graph
            .apply(
                GraphDelta::for_source(cdp.clone()).upsert(ResourceUpsert::new(
                    process.clone(),
                    ResourceFacts::of_kind(ResourceKind::browser())
                        .with_attribute("product", json!("Chrome")),
                )),
            )
            .unwrap();

        assert_eq!(
            graph.kinds(&process),
            BTreeSet::from([ResourceKind::process(), ResourceKind::browser()])
        );
        assert_eq!(
            graph.contributors(&process),
            BTreeSet::from([os.clone(), cdp.clone()])
        );
        assert_eq!(graph.attributes(&process).len(), 2);
        assert_eq!(graph.label(&process).as_deref(), Some("node"));
    }

    #[test]
    fn retracting_one_source_keeps_the_other_facet() {
        let mut graph = ResourceGraph::new();
        let os = SourceId::new("os-scan");
        let cdp = SourceId::new("cdp");
        let process = id("process", "1234");
        let page = id("page", "A");

        graph
            .apply(
                GraphDelta::for_source(os.clone())
                    .upsert(ResourceUpsert::new(
                        process.clone(),
                        ResourceFacts::of_kind(ResourceKind::process()),
                    ))
                    .upsert(ResourceUpsert::new(
                        page.clone(),
                        ResourceFacts::of_kind(ResourceKind::page()),
                    ))
                    .relate(RelationKind::Contains, process.clone(), page.clone()),
            )
            .unwrap();
        graph
            .apply(
                GraphDelta::for_source(cdp.clone()).upsert(ResourceUpsert::new(
                    process.clone(),
                    ResourceFacts::of_kind(ResourceKind::browser()),
                )),
            )
            .unwrap();

        graph.retract_source(&os);

        assert!(graph.contains(&process));
        assert!(!graph.contains(&page), "page had only the retracted source");
        assert_eq!(graph.contributors(&process), BTreeSet::from([cdp]));
        assert_eq!(
            graph.relations().count(),
            0,
            "relations to removed resources are pruned"
        );
    }

    #[test]
    fn relations_survive_until_the_last_contributor_retracts() {
        let mut graph = ResourceGraph::new();
        let a = SourceId::new("a");
        let b = SourceId::new("b");
        let parent = id("process", "1");
        let child = id("process", "2");

        for source in [&a, &b] {
            graph
                .apply(
                    GraphDelta::for_source(source.clone())
                        .upsert(ResourceUpsert::new(
                            parent.clone(),
                            ResourceFacts::of_kind(ResourceKind::process()),
                        ))
                        .upsert(ResourceUpsert::new(
                            child.clone(),
                            ResourceFacts::of_kind(ResourceKind::process()),
                        ))
                        .relate(RelationKind::Spawned, parent.clone(), child.clone()),
                )
                .unwrap();
        }
        let relation = Relation::new(RelationKind::Spawned, parent.clone(), child.clone());
        assert_eq!(graph.relation_sources(&relation).len(), 2);

        graph.retract_source(&a);
        assert_eq!(
            graph.relation_sources(&relation),
            BTreeSet::from([b.clone()])
        );

        graph.retract_source(&b);
        assert!(graph.is_empty());
        assert_eq!(graph.relations().count(), 0);
    }

    #[test]
    fn rejected_delta_changes_nothing() {
        let mut graph = ResourceGraph::new();
        let source = SourceId::new("s");
        let known = id("process", "1");
        graph
            .apply(
                GraphDelta::for_source(source.clone()).upsert(ResourceUpsert::new(
                    known.clone(),
                    ResourceFacts::of_kind(ResourceKind::process()),
                )),
            )
            .unwrap();
        let before = graph.revision();

        let unknown = id("process", "9");
        let error = graph
            .apply(
                GraphDelta::for_source(source.clone())
                    .upsert(
                        ResourceUpsert::new(
                            id("process", "2"),
                            ResourceFacts::of_kind(ResourceKind::process()),
                        )
                        .with_capability(CapabilityObject::Process(Arc::new(FakeProcess::new(2)))),
                    )
                    .relate(RelationKind::Spawned, known.clone(), unknown.clone()),
            )
            .unwrap_err();

        assert!(matches!(error, GraphError::UnknownRelationEndpoint(_)));
        assert_eq!(graph.revision(), before);
        assert_eq!(graph.len(), 1);
        assert_eq!(
            graph.capability_registry().len(),
            0,
            "staged capability registration is rolled back"
        );
    }

    #[test]
    fn upsert_replaces_the_previous_facet_of_the_same_source() {
        let mut graph = ResourceGraph::new();
        let source = SourceId::new("s");
        let process = id("process", "1");

        graph
            .apply(
                GraphDelta::for_source(source.clone()).upsert(
                    ResourceUpsert::new(
                        process.clone(),
                        ResourceFacts::of_kind(ResourceKind::process()),
                    )
                    .with_capability(CapabilityObject::Process(Arc::new(FakeProcess::new(1))))
                    .with_capability(CapabilityObject::Explore(Arc::new(FakeExplore::new()))),
                ),
            )
            .unwrap();
        assert_eq!(graph.capability_registry().len(), 2);

        graph
            .apply(
                GraphDelta::for_source(source.clone()).upsert(
                    ResourceUpsert::new(
                        process.clone(),
                        ResourceFacts::of_kind(ResourceKind::process()),
                    )
                    .with_capability(CapabilityObject::Process(Arc::new(FakeProcess::new(1)))),
                ),
            )
            .unwrap();

        assert_eq!(
            graph.capability_kinds(&process),
            BTreeSet::from([CapabilityKind::Process])
        );
        assert_eq!(
            graph.capability_registry().len(),
            1,
            "the dropped capability is unregistered"
        );
    }

    #[test]
    fn summaries_are_derived_from_the_registered_object() {
        let mut graph = ResourceGraph::new();
        let source = SourceId::new("s");
        let process = id("process", "1");
        let object = Arc::new(FakeProcess::new(7));
        graph
            .apply(
                GraphDelta::for_source(source).upsert(
                    ResourceUpsert::new(
                        process.clone(),
                        ResourceFacts::of_kind(ResourceKind::process()),
                    )
                    .with_capability(CapabilityObject::Process(object.clone())),
                ),
            )
            .unwrap();

        let before = graph.snapshot().resources[&process].capabilities[0]
            .summary
            .clone();
        assert_eq!(before.title, "process 7");

        object.rename("renamed");
        let after = graph.snapshot().resources[&process].capabilities[0]
            .summary
            .clone();
        assert_eq!(after.title, "renamed");
        assert_ne!(before, after, "the descriptor follows the live object");
    }

    #[test]
    fn snapshot_round_trips_as_json() {
        let mut graph = ResourceGraph::new();
        let source = SourceId::new("s");
        let process = id("process", "1");
        let page = id("page", "A");
        graph
            .apply(
                GraphDelta::for_source(source)
                    .upsert(
                        ResourceUpsert::new(
                            process.clone(),
                            ResourceFacts::of_kind(ResourceKind::process())
                                .with_attribute("pid", json!(1)),
                        )
                        .with_capability(CapabilityObject::Process(Arc::new(FakeProcess::new(1))))
                        .with_frontier(RelationKind::Contains, DiscoveryState::Live),
                    )
                    .upsert(ResourceUpsert::new(
                        page.clone(),
                        ResourceFacts::of_kind(ResourceKind::page()),
                    ))
                    .relate(RelationKind::Contains, process.clone(), page.clone()),
            )
            .unwrap();

        let snapshot = graph.snapshot();
        let text = serde_json::to_string(&snapshot).unwrap();
        let parsed: GraphSnapshot = serde_json::from_str(&text).unwrap();
        assert_eq!(parsed, snapshot);
        assert_eq!(parsed.resources[&process].capabilities.len(), 1);
        assert_eq!(parsed.relations.len(), 1);
    }

    #[test]
    fn shared_graph_is_an_object_safe_sink() {
        let shared = SharedResourceGraph::new();
        let sink: Arc<dyn GraphSink> = Arc::new(shared.clone());
        let source = SourceId::new("s");
        let process = id("process", "1");
        sink.apply(
            GraphDelta::for_source(source.clone()).upsert(ResourceUpsert::new(
                process.clone(),
                ResourceFacts::of_kind(ResourceKind::process()),
            )),
        )
        .unwrap();
        assert!(shared.read(|graph| graph.contains(&process)));
        sink.retract_source(&source);
        assert!(shared.read(|graph| graph.is_empty()));
    }

    #[tokio::test]
    async fn shared_graph_observation_starts_at_its_atomic_snapshot_revision() {
        let shared = SharedResourceGraph::new();
        let source = SourceId::new("s");
        let process = id("process", "1");
        let mut observation = shared.observe();
        assert_eq!(
            observation.snapshot.revision,
            *observation.revisions.borrow()
        );

        shared
            .apply(
                GraphDelta::for_source(source.clone()).upsert(ResourceUpsert::new(
                    process.clone(),
                    ResourceFacts::of_kind(ResourceKind::process()),
                )),
            )
            .unwrap();
        observation.revisions.changed().await.unwrap();
        assert_eq!(*observation.revisions.borrow_and_update(), GraphRevision(1));
        assert!(shared.snapshot().resources.contains_key(&process));

        shared.retract_source(&source);
        observation.revisions.changed().await.unwrap();
        assert_eq!(*observation.revisions.borrow_and_update(), GraphRevision(2));
        assert!(shared.snapshot().resources.is_empty());
    }

    #[tokio::test]
    async fn failed_staged_update_does_not_publish_partial_graph_state() {
        let shared = SharedResourceGraph::new();
        let source = SourceId::new("s");
        let process = id("process", "1");
        let mut observation = shared.observe();

        let result: Result<(), &'static str> = shared.try_update(|graph| {
            graph
                .apply(GraphDelta::for_source(source).upsert(ResourceUpsert::new(
                    process,
                    ResourceFacts::of_kind(ResourceKind::process()),
                )))
                .unwrap();
            Err("reject staged update")
        });

        assert_eq!(result, Err("reject staged update"));
        assert!(shared.snapshot().resources.is_empty());
        assert_eq!(shared.revision(), GraphRevision(0));
        assert!(
            tokio::time::timeout(
                std::time::Duration::from_millis(10),
                observation.revisions.changed(),
            )
            .await
            .is_err()
        );
    }
}
