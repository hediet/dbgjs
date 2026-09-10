//! JSON-RPC 2.0 wire types for the hubrpc dialect (port of TS `protocol/jsonRpc.ts`).
//!
//! Serialization is hand-rolled so the three message kinds are classified exactly like the
//! TS side: by the presence of the `method` / `id` / `result` / `error` keys. This keeps a
//! `"result": null` response distinct from one with no `result` key, which a derived
//! `Option` would collapse.

use serde::de::Error as _;
use serde::{Deserialize, Deserializer, Serialize, Serializer};

use crate::protocol::json_value::{JsonMap, JsonValue};

/// JSON-RPC request / response id. Numbers or strings (matches TS `RequestId`).
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub enum RequestId {
    Number(i64),
    String(String),
}

impl RequestId {
    fn to_value(&self) -> JsonValue {
        match self {
            RequestId::Number(n) => JsonValue::from(*n),
            RequestId::String(s) => JsonValue::from(s.clone()),
        }
    }

    fn from_value(v: &JsonValue) -> Option<RequestId> {
        match v {
            JsonValue::Number(n) => n.as_i64().map(RequestId::Number),
            JsonValue::String(s) => Some(RequestId::String(s.clone())),
            _ => None,
        }
    }
}

/// The `{ code, message, data? }` error payload object.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct JsonRpcError {
    pub code: i64,
    pub message: String,
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub data: Option<JsonValue>,
}

impl JsonRpcError {
    pub fn new(code: i64, message: impl Into<String>) -> Self {
        Self {
            code,
            message: message.into(),
            data: None,
        }
    }
}

#[derive(Clone, Debug, PartialEq)]
pub struct JsonRpcRequest {
    pub id: RequestId,
    pub method: String,
    pub params: Option<JsonValue>,
}

#[derive(Clone, Debug, PartialEq)]
pub struct JsonRpcNotification {
    pub method: String,
    pub params: Option<JsonValue>,
}

/// A response is either success (`result`) or failure (`error`); the `id` may be `null`
/// (an error produced before a request id could be determined).
#[derive(Clone, Debug, PartialEq)]
pub struct JsonRpcResponse {
    pub id: Option<RequestId>,
    pub payload: ResponsePayload,
}

#[derive(Clone, Debug, PartialEq)]
pub enum ResponsePayload {
    Result(JsonValue),
    Error(JsonRpcError),
}

#[derive(Clone, Debug, PartialEq)]
pub enum JsonRpcMessage {
    Request(JsonRpcRequest),
    Notification(JsonRpcNotification),
    Response(JsonRpcResponse),
}

impl JsonRpcMessage {
    /// Build the wire `serde_json::Value` for this message (always a `"2.0"` object).
    fn to_value(&self) -> JsonValue {
        let mut map = JsonMap::new();
        map.insert("jsonrpc".into(), JsonValue::from("2.0"));
        match self {
            JsonRpcMessage::Request(r) => {
                map.insert("id".into(), r.id.to_value());
                map.insert("method".into(), JsonValue::from(r.method.clone()));
                if let Some(p) = &r.params {
                    map.insert("params".into(), p.clone());
                }
            }
            JsonRpcMessage::Notification(n) => {
                map.insert("method".into(), JsonValue::from(n.method.clone()));
                if let Some(p) = &n.params {
                    map.insert("params".into(), p.clone());
                }
            }
            JsonRpcMessage::Response(resp) => {
                map.insert(
                    "id".into(),
                    resp.id
                        .as_ref()
                        .map(RequestId::to_value)
                        .unwrap_or(JsonValue::Null),
                );
                match &resp.payload {
                    ResponsePayload::Result(v) => {
                        map.insert("result".into(), v.clone());
                    }
                    ResponsePayload::Error(e) => {
                        map.insert(
                            "error".into(),
                            serde_json::to_value(e).expect("error serializes"),
                        );
                    }
                }
            }
        }
        JsonValue::Object(map)
    }

