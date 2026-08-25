# Plan: reliable runtime introspection and execution triggers

Status: ready for implementation in a new session.

This plan focuses the next implementation cycle on the runtime-introspection
gaps exposed while debugging a live VS Code extension. It refines Milestones 5
through 7 in [the main roadmap](../../plan.md) without introducing a second
debugger state model.

Related design notes:

- [Debugger data model](../debugger-data-model.md)
- [CLI design](../cli-design.md)
- [Source discovery and reconstruction](./idea-source-reconstruction.md)
- [Promise and pending-work debugging](./idea-promise-debugging.md)

## Corrections to preserve

### One logical breakpoint already fans out

The current reducer already has the right basic multiplicity:

```text
one durable user breakpoint
  -> each eligible target attachment
    -> each loaded script/version whose source view exposes the requested source
      -> zero or more reverse-projected generated positions
        -> one physical CDP breakpoint per distinct generated position/condition
```

`BreakpointState.bindings` is keyed by `PhysicalBreakpointKey`, which includes
the session-qualified script, script version, generated position, and condition.
The missing information is not merely "a resolved location on the breakpoint."
Each physical binding needs to retain:

1. The durable location requested by the user.
2. The source-graph projection that selected this script and generated position.
3. The generated position requested from CDP.
4. The `actualLocation` returned by `Debugger.setBreakpoint`.
5. A backprojection of that actual generated location into the best available
   logical/authored source view, including alternatives and diagnostics.

The number of bindings is therefore the number of installed physical runtime
locations, not the number of logical specifications and not necessarily the
number of targets. One target can contribute several bindings when several
scripts or several reverse mappings match.

### A logpoint is currently lowered through a breakpoint condition

CDP has no first-class logpoint operation. The current implementation creates a
physical breakpoint whose condition is conceptually:

```javascript
console.log(id, JSON.stringify(expression)), false
```

Returning `false` prevents a pause. This is a useful backend technique, but it
is currently also the semantic model. That coupling causes the problems:

- hit count and last-hit time are unavailable;
- successful evaluation cannot be distinguished from no hit;
- evaluation exceptions are not surfaced as action failures;
- serialization failure is conflated with expression failure;
- console output is both the transport and the renderer;
- pause, evaluation, side effect, observation, and presentation cannot compose.

The implementation may continue compiling non-pausing triggers into CDP
breakpoint conditions, but this must become an internal lowering of a richer
trigger specification, not a separate logpoint state model.

### Paused-frame evaluation cannot directly await

`Runtime.evaluate` supports `awaitPromise`.
`Debugger.evaluateOnCallFrame` does not.

Running-target evaluation can therefore offer `--await` directly. Paused-frame
evaluation must reject unsupported awaiting explicitly or use a separately
designed resume-aware workflow. It must not pretend that a pending promise can
settle while the JavaScript event loop is paused.

## Capabilities

The completed work must support these workflows:

1. Set one authored breakpoint before any runtime is connected, then observe
   all projections and physical bindings as matching scripts appear in several
   targets.
2. Distinguish requested authored location, projected generated location,
   CDP-corrected generated location, and backprojected effective authored
   location for every binding.
3. Define one location trigger that may pause, evaluate one or more
   expressions, record hit metadata, emit observations, or combine these
   actions.
4. Keep `breakpoint` and `logpoint` only as convenience aliases over that one
   trigger model.
5. Evaluate an expression in a running target and optionally await its promise.
6. Resolve a heap-snapshot node into a live remote object, correlate a live
   object back to its heap identity, and use either as the receiver of
   evaluation.
7. Send an arbitrary target-scoped CDP request through the same context,
   connection, target, generation, error, and structured-output machinery.
8. Inspect a promise's live state and settlement value/reason.
9. Analyze retained rejected promises and suspicious pending promises using
   heap paths and explicitly qualified historical evidence.
10. Continue controlling and evaluating a target when source reconstruction or
    source rendering is partial; source failures remain diagnostics rather than
    control-path failures.

## Concepts

### Logical trigger specification

Durable intent owned by a debug context:

