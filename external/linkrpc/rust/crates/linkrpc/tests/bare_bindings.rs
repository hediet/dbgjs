use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use linkrpc::prelude::*;
use serde_json::{json, Value};

fn interface(id: &str, requests: &[&str], notifications: &[&str]) -> Arc<InterfaceDefinition> {
    let mut members = Vec::new();
    for name in requests {
        members.push((
            (*name).to_string(),
            Member::Request(Box::new(RequestMember {
                params_schema: json!({ "type": "object" }),
                result_schema: json!({}),
                client_stream_schema: None,
                server_stream_schema: None,
                docs: MemberDocs::default(),
            })),
        ));
    }
    for name in notifications {
        members.push((
            (*name).to_string(),
            Member::Notification(NotificationMember {
                params_schema: json!({ "type": "object" }),
                docs: MemberDocs::default(),
            }),
        ));
    }
    Arc::new(InterfaceDefinition::new(InterfaceInfo::new(id), members))
}

struct RecordingHandler {
    interface_id: String,
    service_id: Option<String>,
    notifications: Arc<Mutex<Vec<(String, JsonValue)>>>,
}

#[async_trait]
impl InterfaceHandler for RecordingHandler {
    async fn handle_request(
        &self,
        member: &str,
        params: JsonValue,
        _ctx: CallCtx,
    ) -> Result<JsonValue, JsonRpcError> {
        let mut result = json!({
            "interfaceId": self.interface_id,
            "member": member,
            "params": params,
        });
        if let Some(service_id) = &self.service_id {
            result["serviceId"] = json!(service_id);
        }
        Ok(result)
    }

    async fn handle_notification(&self, member: &str, params: JsonValue, _ctx: CallCtx) {
        self.notifications
            .lock()
            .unwrap()
            .push((member.to_string(), params));
    }
}

fn register(
    connection: &LinkRpcConnection,
    iface: Arc<InterfaceDefinition>,
    service_id: Option<&str>,
    notifications: Arc<Mutex<Vec<(String, JsonValue)>>>,
) {
    let interface_id = iface.id().to_string();
    let service_id = service_id.map(str::to_string);
    connection
        .register(
            iface,
            Arc::new(RecordingHandler {
                interface_id,
                service_id: service_id.clone(),
                notifications,
            }),
            RegisterOptions {
                service_id,
                ..Default::default()
            },
        )
        .unwrap();
}

fn connected_pair() -> (LinkRpcConnection, LinkRpcConnection) {
    let (a, b) = transport_pair();
    let caller = LinkRpcConnection::new(Box::new(a));
    let provider = LinkRpcConnection::new(Box::new(b));
    let caller_task = caller.clone();
    let provider_task = provider.clone();
    tokio::spawn(async move { caller_task.run().await });
    tokio::spawn(async move { provider_task.run().await });
    (caller, provider)
}

fn bare_binding_vectors() -> Value {
    let path = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("..")
        .join("..")
        .join("..")
        .join("conformance")
        .join("vectors")
        .join("bare_bindings.json");
    let text =
        std::fs::read_to_string(&path).unwrap_or_else(|e| panic!("read {}: {e}", path.display()));
    serde_json::from_str(&text).expect("bare binding vectors parse")
}

fn reflection_defaults_vector() -> Value {
    let path = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("..")
        .join("..")
        .join("..")
        .join("conformance")
        .join("vectors")
        .join("reflection_defaults.json");
    let text =
        std::fs::read_to_string(&path).unwrap_or_else(|e| panic!("read {}: {e}", path.display()));
    serde_json::from_str(&text).expect("reflection defaults vector parses")
}

#[test]
fn reflection_defaults_schema_and_hash_match_shared_fixture() {
    let expected = reflection_defaults_vector();
    let iface = linkrpc::connection::reflection::defaults_interface();
    let mut actual = serde_json::to_value(iface.to_schema()).unwrap();
    actual.as_object_mut().unwrap().remove("hash");
    assert_eq!(actual, expected["schema"]);
    assert_eq!(iface.schema_hash(), expected["hash"].as_str().unwrap());
}

