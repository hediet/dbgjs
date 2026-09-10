//! Bridge raw `schemars` output into the shape the shared
//! [`normalize_json_schema`](crate::schema::normalize::normalize_json_schema) expects, so a
//! Rust-derived schema hashes identically to the equivalent zod-derived one in TypeScript.
//!
//! `schemars` (draft-07) and zod's `toJSONSchema` describe the same types differently. This
//! pre-pass erases the incidental differences **before** the language-agnostic normalize runs:
//!
//! 1. **Inline `$ref`** against the root `definitions` / `$defs` bag and drop the bag — zod
//!    inlines every subschema, schemars factors them out.
//! 2. **Drop `title`** (schemars stamps the Rust type name; `title` is otherwise hash-kept)
//!    and `$schema`.
//! 3. **Drop numeric `format`** (`uint32`, `float`, …) on `number`/`integer` schemas — a
//!    representation hint zod never emits. String formats (`date-time`, `uuid`, …) are kept.
//! 4. **Single-element `enum` → `const`** — schemars lowers a unit/`literal` to `enum: [x]`,
//!    zod to `const: x`; the latter is also what `normalize`'s discriminator synthesis keys on.
//!
//! After this pass the shared `normalize_json_schema` (required-sort, object closure,
//! discriminator synthesis, `{}`/`{"not":{}}` collapse) yields a value byte-identical to the TS
//! side, and therefore the same interface hash.

use crate::protocol::json_value::{JsonMap, JsonValue};
use crate::schema::normalize::{normalize_json_schema, NormalizeError};
use std::collections::BTreeSet;

const REF_PREFIXES: &[&str] = &["#/definitions/", "#/$defs/"];

/// Convert a serialized `schemars` schema (a `RootSchema` as `serde_json::Value`) into the
/// decidable hubrpc subset. Equivalent to TS `zodToSvcJsonSchema`.
pub fn schemars_to_subset(root: &JsonValue) -> Result<JsonValue, SchemarsSubsetError> {
    let defs = collect_defs(root);
    let mut active = BTreeSet::new();
    let prepared = prepare(root, &defs, &mut active)?;
    Ok(normalize_json_schema(&prepared)?)
}

fn collect_defs(root: &JsonValue) -> JsonMap<String, JsonValue> {
    let mut defs = JsonMap::new();
    if let Some(obj) = root.as_object() {
        for key in ["definitions", "$defs"] {
            if let Some(JsonValue::Object(bag)) = obj.get(key) {
                for (name, schema) in bag {
                    defs.insert(name.clone(), schema.clone());
                }
            }
        }
    }
    defs
}

fn ref_target(s: &str) -> Option<&str> {
    REF_PREFIXES
        .iter()
        .find_map(|p| s.strip_prefix(p))
        .filter(|name| !name.is_empty())
}

fn prepare(
    value: &JsonValue,
    defs: &JsonMap<String, JsonValue>,
    active: &mut BTreeSet<String>,
) -> Result<JsonValue, SchemarsSubsetError> {
    match value {
        JsonValue::Array(arr) => {
            let mut out = Vec::with_capacity(arr.len());
            for v in arr {
                out.push(prepare(v, defs, active)?);
            }
            Ok(JsonValue::Array(out))
        }
        JsonValue::Object(obj) => prepare_object(obj, defs, active),
        other => Ok(other.clone()),
    }
}

