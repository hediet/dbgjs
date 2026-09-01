//! The [`TargetSource`] behind a connected OS process tree.
//!
//! It combines three discovery mechanisms into one CDP `Target` domain: the OS descendant scan
//! performed by `providers/process_tree.mjs` (Node.js child processes and Chromium DevTools
//! endpoints), the Electron main process itself, and - when the root process is an Electron app -
//! the renderer targets reported by the main-process bridge. Browser roots and renderer endpoints
//! recursively contribute their own `Target` inventory. Every Electron and WebContents detail
//! stays inside this module and [`crate::electron_renderer_transport`].

use std::collections::{BTreeMap, BTreeSet};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};

use async_trait::async_trait;
use tokio::io::AsyncWriteExt;
use tokio::process::ChildStdin;
use tokio::sync::{Mutex, mpsc, oneshot};
use tokio::task::JoinHandle;

use crate::cdp::{
    TargetGetTargetsParams, TargetSetDiscoverTargetsParams, TargetTargetCreatedParams,
    TargetTargetDestroyedParams, TargetTargetInfo, TargetTargetInfoChangedParams,
};
use crate::connection_provider::ProviderEvent;
use crate::electron_renderer_transport::{
    BridgeEvent, ElectronRendererBridge, ElectronRendererTarget,
};
use crate::service_api::TargetSnapshot;
use crate::virtual_browser_root::{
    HostTarget, TargetAttachment, TargetEndpoint, TargetSource, TargetSourceEvent,
};
use crate::websocket_transport::CdpWebSocketTransport;

/// The CDP target id of the process the connection was made to. The debugger service canonicalizes
/// it into its own synthetic per-connection id.
pub const ROOT_TARGET_ID: &str = "$node-root";

const RENDERER_TARGET_PREFIX: &str = "renderer-";

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
    bridge: Mutex<Option<Arc<ElectronRendererBridge>>>,
    control: Mutex<Option<ChildStdin>>,
    events: mpsc::UnboundedSender<TargetSourceEvent>,
    nodes: std::sync::Mutex<BTreeMap<String, NodeRecord>>,
    renderers: std::sync::Mutex<BTreeMap<String, ElectronRendererTarget>>,
    nested_targets: Arc<std::sync::Mutex<BTreeMap<String, NestedTargetRecord>>>,
    supervised_targets: Arc<std::sync::Mutex<BTreeSet<String>>>,
    attachments: Mutex<BTreeMap<String, Arc<TargetEndpoint>>>,
    next_scan_id: AtomicU64,
    scans: std::sync::Mutex<BTreeMap<u64, oneshot::Sender<()>>>,
    discovering: AtomicBool,
    tasks: std::sync::Mutex<Vec<JoinHandle<()>>>,
}