#[tokio::test]
async fn shared_conformance_vectors_match_runtime_routing() {
    let vectors = bare_binding_vectors();
    for case in vectors["cases"].as_array().unwrap() {
        let (caller, provider) = connected_pair();
        let notifications = Arc::new(Mutex::new(Vec::new()));
        for binding in case["bindings"].as_array().unwrap() {
            let prefix = binding["prefix"].as_str().unwrap();
            let interface_id = binding["interfaceId"].as_str().unwrap();
            let service_id = binding["serviceId"].as_str();
            let members: Vec<&str> = binding["members"]
                .as_array()
                .unwrap()
                .iter()
                .map(|member| member.as_str().unwrap())
                .collect();
            register(
                &provider,
                interface(interface_id, &members, &[]),
                service_id,
                notifications.clone(),
            );
            provider
                .bind_bare(prefix, service_id, interface_id)
                .unwrap();
        }

        let result = caller
            .call(case["method"].as_str().unwrap(), json!({}))
            .await;
        let expected = &case["expected"];
        if expected["error"] == "methodNotFound" {
            let err = result.unwrap_err();
            assert_eq!(
                err.code,
                error_codes::METHOD_NOT_FOUND,
                "case {:?}",
                case["name"]
            );
        } else {
            let result = result.unwrap_or_else(|e| panic!("case {:?}: {e:?}", case["name"]));
            assert_eq!(result["interfaceId"], expected["interfaceId"]);
            assert_eq!(result["member"], expected["member"]);
            assert_eq!(result.get("serviceId"), expected.get("serviceId"));
        }
    }
}

#[tokio::test]
async fn cdp_and_lsp_prefixes_route_requests_and_notifications() {
    let (caller, provider) = connected_pair();
    let notifications = Arc::new(Mutex::new(Vec::new()));
    register(
        &provider,
        interface("cdp.DOM", &["getDocument"], &[]),
        None,
        notifications.clone(),
    );
    register(
        &provider,
        interface("cdp.Runtime", &["evaluate"], &[]),
        None,
        notifications.clone(),
    );
    register(
        &provider,
        interface("lsp.lifecycle", &["initialize"], &[]),
        None,
        notifications.clone(),
    );
    register(
        &provider,
        interface("lsp.textDocument", &["hover"], &[]),
        None,
        notifications.clone(),
    );
    register(
        &provider,
        interface("lsp.protocol", &[], &["cancelRequest"]),
        None,
        notifications.clone(),
    );

    provider.bind_bare("DOM.", None, "cdp.DOM").unwrap();
    provider.bind_bare("Runtime.", None, "cdp.Runtime").unwrap();
    provider.bind_bare("", None, "lsp.lifecycle").unwrap();
    provider
        .bind_bare("textDocument/", None, "lsp.textDocument")
        .unwrap();
    provider.bind_bare("$/", None, "lsp.protocol").unwrap();

    let runtime = caller
        .call("Runtime.evaluate", json!({ "x": 1 }))
        .await
        .unwrap();
    assert_eq!(runtime["interfaceId"], "cdp.Runtime");
    assert_eq!(runtime["member"], "evaluate");
    let hover = caller.call("textDocument/hover", json!({})).await.unwrap();
    assert_eq!(hover["interfaceId"], "lsp.textDocument");
    let initialize = caller.call("initialize", json!({})).await.unwrap();
    assert_eq!(initialize["interfaceId"], "lsp.lifecycle");

    caller
        .notify("$/cancelRequest", json!({ "id": 7 }))
        .await
        .unwrap();
    for _ in 0..50 {
        if !notifications.lock().unwrap().is_empty() {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(2)).await;
    }
    assert_eq!(notifications.lock().unwrap()[0].0, "cancelRequest");
}

#[tokio::test]
async fn longest_prefix_wins_without_member_fallback_and_addressed_calls_are_unchanged() {
    let (caller, provider) = connected_pair();
    let notifications = Arc::new(Mutex::new(Vec::new()));
    register(
        &provider,
        interface("test.short", &["bc"], &[]),
        None,
        notifications.clone(),
    );
    register(
        &provider,
        interface("test.long", &["other"], &[]),
        Some("tools"),
        notifications,
    );
    provider.bind_bare("a", None, "test.short").unwrap();
    provider
        .bind_bare("ab", Some("tools"), "test.long")
        .unwrap();

    let err = caller.call("abc", json!({})).await.unwrap_err();
    assert_eq!(err.code, error_codes::METHOD_NOT_FOUND);
    let err = caller.call("ab", json!({})).await.unwrap_err();
    assert_eq!(err.code, error_codes::METHOD_NOT_FOUND);
    let err = caller.call("unbound", json!({})).await.unwrap_err();
    assert_eq!(err.code, error_codes::METHOD_NOT_FOUND);

    let short = caller
        .call("test.short::bc", json!({}))
        .await
        .expect("interface-addressed call");
    assert_eq!(short["interfaceId"], "test.short");
    let named = caller
        .call("tools::test.long::other", json!({}))
        .await
        .expect("service-addressed call");
    assert_eq!(named["interfaceId"], "test.long");
}

