# hubrpc examples — Pizza order service

> **Status: design sketch.** This document shows the *target* developer experience for the
> Rust `hubrpc` port using API that does **not exist yet**. It is the north star the
> implementation aims for, not working code. See `plan.md` for the build phases.
>
> **Implemented subset (today).** The `#[hub_rpc_interface]` macro already works for the
> non-streaming core. The shipped shape differs from the tarpc sketch below in a few ways:
> the generated trait methods take **`&self`** and a **`&CallCtx`** first arg (not `self`
> by value + tarpc `Context`); the interface builder is **`pizza_service::interface()`** (a
> module named after the trait, plus `pizza_service::ID`); the server adapter is
> **`PizzaServiceServer::new(Arc<impl>)`** (an `InterfaceHandler`); and the client is
> **`PizzaServiceClient::new(conn)`** with plain `async fn` methods returning
> `Result<T, JsonRpcError>`. Streaming attributes (`#[incoming_stream]` / `#[outgoing_stream]`)
> are rejected for now. The runnable reference is
> `crates/hubrpc-tokio/tests/macro_pizza.rs`.

The model is **tarpc-based**: you write a clean, bare trait spec (no `self`, no `ctx`, no
stream args). The `#[hub_rpc_interface]` macro **rewrites** it into the real service trait
(injecting `self` + a `Context` first arg, plus stream handles) and additionally emits:

- the **rewritten `PizzaService` trait** the provider implements,
- a **`PizzaServiceClient`** struct (the caller proxy; `ctx`-first methods),
- a **`serve()`** adapter (turns an impl into a request handler),
- internal **`PizzaServiceRequest` / `PizzaServiceResponse`** wire enums,
- hubrpc bits: **`PizzaService::interface()`** with its `id@hash`, `::` method-name routing,
  and reflection registration.

Stream directions are named **from the caller's perspective**:

| attribute | direction | provider sees | caller does |
|---|---|---|---|
| `#[incoming_stream(T)]` | server → client | `IncomingStream<T>` (source: `.send`) | `.next().await` |
| `#[outgoing_stream(T)]` | client → server | `OutgoingStream<T>` (sink: `.recv`) | `.send().await` |

---

## 1. Shared crate: types + interface trait

This crate is shared by both the provider and the consumer — it is the single source of
truth for the contract (and its `id@hash`).

```rust
use hubrpc::prelude::*;

#[derive(Serialize, Deserialize, JsonSchema, Clone)]
#[serde(rename_all = "snake_case")]
pub enum PizzaKind { Margherita, Pepperoni, Hawaiian, Veggie }

#[derive(Serialize, Deserialize, JsonSchema, Clone)]
#[serde(rename_all = "snake_case")]
pub enum Size { Small, Medium, Large }

#[derive(Serialize, Deserialize, JsonSchema, Clone)]
pub struct OrderConfirmation {
    pub order_id: String,
    pub eta_minutes: u32,
    pub price_cents: u32,
}

#[derive(Serialize, Deserialize, JsonSchema, Clone, Debug)]
#[serde(tag = "state", rename_all = "snake_case")]
pub enum OrderStatus {
    Queued,
    Baking { progress: f32 },
    OutForDelivery { driver: String },
    Delivered,
    Cancelled { reason: String },
}

#[derive(Serialize, Deserialize, JsonSchema, Clone)]
pub struct OrderSummary { pub total_cents: u32 }

#[derive(Serialize, Deserialize, JsonSchema, Clone)]
pub struct Topping { pub name: String, pub cents: u32 }

#[derive(Serialize, Deserialize, JsonSchema, Clone)]
pub enum KitchenCmd { Fire(String), Cancel(String) }

#[derive(Serialize, Deserialize, JsonSchema, Clone)]
pub enum KitchenEvent { Plated(String), Burned(String) }

#[derive(Serialize, Deserialize, JsonSchema, Clone)]
pub struct SessionReport { pub plated: u32, pub burned: u32 }

// Per-method error types (each carries its own structured error on the wire).
#[derive(Serialize, Deserialize, JsonSchema, Clone, Debug, thiserror::Error)]
pub enum WatchError {
    #[error("unknown order")] UnknownOrder,
}

#[derive(Serialize, Deserialize, JsonSchema, Clone, Debug, thiserror::Error)]
pub enum BuildError {
    #[error("out of stock")] OutOfStock,
}

#[derive(Serialize, Deserialize, JsonSchema, Clone, Debug, thiserror::Error)]
pub enum KitchenError {
    #[error("station busy")] StationBusy,
}

/// Normative interface description — part of the interface hash.
///
/// NOTE: this is a **bare tarpc-style spec**. No `self`, no `ctx`, and no stream
/// arguments appear here; the macro injects them into the generated trait + client.
/// Doc comments, `#[annotations(...)]`, and the `id` are normative (hashed).
#[hub_rpc_interface(id = "com.acme.pizza")]
pub trait PizzaService {
    /// Place a new pizza order. Charges money.
    #[annotations(dangerous)]
    async fn order(kind: PizzaKind, size: Size, quantity: u32)
        -> Result<OrderConfirmation, RpcError>;

