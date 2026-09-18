# linkrpc-rust — Architecture & Theory

> **Status: design.** This describes the *model* we are building — the concepts, how they
> relate, and why. It is intentionally a "theory" document: signatures are illustrative and
> need not be 100% accurate or implemented yet. For the concrete target DX, see
> [`examples.md`](./examples.md); for the build plan, see `plan.md`.

linkrpc is a **dialect of JSON-RPC 2.0** that lets many typed services share one bidirectional
connection. It does not invent a transport, an auth scheme, or a streaming framing beyond
JSON-RPC — it layers *routing*, *content-hashed interface identity*, *reflection*, and optional
*streaming* and *signing* on top.

The Rust port has two cooperating halves:

- a **macro layer** (`#[link_rpc_interface]`, tarpc-style) that gives ergonomic, typed traits/clients;
- a **protocol/runtime layer** that multiplexes, routes, validates, and moves bytes.

They meet at the **connection**.

---

## 1. The layer cake

```
┌──────────────────────────────────────────────────────────────┐
│  Your trait + impl + generated Client          (application)  │
├──────────────────────────────────────────────────────────────┤
│  Macro output: rewritten trait, Client, serve() handler,      │
│  interface() descriptor (id@hash)              (linkrpc-macros) │
├──────────────────────────────────────────────────────────────┤
│  Connection: routing (::), dispatch, validation, reflection,  │
│  preset, streaming correlation                 (connection/)  │
├──────────────────────────────────────────────────────────────┤
│  Channel: request/response correlation, inbound demux         │
├──────────────────────────────────────────────────────────────┤
│  Schema + identity: InterfaceSchema, JCS, hash → id@hash      │
├──────────────────────────────────────────────────────────────┤
│  JSON-RPC wire: request/notification/response, error codes,   │
│  method-name grammar a::b::c                   (protocol/)    │
├──────────────────────────────────────────────────────────────┤
│  Transport: framed whole-message byte stream   (transport/)   │
└──────────────────────────────────────────────────────────────┘
```

Each layer depends only on the one beneath it. The macro layer is *optional sugar*: everything
it produces (handlers, clients, schemas) could be written by hand against the runtime layer.

---

## 2. Key definitions

### Message
A single, complete JSON-RPC value: a **request** (has `id`, expects a response), a
**notification** (no `id`, no response), or a **response** (`result` or `error`, carries the
matching `id`). linkrpc never sees partial messages — framing is the transport's job.

### Transport
The lowest abstraction: a **bidirectional stream of whole messages** between this process and
exactly one peer. Its single responsibility is *framing* — turning a byte stream into discrete
messages (ndjson, length/header-delimited, WebSocket frames). It knows **nothing** about
interfaces, methods, or identity.

It is **generic over the per-direction payload**, defaulting to the bare wire `Message`. The
generics exist so an *in-process producer* can staple **out-of-band context** onto messages
(e.g. attested `Participant`, inbound HTTP headers) without that context ever touching the wire:

```rust
trait MessageTransport: Send + Sync {
    type In  = Message;     // what this side receives  (may carry out-of-band context)
    type Out = Message;     // what this side sends
    async fn send(&self, msg: Self::Out) -> Result<(), TransportError>;
    async fn recv(&self) -> Option<Self::In>;          // None = peer closed
}

/// The envelope used when a transport wants to attach in-process context.
/// `ctx` is NEVER serialized — the wire only ever carries `msg`.
struct WithContext<C> { msg: Message, ctx: C }
```

Two rules of thumb fall out of this:
- **Inbound metadata** (HTTP headers, peer identity) rides as context on `In`
  (`In = WithContext<HttpMeta>`), read later through `Context`. The wire `Message` stays pure.
- **Per-send configuration** ("set this header", "use this signer", priority) does **not** go in
  the message type — it belongs in `SendOpts` on the channel (see below). The message type should
  describe *payload*; per-call knobs are *options*. Keeping them apart stops the wire type from
  absorbing transport-specific configuration.

Concrete transports (Unix socket, TCP, stdio, WebSocket) live in `linkrpc-tokio`; the core ships
only the trait and an in-memory pair for tests:

