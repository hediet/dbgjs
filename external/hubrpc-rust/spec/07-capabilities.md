# 07 — Capabilities

**Optional. Requires chapter 06.** Identity says *who* makes a call; a capability says *what that identity is allowed to call*. This chapter defines the `SignedCapability` — a signed grant from an issuer to an audience — how a caller presents one under `$hubrpcUnsigned.capabilities`, and the **gate** a provider runs to authorize a call: verify the call's identity, reject replays, then check that some presented capability *permits* it. The model is fail-closed: absent a permitting capability, a gated call is refused.

## 1. Capability

A capability is a flat JSON object plus a `capability`-domain signature (chapter 06 §2):

```
Capability = {
  issuer:      PrincipalId,     // principal that signed this capability
  audience:    PrincipalId,     // the only principal allowed to wield it
  permissions: Permission[],    // a call is admitted iff ANY permission admits it
  expiresAtMs?: number,         // Unix ms; absent ⇒ never expires
  parentHash?: string,          // signedHash("capability", parent); absent ⇒ root
  nonce:       string           // per-capability distinguisher, base64url
}

SignedCapability = Capability & { $hubrpcSignature: { capability: SignatureEntry } }
```

The `capability` signature commits to `signingInput("capability", cap)` (chapter 06 §2): the capability with `$hubrpcSignature` and `$hubrpcUnsigned` stripped.

### 1.1 Permission

```
Permission = {
  target:       TargetPattern,                  // which endpoints (required)
  canInvoke?:   boolean,                         // may invoke;  default false
  canDelegate?: boolean,                         // may re-issue narrowed; default false
  params?:      { [key: string]: ParamMatcher }, // optional value allowlist
  callBind?:    { alg: "sha256", payloadHash: string } // collapse to one exact call
}

TargetPattern = {
  serviceId:    Pattern,        // delimiter "/"
  interfaceId:  Pattern,        // delimiter "."
  interfaceHash?: string,       // optional version pin; matched against $hubrpc.interfaceHash
  members:      Pattern[]       // any-of; [{prefix:""}] is the universal wildcard
}

Pattern = { exact: string } | { prefix: string }

ParamMatcher =
  | { exact: <value> }          // canonical-JSON equality
  | { enum: <value>[] }         // canonical-JSON equality to any listed value
  | { prefix: string }          // value is a string with this prefix
  | { subsetOf: string[] }      // value is a string[] ⊆ this set
  | { any: true }               // matches anything
```

`canInvoke` and `canDelegate` both default to `false` — a permission that grants neither admits nothing.

**Pattern matching.** A `{exact}` pattern matches a value iff equal. A `{prefix:""}` matches anything. A non-empty `{prefix: p}` over an axis with delimiter `δ` matches `value` iff `value === p` or `value` starts with `p + δ`; for the `members` axis (no delimiter) it degrades to `startsWith(p)`.

**Param allowlist.** When `params` is present it is a **strict allowlist**: the call's user-param key set MUST equal the declared key set, and each value MUST satisfy its matcher. The only way to permit a free value is `{ any: true }`. Value comparison uses RFC 8785 canonical-JSON equality.

**callBind.** When present, the permission admits exactly one call: the call whose content hash `signedHash("call", signedCall)` (chapter 06 §2) equals `payloadHash`. This pins every signed field at once (method, user params, `nonce`, `signedAtMs`, `principal`, `interfaceHash?`) and is therefore intrinsically single-use — the nonce ledger rejects any replay of the one call it names.

> **Note.** `callBind` uses the *same bytes the call signature commits to*, so an issuer can pre-compute it at consent time exactly as the caller will sign — the basis of "Allow once".

## 2. Presenting capabilities

A caller attaches its capabilities under the call's `$hubrpcUnsigned` object (chapter 01 §3):

```
params.$hubrpcUnsigned = {
  capabilities?: SignedCapability[],   // the delegation bag (this chapter)
  bindings?:     { [keyId: string]: <object> }   // key-resolution evidence; identity extension only
}
```

`$hubrpcUnsigned` is **not** covered by the call signature (chapter 06 §3): it is extrinsic, authored by the issuer(s), not the caller. The `capabilities` bag carries the leaf capability and every ancestor needed to resolve the delegation chain by `parentHash`. The `bindings` member is reserved for the identity extension ([identity-extension.md](identity-extension.md)); a Core or Capability node ignores it.

## 3. Delegation chains

A capability with a `parentHash` delegates from exactly one parent, referenced by `signedHash("capability", parent)`. The chain is linearized (at most one parent per link). The parent travels in the same `$hubrpcUnsigned.capabilities` bag and is resolved by hash. **Effective authority is the intersection over the chain**: a call MUST be permitted by every link. A link delegating to a child MUST hold `canDelegate` for the delegated set.

## 4. The gate

A provider that gates a call MUST, in order:

1. **Verify identity** (chapter 06 §4): valid signature, `principal` present, method assertion, freshness, nonce single-use. On failure, reject.
2. **Resolve the chain**: for the presented leaf capability, resolve every ancestor by `parentHash` from the bag, and verify each capability's `capability` signature and `audience`. Every capability's `audience` MUST equal the call's `principal`; every `expiresAtMs` MUST be in the future.
3. **Check permission**: the call MUST be permitted, for the `invoke` ability, by the intersection of the chain — i.e. by *some* permission in *every* link, with `canInvoke` set, whose `target` addresses the call, whose `params` allowlist (if any) the call's user params satisfy, and whose `callBind` (if any) the call's content hash matches. `interfaceHash` in a `target`, when set, MUST equal the call's `$hubrpc.interfaceHash`.

A call that fails authorization MUST be rejected with `permissionRequired` (chapter 01 §4). The gate is **fail-closed**: any missing, malformed, expired, or non-matching capability yields refusal, never a default-allow.

> **Note.** Steps are ordered cheapest-rejection-first only as guidance; the normative requirement is that all of identity, chain validity, and permission hold. An ungated call (one the provider does not require a capability for) skips the gate entirely — this is what lets a Capability provider interoperate with a Core caller on its open surface (chapter 00 §3).
