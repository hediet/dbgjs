// Generates conformance vectors under ../vectors. The "ground truth" for these layers IS
// standard JavaScript behavior, which is exactly what the TS hubrpc impl relies on:
//   - JCS (protocol/jcs.ts) = JSON.stringify for primitives + Object.keys().sort().
//   - method names (protocol/methodName.ts) = split("::") with the documented arity rules.
//   - JSON-RPC framing (protocol/jsonRpc.ts) = plain 2.0 objects.
// So we reproduce that reference behavior here without a fragile build-time import of the
// package. Later milestones (interface hash, signed envelope, capabilities) will import the
// real TS impl, since those are not reproducible from JS primitives alone.

import { writeFileSync, mkdirSync } from "node:fs";
import { fileURLToPath } from "node:url";
import { dirname, join } from "node:path";

const here = dirname(fileURLToPath(import.meta.url));
const outDir = join(here, "..", "vectors");
mkdirSync(outDir, { recursive: true });

function write(name, data) {
  const path = join(outDir, name);
  writeFileSync(path, JSON.stringify(data, null, 2) + "\n");
  console.log(`wrote ${name} (${Array.isArray(data) ? data.length : "?"} cases)`);
}

// ── JCS (inlined from hubrpc/src/protocol/jcs.ts) ──────────────────────────────
function jcsCanonicalize(object, seen = new Set()) {
  if (typeof object === "number" && Number.isNaN(object)) throw new Error("NaN");
  if (typeof object === "number" && !Number.isFinite(object)) throw new Error("Infinity");
  if (object === null || typeof object !== "object") return JSON.stringify(object);
  if (seen.has(object)) throw new Error("circular");
  seen.add(object);
  let result;
  if (Array.isArray(object)) {
    const values = object.map((cv) => jcsCanonicalize(cv === undefined ? null : cv, seen));
    result = `[${values.join(",")}]`;
  } else {
    const parts = [];
    for (const key of Object.keys(object).sort()) {
      const v = object[key];
      if (v === undefined) continue;
      parts.push(`${JSON.stringify(key)}:${jcsCanonicalize(v, seen)}`);
    }
    result = `{${parts.join(",")}}`;
  }
  seen.delete(object);
  return result;
}

const jcsValues = [
  { b: 1, a: 2 },
  { n: 1.0, m: 42 },
  "a\tb\nc\u0001d",
  { z: [3, 2, 1], a: { y: true, x: null } },
  [],
  {},
  { "": "empty-key", "0": "zero", "10": "ten", "2": "two" },
  { price: 1299, currency: "EUR", note: "extra \"cheese\"" },
  "unicode: café ☕ 日本語",
  { negative: -5, zeroFloat: 0.5, neg: -2.25 },
  true,
  false,
  null,
  0,
  [1, "two", false, null, { k: "v" }],
];
write(
  "jcs.json",
  jcsValues.map((value) => ({ value, canonical: jcsCanonicalize(value) }))
);

// ── method names (inlined from hubrpc/src/protocol/methodName.ts) ──────────────
function parseMethodName(method) {
  const parts = method.split("::");
  if (parts.some((p) => p.length === 0)) return null;
  if (parts.length === 1) return { kind: "bare", member: parts[0] };
  if (parts.length === 2) return { kind: "interface", interfaceId: parts[0], member: parts[1] };
  if (parts.length === 3)
    return { kind: "full", serviceId: parts[0], interfaceId: parts[1], member: parts[2] };
  return null;
}

const methodInputs = [
  "list",
  "hubrpc.directory::list",
  "acme::com.acme.pizza@ab12::order",
  "",
  "a::",
  "::a",
  "a::b::c::d",
  "a::b::",
  "single",
  "hub::hubParticipant::registerServiceId",
];
write(
  "method_name.json",
  methodInputs.map((input) => ({ input, expected: parseMethodName(input) }))
);

// ── JSON-RPC framing (plain 2.0 objects; parse → reserialize must be lossless) ─
const framingMessages = [
  { jsonrpc: "2.0", id: 1, method: "s::i::m", params: { a: 1, b: [2, 3] } },
  { jsonrpc: "2.0", id: "req-7", method: "list" },
  { jsonrpc: "2.0", method: "ping", params: [1, 2] },
  { jsonrpc: "2.0", method: "noParams" },
  { jsonrpc: "2.0", id: 1, result: { ok: true } },
  { jsonrpc: "2.0", id: 2, result: null },
  { jsonrpc: "2.0", id: null, result: null },
  { jsonrpc: "2.0", id: 9, error: { code: -32601, message: "method not found" } },
  { jsonrpc: "2.0", id: 9, error: { code: -32602, message: "bad params", data: { field: "a" } } },
];
write(
  "framing.json",
  framingMessages.map((value) => ({ value }))
);

