//! `LinkRpcConnection`: method-name routing + a live interface registry on top of a [`Channel`].
//!
//! Layers the linkrpc `::` addressing grammar and an interface registry over a plain JSON-RPC
//! channel. Inbound requests/notifications are routed to the registered [`InterfaceHandler`] for
//! `(serviceId, interfaceId)`; outbound calls go straight to the channel. Registering any
//! interface also lets you opt into the three reflection interfaces via [`LinkRpcConnection::enable_reflection`].
//!
//! Mirrors TS `connection/linkRpcConnection.ts` (non-streaming subset for this milestone).

use std::sync::{Arc, RwLock};

use async_trait::async_trait;
use serde_json::json;

use crate::connection::channel::{Channel, RequestHandler};
use crate::connection::dispatch::{CallCtx, InterfaceHandler, ServiceExport};
use crate::connection::interface_def::{InterfaceDefinition, Member};
use crate::connection::reflection::iface::{
    BareBindingListing, DefaultsGetResult, DefaultsListBindingsResult, DefaultsService,
    DefaultsServiceServer, DirectoryListResult, DirectoryService, DirectoryServiceServer,
    SchemasGetResult, SchemasService, SchemasServiceServer, ServiceListing,
};
use crate::connection::reflection::DEFAULTS_ID;
use crate::protocol::json_value::JsonValue;
use crate::protocol::jsonrpc::{error_codes, JsonRpcError};
use crate::protocol::method_name::{parse_method_name, ParsedMethodName};
use crate::transport::message::MessageTransport;

/// Errors from registry mutations.
#[derive(Debug, thiserror::Error)]
pub enum ConnError {
    #[error("interface \"{interface_id}\" already registered{}", .service.as_deref().map(|s| format!(" under service \"{s}\"")).unwrap_or_default())]
    AlreadyRegistered {
        interface_id: String,
        service: Option<String>,
    },
    #[error("setPreset: interface \"{0}\" is not registered under the root service")]
    PresetNotRegistered(String),
    #[error("bindBare: prefix must contain only printable ASCII and must not contain \"::\"")]
    InvalidBarePrefix,
    #[error("bindBare: prefix \"{0}\" is already bound")]
    BarePrefixAlreadyBound(String),
    #[error("bindBare: interface \"{interface_id}\" is not registered{}", .service.as_deref().map(|s| format!(" under service \"{s}\"")).unwrap_or_else(|| " under the root service".to_string()))]
    BareTargetNotRegistered {
        interface_id: String,
        service: Option<String>,
    },
}

/// Options for [`LinkRpcConnection::register`].
#[derive(Debug, Clone, Default)]
pub struct RegisterOptions {
    /// Mount the interface under this service id (form-3 addressing). Omit for the root service.
    pub service_id: Option<String>,
    /// Optional human description recorded for `service_id`, surfaced via the directory.
    pub service_description: Option<String>,
}

#[derive(Clone)]
struct RegisteredInterface {
    iface: Arc<InterfaceDefinition>,
    handler: Arc<dyn InterfaceHandler>,
    service_id: Option<String>,
    service_description: Option<String>,
}

impl RegisteredInterface {
    fn key(&self) -> String {
        registry_key(self.service_id.as_deref(), self.iface.id())
    }
}

#[derive(Clone)]
struct BareBinding {
    prefix: String,
    service_id: Option<String>,
    interface_id: String,
    hash: String,
}

#[derive(Default)]
struct RegistryInner {
    entries: RwLock<Vec<RegisteredInterface>>,
    bare_bindings: RwLock<Vec<BareBinding>>,
}

fn registry_key(service_id: Option<&str>, interface_id: &str) -> String {
    format!("{}::{}", service_id.unwrap_or(""), interface_id)
}

impl RegistryInner {
    fn find(&self, service_id: Option<&str>, interface_id: &str) -> Option<RegisteredInterface> {
        let key = registry_key(service_id, interface_id);
        self.entries
            .read()
            .unwrap()
            .iter()
            .find(|e| e.key() == key)
            .cloned()
    }

    fn find_by_id(
        &self,
        interface_id: &str,
        hash: Option<&str>,
    ) -> Option<Arc<InterfaceDefinition>> {
        self.entries.read().unwrap().iter().find_map(|e| {
            if e.iface.id() != interface_id {
                return None;
            }
            if let Some(h) = hash {
                if e.iface.schema_hash() != h {
                    return None;
                }
            }
            Some(e.iface.clone())
        })
    }
}

/// A high-level linkrpc connection.
#[derive(Clone)]
pub struct LinkRpcConnection {
    channel: Channel,
    inner: Arc<RegistryInner>,
}

