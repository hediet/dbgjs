//! Capabilities: the only place where behavior lives.
//!
//! A capability is never a flag. "This process can be debugged" is expressed by registering a
//! [`DebugCapability`] object whose [`DebugCapability::open`] actually opens the session; there is
//! no `can_debug: bool` anywhere. Consequently a resource that advertises a capability is a
//! resource on which that call can be attempted, and code that wants behavior asks for the object
//! instead of pattern matching on a descriptive kind.
//!
//! Capability objects are not serializable, but the resource graph snapshot must be. The
//! [`CapabilityRegistry`] bridges the two: registering an object yields an opaque
//! [`CapabilityHandleId`] that snapshots carry, and every human readable descriptor is *derived*
//! from the registered object through [`Capability::summary`] whenever it is needed. There is
//! deliberately no separately mutable descriptor record that could drift from the object.

use std::collections::BTreeMap;
use std::fmt;

use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use thiserror::Error;

use crate::debugger::cdp_runtime::CdpDebuggerSession;
use crate::connection::discovery::{DiscoveryLease, PauseChildrenLease};
use crate::debugger::resource_graph::ResourceId;

/// The closed vocabulary of capability kinds. Each variant corresponds to exactly one trait and
/// one [`CapabilityObject`] variant.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum CapabilityKind {
    Explore,
    Debug,
    Process,
    Browser,
    Frame,
    PauseFutureChildren,
}

/// A runtime handle into the [`CapabilityRegistry`]. Handles are never reused within one registry,
/// so a stale handle resolves to `None` rather than to a different capability.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct CapabilityHandleId(pub u64);

/// The derived, presentable form of a capability. Produced by the object itself; never stored
/// alongside it.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CapabilitySummary {
    pub title: String,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub detail: BTreeMap<String, Value>,
}

impl CapabilitySummary {
    pub fn new(title: impl Into<String>) -> Self {
        Self {
            title: title.into(),
            detail: BTreeMap::new(),
        }
    }

    pub fn with_detail(mut self, key: impl Into<String>, value: Value) -> Self {
        self.detail.insert(key.into(), value);
        self
    }
}

#[derive(Clone, Debug, Error, PartialEq, Eq)]
pub enum CapabilityError {
    #[error("capability object reports kind {actual:?} but was registered as {expected:?}")]
    KindMismatch {
        expected: CapabilityKind,
        actual: CapabilityKind,
    },
    #[error("capability handle {0:?} is not registered")]
    UnknownHandle(CapabilityHandleId),
    /// The underlying resource is gone; the capability can never succeed again.
    #[error("capability is no longer available: {0}")]
    Unavailable(String),
    /// The request was understood but refused, e.g. a debugger already owns the target.
    #[error("capability request rejected: {0}")]
    Rejected(String),
    #[error("capability failed: {0}")]
    Failed(String),
}

/// The common part of every capability. Both methods read the object's own live state, which is
/// what makes derived descriptors trustworthy.
pub trait Capability: Send + Sync + 'static {
    fn kind(&self) -> CapabilityKind;
    fn summary(&self) -> CapabilitySummary;
}

/// How much of a resource's neighborhood an exploration should reveal, and for how long.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum ExploreMode {
    /// Contribute one consistent snapshot, then stop.
    #[default]
    Snapshot,
    /// Keep contributing until the lease is dropped.
    Live,
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ExploreRequest {
    pub mode: ExploreMode,
    /// How many relation hops to reveal. `None` means "as far as this provider naturally sees".
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_depth: Option<u32>,
}

impl ExploreRequest {
    pub fn snapshot() -> Self {
        Self::default()
    }

    pub fn live() -> Self {
        Self {
            mode: ExploreMode::Live,
            max_depth: None,
        }
    }

    pub fn with_max_depth(mut self, depth: u32) -> Self {
        self.max_depth = Some(depth);
        self
    }
}

/// Discovers the resources reachable from the owning resource and contributes them to the graph.
#[async_trait]
pub trait ExploreCapability: Capability {
    /// Starts discovery. The returned lease is reference counted: discovery may stop once the last
    /// lease for the same key is dropped, so callers must hold it for as long as they need the
    /// frontier to stay fresh.
    async fn start(&self, request: ExploreRequest) -> Result<DiscoveryLease, CapabilityError>;
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct DebugOpenRequest {
    /// Block the target at its first statement until the session is ready.
    pub pause_on_start: bool,
    /// Take the target away from a debugger that already owns it.
    pub steal_existing_owner: bool,
}

/// What a successful [`DebugCapability::open`] yields. The session id is provider defined and only
/// meaningful to the provider that produced it.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct DebugSessionHandle {
    pub session_id: String,
    pub resource: ResourceId,
    /// Whether a foreign debugger had to be evicted.
    pub stole_existing_owner: bool,
}

