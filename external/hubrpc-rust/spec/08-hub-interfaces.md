# 08 — Hub interfaces

**Optional. Requires chapters 05, 06, 07.** A **hub** is a node that routes calls between many connected participants and brokers access between them. Its routing engine is a product and is out of scope. What this chapter specifies is the **wire contract** of the interfaces a hub exposes so that participants and consumers — in any language — interoperate with it: `hubGrantedServiceId` (connection facts and namespace claims), `hubServiceIdRegistry` (admin-gated prefix claims), and `hubAccess` (a consumer's request for scoped access). These are ordinary interfaces (chapter 04), specified here by member schema.

> **Note.** A hub forwards a call by following the request's `id` path for the response and the `requestId` path for stream messages (chapter 03), and rewrites those ids across the forwarding boundary. *Which* participant a call reaches, and the routing tables behind it, are product concerns and unspecified.

## 1. `hubGrantedServiceId`

Served at each participant's connection root (reachable directly, never forwarded, needing no capability of its own). Reports the topology the hub decided for this connection and claims service-id prefixes *within* the granted namespace.

```
hubGrantedServiceId::get                         // unsigned
  params: {}
  result: { grantedServiceIdNamespace: string }  // region this connection may claim; "" ⇒ nothing freely

hubGrantedServiceId::getHubServiceId             // unsigned
  params: {}
  result: { hubServiceId: string }               // prefix the hub mounts its own global services under

hubGrantedServiceId::register
  params: { serviceId: string }                  // MUST equal or nest under the granted namespace
  result: {}
```

`get` reports `grantedServiceIdNamespace`, the absolute service-id region this connection's provenance may claim (anything at or under it; `""` ⇒ claim nothing freely). `getHubServiceId` reports `hubServiceId`, the prefix the hub mounts its own global services under (default `hub`) — the prefix used to address the admin-gated `hubServiceIdRegistry` and the hub's reflection endpoints. Both are served at the connection root (unsigned), so a participant can discover them before it knows where the hub lives.

`register` claims a prefix within `grantedServiceIdNamespace` and needs no capability — the grant happened out of band at attach time. A `serviceId` that is neither equal to nor nested beneath the granted namespace MUST be rejected (claim it through `<hubServiceId>::hubServiceIdRegistry::registerServiceId` instead).

## 2. `hubServiceIdRegistry`

Claims a service-id prefix **outside** the connection's granted namespace. Reached in fully-qualified form `<hubServiceId>::hubServiceIdRegistry::registerServiceId` and gated (chapter 07) by an admin-rooted capability.

```
hubServiceIdRegistry::registerServiceId
  params: { requestedPrefix: string }            // min length 1
  result: {}
```

## 3. `hubAccess`

A consumer (e.g. a sandboxed editor) asks the hub for scoped access to services. Served at the connection root, so it needs no bootstrap capability. On grant, the hub returns `SignedCapability`s (chapter 07) the consumer attaches to subsequent calls via `$hubrpcUnsigned.capabilities`.

```
hubAccess::request
  params: {
    consumer: Consumer,
    dependencies: { [slot: string]: {
      interfaces: { id: string, hash?: string, required?: boolean }[],   // required default true
      members?:   { interfaceId: string, member: Pattern, required?: boolean }[]
    } },
    duration?: Duration
  }
  result:
    | { status: "granted",
        slots: { [slot: string]: { serviceId: string, satisfiedInterfaces: string[] } },
        capabilities: SignedCapability[] }       // audience = consumer principal; MAY be empty
    | { status: "denied", reason?: string }
    | { status: "noCandidates", slots: string[] }

hubAccess::extend                                 // widen an existing grant on a known service
  params: {
    consumer: Consumer,
    serviceId: string,                            // a service the consumer already deals with
    added: { interfaceId: string, member: Pattern, required?: boolean }[],
    duration?: Duration
  }
  result:
    | { status: "granted", serviceId: string,
        granted: { interfaceId: string, member: Pattern }[],
        capabilities?: SignedCapability[] }       // covers only the granted delta
    | { status: "denied", reason?: string }

hubAccess::requestAccess                          // verbatim capability request (no discovery)
  params: {
    consumer: Consumer,
    permissions: RequestPermission[],             // min length 1
    duration?: Duration
  }
  result:
    | { status: "granted", capabilities: SignedCapability[] }
    | { status: "denied", reason?: string }
```

```
Consumer = { name: string, principal: PrincipalId, origin?: string, purpose?: string }
Duration = "once" | "shortLived" | "longLived" | "persistent"

RequestPermission = Permission & { callIntent?: CallIntent }   // Permission per chapter 07 §1.1

CallIntent = {                 // consent-only preview; the hub strips it before minting the cap
  method:        string,
  params?:       <value>,
  interfaceHash?: string,
  nonce:         string,       // the consumer MUST sign with this nonce + signedAtMs
  signedAtMs:    number,
  summary?:      string,
  suggestion?:   Duration
}
```

The capability's `audience` is the consumer's own `principal` (a cap minted for a principal is usable only by the holder of that identity's key — enforced at call time by the gate, chapter 07 §4). `request` performs directory-based discovery and pins each slot to one chosen service; `requestAccess` is verbatim and grants exactly the attenuations asked for (including wildcards such as `serviceId: { prefix: "" }`). When a `requestAccess` permission carries a `callIntent`, the hub MAY bind the resulting capability to that exact call (chapter 07 `callBind`) so the consumer's later signed call matches byte-for-byte.

> **Note.** In v1 a hub MAY hold no signing identity, in which case `capabilities` is an empty array and dispatch is not gated on the grant. Once the hub holds an identity it returns real capabilities and the gate (chapter 07) enforces them.
