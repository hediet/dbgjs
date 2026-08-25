# JavaScript Debugger Roadmap

This plan tracks the path from the current CLI/service prototype to a reliable,
observable JavaScript debugger backend shared by CLI, MCP, and VS Code clients.

Planning and initial implementation session:
[Rust infrastructure for JS debugger](agent-host-session://copilotcli/f900df48-e1e5-44c8-b4bf-581445fbe510)

## Tracking conventions

- `[ ]` means not started.
- `:running:` means actively being worked on.
- `[x]` means completed and validated.
- Every `:running:` task must include its active
  `agent-host-session://...` session link.
- Every completed implementation task must include its `commit:<sha>` link.
- Do not add a commit link until the commit exists. If a task needs several
  commits, link each relevant commit.
- A milestone is complete only when its acceptance gate passes.
- Deferred work remains unchecked rather than being represented as partially
  successful.

Example:

```md
- :running: Add target event ingestion
  ([session](agent-host-session://copilotcli/example))
- [x] Add target event ingestion
  ([commit](commit:0123456789abcdef))
```

## Architectural basis

All milestones must preserve these independent primitives:

1. **Inputs**
   - `UserCommand`
   - `RuntimeObservation`
   - `EffectCompletion`
2. **State**
   - Immutable context snapshots identified by
     `(agentInstanceId, revision)`.
   - Large immutable artifacts addressed by content identity.
   - Runtime references qualified by connection generation, attachment
     incarnation, and narrower epochs where required.
3. **Effects**
   - Semantic operations such as `Connect`, `AttachTarget`, `LoadScript`, and
     `InstallBreakpoint`.
   - Individual `await` calls are implementation details, not reducer effects.
4. **Observation**
   - One atomic subscription primitive returns a snapshot at revision `r` and
     then revisions after `r`.
   - `get` consumes the initial snapshot.
   - `watch` continues consuming.
   - `wait` applies a predicate and stops.
   - Event queries project the same revision history.
5. **Selection**
   - Context, runtime target, and source selectors remain independent.
   - Interactive focus is a convenience selector and never hidden durable
     scope.
6. **Intent and application**
   - Breakpoints, watches, and policies are durable intent.
   - Attachments, scripts, physical breakpoints, frames, scopes, and remote
     objects are ephemeral applications or runtime facts.

## Implemented baseline

The prototype has moved beyond its original uncommitted baseline. The repository
now contains the reducer, service, CLI, source-map, coverage, heap-analysis, and
live-runtime work described below. Checklist state reflects validated behavior;
partially implemented architecture remains unchecked.

- [x] Typed CDP schema import and generated HubRPC client interfaces.
- [x] CDP WebSocket transport and flattened-session multiplexing.
- [x] Deterministic debugger reducer, semantic effects, recording, and replay
  prototypes.
- [x] Source-map projection engine and immutable source-graph kernel.
- [x] Native named-pipe/Unix-socket CLI service with authentication and
  restart persistence.
- [x] Durable contexts with multiple connection definitions and disconnected
  breakpoint intent.
- [x] Explicit connection cancellation, peer-loss recovery, initial target
  enumeration, and live connect/disconnect/reconnect validation.

Focused follow-up work for breakpoint binding fidelity, unified execution
triggers, awaitable evaluation, live heap-object materialization, raw CDP, and
promise analysis is specified in
[Runtime introspection and execution triggers](./docs/todo/plan-runtime-introspection-and-triggers.md).

Before completing the first roadmap milestone, preserve this baseline in one or
more reviewed commits and replace this note with real `commit:<sha>` links.

## CLI E2E acceptance-test contract

Every milestone must add at least one black-box test that:

- launches the real `jsdbg` executable;
- lets the CLI discover or spawn the real `jsdbg-service`;
- communicates over the platform-native named pipe or Unix socket;
- uses the generated HubRPC client rather than calling service methods directly;
- uses a real Chromium/Node runtime or an explicitly named deterministic fake
  CDP endpoint when protocol races must be controlled;
- invokes separate CLI processes for concurrent-client behavior;
- asserts structured JSON/JSONL output and exit status, not terminal prose or
  private Rust state;
- uses observable waits or stream handshakes rather than timing sleeps;
- shuts down the service and verifies that endpoint, process, and temporary
  artifacts are cleaned up.

The command lines below are the canonical acceptance-test spelling. If CLI
grammar changes before a milestone is implemented, update the command and test
together; the asserted state and failure semantics may not be weakened.

Shared fixtures should live under `tests/fixtures/`, orchestration under
`tests/playwright/`, and process-level assertions beside
[`tests/cli_service.rs`](./tests/cli_service.rs). Expensive browser/server
scenarios should have one Playwright entry point that launches the runtimes and
then invokes the focused Rust process test.

---

## Milestone 1 — Deterministic context state kernel

**Goal:** Establish one state-transition path before continuous runtime events
make the current service state harder to replace.

### Steps

- [x] Define the immutable context state owned by the reducer:
  durable intent, runtime facts, derived knowledge, artifact references,
  progress, and diagnostics.
- [x] Define reducer inputs for all currently implemented operations:
  context/connection mutations, connect, disconnect, peer loss, initial target
  enumeration, breakpoint intent, persistence completion, and failures.
- [x] Define semantic effects and exact stale-completion identity requirements.
- [x] Implement:
  `reduce(previous, input) -> { state, effects, events }`.
- [ ] Add a per-context coordinator that serializes inputs, executes effects
  outside the reducer, and publishes only complete revisions.
- [ ] Move all existing service mutations through the coordinator; prohibit
  direct context-state mutation elsewhere.
- [x] Separate durable persistence projection from ephemeral runtime state.
- [x] Define publication semantics:
  durable state is visible only after atomic persistence succeeds, while
  runtime-only revisions do not require durable writes.
- [ ] Add deterministic transcript replay for the existing workflows in
  [`tests/cli_service.rs`](./tests/cli_service.rs).
- [ ] Add model-based interleaving tests for connect, disconnect,
  reconfiguration, peer loss, and stale effect completion.

### Acceptance gate

- [ ] **CLI E2E: `stale_connect_completion_cannot_revive_disconnected_state`.**
  Run a deterministic fake CDP endpoint that pauses `Browser.getVersion`, then:

  ```text
  jsdbg context create --context race "Race context"
  jsdbg connection add ws://<fake-cdp> --context race --connection browser
  jsdbg connection connect --context race --connection browser       # process A, remains pending
  jsdbg connection disconnect --context race --connection browser    # process B
  <fixture releases Browser.getVersion>
  jsdbg context show --context race
  jsdbg service stop
  jsdbg context show --context race                # restarts service
  ```

  Assert that process B succeeds; process A fails with structured
  `staleOperation`; both final snapshots contain one `browser` connection in
  `disconnected` state, no targets, and no runtime handle; the restarted agent
  has a different `agentInstanceId`; durable intent is identical after restart;
  and context revisions never regress within either agent instance.
- [ ] **CLI E2E: `durable_mutation_is_not_published_when_persistence_fails`.**
  Make the persistent-state destination reject the next replacement, invoke
  `breakpoint set`, and assert a non-zero exit with `persistenceFailed`.
  `context show` from a second CLI process must return the exact prior revision
  and no breakpoint.
- [ ] Existing process/restart scenarios in
  [`tests/cli_service.rs`](./tests/cli_service.rs) still pass through the
  coordinator.
- [ ] Replaying the captured reducer transcript from both scenarios produces
  byte-equivalent state revisions, effects, and normalized events.
- [ ] A structural test proves that the coordinator is the only context-state
  writer.

---

## Milestone 2 — Unified observable HubRPC state

**Goal:** Give every client one race-free mechanism for consistent `get`,
`watch`, events, and waits.

### Steps

- [x] Specify the subscription result and cursor model:
  `Current` and `After(revision)`.
- [ ] Extend HubRPC-Rust with server streaming, client cancellation, and
  transport cleanup.
- [x] Implement an atomic initial snapshot plus subsequent revision observation.
  The current transport uses cancellation-safe unary long polling; native
  HubRPC server streaming remains the next transport step.
- [x] Store bounded context revision/event history.
- [x] Return an explicit `HistoryGap` with a current recovery snapshot or
  revision when a cursor is too old.
- [ ] Define and test slow-consumer behavior without blocking target ingestion.
- [ ] Derive unary state `get` from the same subscription primitive.
- [ ] Derive state `watch`, event query/follow, and race-free one-shot `wait`.
- [ ] Add concurrent-client, cancellation, reconnect, history-gap, and restart
  tests.
- [x] Add JSONL CLI rendering without coupling rendering to stored state.

### Acceptance gate

- [ ] **CLI E2E: `state_watch_has_atomic_snapshot_to_revision_handoff`.**
  Create context `observe` with one breakpoint, start:

  ```text
  jsdbg state watch --context observe --output jsonl                    # process A
  jsdbg breakpoint set bp-2 file:///b.ts 2 --column 1 --context observe # process B
  jsdbg connection add ws://<cdp> --context observe --connection browser # process C
  jsdbg connection connect --context observe --connection browser       # process C
  ```

  Wait for process A to acknowledge its initial item before mutations. Assert
  its first JSONL item is a complete snapshot containing `bp-1`; later items
  have strictly increasing, non-duplicated revisions and contain `bp-2`,
  `connecting`, and terminal `connected` transitions in order. Every event in
  an item has the same revision as that item's snapshot.
- [ ] **CLI E2E: `watch_cancellation_does_not_cancel_observation`.** Start two
  `state watch` processes, cancel process A through normal CLI interruption,
  create a new page, and assert process B observes it. A later unary
  `state get` must contain the page, and the service remains responsive.
- [ ] **CLI E2E: `wait_is_race_free_for_current_and_future_state`.** With one
  page already present, `jsdbg wait observe target --type page --timeout 5s`
  returns immediately with that target. Start a second wait for
  `--type worker`, create a worker after subscription acknowledgement, and
  assert it returns exactly that worker without polling.
- [ ] **CLI E2E: `expired_revision_reports_history_gap`.** Overflow a
  deliberately small test history, run
  `jsdbg events observe --after-revision <expired> --output jsonl`, and assert a
  non-zero exit with structured `historyGap`, `requestedRevision`,
  `oldestAvailableRevision`, and `currentRevision`.

---

## Milestone 3 — Continuous targets and flattened attachments

**Goal:** Turn each connection into a continuously observed target topology with
explicit, generation-safe attachments.

### Steps

- [x] Replace the root rejecting handler with typed root CDP event ingestion.
- [x] Reconcile `Target.getTargets` with `targetCreated`,
  `targetInfoChanged`, and `targetDestroyed`.
- [x] Make initial target enumeration race-free by enabling discovery before
  enumeration and buffering concurrent events.
  concurrent events.
- [ ] Model generation-qualified target incarnations so reused target IDs
  cannot revive stale facts.
- [x] Implement the mechanism to attach and detach exactly one target.
- [x] Route flattened sessions through debugger-owned attachment identities.
- [ ] Model attachment loss independently from target destruction and
  connection loss.
- [ ] Add selector-based attachment policy for matching current and future
  targets without adding separate attachment mechanisms.
- [x] Expose target and attachment snapshots through the unified observation
  API.
- [ ] Add live Chromium tests for pages, workers, OOPIFs, target updates,
  destruction, detach, peer loss, and reconnect.

### Acceptance gate

- [ ] **CLI E2E: `chromium_target_topology_is_continuous`.** Launch one
  browser-level Chromium endpoint and start:

  ```text
  jsdbg context create --context topology
  jsdbg connection add ws://<browser-cdp> --context topology --connection browser
  jsdbg target watch --context topology --connection browser --output jsonl
  jsdbg connection connect --context topology --connection browser
  ```

  Through Playwright create page A, rename/navigate it, create a dedicated
  worker, add a cross-origin iframe that becomes an OOPIF, then close each.
  Assert ordered `target.created`, `target.changed`, and `target.destroyed`
  records with connection provenance and one incarnation per target. Final
  `target list topology --output json` must contain none of the closed targets.
- [ ] **CLI E2E: `attachment_policy_follows_related_targets`.** Configure an
  attachment rule selecting page A and related workers/OOPIFs, then create those
  targets. `attachment list` must eventually contain simultaneous qualified
  attachments for all three with distinct debugger attachment IDs and flattened
  sessions.
- [ ] **CLI E2E: `explicit_detach_does_not_change_focus_or_policy`.** Focus page
  A, explicitly detach its worker, and assert focus remains page A. Because the
  rule still matches, a newly created worker is attached; the detached worker's
  old attachment ID never becomes valid again.
- [ ] **CLI E2E: `old_attachment_events_are_rejected_after_reconnect`.** Delay
  one event in a deterministic CDP proxy, disconnect/reconnect generation 2,
  then release the generation-1 event. Assert it is diagnosed as stale and
  absent from the generation-2 target/attachment snapshot.

---

## Milestone 4 — Scripts and the context-wide source graph

**Goal:** Connect disconnected workspace/build sources and live scripts through
one shared graph.

### Steps

- [x] Enable `Runtime` and `Debugger` on eligible attachments.
- [ ] Ingest execution-context and `Debugger.scriptParsed` observations.
- [ ] Invalidate live script endpoints on attachment or generation loss without
  deleting reusable immutable source knowledge.
- [ ] Lift source-graph ownership from individual resolved views into context
  state.
- [ ] Contribute lightweight, provenance-qualified live script endpoints before
  loading source content.
- [ ] Add disconnected providers for workspace files, generated files, and
  inline/external source maps.
- [x] Lazily hydrate generated content and source maps through semantic effects.
- [x] Reuse the existing proven source-map coordinate algorithms while using the
  graph for topology, routing, and provenance.
- [ ] Complete context-wide `source resolve`, `source map`, `source endpoints`,
  and `source show` semantics. HubRPC and CLI projections now expose runtime and
  authored source listing, generated-to-authored mapping, content display,
  grep, safe atomic export, and cache eviction; provider-qualified context-wide
  graph routing and ambiguity reporting remain.
- [ ] Add explicit ambiguity, stale-version, memory-accounting, and cache
  eviction tests.

### Acceptance gate

- [ ] **CLI E2E: `shared_source_resolves_offline_then_to_two_live_endpoints`.**
  Use a fixture containing `src/shared/validation.ts`, a Node server bundle, a
  browser bundle, and source maps:

  ```text
  jsdbg context create --context sources --workspace <fixture>
  jsdbg source resolve src/shared/validation.ts --context sources --output json
  jsdbg source map src/shared/validation.ts:41:1 --to generated --context sources --output json
  jsdbg connection add ws://<node-cdp> --context sources --connection server
  jsdbg connection add ws://<browser-cdp> --context sources --connection browser
  jsdbg connection connect --context sources --connection server
  jsdbg connection connect --context sources --connection browser
  jsdbg source endpoints src/shared/validation.ts --context sources --output json
  ```

  Before connecting, assert exact workspace/authored and both generated snapshot
  identities and projection paths, with zero live endpoints. After connecting,
  assert at least one qualified runtime endpoint from each connection, including
  connection generation, target, attachment, script version, and mapping path.
- [ ] **CLI E2E: `disconnect_removes_only_live_source_endpoints`.** Disconnect
  `browser` and assert `source endpoints` retains the Node endpoint but not the
  browser endpoint. `source resolve` and `source map` must return the same
  immutable snapshot IDs and coordinate candidates as before disconnection.
- [ ] **CLI E2E: `source_detail_hydration_creates_a_new_revision`.** Capture
  state revision `r`, request an initially unloaded runtime source, then query
  both revision `r` and current state. Revision `r` still reports the detail as
  unavailable; current state references a content-addressed artifact and a
  greater revision.
- [ ] **CLI E2E: `ambiguous_source_path_fails_explicitly`.** Add two providers
  with the same friendly path and differing content. Unqualified
  `source resolve` must fail with `ambiguousSource` and both qualified
  candidates; an explicit provider/version selector resolves exactly one.

---

## Milestone 5 — Cross-connection breakpoint reconciliation

**Goal:** Prove the central architecture by binding one durable authored
breakpoint across several runtime connections.

### Steps

- [ ] Expand breakpoint intent with requested source location, enabled state,
  target selector, condition, and log action while preserving disconnected use.
- [ ] Model logical assessment separately from physical runtime applications.
- [ ] Resolve graph paths from requested source snapshots to eligible live
  script endpoints.
- [ ] Reconcile physical CDP breakpoints per attachment and script version.
- [ ] Deduplicate equivalent physical locations without merging distinct
  logical breakpoint intent.
- [ ] Preserve requested location, projection path, corrected runtime location,
  ambiguity, diagnostics, and per-target status.
- [ ] Reconcile on script discovery, source-map hydration, attachment,
  reconnect, script replacement, intent changes, and connection loss.
- [ ] Reject stale installation completions using connection generation,
  attachment incarnation, and script version.
- [ ] Add deterministic race/replay tests for late scripts, corrections,
  reconnects, and competing source candidates.
- [ ] Add a live Node plus Chromium E2E using one shared TypeScript source.

### Acceptance gate

- [ ] **CLI E2E: `one_typescript_breakpoint_binds_node_and_chromium`.** Start the
  shared-source fixture with both runtimes paused before application code:

  ```text
  jsdbg context create --context fullstack --workspace <fixture>
  jsdbg breakpoint set validate src/shared/validation.ts 41 --column 1 --context fullstack
  jsdbg connection add ws://<node-cdp> --context fullstack --connection server
  jsdbg connection add ws://<browser-cdp> --context fullstack --connection browser
  jsdbg connection connect --context fullstack --connection server
  jsdbg connection connect --context fullstack --connection browser
  jsdbg wait fullstack breakpoint --id validate --status fully-bound --timeout 10s
  jsdbg breakpoint show validate --context fullstack --output json
  ```

  Assert one logical specification at the exact requested TypeScript location,
  two peer applications with server/browser provenance, exact projection paths,
  distinct runtime breakpoint identities, and no duplicate physical location
  within one attachment.
- [ ] Trigger the shared function independently through HTTP and page input.
  `jsdbg events fullstack --type debugger.paused --after-revision <r>` must
  report two pauses at the requested authored location, one from each
  connection.
- [ ] **CLI E2E: `breakpoint_survives_partial_disconnect_and_rebinds`.**
  Disconnect `browser`; `breakpoint show` must be `partially-bound`, retain the
  server application, and retain browser applicability as pending. Reconnect
  browser generation 2; status returns to `fully-bound` with a fresh browser
  application and no generation-1 CDP breakpoint identity.
- [ ] **CLI E2E: `pending_and_ambiguous_breakpoints_are_not_silently_widened`.**
  Set one breakpoint before runtimes exist and another against an intentionally
  ambiguous source path. Assert explicit `pending` and `ambiguous` assessments;
  target focus does not alter either breakpoint's selector or choose a source
  candidate.

---

## Milestone 6 — Paused debugger CLI

**Goal:** Validate that immutable state, projections, and ephemeral handles
compose into a useful authored-source debugging session.

### Steps

- [ ] Model pause snapshots, frames, scopes, properties, and remote-object
  handles with explicit epochs.
- [ ] Project every call frame through the context source graph with provenance
  and alternatives.
- [ ] Add target-local pause, continue, step into, step over, and step out.
- [ ] Add stack, scopes, properties, and expression evaluation.
- [ ] Require exactly one selected target for target-local commands.
- [ ] Add explicit interactive focus as a convenience selector only.
- [ ] Implement the configurable short settling policy as observation policy,
  not a second execution mechanism.
- [ ] Reject frame, scope, and object handles after resume, reconnect, pause
  replacement, or attachment loss.
- [ ] Add deterministic tests for pause/resume races and late evaluation
  completions.
- [ ] Add a live authored-source CLI E2E: hit breakpoint, inspect mapped stack,
  evaluate, step, and observe the next pause revision.

### Acceptance gate

- [ ] **CLI E2E: `authored_typescript_debugging_session`.** Using the browser
  fixture and a mapped breakpoint:

  ```text
  jsdbg wait app paused --breakpoint validate --timeout 10s
  jsdbg stack app --output json
  jsdbg scopes app --frame <top-frame> --output json
  jsdbg evaluate app "user.id" --frame <top-frame> --output json
  jsdbg step over app --settle 1s --output json
  jsdbg continue app --settle 1s --output json
  ```

  Assert the pause, top frame, and source excerpt point to the authored
  TypeScript location; scopes include the expected local `user`; evaluation
  returns the fixture value; step-over either returns the next mapped pause in
  the same response or a structured running result; and continue reaches the
  next expected pause. Every result contains context revision, connection,
  generation, target, attachment, execution context where applicable, and
  debugger-local handles—never a raw CDP session ID.
- [ ] **CLI E2E: `target_local_command_rejects_ambiguity`.** Attach two paused
  pages with no focus and invoke `stack`; assert `ambiguousTarget` with both
  qualified candidates. Set focus to one page and assert `stack` resolves it,
  while an explicit selector for the other page overrides focus.
- [ ] **CLI E2E: `ephemeral_handles_fail_after_resume_and_reconnect`.** Save
  frame, scope, and object handles; resume; assert each fails with
  `stalePauseReference`. After reconnect, assert a prior target/attachment handle
  fails with `staleRuntimeReference`, even if CDP reuses its underlying IDs.

---

## Milestone 7 — Automation and reliability surface

**Goal:** Make live failures reproducible and make automation safe under adverse
runtime behavior.

### Steps

- [ ] Add persistent watch-expression intent and pause-qualified results.
- [ ] Add target-scoped raw CDP through the same selectors, routing,
  observation, and generation checks.
- [ ] Integrate service-level user commands, runtime observations, effects, and
  completions with strict recording/replay.
- [ ] Persist reproducible failure bundles containing normalized inputs,
  completions, CDP traffic references, and immutable artifact hashes.
- [ ] Add fault injection for delayed/duplicate completions, reordered events,
  disconnect during commands, malformed payloads, persistence failure, and
  history overflow.
- [ ] Add randomized model-based tests across several contexts, connections,
  targets, and clients.
- [ ] Verify deterministic diagnostics and revisions under replay.

### Acceptance gate

- [ ] **CLI E2E: `recorded_failure_replays_identically_offline`.** Start a
  recording, connect the deterministic faulting CDP fixture, set a breakpoint,
  pause, evaluate, inject connection loss during step-over, then stop recording:

  ```text
  jsdbg record start reliability --output <bundle>
  <run failure workflow>
  jsdbg record stop reliability
  jsdbg replay <bundle> --output jsonl
  ```

  Assert replay opens no CDP sockets and reproduces the same ordered revisions,
  normalized events, terminal connection failure, breakpoint assessments, and
  diagnostic codes after excluding documented nondeterministic metadata such as
  wall-clock timestamps and process IDs.
- [ ] **CLI E2E: `watch_expression_results_follow_pause_epochs`.** Persist a
  watch, hit two pauses, and assert two results qualified by distinct pause
  epochs. The first remote-object result becomes explicitly stale after resume;
  a primitive historical preview remains readable.
- [ ] **CLI E2E: `raw_cdp_uses_normal_routing_and_staleness_rules`.** Send
  `Runtime.evaluate` to one explicit target and assert provenance. An omitted
  target with two matches fails as ambiguous. A delayed response released after
  reconnect fails as stale and is not presented as generation-2 success.
- [ ] Run the CLI failure workflow across the fault matrix: delayed completion,
  duplicate event, reordered event, disconnect during command, malformed
  payload, persistence failure, and history overflow. Every case must terminate
  within its explicit timeout, leave the service responsive, expose structured
  partial failure, and leave zero leaked active runtimes after disconnect.

---

## Milestone 8 — Source exploration and performance

**Goal:** Scale the architecture to large applications while making projected
source knowledge directly useful.

### Steps

- [ ] Add projected source grep over deduplicated content while retaining every
  graph identity and endpoint association.
- [ ] Add managed disk-backed CAS storage behind the existing content interface.
- [ ] Add safe, atomic source export with a provenance manifest.
- [ ] Define explicit memory budgets for core revisions, event history, source
  content, source maps/indexes, and remote-object previews.
- [ ] Add eviction and reload policies that preserve immutable revision
  semantics.
- [ ] Benchmark cold connect, script-heavy sites, source-map hydration,
  breakpoint reconciliation, projected search, and steady-state memory.
- [ ] Add performance regression thresholds to existing test infrastructure.
- [ ] Verify that search schemas remain identical across memory and disk-backed
  implementations.

### Acceptance gate

- [ ] **CLI E2E: `projected_grep_is_identical_before_and_after_disk_eviction`.**
  Index a fixed fixture of 10,000 logical source identities, including duplicate
  content and source-map projections:

  ```text
  jsdbg source grep "validateUser" --context scale --output json
  jsdbg source cache evict --context scale --memory-only
  jsdbg source grep "validateUser" --context scale --output json
  ```

  Assert byte-equivalent normalized match sets before and after eviction:
  logical identity, provider/version, content hash, projection path, location,
  source context, and all live endpoint associations. Physical duplicate content
  is searched once but every graph identity remains in results.
- [ ] **CLI E2E: `source_export_is_safe_atomic_and_reproducible`.** Export the
  graph to an empty directory containing adversarial URLs (`..`, absolute paths,
  query collisions, and duplicate names). Assert no file escapes the directory,
  collisions are deterministic, the manifest resolves every exported file to
  exact source identities/hashes/provenance, and a repeated export has identical
  manifest content with no partially visible generation.
- [ ] **CLI E2E: `large_fixture_stays_within_ratified_budgets`.** On the pinned
  CI runner, connect the 10,000-script fixture, hydrate 500 source maps, install
  100 authored breakpoints, and run projected grep. Assert against versioned
  budgets committed with the benchmark: peak RSS, settled RSS, connect-to-ready,
  p95 source-resolution, p95 reconciliation, and warm-grep latency. Initially
  ratify concrete numbers from five clean baseline runs; later changes may only
  relax them with an explicit plan update and linked evidence.
- [ ] Capture revision `r`, evict all reloadable details, reload one source, and
  assert `state get --revision r` is byte-equivalent before and after while the
  current revision advances and references the same immutable content hash.

---

## Deferred milestones

These should reuse the established primitives rather than introduce parallel
state or lifecycle models:

- [x] Precise coverage values, transforms, comparison, and rendering.
- [x] Heap snapshot capture, progress reporting, constructor grouping, filtering,
  and bounded class/instance rendering.
- [ ] Process launch, endpoint discovery, and PID activation providers.
- [ ] DOM exploration and screenshot artifact conveniences.
- [ ] MCP client integration.
- [ ] VS Code/DAP adapter and unconnected breakpoint integration.
- [ ] Dirty editor-buffer edit projections.
- [ ] TUI over the same observable HubRPC service.

## Recommended execution order

1. Complete Milestones 1–3 without starting breakpoint/source integration.
2. Reassess reducer, observation, and attachment identities after the live
   continuous-target tests.
3. Complete Milestones 4–6 as one end-to-end debugger capability sequence.
4. Harden replay and fault handling in Milestone 7 before adding broad feature
   families.
5. Establish measured budgets in Milestone 8 before coverage, DAP, MCP, or TUI
   substantially increase retained state.

The next task to mark `:running:` should be the first step of Milestone 1, with
the implementation session linked on that task.
