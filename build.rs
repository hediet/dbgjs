use std::collections::BTreeSet;
use std::env;
use std::fs;
use std::path::{Path, PathBuf};

use hubrpc::prelude::{GenerateRustOptions, generate_rust_interface};

#[path = "src/protocol_schema.rs"]
mod protocol_schema;

fn main() {
    let repository_root =
        PathBuf::from(env::var_os("CARGO_MANIFEST_DIR").expect("CARGO_MANIFEST_DIR is set"));
    let protocol_root = repository_root
        .join("node_modules")
        .join("devtools-protocol")
        .join("json");
    let browser_protocol = protocol_root.join("browser_protocol.json");
    let js_protocol = protocol_root.join("js_protocol.json");
    println!("cargo:rerun-if-changed={}", browser_protocol.display());
    println!("cargo:rerun-if-changed={}", js_protocol.display());
    println!("cargo:rerun-if-changed=src/protocol_schema.rs");

    let browser = read_protocol(&browser_protocol);
    let js = read_protocol(&js_protocol);
    let interface = protocol_schema::import_typed_cdp_protocol(&browser, &js)
        .expect("CDP protocol must import as a typed HubRPC interface");
    let generated = generate_rust_interface(
        &interface,
        &GenerateRustOptions {
            client_name: Some("CdpClient".into()),
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

fn read_protocol(path: &Path) -> String {
    fs::read_to_string(path).unwrap_or_else(|error| {
        panic!(
            "failed to read pinned CDP schema {}: {error}. Run `npm ci` first",
            path.display()
        )
    })
}