```text
LogicalTriggerSpec:
  id
  requested logical location
  target selector
  enabled
  condition?
  actions[]
```

The specification remains meaningful with no connections.

### Trigger action

A composable operation requested when the location is reached:

```text
pause
evaluate(expression, result policy)
```

Every hit automatically produces observation metadata, so `count` is not a
separate action. Logging is a renderer or subscription over captured evaluation
results, so `log` is not a separate execution action.

An evaluation result policy is one of:

```text
capture-reference
capture-value
discard-value
```

Discarding a value still records success or exception, allowing a side-effecting
trigger to prove whether its action ran.

### Trigger application

The attachment-specific reconciliation of one logical trigger:

```text
TriggerApplication:
  attachment identity/incarnation
  applicability
  projection candidates
  physical bindings
  diagnostics
```

### Projection candidate

One source-graph path from the requested logical location to a generated script
location:

```text
ProjectionCandidate:
  requested logical location
  effective logical location before runtime installation
  projection path/provenance
  script identity/version
  requested generated location
  mapping quality
```

Several candidates may exist for one script or target. Ambiguity remains
explicit.

### Physical binding

One installed runtime breakpoint:

```text
PhysicalBinding:
  projection candidate
  CDP backend breakpoint ID
  CDP actual generated location
  backprojected effective logical location
  runtime correction/projection quality
  installation status/diagnostics
```

Physical bindings are ephemeral and qualified by connection generation,
attachment incarnation, script identity, and script version. Equivalent
physical locations may be shared by several logical trigger owners without
merging their durable intent.

### Hit observation

A bounded runtime fact emitted when a trigger is reached:

```text
HitObservation:
  trigger ID
  physical binding identity
  target and attachment provenance
  per-binding and aggregate hit counts
  timestamp
  pause epoch, when paused
  evaluation results
  evaluation exceptions
  transport/action diagnostics
```

Hit observations are runtime state/event history, not durable trigger intent.
Retention limits and reconnect/reset semantics must be explicit.

### Value reference

One selector grammar for a JavaScript value:

```text
evaluation result handle
pause/frame/scope/property path
remote object handle
heap capture + heap object ID
tracked promise ID
```

A value reference is an input to evaluation or inspection. It does not require
an `object` command family.

### Evaluation

The primitive is:

```text
evaluate(expression, target, frame?, receiver?, await policy, result policy)
```

The result policy controls transport:

- `reference`: preserve live identity and return a debugger-local remote handle;
- `value`: request CDP serialization, accepting cycle/prototype/function limits.

Rendering as text, JSON, or a tree is independent of this transport policy.

### Raw CDP request

An explicit low-level operation:

```text
call(method, params, selected target generation) -> JSON result | protocol error
```

It bypasses semantic wrappers, but never bypasses target selection, stale
generation checks, authentication, recording, redaction policy, or output
bounding.

## Laws and edge cases

```text
logical trigger identity does not depend on current connections
binding count = installed distinct physical locations
target count != binding count
requested logical location is immutable until the user edits the trigger
CDP actualLocation never overwrites the requested or projected location
backprojection(actual generated location) may be ambiguous or unavailable
disconnect removes only that connection generation's bindings
script replacement invalidates only bindings for that script version
two logical triggers may share one physical CDP binding
every trigger hit increments its count even when an evaluation action fails
render(hit observations) does not execute debuggee code
value transport does not preserve live object identity
reference transport must be released or invalidated explicitly
paused-frame await is unsupported unless a future resume-aware policy says otherwise
raw CDP cannot silently select an ambiguous target
heap materialization failure is not equivalent to object collection unless CDP says so
promise age is unknown unless an explicit observation mechanism established time
```

## Phase 1: preserve complete breakpoint projection and binding state

### Implementation

1. Extend `Input::BreakpointInstalled` to include CDP `actualLocation`.
2. Preserve the actual generated position in `PhysicalBreakpointStatus` and
   `BreakpointBinding`.
3. Replace count-only target breakpoint snapshots with application/binding
   summaries exposing:
   - requested logical location;
   - projection candidate and source/projection provenance;
   - requested generated location;
   - actual generated location;
   - backprojected effective logical location and alternatives;
   - target/script/version identity;
   - installation status and diagnostic.
