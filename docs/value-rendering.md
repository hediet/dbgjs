# Value rendering

`target eval <expression>` and `value <expression>` use the same
`ValueSnapshot` preview and property renderer. Both bound previews to 120
characters by default and append `...` when truncated. Evaluation remains
effectful; `value` remains side-effect-safe unless `--allow-side-effects` is
passed.

Ordinary `target eval` does not publish CDP object IDs. Its `reference` fields
are `null`, including property and Promise settlement references. Generic
`value` inspection retains its existing remote-reference output until those
references can be replaced by debugger-owned, lifetime-checked identities.

## Structured-output compatibility

`target eval --json` intentionally changes from the legacy
`EvaluationSnapshot` fields (`kind`, `value`, `unserializableValue`,
`description`, and `objectId`) to the existing `ValueSnapshot` shape
(`selector`, `subtype`, `className`, `preview`, `properties`, and `promise`).
This avoids an unbounded second representation and makes the command's JSON
match generic value inspection. The service's `evaluateTarget` response keeps
its legacy fields and adds a bounded `preview`; scope and property variables
likewise keep their fields and add `preview`. `PromiseSnapshot.reference` is
nullable so a renderer can preserve Promise state while withholding an unsafe
live identity; existing Promise inspection output still contains its previous
string reference.
