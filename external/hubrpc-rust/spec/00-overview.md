# hubrpc — Protocol Specification

**Status:** Draft. **Version:** v1.

hubrpc is a wire protocol for typed, multiplexed remote procedure calls over a single bidirectional message stream. It is a specialization of [JSON-RPC 2.0](https://www.jsonrpc.org/specification): every hubrpc message is a JSON-RPC 2.0 message, and hubrpc adds an addressing grammar, a schema-and-hash interface model, and optional identity and capability layers — all carried inside the JSON-RPC envelope.

> **Note.** This document specifies *the protocol* — the bytes exchanged at a connection boundary and the behavior a node must exhibit in response. It does not specify any implementation, library, or product. A *hub* (a routing intermediary) is a product built from this protocol; its routing engine is out of scope, though the interfaces a hub exposes (chapter 08) are specified.

## 1. Conventions

The keywords **MUST**, **MUST NOT**, **REQUIRED**, **SHALL**, **SHALL NOT**, **SHOULD**, **SHOULD NOT**, **RECOMMENDED**, **MAY**, and **OPTIONAL** are to be interpreted as in [RFC 2119](https://www.rfc-editor.org/rfc/rfc2119) and [RFC 8174](https://www.rfc-editor.org/rfc/rfc8174).

**Normative vs. explanatory text.** A line is *normative* iff it (a) defines a term, (b) states a requirement using an RFC 2119 keyword, or (c) specifies a concrete wire artifact (a grammar, a field, a byte sequence, a code). Everything else is *explanatory*. Explanatory text appears only in blockquotes tagged `Note`, `Rationale`, or `Example`, and **may be deleted in its entirety without changing what the specification requires.**

> **Rationale.** This rule keeps the normative surface auditable: strip every blockquote and the remaining text is the complete, self-contained contract.

Each term and each wire field is defined **exactly once**, in the chapter that gives it meaning. Later chapters reference that definition rather than restating it. The [glossary](glossary.md) is the single exception (it restates one short line per term as an index).

**External norms** are cited, not re-derived:

| Reference | Used for |
|---|---|
| [JSON-RPC 2.0](https://www.jsonrpc.org/specification) | message envelope, request/response correlation, base error codes |
| [RFC 8259](https://www.rfc-editor.org/rfc/rfc8259) | JSON text |
| [RFC 8785](https://www.rfc-editor.org/rfc/rfc8785) (JCS) | canonical JSON for hashing and signing |
| [RFC 5234](https://www.rfc-editor.org/rfc/rfc5234) (ABNF) | grammars |
| [RFC 3986](https://www.rfc-editor.org/rfc/rfc3986) | endpoint URIs |
| [RFC 4648](https://www.rfc-editor.org/rfc/rfc4648) §5 | base64url (no padding) |
| [FIPS 180-4](https://csrc.nist.gov/pubs/fips/180-4/upd1/final) | SHA-256 |
| [RFC 8032](https://www.rfc-editor.org/rfc/rfc8032) | Ed25519 signatures |

## 2. Layers and chapters

hubrpc is layered. Each chapter builds on the chapters before it. The **optional** chapters may be omitted by a conformant node, and a connection between nodes that disagree on them still works for the calls that need none of them.

| # | Chapter | Status | Defines |
|---|---|---|---|
| 01 | [Messages](01-messages.md) | required | JSON-RPC specialization, `::` method grammar, reserved `$hubrpc` namespace, ids, error codes |
| 02 | [Transport](02-transport.md) | required | abstract transport + NDJSON / WebSocket / stdio bindings + endpoint URIs |
| 03 | [Streaming](03-streaming.md) | optional | the `$stream` interface, in-flight correlated messages, control verbs |
| 04 | [Interfaces](04-interfaces.md) | required | interface schema format, JSON Schema subset, interface hash, `$hubrpc.interfaceHash`, validation |
| 05 | [Reflection](05-reflection.md) | required | `hubrpc.directory`, `hubrpc.schemas`, `hubrpc.defaults` |
| 06 | [Identity](06-identity.md) | optional | principals & keys, the signed-object standard, `$hubrpc` signing members, `$hubrpcSignature` |
| 07 | [Capabilities](07-capabilities.md) | optional | `SignedCapability`, `$hubrpcUnsigned.capabilities`, the authorization gate |
| 08 | [Hub interfaces](08-hub-interfaces.md) | optional | `hubAccess`, `hubGrantedServiceId`, `hubServiceIdRegistry` wire contracts |

> **Note.** Streaming (03) is optional to *implement* but is placed before interfaces (04) because an interface schema declares its members' stream payload types — the `$stream` mechanism must be defined before the schema fields that type it.

[conformance.md](conformance.md) defines the conformance profiles and the interop vector corpus. [identity-extension.md](identity-extension.md) is an optional extension to chapter 06 (key documents and rotation). [glossary.md](glossary.md) is a term index.

## 3. The additive-envelope invariant

Every optional layer is carried in **reserved members** that a node which does not implement the layer treats as opaque. Specifically:

- Optional metadata rides under reserved `$hubrpc`-prefixed keys on a call's `params` object (chapter 01 §3).
- A node that does not implement a layer MUST ignore that layer's reserved members when they are not required for the call it is processing, and MUST NOT let their presence change the outcome of an otherwise-ungated call.

> **Rationale.** This is what makes the layers composable. A Core node and a Capability node interoperate on any call the Capability node does not gate, because the extra `$hubrpcUnsigned`/`$hubrpc` members are invisible to the Core node and inert on an ungated path.

## 4. Conformance model

This specification constrains a single **node** — one endpoint of one connection. A connection is two nodes, each acting as the other's environment. The bidirectional, symmetric behavior of a connection *emerges* from composing two nodes; the specification never describes a connection as a whole.

A node reacts to two kinds of input: **inbound messages** (from the peer) and **local events** (the application decides to call, a stream produces an item, the connection opens). Its behavior is constrained by three kinds of rule:

1. **Reaction obligations** — *if a node receives an inbound message matching X, it MUST emit, within finite time, a response matching Y.* These have both a liveness part (it MUST respond) and a safety part (the response MUST be well-formed). They cover responses, and the control replies of chapters 05–07.
2. **Emission invariants** — *whenever a node emits any outbound message, that message MUST satisfy the framing, addressing, correlation, and (where applicable) signing rules.* These hold over *every* emission regardless of cause, and are what constrain node-initiated messages: the specification never dictates *when* a node initiates, only that whatever it emits is well-formed and correctly correlated.
3. **Ordering predicates** — a small number of rules range over the message history (a transport handshake comes first; a stream's `requestId` is not used before the stream is open; a signing nonce is not reused).

Obligations attach to **roles relative to a single call** — *caller* and *provider* — never to fixed client/server identities. The same node is the provider of one call and the caller of another at the same time; a rule stated for a role applies in both directions.

A node's served contract — which interfaces it provides and their schemas — is itself observable through reflection (chapter 05). A conformance checker therefore discovers a node's contract from the boundary, then verifies each obligation against it.

> **Note.** Two classes of obligation appear. *Protocol obligations* are fully specified here (a malformed request MUST yield `parseError`; an unknown method MUST yield `methodNotFound`). *Contract obligations* are specified in shape but delegated in content: a valid call to a member MUST yield either a result valid against that member's result schema or an enumerated error — hubrpc fixes the envelope of the reaction; the application interface supplies the schemas; hubrpc never specifies the application's value-level mapping. This is the precise sense in which the spec is complete without knowing any application.

## 5. Profiles

A node advertises one of three nested conformance **profiles** (full definitions in [conformance.md](conformance.md)):

- **Core** — chapters 01, 02, 04, 05 (and 03 if it streams).
- **Signed** — Core plus chapter 06.
- **Capability** — Signed plus chapter 07.

Two nodes interoperate at the greatest profile they share; calls that require no gated authority work across any pair.

## 6. Reading order

Read 01, 02, 04, 05 in order for the mandatory core, and 03 alongside 04 if you stream. Read 06 before 07, and 08 last (it depends on 05, 06, and 07). Every chapter opens with a one-paragraph normative summary of what it adds.
