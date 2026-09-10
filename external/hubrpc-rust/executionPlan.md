# hubrpc-rust — Execution Plan

> **Companion to** `docs/architecture.md` (theory), `docs/examples.md` (target DX),
> `docs/hub.md` (hub layer). Those describe *what* we are building; this file describes the
> *order* we build it in, the *acceptance test* for each step, and how we keep the Rust impl
> **wire-compatible with the existing TypeScript `@vscode/hubrpc`**.

---

## 0. North-star scenario (the thing that must work end to end)

> A Rust process joins a hub from an env var, serves a service, and a VS Code (TypeScript) app
> can discover and — once an admin grants it a capability — call that service. An AI agent can
> browse the hub for services and their schemas.

Concretely:

1. **Rust connector.** A Rust binary reads `HUBRPC_ENDPOINT` + `HUBRPC_TOKEN` and connects over a
   **Unix-domain socket with NDJSON framing** (newline-delimited JSON-RPC, `hello` preamble). It
   claims a serviceId namespace and serves `com.acme.pizza` (the running example).
2. **TS consumer (VS Code).** A TypeScript app connects to the *same* hub, walks the directory,
   finds `…::com.acme.pizza`, and tries to `order`.
3. **Consent.** The call is **denied** (`permissionRequired`) until an **admin** grants the
   consumer a capability via `hubAccess`. After the grant, the same call **succeeds**.
4. **AI exploration.** An agent enumerates the hub: every reachable service, its `id@hash`, and
   its JSON Schema — purely from the reflection/directory surface.

This scenario forces three hard requirements that shape the whole plan:

- **R1 — Interop is a first-class, continuously-verified property.** Every wire-visible layer
  (JCS bytes, interface hash, JSON-RPC framing, `$hubrpc` signed envelope, capability chain,
  `hubAccess`/directory interfaces) must match TS **byte-for-byte / decision-for-decision**.
- **R2 — Identity ships in v1 but is optional.** The signing/capability machinery must *exist*
  from the first release (anonymous connections work; signed connections + capability gating
  work when configured). It is not a "phase 5, maybe later" bolt-on.
- **R3 — Reflection must be machine-friendly.** Directory + schema discovery is an explicit
  product surface (for AI), not an afterthought.

---

## 1. Interop strategy (cross-cutting — set up FIRST, run forever)

Interop is the single biggest risk. We treat it as a **conformance corpus**, not an end-of-project
hope.

```
conformance/
  generate/                 # tiny Node script importing @vscode/hubrpc to emit vectors
  vectors/                  # committed JSON fixtures (the source of truth = TS)
    method_name.json        # parse/format round-trips
    jcs.json                # value → canonical UTF-8 bytes (hex)
    interface_hash.json     # interface schema → "id@hash"  (incl. pizza)
    framing.json            # sample request/response/notification/stream/error frames
    endpoint.json           # HUBRPC_ENDPOINT URI → ResolvedEndpoint (parse + token rules)
    signed_envelope.json    # key + payload → $hubrpc envelope; + verify pass/fail cases
    capability.json         # minted capabilities + chain-validation expected verdicts
```

- A small **TS generator** (committed under `conformance/generate/`) imports the real TS impl and
  writes `vectors/*.json`. Regenerated whenever the TS impl changes; the diff is reviewable.
- Every Rust module that touches the wire has a test that **loads the vectors and must match**.
- **CI runs the Rust conformance tests on every commit.** A red conformance test blocks merge.
- Two-way live interop tests where feasible: spawn the TS reference (hub / echo service) as a
  child process and run the Rust client against it (and vice versa).

**Hash parity is the canary** (M2). If `schemars`→subset output can't be normalized to equal
zod's `toJSONSchema` output, a Rust server and TS client silently disagree on `id@hash`. We find
that out in M2, not M7.

---

## 2. Milestones

