# 05 — Reflection

This chapter defines the built-in reflection interfaces every Core node serves: `hubrpc.directory` (what services exist), `hubrpc.schemas` (their interface schemas), and `hubrpc.defaults` (the connection's preset service/interface). Reflection makes a node's served contract observable from the connection boundary — the property the conformance model (chapter 00 §4) relies on. These interfaces are ordinary interfaces (chapter 04); they are specified here by their member schemas.

## 1. `hubrpc.directory`

Lists the services reachable through this node. A directory entry MAY itself point at another directory service, so a node's surface can be explored transitively.

```
hubrpc.directory::list
  params: {
    interfaceId?: string,   // filter: only services implementing this interface
    serviceId?:   string,   // filter: only this service id
    cursor?:      string,   // opaque continuation token from a prior page
    limit?:       number,   // max items this page; provider MAY return fewer
    timeoutMs?:   number     // soft cap on gathering time
  }
  result: {
    items: ServiceListing[],
    nextCursor?: string,    // omitted ⇒ no more pages
    truncated?:  boolean    // true if timeoutMs cut the page short
  }

ServiceListing = {
  serviceId:          string,
  interfaceId:        string,
  interfaceHash:      string,          // hash as implemented by this service (04 §4)
  serviceDescription?: string,         // non-normative
  rootPrincipalSets?: RootPrincipalReq[][] // access requirement in CNF (see below)
}

RootPrincipalReq = { principal: string, transitive?: boolean }
```

A provider MUST list, for each `(serviceId, interfaceId)` pair it exposes, its implemented `interfaceHash`. Paging is at the provider's discretion: it MAY return fewer than `limit` items and MUST set `nextCursor` when more pages remain.

`rootPrincipalSets`, when present, states an access requirement in conjunctive normal form: the caller must satisfy **every** set (AND), and a set is satisfied by holding **any one** of its `principal`s (OR). `transitive: true` propagates the requirement to every service reachable *through* this entry. An omitted or empty `rootPrincipalSets` means no root-principal requirement.

> **Note.** `rootPrincipalSets` advertises *who* may reach a service; it is descriptive metadata for explorers and consent UIs. Enforcement is the capability layer's job (chapter 07) and the hub's (chapter 08); the directory itself only reports the requirement.

### 1.1 `hubrpc.directory::watch`

A coarse change tap on the directory. `watch` is a long-lived streaming request (chapter 03) that emits an **empty tick** whenever the (optionally filtered) directory *might* have changed.

```
hubrpc.directory::watch
  params: {
    interfaceId?: string,   // relevance hint: changes touching this interface
    serviceId?:   string    // relevance hint: changes touching this service
  }
  result: {}                              // resolves when the caller cancels
  serverStream: {}                        // empty tick: "the directory may have changed; re-list"
```

A tick carries no delta and no payload — its only meaning is "re-`list` now"; the consumer reconciles against its own last snapshot. This keeps the provider stateless: it never computes per-item deltas and never replays an initial sync. Over-emission is allowed (the consumer re-`list`s and finds nothing new); under-emission is not. Ticks MAY be coalesced. The `interfaceId` / `serviceId` filters mirror `list` and are a relevance hint, not a guarantee. The request resolves when the caller cancels (or the connection drops).

> **Note.** A static directory that never changes MAY open the stream, never tick, and resolve on cancellation — semantically correct because its listing never changes.

## 2. `hubrpc.schemas`

Serves interface schemas by id (and optional hash). A node MUST serve, through this interface, the schema of every interface it advertises in its directory.

```
hubrpc.schemas::get
  params: {
    interfaceId: string,
    hash?:       string     // omit ⇒ provider returns the version it exposes
  }
  result: { schema: InterfaceSchema }   // a full interface schema (04 §2)
```

When `hash` is supplied, the provider MUST return a schema whose interface hash equals it, or respond with `invalidParams` if it serves no such version.

## 3. `hubrpc.defaults`

Reports the preset (default) service/interface bound to this connection, if any — the binding that gives meaning to the bare and interface method forms (chapter 01 §2).

```
hubrpc.defaults::get
  params: {}
  result: {
    serviceId?:     string,
    interfaceId?:   string,
    interfaceHash?: string
  }
```

A node MAY leave any field absent when it presets no default at that level.

> **Rationale.** A caller that connects to a single-purpose endpoint can discover, in one call, which service/interface its bare-form calls dispatch to — without hard-coding the binding.
