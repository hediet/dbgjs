---
name: orthogonal-primitives
description: Factor a complex API, CLI, protocol, state model, or feature design into a small orthogonal basis of composable primitives without losing expressive power. Use when a design has command explosion, overlapping concepts, coupled lifecycle and presentation, special-case workflows, or many features that should compose.
---

# Orthogonal Primitives

Simplify a design by finding a minimal set of independent concepts and
operations from which the required workflows can be composed.

The goal is not fewer features. The goal is less conceptual machinery for the
same expressive power.

## Core principle

Prefer:

```text
small orthogonal basis + composition
```

over:

```text
one operation for every named workflow
```

Treat the task like choosing a basis for a vector space:

- Each primitive should represent an independent dimension.
- No primitive should be derivable trivially from the others.
- Required workflows should be expressible by composing primitives.
- Adding a new workflow should usually not require adding a new concept.

## Method

### 1. Enumerate capabilities as workflows

Write representative user goals without committing to command names or types.
Include ordinary, automated, concurrent, and failure workflows.

For example:

```text
begin collecting
take a snapshot
compare two snapshots
render the result in several forms
```

Do not treat each workflow as evidence for a separate command.

### 2. Identify the nouns

List the durable values, live resources, selectors, policies, and views.

Ask:

- What is an immutable value?
- What is an active process or resource?
- What is persistent desired state?
- What is ephemeral runtime state?
- What merely selects data?
- What merely renders data?

Name different concepts differently, even when an underlying protocol overloads
the same word.

### 3. Separate operation categories

Try to factor operations into:

- **Producers**: create values.
- **Transforms**: derive values from values.
- **Selectors**: choose values or parts of values.
- **Observers**: query live or stored state.
- **Lifecycle operations**: start, stop, attach, detach, open, close.
- **Renderers**: present a value without changing its meaning.
- **Policies**: control recurring behavior without becoming new mechanisms.

An operation that spans several categories is a candidate for decomposition.

### 4. Find independent dimensions

Look for options that should compose freely:

```text
what data x which scope x which transformation x which rendering
```

Avoid encoding combinations into names such as:

```text
print-functions-with-source
print-blocks-with-counts
capture-and-print-difference
```

Prefer:

```text
print --style functions --source excerpt --counts
capture
print --exclude baseline --style blocks
```

### 5. Make values first-class

If users need to inspect, compare, store, filter, or render a result in multiple
ways, model that result as a first-class value.

Prefer immutable values when practical. They make comparison, caching,
concurrency, retries, and JSON serialization easier to reason about.

Keep the lifecycle that produces a value separate from transformations and
rendering of that value.

### 6. Define algebraic operations and laws

For each transform, define exact semantics and useful laws.

Examples:

```text
filter(filter(x, a), b) = filter(x, a and b)
render(x) does not mutate x
snapshot(active) is immutable
exclude(x, empty) = x
exclude(x, x) = empty
```

State behavior for identity values, missing values, ordering, duplicates,
errors, and incompatible operands.

Do not use mathematical terminology to hide approximate or stateful behavior.

### 7. Derive convenience workflows

Show that higher-level features reduce to the basis:

```text
named workflow = primitive A + transform B + renderer C
```

Convenience aliases are acceptable when they abbreviate a common composition.
They should not introduce a second semantic model.

### 8. Test completeness

Apply the proposed basis to:

- The simplest common workflow.
- A complex workflow.
- Automation and structured input/output.
- Concurrent clients.
- Persistence and restart.
- Empty and ambiguous state.
- Partial failure.
- A plausible future feature.

If a workflow requires hidden state or exceptions to normal rules, revise the
basis.

### 9. Test independence

For every primitive, ask:

- Can it be expressed as a composition of the others?
- Does it combine mechanism and presentation?
- Does changing it force unrelated dimensions to change?
- Is it only present to support one example?

Merge redundant primitives and split coupled ones.

## Design heuristics

### Separate mechanism from policy

Mechanism defines what can happen. Policy defines when or where it happens.

Do not create a new mechanism for every policy.

### Separate data from presentation

The stored or transmitted value should not depend on table, source, tree, JSON,
or TUI presentation.

Renderers may choose granularity, but they must not silently change the
underlying meaning.

### Separate durable intent from ephemeral handles

Persist specifications and selectors. Reconstruct runtime handles after
reconnection rather than pretending they are durable.

### Prefer one selector grammar

Use the same identity and relative-selection rules across read operations where
possible. Defaults should be ordinary selector behavior, not hidden special
cases.

### Make defaults identities or aliases

A default should ideally mean:

- The identity transformation.
- The latest value.
- The only unambiguous scope.
- A documented composition of primitives.

Defaults should not create a separate behavior model.

### Keep errors compositional

Ambiguous, stale, incompatible, unavailable, and partial states should remain
explicit through compositions. Do not turn them into empty successful results.

## Warning signs

Reconsider the design when it has:

- Many commands differing only by output shape.
- Commands that both mutate live state and format historical data.
- Several names for the same underlying value.
- IDs whose meaning changes between commands.
- Boolean flags that encode mutually dependent modes.
- Special commands for pairwise combinations of existing operations.
- A growing matrix of `thing-with-option` features.
- Hidden target or scope selection.
- Convenience operations with semantics unavailable through the core API.

## Avoid false simplification

Fewer commands do not necessarily mean a simpler model.

Do not:

- Collapse distinct lifecycles into one overloaded operation.
- Replace explicit state with hidden global state.
- Use a generic "execute" command that merely moves complexity into payloads.
- Force unrelated values into one universal type.
- Remove names that express real domain distinctions.
- Invent an elegant algebra that loses provenance, diagnostics, or precision.

Orthogonality is about independent semantics, not minimal syntax.

## Recommended design output

When applying this skill, present:

1. **Capabilities**: representative workflows that must remain possible.
2. **Concepts**: durable values, live state, selectors, policies, and views.
3. **Primitive basis**: the smallest independent operations.
4. **Composition examples**: how named workflows derive from the basis.
5. **Laws and edge cases**: identities, errors, ordering, and invalidation.
6. **Conveniences**: aliases that do not add semantics.
7. **Deferred choices**: syntax, storage, rendering, and optimizations.

Explicitly call out which concepts or commands were removed, merged, or split
and why the resulting basis retains the original capabilities.
