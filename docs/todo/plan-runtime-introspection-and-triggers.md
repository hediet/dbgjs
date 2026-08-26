# Plan: next runtime-introspection increment

Status: ready for implementation.

This plan narrows the next debugger increment to truthful breakpoint bindings,
safe expression evaluation, and basic live-value inspection. It refines the
relevant parts of Milestones 5 and 6 in [the main roadmap](../../plan.md) and
uses the identities and state categories in
[the debugger data model](../debugger-data-model.md).

## Goal

Deliver one coherent live-debugging path:

1. Report every physical installation of a logical breakpoint and the location
   CDP actually accepted.
2. Evaluate in a selected running target or paused frame with correct await
   behavior.
3. Inspect object results through debugger-owned, lifetime-checked references,
   not public raw CDP object IDs.
4. Prevent disconnect, reconnect, resume, or target replacement from making
   stale completions and handles appear current.

## Non-goals and deferred work

This increment does **not** include:

- promise history, rejection timelines, retained-promise analysis, or heap-based
  pending-work heuristics;
- promise-specific features beyond ordinary evaluation and property access;
- source reconstruction, workspace recovery, pretty-printing, or new providers;
- broad breakpoint/logpoint/probe/trigger unification, hit history, or
  trigger-side evaluation;
- heap-snapshot object materialization or live-object-to-heap correlation;
- persistent watch redesign, cross-runtime values, speculative renderers,
  query languages, caching, or performance work.

Raw target-scoped CDP, with schema validation by default and
`--no-validation`, is being implemented in parallel. It is neither work for
this plan nor a completed prerequisite. Both increments must independently use
normal target selection and generation safety.

## Facts and invariants

### Breakpoint multiplicity and correction

One durable logical breakpoint can produce many physical bindings:

```text
logical breakpoint
  -> eligible target attachments
    -> matching script versions
      -> zero or more generated positions
        -> distinct physical CDP breakpoints
```

`BreakpointState.bindings` is already keyed by `PhysicalBreakpointKey`, which
includes the session-qualified script, script version, generated position, and
condition. Do not collapse it to one location per logical breakpoint or one
binding per target. One target can contribute several bindings.

Each installed binding must distinguish:

1. The logical location requested by the user.
2. The projected generated location requested from CDP.
3. `Debugger.setBreakpoint.actualLocation`, which CDP accepted.
4. The best available logical backprojection of `actualLocation`, including
   quality, alternatives, and diagnostics when available.

The requested location is durable intent. Projection and backprojection are
derived knowledge. Backend IDs and `actualLocation` are runtime facts. CDP
correction must never overwrite the requested location.

### Paused-frame await limitation

`Runtime.evaluate` supports `awaitPromise`;
`Debugger.evaluateOnCallFrame` does not. Therefore:

- running-target evaluation may support `await`;
- paused-frame evaluation must reject `await` before sending CDP;
- the debugger must not imply that a promise can settle while its event loop is
  paused;
- any resume-aware workflow is separate future work.

### Target and generation safety

Every live operation resolves exactly one context, connection, target, and
current attachment before dispatch. References and completions reject reuse
after connection-generation replacement, attachment loss, script-version
replacement, pause-epoch replacement, or execution-context destruction, as
applicable.

Raw CDP session IDs, call-frame IDs, breakpoint IDs, and object IDs remain
adapter details, not durable public selectors. A command resolves target scope
once and never silently retargets.

## Current gaps

The implementation already has physical breakpoint multiplicity, running and
paused-frame evaluation, scope/property requests, and generation-aware target
sessions. This increment closes only these gaps:

- breakpoint installation keeps the backend ID but drops `actualLocation`;
- public state cannot explain requested, projected, corrected, and effective
  locations per binding;
- running evaluation has no await policy, and paused evaluation has no explicit
  await capability check;
- evaluation and property results expose raw CDP object IDs;
- live references do not consistently encode generation, attachment, execution
  context, and optional pause epoch;
- late evaluation and property completions need service-boundary staleness
  checks.

## State and API effects

Keep desired intent, runtime facts, derived knowledge, immutable details, and
progress/diagnostics separate.

### Breakpoint binding

Extend each installed physical binding, not the logical specification, with:

```text
requested logical location
requested generated location
actual generated location
effective logical location, quality, and projection path when available
backend binding reference
context/connection-generation/attachment/script-version provenance
diagnostics
```

Shared physical installations may have several logical owners, but each owner
retains its own requested location and projection evidence.

### Evaluation

Use one operation with independent dimensions:

