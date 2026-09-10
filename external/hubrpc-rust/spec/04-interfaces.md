# 04 — Interfaces

This chapter defines what a hubrpc **interface** is: a named set of typed members described by an *interface schema*, and a content **hash** that gives the schema a stable identity. It defines the schema format, the restricted JSON Schema subset, the normalization and hashing that derive the interface hash, the `$hubrpc.interfaceHash` caller assertion, and the validation obligations on calls and responses. This layer is a pure addressing/typing mechanism; it requires no identity or capabilities.

## 1. Interfaces and members

An **interface** is identified by an `interface-id` (a `.`-delimited string, e.g. `de.hediet.notification-target`) and declares a set of **members**. A member is one of:

- a **request** member — invoked by a request, answered by a response;
- a **notification** member — invoked by a notification, never answered.

A member is addressed on the wire by a `method` (chapter 01 §2) whose final segment is the member name. The user params of a call (chapter 01 §3) MUST match the member's parameter schema.

A **service** is a concrete provider of one or more interfaces, identified by a `service-id` (a `/`-delimited string). The empty service id denotes the **root service** of a connection, addressed by the two-segment `interface::member` form.

## 2. Interface schema format

An interface schema is a JSON object — a strict subset of OpenRPC 1.x describing one interface (members plus reusable JSON Schemas). Identity and addressing are hubrpc's concern and are absent from this format.

```
InterfaceSchema = {
  id:          string,             // interface id
  hash:        string,             // content hash (§4); id@hash is a diagnostic notation only
  description?: string,            // NORMATIVE markdown; part of the hash
  comment?:    string,             // non-normative notes; stripped from the hash
  methods:     MethodSchema[],     // member names MUST be unique
  components?: { schemas?: { [name: string]: JsonSchema } }
}

MethodSchema = {
  name:          string,           // member name, no "::", unique in the interface
  params:        ContentDescriptor[],   // required params MUST precede optional ones
  result?:       ContentDescriptor,     // omit ⇒ notification-only member
  clientStream?: ContentDescriptor,     // schema for caller→provider stream payloads (03)
  serverStream?: ContentDescriptor,     // schema for provider→caller stream payloads (03)
  errors?:       ErrorSchema[],         // application errors; codes MUST be unique
  summary?:      string,
  description?:  string,           // NORMATIVE; part of the hash
  comment?:      string,           // non-normative; stripped
  deprecated?:   boolean,
  annotations?:  MemberAnnotations
}

ContentDescriptor = {
  name:         string,
  schema:       JsonSchema,
  required?:    boolean,           // default false
  summary?:     string,
  description?: string,            // NORMATIVE; part of the hash
  comment?:     string,            // non-normative; stripped
  deprecated?:  boolean
}

ErrorSchema = { code: number, message: string, data?: JsonSchema }

MemberAnnotations = {             // every flag defaults to false; NORMATIVE
  readOnly?:   boolean,           // no observable state change; implies idempotent, reversible
  idempotent?: boolean,           // N calls ≡ one call
  reversible?: boolean,           // effects can be undone; implies not dangerous
  expensive?:  boolean,           // slow / costly / rate-limited
  dangerous?:  boolean            // irreversible or destructive
}
```

`JsonSchema` is the restricted JSON Schema subset defined in §3. Reusable definitions are referenced as `#/components/schemas/<name>`. A boolean `true` schema matches any value (used where a member declares no value, e.g. a void result); `false` matches nothing.

**Normative vs. non-normative schema fields.** `description` and `annotations` are part of the interface's identity (§4): changing them is a contract change. `comment`, `summary`, and `deprecated` are not part of identity.

## 3. The JSON Schema subset

`JsonSchema` is a deliberately restricted subset of JSON Schema chosen so that *assignability* — does every value matching schema `X` also match schema `Y` — is structurally decidable. A `JsonSchema` is one of:

```
JsonSchema =
  | true | false                                   // top / bottom
  | { type: "null" }
  | { type: "boolean" }
  | { type: "number",  format?: string }           // format = opaque refinement tag
  | { type: "integer", format?: string }
  | { type: "string",  format?: string }
  | { const: <value> }                             // single literal
  | { enum: <value>[] }                            // finite literal set
  | { type: "array", items: JsonSchema }           // homogeneous array
  | { type: "array", prefixItems: JsonSchema[], items?: JsonSchema | false }  // tuple
  | { type: "object",
      properties: { [name: string]: JsonSchema },
      required?: string[],                         // each MUST be a key of properties
      additionalProperties: JsonSchema | false }   // false ⇒ closed
  | { anyOf: JsonSchema[] }                        // untagged union
  | { oneOf: JsonSchema[], discriminator?: { propertyName: string } }  // tagged union
  | { $ref: string }                               // "#/components/schemas/<name>" only
```

Every node MAY additionally carry the annotation keys `title` and `description`.