```rust
// Test/in-process: a connected pair with no identity (In = Out = Message).
let (a, b) = InMemoryTransport::pair();
let server = LinkRpcConnection::new(a);
let client = LinkRpcConnection::new(b);
```

### Provenance — a *separate* capability trait
Trustworthy facts about the *peer*, established **once by the acceptor** before any RPC runs
(Unix peer-cred uid/pid, container id, a TLS identity, a verified public key). It is **not** part
of `MessageTransport`, because most transports are anonymous and provenance has a different
source and lifecycle. Attested transports opt in via a second trait:

```rust
struct ConnectionProvenance {
    identity_key: Option<IdentityKey>,        // e.g. verified public key / cert subject
    attributes:   HashMap<String, JsonValue>, // uid, pid, container, peer_addr, …
}

/// Orthogonal capability — only attested transports implement it.
trait Attested {
    fn provenance(&self) -> &ConnectionProvenance;
}
```

This mirrors the TS `withProvenance` decorator: you can wrap *any* `MessageTransport` to add
attestation without each transport reimplementing it.

```rust
// A plain transport, later wrapped with attestation by the acceptor:
let attested = WithProvenance::new(raw_transport, ConnectionProvenance {
    identity_key: None,
    attributes: HashMap::from([("uid".into(), json!(1000)), ("pid".into(), json!(4242))]),
});
```

The application reads it where it matters — at **registration** (per-peer wiring) and in
**`Context`** (per-call authorization), never from the messaging path.

### Identity (signing / wrapping) — the cryptographic vocabulary
*Provenance* is what the **transport** vouches for; *identity* is what a peer **cryptographically
proves** via the optional `$hubrpc` envelope (P5). The layered vocabulary, ported from TS:

```rust
/// Branded ids — newtypes, NOT bare aliases, so you can't pass a random String where an
/// id is expected. A small `brand!` macro stamps them out (NodeId, ServiceId, Nonce, …).
struct NodeId(String);     // base64url of the public signing key — a peer's stable name
struct Nonce(String);      // base64url replay-protection nonce

/// Minimal signing capability — abstract so it can be an in-memory key OR an HSM/keystore.
trait SigningIdentity {
    fn public(&self) -> &PublicSigningIdentity;          // PublicSigningIdentity { node_id: NodeId }
    async fn sign(&self, bytes: &[u8]) -> Result<Signature, CryptoError>;
}

/// Encryption counterpart (X25519 / HPKE).
trait WrappingIdentity {
    fn public(&self) -> &PublicWrappingIdentity;
    async fn wrap(&self, domain: &str, bytes: &[u8]) -> Result<Vec<u8>, CryptoError>;
    async fn unwrap(&self, domain: &str, blob: &[u8]) -> Result<Vec<u8>, CryptoError>;
}

/// Both halves; KeypairIdentity is the in-process Ed25519 + X25519 impl.
trait Identity: SigningIdentity + WrappingIdentity {}

/// A signing identity bundled with its durable capability bag — *who you act as*.
struct Principal { identity: Arc<dyn Identity>, caps: CapBag }
```

`NodeId` is a **branded newtype** derived from the public signing key (a peer's stable
cryptographic name). The chain is: **`SigningIdentity`** (how you sign) → **`Principal`** (who you
are + what you may do) → `$hubrpc` envelope on the wire → **`Participant`** (who the callee
believes called).

### Participant (deferred — not a core concept)
"Who is calling," as seen by a handler. It is **not** baked into the core: handlers read it
through `Context`'s typed extension map, so it is *pluggable and deferrable*.

- **P1–P3 (core RPC):** no `Participant` type exists; handlers never need it. `Context` only
  carries `request_id` + cancellation.
- **P3 (provenance) / P5 (signing):** we *introduce* `Participant`, derived from a verified
  `$hubrpc` signer when present, else from transport provenance (the unsigned fallback). Nothing
  in the core changes — it just becomes available via `ctx.get::<Participant>()`.