// ── interface hash (inlined from hubrpc/src/schema/hash.ts) ────────────────────
import { createHash } from "node:crypto";

// Schema normalization (inlined from hubrpc/src/schema/normalize.ts). Used by
// both the interface-hash projection below and the `normalize.json` fixtures.
const KEPT_KEYS = new Set([
  "type", "format", "properties", "required", "additionalProperties", "items",
  "prefixItems", "const", "enum", "anyOf", "oneOf", "discriminator", "$ref",
  "title", "description",
]);

function isPlainObject(v) {
  return v !== null && typeof v === "object" && !Array.isArray(v);
}
function isEmptyObject(v) {
  return isPlainObject(v) && Object.keys(v).length === 0;
}
function constPropNames(props) {
  const out = [];
  for (const [k, v] of Object.entries(props)) {
    if (v === true || v === false) continue;
    if (isPlainObject(v) && "const" in v) out.push(k);
  }
  return out;
}
function detectDiscriminator(branches) {
  if (branches.length < 2) return undefined;
  const branchPropsList = [];
  for (const b of branches) {
    if (b === true || b === false) return undefined;
    if (b.type !== "object") return undefined;
    if (!b.properties) return undefined;
    branchPropsList.push(b.properties);
  }
  let candidates = constPropNames(branchPropsList[0]);
  for (let i = 1; i < branchPropsList.length; i++) {
    candidates = candidates.filter((n) => constPropNames(branchPropsList[i]).includes(n));
    if (candidates.length === 0) return undefined;
  }
  for (const name of candidates) {
    const seen = new Set();
    let allDistinct = true;
    for (const props of branchPropsList) {
      const key = JSON.stringify(props[name].const);
      if (seen.has(key)) { allDistinct = false; break; }
      seen.add(key);
    }
    if (allDistinct) return { propertyName: name };
  }
  return undefined;
}
function normalizeChild(key, v) {
  switch (key) {
    case "properties": {
      if (!isPlainObject(v)) return {};
      const out = {};
      for (const [pk, pv] of Object.entries(v)) out[pk] = normalizeJsonSchema(pv);
      return out;
    }
    case "items":
    case "additionalProperties":
      if (v === false) return false;
      return normalizeJsonSchema(v);
    case "prefixItems":
    case "anyOf":
    case "oneOf":
      if (!Array.isArray(v)) return [];
      return v.map((s) => normalizeJsonSchema(s));
    case "discriminator": {
      if (!isPlainObject(v)) return undefined;
      const name = v.propertyName;
      if (typeof name !== "string" || name.length === 0) return undefined;
      return { propertyName: name };
    }
    case "enum":
      if (!Array.isArray(v)) return [];
      return v.slice();
    case "required":
      if (!Array.isArray(v)) return [];
      return v.slice().sort();
    default:
      return v;
  }
}
function normalizeJsonSchema(raw) {
  if (raw === true || raw === false) return raw;
  if (raw === null || typeof raw !== "object") throw new Error("expected object");
  if (Array.isArray(raw)) throw new Error("expected object, got array");
  if ("not" in raw && isEmptyObject(raw.not)) return false;
  const out = {};
  for (const [k, v] of Object.entries(raw)) {
    if (!KEPT_KEYS.has(k)) continue;
    const normalized = normalizeChild(k, v);
    if (normalized === undefined) continue;
    out[k] = normalized;
  }
  if (out.type === "object" && !("additionalProperties" in out)) out.additionalProperties = false;
  if (Array.isArray(out.oneOf) && !("discriminator" in out)) {
    const d = detectDiscriminator(out.oneOf);
    if (d !== undefined) out.discriminator = d;
  }
  if ("discriminator" in out && !Array.isArray(out.oneOf)) delete out.discriminator;
  if (Object.keys(out).length === 0) return true;
  return out;
}

// The interface-hash projection normalizes every JSON-Schema position (each
// method's params/result/clientStream/serverStream and every components.schemas
// entry) onto the decidable subset before stripping + hashing. Un-normalized
// schema spelling and schema-level `x-…` extensions therefore never change the
// hash; already-normalized schemas are a fixed point. Best-effort: a position
// that cannot be normalized is left untouched.
function normalizeForHash(v) {
  try {
    return normalizeJsonSchema(v);
  } catch {
    return v;
  }
}
function projectForHash(schema) {
  if (!isPlainObject(schema)) return schema;
  const out = JSON.parse(JSON.stringify(schema));
  if (isPlainObject(out.methods)) {
    for (const method of Object.values(out.methods)) {
      if (!isPlainObject(method)) continue;
      for (const field of ["params", "result", "clientStream", "serverStream"]) {
        if (field in method) method[field] = normalizeForHash(method[field]);
      }
    }
  }
  if (isPlainObject(out.components) && isPlainObject(out.components.schemas)) {
    for (const [k, v] of Object.entries(out.components.schemas)) {
      out.components.schemas[k] = normalizeForHash(v);
    }
  }
  return out;
}