    /// Classify a wire object into one of the three message kinds (key-presence based,
    /// matching the TS predicates).
    fn from_value(v: JsonValue) -> Result<JsonRpcMessage, String> {
        let map = match v {
            JsonValue::Object(m) => m,
            _ => return Err("jsonrpc message must be an object".into()),
        };
        let has_method = map.contains_key("method");
        let has_id = map.contains_key("id");
        let has_result = map.contains_key("result");
        let has_error = map.contains_key("error");

        if has_method && has_id {
            let id = RequestId::from_value(&map["id"]).ok_or("invalid request id")?;
            let method = map["method"]
                .as_str()
                .ok_or("method must be a string")?
                .to_string();
            return Ok(JsonRpcMessage::Request(JsonRpcRequest {
                id,
                method,
                params: map.get("params").cloned(),
            }));
        }
        if has_method && !has_id {
            let method = map["method"]
                .as_str()
                .ok_or("method must be a string")?
                .to_string();
            return Ok(JsonRpcMessage::Notification(JsonRpcNotification {
                method,
                params: map.get("params").cloned(),
            }));
        }
        if !has_method && (has_result || has_error) {
            let id = match map.get("id") {
                Some(JsonValue::Null) | None => None,
                Some(v) => Some(RequestId::from_value(v).ok_or("invalid response id")?),
            };
            let payload = if has_error {
                let e: JsonRpcError =
                    serde_json::from_value(map["error"].clone()).map_err(|e| e.to_string())?;
                ResponsePayload::Error(e)
            } else {
                ResponsePayload::Result(map["result"].clone())
            };
            return Ok(JsonRpcMessage::Response(JsonRpcResponse { id, payload }));
        }
        Err("object is not a valid JSON-RPC request, notification, or response".into())
    }
}

impl Serialize for JsonRpcMessage {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        self.to_value().serialize(serializer)
    }
}

impl<'de> Deserialize<'de> for JsonRpcMessage {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let v = JsonValue::deserialize(deserializer)?;
        JsonRpcMessage::from_value(v).map_err(D::Error::custom)
    }
}

/// JSON-RPC + hubrpc error codes (port of TS `ErrorCode`).
pub mod error_codes {
    pub const PARSE_ERROR: i64 = -32700;
    pub const INVALID_REQUEST: i64 = -32600;
    pub const METHOD_NOT_FOUND: i64 = -32601;
    pub const INVALID_PARAMS: i64 = -32602;
    pub const INTERNAL_ERROR: i64 = -32603;
    /// Caller is authenticated but lacks a capability covering the call.
    pub const PERMISSION_REQUIRED: i64 = -32401;
    /// The peer a request was routed to detached before it could respond.
    pub const PEER_DISCONNECTED: i64 = -32402;
    /// The request exceeded the hub's idle timeout with no stream activity.
    pub const REQUEST_TIMEOUT: i64 = -32403;
    /// The request was cancelled (by the caller, or by the hub on disconnect).
    pub const CANCELLED: i64 = -32800;
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn round_trip(v: JsonValue) {
        let msg: JsonRpcMessage = serde_json::from_value(v.clone()).unwrap();
        let back = serde_json::to_value(&msg).unwrap();
        assert_eq!(back, v);
    }

    #[test]
    fn request_round_trips() {
        round_trip(json!({"jsonrpc":"2.0","id":1,"method":"s::i::m","params":{"a":1}}));
        round_trip(json!({"jsonrpc":"2.0","id":"abc","method":"list"}));
    }

    #[test]
    fn notification_round_trips() {
        round_trip(json!({"jsonrpc":"2.0","method":"ping","params":[1,2]}));
    }

    #[test]
    fn response_round_trips() {
        round_trip(json!({"jsonrpc":"2.0","id":1,"result":{"ok":true}}));
        round_trip(json!({"jsonrpc":"2.0","id":null,"result":null}));
        round_trip(json!({"jsonrpc":"2.0","id":7,"error":{"code":-32601,"message":"nope"}}));
    }

    #[test]
    fn classifies_kinds() {
        let req: JsonRpcMessage =
            serde_json::from_value(json!({"jsonrpc":"2.0","id":1,"method":"m"})).unwrap();
        assert!(matches!(req, JsonRpcMessage::Request(_)));
        let note: JsonRpcMessage =
            serde_json::from_value(json!({"jsonrpc":"2.0","method":"m"})).unwrap();
        assert!(matches!(note, JsonRpcMessage::Notification(_)));
        let resp: JsonRpcMessage =
            serde_json::from_value(json!({"jsonrpc":"2.0","id":1,"result":5})).unwrap();
        assert!(matches!(resp, JsonRpcMessage::Response(_)));
    }
}