```rust
// Available only once provenance/signing land; absent (None) before then:
async fn order(self, ctx: Context, /* … */) -> Result<OrderConfirmation, RpcError> {
    match ctx.get::<Participant>() {
        Some(p) if p.is_staff() => { /* allowed */ }
        _ => return Err(RpcError::forbidden("staff only")),
    }
    # unimplemented!()
}
```

Because identity rides in the extension map rather than a `Context` field (or a generic `Cx`),
**we can ship P1–P3 with no identity type at all** and add `Participant` later without touching
any handler signature.

### Message
A single, complete JSON-RPC value. We model it as a Rust enum and (de)serialize with serde:

```rust
enum Message {
    Request      { id: RequestId, method: String, params: JsonValue },
    Notification { method: String, params: JsonValue },
    Response     { id: RequestId, result: JsonValue },
    Error        { id: RequestId, error: RpcError },
}

enum RequestId { Num(i64), Str(String) }
```

### Channel
The correlation engine sitting directly on a transport. It **owns response correlation itself**
(matches outbound requests to inbound responses by `id`), routes inbound
**requests/notifications** to a bound handler, and (in the streaming phase) routes `$stream::send`
frames to the owning in-flight call. The channel is symmetric — it serves *and* calls over the
same transport — and is **generic over per-call context** in both directions:

```rust
// InCtx  = context the transport attached to inbound messages (Participant / headers).
// OutCtx = per-call override bag a sender decorator understands (e.g. signer override).
struct Channel<InCtx = (), OutCtx = ()> { /* … */ }

impl<InCtx, OutCtx> Channel<InCtx, OutCtx> {
    fn sender(&self) -> &dyn RequestSender<OutCtx>;
    fn set_request_handler(&self, h: impl RequestHandler<InCtx> + 'static);
}

/// Caller side. `SendOpts` is where per-call "configuration" lives — NOT the message type.
trait RequestSender<OutCtx> {
    async fn request(&self, method: &str, params: JsonValue, opts: SendOpts<OutCtx>)
        -> Result<JsonValue, RpcError>;
    fn notify(&self, method: &str, params: JsonValue, opts: SendOpts<OutCtx>);
    fn request_with_stream(&self, method: &str, params: JsonValue, opts: SendOpts<OutCtx>)
        -> RawStreamingCall;
}

struct SendOpts<OutCtx> {
    ctx: Option<OutCtx>,            // decorator override bag (signer/caps override, …)
    interface_hash: Option<String>,// interface-level metadata stamped by the connection
}
```

#### How dispatch works (the callee side)

Dispatch is **not** a bare closure. The channel parses inbound wire messages and, for each
inbound *request/notification*, builds a rich `IncomingCall` and invokes the bound
`RequestHandler`:

```rust
trait RequestHandler<InCtx> {
    async fn handle_request(&self, call: IncomingCall<InCtx>) -> CallResult;  // result | error
    fn handle_notification(&self, call: IncomingCall<InCtx>);                 // no response
}

struct IncomingCall<InCtx> {
    method:  String,
    params:  JsonValue,
    context: InCtx,                  // out-of-band ctx the transport attached (Participant/headers)
    stream:  IncomingStream,         // per-call streaming handle, both directions; auto-detached
    signal:  CancellationToken,      // aborts on caller cancel / disconnect / idle
}

enum CallResult { Ok(JsonValue), Err(RpcError) }
```

**`LinkRpcConnection` is the `RequestHandler`.** Its `handle_request` does the linkrpc-specific
work: parse the `::` method name, find the registered handler, validate params against the
interface schema, assemble a `Context` from `call.context` + `call.signal`, invoke the service
method, and encode the typed `Result<R, E>` back into a `CallResult`. The channel never knows
about interfaces — it only knows wire correlation + this one trait.

### Method-name grammar
Routing is encoded in the JSON-RPC `method` string using `::`:

| form | meaning |
|---|---|
| `member` | dispatch to the connection's **preset** interface (vanilla JSON-RPC backcompat) |
| `interfaceId::member` | interface-qualified, mounted at the root |
| `serviceId::interfaceId::member` | fully qualified, mounted under a serviceId |
| `rpc.*` | reserved by JSON-RPC; never used by linkrpc |
| `$stream::*`, `$hubrpc` | reserved linkrpc control/envelope namespaces |