function stripNonNormative(value, isRoot = false) {
  if (value === null || typeof value !== "object") return value;
  if (Array.isArray(value)) return value.map((v) => stripNonNormative(v));
  const out = {};
  for (const k of Object.keys(value)) {
    // Non-normative: `comment`, any `x-…` specification extension, and the root `hash`.
    if (k === "comment" || k.startsWith("x-") || (isRoot && k === "hash")) continue;
    const v = value[k];
    if (v === undefined) continue;
    out[k] = stripNonNormative(v);
  }
  return out;
}

function computeInterfaceHash(schema) {
  const json = jcsCanonicalize(stripNonNormative(projectForHash(schema), true));
  const digest = createHash("sha256").update(Buffer.from(json, "utf8")).digest();
  return Array.from(digest.subarray(0, 8), (b) => b.toString(16).padStart(2, "0")).join("");
}

const pizzaSchema = {
  id: "com.acme.pizza",
  hash: "ignored-at-root",
  description: "Order pizzas and track delivery.",
  comment: "non-normative note that MUST NOT affect the hash",
  methods: {
    order: {
      description: "Place a new pizza order. Charges money.",
      comment: "calls the payment provider",
      annotations: { dangerous: true },
      params: { $ref: "#/components/schemas/PizzaOrder" },
      result: { $ref: "#/components/schemas/OrderConfirmation" },
      errors: [{ code: 1001, message: "out of dough" }],
    },
    trackDelivery: {
      annotations: { readOnly: true },
      params: {
        type: "object",
        properties: { orderId: { type: "string" } },
        required: ["orderId"],
        additionalProperties: false,
      },
      result: { type: "string" },
      serverStream: { $ref: "#/components/schemas/DeliveryUpdate" },
    },
    cancelOrder: {
      params: {
        type: "object",
        properties: { orderId: { type: "string" } },
        required: ["orderId"],
        additionalProperties: false,
      },
    },
  },
  components: {
    schemas: {
      PizzaOrder: {
        type: "object",
        properties: {
          kind: { enum: ["margherita", "pepperoni", "hawaiian", "veggie"] },
          size: { enum: ["small", "medium", "large"] },
          quantity: { type: "integer" },
        },
        required: ["kind", "quantity", "size"],
        additionalProperties: false,
      },
      OrderConfirmation: {
        type: "object",
        properties: {
          orderId: { type: "string" },
          etaMinutes: { type: "integer" },
          priceCents: { type: "integer" },
        },
        required: ["orderId", "etaMinutes", "priceCents"],
        additionalProperties: false,
      },
      DeliveryUpdate: { type: "string" },
    },
  },
};

