//! Shared helpers for the two places where jsdbg *implements* the CDP `Target` domain instead of
//! consuming it: the context relay (jsdbg as a CDP server for external clients) and the virtual
//! browser root that fronts a process tree ([`crate::virtual_browser_root`]).

use hubrpc::prelude::{JsonRpcError, error_codes};
use serde::de::DeserializeOwned;
use serde_json::Value;

use crate::cdp::TargetTargetInfo;
use crate::service_api::TargetSnapshot;

pub fn target_info_from_snapshot(snapshot: &TargetSnapshot) -> TargetTargetInfo {
    let mut info = TargetTargetInfo::new(
        snapshot.target_id.clone(),
        snapshot.target_type.clone(),
        snapshot.title.clone(),
        snapshot.url.clone(),
        snapshot.attached,
        false,
    );
    info.parent_id = snapshot.parent_id.clone();
    info.opener_id = snapshot.opener_id.clone();
    info.browser_context_id = snapshot.browser_context_id.clone();
    info.subtype = snapshot.subtype.clone();
    info
}

pub fn to_json(value: impl serde::Serialize) -> Result<Value, JsonRpcError> {
    serde_json::to_value(value)
        .map_err(|error| JsonRpcError::new(error_codes::INTERNAL_ERROR, error.to_string()))
}

pub fn from_json<T: DeserializeOwned>(params: Value) -> Result<T, JsonRpcError> {
    serde_json::from_value(params).map_err(|error| invalid_params(error.to_string()))
}

pub fn invalid_params(message: impl Into<String>) -> JsonRpcError {
    JsonRpcError::new(error_codes::INVALID_PARAMS, message.into())
}