**Excluded keywords.** The following JSON Schema keywords are **not** part of the subset and MUST NOT appear in a `JsonSchema`: `not` (except the special form below), `if`/`then`/`else`, `allOf`, `pattern`, `patternProperties`, `propertyNames`, `dependentSchemas`, numeric bounds (`minimum`, `maximum`, `multipleOf`, …), and length/size bounds. A producer that lowers a richer schema into this subset MUST drop these.

**`format`** is an opaque refinement tag on `number`/`integer`/`string`. Two schemas with different `format`s are incomparable; adding a `format` narrows, dropping one widens.

**`oneOf` vs `anyOf`.** `oneOf` is structurally identical to `anyOf` for assignability; the distinction is preserved only to record that the source was a tagged (discriminated) union. `discriminator.propertyName` names the property branches dispatch on; it is a consumer hint and is not otherwise enforced.

### 3.1 Normalization

Before an interface schema is hashed (§4), every `JsonSchema` in it MUST be in the following canonical form, so that structurally-equal schemas produce byte-identical bytes:

1. Drop every key not listed above (annotation keys such as `examples`, `default`, `$comment`, `readOnly`, and every excluded keyword).
2. The empty schema `{}` (after dropping) becomes `true`.
3. `{ "not": {} }` becomes `false`; any other `not` is dropped.
4. A `{ type: "object" }` with no `additionalProperties` becomes closed — `additionalProperties: false`.
5. `required` is sorted ascending.
6. For a `oneOf` with no explicit `discriminator`, a `discriminator` MAY be synthesized when every branch is an object schema sharing exactly one property that is `const`-valued and pairwise-distinct across branches; a `discriminator` with no `oneOf` to attach to is dropped.

> **Rationale.** The subset trades JSON Schema's full expressivity for decidable assignability and a stable, language-independent canonical form — both prerequisites for the interface hash (§4) and for schema-compatibility checks across versions.

## 4. Interface hash

The **interface hash** is the content identity of an interface schema. It is computed as:

1. **Canonical document.** Take the schema with each `JsonSchema` normalized (§3.1), recursively remove every `comment` field and, at the top level, the `hash` field itself, and remove members whose value is `undefined`. (No other field is removed; `description` and `annotations` remain.)
2. **Canonicalize.** Serialize the canonical document with [RFC 8785](https://www.rfc-editor.org/rfc/rfc8785) (JCS).
3. **Hash.** Compute SHA-256 ([FIPS 180-4](https://csrc.nist.gov/pubs/fips/180-4/upd1/final)) over the UTF-8 bytes of the JCS output.
4. **Truncate.** Take the first 8 bytes of the digest and render them as 16 lowercase hexadecimal characters. This string is the hash.

Two schemas have the same hash iff their canonical documents are byte-identical.

> **Note.** `id@hash` is a human/diagnostic notation for "interface `id` at version `hash`". It is never sent as a single token on the wire: a method (chapter 01 §2) carries only the id, and a version assertion travels as the separate `$hubrpc.interfaceHash` field (§5).

> **Rationale.** 64 bits of hash is a deliberate collision budget per id, not a security boundary — identity is scoped by `id`, and the hash only distinguishes versions of that id.

> **Note.** Producers and consumers in different languages MUST agree byte-for-byte on the canonical document. The JCS encoding of numbers and strings, the normalization rules (§3.1), and the exact set of stripped fields are the interop-critical surface; the conformance corpus (conformance.md) pins them with vectors.

## 5. `$hubrpc.interfaceHash`

A caller MAY assert the interface hash it believes the target implements by setting `interfaceHash` (a hash string, §4) inside the call's `$hubrpc` object (chapter 01 §3.1):

```
params.$hubrpc.interfaceHash = "<16 hex>"
```

When present, this is a fail-fast contract check: a provider MAY reject a call whose asserted `interfaceHash` does not equal the hash of the interface it actually serves, responding with `invalidParams`. When absent, the caller has not pinned a version and the provider MUST NOT reject on this basis.

> **Note.** `interfaceHash` is meaningful with no identity layer present — it is a plain assertion in the call, verifiable by the provider against its own schema. Chapter 07 additionally allows a capability to *require* a particular `interfaceHash` (`TargetPattern.interfaceHash`).

## 6. Validation

For a request whose method names a member it serves, a provider MUST validate the user params against the member's parameter schema. If validation fails, the provider MUST respond with `invalidParams`. On success it MUST emit either a result valid against the member's result schema, or an error whose `code` is one of the protocol codes (chapter 01 §4) or an application code declared in the member's `errors`.

A caller SHOULD validate a received result against the member's result schema and MAY treat a schema-invalid result as a protocol fault.

> **Note.** This is the *contract obligation* of chapter 00 §4 made concrete: hubrpc fixes the shape of the reaction (schema-valid result | enumerated error); the interface schema supplies the schemas; the application supplies the values.
