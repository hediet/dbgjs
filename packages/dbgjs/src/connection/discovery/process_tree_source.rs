//! The [`TargetSource`] behind a connected OS process tree.
//!
//! It combines three discovery mechanisms into one CDP `Target` domain: the OS descendant scan
//! performed by `providers/process_tree.mjs` (Node.js child processes and Chromium DevTools
//! endpoints), the Electron main process itself, and - when the root process is an Electron app -
//! the renderer targets reported by the main-process bridge. Browser roots and renderer endpoints
//! contribute their immediate `Target` inventory only while that specific parent is being
//! observed. Recursive traversal is driven by [`crate::service::virtual_browser_root::VirtualBrowserRoot`].
//! Every Electron and WebContents detail stays inside this module and
//! [`crate::connection::transport::electron_renderer_transport`].

use std::collections::{BTreeMap, BTreeSet};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};

use async_trait::async_trait;
use tokio::io::AsyncWriteExt;
use tokio::process::ChildStdin;
use tokio::sync::{Mutex, mpsc, oneshot};
use tokio::task::JoinHandle;

use crate::cdp::{
    TargetAttachedToTargetParams, TargetDetachedFromTargetParams, TargetTargetCreatedParams,
    TargetTargetDestroyedParams, TargetTargetInfo, TargetTargetInfoChangedParams,
};
use crate::connection::providers::ProviderEvent;
use crate::connection::transport::electron_renderer_transport::{
    BridgeEvent, ElectronRendererBridge, ElectronRendererTarget,
};
use crate::api::service_api::TargetSnapshot;
use crate::service::virtual_browser_root::{
    HostTarget, TargetAttachment, TargetEndpoint, TargetSource, TargetSourceEvent,
};
use crate::connection::transport::websocket_transport::CdpWebSocketTransport;

/// The CDP target id of the process the connection was made to. The debugger service canonicalizes
/// it into its own synthetic per-connection id.
pub const ROOT_TARGET_ID: &str = "$node-root";

const RENDERER_TARGET_PREFIX: &str = "renderer-";

#[derive(Clone)]
struct NodeRecord {
    target: HostTarget,
    endpoint: Option<String>,
}

#[derive(Clone)]
struct NestedTargetRecord {
    parent_target_id: String,
    native_target_id: String,
    target: HostTarget,
}

pub struct ProcessTreeTargetSource {
    root_pid: u32,
    root_endpoint: Arc<TargetEndpoint>,
    browser_endpoint: Option<Arc<TargetEndpoint>>,
    bridge: Mutex<Option<Arc<ElectronRendererBridge>>>,
    control: Mutex<Option<ChildStdin>>,
    events: mpsc::UnboundedSender<TargetSourceEvent>,
    nodes: std::sync::Mutex<BTreeMap<String, NodeRecord>>,
    renderers: Arc<std::sync::Mutex<BTreeMap<String, ElectronRendererTarget>>>,
    renderer_correlations: std::sync::Mutex<BTreeSet<String>>,
    native_target_aliases: Arc<std::sync::Mutex<BTreeMap<String, String>>>,
    nested_targets: Arc<std::sync::Mutex<BTreeMap<String, NestedTargetRecord>>>,
    supervised_targets: Arc<std::sync::Mutex<BTreeSet<String>>>,
    debugger_targets: std::sync::Mutex<BTreeSet<String>>,
    attachments: Mutex<BTreeMap<String, Arc<TargetEndpoint>>>,
    attachment_guard: Mutex<()>,
    next_scan_id: AtomicU64,
    scans: std::sync::Mutex<BTreeMap<u64, oneshot::Sender<()>>>,
    activations: std::sync::Mutex<BTreeMap<u64, oneshot::Sender<Result<String, String>>>>,
    discovering: AtomicBool,
    discovery_change: Mutex<()>,
    tasks: std::sync::Mutex<Vec<JoinHandle<()>>>,
}

impl ProcessTreeTargetSource {
    /// Opens the shared main-process endpoint, installs the Electron bridge when the root process
    /// is an Electron main process, and starts pumping provider and bridge discovery events.
    pub(crate) async fn start(
        root_pid: u32,
        root_endpoint_url: &str,
        browser_endpoint_url: Option<&str>,
        control: ChildStdin,
        provider_events: mpsc::UnboundedReceiver<ProviderEvent>,
    ) -> Result<(Arc<Self>, mpsc::UnboundedReceiver<TargetSourceEvent>), String> {
        let transport = Arc::new(
            CdpWebSocketTransport::connect(root_endpoint_url)
                .await
                .map_err(|error| error.to_string())?,
        );
        let root_endpoint = TargetEndpoint::open(transport)?;
        let browser_endpoint = if let Some(url) = browser_endpoint_url {
            let transport = Arc::new(
                CdpWebSocketTransport::connect(url)
                    .await
                    .map_err(|error| error.to_string())?,
            );
            Some(TargetEndpoint::open(transport)?)
        } else {
            None
        };
        let (events, receiver) = mpsc::unbounded_channel();
        let source = Arc::new(Self {
            root_pid,
            root_endpoint: root_endpoint.clone(),
            browser_endpoint,
            bridge: Mutex::new(None),
            control: Mutex::new(Some(control)),
            events,
            nodes: std::sync::Mutex::new(BTreeMap::new()),
            renderers: Arc::new(std::sync::Mutex::new(BTreeMap::new())),
            renderer_correlations: std::sync::Mutex::new(BTreeSet::new()),
            native_target_aliases: Arc::new(std::sync::Mutex::new(BTreeMap::new())),
            nested_targets: Arc::new(std::sync::Mutex::new(BTreeMap::new())),
            supervised_targets: Arc::new(std::sync::Mutex::new(BTreeSet::new())),
            debugger_targets: std::sync::Mutex::new(BTreeSet::new()),
            attachments: Mutex::new(BTreeMap::new()),
            attachment_guard: Mutex::new(()),
            next_scan_id: AtomicU64::new(1),
            scans: std::sync::Mutex::new(BTreeMap::new()),
            activations: std::sync::Mutex::new(BTreeMap::new()),
            discovering: AtomicBool::new(false),
            discovery_change: Mutex::new(()),
            tasks: std::sync::Mutex::new(Vec::new()),
        });
        source.supervise_provider_events(provider_events);
        // Browser roots have no Runtime domain; plain Node roots explicitly return no bridge.
        if !root_endpoint_url.contains("/devtools/browser/") {
            match ElectronRendererBridge::install(
                root_endpoint.client(),
                source.browser_endpoint.is_some(),
            )
            .await
            {
                Ok(Some(bridge)) => {
                    source.supervise_bridge_events(&bridge).await;
                    *source.bridge.lock().await = Some(bridge);
                }
                Ok(None) => {}
                Err(error) => {
                    source.close().await;
                    return Err(error.to_string());
                }
            }
        }
        Ok((source, receiver))
    }

