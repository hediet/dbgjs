# 06 — Identity

**Optional.** This chapter defines how a call is *signed*: the **principal** that names a signing identity (resolved to a verifying key via its **keyId**), the **signed-object standard** that fixes the bytes a signature commits to, the `$hubrpc` signing members and the `$hubrpcSignature` map that carry a call's signature, and the verification rules. Identity is self-contained: it authenticates *who* makes a call but grants no authority on its own (that is chapter 07). A node that does not implement identity omits these members; an unsigned call is well-formed (chapter 00 §3).

## 1. Principals and keys

A signer is named by two role-tagged strings. A **PrincipalId** names a stable identity; a **KeyId** names a verifying key. They are kept distinct so an identity can outlive any one key.

The v1 signature suite is **Ed25519** ([RFC 8032](https://www.rfc-editor.org/rfc/rfc8032)): a signature is 64 bytes, a public key 32 bytes, encoded on the wire as base64url without padding ([RFC 4648](https://www.rfc-editor.org/rfc/rfc4648) §5).

```
KeyId       = "key:" <base64url(publicKey)>     // an inline verifying key
PrincipalId = "id:" <KeyId>                      // a stable identity, named by its genesis key
```

**Key resolution** maps a `(PrincipalId, KeyId)` pair to a verifying key. For the inline forms above, resolution succeeds iff `"id:" + keyId === principal` (the keyId is the principal's genesis key), and the key is the base64url decoding of `keyId` after its `key:` prefix. A pair that does not resolve MUST be rejected (fail-closed).

> **Note.** The trust anchor is the **principal**, which travels inside the signed bytes; the `keyId` is an unsigned hint (§2) that only says *which key to try*. The `keydoc:` keyId form and key rotation — letting a principal name keys beyond its genesis key — are an optional extension ([identity-extension.md](identity-extension.md)) that generalizes key resolution without changing any wire shape in this chapter.

## 2. The signed-object standard

Signing and content-hashing in hubrpc follow one rule, applied to any JSON object, in a **domain**. This chapter uses `call`; chapter 07 uses `capability`. The identity extension adds further domains the same way (the domain set is open; a pad makes each one distinct).

**Reserved keys** (chapter 01 §3), stripped before canonicalization on *every* object:

- `$hubrpcSignature` — the signature map itself; it cannot sign itself.
- `$hubrpcUnsigned` — extrinsic attachments authored by someone other than the signer.

**Signing pad.** For a domain `d`, the *pad* is the UTF-8 bytes of the ASCII string `hubrpc-sig/v1/<d>` (e.g. `hubrpc-sig/v1/call`).

**Signing input.** For an object `obj` in domain `d`:

```
signingInput(d, obj) = pad(d) ‖ jcs( obj without $hubrpcSignature and $hubrpcUnsigned )
```

where `jcs` is RFC 8785 canonicalization (chapter 04 §4) and `‖` is byte concatenation.

**Signature.** `sig = base64url( Ed25519_sign( privateKey, signingInput(d, obj) ) )`.

**Content hash.** `signedHash(d, obj) = base64url( SHA-256( signingInput(d, obj) ) )`. (Used by chapter 07's `callBind.payloadHash` with domain `call` and `parentHash` with domain `capability`.)

**Signature map.** A signed object carries its signatures under `$hubrpcSignature`, a map from domain name to a signature entry:

```
$hubrpcSignature = { "call"?: SignatureEntry, "capability"?: SignatureEntry }
SignatureEntry   = { keyId: KeyId, sig: string }       // sig is base64url
```

`keyId` names the verifying key (§1); `sig` is the signature over `signingInput(d, obj)`. Because `keyId` lives under the stripped `$hubrpcSignature`, it is **unsigned** — a hint that says which key to try, never a trust claim. The trust anchor is the signed-in **principal** (a call's `$hubrpc.principal`, a capability's `issuer`); a `keyId` that does not resolve to that principal (§1) makes verification fail.

Domain separation is intrinsic: because the pad is folded into the signed bytes, a `call` signature can never verify as a `capability` signature or vice versa.

> **Rationale.** One rule covers every signature and content hash in hubrpc. The reserved-key strip and the pad are the only two moving parts, so a verifier in any language re-derives the exact bytes from the wire object.

## 3. Signed calls

A call's signing metadata lives in its `$hubrpc` object (chapter 01 §3.1), the `CallMeta`:

```
params.$hubrpc = {
  method:        string,       // fully-qualified wire method; MUST equal the JSON-RPC method
  nonce:         string,       // replay-protection nonce, base64url
  signedAtMs:    number,       // Unix milliseconds the call was signed
  principal?:    PrincipalId,  // present iff signed; the identity wielding the call
  interfaceHash?: string       // optional version assertion (chapter 04 §5)
}
```

`$hubrpc` is present on both signed and unsigned calls (an unsigned call omits `principal` and carries no `$hubrpcSignature`).

A **signed call** is one whose `params` object is call-signed (domain `call`, §2): `params.$hubrpcSignature.call` carries the `keyId` and the `sig` over `signingInput("call", params)`. On a signed call, `principal` MUST be present, `keyId` MUST resolve to `principal` (§1), and `sig` MUST verify under the resolved key.

> **Note.** The strip rule (§2) removes `$hubrpcSignature` and `$hubrpcUnsigned` but keeps `$hubrpc`, so the signature binds `method`, `nonce`, `signedAtMs`, `principal`, and the user params — a forwarder that rewrites the outer `method` invalidates the signature, and the bound `nonce` makes the bytes unique per attempt.

## 4. Verification

A node that requires a signed call MUST, before acting on it:

1. resolve the verifying key for `($hubrpc.principal, $hubrpcSignature.call.keyId)` (§1) and verify `params` is call-signed under it (§2);
2. assert `$hubrpc.method` equals the JSON-RPC `method`;
3. enforce a freshness window on `signedAtMs` (reject calls outside an acceptable clock-skew bound);
4. enforce single-use of `nonce` per principal (reject a replay of a previously-seen `(principal, nonce)`).

A call failing any step MUST be rejected. The error is `permissionRequired` when the gate is the capability layer (chapter 07); a node enforcing identity alone MAY use `invalidParams`.

## 5. Skipping identity

A node MAY operate without identity, in which case calls carry no `principal` and no `$hubrpcSignature`. A node that requires identity for a given call but receives an unsigned one MUST reject it (§4); a node that does not require identity MUST NOT reject a call merely for being unsigned.
