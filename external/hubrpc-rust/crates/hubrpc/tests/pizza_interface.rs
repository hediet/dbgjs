//! The cross-language hash-parity canary.
//!
//! Builds the pizza interface from Rust types via `schemars` + the schemars→subset bridge and
//! asserts byte-identical schema + interface hash against `pizza_interface.json`, which is
//! generated from the **real TypeScript** hubrpc impl (zod → `z.toJSONSchema` → normalize →
//! `computeInterfaceHash`). Red here means a Rust-derived contract would not be recognized by a
//! TS peer (and vice-versa).

use std::path::PathBuf;

use hubrpc::schema::interface_schema::{
    HubRpcInterfaceSchema, MemberAnnotations, MethodMap, MethodSchema,
};
use hubrpc::schema::{compute_interface_hash, schemars_to_subset};
use schemars::{schema_for, JsonSchema};
use serde::{Deserialize, Serialize};
use serde_json::Value;

// ── shared value types (mirror docs/examples.md + gen-pizza-ref.mjs) ──────────
#[derive(Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
enum PizzaKind {
    Margherita,
    Pepperoni,
    Hawaiian,
    Veggie,
}

#[derive(Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
enum Size {
    Small,
    Medium,
    Large,
}

#[derive(Serialize, Deserialize, JsonSchema)]
struct OrderConfirmation {
    order_id: String,
    eta_minutes: u32,
    price_cents: u32,
}

#[derive(Serialize, Deserialize, JsonSchema)]
#[serde(tag = "state", rename_all = "snake_case")]
enum OrderStatus {
    Queued,
    Baking { progress: f32 },
    OutForDelivery { driver: String },
    Delivered,
    Cancelled { reason: String },
}

#[derive(Serialize, Deserialize, JsonSchema)]
struct OrderSummary {
    total_cents: u32,
}

#[derive(Serialize, Deserialize, JsonSchema)]
struct Topping {
    name: String,
    cents: u32,
}

#[derive(Serialize, Deserialize, JsonSchema)]
struct SessionReport {
    plated: u32,
    burned: u32,
}

#[derive(Serialize, Deserialize, JsonSchema)]
#[serde(tag = "type", rename_all = "snake_case")]
enum KitchenCmd {
    Fire { ticket: String },
    Cancel { ticket: String },
}

#[derive(Serialize, Deserialize, JsonSchema)]
#[serde(tag = "type", rename_all = "snake_case")]
enum KitchenEvent {
    Plated { dish: String },
    Burned { dish: String },
}

// ── per-method param objects (what the macro will synthesize) ─────────────────
#[derive(Serialize, Deserialize, JsonSchema)]
struct OrderArgs {
    kind: PizzaKind,
    size: Size,
    quantity: u32,
}

#[derive(Serialize, Deserialize, JsonSchema)]
struct OrderIdArgs {
    order_id: String,
}

#[derive(Serialize, Deserialize, JsonSchema)]
struct BuildOrderArgs {
    base: PizzaKind,
}

#[derive(Serialize, Deserialize, JsonSchema)]
struct KitchenSessionArgs {
    station: String,
}

fn subset_of<T: JsonSchema>() -> Value {
    schemars_to_subset(&serde_json::to_value(schema_for!(T)).unwrap())
        .expect("schemars→subset succeeds")
}

fn method<P: JsonSchema>() -> MethodSchema {
    MethodSchema::notification(subset_of::<P>())
}

fn build_pizza_interface() -> HubRpcInterfaceSchema {
    let mut order = method::<OrderArgs>();
    order.result = Some(subset_of::<OrderConfirmation>());
    order.description = Some("Place a new pizza order. Charges money.".to_string());
    order.annotations = Some(MemberAnnotations {
        dangerous: Some(true),
        ..Default::default()
    });

    let mut order_status = method::<OrderIdArgs>();
    order_status.result = Some(subset_of::<OrderStatus>());
    order_status.description = Some("Current status of an order. Pure query.".to_string());
    order_status.annotations = Some(MemberAnnotations {
        read_only: Some(true),
        ..Default::default()
    });

    let mut cancel_order = method::<OrderIdArgs>();
    cancel_order.description = Some("Fire-and-forget cancel request.".to_string());

    let mut watch_order = method::<OrderIdArgs>();
    watch_order.result = Some(subset_of::<OrderSummary>());
    watch_order.server_stream = Some(subset_of::<OrderStatus>());
    watch_order.description = Some("Live status updates until delivered/cancelled.".to_string());

    let mut build_order = method::<BuildOrderArgs>();
    build_order.result = Some(subset_of::<OrderConfirmation>());
    build_order.client_stream = Some(subset_of::<Topping>());

    let mut kitchen_session = method::<KitchenSessionArgs>();
    kitchen_session.result = Some(subset_of::<SessionReport>());
    kitchen_session.client_stream = Some(subset_of::<KitchenCmd>());
    kitchen_session.server_stream = Some(subset_of::<KitchenEvent>());

    let mut methods: MethodMap = MethodMap::new();
    methods.insert("order".to_string(), order);
    methods.insert("order_status".to_string(), order_status);
    methods.insert("cancel_order".to_string(), cancel_order);
    methods.insert("watch_order".to_string(), watch_order);
    methods.insert("build_order".to_string(), build_order);
    methods.insert("kitchen_session".to_string(), kitchen_session);

    HubRpcInterfaceSchema {
        id: "com.acme.pizza".to_string(),
        hash: String::new(),
        description: Some("Order pizzas and track delivery.".to_string()),
        comment: None,
        methods,
        components: None,
        extensions: Default::default(),
    }
}

fn reference() -> Value {
    let path = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("..")
        .join("..")
        .join("conformance")
        .join("vectors")
        .join("pizza_interface.json");
    let text = std::fs::read_to_string(&path).expect("read pizza_interface.json");
    serde_json::from_str(&text).expect("parse pizza_interface.json")
}

#[test]
fn pizza_interface_hash_matches_typescript() {
    let iface = build_pizza_interface();
    let hash = compute_interface_hash(&iface);
    let reference = reference();
    assert_eq!(
        hash,
        reference["hash"].as_str().unwrap(),
        "Rust-derived pizza interface hash diverged from the TypeScript reference"
    );
}

#[test]
fn pizza_interface_schema_matches_typescript() {
    let mut iface = build_pizza_interface();
    iface.hash = compute_interface_hash(&iface);
    let got = serde_json::to_value(&iface).unwrap();
    let reference = reference();
    assert_eq!(
        got, reference["schema"],
        "Rust-derived pizza interface schema diverged from the TypeScript reference"
    );
}