4. Backproject CDP's actual position through the exact source view used for the
   forward/reverse mapping. Do not search unrelated source views silently.
5. Retain physical-binding deduplication across logical owners.
6. Define aggregate statuses from explicit applications:
   `unconfirmed`, `pending`, `partially-bound`, `fully-bound`, `ambiguous`,
   `failed`.
7. Update CLI text and JSON output to show requested and resolved locations
   without rendering entire minified lines.
8. Update the canonical breakpoint shapes in
   [the debugger data model](../debugger-data-model.md), including
   `GeneratedBreakpointResolution`, `PhysicalBreakpointBinding`, and
   `BreakpointApplication`, in the same change as the implementation.

### Tests

- Reducer test: one logical source maps to two generated positions in one script.
- Reducer test: one logical trigger maps into two targets and three total
  physical bindings.
- CDP test: `actualLocation` differs from requested generated position and both
  survive.
- Projection test: corrected generated position backprojects to a corrected
  authored position without changing requested intent.
- Reconnect/script-version test: stale installation completion cannot populate
  a replacement binding.
- CLI E2E: JSON exposes every binding and the binding count equals the number of
  installed physical locations.

### Acceptance gate

Using shared TypeScript loaded by Node and Chromium, `breakpoint show --json`
reports one logical specification, one application per attachment, all physical
bindings, projection paths, requested/actual generated positions, and
backprojected authored positions. A deliberate CDP correction is visible and
does not mutate the requested TypeScript location.

## Phase 2: unify breakpoint, logpoint, and probe behavior

### Implementation

1. Replace the pause/log behavior union with a logical trigger containing
   composable actions.
2. Make hit count and last-hit metadata automatic for every trigger.
3. Add structured evaluation results and exceptions to hit observations.
4. Implement one backend action compiler:
   - pausing triggers use the physical breakpoint normally;
   - non-pausing synchronous actions may use a condition expression;
   - results/errors are emitted through a debugger-owned structured channel
     installed with `Runtime.addBinding` and observed through
     `Runtime.bindingCalled`, not debuggee console text;
   - the binding/helper name is agent-namespaced, checked for collision, and
     installed independently in every applicable execution context/realm;
   - helper and binding lifecycle is reconciled across execution-context
     creation/destruction, reconnects, and target generations;
   - condition/action failure becomes a hit diagnostic.
5. Keep current command spellings as aliases:
   - `breakpoint` = trigger with `pause`;
   - `logpoint` = non-pausing trigger with captured expression and streaming
     renderer;
   - `probe` = non-pausing trigger with captured/discarded evaluation.
6. Migrate persisted legacy breakpoint/logpoint intent deterministically.
7. Bound stored observations and define reset/retention across reconnects.
8. Update `BreakpointBehavior`, breakpoint applications, and related canonical
   types in [the debugger data model](../debugger-data-model.md) so the
   documentation and implementation continue to describe one model.

Non-pausing hit actions are synchronous. Awaiting a hit expression is out of
scope unless the trigger pauses or target-side instrumentation performs the
asynchronous work.

### Tests

- Breakpoint alias pauses and records one hit.
- Logpoint alias does not pause, records a hit, and renders its captured value.
- Side-effecting probe records success while discarding its result.
- Throwing evaluation records an action exception and continues according to
  policy.
- Cyclic values do not turn successful evaluation into serialization failure.
- Combined pause plus multiple evaluations produces one hit observation.
- Debugger binding-name collision is detected and reported rather than
  overwriting target state.
- Binding/helper installation covers newly created realms and is removed or
  invalidated with the owning target generation.
- Reconnect reinstalls actions without retaining backend IDs from the old
  generation.

### Acceptance gate

One trigger can pause, evaluate two expressions, and report hit metadata. The
same core trigger configured without pause runs a side effect and records either
success or exception without relying on `console.log` as transport.

## Phase 3: running-target awaitable evaluation

### Implementation