    fn track(&self, task: JoinHandle<()>) {
        self.tasks.lock().unwrap().push(task);
    }

    fn root_target(&self) -> HostTarget {
        HostTarget {
            snapshot: TargetSnapshot {
                target_id: ROOT_TARGET_ID.to_owned(),
                target_type: "node".to_owned(),
                title: format!("Process {}", self.root_pid),
                url: format!("process:{}", self.root_pid),
                attached: false,
                parent_id: None,
                opener_id: None,
                browser_context_id: None,
                subtype: None,
            },
            process_id: Some(self.root_pid),
            primary_window_id: None,
            waiting_for_debugger: false,
        }
    }

    fn supervise_provider_events(
        self: &Arc<Self>,
        mut events: mpsc::UnboundedReceiver<ProviderEvent>,
    ) {
        let source = Arc::downgrade(self);
        self.track(tokio::spawn(async move {
            while let Some(event) = events.recv().await {
                let Some(source) = source.upgrade() else {
                    return;
                };
                match event {
                    ProviderEvent::NodeTarget {
                        target_id,
                        parent_target_id,
                        target_type,
                        title,
                        url,
                        endpoint,
                        process_id,
                    } => {
                        let target = HostTarget {
                            snapshot: node_target_snapshot(
                                target_id.clone(),
                                parent_target_id,
                                target_type,
                                title,
                                url,
                            ),
                            process_id,
                            primary_window_id: None,
                            waiting_for_debugger: false,
                        };
                        source.nodes.lock().unwrap().insert(
                            target_id.clone(),
                            NodeRecord {
                                target: target.clone(),
                                endpoint,
                            },
                        );
                        let _ = source.events.send(TargetSourceEvent::Upserted(target));
                    }
                    ProviderEvent::NodeTargetRemoved { target_id } => {
                        source.nodes.lock().unwrap().remove(&target_id);
                        source.remove_nested_targets(&target_id);
                        source.supervised_targets.lock().unwrap().remove(&target_id);
                        source.debugger_targets.lock().unwrap().remove(&target_id);
                        if let Some(endpoint) = source.attachments.lock().await.remove(&target_id) {
                            endpoint.close().await;
                        }
                        let _ = source.events.send(TargetSourceEvent::Removed(target_id));
                    }
                    ProviderEvent::ScanComplete { id } => {
                        if let Some(waiter) =
                            id.and_then(|id| source.scans.lock().unwrap().remove(&id))
                        {
                            let _ = waiter.send(());
                        }
                    }
                    ProviderEvent::ActivationComplete {
                        id,
                        target_id,
                        endpoint,
                        error,
                    } => {
                        let result = match (endpoint, error) {
                            (Some(endpoint), None) => {
                                if let Some(record) =
                                    source.nodes.lock().unwrap().get_mut(&target_id)
                                {
                                    record.endpoint = Some(endpoint.clone());
                                }
                                Ok(endpoint)
                            }
                            (_, Some(error)) => Err(error),
                            _ => Err("process activation returned no endpoint".to_owned()),
                        };
                        if let Some(waiter) =
                            id.and_then(|id| source.activations.lock().unwrap().remove(&id))
                        {
                            let _ = waiter.send(result);
                        }
                    }
                }
            }
        }));
    }

    async fn supervise_bridge_events(self: &Arc<Self>, bridge: &Arc<ElectronRendererBridge>) {
        let Some(mut events) = bridge.take_events().await else {
            return;
        };
        let source = Arc::downgrade(self);
        self.track(tokio::spawn(async move {
            while let Some(event) = events.recv().await {
                let Some(source) = source.upgrade() else {
                    return;
                };
                match event {
                    BridgeEvent::TargetCreated(target) | BridgeEvent::TargetInfoChanged(target) => {
                        let host = renderer_host_target(&target);
                        let target_id = host.snapshot.target_id.clone();
                        source
                            .renderers
                            .lock()
                            .unwrap()
                            .insert(target_id.clone(), target);
                        let _ = source.events.send(TargetSourceEvent::Upserted(host));
                    }
                    BridgeEvent::TargetDestroyed { web_contents_id } => {
                        let target_id = renderer_target_id(web_contents_id);
                        source.renderers.lock().unwrap().remove(&target_id);
                        source
                            .renderer_correlations
                            .lock()
                            .unwrap()
                            .remove(&target_id);
                        source
                            .native_target_aliases
                            .lock()
                            .unwrap()
                            .retain(|_, alias| alias != &target_id);
                        source.remove_nested_targets(&target_id);
                        source.supervised_targets.lock().unwrap().remove(&target_id);
                        source.debugger_targets.lock().unwrap().remove(&target_id);
                        if let Some(endpoint) = source.attachments.lock().await.remove(&target_id) {
                            endpoint.close().await;
                        }
                        let _ = source.events.send(TargetSourceEvent::Removed(target_id));
                    }
                }
            }
        }));
    }

