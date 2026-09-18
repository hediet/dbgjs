//! End-to-end `LinkRpcConnection` round-trip over the in-memory transport pair.
//!
//! Hand-builds a (subset of the) pizza interface, registers a handler on one connection, and
//! drives requests / a notification / reflection from the peer connection — exercising routing,
//! the registry, and the three reflection interfaces without the proc-macro or sockets.

use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use linkrpc::prelude::*;
use schemars::{schema_for, JsonSchema};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};

const PIZZA_ID: &str = "com.acme.pizza";

#[derive(Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
enum PizzaKind {
    Margherita,
    Pepperoni,
}

#[derive(Serialize, Deserialize, JsonSchema)]
struct OrderArgs {
    kind: PizzaKind,
    quantity: u32,
}

#[derive(Serialize, Deserialize, JsonSchema)]
struct OrderConfirmation {
    order_id: String,
    price_cents: u32,
}

#[derive(Serialize, Deserialize, JsonSchema)]
struct OrderIdArgs {
    order_id: String,
}

fn subset_of<T: JsonSchema>() -> Value {
    linkrpc::schema::schemars_to_subset(&serde_json::to_value(schema_for!(T)).unwrap())
        .expect("schemars→subset succeeds")
}

fn pizza_interface() -> InterfaceDefinition {
    let order = Member::Request(Box::new(RequestMember {
        params_schema: subset_of::<OrderArgs>(),
        result_schema: subset_of::<OrderConfirmation>(),
        client_stream_schema: None,
        server_stream_schema: None,
        docs: MemberDocs {
            description: Some("Place a new pizza order. Charges money.".to_string()),
            ..Default::default()
        },
    }));
    let cancel = Member::Notification(NotificationMember {
        params_schema: subset_of::<OrderIdArgs>(),
        docs: MemberDocs::default(),
    });
    InterfaceDefinition::new(
        InterfaceInfo::new(PIZZA_ID).with_description("Order pizzas and track delivery."),
        vec![
            ("order".to_string(), order),
            ("cancel_order".to_string(), cancel),
        ],
    )
}

struct PizzaShop {
    cancelled: Arc<Mutex<Vec<String>>>,
}

#[async_trait]
impl InterfaceHandler for PizzaShop {
    async fn handle_request(
        &self,
        member: &str,
        params: JsonValue,
        _ctx: CallCtx,
    ) -> Result<JsonValue, JsonRpcError> {
        match member {
            "order" => {
                let args: OrderArgs = serde_json::from_value(params)
                    .map_err(|e| JsonRpcError::new(error_codes::INVALID_PARAMS, e.to_string()))?;
                let confirmation = OrderConfirmation {
                    order_id: "ord-1".to_string(),
                    price_cents: 1299 * args.quantity,
                };
                Ok(serde_json::to_value(confirmation).unwrap())
            }
            _ => Err(JsonRpcError::new(error_codes::METHOD_NOT_FOUND, member)),
        }
    }

    async fn handle_notification(&self, member: &str, params: JsonValue, _ctx: CallCtx) {
        if member == "cancel_order" {
            if let Ok(args) = serde_json::from_value::<OrderIdArgs>(params) {
                self.cancelled.lock().unwrap().push(args.order_id);
            }
        }
    }
}

/// Wire two connections over the in-memory pair; the server registers pizza + reflection.
fn make_pair(cancelled: Arc<Mutex<Vec<String>>>) -> (LinkRpcConnection, LinkRpcConnection) {
    let (a, b) = transport_pair();
    let client = LinkRpcConnection::new(Box::new(a));
    let server = LinkRpcConnection::new(Box::new(b));

    let iface = Arc::new(pizza_interface());
    let shop: Arc<dyn InterfaceHandler> = Arc::new(PizzaShop { cancelled });
    server
        .register(iface.clone(), shop, RegisterOptions::default())
        .expect("register pizza");
    server.set_preset(PIZZA_ID).expect("set preset");
    server.enable_reflection();

    let (c, s) = (client.clone(), server.clone());
    tokio::spawn(async move { c.run().await });
    tokio::spawn(async move { s.run().await });
    (client, server)
}

#[tokio::test]
async fn request_round_trips() {
    let (client, _server) = make_pair(Arc::new(Mutex::new(Vec::new())));
    let result = client
        .call_member(
            None,
            PIZZA_ID,
            "order",
            json!({ "kind": "pepperoni", "quantity": 2 }),
        )
        .await
        .expect("order succeeds");
    assert_eq!(result["order_id"], "ord-1");
    assert_eq!(result["price_cents"], 2598);
}

