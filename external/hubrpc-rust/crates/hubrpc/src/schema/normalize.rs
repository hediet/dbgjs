//! Normalize a raw JSON Schema into the decidable hubrpc subset (`HubRpcJsonSchema`).
//!
//! This is the cross-language interop bridge. TS derives schemas from zod
//! (`z.toJSONSchema`), Rust derives them from `schemars`; the two emit different
//! incidental keys and defaults. By running *both* through this identical
//! normalization — keeping only [`KEPT_KEYS`], defaulting object closure,
//! synthesizing discriminators, collapsing `{}`/`{"not":{}}` to `true`/`false` —
//! the canonical subset (and therefore the interface hash) matches.
//!
//! Ported verbatim from the TS `schema/normalize.ts`.

use crate::protocol::jcs::jcs_canonicalize;
use crate::protocol::json_value::{JsonMap, JsonValue};

/// Keys that survive normalization. Everything else (annotations like
/// `examples`, `default`, `$schema`, `minLength`, …) is dropped: it is not part
/// of the decidable subset and must not influence the interface hash.
pub const KEPT_KEYS: &[&str] = &[
    "type",
    "format",
    "properties",
    "required",
    "additionalProperties",
    "items",
    "prefixItems",
    "const",
    "enum",
    "anyOf",
    "oneOf",
    "discriminator",
    "$ref",
    "title",
    "description",
];

fn is_kept_key(k: &str) -> bool {
    KEPT_KEYS.contains(&k)
}

/// Normalize a raw JSON Schema value into the hubrpc subset.
///
/// Returns `Value::Bool(true)` for top (any) and `Value::Bool(false)` for bottom
/// (never), mirroring the boolean-schema convention. Errors only on structurally
/// impossible inputs (a bare array where an object/boolean schema is required),
/// matching the TS `throw`.
pub fn normalize_json_schema(raw: &JsonValue) -> Result<JsonValue, NormalizeError> {
    match raw {
        JsonValue::Bool(b) => return Ok(JsonValue::Bool(*b)),
        JsonValue::Object(_) => {}
        JsonValue::Null => return Err(NormalizeError::ExpectedObject("null")),
        JsonValue::Array(_) => return Err(NormalizeError::ExpectedObject("array")),
        JsonValue::Number(_) => return Err(NormalizeError::ExpectedObject("number")),
        JsonValue::String(_) => return Err(NormalizeError::ExpectedObject("string")),
    }

    let r = raw.as_object().expect("checked object above");

    // {"not": {}} => false (bottom). Handle before generic descent.
    if let Some(not) = r.get("not") {
        if is_empty_object(not) {
            return Ok(JsonValue::Bool(false));
        }
    }

    let mut out = JsonMap::new();
    for (k, v) in r {
        if !is_kept_key(k) {
            continue;
        }
        if let Some(normalized) = normalize_child(k, v)? {
            out.insert(k.clone(), normalized);
        }
    }

    // Object: hubrpc requires `additionalProperties` to be set. Default to
    // closed, matching what zod typically emits for `z.object`.
    if out.get("type") == Some(&JsonValue::String("object".to_string()))
        && !out.contains_key("additionalProperties")
    {
        out.insert("additionalProperties".to_string(), JsonValue::Bool(false));
    }

    // `oneOf` without an explicit `discriminator`: try to synthesize one from
    // the branches (the JSON-Schema-only encoding of a tagged union).
    let has_one_of = out.get("oneOf").map(JsonValue::is_array).unwrap_or(false);
    if has_one_of && !out.contains_key("discriminator") {
        let branches = out["oneOf"].as_array().expect("checked array");
        if let Some(detected) = detect_discriminator(branches) {
            out.insert("discriminator".to_string(), detected);
        }
    }

    // Drop a `discriminator` that has no `oneOf` to attach to.
    if out.contains_key("discriminator") && !has_one_of {
        out.remove("discriminator");
    }

    // Collapse empty into `true` (top).
    if out.is_empty() {
        return Ok(JsonValue::Bool(true));
    }

    Ok(JsonValue::Object(out))
}