impl LinkRpcConnection {
    /// Build a connection over `transport`, binding the inbound dispatch handler.
    pub fn new(transport: Box<dyn MessageTransport>) -> Self {
        let inner = Arc::new(RegistryInner::default());
        let handler = ConnectionDispatch {
            inner: inner.clone(),
        };
        let channel = Channel::new(transport, Box::new(handler));
        LinkRpcConnection { channel, inner }
    }

    /// The underlying JSON-RPC channel (raw access for callers that bypass the registry).
    pub fn channel(&self) -> &Channel {
        &self.channel
    }

    /// Drive the inbound read loop until the transport closes. Spawn on your executor.
    pub async fn run(&self) {
        self.channel.run().await;
    }

    /// Register an interface's server-side handler.
    pub fn register(
        &self,
        iface: Arc<InterfaceDefinition>,
        handler: Arc<dyn InterfaceHandler>,
        opts: RegisterOptions,
    ) -> Result<(), ConnError> {
        let entry = RegisteredInterface {
            iface,
            handler,
            service_id: opts.service_id.clone(),
            service_description: opts.service_description,
        };
        let key = entry.key();
        let mut entries = self.inner.entries.write().unwrap();
        if entries.iter().any(|e| e.key() == key) {
            return Err(ConnError::AlreadyRegistered {
                interface_id: entry.iface.id().to_string(),
                service: opts.service_id,
            });
        }
        entries.push(entry);
        Ok(())
    }

    /// Register a service from its handler alone, deriving the [`InterfaceDefinition`] via
    /// [`ServiceExport`]. Convenience over [`register`](Self::register) for macro-generated
    /// `…Server` adapters: no separate `interface()` argument needed.
    pub fn register_service<S>(
        &self,
        service: Arc<S>,
        opts: RegisterOptions,
    ) -> Result<(), ConnError>
    where
        S: ServiceExport + 'static,
    {
        self.register(Arc::new(S::interface()), service, opts)
    }

    /// Declare the preset interface for form-1 (bare-method) dispatch. Must already be registered
    /// under the root service (no service id). Surfaced via `hubrpc.defaults::get`.
    pub fn set_preset(&self, interface_id: &str) -> Result<(), ConnError> {
        let entry = self
            .inner
            .find(None, interface_id)
            .ok_or_else(|| ConnError::PresetNotRegistered(interface_id.to_string()))?;
        let binding = BareBinding {
            prefix: String::new(),
            service_id: None,
            interface_id: entry.iface.id().to_string(),
            hash: entry.iface.schema_hash().to_string(),
        };
        let mut bindings = self.inner.bare_bindings.write().unwrap();
        bindings.retain(|b| !b.prefix.is_empty());
        bindings.push(binding);
        Ok(())
    }

    /// Bind bare wire methods beginning with `prefix` to a registered interface.
    ///
    /// The prefix may be empty, must consist only of printable ASCII, and must not contain `::`.
    /// The target must be registered before it is bound. Prefixes are unique; use
    /// [`unbind_bare`](Self::unbind_bare) before replacing an explicit binding.
    pub fn bind_bare(
        &self,
        prefix: &str,
        service_id: Option<&str>,
        interface_id: &str,
    ) -> Result<(), ConnError> {
        validate_bare_prefix(prefix)?;
        let entry = self.inner.find(service_id, interface_id).ok_or_else(|| {
            ConnError::BareTargetNotRegistered {
                interface_id: interface_id.to_string(),
                service: service_id.map(str::to_string),
            }
        })?;
        let mut bindings = self.inner.bare_bindings.write().unwrap();
        if bindings.iter().any(|b| b.prefix == prefix) {
            return Err(ConnError::BarePrefixAlreadyBound(prefix.to_string()));
        }
        bindings.push(BareBinding {
            prefix: prefix.to_string(),
            service_id: service_id.map(str::to_string),
            interface_id: interface_id.to_string(),
            hash: entry.iface.schema_hash().to_string(),
        });
        Ok(())
    }

    /// Remove the bare-method binding for `prefix`, returning whether one existed.
    pub fn unbind_bare(&self, prefix: &str) -> bool {
        let mut bindings = self.inner.bare_bindings.write().unwrap();
        let old_len = bindings.len();
        bindings.retain(|b| b.prefix != prefix);
        bindings.len() != old_len
    }

    /// Register the reflection interfaces (`defaults`, `directory`, `schemas`) backed by
    /// this connection's live registry. Idempotent.
    pub fn enable_reflection(&self) {
        if self.inner.find(None, DEFAULTS_ID).is_some() {
            return;
        }
        let reflection = Arc::new(Reflection {
            inner: self.inner.clone(),
        });
        // Unwrap is safe: idempotency guard above guarantees these keys are free.
        self.register_service(
            Arc::new(DefaultsServiceServer::new(reflection.clone())),
            RegisterOptions::default(),
        )
        .unwrap();
        self.register_service(
            Arc::new(DirectoryServiceServer::new(reflection.clone())),
            RegisterOptions::default(),
        )
        .unwrap();
        self.register_service(
            Arc::new(SchemasServiceServer::new(reflection)),
            RegisterOptions::default(),
        )
        .unwrap();
    }

