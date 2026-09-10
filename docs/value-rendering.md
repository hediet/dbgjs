# Value rendering

`target eval <expression>` and `value <expression>` use the same
`ValueSnapshot` preview and property renderer. Both bound previews to 120
characters by default and append `...` when truncated. Evaluation remains
effectful; `value` remains side-effect-safe unless `--allow-side-effects` is
passed.

`target eval --full <expression>` removes the preview limit for strings. For a
JSON-serializable object, use
`target eval --full 'JSON.stringify(value)'`; full mode does not recursively
serialize arbitrary objects or invoke their getters. The JSON response keeps the
same `ValueSnapshot` shape and preserves the complete string in
`preview.preview`. `--max-preview-length <n>` selects a character limit instead;
it cannot be combined with `--full`. Human output suggests these options when
the result preview is truncated. If a value remains incomplete in `--full`
mode (for example, a `Symbol` whose description would require invoking a
getter), human output says that it cannot be represented safely and recommends
evaluating `JSON.stringify(value)` when the value is JSON-serializable. JSON
output never adds prose around the response: `preview.truncated` explicitly
reports completeness in every rendering mode.

Object property lists are bounded to 20 entries by default. `target eval` uses
an ephemeral object group and releases it after rendering; `value` retains
references for explicit follow-up inspection and accepts `--max-properties` to
adjust the limit.

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
