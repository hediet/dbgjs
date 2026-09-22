use linkrpc::prelude::compute_interface_hash;
use std::collections::BTreeSet;

#[test]
fn generated_domains_preserve_schema_identity_and_bare_prefixes() {
    let mut interfaces = BTreeSet::new();
    for (events, catalog) in [
        (false, cdp_protocol::command_interfaces()),
        (true, cdp_protocol::event_interfaces()),
    ] {
        let mut prefixes = BTreeSet::new();
        for (prefix, definition) in catalog {
            let schema = definition.to_schema();
            assert!(interfaces.insert(schema.id.clone()));
            assert!(prefixes.insert(prefix.clone()));
            assert!(prefix.ends_with('.'));
            let domain = prefix.trim_end_matches('.');
            assert_eq!(
                schema.id,
                if events {
                    format!("cdp.{domain}.events")
                } else {
                    format!("cdp.{domain}")
                }
            );
            assert_eq!(schema.hash, compute_interface_hash(&schema));
            assert!(!schema.methods.is_empty());
            for (member, method) in &schema.methods {
                assert!(!member.contains('.'));
                assert_eq!(method.result.is_none(), events);
                assert_eq!(
                    method.extension("x-linkrpc-codegen").unwrap()["kind"],
                    if events { "notification" } else { "request" },
                );
            }
        }
        assert!(prefixes.contains("Runtime."));
        assert!(prefixes.contains("Debugger."));
        assert!(prefixes.contains("Target."));
    }
}

#[test]
fn command_and_event_contracts_have_distinct_identity_but_the_same_wire_prefix() {
    let commands = cdp_protocol::runtime::DOMAIN.interface().to_schema();
    let events = cdp_protocol::runtime_events::DOMAIN.interface().to_schema();
    assert_ne!(commands.id, events.id);
    assert_ne!(commands.hash, events.hash);
    assert_eq!(
        cdp_protocol::runtime::DOMAIN.prefix(),
        cdp_protocol::runtime_events::DOMAIN.prefix(),
    );
    assert!(commands.methods.contains_key("evaluate"));
    assert!(!commands.methods.contains_key("consoleAPICalled"));
    assert!(events.methods.contains_key("consoleAPICalled"));
    assert!(!events.methods.contains_key("evaluate"));
    assert!(
        !include_str!("../src/generated/runtime_events.rs").contains("#[server_notification]")
    );
}