    /// Snapshot of registered interfaces, in registration order.
    pub fn list_registered(&self) -> Vec<RegisteredListing> {
        self.inner
            .entries
            .read()
            .unwrap()
            .iter()
            .map(|e| RegisteredListing {
                service_id: e.service_id.clone().unwrap_or_default(),
                interface_id: e.iface.id().to_string(),
                interface_hash: e.iface.schema_hash().to_string(),
                service_description: e.service_description.clone(),
            })
            .collect()
    }

    // ---- client side ----

    /// Invoke a request method by raw wire name (e.g. `"com.acme.pizza::order"`).
    pub async fn call(&self, method: &str, params: JsonValue) -> Result<JsonValue, JsonRpcError> {
        self.channel.call(method, params).await
    }

    /// Send a notification by raw wire name.
    pub async fn notify(&self, method: &str, params: JsonValue) -> Result<(), JsonRpcError> {
        self.channel.notify(method, params).await
    }

    /// Invoke a request member, building the wire method name from its address.
    pub async fn call_member(
        &self,
        service_id: Option<&str>,
        interface_id: &str,
        member: &str,
        params: JsonValue,
    ) -> Result<JsonValue, JsonRpcError> {
        self.call(&wire_method(service_id, interface_id, member), params)
            .await
    }

    /// Send a notification member, building the wire method name from its address.
    pub async fn notify_member(
        &self,
        service_id: Option<&str>,
        interface_id: &str,
        member: &str,
        params: JsonValue,
    ) -> Result<(), JsonRpcError> {
        self.notify(&wire_method(service_id, interface_id, member), params)
            .await
    }
}

fn validate_bare_prefix(prefix: &str) -> Result<(), ConnError> {
    if prefix.contains("::") || !prefix.bytes().all(|byte| (b' '..=b'~').contains(&byte)) {
        return Err(ConnError::InvalidBarePrefix);
    }
    Ok(())
}

fn wire_method(service_id: Option<&str>, interface_id: &str, member: &str) -> String {
    match service_id {
        Some(sid) => format!("{sid}::{interface_id}::{member}"),
        None => format!("{interface_id}::{member}"),
    }
}

/// A directory listing entry (see [`LinkRpcConnection::list_registered`]).
#[derive(Debug, Clone)]
pub struct RegisteredListing {
    pub service_id: String,
    pub interface_id: String,
    pub interface_hash: String,
    pub service_description: Option<String>,
}

// ── inbound dispatch ──────────────────────────────────────────────────────────

struct Routed {
    entry: RegisteredInterface,
    member: String,
}

impl RegistryInner {
    fn route(&self, method: &str) -> Result<Routed, JsonRpcError> {
        let Some(parsed) = parse_method_name(method) else {
            return Err(not_found("bad-method-grammar", method));
        };
        match parsed {
            ParsedMethodName::Bare { member } => {
                let selected = self
                    .bare_bindings
                    .read()
                    .unwrap()
                    .iter()
                    .filter(|binding| member.starts_with(&binding.prefix))
                    .max_by_key(|binding| binding.prefix.len())
                    .cloned();
                let Some(binding) = selected else {
                    return Err(not_found("no-preset", method));
                };
                let Some(entry) = self.find(binding.service_id.as_deref(), &binding.interface_id)
                else {
                    return Err(not_found("no-preset", method));
                };
                Ok(Routed {
                    entry,
                    member: member[binding.prefix.len()..].to_string(),
                })
            }
            ParsedMethodName::Interface {
                interface_id,
                member,
            } => {
                let Some(entry) = self.find(None, &interface_id) else {
                    return Err(not_found("unknown-interface", method));
                };
                Ok(Routed { entry, member })
            }
            ParsedMethodName::Full {
                service_id,
                interface_id,
                member,
            } => {
                let Some(entry) = self.find(Some(&service_id), &interface_id) else {
                    return Err(not_found("unknown-service", method));
                };
                Ok(Routed { entry, member })
            }
        }
    }
}

struct ConnectionDispatch {
    inner: Arc<RegistryInner>,
}

#[async_trait]
impl RequestHandler for ConnectionDispatch {
    async fn handle_request(
        &self,
        method: String,
        params: JsonValue,
    ) -> Result<JsonValue, JsonRpcError> {
        let routed = self.inner.route(&method)?;
        match routed.entry.iface.member(&routed.member) {
            Some(Member::Request(_)) => {}
            // Request semantics used on a notification-only method, or unknown member.
            Some(Member::Notification(_)) | None => {
                return Err(not_found("unknown-method", &method))
            }
        }
        routed
            .entry
            .handler
            .handle_request(&routed.member, params, CallCtx::default())
            .await
    }

