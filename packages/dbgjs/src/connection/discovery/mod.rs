//! Discovery: what is known, what is only partially known, and who still wants to know.
//!
//! Two independent primitives live here.
//!
//! [`DiscoveryState`] makes ignorance explicit. A resource whose children were never enumerated is
//! not "a resource without children" - it is [`DiscoveryState::Unobserved`], and a traversal that
//! crosses it reports a gap instead of a confident empty answer.
//!
//! [`Lease`] makes demand explicit and reference counted. Discovery is expensive (OS scans, CDP
//! auto-attach, blocked child processes), so a provider should run it exactly while someone holds
//! a lease. The last drop releases the key, which is reported to an optional [`LeaseObserver`].

pub mod process_discovery;
pub mod process_tree_source;

use std::collections::BTreeMap;
use std::sync::{Arc, Mutex};

use serde::{Deserialize, Serialize};

use crate::debugger::resource_graph::{RelationKind, ResourceId};

/// How well one relation of one resource is currently known.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(
    tag = "state",
    rename_all = "camelCase",
    rename_all_fields = "camelCase"
)]
pub enum DiscoveryState {
    /// Nobody has looked yet. Neighbors may exist and are simply unknown.
    #[default]
    Unobserved,
    /// A one-shot enumeration succeeded and returned everything that existed at that moment.
    SnapshotComplete,
    /// A one-shot enumeration returned only part of the truth, e.g. a permission denied subtree.
    SnapshotPartial { reason: String },
    /// The provider is subscribed and keeps the neighborhood up to date.
    Live,
    /// Previously observed, but the observation is no longer maintained and may be outdated.
    Stale { reason: String },
    /// Observation was attempted and failed. Neighbors remain unknown.
    Failed { error: String },
}

impl DiscoveryState {
    pub fn partial(reason: impl Into<String>) -> Self {
        Self::SnapshotPartial {
            reason: reason.into(),
        }
    }

    pub fn stale(reason: impl Into<String>) -> Self {
        Self::Stale {
            reason: reason.into(),
        }
    }

    pub fn failed(error: impl Into<String>) -> Self {
        Self::Failed {
            error: error.into(),
        }
    }

    /// Whether the neighborhood can be treated as fully known.
    pub fn is_complete(&self) -> bool {
        matches!(self, Self::SnapshotComplete | Self::Live)
    }

    /// Whether anyone ever looked, successfully or not.
    pub fn is_observed(&self) -> bool {
        !matches!(self, Self::Unobserved)
    }

    pub fn is_live(&self) -> bool {
        matches!(self, Self::Live)
    }

    /// How much the state is worth when several sources disagree. Higher wins.
    fn rank(&self) -> u8 {
        match self {
            Self::Unobserved => 0,
            Self::Failed { .. } => 1,
            Self::Stale { .. } => 2,
            Self::SnapshotPartial { .. } => 3,
            Self::SnapshotComplete => 4,
            Self::Live => 5,
        }
    }

    /// Merges two observations of the same frontier, keeping the most complete one. Ties keep the
    /// receiver, which makes merging order independent for equally ranked states.
    pub fn merge(self, other: Self) -> Self {
        if other.rank() > self.rank() {
            other
        } else {
            self
        }
    }
}

/// One source's statement about a single relation frontier of a single resource.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct FrontierDeclaration {
    pub relation: RelationKind,
    pub state: DiscoveryState,
}

impl FrontierDeclaration {
    pub fn new(relation: RelationKind, state: DiscoveryState) -> Self {
        Self { relation, state }
    }
}

/// What a discovery lease keeps alive: expansion of one relation from one resource, or - when
/// `relation` is `None` - everything reachable from it.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct DiscoveryKey {
    pub resource: ResourceId,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub relation: Option<RelationKind>,
}

impl DiscoveryKey {
    pub fn new(resource: ResourceId, relation: Option<RelationKind>) -> Self {
        Self { resource, relation }
    }

    pub fn all(resource: ResourceId) -> Self {
        Self {
            resource,
            relation: None,
        }
    }
}

/// What a pause lease keeps armed: blocking children of one resource.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct PauseChildrenKey(pub ResourceId);

/// Notified whenever the demand for a key changes, including the 0 -> 1 and 1 -> 0 transitions
/// that a provider turns into "start observing" and "stop observing".
pub trait LeaseObserver<K>: Send + Sync {
    fn demand_changed(&self, key: &K, demand: usize);
}

struct LeaseState<K> {
    counts: Mutex<BTreeMap<K, usize>>,
    observer: Option<Arc<dyn LeaseObserver<K>>>,
}

