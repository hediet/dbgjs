# Conformance

This chapter defines what it means to conform to hubrpc: the three nested **profiles** a node may advertise, and the **vector corpus** that pins the interop-critical byte-level behavior. Conformance is judged against the conformance model of [chapter 00 §4](00-overview.md): a node satisfies its profile iff it meets every reaction obligation, emission invariant, and ordering predicate of the chapters that profile includes.

## 1. Profiles

A node advertises exactly one profile. Profiles are nested: each includes the obligations of the one before it.

| Profile | Chapters | A conformant node… |
|---|---|---|
| **Core** | 01, 02, 04, 05 (+03 if it streams) | speaks the message dialect, serves reflection, validates against interface schemas |
| **Signed** | Core + 06 | additionally verifies signed calls and may require identity |
| **Capability** | Signed + 07 | additionally runs the authorization gate |

A node implementing the hub interfaces (08) additionally serves those member contracts; a hub is a Capability node (08 depends on 05/06/07).

**Interop guarantee.** Two nodes interoperate at the greatest profile they share. A call that requires no gated authority (carries no `$hubrpc` signing members and is not gated by the provider) MUST work across any pair of profiles, because the additive-envelope invariant (chapter 00 §3) makes the higher layers' reserved members inert on that path.

**Optional sub-features.** Streaming (03) is optional even within Core: a node that never sends `$stream::send` is Core-conformant. A node MUST NOT advertise a profile whose mandatory chapters (01, 02, 04, 05) it does not fully implement. The identity extension ([identity-extension.md](identity-extension.md)) is an optional add-on to the Signed profile: a Signed node that resolves only inline (`key:`) keys is conformant; one that also resolves `keydoc:` keys and rotation records additionally implements the extension. Either way the call envelope is identical.

## 2. Self-description

A node MUST make its served contract discoverable through reflection (chapter 05): `hubrpc.directory::list` enumerates its services and their `interfaceHash`es, and `hubrpc.schemas::get` returns each advertised interface schema. A conformance checker discovers the node's contract this way, then verifies each obligation against it.

## 3. Vector corpus

The interop-critical behavior is byte-level and is pinned by a corpus of vectors under `conformance/vectors/`. Each vector is a JSON file of `input → expected-output` pairs; a conformant implementation MUST reproduce every expected output exactly. The corpus is the executable form of the obligations that cannot be checked structurally.

| Vector file | Pins | Chapter |
|---|---|---|
| `jcs.json` | RFC 8785 canonicalization (key order, number/string encoding, control escapes) | 04 §4, 06 §2 |
| `method_name.json` | `::` parsing — segment counts, empty-segment and over-length rejection | 01 §2 |
| `framing.json` | NDJSON framing — line splitting, whitespace trim, empty/invalid-line handling, `hubrpc::initialize` handshake | 02 §2 |
| `normalize.json` | JSON Schema normalization (the canonical form before hashing) | 04 §3.1 |
| `interface_hash.json` | full interface-hash derivation from a schema | 04 §4 |
| `pizza_interface.json` | a complete worked interface schema + its hash (the running example) | 04 |

> **Note.** Vectors for the optional layers (signed-call signing input and content hash, capability content hash and the gate's accept/reject decisions, endpoint-URI parsing) extend the corpus as those layers are exercised. The canonicalization, hashing, and signing-input vectors are the load-bearing ones: they are the only behavior two independent implementations cannot rederive from primitives, and a mismatch there breaks identity and authorization interop silently.

## 4. Versioning

This document specifies hubrpc **v1**. The signing pad (`hubrpc-sig/v1/<domain>`) and the reserved key names carry the version. A future major version changes the pad and is therefore cryptographically non-interoperable with v1 by construction, never by accident.