1. Add an await policy to evaluation requests.
2. Set `Runtime.evaluate.awaitPromise = true` only for running-target
   evaluation.
3. Reject `--await` with a precise unsupported-state error when evaluation is
   bound to a paused frame.
4. Keep await policy independent from result transport:
   `--await --result reference|value`.
5. Add timeout/cancellation without returning a success-shaped placeholder.
6. Qualify returned references by target generation and execution context.
7. Generalize the canonical `RemoteObjectRef` model beyond pause-only handles:
   - paused evaluation results additionally carry `PauseRef` and become stale
     on resume;
   - running evaluation results carry attachment/generation and execution
     context identity and remain valid until release, collection, context
     destruction, or generation replacement.
8. Update [the debugger data model](../debugger-data-model.md) with these
   distinct lifetime rules in the same change.

### Tests

- Running target awaits a resolving promise.
- Running target surfaces a rejected promise with structured exception details.
- Await timeout is explicit and cancel-safe.
- Paused-frame `--await` is rejected before sending an unsupported CDP request.
- Awaited by-reference result remains inspectable until released/invalidated.

### Acceptance gate

`target eval --await 'fetch(...).then(r => r.status)'` returns the settled value
in a running target, while the same flag against a paused frame returns a clear
capability error.

## Phase 4: value references and live heap materialization

### Implementation

1. Add typed service operations for:
   - `HeapProfiler.getObjectByHeapObjectId`;
   - `HeapProfiler.getHeapObjectId`;
   - `Runtime.releaseObject` / object-group release.
2. Introduce debugger-local value references that hide raw CDP object IDs and
   carry target/session/generation provenance.
3. Resolve `heap:<capture>#<heap-object-id>` into a live value reference.
4. Correlate a live value reference back to a heap identity.
5. Let evaluation accept an optional receiver reference and lower it through
   `Runtime.callFunctionOn`.
6. Reuse value references for property inspection and promise-state inspection;
   do not add an independent object hierarchy.
7. Make collected, stale-session, wrong-target, and profiler-reset failures
   distinguishable.
8. Reuse the generalized running `RemoteObjectRef`/value-reference lifetime
   model from Phase 3 rather than creating a heap-specific object handle.

### Tests

- Snapshot object materializes and its property can be evaluated through
  `this`.
- Live object correlates back to the matching heap identity.
- Collected object reports not-live rather than an empty result.
- A handle from a prior connection generation is rejected.
- Object groups are released and later use fails explicitly.

### Acceptance gate

A repository-like object found by heap selection can be materialized and
patched with:

```text
jsdbg target eval --this heap:<capture>#<id> \
  'this._metadata = undefined'
```

without waiting for a breakpoint.

## Phase 5: target-scoped raw CDP

### Implementation

1. Add one service/CLI operation taking a CDP method and JSON params.
2. Route it through normal context/connection/target selection.
3. Require an unambiguous target and current generation.
4. Return protocol success/error as bounded structured JSON.
5. Include calls in recording/replay with redaction policy.
6. Do not expose raw CDP session IDs as durable public selectors.

Suggested spelling:

```text
jsdbg target cdp <method> --params <json> [target scope]
```

### Tests

- `Runtime.evaluate` succeeds through raw CDP.
- `HeapProfiler.getObjectByHeapObjectId` can be called before the typed wrapper
  exists.
- Ambiguous target selection fails with candidates.
- Disconnect during a call produces a stale/disconnected error.
- Sensitive fields are handled by the same recording/redaction policy as other
  requests.

### Acceptance gate

Any generated CDP method can be exercised against one selected target without
adding a semantic service method, while target identity and failures remain as
strict as typed commands.

## Phase 6: promise state inspection

### Implementation

1. Detect promise remote objects by subtype and engine-supported internal
   properties.
2. Normalize engine-specific names into:
   `pending | fulfilled | rejected | unknown`.
3. Return the settlement value/reason as a value reference plus bounded preview.
4. Preserve the raw internal-property evidence for diagnostics.
5. Support promise references reached from:
   - evaluation;
   - paused variables/properties;
   - heap materialization.
