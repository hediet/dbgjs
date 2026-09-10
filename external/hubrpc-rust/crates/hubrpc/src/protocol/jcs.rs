//! RFC 8785 JSON Canonicalization Scheme (JCS).
//!
//! Byte-deterministic JSON encoding used under every hubrpc signature. Both signer and
//! verifier canonicalize the same value and obtain byte-identical UTF-8.
//!
//! Ported to match the TS `protocol/jcs.ts` exactly, which canonicalizes via JavaScript
//! `JSON.stringify` for primitives and `Object.keys(obj).sort()` (UTF-16 code-unit order)
//! for object keys. We therefore replicate **JS `JSON.stringify`** semantics:
//!   - object keys sorted by UTF-16 code units;
//!   - JS string escaping (`\b \t \n \f \r`, other control chars as lowercase `\u00xx`,
//!     non-ASCII emitted raw);
//!   - ECMAScript `Number::toString` number formatting.
//!
//! Number formatting note: integers (and integer-valued floats below 1e21) are exact.
//! Non-integer floats use a best-effort formatter that can diverge from ES6 in exponent
//! edge cases — covered by the `jcs` conformance vectors (a known interop risk; RPC
//! payloads in practice use integers/strings/bools).

use crate::protocol::json_value::JsonValue;

/// Error returned when a value cannot be canonicalized (non-finite numbers).
#[derive(Debug, thiserror::Error)]
pub enum JcsError {
    #[error("jcs: non-finite number is not allowed")]
    NonFinite,
}

/// RFC 8785 canonical JSON string for `value`.
pub fn jcs_canonicalize(value: &JsonValue) -> Result<String, JcsError> {
    let mut out = String::new();
    write_value(&mut out, value)?;
    Ok(out)
}

/// UTF-8 bytes of the canonical JSON — the thing actually signed / hashed.
pub fn jcs_canonicalize_bytes(value: &JsonValue) -> Result<Vec<u8>, JcsError> {
    Ok(jcs_canonicalize(value)?.into_bytes())
}

fn write_value(out: &mut String, value: &JsonValue) -> Result<(), JcsError> {
    match value {
        JsonValue::Null => out.push_str("null"),
        JsonValue::Bool(true) => out.push_str("true"),
        JsonValue::Bool(false) => out.push_str("false"),
        JsonValue::Number(n) => write_number(out, n)?,
        JsonValue::String(s) => write_js_string(out, s),
        JsonValue::Array(items) => {
            out.push('[');
            for (i, item) in items.iter().enumerate() {
                if i > 0 {
                    out.push(',');
                }
                write_value(out, item)?;
            }
            out.push(']');
        }
        JsonValue::Object(map) => {
            let mut keys: Vec<&String> = map.keys().collect();
            keys.sort_by(|a, b| cmp_utf16(a, b));
            out.push('{');
            for (i, key) in keys.iter().enumerate() {
                if i > 0 {
                    out.push(',');
                }
                write_js_string(out, key);
                out.push(':');
                write_value(out, &map[*key])?;
            }
            out.push('}');
        }
    }
    Ok(())
}

/// Compare two strings by UTF-16 code units (JavaScript default string ordering).
fn cmp_utf16(a: &str, b: &str) -> std::cmp::Ordering {
    a.encode_utf16().cmp(b.encode_utf16())
}

fn write_number(out: &mut String, n: &serde_json::Number) -> Result<(), JcsError> {
    if let Some(i) = n.as_i64() {
        out.push_str(itoa(i).as_str());
        return Ok(());
    }
    if let Some(u) = n.as_u64() {
        out.push_str(u.to_string().as_str());
        return Ok(());
    }
    let f = n.as_f64().ok_or(JcsError::NonFinite)?;
    if !f.is_finite() {
        return Err(JcsError::NonFinite);
    }
    out.push_str(&format_f64(f));
    Ok(())
}

fn itoa(i: i64) -> String {
    i.to_string()
}

/// Best-effort ECMAScript `Number::toString` for non-integer f64. Integer-valued floats
/// below 1e21 are emitted without a decimal point (matching ES6); other values fall back
/// to Rust's shortest `Display`, which can diverge in exponent edge cases (see module note).
fn format_f64(f: f64) -> String {
    if f.fract() == 0.0 && f.abs() < 1e21 {
        // Integer-valued: ES6 prints no decimal point.
        return format!("{}", f as i128);
    }
    format!("{f}")
}

/// Append `s` as a JS-`JSON.stringify`-escaped string (with surrounding quotes).
fn write_js_string(out: &mut String, s: &str) {
    out.push('"');
    for c in s.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\u{0008}' => out.push_str("\\b"),
            '\u{0009}' => out.push_str("\\t"),
            '\u{000A}' => out.push_str("\\n"),
            '\u{000C}' => out.push_str("\\f"),
            '\u{000D}' => out.push_str("\\r"),
            c if (c as u32) < 0x20 => {
                out.push_str(&format!("\\u{:04x}", c as u32));
            }
            c => out.push(c),
        }
    }
    out.push('"');
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn sorts_keys_and_strips_whitespace() {
        let v = json!({ "b": 1, "a": 2 });
        assert_eq!(jcs_canonicalize(&v).unwrap(), r#"{"a":2,"b":1}"#);
    }

    #[test]
    fn integers_have_no_decimal() {
        let v = json!({ "n": 1.0, "m": 42 });
        assert_eq!(jcs_canonicalize(&v).unwrap(), r#"{"m":42,"n":1}"#);
    }

    #[test]
    fn escapes_control_chars_like_js() {
        let v = json!("a\tb\nc\u{0001}d");
        assert_eq!(jcs_canonicalize(&v).unwrap(), r#""a\tb\nc\u0001d""#);
    }

    #[test]
    fn nested_arrays_and_objects() {
        let v = json!({ "z": [3, 2, 1], "a": { "y": true, "x": null } });
        assert_eq!(
            jcs_canonicalize(&v).unwrap(),
            r#"{"a":{"x":null,"y":true},"z":[3,2,1]}"#
        );
    }
}