    async fn correlate_renderer(&self, renderer_id: &str) {
        let is_new = self
            .renderer_correlations
            .lock()
            .unwrap()
            .insert(renderer_id.to_owned());
        if !is_new {
            self.refresh_renderer_children().await;
            return;
        }
        let Some(endpoint) = self.attachments.lock().await.get(renderer_id).cloned() else {
            self.renderer_correlations
                .lock()
                .unwrap()
                .remove(renderer_id);
            return;
        };
        let result = endpoint.client().target().get_target_info(None).await;
        if let Ok(result) = result {
            let native_target_id = result.target_info.target_id;
            self.native_target_aliases
                .lock()
                .unwrap()
                .insert(native_target_id.clone(), renderer_id.to_owned());
            let removed = {
                let mut nested = self.nested_targets.lock().unwrap();
                let removed = nested
                    .iter()
                    .filter(|(_, record)| record.native_target_id == native_target_id)
                    .map(|(id, _)| id.clone())
                    .collect::<Vec<_>>();
                for id in &removed {
                    nested.remove(id);
                }
                removed
            };
            for id in removed {
                let _ = self.events.send(TargetSourceEvent::Removed(id));
            }
        } else {
            self.renderer_correlations
                .lock()
                .unwrap()
                .remove(renderer_id);
        }
        let mut notifications = endpoint.subscribe();
        let _ = endpoint
            .client()
            .target()
            .set_discover_targets(true, None)
            .await;
        self.supervised_targets
            .lock()
            .unwrap()
            .insert(renderer_id.to_owned());
        let nested_targets = self.nested_targets.clone();
        let aliases = self.native_target_aliases.clone();
        let renderers = self.renderers.clone();
        let events = self.events.clone();
        let parent_id = renderer_id.to_owned();
        let process_id = renderers
            .lock()
            .unwrap()
            .get(renderer_id)
            .map(|renderer| renderer.process_id);
        self.track(tokio::spawn(async move {
            let mut native_sessions = BTreeMap::<String, String>::new();
            while let Some((method, params)) = notifications.recv().await {
                match method.as_str() {
                    "Target.targetCreated" => {
                        if let Ok(params) =
                            serde_json::from_value::<TargetTargetCreatedParams>(params)
                            && renderer_owns_nested_target(
                                &params.target_info,
                                &parent_id,
                                &aliases,
                            )
                        {
                            upsert_nested_target(
                                &nested_targets,
                                &events,
                                &parent_id,
                                process_id,
                                &aliases,
                                &renderers,
                                true,
                                params.target_info,
                            );
                        }
                    }
                    "Target.attachedToTarget" => {
                        if let Ok(params) =
                            serde_json::from_value::<TargetAttachedToTargetParams>(params)
                            && renderer_owns_nested_target(
                                &params.target_info,
                                &parent_id,
                                &aliases,
                            )
                        {
                            native_sessions.insert(
                                params.session_id.clone(),
                                params.target_info.target_id.clone(),
                            );
                            upsert_nested_target(
                                &nested_targets,
                                &events,
                                &parent_id,
                                process_id,
                                &aliases,
                                &renderers,
                                true,
                                params.target_info,
                            );
                        }
                    }
                    "Target.targetInfoChanged" => {
                        if let Ok(params) =
                            serde_json::from_value::<TargetTargetInfoChangedParams>(params)
                            && renderer_owns_nested_target(
                                &params.target_info,
                                &parent_id,
                                &aliases,
                            )
                        {
                            upsert_nested_target(
                                &nested_targets,
                                &events,
                                &parent_id,
                                process_id,
                                &aliases,
                                &renderers,
                                true,
                                params.target_info,
                            );
                        }
                    }
                    "Target.detachedFromTarget" => {
                        if let Ok(params) =
                            serde_json::from_value::<TargetDetachedFromTargetParams>(params)
                        {
                            let target_id = params
                                .target_id
                                .or_else(|| native_sessions.get(&params.session_id).cloned());
                            native_sessions.remove(&params.session_id);
                            if let Some(target_id) = target_id {
                                mark_nested_target_detached(
                                    &nested_targets,
                                    &events,
                                    &parent_id,
                                    &target_id,
                                );
                            }
                        }
                    }
                    "Target.targetDestroyed" => {
                        if let Ok(params) =
                            serde_json::from_value::<TargetTargetDestroyedParams>(params)
                        {
                            remove_nested_target(
                                &nested_targets,
                                &aliases,
                                &events,
                                &parent_id,
                                &params.target_id,
                            );
                        }
                    }
                    _ => {}
                }
            }
        }));
        self.refresh_renderer_children().await;
        return;
    }

    async fn start_nested_discovery(&self, parent_target_id: &str) {
        if !self
            .supervised_targets
            .lock()
            .unwrap()
            .insert(parent_target_id.to_owned())
        {
            if let Some(endpoint) = self.attachments.lock().await.get(parent_target_id).cloned() {
                let _ = endpoint
                    .client()
                    .target()
                    .set_discover_targets(true, None)
                    .await;
            }

            return;
        }
        let Some(endpoint) = self.attachments.lock().await.get(parent_target_id).cloned() else {
            self.supervised_targets
                .lock()
                .unwrap()
                .remove(parent_target_id);
            return;
        };
        let process_id = self
            .renderers
            .lock()
            .unwrap()
            .get(parent_target_id)
            .map(|target| target.process_id)
            .or_else(|| {
                self.nodes
                    .lock()
                    .unwrap()
                    .get(parent_target_id)
                    .and_then(|record| record.target.process_id)
            });
        let mut notifications = endpoint.subscribe();
        let nested_targets = self.nested_targets.clone();
        let native_target_aliases = self.native_target_aliases.clone();
        let renderers = self.renderers.clone();
        let events = self.events.clone();
        let parent_id = parent_target_id.to_owned();
        self.track(tokio::spawn(async move {
            while let Some((method, params)) = notifications.recv().await {
                match method.as_str() {
                    "Target.targetCreated" => {
                        if let Ok(params) =
                            serde_json::from_value::<TargetTargetCreatedParams>(params)
                        {
                            upsert_nested_target(
                                &nested_targets,
                                &events,
                                &parent_id,
                                process_id,
                                &native_target_aliases,
                                &renderers,
                                false,
                                params.target_info,
                            );
                        }
                    }
                    "Target.targetInfoChanged" => {
                        if let Ok(params) =
                            serde_json::from_value::<TargetTargetInfoChangedParams>(params)
                        {
                            upsert_nested_target(
                                &nested_targets,
                                &events,
                                &parent_id,
                                process_id,
                                &native_target_aliases,
                                &renderers,
                                false,
                                params.target_info,
                            );
                        }
                    }
                    "Target.targetDestroyed" => {
                        if let Ok(params) =
                            serde_json::from_value::<TargetTargetDestroyedParams>(params)
                        {
                            remove_nested_target(
                                &nested_targets,
                                &native_target_aliases,
                                &events,
                                &parent_id,
                                &params.target_id,
                            );
                        }
                    }
                    _ => {}
                }
            }
        }));
        if endpoint
            .client()
            .target()
            .set_discover_targets(true, None)
            .await
            .is_err()
        {
            self.supervised_targets
                .lock()
                .unwrap()
                .remove(parent_target_id);
            return;
        }
        if let Ok(targets) = endpoint.client().target().get_targets(None).await {
            for target in targets.target_infos {
                upsert_nested_target(
                    &self.nested_targets,
                    &self.events,
                    parent_target_id,
                    process_id,
                    &self.native_target_aliases,
                    &self.renderers,
                    false,
                    target,
                );
            }
        }
    }

