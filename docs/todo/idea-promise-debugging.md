# Idea: promise and pending-work debugging

Status: a bounded, evidence-only subset is implemented. The broader lifecycle,
history, provenance, and policy design below remains exploratory.

## Implemented bounded subset

The implementation deliberately consists of two observers that produce the
same `PromiseSnapshot` value:

- `jsdbg promise inspect <remote-object-id>` reads the current engine-supplied
  internal properties for an existing live reference. Recognized V8
  `[[PromiseState]]`/`[[PromiseStatus]]` evidence yields `pending`,
  `fulfilled`, or `rejected`; missing or unfamiliar evidence yields `unknown`.
  Fulfillment values and rejection reasons use a bounded preview and retain an
  existing remote-object reference when one is available.
- `jsdbg promise list [<capture>] [--state <state>]` scans an existing immutable
  heap capture for exact promise-like V8 node names. It reports only nodes
  strongly reachable from the snapshot root, reuses capture-scoped heap
  references, and reads state/result edges only when the snapshot explicitly
  exposes recognized engine names. The returned references compose with the
  existing `heap refs`, `heap retainer-path`, and `heap dominators` operations.

Both observers report their evidence and use the classification
`indeterminate`; the subset has no historical evidence with which to claim
that a pending promise is hung, abandoned, or still doing useful work. List
size defaults to 100 and settlement previews default to 120 characters, with
CLI options to lower or raise those explicit bounds.

This subset does not wrap the global `Promise`, count awaits or reactions,
continuously track production targets, infer promise age, materialize heap
objects into new live handles, or introduce another source model. Heap captures
made without engine internals can still enumerate promise-like nodes, but their
state remains `unknown`. Current V8 snapshots may expose
`reactions_or_result` without a separate state bit; because that edge cannot
distinguish a fulfilled `Error` value from a rejection reason, this subset
correctly leaves those nodes `unknown` and a state-filtered query may be empty.

## Motivation

A paused or running target frequently has promises that never resolve, that
resolve far later than expected, or that are rejected and then quietly kept
alive by a cache. None of this is visible from a single `Runtime.evaluate`
call or a single heap snapshot taken after the fact: a snapshot shows shape,
not history, and a live handle shows current value, not why the value is what
it is or how long it has been that way.

A concrete motivating case: a GitHub extension caches `currentUser` as a
promise so concurrent callers share one in-flight request. If that request
rejects, some cache implementations still store the rejected promise and keep
returning it to every subsequent caller. Symptoms in the field look like
"random" `-1` exit codes, an auth check that "was fine a minute ago," or a
status bar item stuck showing a stale error. The debuggee itself did nothing
wrong at the moment you attach: the interesting event, the original rejection
and its cause, already happened. The report a user can give is close to
worthless ("it doesn't work"), so the debugger has to reconstruct history from
whatever the runtime retained plus whatever we started recording before the
report arrived.

This is a different problem from making generated source readable. Formatting,
deminification, and source recovery are covered by
[idea-source-reconstruction.md](./idea-source-reconstruction.md) and are not
duplicated here. This document assumes that stack frames, call sites, and
async creation frames may need that machinery to become readable, and simply
treats "source location" as an opaque, already-resolved concept supplied by
that pipeline. Nothing here should reintroduce a second source-artifact model,
a second confidence scale, or a second resolver chain.

## Why this needs its own model

Existing CDP primitives are individually insufficient, and one of the gaps is
not a convenience gap but a missing primitive:

- CDP has **no generic promise-lifecycle event stream**. There is no
  `Promise.created` / `Promise.settled` / `Promise.awaited` (or similarly
  named) protocol event that fires for every promise in a target, and nothing
  in this document should imply one is available or would be cheap to add.
  Continuous lifecycle tracking (below) is only possible through one of:
  target-side or library instrumentation the target opts into, Node's
  `async_hooks` where the runtime is Node and exposes them, injected
  JavaScript instrumentation (e.g. wrapping the global `Promise` constructor
  and prototype methods) with the semantic limitations that implies, or a
  future runtime-specific CDP provider that does not exist today. Every
  "durable, timestamped" fact in this design traces back to one of these, not
  to a built-in CDP stream.