impl ProcessTreeTargetSource {
    /// Opens the shared main-process endpoint, installs the Electron bridge when the root process
    /// is an Electron main process, and starts pumping provider and bridge discovery events.
    pub(crate) async fn start(
        root_pid: u32,
        root_endpoint_url: &str,
        control: ChildStdin,
        provider_events: mpsc::UnboundedReceiver<ProviderEvent>,
    ) -> Result<(Arc<Self>, mpsc::UnboundedReceiver<TargetSourceEvent>), String> {
        let transport = Arc::new(
            CdpWebSocketTransport::connect(root_endpoint_url)
                .await
                .map_err(|error| error.to_string())?,
        );
        let root_endpoint = TargetEndpoint::open(transport)?;
        let (events, receiver) = mpsc::unbounded_channel();
        let source = Arc::new(Self {
            root_pid,
            root_endpoint: root_endpoint.clone(),
            bridge: Mutex::new(None),
            control: Mutex::new(Some(control)),
            events,
            nodes: std::sync::Mutex::new(BTreeMap::new()),
            renderers: std::sync::Mutex::new(BTreeMap::new()),
            nested_targets: Arc::new(std::sync::Mutex::new(BTreeMap::new())),
            supervised_targets: Arc::new(std::sync::Mutex::new(BTreeSet::new())),
            attachments: Mutex::new(BTreeMap::new()),
            next_scan_id: AtomicU64::new(1),
            scans: std::sync::Mutex::new(BTreeMap::new()),
            discovering: AtomicBool::new(false),
            tasks: std::sync::Mutex::new(Vec::new()),
        });
        source.supervise_provider_events(provider_events);
        match ElectronRendererBridge::install(root_endpoint.client()).await {
            Ok(bridge) => {
                source.supervise_bridge_events(&bridge).await;
                *source.bridge.lock().await = Some(bridge);
            }
            Err(error) => {
                // A plain Node.js root process has no renderers; that is not an error.
                eprintln!("Electron renderer bridge is unavailable: {error}");
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
                        let discovers_children =
                            target_type.as_deref() == Some("browser") && endpoint.is_some();
                        let target = HostTarget {
                            snapshot: node_target_snapshot(
                                target_id.clone(),
                                parent_target_id,
                                target_type,
                                title,
                                url,
                            ),
                            process_id,
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
                        if discovers_children && source.discovering.load(Ordering::Relaxed) {
                            let nested_source = source.clone();
                            source.track(tokio::spawn(async move {
                                nested_source.start_nested_discovery(&target_id).await;
                            }));
                        }
                    }
                    ProviderEvent::NodeTargetRemoved { target_id } => {
                        source.nodes.lock().unwrap().remove(&target_id);
                        source.remove_nested_targets(&target_id);
                        source.supervised_targets.lock().unwrap().remove(&target_id);
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
                        if source.discovering.load(Ordering::Relaxed) {
                            let nested_source = source.clone();
                            source.track(tokio::spawn(async move {
                                nested_source.start_nested_discovery(&target_id).await;
                            }));
                        }
                    }
                    BridgeEvent::TargetDestroyed { web_contents_id } => {
                        let target_id = renderer_target_id(web_contents_id);
                        source.renderers.lock().unwrap().remove(&target_id);
                        source.remove_nested_targets(&target_id);
                        source.supervised_targets.lock().unwrap().remove(&target_id);
                        if let Some(endpoint) = source.attachments.lock().await.remove(&target_id) {
                            endpoint.close().await;
                        }
                        let _ = source.events.send(TargetSourceEvent::Removed(target_id));
                    }
                }
            }
        }));
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
                    .target_set_discover_targets(TargetSetDiscoverTargetsParams::new(true))
                    .await;
            }
            return;
        }
        let attachment = match self.attach(parent_target_id, false).await {
            Ok(attachment) => attachment,
            Err(error) => {
                self.supervised_targets
                    .lock()
                    .unwrap()
                    .remove(parent_target_id);
                eprintln!("nested target discovery is unavailable for {parent_target_id}: {error}");
                return;
            }
        };
        let endpoint = attachment.endpoint;
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
            .target_set_discover_targets(TargetSetDiscoverTargetsParams::new(true))
            .await
            .is_err()
        {
            self.supervised_targets
                .lock()
                .unwrap()
                .remove(parent_target_id);
            return;
        }
        if let Ok(targets) = endpoint
            .client()
            .target_get_targets(TargetGetTargetsParams::new())
            .await
        {
            for target in targets.target_infos {
                upsert_nested_target(
                    &self.nested_targets,
                    &self.events,
                    parent_target_id,
                    process_id,
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
            let mut renderer_ids = self
                .renderers
                .lock()
                .unwrap()
                .keys()
                .cloned()
                .collect::<Vec<_>>();
            renderer_ids.extend(
                self.nodes
                    .lock()
                    .unwrap()
                    .iter()
                    .filter(|(_, record)| {
                        record.endpoint.is_some() && record.target.snapshot.target_type == "browser"
                    })
                    .map(|(id, _)| id.clone()),
            );
            for renderer_id in renderer_ids {
                self.start_nested_discovery(&renderer_id).await;
            }
        } else {
            let renderer_ids = self
                .supervised_targets
                .lock()
                .unwrap()
                .iter()
                .cloned()
                .collect::<Vec<_>>();
            for renderer_id in renderer_ids {
                if let Some(endpoint) = self.attachments.lock().await.get(&renderer_id).cloned() {
                    let _ = endpoint
                        .client()
                        .target_set_discover_targets(TargetSetDiscoverTargetsParams::new(false))
                        .await;
                }
            }
        }
    }

    async fn set_wait_for_debugger_on_start(&self, enabled: bool) {
        if let Some(bridge) = self.bridge.lock().await.clone() {
            let _ = bridge.set_wait_for_debugger_on_start(enabled).await;
        }
    }

    async fn attach(&self, target_id: &str, force: bool) -> Result<TargetAttachment, String> {
        if target_id == ROOT_TARGET_ID {
            return Ok(TargetAttachment {
                endpoint: self.root_endpoint.clone(),
                stole_external_owner: false,
            });
        }
        if let Some(endpoint) = self.attachments.lock().await.get(target_id).cloned() {
            return Ok(TargetAttachment {
                endpoint,
                stole_external_owner: false,
            });
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
            return Ok(TargetAttachment {
                endpoint,
                stole_external_owner: false,
            });
        }
        let node_endpoint = self
            .nodes
            .lock()
            .unwrap()
            .get(target_id)
            .and_then(|record| record.endpoint.clone());
        let (endpoint, stole_external_owner) = if let Some(url) = node_endpoint {
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
            let (transport, stolen) = bridge
                .attach(target_id.to_owned(), &renderer, force)
                .await?;
            (TargetEndpoint::open(transport)?, stolen)
        };
        self.attachments
            .lock()
            .await
            .insert(target_id.to_owned(), endpoint.clone());
        Ok(TargetAttachment {
            endpoint,
            stole_external_owner,
        })
    }

    async fn detach(&self, target_id: &str) {
        if target_id == ROOT_TARGET_ID {
            // The main process endpoint is shared with the renderer bridge and outlives sessions.
            return;
        }
        if self.supervised_targets.lock().unwrap().contains(target_id) {
            return;
        }
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

fn nested_target_id(parent_target_id: &str, native_target_id: &str) -> String {
    format!("{parent_target_id}/target/{native_target_id}")
}

fn upsert_nested_target(
    nested_targets: &std::sync::Mutex<BTreeMap<String, NestedTargetRecord>>,
    events: &mpsc::UnboundedSender<TargetSourceEvent>,
    parent_target_id: &str,
    process_id: Option<u32>,
    info: TargetTargetInfo,
) {
    let native_target_id = info.target_id.clone();
    let target_id = nested_target_id(parent_target_id, &native_target_id);
    let parent_id = info
        .parent_id
        .as_ref()
        .map(|id| nested_target_id(parent_target_id, id))
        .unwrap_or_else(|| parent_target_id.to_owned());
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
                .map(|id| nested_target_id(parent_target_id, &id)),
            browser_context_id: info.browser_context_id,
            subtype: info.subtype,
        },
        process_id,
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
    events: &mpsc::UnboundedSender<TargetSourceEvent>,
    parent_target_id: &str,
    native_target_id: &str,
) {
    let target_id = nested_target_id(parent_target_id, native_target_id);
    if nested_targets.lock().unwrap().remove(&target_id).is_some() {
        let _ = events.send(TargetSourceEvent::Removed(target_id));
    }
}

fn renderer_host_target(target: &ElectronRendererTarget) -> HostTarget {
    HostTarget {
        snapshot: TargetSnapshot {
            target_id: renderer_target_id(target.web_contents_id),
            target_type: "page".to_owned(),
            title: target.title.clone(),
            url: target.url.clone(),
            attached: target.attached,
            parent_id: Some(ROOT_TARGET_ID.to_owned()),
            opener_id: None,
            browser_context_id: None,
            subtype: Some("electron-renderer".to_owned()),
        },
        process_id: (target.process_id > 0).then_some(target.process_id),
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