/// Hands out reference counted leases for keys of type `K`.
pub struct LeaseRegistry<K>
where
    K: Ord + Clone + Send + Sync + 'static,
{
    state: Arc<LeaseState<K>>,
}

impl<K> Clone for LeaseRegistry<K>
where
    K: Ord + Clone + Send + Sync + 'static,
{
    fn clone(&self) -> Self {
        Self {
            state: self.state.clone(),
        }
    }
}

impl<K> Default for LeaseRegistry<K>
where
    K: Ord + Clone + Send + Sync + 'static,
{
    fn default() -> Self {
        Self::new()
    }
}

impl<K> LeaseRegistry<K>
where
    K: Ord + Clone + Send + Sync + 'static,
{
    pub fn new() -> Self {
        Self {
            state: Arc::new(LeaseState {
                counts: Mutex::new(BTreeMap::new()),
                observer: None,
            }),
        }
    }

    pub fn with_observer(observer: Arc<dyn LeaseObserver<K>>) -> Self {
        Self {
            state: Arc::new(LeaseState {
                counts: Mutex::new(BTreeMap::new()),
                observer: Some(observer),
            }),
        }
    }

    /// Takes one reference on `key`. Discovery for the key must run while the lease is alive.
    pub fn acquire(&self, key: K) -> Lease<K> {
        let demand = {
            let mut counts = self.state.counts.lock().unwrap();
            let entry = counts.entry(key.clone()).or_insert(0);
            *entry += 1;
            *entry
        };
        self.notify(&key, demand);
        Lease {
            key,
            state: self.state.clone(),
        }
    }

    pub fn demand(&self, key: &K) -> usize {
        self.state
            .counts
            .lock()
            .unwrap()
            .get(key)
            .copied()
            .unwrap_or(0)
    }

    /// Every key that currently has at least one live lease.
    pub fn active(&self) -> Vec<K> {
        self.state.counts.lock().unwrap().keys().cloned().collect()
    }

    pub fn is_idle(&self) -> bool {
        self.state.counts.lock().unwrap().is_empty()
    }

    fn notify(&self, key: &K, demand: usize) {
        if let Some(observer) = &self.state.observer {
            observer.demand_changed(key, demand);
        }
    }
}

/// A live reference on one key. Cloning takes another reference; dropping releases exactly one.
pub struct Lease<K>
where
    K: Ord + Clone + Send + Sync + 'static,
{
    key: K,
    state: Arc<LeaseState<K>>,
}

impl<K> Lease<K>
where
    K: Ord + Clone + Send + Sync + 'static,
{
    pub fn key(&self) -> &K {
        &self.key
    }

    /// The demand for this lease's key, including this lease.
    pub fn demand(&self) -> usize {
        self.state
            .counts
            .lock()
            .unwrap()
            .get(&self.key)
            .copied()
            .unwrap_or(0)
    }
}

impl<K> Clone for Lease<K>
where
    K: Ord + Clone + Send + Sync + 'static,
{
    fn clone(&self) -> Self {
        let demand = {
            let mut counts = self.state.counts.lock().unwrap();
            let entry = counts.entry(self.key.clone()).or_insert(0);
            *entry += 1;
            *entry
        };
        if let Some(observer) = &self.state.observer {
            observer.demand_changed(&self.key, demand);
        }
        Self {
            key: self.key.clone(),
            state: self.state.clone(),
        }
    }
}

impl<K> Drop for Lease<K>
where
    K: Ord + Clone + Send + Sync + 'static,
{
    fn drop(&mut self) {
        let demand = {
            let mut counts = self.state.counts.lock().unwrap();
            match counts.get_mut(&self.key) {
                None => return,
                Some(count) => {
                    *count -= 1;
                    let remaining = *count;
                    if remaining == 0 {
                        counts.remove(&self.key);
                    }
                    remaining
                }
            }
        };
        if let Some(observer) = &self.state.observer {
            observer.demand_changed(&self.key, demand);
        }
    }
}

impl<K> std::fmt::Debug for Lease<K>
where
    K: Ord + Clone + Send + Sync + std::fmt::Debug + 'static,
{
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Lease").field("key", &self.key).finish()
    }
}

pub type DiscoveryLeaseRegistry = LeaseRegistry<DiscoveryKey>;
pub type DiscoveryLease = Lease<DiscoveryKey>;
pub type PauseChildrenLeaseRegistry = LeaseRegistry<PauseChildrenKey>;
pub type PauseChildrenLease = Lease<PauseChildrenKey>;

#[cfg(test)]
mod tests {
    use super::*;

