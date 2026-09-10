//! Regenerates the golden `generated_graph.rs` from `graph_interface.json`.
//!
//! Run with `cargo test -p hubrpc --test codegen_bless -- --ignored --nocapture`
//! to (re)write the golden file after intentional generator changes.

use std::path::PathBuf;

use hubrpc::schema::codegen::{generate_rust_interface, GenerateRustOptions};
use hubrpc::schema::interface_schema::HubRpcInterfaceSchema;

fn dir() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("tests")
        .join("codegen")
}

fn generate() -> hubrpc::schema::codegen::GeneratedRust {
    let text = std::fs::read_to_string(dir().join("graph_interface.json")).unwrap();
    let schema: HubRpcInterfaceSchema = serde_json::from_str(&text).unwrap();
    generate_rust_interface(&schema, &GenerateRustOptions::default())
}

#[test]
#[ignore = "regenerates the golden file on demand"]
fn bless_golden() {
    let generated = generate();
    let target = dir().join("generated_graph.rs");
    std::fs::write(&target, generated.code).unwrap();
    eprintln!("wrote {}", target.display());
    eprintln!("unsupported constructs: {:#?}", generated.unsupported);
}
