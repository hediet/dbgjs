//! hubrpc — a Rust port of the `@vscode/hubrpc` JSON-RPC dialect.
//!
//! This crate is built in phases (see `executionPlan.md`). Phase 1 (current) is the
//! wire core: JSON values, the JSON-RPC 2.0 message types, the `::` method-name grammar,
//! JCS canonicalization, a `MessageTransport` seam with an in-memory pair, and a `Channel`
//! that correlates requests/responses and dispatches inbound calls.

// Lets the `#[hub_rpc_interface]` macro's absolute `::hubrpc::…` paths resolve when the macro is
// used *within* this crate (e.g. the built-in reflection interfaces).
extern crate self as hubrpc;

pub mod client;
pub mod connection;
pub mod protocol;
pub mod schema;
pub mod transport;

pub mod prelude {
    pub use crate::client::RpcCall;
    pub use crate::connection::channel::{Channel, RequestHandler};
    pub use crate::connection::dispatch::{CallCtx, InterfaceHandler, ServiceExport};
    pub use crate::connection::endpoint::{
        format_endpoint_uri, is_hub_endpoint, parse_endpoint_uri, EndpointCommand, EndpointError,
        FormatEndpointOptions, ResolvedEndpoint,
    };
    pub use crate::connection::hub_connection::{
        ConnError, HubRpcConnection, RegisterOptions, RegisteredListing,
    };
    pub use crate::connection::interface_def::{
        InterfaceDefinition, InterfaceInfo, Member, MemberDocs, NotificationMember, RequestMember,
    };
    pub use crate::connection::reflection::{
        defaults_interface, directory_interface, schemas_interface,
    };
    // Typed clients (+ their result types) for the built-in reflection interfaces, so consumers
    // can introspect an endpoint without hand-rolling `call_member` calls.
    pub use crate::connection::reflection::iface::{
        DefaultsGetResult, DefaultsServiceClient, DirectoryListResult, DirectoryServiceClient,
        SchemasGetResult, SchemasServiceClient, ServiceListing,
    };
    pub use crate::protocol::json_value::JsonValue;
    pub use crate::protocol::jsonrpc::{
        error_codes, JsonRpcError, JsonRpcMessage, JsonRpcNotification, JsonRpcRequest,
        JsonRpcResponse, RequestId,
    };
    pub use crate::protocol::method_name::{
        format_method_name, parse_method_name, ParsedMethodName,
    };
    pub use crate::schema::{
        compute_interface_hash, compute_interface_hash_value, generate_rust_interface,
        normalize_json_schema, Components, ErrorSchema, GenerateRustOptions, GeneratedRust,
        HubRpcInterfaceSchema, MemberAnnotations, MethodMap, MethodSchema,
    };
    pub use crate::transport::memory::transport_pair;
    pub use crate::transport::message::{MessageTransport, TransportError};
    pub use crate::transport::multiplexed::{
        MultiplexedTransport, MuxChannel, MuxCodec, MuxEnvelope, MuxEnvelopeCodec, MuxError,
        RouteOutcome,
    };

    /// Derive a hubrpc interface (trait + server adapter + client + `interface()`) from a bare
    /// trait spec. See `docs/examples.md`.
    pub use hubrpc_macros::hub_rpc_interface;
}
