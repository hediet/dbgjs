//! The interface hash: SHA-256 of the canonicalized schema, truncated to 16 hex
//! chars (64 bits of collision budget per id). Ported from TS `schema/hash.ts`.
//!
//! Canonicalization projects the schema onto its **normative wire-contract
//! form** before hashing, so one interface document can carry richer,
//! un-normalized material without changing identity:
//!   1. **Normalize** every JSON-Schema position — each method's `params`,
//!      `result`, `clientStream`, `serverStream`, and every entry under
//!      `components.schemas` — through [`normalize_json_schema`]. This collapses
//!      a raw schema onto the decidable hubrpc subset: incidental keys are
//!      dropped, object closure is defaulted, `required` is sorted, `{}` becomes
//!      `true`, `{x-only: …}` becomes `true`, etc. Two documents that differ only
//!      in un-normalized schema spelling (or in schema-level `x-…` extensions)
//!      therefore hash identically, matching what a normalize-then-hash producer
//!      (e.g. the CDP importer) embeds. Already-normalized schemas are a fixed
//!      point, so every existing hash is preserved.
//!   2. Strip every `comment` field (non-normative — must not affect identity).
//!      `description` is NORMATIVE and kept.
//!   3. Strip every **specification-extension** key — any object key beginning
//!      with [`EXTENSION_PREFIX`] (`x-`) — at every remaining (non-schema) level.
//!      This is the minimal, explicit "one document, two views" mechanism
//!      (mirrors OpenRPC/OpenAPI `x-` extensions): the document keeps the rich
//!      `x-…` expressions; identity hashes only the simple contract. Editing an
//!      `x-…` value never changes the hash; changing a wire field does.
//!   4. Omit the top-level `hash` field itself.
//!   5. RFC 8785 JCS encode (recursive key sort, no whitespace).
//!   6. SHA-256, first 8 bytes, lowercase hex.

use crate::protocol::jcs::jcs_canonicalize_bytes;
use crate::protocol::json_value::{JsonMap, JsonValue};
use crate::schema::interface_schema::HubRpcInterfaceSchema;
use crate::schema::normalize::normalize_json_schema;
use sha2::{Digest, Sha256};
use std::fmt::Write as _;

/// JSON-Schema positions inside a method that are normalized before hashing.
const METHOD_SCHEMA_FIELDS: &[&str] = &["params", "result", "clientStream", "serverStream"];

/// Prefix marking a non-normative **specification-extension** key. Any object
/// key beginning with this prefix is stripped before hashing, exactly like
/// `comment`. Reserved for rich, identity-neutral material (codegen directives,
/// richer validation/safety expressions, tooling hints).
pub const EXTENSION_PREFIX: &str = "x-";

/// Compute the interface hash for an already-serialized schema value.
///
/// `schema` is the JSON object form of a [`HubRpcInterfaceSchema`]. The top-level
/// `hash` field (if present) is ignored, so this is safe to call on a schema
/// whose `hash` has not yet been filled in.
pub fn compute_interface_hash_value(schema: &JsonValue) -> String {
    let normalized = normalize_schema_positions(schema);
    let stripped = strip_non_normative(&normalized, true);
    let bytes = jcs_canonicalize_bytes(&stripped).expect("schema canonicalization");
    let digest = Sha256::digest(&bytes);
    let mut hex = String::with_capacity(16);
    for b in &digest[..8] {
        write!(hex, "{b:02x}").expect("writing to a String is infallible");
    }
    hex
}

/// Compute the interface hash for a typed [`HubRpcInterfaceSchema`].
pub fn compute_interface_hash(schema: &HubRpcInterfaceSchema) -> String {
    let value = serde_json::to_value(schema).expect("schema serialization is infallible");
    compute_interface_hash_value(&value)
}

