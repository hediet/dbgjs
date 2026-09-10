# 03 — Streaming

**Optional.** This chapter defines in-flight, bidirectionally-correlated messages on a long-running request: progress and partial results from provider to caller, and input, cancellation, and keepalive from caller to provider. The whole layer is one reserved interface, `$stream`, whose single member carries every stream message; this chapter leads with that interface and then defines its semantics. A node that does not implement streaming simply never sends `$stream::send` and treats received ones per chapter 00 §3.

## 1. The `$stream` interface

```
$stream::send                       // a JSON-RPC NOTIFICATION (never answered)
  params: {
    requestId: number | string,     // the in-flight request this message belongs to
    dir:       "toCaller" | "toCallee",
    control?:  {
      type:    "cancel" | "ping" | "pong",
      reason?: string,               // open set; diagnostic
      nonce?:  string                // ping carries it; matching pong echoes it
    },
    payload?:  <typed per call>      // application stream payload
  }
```

`$stream::send` is a notification (no `id`, never answered). The interface is declarative: no node *registers* it and no node *calls* it as an ordinary member — the runtime intercepts the method `$stream::send` directly. Its definition exists so reflection and consent UIs can describe the wire shape and so all stream constants derive from one source.

## 2. Correlation and direction

A stream message is correlated to its originating request solely by `requestId`, which MUST equal the `id` of an in-flight request. A stream message MUST NOT reference a `requestId` that is not currently in flight (an ordering predicate, chapter 00 §4). The originating request's authorization (chapter 07), if any, covers the request's entire stream lifetime; stream messages carry no method or interface namespace of their own beyond `$stream`.

`dir` states the travel direction relative to that request, **named from the caller's perspective**:

- `toCaller` — provider → caller (progress, partial results); routes like the request's response.
- `toCallee` — caller → provider (input, cancellation, keepalive).

`dir` is explicit on the wire so an intermediary can *author* a stream message (e.g. inject a `cancel` when a caller disconnects) without an inbound message from whose arrival it could infer direction.

## 3. Application payload

`payload` is opaque to this layer; its type is fixed per call by the originating member's `clientStream` (for `toCallee` payloads) and `serverStream` (for `toCaller` payloads) schemas (chapter 04 §2). A member that declares neither MUST NOT carry app `payload` stream messages; control messages (§4) are always permitted regardless of schema.

A stream message SHOULD carry either a `control` or a `payload`, not both: a `control` message is interpreted by the runtime, not the application.

## 4. Control verbs

`control.type` is one of three reserved verbs, interpreted by the runtime independently of any application schema (so *any* request is cancellable and keepalive-able):

- **`cancel`** — `toCallee` only. Asks the provider to abort the in-flight request. A provider that honors a `cancel` SHOULD settle the request, typically with a `cancelled` error response (chapter 01 §4).
- **`ping`** — either direction. A liveness probe carrying a fresh `nonce`. It resets the idle timer (§5) and requests a `pong`.
- **`pong`** — the reply to a `ping`, echoing the ping's `nonce`, traveling opposite the ping. It carries no application effect.

`control.reason` is an open set of human/diagnostic strings; any string is valid. The reasons hubrpc itself emits include `clientDisconnected` and `idleTimeout`.

A node that receives a `ping` SHOULD emit a `pong` echoing its `nonce` (a reaction obligation). A node MUST NOT treat an unrecognized `control.reason` as an error.

## 5. Idle timeout

An intermediary MAY enforce a per-request idle timeout: a request that produces no stream activity (no `control` ping, no stream message) for the timeout window MAY be cancelled, in which case its caller receives a `requestTimeout` error (chapter 01 §4). A node that wishes to keep a long-running call alive SHOULD emit periodic `ping` control messages.

> **Rationale.** The idle timeout bounds an intermediary's pending-request table and is its defense against a slow-loris that opens requests and never completes them. Streaming-enabled calls keep themselves alive by pinging.