This is how one connection multiplexes many interfaces **without a session handshake**: every
call self-describes its target.

```rust
enum MethodName {
    Bare    { member: String },                                       // → preset interface
    Qualified { interface_id: String, member: String },               // "com.acme.pizza::order"
    Full    { service_id: String, interface_id: String, member: String }, // "uptown::com.acme.pizza::order"
}

fn parse_method(s: &str) -> Result<Target, RouteError> {
    match s.split("::").collect::<Vec<_>>().as_slice() {
        [m]          => Ok(Target::preset(m)),
        [iid, m]     => Ok(Target::qualified(iid, m)),
        [sid, iid, m]=> Ok(Target::full(sid, iid, m)),
        _            => Err(RouteError::Malformed),
    }
}
```

### Member
A single operation on an interface — either a **method** (request → response) or a
**notification** (fire-and-forget). Members have typed params and, for methods, a typed
result + a typed error.

### Interface
A **first-class contract**: an `id`, a set of members, and reusable type `components`. It is the
unit of versioning and discovery.

- **interfaceId** — a stable, human-chosen name (e.g. `com.acme.pizza`). *Intrinsic* to the
  contract; part of the hash.
- **InterfaceSchema** — the full machine-readable description (`LinkRpcInterfaceSchema`) using a
  decidable JSON-Schema subset (`LinkRpcJsonSchema`). Doc comments and `#[annotations]` are
  **normative** (they affect identity); the `comment` field is stripped before hashing.

```rust
struct LinkRpcInterfaceSchema {
    id:         String,                          // "com.acme.pizza"
    hash:       Option<String>,                  // filled in after hashing; omitted while hashing
    members:    Vec<MemberSchema>,
    components: BTreeMap<String, LinkRpcJsonSchema>, // reusable named types ($ref targets)
}

struct MemberSchema {
    name:        String,
    kind:        MemberKind,                     // Method | Notification
    params:      Vec<ParamSchema>,               // inline, by-name (no wrapper object)
    result:      Option<LinkRpcJsonSchema>,       // None for notifications
    error:       Option<LinkRpcJsonSchema>,
    streams:     Vec<StreamSchema>,              // incoming/outgoing, with element type
    description: Option<String>,                 // normative (hashed)
    annotations: Vec<String>,                    // e.g. ["dangerous"] — normative (hashed)
}
```

### Identity (`id@hash`)
An interface's identity is **content-addressed**: `interfaceId@hash`, where `hash` =
SHA-256 (first 8 bytes → 16 hex) of the **JCS-canonicalized** (RFC 8785) schema with the
top-level `hash` omitted and `comment` stripped.

```rust
fn compute_interface_hash(schema: &LinkRpcInterfaceSchema) -> String {
    let mut s = schema.clone();
    s.hash = None;                               // top-level hash omitted
    let normalized = normalize(s);               // strip `comment`, sort, canonical defaults
    let canonical: Vec<u8> = jcs::to_vec(&normalized).unwrap();  // RFC 8785
    let digest = Sha256::digest(&canonical);
    hex::encode(&digest[..8])                    // first 8 bytes → 16 hex chars
}

// Identity = "com.acme.pizza@1f3c9a02b7d45e60"
fn interface_id(schema: &LinkRpcInterfaceSchema) -> String {
    format!("{}@{}", schema.id, compute_interface_hash(schema))
}
```

The consequence: two endpoints agree on `com.acme.pizza@<hash>` **iff their contracts are
structurally identical**. Drift is detected as an *identity mismatch up front*, not as a runtime
decode error. (Cross-language hash parity with the TS impl is an explicit verification task, not
an assumption — see `plan.md` P2.)

### ServiceId vs interfaceId (important distinction)
- The **interfaceId** is *what* the contract is (intrinsic, hashed). It is inferred from a
  handler — a handler is statically bound to exactly one interface.
- The **serviceId** is *where* an interface is mounted on a given connection (a deployment
  choice, not part of the contract). It exists so the **same interface can be mounted multiple
  times** on one connection (e.g. two shops). It is therefore supplied **explicitly** at
  registration and may be omitted (root mount).

