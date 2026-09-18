# linkrpc-rust — The Hub

> **Status: design sketch.** This explores how a **hub** looks for the Rust port, mirroring the
> TypeScript `@hediet/linkrpc-hub` package. The hub is an *optional, separate crate* layered on top
> of the core (`docs/architecture.md`) — the core RPC/connection model does not depend on it.
> Signatures are illustrative.

A single `LinkRpcConnection` links exactly **two** peers. A **hub** is what you reach for when you
want *many* participants to discover and call each other over one endpoint (a WebSocket server, a
Unix socket, an iframe bus). It is a **message router**, not an RPC server: it forwards
JSON-RPC messages between links based on the `serviceId` prefix in the method name, and serves a
small set of well-known *root services* (directory, access/consent, identity) to each participant.

---

## 1. The one noun: `Hub` (a longest-prefix router)

> The entire model is **one noun: the `Hub`**. Everything else is built from hubs.

A `Hub` is a prefix router with three kinds of link:

```rust
struct Hub { /* forwarding table + pending-forward map + timers */ }

struct AttachedLink { /* handle; drop = detach + release this link's prefixes */ }

impl Hub {
    fn attach(&self, link: impl MessageTransport) -> AttachedLink;   // add a routable link
    fn attach_out(&self) -> (AttachedLink, impl MessageTransport);   // attach + get the paired end
    fn set_loopback(&self, link: Option<impl MessageTransport>);     // local root services
    fn set_uplink(&self, link: Option<impl MessageTransport>);       // parent hub (nesting)
}
```

- **forwarding table** — `serviceId` prefix → the link that owns it (claimed by a participant).
- **loopback** — an optional link to the hub's *local root services* (reflection, etc.).
- **uplink** — an optional link to a *parent* hub, so hubs nest.

### How a method name resolves

