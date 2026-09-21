use std::process::Command;

use dbgjs::service_api::debugger_service_api;
use linkrpc::prelude::{LinkRpcInterfaceSchema, compute_interface_hash};
use serde_json::{Value, json};

const CONTRACT_BUNDLE: &str = include_str!("../schemas/dbgjs.interfaces.json");
const CDP_INTERFACE_ID: &str = "cdp.protocol";
const DAEMON_INTERFACE_ID: &str = "dev.dbgjs.cdp-debugger";

fn bundle() -> Value {
    serde_json::from_str(CONTRACT_BUNDLE).expect("checked-in contract bundle is valid JSON")
}

fn interface(bundle: &Value, id: &str) -> LinkRpcInterfaceSchema {
    let matching = bundle["interfaceSchemas"]
        .as_array()
        .expect("interfaceSchemas is an array")
        .iter()
        .filter(|schema| schema["id"] == id)
        .collect::<Vec<_>>();
    let [schema] = matching.as_slice() else {
        panic!("expected exactly one {id} schema, found {}", matching.len());
    };
    serde_json::from_value((*schema).clone()).expect("interface is a valid LinkRPC schema")
}

#[test]
fn canonical_bundle_contains_complete_hashed_interfaces() {
    let bundle = bundle();
    let cdp = interface(&bundle, CDP_INTERFACE_ID);
    let daemon = interface(&bundle, DAEMON_INTERFACE_ID);

    assert_eq!(cdp.methods.len(), 896);
    assert_eq!(
        cdp.components
            .as_ref()
            .and_then(|components| components.schemas.as_ref())
            .map(|schemas| schemas.len()),
        Some(607)
    );
    assert_eq!(compute_interface_hash(&cdp), cdp.hash);
    assert_eq!(compute_interface_hash(&daemon), daemon.hash);
    assert_eq!(cdp, dbgjs::cdp::interface().to_schema());
    assert_eq!(daemon, debugger_service_api::interface().to_schema());
}

#[test]
fn canonical_bundle_advertises_only_the_daemon_at_the_root() {
    let bundle = bundle();
    let daemon = interface(&bundle, DAEMON_INTERFACE_ID);
    let expected_reference = json!({
        "interfaceId": DAEMON_INTERFACE_ID,
        "interfaceHash": daemon.hash,
    });
    assert_eq!(
        bundle["services"],
        json!([{
            "serviceId": "",
            "interfaces": [expected_reference.clone()],
        }])
    );
    assert_eq!(bundle["defaultInterface"], expected_reference);
}

#[test]
fn checked_in_contract_bundle_is_not_stale() {
    let status = Command::new(env!("CARGO_BIN_EXE_export_contracts"))
        .arg("--check")
        .status()
        .expect("contract exporter starts");
    assert!(status.success(), "contract exporter reported drift");
}