/// Opens a debug session on the owning resource.
#[async_trait]
pub trait DebugCapability: Capability {
    async fn open(&self, request: DebugOpenRequest) -> Result<DebugSessionHandle, CapabilityError>;

    /// Transfers the opened protocol session to its consumer. A handle can be taken exactly once.
    fn take_session(
        &self,
        handle: &DebugSessionHandle,
    ) -> Result<CdpDebuggerSession, CapabilityError>;

    /// Releases an opened or already-consumed session and its provider-specific ownership.
    async fn close(&self, handle: &DebugSessionHandle) -> Result<(), CapabilityError>;

    /// Whether the runtime behind this capability is genuinely blocked waiting to be resumed.
    fn waiting_for_debugger(&self) -> bool {
        false
    }
}

/// An OS process that dbgjs can inspect and control.
#[async_trait]
pub trait ProcessCapability: Capability {
    fn pid(&self) -> u32;
    async fn command_line(&self) -> Result<Vec<String>, CapabilityError>;
    async fn kill(&self) -> Result<(), CapabilityError>;
}

/// A browser-shaped host that owns pages.
#[async_trait]
pub trait BrowserCapability: Capability {
    async fn product(&self) -> Result<String, CapabilityError>;
    async fn pages(&self) -> Result<Vec<ResourceId>, CapabilityError>;
    async fn open_page(&self, url: &str) -> Result<ResourceId, CapabilityError>;
}

/// One frame of a document tree.
#[async_trait]
pub trait FrameCapability: Capability {
    fn frame_id(&self) -> String;
    async fn url(&self) -> Result<String, CapabilityError>;
    async fn evaluate(&self, expression: &str) -> Result<Value, CapabilityError>;
}

/// Blocks children created from now on so a debugger can attach before they run.
#[async_trait]
pub trait PauseFutureChildrenCapability: Capability {
    /// Arms pausing and returns a reference counted lease. Pausing stays armed while any lease is
    /// alive and is disarmed when the last one is dropped.
    async fn arm(&self) -> Result<PauseChildrenLease, CapabilityError>;

    /// Releases one target that is currently blocked waiting for a debugger.
    async fn resume(&self, resource: &ResourceId) -> Result<(), CapabilityError>;
}

/// The registrable form of a capability: one variant per trait, each holding the callable object.
#[derive(Clone)]
pub enum CapabilityObject {
    Explore(std::sync::Arc<dyn ExploreCapability>),
    Debug(std::sync::Arc<dyn DebugCapability>),
    Process(std::sync::Arc<dyn ProcessCapability>),
    Browser(std::sync::Arc<dyn BrowserCapability>),
    Frame(std::sync::Arc<dyn FrameCapability>),
    PauseFutureChildren(std::sync::Arc<dyn PauseFutureChildrenCapability>),
}

impl CapabilityObject {
    /// The kind the object reports about itself.
    pub fn kind(&self) -> CapabilityKind {
        match self {
            Self::Explore(object) => object.kind(),
            Self::Debug(object) => object.kind(),
            Self::Process(object) => object.kind(),
            Self::Browser(object) => object.kind(),
            Self::Frame(object) => object.kind(),
            Self::PauseFutureChildren(object) => object.kind(),
        }
    }

    /// The kind the variant guarantees structurally.
    pub fn variant_kind(&self) -> CapabilityKind {
        match self {
            Self::Explore(_) => CapabilityKind::Explore,
            Self::Debug(_) => CapabilityKind::Debug,
            Self::Process(_) => CapabilityKind::Process,
            Self::Browser(_) => CapabilityKind::Browser,
            Self::Frame(_) => CapabilityKind::Frame,
            Self::PauseFutureChildren(_) => CapabilityKind::PauseFutureChildren,
        }
    }

