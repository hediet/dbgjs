# 01 — Messages

This chapter defines the hubrpc message envelope: a specialization of [JSON-RPC 2.0](https://www.jsonrpc.org/specification). It adds the `::` method-addressing grammar, the reserved `$hubrpc`-prefixed `params` namespace that carries every optional layer, the set of error codes, and the request/response correlation rule. It defines the *names* of the reserved members and the rule for stripping them before canonicalization; each member's *meaning* is defined in the chapter that owns its layer.

## 1. Relationship to JSON-RPC 2.0

hubrpc **is** JSON-RPC 2.0: every hubrpc message is a well-formed JSON-RPC 2.0 message ([RFC 8259](https://www.rfc-editor.org/rfc/rfc8259) JSON text, `"jsonrpc": "2.0"`), and everything JSON-RPC 2.0 specifies — the `jsonrpc` tag, the four message kinds, the `id` matching rule, the base error object — applies unchanged and is not restated here. hubrpc is a **specialization**: it narrows JSON-RPC's open choices rather than altering them.

hubrpc differs from plain JSON-RPC 2.0 in exactly the following ways, and in no others:

**Extensions** (hubrpc gives meaning to things JSON-RPC leaves open):

1. The `method` string is given structure: a one-to-three-segment `::` address (§2).
2. The `params` object gains a reserved `$hubrpc`-prefixed namespace carrying every optional layer's metadata (§3). User params are `params` minus those keys.
3. Additional error codes are defined in the JSON-RPC reserved range (§4).

**Constraints** (hubrpc narrows what JSON-RPC permits):

1. `method` MUST conform to the §2 grammar.
2. Every request and notification MUST carry `params`, and `params` MUST be a JSON object. JSON-RPC's omitted `params`, by-position (array) `params`, and primitive `params` are not supported; a node MUST reject such a request with `invalidParams`.
3. Batch arrays are not used; a node MAY reject a JSON-RPC batch with `invalidRequest`.

Everything else — message framing into the four kinds, id correlation, error semantics — is JSON-RPC 2.0 verbatim.

## 2. Method names

A `method` is one to three non-empty segments joined by the two-character delimiter `::`.

```abnf
method      = full / interface / bare
full        = service-id "::" interface-id "::" member
interface   = interface-id "::" member
bare        = member
service-id  = segment
interface-id= segment
member      = segment
segment     = 1*char        ; one or more characters; a segment MUST NOT contain "::"
```

A single `:` is legal inside a `segment`; only the two-character sequence `::` delimits. Parsing rule: split `method` on the literal `"::"`. The result MUST have one, two, or three parts and **no part may be empty**; otherwise the method is malformed.

- **1 part** — `bare`: a `member` only.
- **2 parts** — `interface`: `interface-id :: member`. Addresses the **root service** (service id = the empty string).
- **3 parts** — `full`: `service-id :: interface-id :: member`.

A node that receives a syntactically malformed method (empty segment, or more than three parts) MUST respond with `methodNotFound` (for a request) or ignore it (for a notification).

> **Note.** A segment may itself contain characters meaningful to higher layers (a `service-id` is a `/`-delimited path, an `interface-id` is a `.`-delimited path) — but at this layer `::` is the only structural delimiter. The bare form has no interface context; an intermediary that routes on interface (a hub) does not accept it. See chapter 04 for the addressing model and chapter 08 for hub routing surfaces.

## 3. The reserved `$hubrpc` namespace

On a call's `params` object, member keys beginning with `$hubrpc` are **reserved by hubrpc**; no other keys are reserved. Three are defined:

| Key | Carries | Defined in |
|---|---|---|
| `$hubrpc` | signed call metadata (and the interface-hash assertion) | §3.1 below; members in 04 §4, 06 §3 |
| `$hubrpcSignature` | the per-domain signature map | 06 §2 |
| `$hubrpcUnsigned` | extrinsic unsigned attachments (the capability bag) | 07 §2 |

**User params.** The *user params* of a call are its `params` object with every `$hubrpc`-prefixed key removed. Application interface schemas (chapter 04) describe the user params only.

**Strip rule.** Wherever this specification canonicalizes a call for hashing or signing, it canonicalizes the call object with `$hubrpcSignature` and `$hubrpcUnsigned` removed and `$hubrpc` retained. `$hubrpcSignature` cannot cover itself; `$hubrpcUnsigned` is authored by a party other than the signer. (The exact signing input is defined in 06 §3.)

> **Note.** `$hubrpc` lives inside `params` rather than as a sibling of `params` on the JSON-RPC object because JSON-RPC 2.0 fixes the top-level member set (`jsonrpc`, `id`, `method`, `params`); carrying metadata inside `params` keeps the message a valid JSON-RPC message and lets it travel with the call's arguments through a forwarder that treats `params` as opaque. The `$hubrpc` prefix — not the placement — is what reserves these keys from colliding with user param keys.

### 3.1 `$hubrpc` presence

`$hubrpc` is present on a call whenever any layer attaches metadata to it (an interface-hash assertion, a signature, or both). When present it MUST be a JSON object. Its individual members are defined where they gain meaning — `interfaceHash` in chapter 04, the signing members (`method`, `nonce`, `signedAtMs`, `principal`) in chapter 06. A node that does not implement those layers MUST ignore the corresponding members.

## 4. Error codes

JSON-RPC 2.0 already defines the codes in the first group; hubrpc reuses them unchanged and adds the second group from the JSON-RPC reserved range:

| Code | Name | Source | Meaning |
|---|---|---|---|
| -32700 | `parseError` | JSON-RPC 2.0 | invalid JSON was received |
| -32600 | `invalidRequest` | JSON-RPC 2.0 | not a valid request object |
| -32601 | `methodNotFound` | JSON-RPC 2.0 | the method does not exist / is not served |
| -32602 | `invalidParams` | JSON-RPC 2.0 | params are invalid for the method |
| -32603 | `internalError` | JSON-RPC 2.0 | internal error |
| -32401 | `permissionRequired` | hubrpc | caller is authenticated but lacks a capability covering the call (07) |
| -32402 | `peerDisconnected` | hubrpc | the peer a request was routed to detached before responding |
| -32403 | `requestTimeout` | hubrpc | the request exceeded the idle timeout with no stream activity (03) |
| -32800 | `cancelled` | hubrpc | the request was cancelled (03) |

A node MUST use the listed code for the listed condition. Application interfaces MAY define additional error codes outside the JSON-RPC reserved range (`-32768`..`-32000`); see chapter 04.
