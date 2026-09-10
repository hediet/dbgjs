//! The hubrpc interface schema model: `{ id, hash, methods{}, components }`.
//! A strict subset of OpenRPC 1.x — identity & addressing live in hubrpc, so
//! this describes only the contract of a single interface.
//!
//! Ported from TS `schema/hubRpcInterfaceSchema.ts` (canonical shape, spec
//! chapter 04 §2): `methods` is a map **keyed by member name** (the key is the
//! member name — schemas carry no redundant synthetic name), and each method's
//! `params` / `result` / stream fields are a single `JsonSchema` value rather
//! than a list of named content descriptors.
//!
//! JSON-Schema bodies (`MethodSchema::params`, `components.schemas`) are kept
//! as opaque [`JsonValue`]; they hold the decidable `HubRpcJsonSchema` subset
//! produced by [`crate::schema::normalize::normalize_json_schema`].
//!
//! Because `methods` is a keyed map, member **order is not part of identity**:
//! the interface hash JCS-sorts object keys (spec 04 §4), so two schemas that
//! differ only in member insertion order hash identically. This matches the
//! canonical TS `Record<string, MethodSchema>`.

use crate::protocol::json_value::JsonValue;
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

/// A name-keyed map of methods. Keys are sorted, which is irrelevant to
/// identity (the interface hash JCS-sorts object keys, spec 04 §4) and keeps
/// serialization deterministic. This mirrors the canonical TS
/// `Record<string, MethodSchema>`.
pub type MethodMap = BTreeMap<String, MethodSchema>;

/// A single interface contract. Serializes to the exact JSON shape the
/// interface hash is computed over.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct HubRpcInterfaceSchema {
    /// Stable interface id, e.g. `"de.hediet.notification-target"`.
    pub id: String,

    /// Content hash of the normalized schema. Pairs with `id` to form `id@hash`.
    /// Omitted from the hash input itself.
    pub hash: String,

    /// Normative human description (markdown). Part of the interface hash.
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub description: Option<String>,

    /// Non-normative implementation notes. Stripped from the hash.
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub comment: Option<String>,

    /// Methods keyed by local member name. Member names MUST be unique (they are
    /// map keys) and conform to the `member` grammar (chapter 01 §2). Order is
    /// **not** normative — the hash sorts keys.
    pub methods: MethodMap,

    /// Reusable schema definitions, referenced via `#/components/schemas/<name>`.
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub components: Option<Components>,

    /// Specification-extension keys captured verbatim from the source document
    /// and preserved across a parse → serialize round-trip. Any top-level key
    /// that is not a known field lands here; in practice these are `x-…`
    /// extensions (codegen directives, tooling hints).
    ///
    /// **Identity-neutral:** the interface hash strips every `x-…` key (see
    /// [`crate::schema::hash`]), so extensions never change `id@hash`. This is
    /// the "one document, two views" mechanism: the richer document survives
    /// typed parsing so tooling (e.g. the Rust code generator, which reads
    /// `x-hubrpc-codegen`) can see it, while identity hashes only the simple
    /// wire contract. Empty by default, so schemas without extensions serialize
    /// and hash exactly as before.
    #[serde(flatten)]
    pub extensions: BTreeMap<String, JsonValue>,
}

impl HubRpcInterfaceSchema {
    /// `id@hash` addressing form.
    pub fn id_at_hash(&self) -> String {
        format!("{}@{}", self.id, self.hash)
    }

    /// Look up a preserved specification-extension value (typically an `x-…`
    /// key) by exact key. Returns `None` when absent.
    pub fn extension(&self, key: &str) -> Option<&JsonValue> {
        self.extensions.get(key)
    }
}

/// Reusable schema definitions bag, referenced via `#/components/schemas/<name>`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Components {
    /// Named JSON Schemas (the decidable hubrpc subset). Keyed by name; sorted
    /// for deterministic serialization (order is irrelevant to the hash).
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub schemas: Option<BTreeMap<String, JsonValue>>,
}

/// A single method declaration. The owning member name is the map key in
/// [`HubRpcInterfaceSchema::methods`], so it does not appear here.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct MethodSchema {
    /// Schema for the user params object (the decidable `HubRpcJsonSchema`
    /// subset). A boolean `true` schema means "any params".
    pub params: JsonValue,

    /// Result schema. Omit to declare a notification-only method.
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub result: Option<JsonValue>,

    /// Schema for client-emitted stream messages on an in-flight call.
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub client_stream: Option<JsonValue>,

    /// Schema for server-emitted stream messages on an in-flight call.
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub server_stream: Option<JsonValue>,

    /// Application-level errors. Codes MUST be unique.
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub errors: Option<Vec<ErrorSchema>>,

    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub summary: Option<String>,

    /// Normative description of this method's contract. Part of the hash.
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub description: Option<String>,

    /// Non-normative implementation notes. Stripped from the hash.
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub comment: Option<String>,

    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub deprecated: Option<bool>,

    /// Behavioral claims. Normative — part of the interface hash.
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub annotations: Option<MemberAnnotations>,

    /// Specification-extension keys captured verbatim from the source document
    /// and preserved across a parse → serialize round-trip. Any method-level key
    /// that is not a known field lands here; in practice these are `x-…`
    /// extensions. **Identity-neutral** (stripped from the interface hash). The
    /// code generator reads `x-hubrpc-codegen` from here — notably
    /// `x-hubrpc-codegen.kind = "serverNotification"` to mark a result-less
    /// method as a server→client event rather than a client-sent notification.
    /// Empty by default, so methods without extensions are unchanged.
    #[serde(flatten)]
    pub extensions: BTreeMap<String, JsonValue>,
}