#[tokio::test]
async fn bare_preset_dispatch_works() {
    let (client, _server) = make_pair(Arc::new(Mutex::new(Vec::new())));
    // Form-1 (bare member) routes to the preset interface.
    let result = client
        .call("order", json!({ "kind": "margherita", "quantity": 1 }))
        .await
        .expect("preset order succeeds");
    assert_eq!(result["price_cents"], 1299);
}

#[tokio::test]
async fn notification_round_trips() {
    let cancelled = Arc::new(Mutex::new(Vec::new()));
    let (client, _server) = make_pair(cancelled.clone());
    client
        .notify_member(
            None,
            PIZZA_ID,
            "cancel_order",
            json!({ "order_id": "ord-9" }),
        )
        .await
        .expect("notify");

    // Give the server loop a moment to process the notification.
    for _ in 0..50 {
        if !cancelled.lock().unwrap().is_empty() {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(2)).await;
    }
    assert_eq!(cancelled.lock().unwrap().as_slice(), ["ord-9".to_string()]);
}

#[tokio::test]
async fn unknown_interface_is_method_not_found() {
    let (client, _server) = make_pair(Arc::new(Mutex::new(Vec::new())));
    let err = client
        .call_member(None, "com.acme.unknown", "order", json!({}))
        .await
        .expect_err("unknown interface should fail");
    assert_eq!(err.code, error_codes::METHOD_NOT_FOUND);
}

#[tokio::test]
async fn reflection_directory_lists_pizza() {
    let (client, _server) = make_pair(Arc::new(Mutex::new(Vec::new())));
    let listing = client
        .call_member(None, "hubrpc.directory", "list", json!({}))
        .await
        .expect("directory list");
    let items = listing["items"].as_array().expect("items array");
    let found = items
        .iter()
        .any(|i| i["interfaceId"] == PIZZA_ID && i["interfaceHash"].is_string());
    assert!(
        found,
        "pizza interface should appear in the directory: {items:?}"
    );
}

#[tokio::test]
async fn reflection_directory_filters_by_interface() {
    let (client, _server) = make_pair(Arc::new(Mutex::new(Vec::new())));
    let listing = client
        .call_member(
            None,
            "hubrpc.directory",
            "list",
            json!({ "interfaceId": PIZZA_ID }),
        )
        .await
        .expect("directory list");
    let items = listing["items"].as_array().expect("items array");
    assert_eq!(items.len(), 1);
    assert_eq!(items[0]["interfaceId"], PIZZA_ID);
}

#[tokio::test]
async fn reflection_defaults_reports_preset() {
    let (client, _server) = make_pair(Arc::new(Mutex::new(Vec::new())));
    let defaults = client
        .call_member(None, "hubrpc.defaults", "get", json!({}))
        .await
        .expect("defaults get");
    assert_eq!(defaults["interfaceId"], PIZZA_ID);
    assert!(defaults["interfaceHash"].is_string());
}

#[tokio::test]
async fn reflection_schemas_returns_interface() {
    let (client, _server) = make_pair(Arc::new(Mutex::new(Vec::new())));
    let schema = client
        .call_member(
            None,
            "hubrpc.schemas",
            "get",
            json!({ "interfaceId": PIZZA_ID }),
        )
        .await
        .expect("schemas get");
    assert_eq!(schema["schema"]["id"], PIZZA_ID);
    let methods = schema["schema"]["methods"].as_object().expect("methods");
    assert!(methods.contains_key("order"));
}

#[tokio::test]
async fn reflection_schemas_unknown_is_method_not_found() {
    let (client, _server) = make_pair(Arc::new(Mutex::new(Vec::new())));
    let err = client
        .call_member(
            None,
            "hubrpc.schemas",
            "get",
            json!({ "interfaceId": "com.acme.nope" }),
        )
        .await
        .expect_err("unknown schema should fail");
    assert_eq!(err.code, error_codes::METHOD_NOT_FOUND);
}

#[tokio::test]
async fn duplicate_registration_is_rejected() {
    let (_a, b) = transport_pair();
    let conn = LinkRpcConnection::new(Box::new(b));
    let iface = Arc::new(pizza_interface());
    let shop: Arc<dyn InterfaceHandler> = Arc::new(PizzaShop {
        cancelled: Arc::new(Mutex::new(Vec::new())),
    });
    conn.register(iface.clone(), shop.clone(), RegisterOptions::default())
        .expect("first registration");
    let err = conn
        .register(iface, shop, RegisterOptions::default())
        .expect_err("duplicate should be rejected");
    assert!(matches!(err, ConnError::AlreadyRegistered { .. }));
}