    /// Derives the summary from the object itself.
    pub fn summary(&self) -> CapabilitySummary {
        match self {
            Self::Explore(object) => object.summary(),
            Self::Debug(object) => object.summary(),
            Self::Process(object) => object.summary(),
            Self::Browser(object) => object.summary(),
            Self::Frame(object) => object.summary(),
            Self::PauseFutureChildren(object) => object.summary(),
        }
    }

    /// Rejects an object whose self-reported kind contradicts its variant, so a descriptor derived
    /// from it can never be misleading.
    pub fn validate(&self) -> Result<(), CapabilityError> {
        let expected = self.variant_kind();
        let actual = self.kind();
        if expected == actual {
            Ok(())
        } else {
            Err(CapabilityError::KindMismatch { expected, actual })
        }
    }

    pub fn as_explore(&self) -> Option<std::sync::Arc<dyn ExploreCapability>> {
        match self {
            Self::Explore(object) => Some(object.clone()),
            _ => None,
        }
    }

    pub fn as_debug(&self) -> Option<std::sync::Arc<dyn DebugCapability>> {
        match self {
            Self::Debug(object) => Some(object.clone()),
            _ => None,
        }
    }

    pub fn as_process(&self) -> Option<std::sync::Arc<dyn ProcessCapability>> {
        match self {
            Self::Process(object) => Some(object.clone()),
            _ => None,
        }
    }

    pub fn as_browser(&self) -> Option<std::sync::Arc<dyn BrowserCapability>> {
        match self {
            Self::Browser(object) => Some(object.clone()),
            _ => None,
        }
    }

    pub fn as_frame(&self) -> Option<std::sync::Arc<dyn FrameCapability>> {
        match self {
            Self::Frame(object) => Some(object.clone()),
            _ => None,
        }
    }

    pub fn as_pause_future_children(
        &self,
    ) -> Option<std::sync::Arc<dyn PauseFutureChildrenCapability>> {
        match self {
            Self::PauseFutureChildren(object) => Some(object.clone()),
            _ => None,
        }
    }
}

impl fmt::Debug for CapabilityObject {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_tuple("CapabilityObject")
            .field(&self.variant_kind())
            .finish()
    }
}

/// A descriptor for one registered capability, derived on demand.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CapabilityDescriptor {
    pub handle: CapabilityHandleId,
    pub kind: CapabilityKind,
    pub summary: CapabilitySummary,
}

/// Keeps callable capability objects alive and addressable by handle.
///
/// The registry is intentionally the *only* authority: it stores objects, and every descriptor it
/// hands out is computed from those objects when asked. Nothing here can be mutated independently
/// of the object it describes.
#[derive(Clone, Default)]
pub struct CapabilityRegistry {
    next_handle: u64,
    entries: BTreeMap<CapabilityHandleId, CapabilityObject>,
}

impl CapabilityRegistry {
    pub fn new() -> Self {
        Self {
            next_handle: 1,
            entries: BTreeMap::new(),
        }
    }

    /// Registers a validated object. Registration is the single atomic step that publishes a
    /// capability; a rejected object is not stored at all.
    pub fn register(
        &mut self,
        object: CapabilityObject,
    ) -> Result<CapabilityHandleId, CapabilityError> {
        object.validate()?;
        if self.next_handle == 0 {
            self.next_handle = 1;
        }
        let handle = CapabilityHandleId(self.next_handle);
        self.next_handle += 1;
        self.entries.insert(handle, object);
        Ok(handle)
    }

    /// Removes a capability. Returns whether the handle was registered.
    pub fn remove(&mut self, handle: CapabilityHandleId) -> bool {
        self.entries.remove(&handle).is_some()
    }

    pub fn get(&self, handle: CapabilityHandleId) -> Option<CapabilityObject> {
        self.entries.get(&handle).cloned()
    }

    pub fn contains(&self, handle: CapabilityHandleId) -> bool {
        self.entries.contains_key(&handle)
    }

    pub fn len(&self) -> usize {
        self.entries.len()
    }

    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    pub fn kind(&self, handle: CapabilityHandleId) -> Option<CapabilityKind> {
        self.entries.get(&handle).map(CapabilityObject::kind)
    }

    /// Derives the summary from the registered object.
    pub fn summary(&self, handle: CapabilityHandleId) -> Option<CapabilitySummary> {
        self.entries.get(&handle).map(CapabilityObject::summary)
    }

