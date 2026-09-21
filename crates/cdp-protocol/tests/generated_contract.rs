use linkrpc::prelude::{LinkRpcInterfaceSchema, compute_interface_hash};

#[test]
fn generated_bindings_preserve_canonical_schema_and_hash() {
    let bundle: serde_json::Value =
        serde_json::from_str(include_str!("../../../schemas/dbgjs.interfaces.json")).unwrap();
    let schema = bundle["interfaceSchemas"]
        .as_array()
        .unwrap()
        .iter()
        .find(|schema| schema["id"] == "cdp.protocol")
        .unwrap();
    let expected: LinkRpcInterfaceSchema = serde_json::from_value(schema.clone()).unwrap();
    let actual = cdp_protocol::interface().to_schema();

    assert_eq!(actual, expected);
    assert_eq!(compute_interface_hash(&actual), expected.hash);
}