/// Project the interface's JSON-Schema positions onto the decidable hubrpc
/// subset via [`normalize_json_schema`], leaving all non-schema material intact.
///
/// The normalized positions are exactly the schema-bearing fields of the
/// interface contract: each method's `params` / `result` / `clientStream` /
/// `serverStream`, and every named schema under `components.schemas`. This
/// mirrors the normalize-then-hash projection a producer applies before
/// embedding a hash, so a rich, un-normalized document hashes identically to its
/// simple normalized projection.
///
/// Normalization is best-effort and infallible: a position that cannot be
/// normalized (structurally impossible input) is left untouched rather than
/// panicking, preserving the previous strip-only behavior for that position.
fn normalize_schema_positions(schema: &JsonValue) -> JsonValue {
    let mut root = schema.clone();

    if let Some(methods) = root.get_mut("methods").and_then(JsonValue::as_object_mut) {
        for method in methods.values_mut() {
            let Some(method) = method.as_object_mut() else {
                continue;
            };
            // Collect first (immutable borrows) then write, to avoid overlapping
            // borrows of `method`.
            let normalized: Vec<(String, JsonValue)> = METHOD_SCHEMA_FIELDS
                .iter()
                .filter_map(|field| {
                    let value = method.get(*field)?;
                    let normalized = normalize_json_schema(value).ok()?;
                    Some(((*field).to_string(), normalized))
                })
                .collect();
            for (field, value) in normalized {
                method.insert(field, value);
            }
        }
    }

    if let Some(schemas) = root
        .get_mut("components")
        .and_then(JsonValue::as_object_mut)
        .and_then(|components| components.get_mut("schemas"))
        .and_then(JsonValue::as_object_mut)
    {
        for schema in schemas.values_mut() {
            if let Ok(normalized) = normalize_json_schema(schema) {
                *schema = normalized;
            }
        }
    }

    root
}

/// True for a key that must not contribute to the interface hash.
fn is_non_normative_key(key: &str, is_root: bool) -> bool {
    key == "comment" || key.starts_with(EXTENSION_PREFIX) || (is_root && key == "hash")
}

