use std::collections::BTreeSet;
use std::env;
use std::fs;
use std::path::PathBuf;

use linkrpc::prelude::{
    GenerateRustOptions, LinkRpcInterfaceSchema, compute_interface_hash, generate_rust_interface,
};

fn main() {
    let crate_root =
        PathBuf::from(env::var_os("CARGO_MANIFEST_DIR").expect("CARGO_MANIFEST_DIR is set"));
    let bundle_path = crate_root
        .join("..")
        .join("..")
        .join("schemas")
        .join("dbgjs.interfaces.json");
    println!("cargo:rerun-if-changed={}", bundle_path.display());

    let bundle: serde_json::Value =
        serde_json::from_str(&fs::read_to_string(&bundle_path).unwrap_or_else(|error| {
            panic!(
                "failed to read canonical contract bundle {}: {error}. \
                 Run `node scripts/generate-contracts.mjs` first",
                bundle_path.display()
            )
        }))
        .expect("canonical contract bundle must be valid JSON");
    let schemas = bundle["interfaceSchemas"]
        .as_array()
        .expect("canonical contract bundle must contain interfaceSchemas");
    let matches = schemas
        .iter()
        .filter(|schema| schema["id"] == "cdp.protocol")
        .collect::<Vec<_>>();
    let [schema] = matches.as_slice() else {
        panic!(
            "canonical contract bundle must contain exactly one cdp.protocol interface, found {}",
            matches.len()
        );
    };
    let interface: LinkRpcInterfaceSchema = serde_json::from_value((*schema).clone())
        .expect("canonical cdp.protocol contract must be a valid LinkRPC interface schema");
    let actual_hash = compute_interface_hash(&interface);
    assert_eq!(
        interface.hash, actual_hash,
        "canonical cdp.protocol contract has a stale interface hash"
    );
    let generated = generate_rust_interface(
        &interface,
        &GenerateRustOptions {
            client_name: Some("CdpClient".into()),
            generate_server: true,
            default_server_methods: true,
            ..GenerateRustOptions::default()
        },
    );
    verify_expected_fallbacks(&generated.unsupported);

    let module_code = generated
        .code
        .lines()
        .filter(|line| !line.starts_with("#!"))
        .collect::<Vec<_>>()
        .join("\n");
    let output =
        PathBuf::from(env::var_os("OUT_DIR").expect("OUT_DIR is set")).join("cdp_generated.rs");
    fs::write(output, module_code).expect("generated CDP Rust source must be writable");
}

fn verify_expected_fallbacks(unsupported: &[String]) {
    const EXPECTED: [&str; 7] = [
        "AccessibilityAxvalueValue",
        "DomShapeOutsideInfoMarginShapeItem",
        "DomShapeOutsideInfoShapeItem",
        "RuntimeCallArgumentValue",
        "RuntimeDeepSerializedValueValue",
        "RuntimeRemoteObjectValue",
        "WebMcpToolRespondedParamsOutput",
    ];
    let expected: BTreeSet<_> = EXPECTED.into_iter().collect();
    let actual: BTreeSet<_> = unsupported
        .iter()
        .filter_map(|message| {
            message
                .strip_prefix("component `")
                .and_then(|message| message.split_once('`'))
                .map(|(component, _)| component)
        })
        .collect();
    if actual != expected || actual.len() != unsupported.len() {
        panic!(
            "CDP schema introduced unexpected Rust codegen fallbacks:\n{}",
            unsupported.join("\n")
        );
    }
}