- `Runtime.evaluate` / `Runtime.getProperties` on a promise value show the
  current promise-state internal properties, but only for a promise you
  already have a handle to, and only "now." The internal-property names used
  in this document (e.g. V8's `[[PromiseState]]` / `[[PromiseResult]]`) are
  one engine's example, not a fixed contract; see "Inspection and mutation
  limitations."
- `Runtime.awaitPromise` waits for settlement of a promise for which the
  debugger already has a live handle; it does not explain why something is
  still pending, and it is not a query over arbitrary retained promises.
- `Debugger.setAsyncCallStackDepth` plus the `asyncStackTrace` attached to
  pause/exception-time data gives creation-chain frames only while capture is
  enabled, only up to a depth limit, and only at the moment an async task is
  actually captured. Enabling it does **not** make a promise's creation stack
  retroactively queryable later: nothing in CDP lets a later call ask "what
  was this promise's creation stack" unless something read the async stack
  trace at creation time and stored it against that specific promise then.
  That capture-and-associate step is itself instrumentation's job, not a
  built-in query.
- `HeapProfiler.getObjectByHeapObjectId` can materialize a heap-snapshot
  object into a live remote object, and its inverse,
  `HeapProfiler.getHeapObjectId`, maps an already-live `RemoteObject` back to
  its heap object ID for correlation. A heap object ID is not tied to a
  single GC cycle: it can remain valid across garbage collection and
  compaction for as long as the object stays alive and the heap-profiler
  session/profile that assigned the ID is still current. Materialization
  fails when the object has actually been collected, or when that identity is
  no longer valid because the profiler state was reset/disabled or the target
  session ended — not merely "because another GC or snapshot happened."
- A heap snapshot's retainer graph explains *what currently holds* an object,
  but producing one is not a cheap live operation: it requires
  `HeapProfiler.takeHeapSnapshot`, which walks the heap graph and is a
  comparatively expensive, point-in-time operation.
  `HeapProfiler.startTrackingHeapObjects` can preserve allocation identities
  and emit heap statistics, but it does not itself provide the complete
  retainer graph. A snapshot cannot show when a promise was created, when it
  settled, or how many times it has been awaited.

None of these individually distinguishes "this request is still legitimately
in flight" from "this request finished 40 minutes ago and its rejection is
still being handed out." That distinction needs state that something
explicitly captured over time, not a single call.

## Capabilities

The design should support these workflows:

- Answer "what is the current state of this promise?" for a promise reached
  through a variable, property, closure, or heap-snapshot object ID.
- Answer "how long has this promise been in its current state?" using the
  earliest timestamp we can attribute to it, snapshot or live.
- Distinguish a promise that is plausibly still doing work from one that looks
  abandoned, using duration plus retainer evidence rather than duration alone.
- Enumerate rejected promises that are still reachable from GC roots, i.e.
  rejections a caller could still observe by awaiting a cached value.
- Detect that the same rejected promise instance has been awaited or read
  more than once, which is exactly the caching bug pattern — but only where
  call-site/cache instrumentation or a suitable runtime-specific hook (e.g.
  Node `async_hooks`) actually records each attachment; CDP itself exposes no
  count of how many times a promise's reactions were attached or its value
  was read. Distinguish "a `.then`/`.catch` reaction was attached" (visible,
  if at all, only through such instrumentation) from "a JS `await` expression
  consumed this promise" (a narrower fact, since `await` desugars to reaction
  attachment and is not separately surfaced to introspection); without
  instrumentation, this must be reported as unknown, not zero.
- Show one or more retainer/ownership paths from GC roots to a promise, each
  with its own best-effort classification (intentional cache versus
  incidental survival, e.g. a live closure over a variable versus a stale
  debugger console reference). Producing any of this always means taking or
  reusing a heap snapshot, so it is an expensive, deliberate operation, not
  something run on every tracked promise.