Each milestone lists **deliverable**, **acceptance** (how we know it's done), and **interop gate**
(the conformance bar it must clear). Phases `P1–P6` reference `plan.md`.

### M0 — Workspace + interop harness
- **Deliverable.** Cargo workspace (`crates/hubrpc`, `crates/hubrpc-macros`, `crates/hubrpc-tokio`,
  `examples/`), CI (build + clippy + test), and the `conformance/` harness from §1 with the TS
  generator wired up and producing vectors.
- **Acceptance.** `cargo test` green on an empty corpus loader; `npm run gen` (in `conformance/`)
  produces `vectors/*.json`; CI runs both.
- **Interop gate.** Harness exists and is enforced by CI (even if no vectors assert yet).

### M1 — Wire core (P1)
- **Deliverable.** `JsonValue` (= `serde_json::Value`), JSON-RPC 2.0 types + error codes,
  the `::` method-name grammar (parse/format), JCS canonicalization, `MessageTransport` trait +
  in-memory `TransportPair`, `Channel` (request/response correlation + inbound dispatch).
- **Acceptance.** Round-trip a request/response/notification over the in-memory pair.
- **Interop gate.** `method_name.json`, `jcs.json`, `framing.json` vectors all pass byte-for-byte.

### M2 — Schema & hashing (P2)  ⟵ **interop canary**
- **Deliverable.** JSON-Schema subset model, `schemars`→subset conversion, normalization, and
  `compute_interface_hash` (SHA-256 → 16 hex), producing `id@hash`.
- **Acceptance.** Rust hashes are internally stable across runs/ordering.
- **Interop gate.** `interface_hash.json` passes: **Rust `id@hash` == TS `id@hash`** for the whole
  corpus, *including the pizza service*. If full parity is too costly, decision point here:
  (a) invest until parity, or (b) ship a documented normalization profile + conformance suite and
  accept a known gap. **Do not proceed to M6/M7 without resolving this** — the demo needs a Rust
  server and TS client to agree on `id@hash`.

### M3 — Connection + reflection + macro (P3)
- **Deliverable.** `InterfaceDefinition`, `HubRpcConnection` (register / get / dispatch / validate /
  preset), the three reflection interfaces (`hubrpc.directory`, `hubrpc.schemas`,
  `hubrpc.defaults`), and the `#[hub_rpc_interface]` proc-macro (emits the rewritten trait, the
  `…Client` struct, `serve()`, and `interface()`), per `docs/examples.md`.
- **Acceptance.** The pizza example from `docs/examples.md` runs: Rust provider ↔ Rust caller over
  the in-memory pair **and** over a Unix socket (`hubrpc-tokio`).
- **Interop gate.** **Cross-impl, unsigned:** a Rust caller invokes a TS-served service and a TS
  caller invokes a Rust-served service over a socket; reflection (`hubrpc.directory::list`,
  `hubrpc.schemas::get`) returns identical shapes both directions.

### M4 — Identity core (R2: present in v1, optional)
- **Deliverable.** `SigningIdentity` abstraction (Ed25519 in-memory now; pluggable HSM later),
  branded `NodeId` derived from the public key, the `$hubrpc` **signed envelope**
  (canonical signing input, sign/verify, clock-skew window), and the `Principal` / capability data
  model. All **optional at the connection level** — anonymous connections work unchanged; the
  verify path is only engaged when a signature is present or required.
- **Acceptance.** Rust signs an envelope that Rust verifies; toggling identity on a connection is a
  config flag, and an unsigned call still flows when identity is off.
- **Interop gate.** `signed_envelope.json` passes **both directions**: Rust verifies a TS-produced
  envelope, and TS verifies a Rust-produced one (same `NodeId` derivation, same canonical input).

### M5 — Capability gate + consent (`hubAccess`)
- **Deliverable.** The receive-side **`CapGate`** decorator (`verify_call` → replay ledger →
  pure `permits()` predicate with fail-closed `accepted_root_issuers`), `SignedCapability`
  mint/verify + attenuation/chain validation, and the `hubAccess` interface
  (`request` / `extend` / `requestAccess`) returning capabilities with `audience = consumer NodeId`.
  Per-method gating driven by interface annotations (e.g. `#[annotations(dangerous)]`).
- **Acceptance.** A gated method is denied (`permissionRequired`) without a capability and allowed
  with a valid one; replayed nonces are rejected.
- **Interop gate.** `capability.json` passes both directions: a **TS-minted** capability is accepted
  by the **Rust** gate, and a **Rust-minted** capability is accepted by the **TS** gate; chain /
  audience / root-issuer verdicts match the corpus exactly.

### M6 — Socket transport + hub client (the connector)
- **Deliverable.** The **endpoint layer** (`parse_endpoint_uri` + `HUBRPC_ENDPOINT`/`HUBRPC_TOKEN`
  resolution — see Appendix A for the exact contract), a **Unix-domain-socket transport with NDJSON
  framing + `hello` preamble** (the north-star transport), `connect_to_hub`, and a thin **hub client
  facade** (`get_connection_info` → `hubGrantedServiceId::get`, `register_service_id_namespace` →
  `hubGrantedServiceId::register`, `walk_hub_detailed` directory flattening). The **env-var
  connector**: a `hubrpc-connect` entrypoint reading `HUBRPC_ENDPOINT` + `HUBRPC_TOKEN`.
  WebSocket / named-pipe transports are deferred (not on the north-star path).
- **Acceptance.** `HUBRPC_ENDPOINT=… HUBRPC_TOKEN=… hubrpc-connect` connects to the **existing TS
  standalone hub**, claims a namespace, serves pizza, and the Rust process appears in the hub's
  directory.
- **Interop gate.** `endpoint.json` parse parity passes; **live**: Rust connector against the
  **unmodified TS hub** — handshake, token auth, namespace claim, and directory listing all
  succeed.

### M7 — End-to-end north-star scenario
- **Deliverable.** Wire §0 together as an automated scenario test: Rust pizza provider on a hub; a
  TS consumer discovers it, is denied, an admin grants via `hubAccess`, the call then succeeds; an
  AI-style directory walk enumerates services + `id@hash` + schemas.
- **Acceptance.** The scenario runs green in CI (TS hub + TS consumer + Rust provider as child
  processes), covering deny→grant→allow and full directory/schema enumeration.
- **Interop gate.** This *is* the integration gate — the whole stack interoperating with TS.

### M8 — Rust hub server (P6, optional / parallelizable)
- **Deliverable.** `crates/hubrpc-hub`: `Hub` longest-prefix router, `OverlaySplitter`,
  `RootOverlay`, `HubConnectionAcceptor`, prefix policy, forwarded-call gate, root services
  (`hubGrantedServiceId` / `hubParticipant` / `hubAccess` / `identity`), directory referral-walk,
  and a standalone WS hub CLI (signed-by-default, token auth). See `docs/hub.md`.
- **Acceptance.** A Rust consumer and a TS consumer both work against the **Rust** hub; the M7
  scenario passes with the Rust hub swapped in for the TS hub.
- **Interop gate.** TS client ↔ Rust hub ↔ Rust client, including consent + capability gating.
- **Note.** *Not required for the M7 demo* (we can lean on the existing TS hub). Schedule only if a
  Rust-hosted hub is itself a goal.

---

## 3. Dependency order

```
M0 ─▶ M1 ─▶ M2 ─▶ M3 ─▶ M6 ─▶ M7
                   │      ▲
                   ▼      │
            M4 ─▶ M5 ─────┘        (M4/M5 can start once M1's envelope plumbing lands;
                                    they must land before M6 to enable consent in M7)
            M8 (optional, needs M1–M5; parallel to M6/M7)
```

- **Critical path to the demo:** M0 → M1 → M2 → M3 → M4 → M5 → M6 → M7.
- **M2 is the gating risk.** Resolve hash parity before investing in M6/M7.
- **M4+M5 (identity/consent)** are on the critical path *because the demo's whole point is the
  admin-grant step* — this is the concrete expression of R2.

---

## 4. Key design commitments carried in (from the docs)

- **Macro model = tarpc conventions, hubrpc codegen.** Bare trait spec → macro rewrites to
  `self`+`Context`, emits `…Client` / `serve()` / `interface()` (hubrpc wire, not tarpc's).
- **Identity optional but native** (R2): `MessageTransport` (bytes) ⟂ `Attested` (provenance) ⟂
  `SigningIdentity` (keys); `CapGate` is a receive-side `RequestHandler` decorator, installed only
  when gating is on.
- **`ForwardCheckingPolicy` fails closed by type** — capability mode can't compile without trust
  anchors.
- **Reflection is the discovery API** (R3): `hubrpc.directory` (referral-walkable),
  `hubrpc.schemas`, `hubrpc.defaults` — the AI exploration surface.
- **Streaming** (P4) is *not* on the north-star critical path (pizza `order` is unary). Slot it
  after M3 if/when a streaming method is demoed; otherwise defer.

---

## 5. Risks & open questions

| # | Risk / question | Mitigation / where resolved |
|---|---|---|
| 1 | **Hash parity** Rust `schemars` vs TS `zod.toJSONSchema`. | M2 interop gate; decide invest-vs-document-gap there. |
| 2 | **Signed-envelope canonicalization** differs subtly (field order, NodeId derivation, skew). | M4 two-way `signed_envelope.json` vectors. |
| 3 | **Capability semantics** (attenuation, audience, root-issuer fail-closed) drift from TS. | M5 two-way `capability.json` vectors. |
| 4 | **TS hub handshake/token** details (WS subprotocol, auth framing). | M6 live test against the real TS hub; read `connectToHubWs`. |
| 5 | **Streaming interop** if a streamed method enters the demo. | Add a streaming milestone before that demo; vector its frames. |
| 6 | Async runtime lock-in. | Core stays runtime-agnostic; `tokio` confined to `hubrpc-tokio`. |

**Open questions for you:**
1. **Hub ownership** — is a *Rust* hub (M8) a goal, or is interop with the existing TS hub enough
   for now?
2. **Crypto suite** — Ed25519 to match TS exactly (confirm the TS curve/sig scheme before M4)?
3. **Capability minting** — does the Rust side ever need to *mint/admin* capabilities, or only
   *present/verify* them (consumer + provider roles, admin stays TS)?
4. **AI surface shape** — is programmatic directory/schema enumeration enough, or do you want a CLI
   (`hubrpc ls` / `hubrpc describe`) and/or an MCP server fronting the hub?

---

## Appendix A — The endpoint contract (what the Rust connector must reproduce)

Distilled from the TS source so the Rust connector reads/parses identically:
- `hubrpc/src/connection/endpointUri.ts` — `parseEndpointUri` / `formatEndpointUri`.
- `hubrpc/src/node/hubClient.ts` — `HUBRPC_ENDPOINT_VAR` / `HUBRPC_TOKEN_VAR`, `openHubChannel`,
  transport selection + preamble.
- `hubrpc-cli/src/endpoint.ts` — token-precedence rules.

### Environment variables
- **`HUBRPC_ENDPOINT`** — required. If unset, fail with
  *"HUBRPC_ENDPOINT is not set; cannot connect to hubrpc hub."*
- **`HUBRPC_TOKEN`** — optional; **defaults to `""`** (empty string) when connecting. Empty token
  is valid against provenance-authenticated (hubv2) hubs, which ignore it.

### Endpoint URI grammar (`parse_endpoint_uri`)
Trim first; empty → error. Try `URL` parse. **If it does NOT parse as a URL (bare string):**
- matches `^wss?://` (case-insensitive) → `Ws { url }`
- otherwise → `Socket { path }` (legacy bare `HUBRPC_ENDPOINT`).

**If it parses as a URL, switch on scheme:**

| scheme | → | notes |
|---|---|---|
| `ws:` / `wss:` | `Ws { url, token? }` | read `?token=`, then **delete it from the URL** (token never travels in the URL); `url` = cleaned string |
| `unix:` | `Socket { path, token? }` | `path = decodeURIComponent(pathname)`; `?token=` |
| `npipe:` | `Socket { path, token? }` | `npipe://host/tail` → `\\<host>\<tail>` (host defaults `.`; `decodeURIComponent`, `/`→`\`); `?token=` |
| `cmd-stdio:` | `CmdStdio { command, env? }` | spawn child, talk over its stdio |
| `cmd:` | `CmdEnv { command, provisionSlot?, env? }` | start a local hub, inject `HUBRPC_ENDPOINT`/`HUBRPC_TOKEN` into the child |
| anything else | **error** | *"unsupported endpoint scheme …"* |

Command payload: repeated `?argv=` params → `{ argv }` (structure-preserving), else `?command=` →
`{ command }` (one string, shell-split), else error. `?env=KEY=VALUE` (repeatable, first `=`
splits). For the **connector** (a participant that dials a hub), only `ws`/`wss`/`unix`/`npipe`
matter — `isHubEndpoint()` is true only for `socket` and `ws`. The `cmd*` schemes are *spawn*
modes, out of scope for v1's "connect to an existing hub."

### Token precedence (env-var path — what `hubrpc-connect` uses)
`--endpoint-token (if any)` ?? `token from the URI` ?? `HUBRPC_TOKEN` ?? `""`.
(Explicit `--endpoint <uri>` path differs: it never falls back to `HUBRPC_TOKEN`.)

### Transport selection + handshake (`open_hub_channel`)
- **`ws://` / `wss://`** → WebSocket; token rides in the **`Authorization: Bearer <token>`**
  header; **no preamble line**. Each WS message is one JSON-RPC frame.
- **anything else (UDS / named pipe)** → open socket, then **write one newline-terminated preamble
  line** `{"hello":1,"token":"<token>"}\n` **before any RPC**, then **NDJSON** framing
  (one JSON-RPC message per `\n`-delimited line). An immediate socket close = auth failure.

### After connecting (the connector's job)
1. Wrap the transport in a `HubRpcConnection`.
2. Claim a prefix: `hubGrantedServiceId::register` (capability-free, within the granted namespace)
   or `hub::hubParticipant::registerServiceId` (signed/admin-gated, outside it).
3. Serve the interface(s) under the claimed prefix; become discoverable via `hubrpc.directory`.

> **Conformance:** `endpoint.json` vectors cover every row above (incl. token-strip-from-ws-url,
> npipe backslashing, decode of `unix:` path, bare-string auto-detect, and the precedence chain).
> A live socket/ws round-trip against the TS hub validates the preamble + `Authorization` header.