    /// Current status of an order. Pure query.
    #[annotations(read_only)]
    async fn order_status(order_id: String) -> Result<OrderStatus, RpcError>;

    /// Fire-and-forget cancel request (JSON-RPC notification — no result).
    #[notification]
    async fn cancel_order(order_id: String);

    /// Live status updates until delivered/cancelled.            (server → client)
    #[incoming_stream(OrderStatus)]
    async fn watch_order(order_id: String) -> Result<OrderSummary, WatchError>;

    /// Caller streams toppings; confirmation on stream close.    (client → server)
    #[outgoing_stream(Topping)]
    async fn build_order(base: PizzaKind) -> Result<OrderConfirmation, BuildError>;

    /// Live kitchen session.                                     (client → server + server → client)
    #[outgoing_stream(KitchenCmd)]
    #[incoming_stream(KitchenEvent)]
    async fn kitchen_session(station: String) -> Result<SessionReport, KitchenError>;
}
```

---

## 2. Provider — implements the *rewritten* trait

The provider implements the trait the macro generated. Every method gains `self` and a
`Context` first arg; stream methods additionally gain their `IncomingStream<T>` /
`OutgoingStream<T>` handle(s).

```rust
use hubrpc::prelude::*;
use std::sync::Arc;

#[derive(Clone)]
struct PizzaShop { /* oven, db, … */ }

impl PizzaService for PizzaShop {
    async fn order(self, _ctx: Context, kind: PizzaKind, size: Size, quantity: u32)
        -> Result<OrderConfirmation, RpcError>
    {
        let order_id = self.create(kind, size, quantity).await;
        Ok(OrderConfirmation { order_id, eta_minutes: 30, price_cents: 1299 * quantity })
    }

    async fn order_status(self, _ctx: Context, order_id: String)
        -> Result<OrderStatus, RpcError>
    {
        self.lookup(&order_id).ok_or_else(|| RpcError::not_found("unknown order"))
    }

    async fn cancel_order(self, ctx: Context, order_id: String) {
        tracing::info!(req = %ctx.request_id(), %order_id, "cancel");
        self.mark_cancelled(&order_id).await;
    }

    // incoming = server → client: `out` is a source the provider pushes into.
    async fn watch_order(self, ctx: Context, order_id: String, out: IncomingStream<OrderStatus>)
        -> Result<OrderSummary, WatchError>
    {
        let mut o = self.lookup(&order_id).ok_or(WatchError::UnknownOrder)?;
        while !o.done() {
            ctx.throw_if_cancelled().map_err(|_| WatchError::UnknownOrder)?;
            out.send(o.status()).await;                          // server → client
            o = self.advance(&order_id).await;
        }
        Ok(OrderSummary { total_cents: o.total() })
    }

    // outgoing = client → server: `toppings` is a sink the provider pulls from.
    async fn build_order(self, _ctx: Context, base: PizzaKind, mut toppings: OutgoingStream<Topping>)
        -> Result<OrderConfirmation, BuildError>
    {
        let mut order = self.start(base).ok_or(BuildError::OutOfStock)?;
        while let Some(t) = toppings.recv().await { order.add(t); } // client → server
        Ok(order.confirm().await)
    }