#[test]
fn binding_validation_duplicates_and_unbind_lifecycle() {
    let (_a, b) = transport_pair();
    let provider = LinkRpcConnection::new(Box::new(b));
    let notifications = Arc::new(Mutex::new(Vec::new()));

    assert!(matches!(
        provider.bind_bare("DOM.", None, "cdp.DOM"),
        Err(ConnError::BareTargetNotRegistered { .. })
    ));
    register(
        &provider,
        interface("cdp.DOM", &["getDocument"], &[]),
        None,
        notifications,
    );
    let vectors = bare_binding_vectors();
    for prefix in vectors["invalidPrefixes"].as_array().unwrap() {
        let prefix = prefix.as_str().unwrap();
        assert!(
            matches!(
                provider.bind_bare(prefix, None, "cdp.DOM"),
                Err(ConnError::InvalidBarePrefix)
            ),
            "{prefix:?}"
        );
    }
    provider.bind_bare("DOM.", None, "cdp.DOM").unwrap();
    assert!(matches!(
        provider.bind_bare("DOM.", None, "cdp.DOM"),
        Err(ConnError::BarePrefixAlreadyBound(prefix)) if prefix == "DOM."
    ));
    assert!(provider.unbind_bare("DOM."));
    assert!(!provider.unbind_bare("DOM."));
    provider.bind_bare("DOM.", None, "cdp.DOM").unwrap();
}

#[tokio::test]
async fn legacy_preset_replaces_empty_binding_and_reflection_lists_sorted_bindings() {
    let (caller, provider) = connected_pair();
    let notifications = Arc::new(Mutex::new(Vec::new()));
    let old = interface("test.old", &["run"], &[]);
    let old_hash = old.schema_hash().to_string();
    register(&provider, old, None, notifications.clone());
    let lifecycle = interface("lsp.lifecycle", &["run"], &[]);
    let lifecycle_hash = lifecycle.schema_hash().to_string();
    register(&provider, lifecycle, None, notifications.clone());
    let runtime = interface("cdp.Runtime", &["evaluate"], &[]);
    let runtime_hash = runtime.schema_hash().to_string();
    register(&provider, runtime, Some("browser"), notifications.clone());

    provider.bind_bare("", None, "test.old").unwrap();
    provider
        .bind_bare("Runtime.", Some("browser"), "cdp.Runtime")
        .unwrap();
    assert!(matches!(
        provider.bind_bare("", None, "lsp.lifecycle"),
        Err(ConnError::BarePrefixAlreadyBound(prefix)) if prefix.is_empty()
    ));
    provider.set_preset("lsp.lifecycle").unwrap();
    provider.enable_reflection();

    let routed = caller.call("run", json!({})).await.unwrap();
    assert_eq!(routed["interfaceId"], "lsp.lifecycle");

    let defaults = caller
        .call("hubrpc.defaults::get", json!({}))
        .await
        .unwrap();
    assert_eq!(defaults["interfaceId"], "lsp.lifecycle");
    assert_eq!(defaults["interfaceHash"], lifecycle_hash);
    assert_ne!(defaults["interfaceHash"], old_hash);

    let listed = caller
        .call("hubrpc.defaults::listBindings", json!({}))
        .await
        .unwrap();
    assert_eq!(
        listed["bindings"],
        json!([
            {
                "prefix": "",
                "interfaceId": "lsp.lifecycle",
                "interfaceHash": lifecycle_hash,
            },
            {
                "prefix": "Runtime.",
                "serviceId": "browser",
                "interfaceId": "cdp.Runtime",
                "interfaceHash": runtime_hash,
            }
        ])
    );
}

#[tokio::test]
async fn defaults_get_only_reports_the_empty_prefix_binding() {
    let (caller, provider) = connected_pair();
    let notifications = Arc::new(Mutex::new(Vec::new()));
    let runtime = interface("cdp.Runtime", &["evaluate"], &[]);
    let runtime_hash = runtime.schema_hash().to_string();
    register(&provider, runtime, Some("browser"), notifications.clone());
    provider.enable_reflection();

    let defaults = caller
        .call("hubrpc.defaults::get", json!({}))
        .await
        .unwrap();
    assert_eq!(defaults, json!({}));

    provider
        .bind_bare("Runtime.", Some("browser"), "cdp.Runtime")
        .unwrap();
    let defaults = caller
        .call("hubrpc.defaults::get", json!({}))
        .await
        .unwrap();
    assert_eq!(defaults, json!({}));

    provider
        .bind_bare("", Some("browser"), "cdp.Runtime")
        .unwrap();
    let defaults = caller
        .call("hubrpc.defaults::get", json!({}))
        .await
        .unwrap();
    assert_eq!(
        defaults,
        json!({
            "serviceId": "browser",
            "interfaceId": "cdp.Runtime",
            "interfaceHash": runtime_hash,
        })
    );
}
