# linkrpc

Typed, multiplexed RPC over a single bidirectional connection — a Rust implementation of the
[LinkRPC protocol](../../../spec).

## Installation

Add the published package to your project:

```toml
[dependencies]
linkrpc = "0.1.0"
```

The `linkrpc-macros` dependency is included automatically; its procedural macros
are re-exported by `linkrpc`.

linkrpc is a **specialization of [JSON-RPC 2.0](https://www.jsonrpc.org/specification)**: every
message on the wire is a valid JSON-RPC message, and linkrpc adds just enough on top to make many
strongly-typed services share one connection — an addressing grammar, content-hashed interface
identity, built-in reflection, and optional layers for streaming, signing, and capabilities.

You write a plain Rust trait. The `#[link_rpc_interface]` macro turns it into a server adapter, a
typed client, and a self-describing interface descriptor. The rest — routing, request/response
correlation, schema validation, reflection — is handled for you.

```rust
use linkrpc::prelude::*;

/// Order pizzas and track delivery.
#[link_rpc_interface(id = "com.acme.pizza")]
pub trait PizzaService {
    /// Place a new pizza order. Charges money.
    #[annotations(dangerous)]
    async fn order(kind: PizzaKind, size: Size, quantity: u32)
        -> Result<OrderConfirmation, JsonRpcError>;

    /// Fire-and-forget cancel request.
    #[notification]
    async fn cancel_order(order_id: String);
}
```

From that one trait you get:

| Generated item | Role |
|---|---|
| `PizzaService` (rewritten) | the trait your provider implements (`&self`, `&CallCtx`-first) |
| `PizzaServiceServer` | an `InterfaceHandler` adapter — register it and serve |
| `PizzaServiceClient` | a typed caller proxy with plain `async fn` methods |
| `pizza_service::interface()` / `::ID` | the interface descriptor and its content hash |

## Why content-hashed interfaces?

Every interface has an identity of the form `com.acme.pizza@<hash>`, where the hash is computed
from the *normalized* schema of its members (parameter types, result types, docs, annotations).
Two peers agree on an interface **only when their contracts are structurally identical** — so a
Rust server and a TypeScript client interoperate at
`com.acme.pizza@<hash>` only if their types line up exactly. Mismatches surface up front as a hash
disagreement instead of as a runtime decode error three calls later.

The hashing is byte-for-byte defined (RFC 8785 JCS → SHA-256, truncated to 16 hex chars) and pinned
by a shared [conformance corpus](../../../conformance), so independent implementations in different
languages compute the same hash for the same contract.

## Serving and calling

A connection wraps any message transport and hosts a live registry of interfaces. Registering an
interface also lets you opt into reflection, so peers can discover and introspect your services
with no prior knowledge.

```rust
use linkrpc::prelude::*;
use std::sync::Arc;

// in-memory pair for tests; real transports live in `linkrpc-tokio`
let (a, b) = transport_pair();
let client_conn = LinkRpcConnection::new(Box::new(a));
let server_conn = LinkRpcConnection::new(Box::new(b));

server_conn.register(
    Arc::new(pizza_service::interface()),
    Arc::new(PizzaServiceServer::new(Arc::new(Shop::default()))) as Arc<dyn InterfaceHandler>,
    RegisterOptions::default(),
)?;
server_conn.enable_reflection();   // directory / schemas / defaults, for free

let pizza = PizzaServiceClient::new(client_conn.clone());
tokio::spawn(async move { server_conn.run().await });
tokio::spawn(async move { client_conn.run().await });

let confirmation = pizza.order(PizzaKind::Pepperoni, Size::Large, 2).await?;
```

See [`crates/linkrpc-tokio/tests/macro_pizza.rs`](../linkrpc-tokio/tests/macro_pizza.rs) for the full
runnable example.

## Reflection — discoverability built in

Calling `enable_reflection()` exposes three standard interfaces backed by the connection's live
registry:

- **`hubrpc.directory`** — which services and interfaces this connection serves (each with its `id@hash`).
- **`hubrpc.schemas`** — the full schema for any advertised interface.
- **`hubrpc.defaults`** — the connection's preset service/interface, if any.

Because a node's contract is observable from the boundary, tools and conformance checkers can
discover what a node offers rather than being told.

## A layered protocol

linkrpc is built in additive layers. Lower layers stand alone; higher ones ride in reserved
`$hubrpc`-prefixed members that a peer who doesn't implement them treats as opaque. That's what lets
a minimal "Core" node and a full "Capability" node interoperate on any call that needs no gated
authority.

| Layer | What it adds |
|---|---|
| Messages | JSON-RPC envelope, `::` method grammar, error codes |
| Transport | framed whole-message byte stream + endpoint URIs |
| Interfaces | schema format, JSON Schema subset, the interface hash |
| Reflection | `hubrpc.directory` / `.schemas` / `.defaults` |
| Streaming *(optional)* | in-flight correlated stream messages |
| Identity *(optional)* | Ed25519-signed calls |
| Capabilities *(optional)* | signed grants + an authorization gate |

The full normative protocol lives in the repository's shared [`spec/`](../../../spec) directory.

## The crates

| Crate | Contents |
|---|---|
| **`linkrpc`** (this crate) | protocol core, schema/hashing, the connection runtime, reflection |
| **`linkrpc-macros`** | the `#[link_rpc_interface]` procedural macro |
| **`linkrpc-tokio`** | Tokio transports (NDJSON over Unix sockets) |

## Status

This is a port-in-progress of the TypeScript `@hediet/linkrpc` framework. The non-streaming **wire
core, schema hashing, the connection runtime, reflection, and the `#[link_rpc_interface]` macro are
implemented and interop-tested** against the TS reference via the conformance vectors. Streaming,
identity (Ed25519), and capabilities are specified and on the roadmap — see
[`executionPlan.md`](../../executionPlan.md). The `incoming_stream` / `outgoing_stream` macro
attributes are reserved but not yet wired.

## License

MIT.
