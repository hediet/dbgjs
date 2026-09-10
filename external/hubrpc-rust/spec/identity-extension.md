# Identity extension — key documents and rotation

**Optional. Extends [chapter 06](06-identity.md).** Chapter 06 names a signer by a `PrincipalId` and resolves its verifying key from an inline `key:` keyId, so an identity is pinned to one perpetual key. This extension generalizes **key resolution** so a principal can commit its genesis key by a *document* (carrying an expiry and a rotation admin) and rotate to later keys — **without changing any wire shape defined in chapter 06**. It adds only (a) two `KeyId` schemes, (b) the `$hubrpcUnsigned.bindings` member that carries the resolving documents, and (c) one signing domain. A node that resolves only inline keys is unaffected; it ignores `bindings`.

## 1. KeyId schemes

A `KeyId` (06 §1) is one of:

```
KeyId = "key:"    <base64url(publicKey)>     // inline key — self-contained (chapter 06)
      | "keydoc:" <signedHash(d, doc)>       // key committed by a document carried in bindings
```

For the `keydoc:` form, `d` is the document's **role domain** (§4): `keyGenesis` for a genesis document, `keyRotation` for a rotation record. `signedHash` is chapter 06 §2's content hash, so the role domain is folded into the committed bytes: a rotation record can never be read as a genesis document.

A `PrincipalId` remains `"id:" + <its genesis KeyId>` (06 §1). The genesis keyId MAY therefore be `key:…` (inline, perpetual, unrotatable) or `keydoc:…` (a genesis document, §3).

> **Note.** `id:key:…` exposes the raw public key in the name; `id:keydoc:…` hides it behind a hash and is the only form that can carry expiry or name an admin. Both are accepted; the choice is the identity holder's.

## 2. The `bindings` member

`keydoc:` keyIds are resolved against documents the caller presents under the unsigned attachment object (chapter 07 §2):

```
params.$hubrpcUnsigned.bindings = { [keyId: KeyId]: KeyGenesis | RotationRecord }
```

`bindings` is **unsigned**: every entry is self-verifying. A resolver MUST accept an entry only if its key equals `"keydoc:" + signedHash(d, doc)` for the entry's role domain `d` (§4) — a tampered document yields a different keyId and resolves to nothing. A `key:` keyId never consults `bindings`.

## 3. Key genesis documents

A `keyGenesis` document commits a principal's genesis key and optional rotation policy:

```
KeyGenesis = {
  key:        string,        // base64url(publicKey)
  expiresMs?: number,        // identity sunset ceiling; absent ⇒ perpetual
  admin?:     PrincipalId    // who may rotate this identity; absent ⇒ unrotatable
}
```

`expiresMs` and `admin` are inside the hash that names the principal, so a leaked genesis key cannot forge a later expiry or swap the admin without becoming a different principal. A genesis document carries no signature.

## 4. Rotation records

To rotate an `id:keydoc:…` identity to a new key, its `admin` signs a rotation record in domain `keyRotation`:

```
RotationRecord = {
  genesisKeyId: KeyId,       // the subject's genesis keyId; subject principal = "id:" + this
  key:          string,      // base64url(new publicKey)
  notBeforeMs:  number,
  expiresMs:    number,      // working-key window end
  supersedes?:  KeyId,       // a prior key this retires
  nonce:        string,      // base64url
  $hubrpcSignature: { keyRotation: SignatureEntry }   // signed by the admin (chapter 06 §2)
}
```

The signing pad for the new domain is `hubrpc-sig/v1/keyRotation` (06 §2). The record's own keyId is `"keydoc:" + signedHash("keyRotation", record)`.

## 5. Key resolution

Key resolution (06 §1) generalizes to the following procedure for a `(principal, keyId)` pair against `bindings` at evaluation time `t`. It returns a verifying key or **rejects** (fail-closed). Implementations MUST bound recursion depth and reject cycles.

1. **Inline** — `keyId` begins `key:`: succeed iff `"id:" + keyId === principal`; the key is the base64url decoding after `key:`.
2. **Document lookup** — `keyId` begins `keydoc:`: let `doc = bindings[keyId]`; reject if absent. Determine the role domain `d` from `doc` (`keyRotation` if `doc` has a `genesisKeyId`, else `keyGenesis`) and reject unless `"keydoc:" + signedHash(d, doc) === keyId`.
   - **Genesis** (`d = keyGenesis`): reject unless `"id:" + keyId === principal`; reject if `doc.expiresMs` is set and `t > doc.expiresMs`; the key is the decoding of `doc.key`.
   - **Rotation** (`d = keyRotation`): reject unless `"id:" + doc.genesisKeyId === principal`. Resolve the genesis document `g = bindings[doc.genesisKeyId]` (its own integrity per step 2); reject if `g.admin` is absent. **Recursively resolve** the admin's key for `(g.admin, doc.$hubrpcSignature.keyRotation.keyId)` at `t`, and reject unless the record's `keyRotation` signature verifies under it. Reject unless `doc.notBeforeMs ≤ t ≤ min(doc.expiresMs, g.expiresMs ?? ∞)`. The key is the decoding of `doc.key`.

The same procedure resolves call-signature keys, capability-issuer keys, and admin keys — a capability `issuer` is just another principal, its `keyId` says which key signed.

## 6. Lifetime and compromise

- `expiresMs` baked into a principal (via its genesis document) is a **sunset**, not revocation: it guarantees the identity dies *by* that time but cannot bring death forward.
- Early revocation lives at the relying party: a service removes a principal from its accepted roots (chapter 05 `rootPrincipalSets`, chapter 08). Trust is the verifier's, never the token's.
- `expiresMs` on a **rotation record** genuinely bounds a leaked working key, since the admin sets the window and the working key cannot widen it — the case for an offline admin plus short-lived working keys.
- The effective lifetime of a call is the minimum over the chain: `min(genesis.expiresMs, rotation.expiresMs, capability.expiresAtMs, …)`.

## 7. Versioning

There is **no per-message algorithm field**. The signing pad encodes the version: `hubrpc-sig/v1/<domain>` (06 §2) fixes one frozen suite `(Ed25519, SHA-256)` and, being inside the signed bytes, cannot be downgraded. Algorithm agility is achieved by **adding** a version (a new pad, e.g. `hubrpc-sig/v2/<domain>`), never by negotiating within one. The algorithm is a property of the resolved key, never of caller-supplied input.

> **Note.** This follows PASETO's versioned-fixed-suite model and the DID subject / verificationMethod / proof split (`principal` / `keyId` / `{keyId, sig}`), borrowing the models without their serialization machinery.