impl MethodSchema {
    /// A notification-only method (no result) with the given params schema.
    pub fn notification(params: JsonValue) -> Self {
        MethodSchema {
            params,
            result: None,
            client_stream: None,
            server_stream: None,
            errors: None,
            summary: None,
            description: None,
            comment: None,
            deprecated: None,
            annotations: None,
            extensions: BTreeMap::new(),
        }
    }

    /// A request method with the given params and result schemas.
    pub fn request(params: JsonValue, result: JsonValue) -> Self {
        MethodSchema {
            result: Some(result),
            ..MethodSchema::notification(params)
        }
    }

    /// Look up a preserved specification-extension value (typically an `x-…`
    /// key) by exact key. Returns `None` when absent.
    pub fn extension(&self, key: &str) -> Option<&JsonValue> {
        self.extensions.get(key)
    }
}

/// Behavioral claims about a method. Every flag is a positive assertion
/// (default `false` ≡ "no claim"). Normative — included in the interface hash.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct MemberAnnotations {
    /// Pure query: does not modify observable state.
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub read_only: Option<bool>,
    /// Safe to retry: N calls ≡ one call.
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub idempotent: Option<bool>,
    /// Effects can be undone with a follow-up call.
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub reversible: Option<bool>,
    /// Slow, costly, or rate-limited.
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub expensive: Option<bool>,
    /// Irreversible or destructive effects. UIs should confirm.
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub dangerous: Option<bool>,
}

/// An application-level error declaration.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ErrorSchema {
    /// JSON-RPC error code. -32768..-32000 are reserved.
    pub code: i64,
    pub message: String,
    /// Optional schema describing the shape of `error.data`.
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub data: Option<JsonValue>,
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::schema::hash::compute_interface_hash;
    use serde_json::json;

    const ENRICHED: &str = r##"{
        "id": "com.example.events",
        "hash": "",
        "description": "Interface with x-* extensions at every level.",
        "x-hubrpc-codegen": { "clientName": "EventsClient" },
        "methods": {
            "subscribe": {
                "x-safety": { "readOnly": true },
                "params": { "type": "object", "additionalProperties": false },
                "result": { "type": "boolean" }
            },
            "on_event": {
                "x-hubrpc-codegen": { "kind": "serverNotification" },
                "params": { "type": "object", "additionalProperties": false }
            }
        }
    }"##;

    fn parse(doc: &str) -> HubRpcInterfaceSchema {
        serde_json::from_str(doc).expect("valid schema")
    }

    #[test]
    fn top_level_and_method_extensions_survive_typed_parsing() {
        let schema = parse(ENRICHED);
        // Top-level x-* preserved and reachable.
        assert_eq!(
            schema.extension("x-hubrpc-codegen"),
            Some(&json!({ "clientName": "EventsClient" }))
        );
        // Method-level x-* preserved.
        let on_event = &schema.methods["on_event"];
        assert_eq!(
            on_event.extension("x-hubrpc-codegen"),
            Some(&json!({ "kind": "serverNotification" }))
        );
        assert_eq!(
            schema.methods["subscribe"].extension("x-safety"),
            Some(&json!({ "readOnly": true }))
        );
        // Absent extensions are None.
        assert_eq!(schema.methods["subscribe"].extension("x-missing"), None);
    }

    #[test]
    fn extensions_round_trip_through_serialization() {
        let schema = parse(ENRICHED);
        let reserialized = serde_json::to_value(&schema).unwrap();
        // The flattened x-* keys appear at their original level on the way out.
        assert_eq!(
            reserialized["x-hubrpc-codegen"],
            json!({ "clientName": "EventsClient" })
        );
        assert_eq!(
            reserialized["methods"]["on_event"]["x-hubrpc-codegen"]["kind"],
            "serverNotification"
        );
        // Full fidelity: re-parsing yields the same typed value.
        let round: HubRpcInterfaceSchema = serde_json::from_value(reserialized).unwrap();
        assert_eq!(schema, round);
    }

    #[test]
    fn extensions_are_hash_invisible() {
        // The enriched document hashes identically to the same contract with all
        // x-* keys removed: identity is the simple wire projection.
        let enriched = parse(ENRICHED);
        let plain = parse(
            r##"{
                "id": "com.example.events",
                "hash": "",
                "description": "Interface with x-* extensions at every level.",
                "methods": {
                    "subscribe": {
                        "params": { "type": "object", "additionalProperties": false },
                        "result": { "type": "boolean" }
                    },
                    "on_event": {
                        "params": { "type": "object", "additionalProperties": false }
                    }
                }
            }"##,
        );
        assert_eq!(
            compute_interface_hash(&enriched),
            compute_interface_hash(&plain)
        );
    }

    #[test]
    fn editing_an_extension_value_does_not_change_the_hash() {
        let a = parse(ENRICHED);
        let mut b = a.clone();
        b.extensions.insert(
            "x-hubrpc-codegen".to_string(),
            json!({ "clientName": "SomethingElse", "extra": [1, 2, 3] }),
        );
        assert_ne!(a, b); // typed values differ...
        assert_eq!(compute_interface_hash(&a), compute_interface_hash(&b)); // ...but hash is stable.
    }

    #[test]
    fn schema_without_extensions_serializes_without_extra_keys() {
        // Backward compatibility: an empty extension map adds nothing to the wire form.
        let schema = MethodSchema::request(json!(true), json!({ "type": "string" }));
        let value = serde_json::to_value(&schema).unwrap();
        let obj = value.as_object().unwrap();
        assert!(obj.contains_key("params"));
        assert!(obj.contains_key("result"));
        // No stray flattened keys.
        assert_eq!(obj.len(), 2);
    }
}
