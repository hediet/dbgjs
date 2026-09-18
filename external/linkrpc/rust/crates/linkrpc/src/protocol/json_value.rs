//! The linkrpc `JsonValue` is `serde_json::Value`. This module re-exports it under the
//! linkrpc name so call sites read intentionally and we have one place to change later.

pub use serde_json::{Map as JsonMap, Number as JsonNumber, Value as JsonValue};

/// Convenience: the canonical "no params" value (`null`).
pub fn null() -> JsonValue {
    JsonValue::Null
}