```rust
conn.serve(downtown.serve());                 // root:     com.acme.pizza::order
conn.serve_as("uptown", uptown.serve());      // mounted:  uptown::com.acme.pizza::order
conn.serve_as("downtown", downtown.serve());  // same interface, second mount
```

So registration needs **no interface argument** (inferred) but optionally a serviceId.

### Handler
The callee side of an interface: an object that *implements* the (macro-rewritten) trait. The
generated `serve()` adapter turns it into a uniform dispatch function the connection can call,
and carries the interface's `id@hash` so the connection can register it without being told.

```rust
// What the macro's serve() roughly produces — a self-describing handler:
struct ServeHandler<T> { inner: Arc<T>, interface: &'static LinkRpcInterfaceSchema }

impl<T: PizzaService> Handler for ServeHandler<T> {
    fn interface(&self) -> &LinkRpcInterfaceSchema { self.interface }  // ← inferred id@hash

    async fn dispatch(&self, member: &str, ctx: Context, params: JsonValue, streams: Streams)
        -> Outbound
    {
        match member {
            "order" => {
                let (kind, size, quantity) = decode_params(params)?;   // by-name → tuple
                let r = self.inner.clone().order(ctx, kind, size, quantity).await;
                encode_result(r)
            }
            // … one arm per member, generated …
            _ => Outbound::Error(RpcError::method_not_found(member)),
        }
    }
}
```

### Client (proxy)
The caller side: a generated `XxxClient` **struct** whose methods marshal params, send a
request under the right `::` method name, await the response, and decode the typed result/error.
Provider and client **deliberately diverge** (tarpc "Option A"): the trait is the callee's
handler contract; the client is a separate generated proxy. No location-transparency requirement.

```rust
// Roughly what a generated client method looks like:
impl PizzaServiceClient {
    pub async fn order(&self, ctx: Context, kind: PizzaKind, size: Size, quantity: u32)
        -> Result<OrderConfirmation, RpcError>
    {
        let params = encode_params!(kind, size, quantity);            // by-name object
        let method = self.method_name("order");                       // "com.acme.pizza::order"
        let raw = self.conn.channel().request(&method, params).await?;
        decode_result(raw)
    }
}
```

### Connection (`LinkRpcConnection`)
The runtime hub, **one per connected peer** (one transport = one symmetric, bidirectional link).
It owns:

- a **registry** of served interfaces (keyed by `serviceId? + interfaceId@hash`),
- **routing & dispatch** (parse `::`, validate params against schema, invoke the handler),
- **outbound proxying** (hand out `Client`s that call the peer),
- **reflection** (the three interfaces below),
- an optional **preset** interface for bare-`member` calls.

```rust
impl LinkRpcConnection {
    fn new(transport: impl MessageTransport + 'static) -> Self;

    fn serve(&self, handler: impl Handler + 'static);                 // root mount, id@hash inferred
    fn serve_as(&self, service_id: &str, handler: impl Handler + 'static);

    fn client<C: LinkRpcClient>(&self) -> C;                           // outbound proxy for an interface
    fn set_preset(&self, interface: &LinkRpcInterfaceSchema);          // bare-method dispatch target
    fn enable_reflection(&self, opts: ReflectionOptions);

    async fn run(self) -> Result<(), ConnError>;                      // pump until the peer closes
}
```

Because it is symmetric, both peers can serve *and* call over the same link.

### Preset
An interface nominated as the default target for bare `member` calls (no `::`). This is the
backwards-compatibility bridge to vanilla JSON-RPC clients that don't know about linkrpc routing.

### Reflection
Discovery is *not* a special protocol — it is **three ordinary interfaces** auto-registered
alongside your service, callable like anything else:

- `hubrpc.directory` — which interfaces this connection serves (their `id@hash`).
- `hubrpc.schemas` — fetch the full `InterfaceSchema` for an `id@hash`.
- `hubrpc.defaults` — connection defaults (e.g. the preset).