fn normalize_child(key: &str, v: &JsonValue) -> Result<Option<JsonValue>, NormalizeError> {
    match key {
        "properties" => {
            let Some(obj) = v.as_object() else {
                return Ok(Some(JsonValue::Object(JsonMap::new())));
            };
            let mut out = JsonMap::new();
            for (pk, pv) in obj {
                out.insert(pk.clone(), normalize_json_schema(pv)?);
            }
            Ok(Some(JsonValue::Object(out)))
        }
        "items" | "additionalProperties" => {
            if *v == JsonValue::Bool(false) {
                return Ok(Some(JsonValue::Bool(false)));
            }
            Ok(Some(normalize_json_schema(v)?))
        }
        "prefixItems" | "anyOf" | "oneOf" => {
            let Some(arr) = v.as_array() else {
                return Ok(Some(JsonValue::Array(Vec::new())));
            };
            let mut out = Vec::with_capacity(arr.len());
            for s in arr {
                out.push(normalize_json_schema(s)?);
            }
            Ok(Some(JsonValue::Array(out)))
        }
        "discriminator" => {
            // Only `propertyName` is part of the subset.
            let Some(obj) = v.as_object() else {
                return Ok(None);
            };
            match obj.get("propertyName") {
                Some(JsonValue::String(name)) if !name.is_empty() => {
                    let mut m = JsonMap::new();
                    m.insert("propertyName".to_string(), JsonValue::String(name.clone()));
                    Ok(Some(JsonValue::Object(m)))
                }
                _ => Ok(None),
            }
        }
        "enum" => {
            let Some(arr) = v.as_array() else {
                return Ok(Some(JsonValue::Array(Vec::new())));
            };
            Ok(Some(JsonValue::Array(arr.clone())))
        }
        "required" => {
            let Some(arr) = v.as_array() else {
                return Ok(Some(JsonValue::Array(Vec::new())));
            };
            let mut items = arr.clone();
            // JS `Array.prototype.sort()` with default comparator: lexicographic
            // by UTF-16 code units of the string form. Required entries are
            // strings; mirror that ordering.
            items.sort_by(|a, b| {
                let sa = a.as_str().unwrap_or("");
                let sb = b.as_str().unwrap_or("");
                sa.encode_utf16().cmp(sb.encode_utf16())
            });
            Ok(Some(JsonValue::Array(items)))
        }
        _ => Ok(Some(v.clone())),
    }
}

fn is_empty_object(v: &JsonValue) -> bool {
    v.as_object().map(|o| o.is_empty()).unwrap_or(false)
}

/// Inspect a `oneOf` branch list and synthesize a `discriminator` when every
/// branch is an object schema sharing exactly one property name carrying a
/// distinct `const` value. Mirrors `z.toJSONSchema(z.discriminatedUnion(...))`.
fn detect_discriminator(branches: &[JsonValue]) -> Option<JsonValue> {
    if branches.len() < 2 {
        return None;
    }

    let mut branch_props: Vec<&JsonMap<String, JsonValue>> = Vec::with_capacity(branches.len());
    for b in branches {
        let obj = b.as_object()?;
        if obj.get("type") != Some(&JsonValue::String("object".to_string())) {
            return None;
        }
        let props = obj.get("properties")?.as_object()?;
        branch_props.push(props);
    }

    let mut candidates = const_prop_names(branch_props[0]);
    for props in &branch_props[1..] {
        let names = const_prop_names(props);
        candidates.retain(|n| names.contains(n));
        if candidates.is_empty() {
            return None;
        }
    }

    for name in &candidates {
        let mut seen = std::collections::HashSet::new();
        let mut all_distinct = true;
        for props in &branch_props {
            let const_val = props
                .get(name)
                .and_then(|s| s.get("const"))
                .cloned()
                .unwrap_or(JsonValue::Null);
            let key = jcs_canonicalize(&const_val).unwrap_or_default();
            if !seen.insert(key) {
                all_distinct = false;
                break;
            }
        }
        if all_distinct {
            let mut m = JsonMap::new();
            m.insert("propertyName".to_string(), JsonValue::String(name.clone()));
            return Some(JsonValue::Object(m));
        }
    }

    None
}

fn const_prop_names(props: &JsonMap<String, JsonValue>) -> Vec<String> {
    let mut out = Vec::new();
    for (k, v) in props {
        if v.is_boolean() {
            continue;
        }
        if v.get("const").is_some() {
            out.push(k.clone());
        }
    }
    out
}

/// Error returned by [`normalize_json_schema`] for structurally impossible input.
#[derive(Debug, thiserror::Error)]
pub enum NormalizeError {
    #[error("normalizeJsonSchema: expected object, got {0}")]
    ExpectedObject(&'static str),
}
