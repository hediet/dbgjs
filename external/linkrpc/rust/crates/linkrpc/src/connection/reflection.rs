//! The three linkrpc reflection interfaces — `hubrpc.defaults`, `hubrpc.directory`,
//! `hubrpc.schemas` — defined with the `#[link_rpc_interface]` macro. Registering any interface on
//! a connection also exposes these so peers can discover and introspect it.
//!
//! Param/result schemas are derived from Rust types via `schemars` + the schemars→subset bridge,
//! mirroring the TS zod definitions in `hub/common/reflection.interfaces.ts`. The macro is used
//! here from *within* the `linkrpc` crate itself (see `extern crate self as linkrpc` in `lib.rs`);
//! the registry-backed implementation of the generated traits lives in `hub_connection.rs`.
//!
//! Inline method parameters are wrapped by the macro into a camelCase params object, so only the
//! result types need to be declared here.

use crate::connection::interface_def::InterfaceDefinition;

pub const DEFAULTS_ID: &str = "hubrpc.defaults";
pub const DIRECTORY_ID: &str = "hubrpc.directory";
pub const SCHEMAS_ID: &str = "hubrpc.schemas";

// Generated trait/client/server items for the three reflection interfaces. The typed `*Client`
// proxies are re-exported (see `prelude`) so consumers can introspect an endpoint without hand-
// rolling `call_member` calls; some items are unused inside this crate.
#[allow(dead_code)]
pub mod iface {
    #![allow(non_snake_case)]

    use schemars::JsonSchema;
    use serde::{Deserialize, Serialize};

    use crate::protocol::json_value::JsonValue;
    use crate::protocol::jsonrpc::JsonRpcError;
    use linkrpc_macros::link_rpc_interface;

    // ── hubrpc.defaults ───────────────────────────────────────────────────────
    #[derive(Serialize, Deserialize, JsonSchema)]
    #[serde(rename_all = "camelCase")]
    pub struct DefaultsGetResult {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        #[schemars(with = "String")]
        pub service_id: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        #[schemars(with = "String")]
        pub interface_id: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        #[schemars(with = "String")]
        pub interface_hash: Option<String>,
    }

    #[derive(Serialize, Deserialize, JsonSchema)]
    #[serde(rename_all = "camelCase")]
    pub struct BareBindingListing {
        pub prefix: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        #[schemars(with = "String")]
        pub service_id: Option<String>,
        pub interface_id: String,
        pub interface_hash: String,
    }

    #[derive(Serialize, Deserialize, JsonSchema)]
    pub struct DefaultsListBindingsResult {
        pub bindings: Vec<BareBindingListing>,
    }

    /// Reflection: preset default service / interface on this connection.
    #[link_rpc_interface(id = "hubrpc.defaults")]
    #[allow(non_snake_case)]
    pub trait DefaultsService {
        async fn get() -> Result<DefaultsGetResult, JsonRpcError>;
        async fn listBindings() -> Result<DefaultsListBindingsResult, JsonRpcError>;
    }

    // ── hubrpc.directory ──────────────────────────────────────────────────────
    #[derive(Serialize, Deserialize, JsonSchema, Clone)]
    #[serde(rename_all = "camelCase")]
    pub struct ServiceListing {
        pub service_id: String,
        pub interface_id: String,
        pub interface_hash: String,
        #[serde(skip_serializing_if = "Option::is_none")]
        pub service_description: Option<String>,
    }

    #[derive(Serialize, Deserialize, JsonSchema)]
    #[serde(rename_all = "camelCase")]
    pub struct DirectoryListResult {
        pub items: Vec<ServiceListing>,
        #[serde(skip_serializing_if = "Option::is_none")]
        pub next_cursor: Option<String>,
        #[serde(skip_serializing_if = "Option::is_none")]
        pub truncated: Option<bool>,
    }

    /// Reflection: list services exposed by this endpoint. Can also list other directory services that can be explored.
    #[link_rpc_interface(id = "hubrpc.directory")]
    pub trait DirectoryService {
        async fn list(
            interface_id: Option<String>,
            service_id: Option<String>,
            cursor: Option<String>,
            limit: Option<u32>,
            timeout_ms: Option<u32>,
        ) -> Result<DirectoryListResult, JsonRpcError>;
    }

    // ── hubrpc.schemas ────────────────────────────────────────────────────────
    #[derive(Serialize, Deserialize, JsonSchema)]
    pub struct SchemasGetResult {
        /// A full `LinkRpcInterfaceSchema`.
        pub schema: JsonValue,
    }

    /// Reflection: fetch interface schemas by id (+ optional hash). Must serve the interfaces advertised in this endpoint's directory.
    #[link_rpc_interface(id = "hubrpc.schemas")]
    pub trait SchemasService {
        async fn get(
            interface_id: String,
            hash: Option<String>,
        ) -> Result<SchemasGetResult, JsonRpcError>;
    }
}

/// `hubrpc.defaults` — the preset default service/interface on this connection.
pub fn defaults_interface() -> InterfaceDefinition {
    iface::defaults_service::interface()
}

/// `hubrpc.directory` — list the services/interfaces exposed by this endpoint.
pub fn directory_interface() -> InterfaceDefinition {
    iface::directory_service::interface()
}

/// `hubrpc.schemas` — fetch interface schemas by id (+ optional hash).
pub fn schemas_interface() -> InterfaceDefinition {
    iface::schemas_service::interface()
}
