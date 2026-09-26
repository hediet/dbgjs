//! Shared helpers for the two places where dbgjs *implements* the CDP `Target` domain instead of
//! consuming it: the context relay (dbgjs as a CDP server for external clients) and the virtual
//! browser root that fronts a process tree ([`crate::service::virtual_browser_root`]).

use linkrpc::prelude::{JsonRpcError, error_codes};
use serde_json::{Map, Value};

use crate::cdp::TargetTargetInfo;
use crate::api::service_api::TargetSnapshot;

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

pub fn target_snapshot_from_info(target: TargetTargetInfo) -> TargetSnapshot {
    TargetSnapshot {
        target_id: target.target_id,
        target_type: target.r#type,
        title: target.title,
        url: target.url,
        attached: target.attached,
        parent_id: target.parent_id,
        opener_id: target.opener_id,
        browser_context_id: target.browser_context_id,
        subtype: target.subtype,
    }
}

pub fn invalid_params(message: impl Into<String>) -> JsonRpcError {
    JsonRpcError::new(error_codes::INVALID_PARAMS, message.into())
}

pub fn normalize_typed_cdp_params(params: Value) -> Value {
    if params.is_null() {
        Value::Object(Map::new())
    } else {
        params
    }
}