    fn remove_nested_targets(&self, parent_target_id: &str) {
        let removed = {
            let mut nested = self.nested_targets.lock().unwrap();
            let ids = nested
                .iter()
                .filter(|(_, record)| record.parent_target_id == parent_target_id)
                .map(|(id, _)| id.clone())
                .collect::<Vec<_>>();
            for id in &ids {
                nested.remove(id);
            }
            ids
        };
        for id in removed {
            let _ = self.events.send(TargetSourceEvent::Removed(id));
        }
    }

    async fn send_command(&self, command: serde_json::Value) {
        let mut control = self.control.lock().await;
        let Some(stdin) = control.as_mut() else {
            return;
        };
        let line = format!("{command}\n");
        if stdin.write_all(line.as_bytes()).await.is_err() || stdin.flush().await.is_err() {
            *control = None;
        }
    }

    /// Runs exactly one descendant scan and waits for it to finish, so `Target.getTargets` can be
    /// answered without leaving continuous discovery running.
    async fn scan_once(&self) {
        if self.control.lock().await.is_none() {
            return;
        }
        let id = self.next_scan_id.fetch_add(1, Ordering::Relaxed);
        let (sender, receiver) = oneshot::channel();
        self.scans.lock().unwrap().insert(id, sender);
        self.send_command(serde_json::json!({ "command": "scan", "id": id }))
            .await;
        if tokio::time::timeout(std::time::Duration::from_secs(30), receiver)
            .await
            .is_err()
        {
            self.scans.lock().unwrap().remove(&id);
        }
    }

    async fn activate_node_endpoint(
        &self,
        target_id: &str,
        process_id: u32,
    ) -> Result<String, String> {
        if self.control.lock().await.is_none() {
            return Err("process-tree provider is closed".to_owned());
        }
        let id = self.next_scan_id.fetch_add(1, Ordering::Relaxed);
        let (sender, receiver) = oneshot::channel();
        self.activations.lock().unwrap().insert(id, sender);
        self.send_command(serde_json::json!({
            "command": "activate",
            "id": id,
            "targetId": target_id,
            "processId": process_id,
        }))
        .await;
        match tokio::time::timeout(std::time::Duration::from_secs(30), receiver).await {
            Ok(Ok(result)) => result,
            Ok(Err(_)) => Err("process-tree provider closed during activation".to_owned()),
            Err(_) => {
                self.activations.lock().unwrap().remove(&id);
                Err(format!(
                    "timed out while activating the inspector for process {process_id}"
                ))
            }
        }
    }

    async fn refresh_renderers(&self) -> Vec<HostTarget> {
        let bridge = self.bridge.lock().await.clone();
        let Some(bridge) = bridge else {
            return Vec::new();
        };
        let Ok(targets) = bridge.list_targets().await else {
            return Vec::new();
        };
        let mut renderers = self.renderers.lock().unwrap();
        *renderers = targets
            .into_iter()
            .map(|target| (renderer_target_id(target.web_contents_id), target))
            .collect();
        renderers.values().map(renderer_host_target).collect()
    }

    async fn refresh_renderer_children(&self) {
        let attached = self
            .attachments
            .lock()
            .await
            .iter()
            .filter(|(id, _)| self.renderers.lock().unwrap().contains_key(*id))
            .map(|(id, endpoint)| (id.clone(), endpoint.clone()))
            .collect::<Vec<_>>();
        for (renderer_id, endpoint) in attached {
            let lookup = tokio::time::timeout(std::time::Duration::from_secs(2), async {
                let client = endpoint.client();
                let target_client = client.target();
                let parent = target_client.get_target_info(None).await.ok()?;
                let targets = target_client.get_targets(None).await.ok()?;
                Some((parent, targets))
            })
            .await;
            let Ok(Some((parent, targets))) = lookup else {
                continue;
            };
            let process_id = self
                .renderers
                .lock()
                .unwrap()
                .get(&renderer_id)
                .map(|renderer| renderer.process_id);
            let mut remaining = targets
                .target_infos
                .into_iter()
                .filter(|info| info.r#type == "iframe")
                .collect::<Vec<_>>();
            let mut parents = BTreeSet::from([parent.target_info.target_id]);
            let mut current = BTreeSet::new();
            while !remaining.is_empty() {
                let before = remaining.len();
                remaining.retain(|info| {
                    let Some(parent_id) = info.parent_id.as_ref() else {
                        return false;
                    };
                    if !parents.contains(parent_id) {
                        return true;
                    }
                    parents.insert(info.target_id.clone());
                    current.insert(info.target_id.clone());
                    upsert_nested_target(
                        &self.nested_targets,
                        &self.events,
                        &renderer_id,
                        process_id,
                        &self.native_target_aliases,
                        &self.renderers,
                        true,
                        info.clone(),
                    );
                    false
                });
                if remaining.len() == before {
                    break;
                }
            }
            let stale = self
                .nested_targets
                .lock()
                .unwrap()
                .values()
                .filter(|record| {
                    record.parent_target_id == renderer_id
                        && !current.contains(&record.native_target_id)
                })
                .map(|record| record.native_target_id.clone())
                .collect::<Vec<_>>();
            for native_id in stale {
                remove_nested_target(
                    &self.nested_targets,
                    &self.native_target_aliases,
                    &self.events,
                    &renderer_id,
                    &native_id,
                );
            }
        }
    }
}

#[async_trait]
impl TargetSource for ProcessTreeTargetSource {
    fn product(&self) -> String {
        format!("Process {}", self.root_pid)
    }