    /// Derives the full descriptor from the registered object.
    pub fn descriptor(&self, handle: CapabilityHandleId) -> Option<CapabilityDescriptor> {
        let object = self.entries.get(&handle)?;
        Some(CapabilityDescriptor {
            handle,
            kind: object.kind(),
            summary: object.summary(),
        })
    }

    pub fn explore(
        &self,
        handle: CapabilityHandleId,
    ) -> Result<std::sync::Arc<dyn ExploreCapability>, CapabilityError> {
        self.typed(
            handle,
            CapabilityObject::as_explore,
            CapabilityKind::Explore,
        )
    }

    pub fn debug(
        &self,
        handle: CapabilityHandleId,
    ) -> Result<std::sync::Arc<dyn DebugCapability>, CapabilityError> {
        self.typed(handle, CapabilityObject::as_debug, CapabilityKind::Debug)
    }

    pub fn process(
        &self,
        handle: CapabilityHandleId,
    ) -> Result<std::sync::Arc<dyn ProcessCapability>, CapabilityError> {
        self.typed(
            handle,
            CapabilityObject::as_process,
            CapabilityKind::Process,
        )
    }

    pub fn browser(
        &self,
        handle: CapabilityHandleId,
    ) -> Result<std::sync::Arc<dyn BrowserCapability>, CapabilityError> {
        self.typed(
            handle,
            CapabilityObject::as_browser,
            CapabilityKind::Browser,
        )
    }

    pub fn frame(
        &self,
        handle: CapabilityHandleId,
    ) -> Result<std::sync::Arc<dyn FrameCapability>, CapabilityError> {
        self.typed(handle, CapabilityObject::as_frame, CapabilityKind::Frame)
    }

    pub fn pause_future_children(
        &self,
        handle: CapabilityHandleId,
    ) -> Result<std::sync::Arc<dyn PauseFutureChildrenCapability>, CapabilityError> {
        self.typed(
            handle,
            CapabilityObject::as_pause_future_children,
            CapabilityKind::PauseFutureChildren,
        )
    }

    fn typed<T>(
        &self,
        handle: CapabilityHandleId,
        project: impl Fn(&CapabilityObject) -> Option<std::sync::Arc<T>>,
        expected: CapabilityKind,
    ) -> Result<std::sync::Arc<T>, CapabilityError>
    where
        T: ?Sized,
    {
        let object = self
            .entries
            .get(&handle)
            .ok_or(CapabilityError::UnknownHandle(handle))?;
        project(object).ok_or(CapabilityError::KindMismatch {
            expected,
            actual: object.variant_kind(),
        })
    }
}

#[cfg(test)]
pub(crate) mod testing {
    //! Minimal capability implementations used by the unit tests of this crate. They exercise the
    //! object safety of every trait and the "summary follows the object" contract.

    use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
    use std::sync::{Arc, Mutex};

    use super::*;
    use crate::connection::discovery::{
        DiscoveryKey, DiscoveryLeaseRegistry, PauseChildrenKey, PauseChildrenLeaseRegistry,
    };
    use crate::debugger::resource_graph::RelationKind;

    pub struct FakeProcess {
        pid: u32,
        title: Mutex<String>,
        terminated: AtomicBool,
    }

    impl FakeProcess {
        pub fn new(pid: u32) -> Self {
            Self {
                pid,
                title: Mutex::new(format!("process {pid}")),
                terminated: AtomicBool::new(false),
            }
        }

        pub fn rename(&self, title: impl Into<String>) {
            *self.title.lock().unwrap() = title.into();
        }

        pub fn is_terminated(&self) -> bool {
            self.terminated.load(Ordering::SeqCst)
        }
    }

    impl Capability for FakeProcess {
        fn kind(&self) -> CapabilityKind {
            CapabilityKind::Process
        }

        fn summary(&self) -> CapabilitySummary {
            CapabilitySummary::new(self.title.lock().unwrap().clone())
                .with_detail("pid", Value::from(self.pid))
        }
    }

    #[async_trait]
    impl ProcessCapability for FakeProcess {
        fn pid(&self) -> u32 {
            self.pid
        }

        async fn command_line(&self) -> Result<Vec<String>, CapabilityError> {
            Ok(vec!["node".to_string(), "app.js".to_string()])
        }

        async fn kill(&self) -> Result<(), CapabilityError> {
            self.terminated.store(true, Ordering::SeqCst);
            Ok(())
        }
    }