Routing is driven entirely by the [`::` method-name grammar](architecture.md#method-name-grammar):

```rust
enum Resolution {
    Malformed,
    Loopback { link: Option<Link> },              // bare / interfaceId::member  → root services
    Prefix   { link: Link, prefix: ServiceId },   // serviceId::…                → the prefix owner
    Uplink   { link: Option<Link> },              // unknown serviceId           → parent hub
}
```

The **structural invariant**: *root-addressed* calls (bare or `interfaceId::member`) resolve to
**loopback only** — they are **never** forwarded to a prefix owner or the uplink. A participant's
own root services are private to it; the hub's are private to the hub. Fully-qualified
(`serviceId::interfaceId::member`) calls route by longest-prefix match, falling through to the
uplink when no local owner claims the prefix.

### Stateless-ish forwarding (id rewriting + anti-forgery)

When the hub forwards a *request*, it rewrites the JSON-RPC `id` and records a small
`PendingForward { origin, original_id, target, timer }`. The response is accepted **only from the
exact link the request was routed to** — a downstream can't forge a reply (with a guessed
sequential id) to a request that went elsewhere. An **idle timer** bounds the pending map (the
hub's slow-loris / leak backstop): a forwarded request whose stream is idle too long is cancelled
toward its target and failed back to its origin with `requestTimeout`.

The hub only ever logs *routing decisions* (method name, resolution kind, claimed prefixes) —
**never payloads** — so attaching a logger never leaks call arguments.

---

## 2. Two different "splitters" — don't conflate them

| primitive | shape | job |
|---|---|---|
| **`MultiplexedTransport`** (mux) | 1 base → N logical transports | carry several *independent connections* over one pipe, keyed by a channel id (`MuxEnvelope { $mux, ch, m }`) |
| **`OverlaySplitter`** | 3 ports (P / C / H) | the hub's **per-participant** router — splits a participant's traffic between its local root services and the parent hub |

```rust
// Mux: fan one transport into named logical transports.
struct MuxEnvelope { mux: &'static str /* "v1" */, ch: String, m: Message }
let mux = MultiplexedTransport::new(base, &["control", "data"]);
let control = mux.channel("control");   // each is an ordinary MessageTransport
```

The mux is plumbing (one wire, many connections). The **overlay splitter** is the hub's routing
heart, below.

---

## 3. `OverlaySplitter` — the per-participant 3-port primitive

Each participant gets its own tiny router with **three ports**:

- **P** — the participant (the downstream connection),
- **C** — the participant's *local root services* connection (directory, identity, access, …),
- **H** — the *uplink* to the parent `Hub`.

```text
routing table (method-form based):

   P·root → C          P·*    → H          (root = bare/interface form; * = serviceId::… )
   H·*    → P          H·root → drop
   C·*    → P

responses follow the reverse path.
```

So a participant's **root-addressed** calls reach *its own* root services (C); its
**fully-qualified** calls go up to the hub (H) to be routed to whoever owns the prefix; anything
the hub or the root services send down reaches the participant (P).

```rust
struct OverlaySplitter {
    participant: Link,  // P
    root:        Link,  // C  (local root services)
    uplink:      Link,  // H  (parent Hub)
}
```

### Stateless demux via id-tagging

P is the only port that receives requests from **two** sources (H and C), so a response from P is
ambiguous and ids can collide (H and C may each send `id: 5`). Rather than a pending map, the
splitter rewrites **only requests destined for P**, prepending the origin and id-type:

```text
   H→P:  id' = "H\0n\042"      (push origin tag + type)
   C→P:  id' = "C\0s\0abc"
   P→?:  decode id' → route to H or C, restore the original id
```

Every other edge passes **verbatim** (single peer, no ambiguity). The splitter therefore holds
**no per-request state** — the id literally encodes its own return path. `$stream::send` frames are
correlated by their `requestId`, not by method form, so they follow the matching request's path
exactly like a response (never dropped by `H·root → drop`).

---

## 4. `RootOverlay` — the per-participant front door

A `RootOverlay` is an `OverlaySplitter` wiring plus a `LinkRpcConnection` for the participant's
**root services**. It deliberately has **no routing table and no knowledge of which services it
serves** — it represents exactly one participant.

```rust
struct RootOverlay {
    /// The per-participant root connection. Register root services here; reachable only by
    /// this overlay's participant (root-addressed calls land here, never forwarded).
    root: LinkRpcConnection,
}

impl RootOverlay {
    fn new(uplink: impl MessageTransport) -> Self;          // uplink = link to the parent Hub
    fn connect_participant(&self, participant: impl MessageTransport); // wire P through the splitter
}
```

Because an overlay is one participant, **prefix claims happen on the parent hub** (via the
participant's uplink), not in any local table.

---

## 5. The root services (served at the overlay root)

These are ordinary linkrpc interfaces served on `overlay.root`. They are addressed in **root form**
(`interfaceId::member`), so by the splitter rule `P·root → C` they are reached **directly and are
never forwarded → never capability-gated**. That's what makes them safe bootstrap surfaces.

| interface | members | purpose |
|---|---|---|
| `hubrpc.directory` | `list` | **referral-only** at a hub: one `<prefix>::hubrpc.directory` row per claimed prefix |
| `hubrpc.schemas` | `get` | schema for an `id@hash` |
| `hubrpc.defaults` | `get` | connection preset, if any |
| `hubGrantedServiceId` | `get`, `register` | connection facts + **capability-free** serviceId claims *within* the granted namespace |
| `hubParticipant` | `registerServiceId` | claims **outside** the granted namespace (admin-capability gated, addressed via the hub's own serviceId) |
| `hubAccess` | `request`, `extend`, `requestAccess` | the **consent front door** — a consumer asks for scoped access; returns `SignedCapability`s |
| `identity` (+ `identity.storage`) | `sign`, `getNodeId`, … | per-participant key oracle (lazy; private keys never leave the executor) |

The split between the two claim doors is deliberate:

- `hubGrantedServiceId::register` — claim a prefix at/under the namespace your **provenance**
  already granted at attach time. No signature, no capability (the grant happened out of band).
- `<hubServiceId>::hubParticipant::registerServiceId` — claim a prefix **outside** that namespace.
  Fully-qualified (so it forwards to the hub) and **admin-capability gated**.

`hubrpc.directory::list` is **referral-only** on a hub: it lists a row *per claimed prefix*
(`<prefix>::hubrpc.directory`) rather than the prefix's leaf interfaces. Flattening the tree into a
full inventory is the consumer's job — a `walk_hub_detailed` breadth-first walk over the referral
tree (shared by the CLI and the hub's own access-candidate resolution).

---

## 6. Attaching a transport to a hub (the acceptor)

The top-level object is a **`HubConnectionAcceptor`**: it consumes an `ITransportServer` (a source
of inbound transports — the same per-connection seam from `architecture.md`) and bridges each
accepted transport onto the hub graph **with a root overlay providing dir + hubAccess + identity**.

```rust
let acceptor = HubConnectionAcceptor::new(HubConnectionAcceptorOptions {
    server,                                  // e.g. withProvenance(ws_server, provider)
    hub: hub.clone(),                        // the central Hub all participants attach under
    resolve_identity: Some(/* lazy: mint/lookup Identity from transport.provenance() */),
    policy: Some(prefix_policy),             // authorize each prefix claim
    granted_service_id_namespace: Some(/* e.g. the attested identity key */),
    hub_access: Some(HubAccessConfig { handlers, fetch_directory }),  // consent front door
    forward_checking: ForwardCheckingPolicy::Capability { admin_ids }, // see §7
    ..Default::default()
});
```

Per accepted transport the acceptor does (this is the **canonical wiring**):

```rust
fn wire(&self, transport: T) -> AcceptedConnection {
    let (a, b) = TransportPair::new();

    // 1. optionally gate the hub-facing link, then attach an uplink to the central hub.
    let hub_facing = self.gate_hub_facing(b);          // withForwardedCallGate (§7) or passthrough
    let upstream   = self.hub.attach(hub_facing);

    // 2. build the per-participant overlay; its uplink is the other end of the pair.
    let overlay = RootOverlay::new(a);

    // 3. install root services on overlay.root:
    register_hub_services(&overlay.root, &upstream, /* hub_service_id, granted ns, authorize_claim */);
    if let Some(access) = &self.hub_access {           // consent front door (root form → never gated)
        register_hub_access_service(&overlay.root, access);
    }
    if let Some(resolve) = &self.resolve_identity {    // lazy identity::*  + identity.storage::*
        register_identity_services(&overlay.root, resolve, transport.provenance());
    }

    // 4. connect the participant (optionally stamping a verified-signature context).
    overlay.connect_participant(with_verified_context(transport, self.verify_signatures));
    AcceptedConnection { overlay, upstream }
}
```

On transport close: drop the overlay (detaches the splitter) and the `upstream` link — **detaching
the uplink releases the participant's claimed prefixes** on the hub. Identity is resolved
**lazily** (only when the participant first calls `identity::*`), so merely accepting a connection
mints no key material.

---

## 7. Forwarded-call gating (where the cap gate lives)

The hub-facing link of each participant is optionally wrapped in `with_forwarded_call_gate` — the
receive-side **capability gate** from [`architecture.md`](architecture.md#the-capability-gate-receive-side).
The policy is a two-arm choice — gating is either **off** or **capability**; there is no
authenticity-only (signature-without-capability) middle ground:

```rust
enum ForwardCheckingPolicy {
    /// Forwarded calls reach the hub unverified.
    Off,
    /// Forwarded (fully-qualified) calls must carry a valid `$hubrpc` signature AND a capability
    /// rooted at one of `admin_ids`. `admin_ids` is REQUIRED (an empty anchor set fails closed →
    /// rejects everything, including validly granted capabilities).
    Capability { admin_ids: Vec<NodeId> },
}
```

Only **forwarded** (fully-qualified) traffic crosses this gate. Root-form calls — including the
`hubAccess::*` consent front door — are served at the overlay root and **never forwarded**, so they
need no capability and are always reachable (that's how a brand-new consumer can negotiate access
before it holds any token).

---

## 8. Nesting hubs

Because the `OverlaySplitter`'s `H` port is just "a link to a parent hub," and a hub's `uplink` is
just another link, **hubs compose**: a participant of hub A can itself be hub B (attach B's link as
A's participant, point B's uplink at A). Prefix claims bubble up; root-addressed calls always stay
local. This is the same primitive at every level — "everything is a hub."

---

## 9. Putting it together — a minimal WebSocket hub

```rust
#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let admin = KeypairIdentity::load_or_generate("hub-admin.key")?;
    let hub   = Hub::new(HubOptions { debug_name: "ws-hub".into(), ..Default::default() });

    // The hub's own reflection/services live on a loopback link.
    install_hub_services(&hub, HubServicesOptions { hub_service_id: "hub".into() });

    // Accept WebSocket transports, attesting each peer's token before exposing it.
    let server = with_provenance(websocket_server("0.0.0.0:7878"), token_attestor());

    let _acceptor = HubConnectionAcceptor::new(HubConnectionAcceptorOptions {
        server,
        hub: hub.clone(),
        resolve_identity: Some(managed_identity_resolver(&admin)),
        hub_access: Some(HubAccessConfig { handlers: consent_handlers(), fetch_directory }),
        forward_checking: ForwardCheckingPolicy::Capability { admin_ids: vec![admin.node_id()] },
        ..Default::default()
    });

    futures::future::pending::<()>().await; // serve until killed
    Ok(())
}
```

A participant then connects with an ordinary `LinkRpcConnection`, claims a prefix
(`hub.register_service_id_namespace("acme")`), serves its interfaces under it, and other
participants discover it via `hubrpc.directory::list` and call it through the hub by
`acme::com.acme.pizza::order`.

---

## 10. Rust placement & phasing

- **Separate crate** (`crates/linkrpc-hub`), depending on `linkrpc` core. The core (P1–P5) never
  imports it.
- Building blocks already modeled in the core: `MessageTransport`, `TransportPair`,
  `MultiplexedTransport`, the `::` grammar, `$stream::send` correlation, and the receive-side
  capability gate.
- Hub-specific pieces to add: `Hub` (forwarding table + pending-forward + idle timers),
  `OverlaySplitter`, `RootOverlay`, the root-service interfaces (`hubGrantedServiceId`,
  `hubParticipant`, `hubAccess`, `identity`), `HubConnectionAcceptor`, prefix policy, and the
  directory referral-walk.
- This is **post-core** work (call it P6); it is only needed when you want many-participant
  discovery/routing rather than a direct two-peer link.
