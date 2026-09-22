use linkrpc::prelude::compute_interface_hash;
use std::collections::BTreeSet;

#[test]
fn generated_domains_preserve_schema_identity_and_bare_prefixes() {
    let mut interfaces = BTreeSet::new();
    let mut prefixes = BTreeSet::new();
    for (prefix, definition) in cdp_protocol::interfaces() {
        let schema = definition.to_schema();
        assert!(interfaces.insert(schema.id.clone()));
        assert!(prefixes.insert(prefix.clone()));
        assert!(prefix.ends_with('.'));
        assert_eq!(schema.hash, compute_interface_hash(&schema));
        assert!(schema.methods.keys().all(|member| !member.contains('.')));
    }
    assert!(prefixes.contains("Runtime."));
    assert!(prefixes.contains("Debugger."));
    assert!(prefixes.contains("Target."));
}