fn prepare_object(
    obj: &JsonMap<String, JsonValue>,
    defs: &JsonMap<String, JsonValue>,
    active: &mut BTreeSet<String>,
) -> Result<JsonValue, SchemarsSubsetError> {
    // Resolve `$ref` by inlining the referenced (and prepared) definition.
    if let Some(JsonValue::String(r)) = obj.get("$ref") {
        let Some(name) = ref_target(r) else {
            return Err(SchemarsSubsetError::UnresolvedRef(r.clone()));
        };
        let Some(target) = defs.get(name) else {
            return Err(SchemarsSubsetError::UnresolvedRef(r.clone()));
        };
        if !active.insert(name.to_string()) {
            return Err(SchemarsSubsetError::RecursiveRef(name.to_string()));
        }
        let inlined = prepare(target, defs, active)?;
        active.remove(name);
        return Ok(inlined);
    }

    let is_numeric = matches!(
        obj.get("type").and_then(JsonValue::as_str),
        Some("number") | Some("integer")
    );

    let mut out = JsonMap::new();
    for (k, v) in obj {
        match k.as_str() {
            // schemars bookkeeping that has no place in the decidable subset.
            "$schema" | "title" | "definitions" | "$defs" => continue,
            // Numeric representation hint zod never emits.
            "format" if is_numeric => continue,
            _ => {}
        }
        out.insert(k.clone(), prepare(v, defs, active)?);
    }

    // Single-element `enum` ⇒ `const` (schemars lowers literals to a one-element enum).
    if let Some(JsonValue::Array(items)) = out.get("enum") {
        if items.len() == 1 {
            let only = items[0].clone();
            out.remove("enum");
            out.insert("const".to_string(), only);
        }
    }

    Ok(JsonValue::Object(out))
}

/// Errors from [`schemars_to_subset`].
#[derive(Debug, thiserror::Error)]
pub enum SchemarsSubsetError {
    #[error("schemars_to_subset: could not resolve {0}")]
    UnresolvedRef(String),
    #[error("schemars_to_subset: recursive $ref to {0} cannot be inlined into the hubrpc subset")]
    RecursiveRef(String),
    #[error(transparent)]
    Normalize(#[from] NormalizeError),
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn strips_title_schema_and_numeric_format() {
        let raw = json!({
            "$schema": "http://json-schema.org/draft-07/schema#",
            "title": "OrderConfirmation",
            "type": "object",
            "required": ["n"],
            "properties": { "n": { "type": "integer", "format": "uint32", "minimum": 0.0 } }
        });
        let got = schemars_to_subset(&raw).unwrap();
        assert_eq!(
            got,
            json!({
                "type": "object",
                "properties": { "n": { "type": "integer" } },
                "required": ["n"],
                "additionalProperties": false
            })
        );
    }

    #[test]
    fn keeps_string_format() {
        let raw = json!({ "type": "string", "format": "date-time" });
        let got = schemars_to_subset(&raw).unwrap();
        assert_eq!(got, json!({ "type": "string", "format": "date-time" }));
    }

    #[test]
    fn inlines_refs_and_drops_definitions() {
        let raw = json!({
            "type": "object",
            "required": ["kind"],
            "properties": { "kind": { "$ref": "#/definitions/Kind" } },
            "definitions": {
                "Kind": { "type": "string", "enum": ["a", "b"] }
            }
        });
        let got = schemars_to_subset(&raw).unwrap();
        assert_eq!(
            got,
            json!({
                "type": "object",
                "properties": { "kind": { "type": "string", "enum": ["a", "b"] } },
                "required": ["kind"],
                "additionalProperties": false
            })
        );
    }

    #[test]
    fn single_element_enum_becomes_const_enabling_discriminator() {
        // Two object branches whose tag is a one-element enum (schemars' lowering
        // of a literal). The subset bridge rewrites them to `const`, and normalize
        // then synthesizes the discriminator — matching zod's output.
        let raw = json!({
            "oneOf": [
                { "type": "object", "required": ["t"],
                  "properties": { "t": { "type": "string", "enum": ["x"] } } },
                { "type": "object", "required": ["t"],
                  "properties": { "t": { "type": "string", "enum": ["y"] } } }
            ]
        });
        let got = schemars_to_subset(&raw).unwrap();
        assert_eq!(got["discriminator"], json!({ "propertyName": "t" }));
        assert_eq!(
            got["oneOf"][0]["properties"]["t"],
            json!({ "type": "string", "const": "x" })
        );
    }

    #[test]
    fn recursive_ref_is_rejected() {
        let raw = json!({
            "$ref": "#/definitions/Node",
            "definitions": {
                "Node": {
                    "type": "object",
                    "properties": { "next": { "$ref": "#/definitions/Node" } }
                }
            }
        });
        assert!(matches!(
            schemars_to_subset(&raw),
            Err(SchemarsSubsetError::RecursiveRef(_))
        ));
    }
}