    pub struct FakeExplore {
        leases: DiscoveryLeaseRegistry,
        key: DiscoveryKey,
        starts: AtomicU32,
    }

    impl FakeExplore {
        pub fn new() -> Self {
            Self::for_key(DiscoveryKey::new(
                ResourceId::from_parts("process", ["1"]).unwrap(),
                Some(RelationKind::Contains),
            ))
        }

        pub fn for_key(key: DiscoveryKey) -> Self {
            Self {
                leases: DiscoveryLeaseRegistry::new(),
                key,
                starts: AtomicU32::new(0),
            }
        }

        pub fn starts(&self) -> u32 {
            self.starts.load(Ordering::SeqCst)
        }

        pub fn demand(&self) -> usize {
            self.leases.demand(&self.key)
        }
    }

    impl Capability for FakeExplore {
        fn kind(&self) -> CapabilityKind {
            CapabilityKind::Explore
        }

        fn summary(&self) -> CapabilitySummary {
            CapabilitySummary::new("explore children")
                .with_detail("starts", Value::from(self.starts.load(Ordering::SeqCst)))
        }
    }

    #[async_trait]
    impl ExploreCapability for FakeExplore {
        async fn start(&self, _request: ExploreRequest) -> Result<DiscoveryLease, CapabilityError> {
            self.starts.fetch_add(1, Ordering::SeqCst);
            Ok(self.leases.acquire(self.key.clone()))
        }
    }

    pub struct FakeDebug {
        resource: ResourceId,
        owned: AtomicBool,
    }

    impl FakeDebug {
        pub fn new(resource: ResourceId) -> Self {
            Self {
                resource,
                owned: AtomicBool::new(true),
            }
        }
    }

    impl Capability for FakeDebug {
        fn kind(&self) -> CapabilityKind {
            CapabilityKind::Debug
        }

        fn summary(&self) -> CapabilitySummary {
            CapabilitySummary::new("open debug session").with_detail(
                "foreignOwner",
                Value::from(self.owned.load(Ordering::SeqCst)),
            )
        }
    }

    #[async_trait]
    impl DebugCapability for FakeDebug {
        async fn open(
            &self,
            request: DebugOpenRequest,
        ) -> Result<DebugSessionHandle, CapabilityError> {
            let owned = self.owned.load(Ordering::SeqCst);
            if owned && !request.steal_existing_owner {
                return Err(CapabilityError::Rejected(
                    "another debugger owns the target".to_string(),
                ));
            }
            self.owned.store(false, Ordering::SeqCst);
            Ok(DebugSessionHandle {
                session_id: "session-1".to_string(),
                resource: self.resource.clone(),
                stole_existing_owner: owned,
            })
        }

        fn take_session(
            &self,
            _handle: &DebugSessionHandle,
        ) -> Result<CdpDebuggerSession, CapabilityError> {
            Err(CapabilityError::Unavailable(
                "fake debug capability has no protocol transport".to_owned(),
            ))
        }

        async fn close(&self, _handle: &DebugSessionHandle) -> Result<(), CapabilityError> {
            Ok(())
        }
    }

    pub struct FakeBrowser {
        pages: Mutex<Vec<ResourceId>>,
    }

    impl FakeBrowser {
        pub fn new(pages: Vec<ResourceId>) -> Self {
            Self {
                pages: Mutex::new(pages),
            }
        }
    }

    impl Capability for FakeBrowser {
        fn kind(&self) -> CapabilityKind {
            CapabilityKind::Browser
        }

        fn summary(&self) -> CapabilitySummary {
            CapabilitySummary::new("browser")
                .with_detail("pages", Value::from(self.pages.lock().unwrap().len()))
        }
    }

    #[async_trait]
    impl BrowserCapability for FakeBrowser {
        async fn product(&self) -> Result<String, CapabilityError> {
            Ok("Chrome/fake".to_string())
        }

        async fn pages(&self) -> Result<Vec<ResourceId>, CapabilityError> {
            Ok(self.pages.lock().unwrap().clone())
        }

        async fn open_page(&self, url: &str) -> Result<ResourceId, CapabilityError> {
            let id = ResourceId::from_parts("page", [url])
                .map_err(|error| CapabilityError::Failed(error.to_string()))?;
            self.pages.lock().unwrap().push(id.clone());
            Ok(id)
        }
    }

    pub struct FakeFrame {
        frame_id: String,
    }