```rust
// They are literally link_rpc_interfaces too:
#[link_rpc_interface(id = "hubrpc.directory")]
trait LinkRpcDirectory {
    async fn list() -> Result<Vec<DirectoryEntry>, RpcError>;  // [{ service_id?, interface_id_hash }]
}

#[link_rpc_interface(id = "hubrpc.schemas")]
trait LinkRpcSchemas {
    async fn get(id_hash: String) -> Result<LinkRpcInterfaceSchema, RpcError>;
}

// A peer that knows nothing can bootstrap:
let dir = conn.client::<LinkRpcDirectoryClient>();
for e in dir.list(Context::current()).await? {
    let schema = conn.client::<LinkRpcSchemasClient>().get(Context::current(), e.id_hash).await?;
    println!("{} has {} members", schema.id, schema.members.len());
}
```

A peer can therefore connect knowing nothing and learn the whole surface at runtime.

### Context
Per-request ambient state handed to every handler invocation (tarpc's `Context`, extended):

```rust
impl Context {
    fn request_id(&self) -> &RequestId;          // wire id, for correlation/logging
    fn cancelled(&self) -> &CancellationToken;   // tied to the call's AbortSignal + cancel frames
    fn throw_if_cancelled(&self) -> Result<(), Cancelled>;
    fn deadline(&self) -> Option<Instant>;       // inherited from tarpc
    fn get<T: Any>(&self) -> Option<&T>;          // typed ambient lookup (e.g. Participant)
}
```

- `request_id()` — the wire id, for correlation/logging.
- `cancelled()` / `throw_if_cancelled()` — a cancellation token tied to the call's `AbortSignal`
  (and to streaming cancel control frames).
- `get::<T>()` — typed lookup of ambient values, notably `Participant` (the attested caller).
  There is **no generic `Cx` type parameter**; identity is read dynamically here.

### Streaming (extension)
JSON-RPC is request/response only; linkrpc adds a streaming sub-protocol — one in-flight call may
carry a bidirectional, request-id-correlated channel of `$stream::send` notifications, plus
reserved cancel/ping/pong controls. **Direction is named from the caller's perspective**:

| attribute | direction | provider handle | caller |
|---|---|---|---|
| `#[incoming_stream(T)]` | server → client | `IncomingStream<T>` (source, `.send`) | `.next()` |
| `#[outgoing_stream(T)]` | client → server | `OutgoingStream<T>` (sink, `.recv`) | `.send()` |

```rust
// provider side (macro-injected handles):
struct IncomingStream<T> { /* … */ }            // server is the SOURCE → push
impl<T: Serialize> IncomingStream<T> { async fn send(&self, item: T); }

struct OutgoingStream<T> { /* … */ }            // server is the SINK → pull
impl<T: DeserializeOwned> OutgoingStream<T> { async fn recv(&mut self) -> Option<T>; }

// caller side: one handle, R = result, Out = client→server, In = server→client, E = error.
struct RpcCall<R, Out, In, E> { /* … */ }
impl<R, Out, In, E> RpcCall<R, Out, In, E> {
    async fn send(&self, item: Out) -> Result<(), E>;     // push to server
    async fn next(&mut self) -> Option<In>;               // pull from server
    async fn finish(self) -> Result<R, E>;                // close + final result
}
```

The caller drives one handle `RpcCall<R, Out, In, E>` (`.send`, `.next`, `.finish().await`).
A member with no stream attributes is a plain awaitable `Result<R, E>`.

### Signing / capabilities (optional, advanced)
An optional `$hubrpc` envelope wraps calls with an Ed25519 signature over a canonical
(JCS) signing input, plus capability tokens/attenuation. Calls without it are plain JSON-RPC
with raw params. This is the layer that upgrades *provenance* (transport-level) to
*cryptographically attested* identity and authority.

```rust
// Carried under params.$hubrpc — out of band from the user's params, never colliding.
struct CallMeta {
    method: String,             // fully-qualified wire method; verifier asserts equality
    nonce: String,              // replay-protection (base64url); gate dedups on it
    signed_at_ms: u64,          // skew window enforced by verifier
    signer: Option<NodeId>,     // present iff signed; == audience of every presented cap
    interface_hash: Option<String>, // optional schema-hash assertion
}
// + params.$hubrpcSignature (the signature) and params.$hubrpcUnsigned.capabilities (the cap bag).
```

A `SigningSender` decorator (wrapping the channel's `RequestSender`, using a `Principal`'s
`SigningIdentity`) produces the envelope on the way out; a verifier on the way in checks the
signature/nonce/skew and resolves the result into the `Participant` a handler sees via
`ctx.get::<Participant>()`.

#### The capability gate (receive side)

Authorization is a **receive-side `RequestHandler` decorator** — the mirror image of the
send-side `SigningSender`. It wraps the connection's handler and runs three stages before any
service method is touched:

```rust
struct CapGate<H> {
    inner: H,                                              // the LinkRpcConnection handler
    accepted_root_issuers: Box<dyn Fn(&ServiceId) -> Vec<AcceptedRootIssuer>>, // trust anchors
    replay: ReplayLedger,                                  // dedups nonces (single-use grants)
    max_skew: Duration,
}

impl<H: RequestHandler<()>> RequestHandler<()> for CapGate<H> {
    async fn handle_request(&self, call: IncomingCall<()>) -> CallResult {
        // 1. envelope: signature + skew, strip `$hubrpc*` from params.
        let v = match verify_call(&call.method, &call.params, now(), self.max_skew) {
            Ok(v) => v,
            Err(VerifyErr::Envelope(r)) => return CallResult::Err(RpcError::invalid_request(r)),
            Err(VerifyErr::Capability(r)) => return CallResult::Err(RpcError::permission_required(r)),
        };
        // 2. replay: a nonce may be spent at most once.
        if !self.replay.try_consume(&v.nonce) {
            return CallResult::Err(RpcError::invalid_request("replayed nonce"));
        }
        // 3. authorization: THE predicate. Trust is the verifier's (fail-closed).
        match permits(&v.call, &v.capabilities, &self.accepted_root_issuers, now()) {
            PermitResult::Ok { .. } => {
                // attach attested identity, forward stripped params to the real handler.
                let forwarded = call.with_params(v.stripped_params)
                                    .with_context_value(Participant::from(&v));
                self.inner.handle_request(forwarded).await
            }
            PermitResult::Err { reason } => CallResult::Err(RpcError::permission_required(reason)),
        }
    }
    fn handle_notification(&self, call: IncomingCall<()>) { /* same verify, no response */ }
}
```

`permits` is the **single, pure authorization predicate**: a call is allowed iff some presented
capability (1) addresses the call's target, (2) has a genuine, well-delegated chain whose **leaf
audience is the caller** (`call.signer`), and (3) **roots at an issuer accepted for the call's
serviceId**. Trust lives entirely in the `accepted_root_issuers(service_id)` callback — an empty
result rejects everything (fail-closed); tokens never confer their own trust. The gate owns the
replay ledger, so `callBind` (one-shot) grants are single-use for free.

```rust
fn permits(
    call: &Call,
    caps: &[SignedCapability],
    accepted_root_issuers: impl Fn(&ServiceId) -> Vec<AcceptedRootIssuer>,
    now: SystemTime,
) -> PermitResult;   // Ok { capability_nonce, root_issuer } | Err { reason }
```

This split — `verify_call` (authenticity) vs `permits` (authority) vs `ReplayLedger`
(freshness) — keeps each concern pure and testable, and lets the whole gate be omitted entirely
for the unsigned P1–P3 core.

---

## 3. The three scopes

The single most useful mental model: state lives at exactly three nested lifetimes.

```
serve_unix(path, |conn, prov| { … })         ── (1) REGISTRATION (per peer)
        │   runs once per accepted connection; the acceptor resolves `prov`
        │   (ConnectionProvenance) and hands it in. Decide what to expose to
        │   THIS peer; capture shared state via Arc.
        ▼
   LinkRpcConnection                          ── (2) LINK (per peer, symmetric)
        │   registry of served interfaces + reflection; mints Client proxies;
        │   lives as long as the transport is open.
        ▼
   Context  (one per handler call)           ── (3) REQUEST (per call)
            request_id, cancellation token, (later: attested Participant).
```

```rust
// The acceptor owns attestation: it resolves provenance from the raw socket
// (peer-cred / TLS / container) and passes it to your per-connection closure.
serve_unix("/tmp/pizza.sock", move |conn: &LinkRpcConnection, prov: &ConnectionProvenance| {
    // (1) per-peer wiring — provenance is known *before* any RPC:
    let shop = if prov.attributes.get("uid") == Some(&json!(0)) {
        admin_shop.clone()          // root gets the admin surface
    } else {
        public_shop.clone()         // everyone else gets the public one
    };

    conn.serve(shop.serve());                       // (2) mount on the link; id@hash inferred
    conn.enable_reflection(Default::default());
    // (3) Context — built per call by the connection — surfaces prov as a Participant:
    //     ctx.get::<Participant>() inside a handler.
})
.await?;
```

- **(1) Registration is a per-connection callback**, *not* a global registry/builder. A listener
  accepts sockets and invokes your closure once per connection (mirroring the TS
  `setConnectionHandler` + `withProvenance`). Shared services are `Arc`-captured; per-peer
  services can be built from the resolved `ConnectionProvenance`.
- **(2)** One `LinkRpcConnection` == one peer link. There is **no multi-accept "server" object** in
  the core — accepting is the listener's job; the core only models the per-peer connection.
- **(3)** `Context` is the only thing a handler needs to reason about *this* call.

---

## 4. Lifecycle of a single call

```
caller                         wire                         callee
──────                         ────                         ──────
client.order(ctx, …)
  └ marshal params
  └ choose method name  ──►  "com.acme.pizza::order"  ──►  Connection.route()
                                                            └ parse :: → (serviceId?, id@hash, member)
                                                            └ find handler in registry
                                                            └ validate params vs schema
                                                            └ build Context (id, cancel, Participant)
                                                            └ invoke handler.order(self, ctx, …)
  await response        ◄──   {result|error, id}      ◄──  └ encode typed Result<R, E>
  └ decode Result<R,E>
```

For streaming, the same `id` additionally correlates a flow of `$stream::send` frames in either
direction until the final response closes the call.

---

## 5. Why these choices (one line each)

- **JSON-RPC dialect, not a new protocol** — interoperate with existing JSON-RPC tooling; routing
  rides in the method name so no handshake is needed.
- **Content-hashed identity** — detect contract drift as an identity mismatch, not a runtime crash.
- **Reflection as ordinary interfaces** — discovery needs no special-casing; it's "just more RPC."
- **Per-connection callback (no builder)** — authorization and per-peer wiring happen where
  provenance is known, with plain Rust closures and `Arc`.
- **tarpc macro model (Option A)** — battle-tested ergonomics; provider trait and caller client may
  diverge because we don't require location transparency.
- **Interface inferred, serviceId explicit** — the contract is intrinsic to the handler; the mount
  point is a deployment decision.
- **Provenance is a separate trait, not on `MessageTransport`** — most transports are anonymous,
  attestation has a different source/lifecycle (resolved once by the acceptor), and keeping it
  orthogonal lets any transport be wrapped with it.
- **Transport is generic over payload; context is out-of-band** — the wire `Message` stays pure
  JSON-RPC, while in-process producers staple `Participant`/headers as context that never serializes.
- **Per-call config lives in `SendOpts`, not the message type** — payload and per-call knobs
  (signer override, headers, interface hash) are different concerns and stay separate.
- **Identity is layered (`SigningIdentity` → `Principal` → `Participant`)** — the signing primitive
  is abstract (in-memory key or HSM); what a handler sees is only the attested `Participant`.
- **Branded ids, not aliases** — `NodeId`/`ServiceId`/`Nonce` are newtypes so the compiler stops
  you mixing them up.
- **`Participant` is deferred via `ctx.get::<T>()`** — identity rides the typed extension map, so
  P1–P3 ship with no identity type and add it later without touching handler signatures.
- **Authorization = a pure `permits()` behind a receive-side gate** — authenticity (`verify_call`),
  freshness (`ReplayLedger`), and authority (`permits`) are separate, testable, and omittable.
- **Streaming/signing are additive layers** — the core is useful at P1–P3; P4/P5 bolt on without
  reshaping it.