    async fn list_targets(&self) -> Vec<HostTarget> {
        if !self.discovering.load(Ordering::Relaxed) {
            self.scan_once().await;
        }
        let mut targets = vec![self.root_target()];
        targets.extend(
            self.nodes
                .lock()
                .unwrap()
                .values()
                .map(|record| record.target.clone()),
        );
        targets.extend(self.refresh_renderers().await);
        self.refresh_renderer_children().await;
        targets.extend(
            self.nested_targets
                .lock()
                .unwrap()
                .values()
                .map(|record| record.target.clone()),
        );
        targets
    }

    async fn set_discovery(&self, enabled: bool) {
        let _discovery_change = self.discovery_change.lock().await;
        if self.discovering.swap(enabled, Ordering::Relaxed) == enabled {
            return;
        }
        self.send_command(serde_json::json!({ "command": "setDiscovery", "enabled": enabled }))
            .await;
        if let Some(bridge) = self.bridge.lock().await.clone() {
            let _ = bridge.set_discovery(enabled).await;
        }
        if enabled {
            self.refresh_renderers().await;
        }
    }

    async fn set_child_discovery(&self, target_id: &str, enabled: bool) {
        let _discovery_change = self.discovery_change.lock().await;
        let process_id = self
            .nodes
            .lock()
            .unwrap()
            .get(target_id)
            .and_then(|record| record.target.process_id);
        if let Some(process_id) = process_id {
            self.send_command(serde_json::json!({
                "command": "setProcessDiscovery",
                "processId": process_id,
                "enabled": enabled,
            }))
            .await;
        }
        if enabled {
            if self.renderers.lock().unwrap().contains_key(target_id) {
                self.correlate_renderer(target_id).await;
            } else if self
                .nodes
                .lock()
                .unwrap()
                .get(target_id)
                .is_some_and(|record| record.target.snapshot.target_type == "browser")
            {
                self.start_nested_discovery(target_id).await;
            }
            return;
        }

        if let Some(endpoint) = self.attachments.lock().await.get(target_id).cloned()
            && endpoint.close_reason().await.is_none()
        {
            let _ = endpoint
                .client()
                .target()
                .set_discover_targets(false, None)
                .await;
        }
        self.supervised_targets.lock().unwrap().remove(target_id);
        self.renderer_correlations.lock().unwrap().remove(target_id);
        self.remove_nested_targets(target_id);
        if !self.debugger_targets.lock().unwrap().contains(target_id)
            && let Some(endpoint) = self.attachments.lock().await.remove(target_id)
        {
            endpoint.close().await;
        }
    }

    async fn set_wait_for_debugger_on_start(&self, enabled: bool) {
        if let Some(bridge) = self.bridge.lock().await.clone() {
            let _ = bridge.set_wait_for_debugger_on_start(enabled).await;
        }
    }

    async fn attach(&self, target_id: &str, force: bool) -> Result<TargetAttachment, String> {
        let _guard = self.attachment_guard.lock().await;
        if target_id == ROOT_TARGET_ID {
            return Ok(TargetAttachment {
                endpoint: self.root_endpoint.clone(),
                stole_external_owner: false,
            });
        }
        if let Some(endpoint) = self.attachments.lock().await.get(target_id).cloned()
            && endpoint.close_reason().await.is_none()
        {
            self.debugger_targets
                .lock()
                .unwrap()
                .insert(target_id.to_owned());
            return Ok(TargetAttachment {
                endpoint,
                stole_external_owner: false,
            });
        }
        if let Some(endpoint) = self.attachments.lock().await.remove(target_id) {
            self.supervised_targets.lock().unwrap().remove(target_id);
            self.renderer_correlations.lock().unwrap().remove(target_id);
            self.debugger_targets.lock().unwrap().remove(target_id);
            endpoint.close().await;
        }
        let nested = { self.nested_targets.lock().unwrap().get(target_id).cloned() };
        if let Some(nested) = nested {
            let parent = self
                .attachments
                .lock()
                .await
                .get(&nested.parent_target_id)
                .cloned()
                .ok_or_else(|| {
                    format!(
                        "parent target '{}' is not attached",
                        nested.parent_target_id
                    )
                })?;
            let endpoint = parent.attach_child(&nested.native_target_id).await?;
            self.attachments
                .lock()
                .await
                .insert(target_id.to_owned(), endpoint.clone());
            self.debugger_targets
                .lock()
                .unwrap()
                .insert(target_id.to_owned());
            return Ok(TargetAttachment {
                endpoint,
                stole_external_owner: false,
            });
        }
        let node = self.nodes.lock().unwrap().get(target_id).cloned();
        let (endpoint, stole_external_owner) = if let Some(node) = node {
            let url = match node.endpoint {
                Some(endpoint) => endpoint,
                None => {
                    let process_id = node.target.process_id.ok_or_else(|| {
                        format!("target '{target_id}' has no process to activate")
                    })?;
                    self.activate_node_endpoint(target_id, process_id).await?
                }
            };
            let transport = Arc::new(
                CdpWebSocketTransport::connect(&url)
                    .await
                    .map_err(|error| error.to_string())?,
            );
            (TargetEndpoint::open(transport)?, false)
        } else {
            let renderer = self
                .renderers
                .lock()
                .unwrap()
                .get(target_id)
                .cloned()
                .ok_or_else(|| format!("target '{target_id}' has no debuggable endpoint"))?;
            let bridge = self
                .bridge
                .lock()
                .await
                .clone()
                .ok_or_else(|| "Electron renderer bridge is unavailable".to_owned())?;
            // Startup blocks belong to the bridge and must be adopted through that same owner.
            if let Some(browser) = &self.browser_endpoint
                && !renderer.waiting_for_debugger
            {
                let targets = browser
                    .client()
                    .target()
                    .get_targets(None)
                    .await
                    .map_err(|error| format!("browser target discovery failed: {error:?}"))?;
                let mappings = bridge
                    .resolve_browser_targets(browser_page_target_ids(targets.target_infos))
                    .await
                    .map_err(|error| error.to_string())?;
                let native_id = select_browser_renderer_target(mappings, renderer.web_contents_id)?;
                let endpoint = browser.attach_child(&native_id).await?;
                self.native_target_aliases
                    .lock()
                    .unwrap()
                    .insert(native_id, target_id.to_owned());
                (endpoint, false)
            } else {
                let (transport, stolen) = bridge
                    .attach(target_id.to_owned(), &renderer, force)
                    .await?;
                (TargetEndpoint::open(transport)?, stolen)
            }
        };
        self.attachments
            .lock()
            .await
            .insert(target_id.to_owned(), endpoint.clone());
        self.debugger_targets
            .lock()
            .unwrap()
            .insert(target_id.to_owned());
        Ok(TargetAttachment {
            endpoint,
            stole_external_owner,
        })
    }