const hashFixtures = [
  {
    name: "minimal",
    schema: {
      id: "test.iface",
      hash: "",
      methods: {
        ping: { params: true, result: { type: "string" } },
      },
    },
  },
  {
    name: "comment-stripped",
    schema: {
      id: "test.iface",
      hash: "",
      comment: "this comment must not change the hash vs `minimal`",
      methods: {
        ping: {
          comment: "method comment also stripped",
          params: true,
          result: { type: "string" },
        },
      },
    },
  },
  {
    name: "with-annotations-and-description",
    schema: {
      id: "test.iface",
      hash: "",
      description: "A normative description (part of the hash).",
      methods: {
        doIt: {
          description: "Does it. Idempotently.",
          annotations: { idempotent: true, dangerous: true },
          params: {
            type: "object",
            properties: { n: { type: "integer" } },
            required: ["n"],
            additionalProperties: false,
          },
          result: { type: "boolean" },
        },
      },
    },
  },
  { name: "pizza", schema: pizzaSchema },
  // ── specification-extension (x-*) parity ──────────────────────────────────
  // `rich-extensions` carries non-normative x-* material at the interface,
  // method, and JSON-Schema levels; `rich-extensions-stripped` is the same wire
  // contract with the extensions removed. Both MUST produce the same hash — the
  // interface identity is the simple projection, not the rich document.
  {
    name: "rich-extensions-stripped",
    schema: {
      id: "test.rich",
      hash: "",
      description: "Normative description.",
      methods: {
        order: {
          params: { type: "object", properties: {}, additionalProperties: false },
          result: { type: "string" },
          description: "Places an order.",
        },
      },
    },
  },
  {
    name: "rich-extensions",
    schema: {
      id: "test.rich",
      hash: "",
      description: "Normative description.",
      "x-codegen": { tsClientName: "OrderClient", package: "@acme/orders" },
      methods: {
        order: {
          "x-safety": { requiresConfirmation: true, rateLimitPerMin: 5 },
          params: {
            type: "object",
            properties: {},
            additionalProperties: false,
            "x-validation": "z.object({}).strict()",
          },
          result: { type: "string", "x-format": "order-id" },
          description: "Places an order.",
        },
      },
    },
  },
  // ── normalization parity ──────────────────────────────────────────────────
  // `rich-unnormalized` has un-normalized JSON-Schema positions (missing
  // `additionalProperties`, unsorted `required`, incidental keys, an empty `{}`
  // schema, and a schema that is nothing but an `x-…` extension) plus `x-…`
  // metadata at every level. `rich-unnormalized-normalized` is its hand-written
  // normalized simple projection. Both MUST hash identically: the hash projection
  // normalizes schema positions, so un-normalized spelling never changes identity.
  {
    name: "rich-unnormalized",
    schema: {
      id: "test.norm",
      hash: "",
      description: "Normative.",
      "x-codegen": { client: "NormClient" },
      methods: {
        op: {
          "x-safety": { readOnly: true },
          params: {
            type: "object",
            properties: {
              b: { type: "string", minLength: 2 },
              a: { type: "integer", default: 0 },
            },
            required: ["b", "a"],
            "x-validation": "z.object({})",
          },
          result: { $ref: "#/components/schemas/Blob", "x-format": "id" },
          clientStream: {},
          serverStream: { "x-only": "extension" },
        },
      },
      components: {
        schemas: {
          Blob: {
            type: "object",
            title: "Blob",
            properties: { y: { type: "number" }, x: { type: "number" } },
            required: ["y", "x"],
            examples: [1],
          },
        },
      },
    },
  },
  {
    name: "rich-unnormalized-normalized",
    schema: {
      id: "test.norm",
      hash: "",
      description: "Normative.",
      methods: {
        op: {
          params: {
            type: "object",
            properties: { b: { type: "string" }, a: { type: "integer" } },
            required: ["a", "b"],
            additionalProperties: false,
          },
          result: { $ref: "#/components/schemas/Blob" },
          clientStream: true,
          serverStream: true,
        },
      },
      components: {
        schemas: {
          Blob: {
            type: "object",
            title: "Blob",
            properties: { y: { type: "number" }, x: { type: "number" } },
            required: ["x", "y"],
            additionalProperties: false,
          },
        },
      },
    },
  },
];
write(
  "interface_hash.json",
  hashFixtures.map(({ name, schema }) => ({ name, schema, hash: computeInterfaceHash(schema) }))
);

const normalizeFixtures = [
  { name: "drops-incidental-keys", raw: { type: "string", minLength: 1, default: "x", title: "Name" } },
  { name: "object-defaults-closed", raw: { type: "object", properties: { a: { type: "number" } }, required: ["a"] } },
  { name: "empty-becomes-true", raw: {} },
  { name: "not-empty-becomes-false", raw: { not: {} } },
  { name: "required-sorted", raw: { type: "object", properties: { b: { type: "string" }, a: { type: "string" } }, required: ["b", "a"] } },
  { name: "nested-array-items", raw: { type: "array", items: { type: "object", properties: { x: { type: "integer", maximum: 9 } }, required: ["x"] } } },
  {
    name: "oneof-discriminator-synth",
    raw: {
      oneOf: [
        { type: "object", properties: { state: { const: "queued" } }, required: ["state"] },
        { type: "object", properties: { state: { const: "baking" }, progress: { type: "number" } }, required: ["state", "progress"] },
      ],
    },
  },
  {
    name: "discriminator-dropped-without-oneof",
    raw: { type: "object", properties: { k: { type: "string" } }, required: ["k"], discriminator: { propertyName: "k" } },
  },
  { name: "ref-passthrough", raw: { $ref: "#/components/schemas/Foo", description: "kept" } },
  { name: "tuple-prefixitems", raw: { type: "array", prefixItems: [{ type: "string" }, { type: "integer" }] } },
];
write(
  "normalize.json",
  normalizeFixtures.map(({ name, raw }) => ({ name, raw, normalized: normalizeJsonSchema(raw) }))
);

// ── endpoint URIs (delegated to gen-endpoint.mjs) ──────────────────────────────
await import("./gen-endpoint.mjs");