    impl FakeFrame {
        pub fn new(frame_id: impl Into<String>) -> Self {
            Self {
                frame_id: frame_id.into(),
            }
        }
    }

    impl Capability for FakeFrame {
        fn kind(&self) -> CapabilityKind {
            CapabilityKind::Frame
        }

        fn summary(&self) -> CapabilitySummary {
            CapabilitySummary::new(format!("frame {}", self.frame_id))
        }
    }

    #[async_trait]
    impl FrameCapability for FakeFrame {
        fn frame_id(&self) -> String {
            self.frame_id.clone()
        }

        async fn url(&self) -> Result<String, CapabilityError> {
            Ok("https://example.test/".to_string())
        }

        async fn evaluate(&self, expression: &str) -> Result<Value, CapabilityError> {
            Ok(Value::from(expression))
        }
    }

    pub struct FakePauseChildren {
        leases: PauseChildrenLeaseRegistry,
        key: PauseChildrenKey,
        resumed: Mutex<Vec<ResourceId>>,
    }

    impl FakePauseChildren {
        pub fn new(resource: ResourceId) -> Self {
            Self {
                leases: PauseChildrenLeaseRegistry::new(),
                key: PauseChildrenKey(resource),
                resumed: Mutex::new(Vec::new()),
            }
        }

        pub fn armed(&self) -> bool {
            self.leases.demand(&self.key) > 0
        }

        pub fn resumed(&self) -> Vec<ResourceId> {
            self.resumed.lock().unwrap().clone()
        }
    }

    impl Capability for FakePauseChildren {
        fn kind(&self) -> CapabilityKind {
            CapabilityKind::PauseFutureChildren
        }

        fn summary(&self) -> CapabilitySummary {
            CapabilitySummary::new("pause future children")
                .with_detail("armed", Value::from(self.leases.demand(&self.key) > 0))
        }
    }

    #[async_trait]
    impl PauseFutureChildrenCapability for FakePauseChildren {
        async fn arm(&self) -> Result<PauseChildrenLease, CapabilityError> {
            Ok(self.leases.acquire(self.key.clone()))
        }

        async fn resume(&self, resource: &ResourceId) -> Result<(), CapabilityError> {
            self.resumed.lock().unwrap().push(resource.clone());
            Ok(())
        }
    }

    /// A capability object that lies about its own kind, used to prove that registration rejects
    /// it instead of publishing a misleading descriptor.
    pub struct LyingProcess;

    impl Capability for LyingProcess {
        fn kind(&self) -> CapabilityKind {
            CapabilityKind::Browser
        }

        fn summary(&self) -> CapabilitySummary {
            CapabilitySummary::new("liar")
        }
    }

    #[async_trait]
    impl ProcessCapability for LyingProcess {
        fn pid(&self) -> u32 {
            0
        }

        async fn command_line(&self) -> Result<Vec<String>, CapabilityError> {
            Ok(Vec::new())
        }

        async fn kill(&self) -> Result<(), CapabilityError> {
            Ok(())
        }
    }

    pub fn process_object(pid: u32) -> (Arc<FakeProcess>, CapabilityObject) {
        let object = Arc::new(FakeProcess::new(pid));
        (object.clone(), CapabilityObject::Process(object))
    }
}

#[cfg(test)]
mod tests {
    use super::testing::*;
    use super::*;
    use std::sync::Arc;

    #[test]
    fn registration_yields_unique_handles_and_typed_lookup() {
        let mut registry = CapabilityRegistry::new();
        let (_, process) = process_object(11);
        let first = registry.register(process).unwrap();
        let second = registry
            .register(CapabilityObject::Frame(Arc::new(FakeFrame::new("F1"))))
            .unwrap();

        assert_ne!(first, second);
        assert_eq!(registry.len(), 2);
        assert_eq!(registry.process(first).unwrap().pid(), 11);
        assert_eq!(registry.frame(second).unwrap().frame_id(), "F1");
        assert!(matches!(
            registry.frame(first),
            Err(CapabilityError::KindMismatch { .. })
        ));
    }

    #[test]
    fn removal_invalidates_the_handle_without_reuse() {
        let mut registry = CapabilityRegistry::new();
        let (_, process) = process_object(3);
        let handle = registry.register(process).unwrap();
        assert!(registry.remove(handle));
        assert!(!registry.remove(handle));
        assert!(registry.get(handle).is_none());
        assert!(registry.summary(handle).is_none());
        assert!(matches!(
            registry.process(handle),
            Err(CapabilityError::UnknownHandle(_))
        ));

        let (_, other) = process_object(4);
        let reregistered = registry.register(other).unwrap();
        assert_ne!(reregistered, handle, "handles are never reused");
    }

