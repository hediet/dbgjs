//! Conformance tests: load the committed interop vectors (generated from the TS reference
//! behavior in `conformance/generate/gen.mjs`) and assert the Rust impl matches.
//!
//! A red test here means the Rust wire behavior has diverged from TypeScript.

use std::path::PathBuf;

use hubrpc::prelude::*;
use hubrpc::protocol::jcs::jcs_canonicalize;
use hubrpc::schema::{compute_interface_hash_value, normalize_json_schema};
use serde_json::Value;

fn vectors_dir() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("..")
        .join("..")
        .join("conformance")
        .join("vectors")
}

fn load(name: &str) -> Value {
    let path = vectors_dir().join(name);
    let text =
        std::fs::read_to_string(&path).unwrap_or_else(|e| panic!("read {}: {e}", path.display()));
    serde_json::from_str(&text).expect("vector json parses")
}

#[test]
fn jcs_matches_reference() {
    let cases = load("jcs.json");
    for case in cases.as_array().unwrap() {
        let value = &case["value"];
        let expected = case["canonical"].as_str().unwrap();
        let got = jcs_canonicalize(value).expect("canonicalizes");
        assert_eq!(got, expected, "JCS mismatch for value {value}");
    }
}

#[test]
fn method_name_matches_reference() {
    let cases = load("method_name.json");
    for case in cases.as_array().unwrap() {
        let input = case["input"].as_str().unwrap();
        let expected = &case["expected"];
        let parsed = parse_method_name(input);
        if expected.is_null() {
            assert!(parsed.is_none(), "expected malformed for {input:?}");
            continue;
        }
        let parsed = parsed.unwrap_or_else(|| panic!("expected a parse for {input:?}"));
        let (kind, value) = describe(&parsed);
        assert_eq!(expected["kind"], kind, "kind mismatch for {input:?}");
        assert_eq!(*expected, value, "fields mismatch for {input:?}");
        // Round-trip: formatting a parsed name yields the original string.
        assert_eq!(format_method_name(&parsed), input);
    }
}

fn describe(p: &ParsedMethodName) -> (&'static str, Value) {
    match p {
        ParsedMethodName::Bare { member } => (
            "bare",
            serde_json::json!({ "kind": "bare", "member": member }),
        ),
        ParsedMethodName::Interface {
            interface_id,
            member,
        } => (
            "interface",
            serde_json::json!({ "kind": "interface", "interfaceId": interface_id, "member": member }),
        ),
        ParsedMethodName::Full {
            service_id,
            interface_id,
            member,
        } => (
            "full",
            serde_json::json!({
                "kind": "full",
                "serviceId": service_id,
                "interfaceId": interface_id,
                "member": member,
            }),
        ),
    }
}

#[test]
fn framing_round_trips() {
    let cases = load("framing.json");
    for case in cases.as_array().unwrap() {
        let value = &case["value"];
        let msg: JsonRpcMessage =
            serde_json::from_value(value.clone()).expect("framing parses to a message");
        let back = serde_json::to_value(&msg).expect("message serializes");
        assert_eq!(back, *value, "framing round-trip mismatch for {value}");
    }
}

#[test]
fn interface_hash_matches_reference() {
    let cases = load("interface_hash.json");
    for case in cases.as_array().unwrap() {
        let name = case["name"].as_str().unwrap();
        let schema = &case["schema"];
        let expected = case["hash"].as_str().unwrap();
        let got = compute_interface_hash_value(schema);
        assert_eq!(
            got, expected,
            "interface hash mismatch for fixture {name:?}"
        );
    }
}

/// The `rich-extensions` fixture (carrying `x-…` specification extensions at the
/// interface, method, and JSON-Schema levels) and its `rich-extensions-stripped`
/// twin (the same wire contract with the extensions removed) MUST hash
/// identically — in Rust and in the TS-authored corpus. This is the
/// cross-language proof that rich-only material never affects identity while the
/// simple wire contract does.
#[test]
fn rich_extensions_hash_identically_to_stripped_twin() {
    let cases = load("interface_hash.json");
    let by_name = |n: &str| {
        cases
            .as_array()
            .unwrap()
            .iter()
            .find(|c| c["name"] == n)
            .unwrap_or_else(|| panic!("fixture {n:?} missing"))
    };
    let rich = &by_name("rich-extensions")["schema"];
    let stripped = &by_name("rich-extensions-stripped")["schema"];
    assert_eq!(
        compute_interface_hash_value(rich),
        compute_interface_hash_value(stripped),
        "x-* extensions must not change the interface hash"
    );
}

#[test]
fn normalize_matches_reference() {
    let cases = load("normalize.json");
    for case in cases.as_array().unwrap() {
        let name = case["name"].as_str().unwrap();
        let raw = &case["raw"];
        let expected = &case["normalized"];
        let got = normalize_json_schema(raw)
            .unwrap_or_else(|e| panic!("normalize failed for {name:?}: {e}"));
        assert_eq!(got, *expected, "normalize mismatch for fixture {name:?}");
    }
}