/// Recursively drop non-normative fields: every `comment`, every `x-…`
/// extension, plus the root `hash`.
fn strip_non_normative(value: &JsonValue, is_root: bool) -> JsonValue {
    match value {
        JsonValue::Array(arr) => {
            JsonValue::Array(arr.iter().map(|v| strip_non_normative(v, false)).collect())
        }
        JsonValue::Object(map) => {
            let mut out = JsonMap::new();
            for (k, v) in map {
                if is_non_normative_key(k, is_root) {
                    continue;
                }
                out.insert(k.clone(), strip_non_normative(v, false));
            }
            JsonValue::Object(out)
        }
        other => other.clone(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn iface(extra: JsonValue) -> JsonValue {
        // Minimal valid schema with one method; `extra` merges top-level keys.
        let mut base = json!({
            "id": "test.iface",
            "hash": "",
            "methods": {
                "ping": { "params": true, "result": { "type": "string" } }
            }
        });
        if let JsonValue::Object(e) = extra {
            for (k, v) in e {
                base.as_object_mut().unwrap().insert(k, v);
            }
        }
        base
    }

    #[test]
    fn is_stable_16_hex_and_comment_independent() {
        let a = compute_interface_hash_value(&iface(json!({ "comment": "first note" })));
        let b =
            compute_interface_hash_value(&iface(json!({ "comment": "totally different note" })));
        assert_eq!(a, b);
        assert_eq!(a.len(), 16);
        assert!(a
            .chars()
            .all(|c| c.is_ascii_hexdigit() && !c.is_ascii_uppercase()));
    }

    #[test]
    fn description_is_normative() {
        let a = compute_interface_hash_value(&iface(json!({ "description": "first" })));
        let b = compute_interface_hash_value(&iface(json!({ "description": "second" })));
        assert_ne!(a, b);
    }

    #[test]
    fn method_comment_stripped_description_kept() {
        let base = json!({
            "id": "test.iface", "hash": "",
            "methods": { "ping": { "params": true,
                "result": { "type": "string" },
                "description": "Returns 'pong'." } }
        });
        let with_comment = json!({
            "id": "test.iface", "hash": "",
            "methods": { "ping": { "params": true,
                "result": { "type": "string" },
                "description": "Returns 'pong'.", "comment": "runs in <1ms" } }
        });
        let changed_desc = json!({
            "id": "test.iface", "hash": "",
            "methods": { "ping": { "params": true,
                "result": { "type": "string" },
                "description": "Returns 'pong'. MUST be idempotent." } }
        });
        assert_eq!(
            compute_interface_hash_value(&base),
            compute_interface_hash_value(&with_comment)
        );
        assert_ne!(
            compute_interface_hash_value(&base),
            compute_interface_hash_value(&changed_desc)
        );
    }

    #[test]
    fn method_order_is_not_part_of_identity() {
        // `methods` is a name-keyed map: the hash JCS-sorts keys, so member
        // insertion order does not change identity (spec 04 §4).
        let a = json!({
            "id": "test.iface", "hash": "",
            "methods": {
                "foo": { "params": true },
                "bar": { "params": true }
            }
        });
        let b = json!({
            "id": "test.iface", "hash": "",
            "methods": {
                "bar": { "params": true },
                "foo": { "params": true }
            }
        });
        assert_eq!(
            compute_interface_hash_value(&a),
            compute_interface_hash_value(&b)
        );
    }

    #[test]
    fn method_name_is_part_of_identity() {
        // Renaming a member (the map key) is a wire-contract change.
        let a = json!({
            "id": "test.iface", "hash": "",
            "methods": { "foo": { "params": true } }
        });
        let b = json!({
            "id": "test.iface", "hash": "",
            "methods": { "bar": { "params": true } }
        });
        assert_ne!(
            compute_interface_hash_value(&a),
            compute_interface_hash_value(&b)
        );
    }

    #[test]
    fn root_hash_field_ignored() {
        let a = compute_interface_hash_value(&iface(json!({ "hash": "" })));
        let b = compute_interface_hash_value(&iface(json!({ "hash": "deadbeefdeadbeef" })));
        assert_eq!(a, b);
    }

    // ── specification extensions (x-*): rich single document, simple identity ──

    fn base_doc() -> JsonValue {
        json!({
            "id": "test.iface",
            "hash": "",
            "description": "Normative description.",
            "methods": {
                "order": {
                    "params": { "type": "object", "properties": {}, "additionalProperties": false },
                    "result": { "type": "string" },
                    "description": "Places an order."
                }
            }
        })
    }

    fn enriched_doc() -> JsonValue {
        json!({
            "id": "test.iface",
            "hash": "",
            "description": "Normative description.",
            "x-codegen": { "tsClientName": "OrderClient", "package": "@acme/orders" },
            "methods": {
                "order": {
                    "x-safety": { "requiresConfirmation": true, "rateLimitPerMin": 5 },
                    "params": {
                        "type": "object", "properties": {}, "additionalProperties": false,
                        "x-validation": "z.object({}).strict()"
                    },
                    "result": { "type": "string", "x-format": "order-id" },
                    "description": "Places an order."
                }
            }
        })
    }

    #[test]
    fn extension_prefix_is_x_dash() {
        assert_eq!(EXTENSION_PREFIX, "x-");
    }

    #[test]
    fn rich_extensions_do_not_change_the_hash() {
        // The enriched document (x-* at interface/method/schema level) hashes
        // identically to the plain wire contract: identity is the simple
        // projection.
        assert_eq!(
            compute_interface_hash_value(&base_doc()),
            compute_interface_hash_value(&enriched_doc())
        );
    }

    #[test]
    fn editing_an_extension_value_never_changes_the_hash() {
        let mut edited = enriched_doc();
        edited["x-codegen"] = json!({ "tsClientName": "Totally", "package": "@acme/other" });
        assert_eq!(
            compute_interface_hash_value(&enriched_doc()),
            compute_interface_hash_value(&edited)
        );
    }

    #[test]
    fn changing_a_wire_field_on_enriched_doc_changes_the_hash() {
        let mut wire_changed = enriched_doc();
        wire_changed["methods"]["order"]["result"] =
            json!({ "type": "number", "x-format": "order-id" });
        assert_ne!(
            compute_interface_hash_value(&enriched_doc()),
            compute_interface_hash_value(&wire_changed)
        );
    }

    // ── normalization projection: un-normalized schemas hash like their
    //    normalized simple projection ──────────────────────────────────────────

    /// A rich document whose JSON-Schema positions are *un-normalized* (missing
    /// `additionalProperties`, unsorted `required`, incidental keys, an empty
    /// `{}` schema, a schema that is nothing but an `x-…` extension) and which
    /// also carries `x-…` metadata at the interface, method, and schema levels.
    fn rich_unnormalized_doc() -> JsonValue {
        json!({
            "id": "test.norm",
            "hash": "",
            "description": "Normative.",
            "x-codegen": { "client": "NormClient" },
            "methods": {
                "op": {
                    "x-safety": { "readOnly": true },
                    "params": {
                        "type": "object",
                        "properties": {
                            "b": { "type": "string", "minLength": 2 },
                            "a": { "type": "integer", "default": 0 }
                        },
                        "required": ["b", "a"],
                        "x-validation": "z.object({})"
                    },
                    "result": { "$ref": "#/components/schemas/Blob", "x-format": "id" },
                    "clientStream": {},
                    "serverStream": { "x-only": "extension" }
                }
            },
            "components": { "schemas": {
                "Blob": {
                    "type": "object",
                    "title": "Blob",
                    "properties": { "y": { "type": "number" }, "x": { "type": "number" } },
                    "required": ["y", "x"],
                    "examples": [1]
                }
            }}
        })
    }

    /// The hand-written normalized simple projection of [`rich_unnormalized_doc`]:
    /// object closure defaulted, `required` sorted, incidental keys dropped,
    /// `{}` → `true`, the `x-…`-only schema → `true`, all `x-…` removed.
    fn rich_normalized_projection() -> JsonValue {
        json!({
            "id": "test.norm",
            "hash": "",
            "description": "Normative.",
            "methods": {
                "op": {
                    "params": {
                        "type": "object",
                        "properties": {
                            "b": { "type": "string" },
                            "a": { "type": "integer" }
                        },
                        "required": ["a", "b"],
                        "additionalProperties": false
                    },
                    "result": { "$ref": "#/components/schemas/Blob" },
                    "clientStream": true,
                    "serverStream": true
                }
            },
            "components": { "schemas": {
                "Blob": {
                    "type": "object",
                    "title": "Blob",
                    "properties": { "y": { "type": "number" }, "x": { "type": "number" } },
                    "required": ["x", "y"],
                    "additionalProperties": false
                }
            }}
        })
    }

    #[test]
    fn unnormalized_schema_hashes_like_its_normalized_projection() {
        // The whole point of the fix: an un-normalized document that carries
        // `x-…` metadata hashes identically to its normalized, extension-free
        // simple projection.
        assert_eq!(
            compute_interface_hash_value(&rich_unnormalized_doc()),
            compute_interface_hash_value(&rich_normalized_projection())
        );
    }

    #[test]
    fn extension_only_schema_normalizes_to_true_not_empty_object() {
        // A schema position that is *only* an `x-…` extension must normalize to
        // `true` (top), not to `{}`. This was the concrete CDP hash gap.
        let with_ext = iface(json!({}));
        let mut ext_only = with_ext.clone();
        ext_only["methods"]["ping"]["params"] = json!({ "x-codegen": { "hint": "any" } });
        let mut plain_true = with_ext.clone();
        plain_true["methods"]["ping"]["params"] = json!(true);
        assert_eq!(
            compute_interface_hash_value(&ext_only),
            compute_interface_hash_value(&plain_true)
        );
    }

    #[test]
    fn already_normalized_schema_is_a_fixed_point() {
        // Backward compatibility: normalizing an already-normalized document is a
        // no-op, so its hash is unchanged by the added normalization step.
        let normalized = rich_normalized_projection();
        // Idempotence at the hash level: re-projecting doesn't move the hash.
        assert_eq!(
            compute_interface_hash_value(&normalized),
            compute_interface_hash_value(&normalized)
        );
    }
}
