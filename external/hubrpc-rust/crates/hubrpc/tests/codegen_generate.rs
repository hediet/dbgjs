//! Determinism + no-drift guard for the generator.
//!
//! Regenerates the interface from the same schema and asserts the output is
//! byte-identical to the committed golden file (`generated_graph.rs`). If this
//! fails after an intentional generator change, re-bless with:
//! `cargo test -p hubrpc --test codegen_bless -- --ignored`.

use std::path::PathBuf;

use hubrpc::schema::codegen::{generate_rust_interface, GenerateRustOptions};
use hubrpc::schema::interface_schema::HubRpcInterfaceSchema;

fn codegen_dir() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("tests")
        .join("codegen")
}

fn generate() -> hubrpc::schema::codegen::GeneratedRust {
    let text = std::fs::read_to_string(codegen_dir().join("graph_interface.json")).unwrap();
    let schema: HubRpcInterfaceSchema = serde_json::from_str(&text).unwrap();
    generate_rust_interface(&schema, &GenerateRustOptions::default())
}

fn schema(json: &str) -> HubRpcInterfaceSchema {
    serde_json::from_str(json).expect("valid interface schema")
}

#[test]
fn matches_golden_file() {
    let generated = generate();
    let golden = std::fs::read_to_string(codegen_dir().join("generated_graph.rs")).unwrap();
    // Normalize line endings so the test is stable across platforms/git config.
    let got = generated.code.replace("\r\n", "\n");
    let want = golden.replace("\r\n", "\n");
    assert_eq!(
        got, want,
        "generated code drifted from golden file; re-bless with `cargo test -p hubrpc --test codegen_bless -- --ignored`"
    );
}

#[test]
fn output_is_deterministic() {
    let a = generate().code;
    let b = generate().code;
    assert_eq!(a, b, "generator must be deterministic across runs");
}

#[test]
fn no_unsupported_constructs_for_this_schema() {
    let generated = generate();
    assert!(
        generated.unsupported.is_empty(),
        "unexpected unsupported constructs: {:#?}",
        generated.unsupported
    );
}

#[test]
fn recursion_uses_box_indirection() {
    let code = generate().code;
    // Direct self / mutual recursion is broken with Box.
    assert!(
        code.contains("pub parent: Option<Box<TreeNode>>"),
        "self-recursive field should be boxed"
    );
    assert!(
        code.contains("pub pong: Option<Box<Pong>>"),
        "mutually recursive field should be boxed"
    );
    assert!(
        code.contains("pub ping: Option<Box<Ping>>"),
        "mutually recursive field should be boxed"
    );
    // Recursion behind Vec/Map must NOT be boxed (heap indirection already).
    assert!(
        code.contains("Variant3(Vec<JsonNode>)"),
        "vec recursion should not be boxed"
    );
    assert!(
        code.contains("Variant4(std::collections::HashMap<String, JsonNode>)"),
        "map recursion should not be boxed"
    );
}

#[test]
fn components_emit_in_sorted_order() {
    let code = generate().code;
    let pos = |needle: &str| {
        code.find(needle)
            .unwrap_or_else(|| panic!("missing {needle}"))
    };
    // BTreeMap ordering: Bag < Color < JsonNode < ... < TreeNode.
    assert!(pos("pub struct Bag") < pos("pub enum Color"));
    assert!(pos("pub enum Color") < pos("pub enum JsonNode"));
    assert!(pos("pub enum JsonNode") < pos("pub struct TreeNode"));
}

#[test]
fn server_notification_emits_no_client_send_method() {
    let code = generate().code;
    // The event surfaces as an addressed-name accessor...
    assert!(
        code.contains("pub fn tree_changed_event_name(&self) -> String"),
        "server notification should expose its addressed wire name"
    );
    assert!(
        code.contains("self.method_name(\"tree_changed\")"),
        "event name should be prefix-addressed"
    );
    // ...but NOT as a client send method (that would be a client→server call).
    assert!(
        !code.contains("pub async fn tree_changed"),
        "no client send method should be generated for a server notification"
    );
    assert!(
        !code.contains("self.caller.notify(&self.method_name(\"tree_changed\")"),
        "server notification must not be sent by the client"
    );
    // A genuine client request/notification is still generated normally.
    assert!(code.contains("pub async fn get_tree"));
    assert!(code.contains("pub async fn notify_changed"));
}

#[test]
fn extensions_do_not_leak_into_generated_code() {
    // `x-*` keys are codegen-visible but their values must not appear as data.
    let code = generate().code;
    assert!(!code.contains("x-safety"));
    assert!(!code.contains("identity-neutral codegen hint"));
}

#[test]
fn struct_constructors_are_generated_for_named_objects() {
    let code = generate().code;

    // A params object with required + optional fields: `new` takes only the
    // required field, in emitted field order, and defaults the optional one.
    assert!(
        code.contains("impl ConfigureParams {"),
        "expected an inherent impl for ConfigureParams:\n{code}"
    );
    assert!(
        code.contains("pub fn new(id: String) -> Self {"),
        "constructor should take only the required field:\n{code}"
    );
    assert!(
        code.contains("Self {\n            id,\n            label: None,\n        }"),
        "optional field should default to None:\n{code}"
    );

    // An empty params object still gets a niladic `new()`.
    assert!(code.contains("pub struct ResetParams {}"));
    assert!(
        code.contains("impl ResetParams {")
            && code.contains("pub fn new() -> Self {\n        Self {}\n    }"),
        "empty struct should get a niladic constructor returning `Self {{}}`:\n{code}"
    );

    // An open object (required field + `#[serde(flatten)]` extra map): the
    // extra map is not a constructor parameter and defaults instead.
    assert!(code.contains("impl Bag {"));
    assert!(code.contains("pub fn new(id: String) -> Self {"));
    assert!(
        code.contains("id,\n            extra: Default::default(),"),
        "flattened extra map should default rather than being a parameter:\n{code}"
    );

    // Every generated struct constructor is a distinct inherent impl (no
    // duplicate `impl X {` blocks for the same type).
    let mut impl_headers: Vec<&str> = code
        .lines()
        .filter(|l| l.starts_with("impl ") && l.ends_with(" {"))
        .collect();
    let before = impl_headers.len();
    impl_headers.sort();
    impl_headers.dedup();
    assert_eq!(
        before,
        impl_headers.len(),
        "no two structs should emit the same inherent impl header"
    );
}

#[test]
fn struct_ctor_skipped_on_field_name_collision() {
    // Two distinct wire property names ("fooBar", "foo_bar") sanitize to the
    // same Rust identifier. The struct itself is unrepresentable, so no `new`
    // constructor should be emitted for it (rather than emitting broken code).
    let s = schema(
        r##"{
            "id": "x", "hash": "", "methods": {},
            "components": { "schemas": {
                "Collide": {
                    "type": "object",
                    "properties": {
                        "fooBar": { "type": "string" },
                        "foo_bar": { "type": "string" }
                    },
                    "required": ["fooBar", "foo_bar"],
                    "additionalProperties": false
                }
            }}
        }"##,
    );
    let g = generate_rust_interface(&s, &GenerateRustOptions::default());
    assert!(g.code.contains("pub struct Collide {"));
    assert!(
        !g.code.contains("impl Collide {"),
        "no constructor should be generated when fields collide on the same Rust name:\n{}",
        g.code
    );
}
