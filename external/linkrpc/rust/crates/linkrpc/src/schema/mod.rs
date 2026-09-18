//! Schema model & interface hashing (the cross-language interop canary).
//!
//! - [`interface_schema`] — the `LinkRpcInterfaceSchema` model.
//! - [`normalize`] — raw JSON Schema → decidable linkrpc subset (zod↔schemars bridge).
//! - [`hash`] — `compute_interface_hash`: strip → JCS → SHA-256 → 16 hex.

pub mod codegen;
pub mod hash;
pub mod interface_schema;
pub mod normalize;
pub mod schemars_subset;

pub use codegen::{generate_rust_interface, GenerateRustOptions, GeneratedRust};
pub use hash::{compute_interface_hash, compute_interface_hash_value};
pub use interface_schema::{
    Components, ErrorSchema, LinkRpcInterfaceSchema, MemberAnnotations, MethodMap, MethodSchema,
};
pub use normalize::{normalize_json_schema, NormalizeError, KEPT_KEYS};
pub use schemars_subset::{schemars_to_subset, SchemarsSubsetError};