    // bidirectional: pull commands, push events.
    async fn kitchen_session(self, _ctx: Context, station: String,
        mut commands: OutgoingStream<KitchenCmd>, events: IncomingStream<KitchenEvent>)
        -> Result<SessionReport, KitchenError>
    {
        if !self.claim(&station) { return Err(KitchenError::StationBusy); }
        let (mut plated, mut burned) = (0, 0);
        while let Some(cmd) = commands.recv().await {              // client → server
            match self.cook(cmd).await {
                Ok(d)  => { plated += 1; events.send(KitchenEvent::Plated(d)).await; } // server → client
                Err(d) => { burned += 1; events.send(KitchenEvent::Burned(d)).await; }
            }
        }
        Ok(SessionReport { plated, burned })
    }
}
```

### Serving it — per-connection callback (no registry/builder)

Mirrors the TS `ITransportServer.setConnectionHandler((transport) => …)` + `withProvenance`.
A listener accepts sockets and invokes the callback per connection; a shared `PizzaShop` is
captured by `Arc`.

```rust
#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let shop = Arc::new(PizzaShop::new());

    serve_unix("/tmp/pizza.sock", move |conn: &HubRpcConnection, prov: &ConnectionProvenance| {
        let _who = prov;                                     // verified uid/pid/container (pre-RPC)

        conn.serve(shop.clone().serve());                    // interface inferred from handler
        conn.enable_reflection(Default::default());          // directory / schemas / defaults
        conn.set_preset(PizzaService::interface());          // optional bare-method dispatch
    })
    .await
}
```

For a single peer link (stdio, or a hub connection) there is no acceptor — build one
connection directly:

```rust
let conn = HubRpcConnection::new(StdioTransport::new());
conn.serve(Arc::new(PizzaShop::new()).serve());          // interface inferred; use serve_as("id", …) to mount
conn.run().await?;
```

---

## 3. Consumer — uses the generated `Client` struct

The consumer depends on the **same shared crate** and drives the generated
`PizzaServiceClient`. Methods are `ctx`-first (tarpc convention); the caller passes
`context::current()`.

```rust
use hubrpc::prelude::*;

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let conn = HubRpcConnection::connect(UnixSocketTransport::dial("/tmp/pizza.sock").await?);
    let pizza = PizzaServiceClient::new(stub_config(), conn.clone()).spawn();

    // 1) plain request/response — inline params:
    let c = pizza.order(context::current(), PizzaKind::Pepperoni, Size::Large, 2).await?;
    println!("order {} — eta {}min — {}¢", c.order_id, c.eta_minutes, c.price_cents);

    // 2) incoming stream (server → client): yield items, then the final result.
    let mut call = pizza.watch_order(context::current(), c.order_id.clone());
    while let Some(status) = call.next().await {              // server → client
        println!("status: {status:?}");
    }
    let _summary: OrderSummary = call.finish().await?;

    // 3) outgoing stream (client → server): push items, close, await result.
    let call = pizza.build_order(context::current(), PizzaKind::Margherita);
    call.send(Topping { name: "mushroom".into(), cents: 150 }).await?; // client → server
    call.send(Topping { name: "basil".into(),    cents: 100 }).await?;
    let _confirm: OrderConfirmation = call.finish().await?;

    // 4) bidirectional: interleave send + next on the same handle.
    let mut session = pizza.kitchen_session(context::current(), "grill-1".into());
    session.send(KitchenCmd::Fire("ticket-7".into())).await?;  // client → server
    while let Some(ev) = session.next().await {                // server → client
        println!("kitchen: {ev:?}");
        if let KitchenEvent::Plated(_) = ev { break; }
    }
    let _report: SessionReport = session.finish().await?;

    // 5) notification — no result, returns ().
    pizza.cancel_order(context::current(), c.order_id).await;

    // 6) reflection — the directory interface is just another generated client.
    let entries = HubRpcDirectoryClient::new(stub_config(), conn).spawn()
        .list(context::current(), Default::default()).await?;
    println!("peer exposes {} interfaces", entries.len());

    Ok(())
}
```

---

## 4. Attested caller via `Context`

There is no generic `Cx` type parameter. The attested peer identity is read dynamically off
`Context`, populated from the transport's verified provenance. The same `Context` also
carries the wire request id and a cancellation token.

```rust
impl PizzaService for PizzaShop {
    async fn order(self, ctx: Context, kind: PizzaKind, size: Size, quantity: u32)
        -> Result<OrderConfirmation, RpcError>
    {
        let who: Option<&Participant> = ctx.get::<Participant>(); // attested caller, if any
        let _id  = ctx.request_id();                             // wire id for correlation
        let _tok = ctx.cancelled();                              // CancellationToken

        if !self.authorized(who) {
            return Err(RpcError::forbidden("not allowed to order"));
        }
        // … place order …
        # unimplemented!()
    }
}
```

---

## 5. What you get "for free"

Registering `PizzaService` on a connection also exposes the three reflection interfaces, so
any peer can discover and introspect the service without prior knowledge:

- **`hubrpc.directory`** — list the interfaces this connection serves (`id@hash`).
- **`hubrpc.schemas`** — fetch the full `HubRpcInterfaceSchema` for an `id@hash`.
- **`hubrpc.defaults`** — connection defaults (preset interface, etc.).

Because the interface identity is a content hash of the (normalized) schema, a Rust server
and a TS client agree on `com.acme.pizza@<hash>` only when their contracts are structurally
identical — mismatches are detected up front rather than as runtime decode errors.