- Show the creation call stack and, where available, the async provenance
  chain (the chain of `then`/`await`/scheduling frames that led to this
  promise's creation), reusing source-location resolution rather than
  reimplementing it. "Where available" is doing real work here: this exists
  only when creation-time instrumentation captured and stored it, since
  enabling async stack capture alone does not make it queryable after the
  fact (see Concepts).
- Associate a promise with request-shaped metadata when the runtime code
  attaches it (URL, method, cache key, request ID) without requiring the
  debugger to understand the target's specific request library.
- Group promises that derive from one root promise (via `.then`, `.catch`,
  `Promise.all`, an internal retry wrapper, etc.) so a fan-out of awaiters
  is shown as one story instead of many unrelated pending promises.
- Report whether a rejection was ever reported as unhandled via
  `Runtime.exceptionThrown` (the engine's own signal that a rejection is
  unhandled) and whether a later `Runtime.exceptionRevoked` correlates to that
  same report (the engine's signal that a previously-unhandled rejection has
  since gained a handler), versus still undetermined because the promise
  remains reachable and no revocation has been observed. Correlation between
  the two relies on matching `exceptionId` and is not always attributable
  back to one specific tracked promise with certainty, so this status stays a
  best-effort observation, not a guarantee.
- Materialize a promise reference (a heap-snapshot node ID or a tracked ID)
  into a live promise handle when the underlying object still exists, and
  clearly report when it does not; conversely, correlate an already-live
  promise handle back to a heap object ID (via
  `HeapProfiler.getHeapObjectId`) so live and snapshot views of the same
  object can be cross-referenced.
- State plainly what cannot be inspected or changed: promise internals are
  read-only, a settled promise cannot be re-settled, and some fields are
  engine/version dependent.
- Make continuous tracking strictly opt-in, bounded, and cheap enough to leave
  on for real workloads, given that it must run **before** the interesting
  failure so history exists when a user reports a problem.
- Make clear which facts came from continuous tracking (durable, timestamped)
  versus a single heap snapshot (instantaneous, retroactive) versus a live
  handle (current, but only for objects still alive), and never blend their
  confidence.
- Tolerate races: a target can settle, get collected, or be mutated between an
  observation and a follow-up query, and the model must say so rather than
  return stale data silently.
- Produce structured, stable-shaped output suitable for both a human summary
  and machine consumption (JSON), analogous to how source-match results are
  structured in the source-reconstruction design.

## Concepts

The concepts below are meant to stay independent, in the same spirit as the
source/provenance/projection split in
[idea-source-reconstruction.md](./idea-source-reconstruction.md): each answers
one question, and none should have to stand in for another.

### Promise reference

A way of pointing at one JS promise value without yet holding a live handle
to it: a variable/property/closure path, a heap-snapshot node ID, or a
previously-assigned tracked ID from continuous tracking. A reference by
itself carries no history and no ownership information, and it may or may not
still resolve to a live object.

### Promise handle

A live `RemoteObject` with `subtype: "promise"` — the only thing this
document calls a "handle." A handle is what `resolve(reference)` or
`materialize(...)` produce when they succeed, and it is what
`Runtime.getProperties`, `Runtime.awaitPromise`, and
`HeapProfiler.getHeapObjectId` actually operate on. Heap-snapshot node IDs
and tracked IDs are *inputs* — promise references — that must be resolved or
materialized into a handle before any live operation is possible; they are
never themselves called a handle here.

### Promise state

The instantaneous engine-reported facts: `pending | fulfilled | rejected`, and
when not pending, the settlement value or reason. This is exactly what
`Runtime.getProperties(..., ownProperties: true)` or the debugger's existing
internal-properties surface already exposes — as `[[PromiseState]]` /
`[[PromiseResult]]` in current V8, but treat those two names as one engine's
example, not a fixed contract: another engine, or a future V8 version, may
expose the same facts under different names (e.g. `[[PromiseStatus]]` /
`[[PromiseValue]]`), a different shape, or not at all. This document does not
propose changing how that is fetched, only how it is combined with everything
else below, and requires that any code reading these names tolerate a
different name, shape, or absence rather than assuming V8's.

### Observation record

A durable, timestamped fact about one promise, produced by continuous
tracking: creation observed, first-seen-pending, settled-to-fulfilled,
settled-to-rejected, reaction-attached (a `.then`/`.catch` handler attached —
which is also what a JS `await` compiles down to, so this record does not by
itself distinguish the two; see the capability note above), became
unreachable. Every kind of record can exist only because some concrete
mechanism produced it — target/library instrumentation calling an opt-in
hook, Node's `async_hooks` where available, injected JavaScript
instrumentation wrapping `Promise`, or a future runtime-specific provider
(see "Why this needs its own model") — there is no observation record this
design can produce without one of those in place. An observation record has a
wall-clock or monotonic timestamp and a source (which of those mechanisms
produced it). Multiple observation records accumulate into a promise's
history; they are never overwritten, only appended, so replaying history
stays possible even if later interpretation changes.

### Age

Derived, not primitive: `now - earliest attributable timestamp`. The
"earliest attributable timestamp" is explicitly ranked, because different
sources have different honesty:

1. An observation record from continuous creation tracking (most trustworthy;
   true creation time).
2. An observation record from first-seen-pending during tracking (creation
   happened at or before this time; age is a lower bound, not exact).
3. A heap snapshot timestamp, when the promise already existed in the earliest
   available snapshot and there is no better record (age is *at least* this
   old; still a lower bound).
4. No record at all: age is unknown, not zero. Unknown age must never be
   presented as "just created."

### Liveness classification

A verdict, not a raw number, so a duration threshold alone never decides
anything by itself: `likely-in-flight | suspicious | abandoned | indeterminate`.
Classification should combine:

- age vs. a policy-configured expectation (which itself may be per
  provenance-tagged request kind, not global);
- whether the promise's task appears to still be doing anything observable
  (e.g. it is the awaited value of a live async frame that is itself still on
  the call stack or in the async-task queue, versus reachable only from a
  cache with no runnable continuation left);
- whether sibling promises from the same root or the same request kind have
  settled in the meantime (if 50 identical requests since this one all
  finished, this one is probably stuck, not merely "slow this once").

"Suspicious" must remain a hypothesis with stated evidence, never an assertion
that the target is broken.

### Retainer path and classification

Retainer analysis is one concept with two parts always produced together, not
two independent queries: a **path** is one chain of ownership edges from a GC
root to the promise, reusing whatever retainer-path representation a heap
snapshot already provides (property name, closure variable, array/map slot,
internal slot); a **classification** is a coarse, best-effort label attached
to that specific path — `looks-like-cache`, `looks-like-in-flight-await`,
`looks-like-incidental-debug-reference`, `unknown` — derived from that path's
own structural evidence (is the retaining slot a `Map`/plain object keyed by
something request-shaped, does the property name look like a memoization
field, etc.) and reported as a hypothesis alongside that evidence, never as a
certainty and never as a call detached from the path it describes.

A promise can have several retainer paths; the model must keep all requested
paths (each with its own classification) rather than silently picking one,
since "who keeps handing this rejection out" is often visible only in a
second or third path (e.g. a memoizing cache map plus a leftover console
variable).

Producing any retainer path at all requires a heap snapshot: either a fresh
`HeapProfiler.takeHeapSnapshot` or an already-running
`HeapProfiler.startTrackingHeapObjects` session, both of which walk the heap
graph and are comparatively expensive, point-in-time operations — not a cheap
live call comparable to `state()`. A design must treat retainer analysis as
something invoked deliberately and sparingly (e.g. on demand for one
suspicious promise), not something continuous tracking runs per observation.

### Creation stack and async provenance

The synchronous stack captured at `new Promise(...)`/async-function-entry
time, plus the async provenance chain of scheduling frames leading to it,
present only when async stack capture (`Debugger.setAsyncCallStackDepth`) was
active *and* something captured and stored those frames against this specific
promise at the moment of creation. Neither condition is retroactive:
`Debugger`'s async stack traces exist only as an attachment to specific
runtime events (a pause, a thrown exception) at the moment they fire, so
nothing in CDP lets a later query ask "what was this promise's creation
stack" unless instrumentation read the async stack trace at creation time and
associated it with the promise then, durably, as part of continuous
tracking's bookkeeping — never derived after the fact. These are source
*positions* until resolved; resolving them into readable locations is the job
of `idea-source-reconstruction.md` and is not reimplemented here. Absence of a
creation stack (capture was off, instrumentation was not present at creation
time, or the promise predates tracking) must be represented explicitly, not
synthesized.

### Request metadata association

Optional, target-supplied key/value data attached to a promise (URL, HTTP
method, cache key, request ID, retry count). The debugger does not know what a
"request" is for an arbitrary target; it only offers an attachment point
(e.g. an opt-in instrumentation helper the target can call, or a convention
for reading a well-known symbol/property on the promise or its retaining
cache entry) and carries whatever metadata shows up. Absence of metadata is
normal, not an error.

### Promise group

A set of promises connected by derivation: `.then`/`.catch`/`.finally`
continuations, `Promise.all`/`Promise.race`/`Promise.allSettled` inputs, and
target-defined wrapping (retry helpers, timeout races). A group has a root (or
roots, for combinators) and members, and the group's own age/state summary
should be distinguishable from any one member's.

### Unhandled-rejection status

Whether a rejection has been reported via `Runtime.exceptionThrown` (the CDP
event the engine emits when it decides a rejection is unhandled, carrying an
`exceptionId` and rejection details), later revoked via a correlated
`Runtime.exceptionRevoked` (the event the engine emits when a rejection it
previously reported unhandled subsequently gains a handler), or remains
undetermined because the promise is still reachable and could still be
awaited later. Correlating an `exceptionRevoked` back to the original
`exceptionThrown` for the same promise relies on matching `exceptionId` and is
not always possible with full confidence, so "handled after being reported
unhandled" must stay a best-effort inference, reported alongside its
correlation confidence, not asserted outright. This status is itself an
observation record, not a static fact, since it can only be known after the
fact and only to the extent the engine emits and this design can correlate
these events.

### Live materialization

Turning a promise reference — a heap-snapshot node ID, or a tracked ID held
from continuous tracking — into a live promise handle via
`HeapProfiler.getObjectByHeapObjectId`, so its current state, properties, or
an on-demand `Runtime.awaitPromise` can be queried; and the inverse,
correlating an already-live promise handle back to its heap object ID via
`HeapProfiler.getHeapObjectId`, so a live view and a snapshot view of the same
object can be cross-referenced. Heap object IDs are not tied to a single GC
cycle: an ID can remain valid across garbage collection and compaction for as
long as the object is alive and the heap-profiler session/profile that
assigned the ID is still the active one. Materialization fails — and must be
reported as "no longer live," distinct from "still pending" — when the object
has actually been collected, or when the originating identity is no longer
valid because the profiler state was reset/disabled or the target session
ended, not merely "because another GC or snapshot happened."

## Primitive basis

A small, composable basis, mirroring the operator style of
`idea-source-reconstruction.md`, grouped by what kind of thing each operation
does rather than left as one flat list:

```text
# producers: turn a reference into something usable, or start/stop tracking
resolve(reference) -> promise handle | not-live
materialize(tracked id | heap node id) -> promise handle | not-live
track(policy) -> enable/disable continuous observation, scoped and bounded

# observers: read facts, at a cost proportional to what they read
state(promise handle) -> promise state
history(promise reference) -> observation record[]
retainer_analysis(promise reference, depth?) -> retainer path[], each with
  its classification attached (see Concepts: classification is derived
  together with its path, not a separate query) — requires a heap snapshot,
  so this is deliberate and expensive, never run per observation
provenance(promise reference) -> creation stack + async chain | absent
metadata(promise reference) -> request metadata | absent
group(promise reference) -> promise group
unhandled_status(promise reference) -> unhandled-rejection status

# transforms: derive a verdict from already-observed facts, never call the runtime
age(history, policy) -> duration | unknown
classify(state, history, group, policy) -> liveness classification

# lifecycle: bundle observers into one consistent, timestamped view
snapshot(scope) -> point-in-time bundle of the above, content- and
  time-stamped

# renderer: presentation only, produces no new facts
render(bundle) -> human summary | structured output
```

Everything a CLI command or UI view needs (`show promise`, `list suspicious
promises`, `find rejected and retained`, `explain age`) should be a
composition of these, the same way source-recovery conveniences compose from
`resolve/match/transform/verify/select/render` without inventing a second
model. Retainer-path classification is folded into `retainer_analysis` rather
than kept as a separate `classify_retainer` step, because a classification
has no meaning detached from the specific path it was computed from; merging
them removes a primitive that never had an independent input, instead of
letting the basis grow one primitive per helper.

## Snapshots vs. live state vs. continuous tracking

Three different temporal views must stay explicitly distinguished, because
conflating them produces confidently wrong answers:

- **Live state**: queried right now, via a live handle. Exact for "what is it
  right now," useless for "how did it get here" or "how long has it been like
  this," and only possible while the object still exists.
- **Heap snapshot**: one instant, already in the past by the time it is
  analyzed. Excellent for retainer paths and for a lower bound on age
  ("already existed at snapshot time T"), useless for anything that happened
  between snapshots, and it freezes GC-graph shape but not creation timestamps
  unless the target itself recorded them.
- **Continuous tracking**: an opt-in, running observer that appends
  observation records as they happen (creation, settlement, repeated reaction
  attachment, reachability loss), only for whichever kinds its underlying
  mechanism can actually see. Only continuous tracking can give an honest
  creation timestamp and an honest settlement timestamp; it is also the only
  source that can answer a user's after-the-fact report, because by the time
  someone reports "auth is stuck," the debugger must already have been
  watching.

A structured result must tag every fact with which of these three views
produced it, and a summary must not silently upgrade a snapshot-derived lower
bound into an exact age.

## Tracking overhead and opt-in policy

Continuous tracking has to assume it runs on production-adjacent, long-lived
processes, not only inside a short debugging session, so it needs a policy
surface, not an all-or-nothing switch:

- Off by default; explicitly enabled per context/target.
- Scoped: by module, by a target-supplied tag/label, by promise "kind"
  inferred from request metadata, or global, with global being the most
  expensive and last resort.
- Bounded: a cap on tracked-promise count, a cap on retained history length
  per promise (e.g. keep only the last N observation records once a promise
  has settled and is no longer of interest), and a cap on total memory the
  tracking metadata itself may hold.
- Cheap by construction: creation/settlement hooks are supplied by whichever
  mechanism is in play (an instrumentation call, an `async_hooks` callback, a
  wrapped-`Promise` shim) and should be O(1) bookkeeping, not a stack walk or
  a heap query, unless the policy explicitly opts into capturing creation
  stacks (which is inherently more expensive and should be a separate,
  finer-grained toggle from "track promise lifecycle at all"). Async
  provenance/creation-stack capture piggybacks on
  `Debugger.setAsyncCallStackDepth` and its existing cost model rather than
  introducing a second stack-walking mechanism.
- Evictable: once a settled promise is unreachable (confirmed via a
  subsequent heap snapshot or a weak-reference-style check) its tracking
  record can be pruned according to policy, but the eviction itself should be
  recorded (a final observation record: "no longer tracked as of T") rather
  than the promise's history simply vanishing.
- Explainable: enabling tracking should report its own resource usage so a
  user can judge whether it is safe to leave on.

## Race, GC, and staleness

- A promise can settle between `state()` and the next call; every read must
  be timestamped so a caller can tell whether two facts are from the same
  instant.
- A heap-snapshot object ID stops resolving via
  `HeapProfiler.getObjectByHeapObjectId` when the object has actually been
  collected, or when the heap-profiler session/profile that assigned the ID
  is no longer current (a resumed target that later triggers a new snapshot,
  a different session) — not simply because *some* GC happened, since these
  IDs are designed to survive GC and compaction while the object is alive;
  `materialize()` must return a distinguishable "not live" result rather than
  an error indistinguishable from other failures.
- Retainer paths computed from an older snapshot can be stale relative to
  current live state; a retainer-path result must carry the snapshot's
  timestamp/identity so it is never presented as "current."
- A tracked promise ID surviving in our bookkeeping does not imply the
  underlying JS object still exists; tracking metadata and the live object
  have independent lifetimes, and losing the live object must not silently
  delete useful history.
- Continuous tracking itself can race with a rapid create/settle cycle; a
  design should not assume every transition is observable, and should be able
  to report "settled before we could observe pending" as its own case rather
  than misreporting duration as unknown-but-zero.

## Inspection and mutation limitations

These must be stated plainly rather than discovered by a user via a confusing
error:

- Promise state and result are readable but not writable: there is no CDP
  operation to force a pending promise to settle, or to change a settled
  promise's value/reason.
- `Runtime.awaitPromise` observes settlement but is a consuming interaction
  aimed at one command's own promise value; it is not a general polling query
  and should not be repurposed as one inside continuous tracking.
- Internal slot names/availability are engine- and version-dependent
  examples, not a fixed contract: current V8 exposes
  `[[PromiseState]]`/`[[PromiseResult]]`, but another engine, or a future V8
  version, could expose the same facts as `[[PromiseStatus]]`/
  `[[PromiseValue]]`, a different internal-properties shape, or omit them
  entirely; a design must degrade to "unknown" rather than assume any one
  engine's names, and this includes handler/reaction internals, which are
  even less standardized.
- Async stack capture has a configurable, finite depth
  (`Debugger.setAsyncCallStackDepth`); provenance chains longer than that
  depth are truncated, and truncation must be visible in the result, not
  silently dropped.
- Enabling `Debugger.setAsyncCallStackDepth` does not retroactively make a
  promise's creation stack queryable: it only affects whether async stack
  frames are captured going forward, at the moment they are attached to a
  runtime event (a pause, a thrown exception). It is instrumentation's job
  (see "Creation stack and async provenance") to read those frames at
  creation time and store them against the promise; without that step, a
  promise created before or without such capture-and-store instrumentation
  has no queryable creation stack, period.
- Retainer-path computation depth/breadth is bounded for cost reasons; a
  result must say when paths were truncated rather than imply a root was
  unreachable.
- None of this can attach retroactive metadata to a promise that already
  existed before tracking started; such a promise's provenance/metadata are
  legitimately "absent," not "not yet fetched."

## Structured output shape

Mirroring how `idea-source-reconstruction.md` keeps `compatibility` as a gate
and `score` as a rank, a promise diagnostic bundle should separate a gate-like
verdict from supporting evidence rather than compressing everything into one
number:

```text
promise:
  id: tracked-id | heap-node-id | remote-object-id   # a promise reference;
                                                       # resolved/materialized
                                                       # into a live promise
                                                       # handle only where an
                                                       # operation below needs
                                                       # one
  state: pending | fulfilled | rejected
  age:
    value: duration | unknown
    lower_bound: true | false
    source: continuous-tracking | heap-snapshot | none
  classification:
    verdict: likely-in-flight | suspicious | abandoned | indeterminate
    evidence: [...]
  retainers:
    - path: [...]
      classification: looks-like-cache | looks-like-in-flight-await | ...
      as_of: snapshot-id   # always a snapshot: retainer_analysis() requires
                           # a heap snapshot, so this is never "live"
  provenance:
    creation_stack: [...] | absent
    async_chain: [...] | absent (truncated: true|false)
  metadata: { ... } | absent
  group:
    root: promise-ref
    members: [...]
  unhandled_rejection:
    status: reported | revoked | undetermined
    as_of: timestamp
    correlation_confidence: high | low | n/a   # for `revoked`: how confident
                                                # the exceptionRevoked match is
  materialization:
    live: true | false
    reason_if_not_live: collected | session-invalidated | never-tracked
```

Human output should render this as a short narrative ("this promise has been
rejected for 42 minutes, is retained only by `authCache.get('currentUser')`,
was never explicitly caught, and 12 other awaiters have read the same
rejection since it settled"), while the structured form keeps every field
above so tooling and later analysis are not limited to prose.

## Composition examples

### Diagnosing a cached transient failure (GitHub `currentUser`)

This example assumes continuous tracking is active via some concrete
mechanism (e.g. injected instrumentation wrapping the target's `Promise`
usage, or a library-level hook the target's cache/request code calls
explicitly): every fact below other than a plain live/snapshot CDP call
(`state()`, `retainer_analysis()`) depends on that instrumentation having
already been running when the rejection happened.

```text
list suspicious promises (policy: rejected + retained > 5 min)
  -> for the matching promise:
       state()               -> rejected
       age()                 -> 42m, source: continuous-tracking
       retainer_analysis()   -> path: authCache map slot "currentUser",
                                classification: looks-like-cache
                                (requires a heap snapshot; not a cheap call)
       provenance()          -> creation stack at getCurrentUser(), async
                                chain through fetchWithRetry() — present only
                                because instrumentation captured and stored
                                it at creation time
       metadata()            -> { url, requestId, retryCount: 3 } — present
                                only because the target's request code
                                attached it explicitly
       unhandled_status()    -> revoked (an exceptionThrown for this
                                rejection was followed by a correlated
                                exceptionRevoked; correlation_confidence: high)
  -> group(root) -> 12 members: every subsequent getCurrentUser() call
       reused the same rejected promise instead of retrying — visible only
       because instrumentation recorded a reaction-attached observation for
       each call site against the same tracked ID
  -> render(bundle) -> human summary + structured JSON
```

The distinguishing fact ("one rejection, twelve reuses") comes from `group()`
plus repeated reaction-attached observation records on the same tracked ID
across many call sites — something call-site/cache instrumentation (or a
suitable runtime-specific hook) must have recorded as it happened, not
something any single CDP call can reconstruct after the fact.

### Distinguishing a slow request from a stuck one

```text
resolve(promise reference)
  -> state()          -> pending
  -> age()             -> 3s, source: continuous-tracking
  -> group(root)        -> 40 sibling requests of the same request "kind"
  -> compare ages of settled siblings from the same kind
  -> classify() -> likely-in-flight (3s is unremarkable for this kind)
```

versus:

```text
resolve(promise reference)
  -> state()           -> pending
  -> age()              -> 25m
  -> group(root)         -> siblings of the same kind settle in ~200ms
  -> classify() -> suspicious (evidence: age far outside sibling distribution)
```

### Materializing a promise found in a heap snapshot

```text
snapshot(scope) -> heap-node-id for a promise reachable from a suspicious
  retainer
  -> materialize(heap-node-id)
       live: true  -> state()/retainer_analysis() reflect current state, not
                      snapshot-time state
       live: false -> report "collected since snapshot" (or "session
                      invalidated" if the originating profile is no longer
                      current), keep the snapshot-derived facts labeled as
                      historical
```

### Reusing source reconstruction for provenance, without duplicating it

```text
provenance(promise reference) -> creation stack frames (generated positions),
    if instrumentation captured and stored them at creation time; otherwise
    absent
  -> hand each frame's position to the source-reconstruction pipeline from
     idea-source-reconstruction.md (resolve/match/render)
  -> render each frame with whatever confidence that pipeline reports
     (generated-only, formatted, verified original, AI-reconstructed)
```

This document does not define a second "make this stack frame readable"
mechanism; it only produces positions and asks the existing pipeline to
render them.

## Failure and ambiguity

- A promise that cannot be resolved from the given reference must be reported
  as `not-found`, distinct from `not-live` (existed, now collected) and
  `never-tracked` (exists, but predates tracking so has no history).
- Truncated async chains, truncated retainer paths, and capped observation
  history must all be visible in the result rather than silently shortened.
- Two retainer paths reaching the same promise are not a contradiction; both
  should be kept, since the "cache plus stray console reference" pattern is
  common and diagnostically useful.
- A `suspicious` classification is a hypothesis with stated evidence; it must
  never be presented as proof that the target has a bug.
- Group membership is best-effort: `Promise.all`/derived-`.then` tracking
  depends on hooking those creations while tracking is active, so a group can
  legitimately be incomplete for promises created before tracking started.
- If tracking was disabled for part of a promise's life, its history must show
  the gap explicitly (e.g. a recorded tracking-start observation later than
  the promise's actual, unknown creation time) rather than implying continuous
  coverage.

## Deferred questions

- What is the right default policy for "expected age per request kind," and
  should it be learned from observed sibling distributions rather than
  configured?
- Should retainer classification be purely structural, or should it eventually
  accept target-supplied hints (e.g. a cache library that self-identifies)?
- How much of request-metadata association should be a documented convention
  (well-known symbol/property) versus a small injected instrumentation helper?
- Should continuous tracking be able to opt into deeper async-chain capture
  temporarily (e.g. once a promise looks suspicious) rather than paying that
  cost for every promise from creation?
- How should tracked-promise identity survive a target reload/navigation, if
  at all?
- What is the right eviction/retention policy default for settled promises so
  "leave tracking on in production" stays safe?
- Should group detection special-case common combinators
  (`Promise.all`/`allSettled`/`race`) structurally, or rely only on generic
  `.then` linkage discovered via instrumentation?
- How should this model represent promise-like thenables that are not native
  `Promise` instances?