```text
evaluate(expression, target, frame?, await policy, result policy)
result policy = reference | value
```

A missing frame means running-target evaluation. Reject `frame + await`. Return
structured exception details rather than an empty value, and keep transport
policy independent of CLI rendering.

### Live values

Replace public `objectId` inputs and outputs with an opaque debugger reference
qualified by context, connection generation, attachment, execution context,
optional pause epoch, and a local ID.

Paused references expire when the pause ends. Running references expire on
release, collection, execution-context destruction, attachment loss, or
generation replacement. Previously returned by-value data may remain readable
after a reference becomes stale.

Update [the debugger data model](../debugger-data-model.md) with implementation
commits that change canonical shapes; its current pause-only `RemoteObjectRef`
must not remain documented as the final model.

## Independently committable steps

### 1. Preserve CDP breakpoint correction

- Carry `actualLocation` through breakpoint-installation completion and store it
  on the exact `PhysicalBreakpointKey`.
- Reject completions with a mismatched effect, generation, attachment, script,
  or script version.
- Expose requested and actual generated locations in structured output without
  changing durable breakpoint persistence.
- Test multiplicity, correction, deduplication, and reconnect races in the
  reducer.

### 2. Backproject the accepted location

- Backproject `actualLocation` through the same projection basis as the requested
  generated position.
- Preserve ambiguity and quality; do not choose silently.
- Report unavailable backprojection as a diagnostic while leaving the physical
  breakpoint active.
- Test exact, corrected, ambiguous, and unavailable mapping.

### 3. Add explicit evaluation policies

- Add `await` and `reference | value` policies to the service request.
- Set `Runtime.evaluate.awaitPromise` only for running-target evaluation.
- Reject paused-frame `await` before `Debugger.evaluateOnCallFrame`.
- Preserve protocol exceptions, timeout, and cancellation errors.
- Validate target generation and optional pause epoch before dispatch and again
  when accepting completion.

### 4. Introduce debugger-owned live references

- Allocate opaque local references and keep raw CDP object IDs in the adapter.
- Resolve only against the same current runtime provenance.
- Return explicit stale-pause, stale-generation, wrong-target, collected, and
  released errors.
- Provide explicit reference and object-group release.

### 5. Route scopes and properties through the same value model

- Return qualified references from evaluation, scope variables, and properties.
- Accept only debugger-owned references for property inspection.
- Preserve bounded previews independently of live identity.
- Invalidate pause-owned values on resume; apply execution-context and
  generation lifetime to running values.
- Update structured CLI output without adding an object command family.

## Focused acceptance tests

1. **`breakpoint_reports_every_corrected_binding`**
   One authored breakpoint maps to two positions in one target and one in
   another. CDP corrects at least one. Output contains three bindings with all
   requested, generated, effective, and runtime-provenance fields.
2. **`stale_breakpoint_install_cannot_reappear`**
   Delay `setBreakpoint`, reconnect, then release generation one's response.
   Reject it; generation two has no old backend ID or `actualLocation`.
3. **`running_evaluation_awaits_and_returns_a_value`**
   `target eval --await` evaluates a resolving promise in a running target.
   By-value output contains the settled value and target provenance.
4. **`paused_frame_rejects_await_without_cdp_dispatch`**
   Evaluate with `await` against a current frame. Return a precise capability
   error; the fixture observes no evaluation call.
5. **`live_reference_properties_and_lifetimes_are_safe`**
   By-reference evaluation returns an opaque reference and property inspection
   succeeds in the same target. It fails explicitly after release, context
   destruction, or reconnect.
6. **`paused_references_and_late_results_are_rejected`**
   Resume after paused-frame object evaluation; old references fail as stale
   while prior by-value previews remain printable. A delayed evaluation released
   after generation or pause replacement reports stale runtime/pause.

## Completion criteria

- all five steps land as independently reviewable commits or equivalent slices;
- every physical binding exposes CDP correction without losing logical intent
  or multiplicity;
- running evaluation supports explicit await and paused-frame await is rejected;
- evaluation, scopes, and properties use opaque lifetime-checked references;
- stale target, generation, script, execution-context, and pause completions
  cannot mutate or masquerade as current state;
- the focused tests pass through the real CLI/service boundary;
- race tests use deterministic CDP fixtures, successful live evaluation uses a
  real Node or Chromium runtime, and all tests assert JSON, errors, exit status,
  and observable waits rather than sleeps;
- the roadmap and data model remain consistent with implemented state and API;
- no deferred item is required to declare this increment done.