6. Keep live state separate from historical observations.
7. Use `Runtime.exceptionThrown` and `Runtime.exceptionRevoked` as optional
   evidence for unhandled/late-handled rejection status.

### Tests

- Pending, fulfilled, and rejected promises normalize correctly.
- Rejection reason remains inspectable by reference.
- Unknown engine shapes report unknown with evidence rather than guessing.
- Late-handled rejection correlation preserves uncertainty when attribution is
  incomplete.

### Acceptance gate

Given a live or materialized promise reference, one command reports its current
state and inspectable result/reason without awaiting it or changing target
execution.

## Phase 7: retained and suspicious promise analysis

Implement the basis in
[the promise debugging design](./idea-promise-debugging.md) incrementally:

1. Enumerate promise-like nodes in heap snapshots.
2. Normalize snapshot promise state where V8 exposes sufficient internal
   evidence.
3. Compute bounded GC-root/retainer paths using the existing heap graph.
4. Correlate snapshot nodes with live handles when possible.
5. Identify retained rejected promises and cache-shaped retainers.
6. Classify pending promises as `likely-in-flight`, `suspicious`, `abandoned`,
   or `indeterminate` only from explicit evidence.
7. Add optional runtime-specific history providers:
   - Node `async_hooks`;
   - target/library instrumentation;
   - explicitly enabled injected instrumentation with documented limitations.
8. Associate request metadata and creation provenance only when a provider
   actually captured them.
9. Reuse the source reconstruction pipeline for every recorded stack position.

### Tests

- Rejected promise retained by a cache property has a readable GC-root path.
- Several `.then` derivatives can be presented as a related group when the
  chosen provider supplies that relationship.
- Old pending promise with no history remains `indeterminate`, not "hung."
- Pending promise with old age, no active work evidence, and newer settled
  siblings can be classified suspicious with its evidence listed.
- Tracking disabled means historical fields are absent, not zero.

### Acceptance gate

The cached-current-user failure can be diagnosed from a heap capture and any
available observation history: the output identifies the rejected promise,
retaining cache path, settlement reason, live materialization status, and the
evidence/uncertainty behind any repeated-use or age claim.

## Phase 8: source reconstruction integration

Do not build a second deminifier in this workstream.

Use [the existing source reconstruction design](./idea-source-reconstruction.md)
for:

- bounded generated-source rendering;
- deterministic formatting/deminification with projections;
- authored-source discovery;
- partial reconstruction;
- source authenticity and transformation certificates;
- readable trigger, promise provenance, and retainer-related stack locations.

Runtime control, raw CDP, value materialization, and evaluation must remain
available when this pipeline returns warnings, ambiguity, or generated-source
fallback.

## Validation matrix

Every phase must cover:

- reducer/state tests for stale effects and multiplicity;
- service API serialization and compatibility;
- CLI text and JSON output;
- Node and Chromium where the CDP behavior differs;
- reconnect and script replacement;
- target ambiguity;
- bounded output for minified/cyclic/large values;
- no visible provider console windows on Windows;
- no regression of existing launch, coverage, profile, heap, and DAP tests.

Before each phase is considered complete:

```text
cargo fmt --check
cargo test --workspace --all-targets
npm test
extension unit/build tests
focused Playwright E2E for the changed runtime workflow
git diff --check
```

Run only relevant focused E2E tests during iteration; run the broader baseline
before merging a phase.

## New-session starting point

The next session should:

1. Read this plan and the four linked design documents.
2. Inspect the current `BreakpointState`, `PhysicalBreakpointKey`,
   `BreakpointBinding`, `BreakpointApplication`, and source-view reverse mapping
   before editing.
3. Start with Phase 1 only.
4. Add CDP `actualLocation` to the reducer input and state without changing CLI
   grammar yet.
5. Prove one logical trigger can own several corrected bindings before moving to
   unified hit actions.
6. Commit Phase 1 independently once its acceptance gate passes.

Do not start promise tracking, raw CDP, or trigger-action transport in parallel
with the Phase 1 state migration. Those features depend on trustworthy target,
binding, and value identity.
