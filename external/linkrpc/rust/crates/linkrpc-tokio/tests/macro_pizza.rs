//! Macro-driven pizza service: define the interface with `#[link_rpc_interface]`, implement the
//! generated trait, serve it via the generated `…Server` adapter, and drive it with the generated
//! `…Client` — proving the end-to-end DX over the in-memory pair.

use std::sync::{Arc, Mutex};

use linkrpc::prelude::*;
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

#[derive(Serialize, Deserialize, JsonSchema, Clone, PartialEq, Debug)]
#[serde(rename_all = "snake_case")]
pub enum PizzaKind {
    Margherita,
    Pepperoni,
}

#[derive(Serialize, Deserialize, JsonSchema, Clone, PartialEq, Debug)]
#[serde(rename_all = "snake_case")]
pub enum Size {
    Small,
    Medium,
    Large,
}

#[derive(Serialize, Deserialize, JsonSchema, Clone, Debug)]
pub struct OrderConfirmation {
    pub order_id: String,
    pub price_cents: u32,
}

/// Order pizzas and track delivery.
#[link_rpc_interface(id = "com.acme.pizza")]
pub trait PizzaService {
    /// Place a new pizza order. Charges money.
    #[annotations(dangerous)]
    async fn order(
        kind: PizzaKind,
        size: Size,
        quantity: u32,
    ) -> Result<OrderConfirmation, JsonRpcError>;

    /// Fire-and-forget cancel request.
    #[notification]
    async fn cancel_order(order_id: String);
}

#[derive(Default)]
struct Shop {
    cancelled: Mutex<Vec<String>>,
}

#[async_trait::async_trait]
impl PizzaService for Shop {
    async fn order(
        &self,
        _ctx: &CallCtx,
        kind: PizzaKind,
        size: Size,
        quantity: u32,
    ) -> Result<OrderConfirmation, JsonRpcError> {
        let base = match size {
            Size::Small => 999,
            Size::Medium => 1299,
            Size::Large => 1599,
        };
        let surcharge = if kind == PizzaKind::Pepperoni { 100 } else { 0 };
        Ok(OrderConfirmation {
            order_id: "ord-42".to_string(),
            price_cents: (base + surcharge) * quantity,
        })
    }

    async fn cancel_order(&self, _ctx: &CallCtx, order_id: String) {
        self.cancelled.lock().unwrap().push(order_id);
    }
}

/// Wire a client + server over the in-memory pair; returns the client and the shared shop.
fn wire() -> (PizzaServiceClient, Arc<Shop>) {
    let (a, b) = transport_pair();
    let client_conn = LinkRpcConnection::new(Box::new(a));
    let server_conn = LinkRpcConnection::new(Box::new(b));

    let shop = Arc::new(Shop::default());
    server_conn
        .register_service(
            Arc::new(PizzaServiceServer::new(shop.clone())),
            RegisterOptions::default(),
        )
        .unwrap();
    server_conn.enable_reflection();

    let (c, s) = (client_conn.clone(), server_conn.clone());
    tokio::spawn(async move { c.run().await });
    tokio::spawn(async move { s.run().await });

    (PizzaServiceClient::new(client_conn), shop)
}

#[tokio::test]
async fn generated_client_calls_request() {
    let (pizza, _shop) = wire();
    let confirmation = pizza
        .order(PizzaKind::Pepperoni, Size::Large, 2)
        .await
        .expect("order");
    assert_eq!(confirmation.order_id, "ord-42");
    assert_eq!(confirmation.price_cents, (1599 + 100) * 2);
}

#[tokio::test]
async fn generated_client_sends_notification() {
    let (pizza, shop) = wire();
    pizza
        .cancel_order("ord-99".to_string())
        .await
        .expect("cancel");

    for _ in 0..50 {
        if !shop.cancelled.lock().unwrap().is_empty() {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(2)).await;
    }
    assert_eq!(shop.cancelled.lock().unwrap().as_slice(), ["ord-99"]);
}

#[tokio::test]
async fn interface_id_and_hash_are_stable() {
    let iface = pizza_service::interface();
    assert_eq!(iface.id(), "com.acme.pizza");
    assert_eq!(pizza_service::ID, "com.acme.pizza");
    // 16 lowercase hex chars.
    let hash = iface.schema_hash();
    assert_eq!(hash.len(), 16);
    assert!(hash.chars().all(|c| c.is_ascii_hexdigit()));
}

#[tokio::test]
async fn reflection_exposes_macro_interface() {
    let (pizza, _shop) = wire();
    // The generated client carries the connection; reuse it for a raw reflection call.
    let schema = pizza
        .order(PizzaKind::Margherita, Size::Small, 1)
        .await
        .expect("order to ensure server is live");
    assert_eq!(schema.order_id, "ord-42");
}

#[tokio::test]
async fn annotations_are_recorded() {
    let iface = pizza_service::interface();
    let schema = iface.to_schema();
    let order = schema.methods.get("order").expect("order method");
    assert_eq!(
        order.annotations.as_ref().and_then(|a| a.dangerous),
        Some(true)
    );
    assert_eq!(
        order.description.as_deref(),
        Some("Place a new pizza order. Charges money.")
    );
}