    #[test]
    fn an_object_that_misreports_its_kind_is_rejected() {
        let mut registry = CapabilityRegistry::new();
        let error = registry
            .register(CapabilityObject::Process(Arc::new(LyingProcess)))
            .unwrap_err();
        assert_eq!(
            error,
            CapabilityError::KindMismatch {
                expected: CapabilityKind::Process,
                actual: CapabilityKind::Browser,
            }
        );
        assert!(registry.is_empty(), "rejected objects are not stored");
    }

    #[test]
    fn descriptors_are_derived_from_the_object_on_every_call() {
        let mut registry = CapabilityRegistry::new();
        let (object, capability) = process_object(5);
        let handle = registry.register(capability).unwrap();

        let before = registry.descriptor(handle).unwrap();
        assert_eq!(before.kind, CapabilityKind::Process);
        assert_eq!(before.summary.title, "process 5");

        object.rename("process 5 (renamed)");
        let after = registry.descriptor(handle).unwrap();
        assert_eq!(after.summary.title, "process 5 (renamed)");
        assert_eq!(before.handle, after.handle);
    }

    #[tokio::test]
    async fn capabilities_are_callable_through_trait_objects() {
        let mut registry = CapabilityRegistry::new();
        let resource = ResourceId::from_parts("process", ["1"]).unwrap();

        let (process, process_object) = process_object(9);
        let process_handle = registry.register(process_object).unwrap();
        let debug_handle = registry
            .register(CapabilityObject::Debug(Arc::new(FakeDebug::new(
                resource.clone(),
            ))))
            .unwrap();
        let browser_handle = registry
            .register(CapabilityObject::Browser(Arc::new(FakeBrowser::new(
                Vec::new(),
            ))))
            .unwrap();
        let frame_handle = registry
            .register(CapabilityObject::Frame(Arc::new(FakeFrame::new("F2"))))
            .unwrap();
        let explore = Arc::new(FakeExplore::new());
        let explore_handle = registry
            .register(CapabilityObject::Explore(explore.clone()))
            .unwrap();
        let pause = Arc::new(FakePauseChildren::new(resource.clone()));
        let pause_handle = registry
            .register(CapabilityObject::PauseFutureChildren(pause.clone()))
            .unwrap();

        registry
            .process(process_handle)
            .unwrap()
            .kill()
            .await
            .unwrap();
        assert!(process.is_terminated());

        let rejected = registry
            .debug(debug_handle)
            .unwrap()
            .open(DebugOpenRequest::default())
            .await
            .unwrap_err();
        assert!(matches!(rejected, CapabilityError::Rejected(_)));
        let session = registry
            .debug(debug_handle)
            .unwrap()
            .open(DebugOpenRequest {
                pause_on_start: true,
                steal_existing_owner: true,
            })
            .await
            .unwrap();
        assert!(session.stole_existing_owner);
        assert_eq!(session.resource, resource);

        let browser = registry.browser(browser_handle).unwrap();
        assert_eq!(browser.product().await.unwrap(), "Chrome/fake");
        let page = browser.open_page("https://example.test/").await.unwrap();
        assert_eq!(browser.pages().await.unwrap(), vec![page]);

        let frame = registry.frame(frame_handle).unwrap();
        assert_eq!(frame.url().await.unwrap(), "https://example.test/");
        assert_eq!(frame.evaluate("1+1").await.unwrap(), Value::from("1+1"));

        {
            let _lease = registry
                .explore(explore_handle)
                .unwrap()
                .start(ExploreRequest::live())
                .await
                .unwrap();
            assert_eq!(explore.demand(), 1);
            assert_eq!(explore.starts(), 1);
        }
        assert_eq!(explore.demand(), 0, "dropping the lease releases discovery");

        let pause_capability = registry.pause_future_children(pause_handle).unwrap();
        {
            let _armed = pause_capability.arm().await.unwrap();
            assert!(pause.armed());
        }
        assert!(!pause.armed());
        pause_capability.resume(&resource).await.unwrap();
        assert_eq!(pause.resumed(), vec![resource]);
    }
}