    fn resource(key: &str) -> ResourceId {
        ResourceId::from_parts("process", [key]).unwrap()
    }

    #[derive(Default)]
    struct RecordingObserver {
        events: Mutex<Vec<(DiscoveryKey, usize)>>,
    }

    impl LeaseObserver<DiscoveryKey> for RecordingObserver {
        fn demand_changed(&self, key: &DiscoveryKey, demand: usize) {
            self.events.lock().unwrap().push((key.clone(), demand));
        }
    }

    #[test]
    fn merge_keeps_the_most_complete_observation() {
        assert_eq!(
            DiscoveryState::Unobserved.merge(DiscoveryState::failed("boom")),
            DiscoveryState::failed("boom")
        );
        assert_eq!(
            DiscoveryState::partial("denied").merge(DiscoveryState::SnapshotComplete),
            DiscoveryState::SnapshotComplete
        );
        assert_eq!(
            DiscoveryState::Live.merge(DiscoveryState::stale("disconnected")),
            DiscoveryState::Live
        );
        assert_eq!(
            DiscoveryState::SnapshotComplete.merge(DiscoveryState::Live),
            DiscoveryState::Live
        );
    }

    #[test]
    fn completeness_is_explicit() {
        assert!(!DiscoveryState::Unobserved.is_observed());
        assert!(!DiscoveryState::Unobserved.is_complete());
        assert!(DiscoveryState::SnapshotComplete.is_complete());
        assert!(DiscoveryState::Live.is_complete());
        assert!(!DiscoveryState::partial("x").is_complete());
        assert!(DiscoveryState::partial("x").is_observed());
        assert!(!DiscoveryState::stale("x").is_complete());
        assert!(!DiscoveryState::failed("x").is_complete());
    }

    #[test]
    fn states_round_trip_as_tagged_json() {
        let states = [
            DiscoveryState::Unobserved,
            DiscoveryState::SnapshotComplete,
            DiscoveryState::partial("denied"),
            DiscoveryState::Live,
            DiscoveryState::stale("disconnected"),
            DiscoveryState::failed("timeout"),
        ];
        for state in states {
            let text = serde_json::to_string(&state).unwrap();
            assert_eq!(
                serde_json::from_str::<DiscoveryState>(&text).unwrap(),
                state,
                "round trip for {text}"
            );
        }
        assert_eq!(
            serde_json::to_string(&DiscoveryState::partial("denied")).unwrap(),
            r#"{"state":"snapshotPartial","reason":"denied"}"#
        );
    }

    #[test]
    fn leases_are_reference_counted() {
        let registry = DiscoveryLeaseRegistry::new();
        let key = DiscoveryKey::new(resource("1"), Some(RelationKind::Contains));

        assert_eq!(registry.demand(&key), 0);
        let first = registry.acquire(key.clone());
        let second = registry.acquire(key.clone());
        assert_eq!(registry.demand(&key), 2);
        assert_eq!(first.demand(), 2);
        assert_eq!(registry.active(), vec![key.clone()]);

        drop(second);
        assert_eq!(registry.demand(&key), 1);
        drop(first);
        assert_eq!(registry.demand(&key), 0);
        assert!(registry.is_idle(), "released keys are forgotten");
    }

    #[test]
    fn cloning_a_lease_takes_another_reference() {
        let registry = DiscoveryLeaseRegistry::new();
        let key = DiscoveryKey::all(resource("1"));
        let lease = registry.acquire(key.clone());
        let copy = lease.clone();
        assert_eq!(registry.demand(&key), 2);
        drop(lease);
        assert_eq!(registry.demand(&key), 1);
        drop(copy);
        assert_eq!(registry.demand(&key), 0);
    }

    #[test]
    fn keys_are_independent_and_observed() {
        let observer = Arc::new(RecordingObserver::default());
        let registry = DiscoveryLeaseRegistry::with_observer(observer.clone());
        let contains = DiscoveryKey::new(resource("1"), Some(RelationKind::Contains));
        let spawned = DiscoveryKey::new(resource("1"), Some(RelationKind::Spawned));

        let first = registry.acquire(contains.clone());
        let second = registry.acquire(spawned.clone());
        assert_eq!(registry.demand(&contains), 1);
        assert_eq!(registry.demand(&spawned), 1);
        drop(first);
        assert_eq!(registry.demand(&contains), 0);
        assert_eq!(registry.demand(&spawned), 1);
        drop(second);

        let events = observer.events.lock().unwrap().clone();
        assert_eq!(
            events,
            vec![
                (contains.clone(), 1),
                (spawned.clone(), 1),
                (contains, 0),
                (spawned, 0),
            ]
        );
    }
}
