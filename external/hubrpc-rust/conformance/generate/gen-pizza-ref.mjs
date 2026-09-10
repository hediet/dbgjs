// Emits the pizza interface schema + hash from the REAL TypeScript hubrpc impl (zod →
// z.toJSONSchema → normalizeJsonSchema → computeInterfaceHash). This is the ground-truth
// reference for the cross-language hash-parity canary: the Rust port derives the same
// interface from Rust types via `schemars` + the shared normalize, and must produce an
// identical `id@hash`.
//
// Method names + param field names are snake_case to match what the Rust `#[hub_rpc_interface]`
// macro emits (Rust method names + serde-default snake_case struct fields).
//
// Run from this directory: `node gen-pizza-ref.mjs`. Resolves the built dist (and its peer
// `zod`) from the TS package; set HUBRPC_TS_DIST to override the dist path.

import { writeFileSync } from "node:fs";
import { createRequire } from "node:module";
import { fileURLToPath, pathToFileURL } from "node:url";
import { dirname, join } from "node:path";

const here = dirname(fileURLToPath(import.meta.url));
const distPath =
  process.env.HUBRPC_TS_DIST ??
  "D:/dev/microsoft/vscode-packages/vscode-team-tools/packages/hubrpc/hubrpc/dist/index.js";

const {
  defineInterface,
  requestType,
  notificationType,
} = await import(pathToFileURL(distPath).href);

// The dist does not re-export `z`; resolve zod from the TS package that owns
// the dist (its peer dependency) via a require rooted at the dist file.
const { z } = createRequire(distPath)("zod");

// ── shared value types (mirror docs/examples.md) ──────────────────────────────
const PizzaKind = z.enum(["margherita", "pepperoni", "hawaiian", "veggie"]);
const Size = z.enum(["small", "medium", "large"]);

const OrderConfirmation = z.object({
  order_id: z.string(),
  eta_minutes: z.number().int(),
  price_cents: z.number().int(),
});

const OrderStatus = z.discriminatedUnion("state", [
  z.object({ state: z.literal("queued") }),
  z.object({ state: z.literal("baking"), progress: z.number() }),
  z.object({ state: z.literal("out_for_delivery"), driver: z.string() }),
  z.object({ state: z.literal("delivered") }),
  z.object({ state: z.literal("cancelled"), reason: z.string() }),
]);

const OrderSummary = z.object({ total_cents: z.number().int() });
const Topping = z.object({ name: z.string(), cents: z.number().int() });
const SessionReport = z.object({ plated: z.number().int(), burned: z.number().int() });

const KitchenCmd = z.discriminatedUnion("type", [
  z.object({ type: z.literal("fire"), ticket: z.string() }),
  z.object({ type: z.literal("cancel"), ticket: z.string() }),
]);
const KitchenEvent = z.discriminatedUnion("type", [
  z.object({ type: z.literal("plated"), dish: z.string() }),
  z.object({ type: z.literal("burned"), dish: z.string() }),
]);

// ── the interface ─────────────────────────────────────────────────────────────
const pizza = defineInterface(
  {
    id: "com.acme.pizza",
    description: "Order pizzas and track delivery.",
  },
  {
    order: requestType(
      z.object({ kind: PizzaKind, size: Size, quantity: z.number().int() }),
      OrderConfirmation,
      { description: "Place a new pizza order. Charges money.", annotations: { dangerous: true } },
    ),
    order_status: requestType(
      z.object({ order_id: z.string() }),
      OrderStatus,
      { description: "Current status of an order. Pure query.", annotations: { readOnly: true } },
    ),
    cancel_order: notificationType(
      z.object({ order_id: z.string() }),
      { description: "Fire-and-forget cancel request." },
    ),
    watch_order: requestType(
      z.object({ order_id: z.string() }),
      OrderSummary,
      { description: "Live status updates until delivered/cancelled." },
    ).withStream({ server: OrderStatus }),
    build_order: requestType(
      z.object({ base: PizzaKind }),
      OrderConfirmation,
    ).withStream({ client: Topping }),
    kitchen_session: requestType(
      z.object({ station: z.string() }),
      SessionReport,
    ).withStream({ client: KitchenCmd, server: KitchenEvent }),
  },
);

const out = { id: pizza.info.id, hash: pizza.schemaHash, schema: pizza.toSchema() };
const path = join(here, "..", "vectors", "pizza_interface.json");
writeFileSync(path, JSON.stringify(out, null, 2) + "\n");
console.log(`wrote pizza_interface.json — ${out.id}@${out.hash}`);