    async fn handle_notification(&self, method: String, params: JsonValue) {
        let Ok(routed) = self.inner.route(&method) else {
            return;
        };
        if !matches!(
            routed.entry.iface.member(&routed.member),
            Some(Member::Notification(_))
        ) {
            return;
        }
        routed
            .entry
            .handler
            .handle_notification(&routed.member, params, CallCtx::default())
            .await;
    }
}

fn not_found(reason: &str, method: &str) -> JsonRpcError {
    JsonRpcError {
        code: error_codes::METHOD_NOT_FOUND,
        message: format!("method not found: {method}"),
        data: Some(json!({ "reason": reason, "method": method })),
    }
}

// ── reflection (backed by the live registry) ─────────────────────────────────

/// Registry-backed implementation of the three reflection interfaces. Served through the
/// macro-generated `*Server` adapters in [`LinkRpcConnection::enable_reflection`].
struct Reflection {
    inner: Arc<RegistryInner>,
}

#[async_trait]
impl DefaultsService for Reflection {
    async fn get(&self, _ctx: &CallCtx) -> Result<DefaultsGetResult, JsonRpcError> {
        let binding = self
            .inner
            .bare_bindings
            .read()
            .unwrap()
            .iter()
            .find(|binding| binding.prefix.is_empty())
            .cloned();
        Ok(match binding {
            Some(binding) => DefaultsGetResult {
                service_id: binding.service_id,
                interface_id: Some(binding.interface_id),
                interface_hash: Some(binding.hash),
            },
            None => DefaultsGetResult {
                service_id: None,
                interface_id: None,
                interface_hash: None,
            },
        })
    }

    #[allow(non_snake_case)]
    async fn listBindings(
        &self,
        _ctx: &CallCtx,
    ) -> Result<DefaultsListBindingsResult, JsonRpcError> {
        let mut bindings: Vec<BareBindingListing> = self
            .inner
            .bare_bindings
            .read()
            .unwrap()
            .iter()
            .map(|binding| BareBindingListing {
                prefix: binding.prefix.clone(),
                service_id: binding.service_id.clone(),
                interface_id: binding.interface_id.clone(),
                interface_hash: binding.hash.clone(),
            })
            .collect();
        bindings.sort_by(|a, b| a.prefix.cmp(&b.prefix));
        Ok(DefaultsListBindingsResult { bindings })
    }
}

#[async_trait]
impl DirectoryService for Reflection {
    async fn list(
        &self,
        _ctx: &CallCtx,
        interface_id: Option<String>,
        service_id: Option<String>,
        cursor: Option<String>,
        limit: Option<u32>,
        _timeout_ms: Option<u32>,
    ) -> Result<DirectoryListResult, JsonRpcError> {
        let filter_iface = interface_id.as_deref();
        let filter_service = service_id.as_deref();
        let cursor = cursor
            .as_deref()
            .and_then(|s| s.parse::<usize>().ok())
            .unwrap_or(0);
        let limit = limit.map(|n| n as usize);

        let all: Vec<ServiceListing> = self
            .inner
            .entries
            .read()
            .unwrap()
            .iter()
            .filter(|e| filter_iface.is_none_or(|f| e.iface.id() == f))
            .filter(|e| filter_service.is_none_or(|f| e.service_id.as_deref().unwrap_or("") == f))
            .map(|e| ServiceListing {
                service_id: e.service_id.clone().unwrap_or_default(),
                interface_id: e.iface.id().to_string(),
                interface_hash: e.iface.schema_hash().to_string(),
                service_description: e.service_description.clone(),
            })
            .collect();

        let start = cursor.min(all.len());
        let end = match limit {
            Some(l) => (start + l).min(all.len()),
            None => all.len(),
        };
        let items = all[start..end].to_vec();
        let next_cursor = (end < all.len()).then(|| end.to_string());
        Ok(DirectoryListResult {
            items,
            next_cursor,
            truncated: None,
        })
    }
}

#[async_trait]
impl SchemasService for Reflection {
    async fn get(
        &self,
        _ctx: &CallCtx,
        interface_id: String,
        hash: Option<String>,
    ) -> Result<SchemasGetResult, JsonRpcError> {
        match self.inner.find_by_id(&interface_id, hash.as_deref()) {
            Some(iface) => {
                let schema = serde_json::to_value(iface.to_schema()).expect("schema serializes");
                Ok(SchemasGetResult { schema })
            }
            None => Err(JsonRpcError {
                code: error_codes::METHOD_NOT_FOUND,
                message: "Interface not found".to_string(),
                data: Some(json!({
                    "reason": "unknown-interface",
                    "interfaceId": interface_id,
                    "hash": hash,
                })),
            }),
        }
    }
}