    async fn detach(&self, target_id: &str) {
        let _guard = self.attachment_guard.lock().await;
        if target_id == ROOT_TARGET_ID {
            // The main process endpoint is shared with the renderer bridge and outlives sessions.
            return;
        }
        self.debugger_targets.lock().unwrap().remove(target_id);
        if self.discovering.load(Ordering::Relaxed)
            && self.supervised_targets.lock().unwrap().contains(target_id)
        {
            return;
        }
        self.supervised_targets.lock().unwrap().remove(target_id);
        self.renderer_correlations.lock().unwrap().remove(target_id);
        self.remove_nested_targets(target_id);
        let endpoint = self.attachments.lock().await.remove(target_id);
        if let Some(endpoint) = endpoint {
            endpoint.close().await;
        }
    }

    async fn wait_closed(&self) -> String {
        self.root_endpoint.wait_closed().await
    }

    async fn close(&self) {
        if let Some(bridge) = self.bridge.lock().await.take() {
            bridge.dispose().await;
        }
        for (_, endpoint) in std::mem::take(&mut *self.attachments.lock().await) {
            endpoint.close().await;
        }
        if let Some(browser) = &self.browser_endpoint {
            browser.close().await;
        }
        self.root_endpoint.close().await;
        // Closing stdin is how `process_tree.mjs` learns to stop scanning and exit.
        self.control.lock().await.take();
        for task in std::mem::take(&mut *self.tasks.lock().unwrap()) {
            task.abort();
        }
    }
}

fn renderer_target_id(web_contents_id: u64) -> String {
    format!("{RENDERER_TARGET_PREFIX}{web_contents_id}")
}

fn browser_page_target_ids(targets: Vec<TargetTargetInfo>) -> Vec<String> {
    // Electron also maps OOPIF target IDs to their owner's webContents.
    targets
        .into_iter()
        .filter(|target| target.r#type == "page")
        .map(|target| target.target_id)
        .collect()
}

fn select_browser_renderer_target(
    mappings: BTreeMap<String, u64>,
    web_contents_id: u64,
) -> Result<String, String> {
    let mut matches = mappings
        .into_iter()
        .filter_map(|(id, contents_id)| (contents_id == web_contents_id).then_some(id));
    match (matches.next(), matches.next()) {
        (Some(id), None) => Ok(id),
        (None, _) => Err(format!(
            "Electron webContents {web_contents_id} has no live browser CDP page target"
        )),
        _ => Err(format!(
            "Electron webContents {web_contents_id} maps to multiple browser CDP page targets"
        )),
    }
}

fn nested_target_id(parent_target_id: &str, native_target_id: &str) -> String {
    format!("{parent_target_id}/target/{native_target_id}")
}

fn renderer_owns_nested_target(
    info: &TargetTargetInfo,
    renderer_id: &str,
    aliases: &std::sync::Mutex<BTreeMap<String, String>>,
) -> bool {
    info.r#type == "iframe"
        && info.parent_id.as_ref().is_some_and(|parent_id| {
            aliases.lock().unwrap().get(parent_id).is_some_and(|alias| {
                alias == renderer_id || alias.starts_with(&format!("{renderer_id}/target/"))
            })
        })
}

fn upsert_nested_target(
    nested_targets: &std::sync::Mutex<BTreeMap<String, NestedTargetRecord>>,
    events: &mpsc::UnboundedSender<TargetSourceEvent>,
    parent_target_id: &str,
    process_id: Option<u32>,
    native_target_aliases: &std::sync::Mutex<BTreeMap<String, String>>,
    renderers: &std::sync::Mutex<BTreeMap<String, ElectronRendererTarget>>,
    prefer_parent: bool,
    info: TargetTargetInfo,
) {
    let native_target_id = info.target_id.clone();
    let target_id = nested_target_id(parent_target_id, &native_target_id);
    if let Some(existing) = native_target_aliases
        .lock()
        .unwrap()
        .get(&native_target_id)
        .cloned()
    {
        if existing == target_id {
            // Continue so metadata updates replace the existing observation.
        } else if prefer_parent && nested_targets.lock().unwrap().remove(&existing).is_some() {
            let _ = events.send(TargetSourceEvent::Removed(existing));
        } else {
            return;
        }
    }
    native_target_aliases
        .lock()
        .unwrap()
        .insert(native_target_id.clone(), target_id.clone());
    let explicit_parent = info.parent_id.as_ref().or(info.opener_id.as_ref());
    let (parent_alias, opener_alias) = {
        let aliases = native_target_aliases.lock().unwrap();
        (
            explicit_parent.and_then(|id| aliases.get(id).cloned()),
            info.opener_id
                .as_ref()
                .and_then(|id| aliases.get(id).cloned()),
        )
    };
    let parent_id = explicit_parent
        .map(|id| {
            parent_alias
                .clone()
                .unwrap_or_else(|| nested_target_id(parent_target_id, id))
        })
        .unwrap_or_else(|| parent_target_id.to_owned());
    let process_id = parent_alias
        .as_ref()
        .and_then(|renderer_id| renderers.lock().unwrap().get(renderer_id).cloned())
        .map(|renderer| renderer.process_id)
        .or(process_id);
    let target = HostTarget {
        snapshot: TargetSnapshot {
            target_id: target_id.clone(),
            target_type: info.r#type,
            title: info.title,
            url: info.url,
            attached: info.attached,
            parent_id: Some(parent_id),
            opener_id: info
                .opener_id
                .map(|id| opener_alias.unwrap_or_else(|| nested_target_id(parent_target_id, &id))),
            browser_context_id: info.browser_context_id,
            subtype: info.subtype,
        },
        process_id,
        primary_window_id: None,
        waiting_for_debugger: false,
    };
    nested_targets.lock().unwrap().insert(
        target_id,
        NestedTargetRecord {
            parent_target_id: parent_target_id.to_owned(),
            native_target_id,
            target: target.clone(),
        },
    );
    let _ = events.send(TargetSourceEvent::Upserted(target));
}

fn remove_nested_target(
    nested_targets: &std::sync::Mutex<BTreeMap<String, NestedTargetRecord>>,
    native_target_aliases: &std::sync::Mutex<BTreeMap<String, String>>,
    events: &mpsc::UnboundedSender<TargetSourceEvent>,
    parent_target_id: &str,
    native_target_id: &str,
) {
    let target_id = nested_target_id(parent_target_id, native_target_id);
    if nested_targets.lock().unwrap().remove(&target_id).is_some() {
        native_target_aliases
            .lock()
            .unwrap()
            .retain(|_, alias| alias != &target_id);
        let _ = events.send(TargetSourceEvent::Removed(target_id));
    }
}

fn mark_nested_target_detached(
    nested_targets: &std::sync::Mutex<BTreeMap<String, NestedTargetRecord>>,
    events: &mpsc::UnboundedSender<TargetSourceEvent>,
    parent_target_id: &str,
    native_target_id: &str,
) {
    let target_id = nested_target_id(parent_target_id, native_target_id);
    let mut targets = nested_targets.lock().unwrap();
    if let Some(record) = targets.get_mut(&target_id) {
        record.target.snapshot.attached = false;
        let _ = events.send(TargetSourceEvent::Upserted(record.target.clone()));
    }
}

fn renderer_host_target(target: &ElectronRendererTarget) -> HostTarget {
    let target_id = renderer_target_id(target.web_contents_id);
    let parent_id = target
        .host_web_contents_id
        .map(renderer_target_id)
        .filter(|parent_id| parent_id != &target_id)
        .unwrap_or_else(|| ROOT_TARGET_ID.to_owned());
    HostTarget {
        snapshot: TargetSnapshot {
            target_id,
            target_type: "page".to_owned(),
            title: target.title.clone(),
            url: target.url.clone(),
            attached: target.attached,
            parent_id: Some(parent_id),
            opener_id: target.opener_web_contents_id.map(renderer_target_id),
            browser_context_id: None,
            subtype: Some("electron-renderer".to_owned()),
        },
        process_id: (target.process_id > 0).then_some(target.process_id),
        primary_window_id: target.primary_window_id,
        waiting_for_debugger: target.waiting_for_debugger,
    }
}

fn node_target_snapshot(
    target_id: String,
    parent_id: String,
    target_type: Option<String>,
    title: String,
    url: String,
) -> TargetSnapshot {
    let target_type = target_type.unwrap_or_else(|| "node".to_owned());
    let subtype = match target_type.as_str() {
        "node" => Some("child-process".to_owned()),
        "page" => Some("electron-renderer".to_owned()),
        _ => None,
    };
    TargetSnapshot {
        target_id,
        target_type,
        title,
        url,
        attached: false,
        parent_id: Some(parent_id),
        opener_id: None,
        browser_context_id: None,
        subtype,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn detached_nested_session_preserves_target_discovery_and_siblings() {
        let nested = std::sync::Mutex::new(BTreeMap::new());
        let aliases = std::sync::Mutex::new(BTreeMap::new());
        let renderers = std::sync::Mutex::new(BTreeMap::new());
        let (events, mut receiver) = mpsc::unbounded_channel();
        for native_id in ["iframe-a", "iframe-b"] {
            let mut info = target_info(native_id, None);
            info.attached = true;
            upsert_nested_target(
                &nested,
                &events,
                "renderer-1",
                Some(10),
                &aliases,
                &renderers,
                true,
                info,
            );
            assert!(matches!(
                receiver.try_recv(),
                Ok(TargetSourceEvent::Upserted(_))
            ));
        }
        mark_nested_target_detached(&nested, &events, "renderer-1", "iframe-a");
        let event = receiver.try_recv().unwrap();
        assert!(matches!(event, TargetSourceEvent::Upserted(target)
            if target.target_id() == "renderer-1/target/iframe-a" && !target.snapshot.attached));
        let nested = nested.lock().unwrap();
        assert!(
            !nested["renderer-1/target/iframe-a"]
                .target
                .snapshot
                .attached
        );
        assert!(
            nested["renderer-1/target/iframe-b"]
                .target
                .snapshot
                .attached
        );
        assert_eq!(
            aliases.lock().unwrap()["iframe-a"],
            "renderer-1/target/iframe-a"
        );
    }

    #[test]
    fn browser_renderer_selection_excludes_oopifs_and_rejects_ambiguous_identity() {
        let mut page = target_info("workbench", None);
        page.r#type = "page".to_owned();
        assert_eq!(
            browser_page_target_ids(vec![target_info("webview-frame", Some("workbench")), page]),
            vec!["workbench"]
        );
        assert_eq!(
            select_browser_renderer_target(BTreeMap::from([("workbench".to_owned(), 5)]), 5),
            Ok("workbench".to_owned())
        );
        assert!(
            select_browser_renderer_target(BTreeMap::new(), 5)
                .unwrap_err()
                .contains("no live")
        );
        assert!(
            select_browser_renderer_target(
                BTreeMap::from([("a".to_owned(), 5), ("b".to_owned(), 5)]),
                5,
            )
            .unwrap_err()
            .contains("multiple")
        );
    }

    fn target_info(target_id: &str, parent_id: Option<&str>) -> TargetTargetInfo {
        let mut info = TargetTargetInfo::new(
            target_id.to_owned(),
            "iframe".to_owned(),
            "frame".to_owned(),
            "https://example.com".to_owned(),
            false,
            false,
        );
        info.parent_id = parent_id.map(str::to_owned);
        info
    }

    #[test]
    fn renderer_child_discovery_excludes_unrelated_windows_and_page_targets() {
        let aliases = std::sync::Mutex::new(BTreeMap::from([
            ("native-a".to_owned(), "renderer-a".to_owned()),
            ("native-b".to_owned(), "renderer-b".to_owned()),
            ("child-a".to_owned(), "renderer-a/target/child-a".to_owned()),
        ]));
        assert!(renderer_owns_nested_target(
            &target_info("child-a", Some("native-a")),
            "renderer-a",
            &aliases,
        ));
        assert!(renderer_owns_nested_target(
            &target_info("grandchild-a", Some("child-a")),
            "renderer-a",
            &aliases,
        ));
        assert!(!renderer_owns_nested_target(
            &target_info("child-b", Some("native-b")),
            "renderer-a",
            &aliases,
        ));
        let mut unrelated_page = target_info("page-b", Some("native-a"));
        unrelated_page.r#type = "page".to_owned();
        assert!(!renderer_owns_nested_target(
            &unrelated_page,
            "renderer-a",
            &aliases
        ));
    }

    #[test]
    fn native_renderer_target_is_not_published_twice() {
        let nested = std::sync::Mutex::new(BTreeMap::new());
        let aliases = std::sync::Mutex::new(BTreeMap::from([(
            "native-page".to_owned(),
            "renderer-2".to_owned(),
        )]));
        let renderers = std::sync::Mutex::new(BTreeMap::new());
        let (events, mut receiver) = mpsc::unbounded_channel();

        upsert_nested_target(
            &nested,
            &events,
            "browser",
            Some(1),
            &aliases,
            &renderers,
            false,
            target_info("native-page", None),
        );

        assert!(nested.lock().unwrap().is_empty());
        assert!(receiver.try_recv().is_err());
    }

    #[test]
    fn native_child_uses_the_correlated_renderer_as_its_parent() {
        let nested = std::sync::Mutex::new(BTreeMap::new());
        let aliases = std::sync::Mutex::new(BTreeMap::from([(
            "native-page".to_owned(),
            "renderer-2".to_owned(),
        )]));
        let renderers = std::sync::Mutex::new(BTreeMap::from([(
            "renderer-2".to_owned(),
            ElectronRendererTarget {
                web_contents_id: 2,
                primary_window_id: None,
                process_id: 77,
                host_web_contents_id: None,
                opener_web_contents_id: None,
                target_type: "window".to_owned(),
                title: "Workbench".to_owned(),
                url: "vscode-file://workbench".to_owned(),
                waiting_for_debugger: false,
                attached: false,
            },
        )]));
        let (events, mut receiver) = mpsc::unbounded_channel();

        upsert_nested_target(
            &nested,
            &events,
            "browser",
            Some(1),
            &aliases,
            &renderers,
            false,
            target_info("native-frame", Some("native-page")),
        );

        let TargetSourceEvent::Upserted(target) = receiver.try_recv().unwrap() else {
            panic!("expected target upsert");
        };
        assert_eq!(target.snapshot.parent_id.as_deref(), Some("renderer-2"));
        assert_eq!(target.process_id, Some(77));
    }

    #[test]
    fn renderer_related_route_replaces_a_browser_global_alias() {
        let nested = std::sync::Mutex::new(BTreeMap::new());
        let aliases = std::sync::Mutex::new(BTreeMap::new());
        let renderers = std::sync::Mutex::new(BTreeMap::new());
        let (events, mut receiver) = mpsc::unbounded_channel();

        upsert_nested_target(
            &nested,
            &events,
            "browser",
            Some(1),
            &aliases,
            &renderers,
            false,
            target_info("native-frame", None),
        );
        let TargetSourceEvent::Upserted(browser_target) = receiver.try_recv().unwrap() else {
            panic!("expected browser target upsert");
        };

        upsert_nested_target(
            &nested,
            &events,
            "renderer-2",
            Some(77),
            &aliases,
            &renderers,
            true,
            target_info("native-frame", None),
        );

        let TargetSourceEvent::Removed(removed) = receiver.try_recv().unwrap() else {
            panic!("expected browser alias removal");
        };
        assert_eq!(removed, browser_target.snapshot.target_id);
        let TargetSourceEvent::Upserted(renderer_target) = receiver.try_recv().unwrap() else {
            panic!("expected renderer target upsert");
        };
        assert_eq!(
            renderer_target.snapshot.target_id,
            "renderer-2/target/native-frame"
        );
        assert_eq!(
            aliases
                .lock()
                .unwrap()
                .get("native-frame")
                .map(String::as_str),
            Some("renderer-2/target/native-frame")
        );
        assert_eq!(nested.lock().unwrap().len(), 1);
    }

    #[test]
    fn web_contents_host_and_opener_are_preserved_as_target_relations() {
        let target = renderer_host_target(&ElectronRendererTarget {
            web_contents_id: 3,
            primary_window_id: Some(7),
            process_id: 77,
            host_web_contents_id: Some(2),
            opener_web_contents_id: Some(1),
            target_type: "webview".to_owned(),
            title: "Webview".to_owned(),
            url: "vscode-webview://example".to_owned(),
            waiting_for_debugger: false,
            attached: false,
        });

        assert_eq!(target.snapshot.parent_id.as_deref(), Some("renderer-2"));
        assert_eq!(target.snapshot.opener_id.as_deref(), Some("renderer-1"));
        assert_eq!(target.primary_window_id, Some(7));
    }
}
