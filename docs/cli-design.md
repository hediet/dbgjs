# JavaScript Debugger CLI Design

## Project and command names

The project and primary Rust crate are `dbgjs`. The npm distribution is
`@hediet/dbgjs`, with matching `@hediet/dbgjs-<platform>` native packages.
The executables are `dbgjs`, `dbgjs-service`, and `dbgjs-tui`; the terminal
client crate is `dbgjs-tui`. The protocol-specific crate remains `cdp-protocol`.

This replaces the prototype names `cdp-client`, `cdp_client`, and `jsdbg`.
Update shell commands, `JSDBG_*` environment variables to `DBGJS_*`, and
VS Code settings and launch configurations from `jsdbg` to `dbgjs`.
There are no old-name aliases.

The renamed tool deliberately starts with fresh state: on Windows it uses
`%LOCALAPPDATA%\dbgjs\service.json`; on Unix it uses
`$XDG_RUNTIME_DIR/dbgjs/service.json` or
`$HOME/.cache/dbgjs/service.json`. `DBGJS_SERVICE_STATE` overrides
the endpoint path. Saved contexts and captures in the old state location are
not migrated or deleted. Stop the old service using the old CLI if it is still
running.

## Status

This document defines the architecture, terminology, and required behavior of a
stateful command-line debugger for JavaScript targets reachable through the
Chrome DevTools Protocol (CDP).

The shared immutable state semantics are defined in
[Debugger Data Model](./debugger-data-model.md). This document derives CLI
workflows and presentation from that model rather than defining a second one.
For operational steps against live VS Code processes, see
[Debugging VS Code Processes with dbgjs](./debugging-vscode-processes.md).

The CLI supports unary commands, ordered result feeds, and an interactive
redrawn daemon-state view. The view uses the same debugger service observation
primitives and state model rather than creating a separate debugger agent, CDP
connection, or client-side state model.

The document is divided into:

1. **Core design**: concepts and behavior that implementations and clients may
   rely on.
2. **Deferred details**: syntax, storage, presentation, and advanced features
   that can be selected or implemented later without changing the core model.

Command examples illustrate the intended experience. Except where explicitly
stated as a requirement, exact command names and flags are not yet fixed.

### Current implementation slice

The first executable vertical slice now validates:

- `dbgjs` discovering or spawning a long-lived `dbgjs-service`;
- generated typed HubRPC requests over authenticated NDJSON on a Windows named
  pipe or Unix domain socket;
- multiple contexts and multiple named connection configurations per context;
- disconnected breakpoint intent;
- atomic restart persistence for durable context intent and connection
  definitions;
- atomic revision-bearing context snapshots;
- activating a configured CDP WebSocket connection and validating it with
  `Browser.getVersion`;
- explicit connection disconnect with WebSocket and mux cancellation;
- continuous target discovery with context-global canonical target IDs and
  retained connection/generation provenance;
- a context-wide immutable capture catalog for coverage, CPU profiles, and heap
  snapshots, including offline queries after disconnect or daemon restart;
- `dbgjs daemon view`, which redraws one selected context or `--all-contexts`
  while observing context and target debugger revisions. When stdout is not a
  terminal it emits one deterministic snapshot and exits.

Strict attachment stealing and selected-page Playwright programs are also
implemented through the same target identities and lifecycle checks.

Selected Electron pages without an upstream browser-context ID receive a stable
context ID at the Playwright proxy boundary. This client-facing identity is used
consistently in target discovery and attachment metadata, without being forwarded
as a real upstream browser context. Genuine upstream context IDs are preserved.
Playwright programs return their value with `return`; console diagnostics go to
stderr so they cannot corrupt the JSON result envelope. Failed child processes
report their exit failure and stderr rather than an incidental empty-JSON error.

### Inspecting stored captures

`dbgjs capture list --context <id>` lists compact catalog entries.
`dbgjs capture show <name> --context <id>` renders the stored capture's default
human-readable view: a coverage tree, CPU profile summary, or heap class tree.
It uses the same output budgets and width trimming as `coverage show`,
`profile show`, and `heap classes`, respectively. Reads work after disconnect
or daemon restart and do not start recording or take a new capture.

Relative selectors (`.`, `.1`, `.2`, ...) resolve across all capture kinds in
the context, once, before rendering the exact stored name. In contrast,
`coverage show .` selects the latest coverage capture, not the latest capture
of any kind.

```console
dbgjs capture show . --context :investigation
dbgjs coverage show click --context :investigation --path-glob "**/contrib/issue/**"
```

Use the kind-specific commands for filtering and output options such as
`--max-lines`; coverage and heap trees also support `--all` and `--no-trim`.
`--no-trim` disables line-width trimming, not tree pruning.
For compatibility, `dbgjs --json capture show <name>` still returns catalog
metadata, as does `capture list` for each entry. Use `--json coverage show`,
`--json profile show`, or `--json heap classes` for structured content.

## CDP over stdio

A durable connection can launch an adapter whose stdin and stdout carry CDP
using the same framing as MCP stdio: each compact UTF-8 JSON message occupies
one line and must not contain embedded newlines. Stdout is protocol-only;
adapter diagnostics belong on stderr.

```console
dbgjs connection add --stdio --connection custom --connect -- \
  ./my-cdp-adapter --foobar
```

The command and all of its arguments follow `--`, so adapter options cannot be
mistaken for dbgjs options. Connection options precede it:

```console
dbgjs connection add --stdio --connection browser \
  --cwd ./adapter --env TOKEN=secret --topology browser --connect -- \
  node ./adapter.mjs --remote production
```

`--topology target` is the default and represents one direct debugger target.
`--topology browser` represents a browser-root CDP endpoint that can discover
multiple targets. The service owns the spawned adapter process and terminates
it when the connection disconnects. Browser-root Playwright programs currently
still require a WebSocket CDP endpoint.

## Relay

`dbgjs context relay --stdio [--context <id>]` and
`dbgjs target relay --stdio [target scope]` expose a context or a single
target as CDP over the same compact newline-delimited JSON framing described
above, so any external CDP consumer (Playwright, Puppeteer, an MCP-style tool)
can drive `dbgjs`-managed targets directly instead of through `dbgjs` commands.

```console
dbgjs context relay --stdio --context :shop
dbgjs target relay --stdio --target page
```

Internally, the service opens a short-lived authenticated loopback WebSocket
endpoint (the same shape as the Playwright proxy endpoint) and the CLI process
bridges it to its own stdin/stdout verbatim; it does not interpret CDP itself.

- **Context relay** exposes a virtual browser root covering every target across
  every connection in the context, using context-global canonical target IDs.
  It minimally supports `Browser.getVersion`, `Target.getTargets`,
  `Target.setDiscoverTargets`, `Target.setAutoAttach`, flattened
  `Target.attachToTarget`/`Target.detachFromTarget`, and mirrors every raw
  target event (not only typed debugger events) to attached sessions.
- **Target relay** exposes exactly one target as a direct CDP root, equivalent
  to connecting straight to that target's own endpoint: every request forwards
  opaquely and every raw event mirrors back, with no `Target.*` domain of its
  own.

Opening either relay takes **exclusive ownership of the target's context**
immediately, before any client connects: ordinary local target debugging
commands (breakpoints, evaluation, stepping, raw CDP, and so on) fail with a
clear error until the relay closes. Relay-internal attachment and forwarding
bypass that guard, existing attachments remain valid once the relay closes,
and attaching a target while relayed can stay lazy. A relay never restarts
the underlying WebSocket, stdio, Electron, Node, or process provider
connection; it only adds a CDP-shaped facade in front of the same live
attachments.

---

# Part I: Core design

## 0. Interaction forms

The CLI distinguishes three user-visible interaction forms:

- **Command:** one invocation sends one input and returns one terminal result.
  A command may wait internally, but it does not publish intermediate results.
- **Feed:** one invocation publishes an ordered sequence of immutable items. A
  feed may be finite or remain open until cancellation. JSON feeds use JSONL.
- **View:** an interactive, continuously redrawn screen over current state. A
  view may accept keyboard input, but redrawing does not create new debugger
  state or a second observation mechanism.

`stream` describes the transport shape used to implement feeds and views, not a
fourth user-facing interaction form. `watch` describes a durable expression or
policy and is not used as a synonym for a feed or view.

Representative spellings are:

```text
dbgjs state get             # command
dbgjs events feed           # feed
dbgjs daemon view           # view
```

Convenience aliases may preserve older `watch` or `follow` spellings, but new
commands should use this terminology.

## 1. Goals

The debugger must:

- Work well from scripts and individual shell invocations.
- Preserve live debugger state between CLI invocations.
- Keep multiple durable debug contexts in one debugger agent.
- Allow a context to exist and remain useful while it has no live connection.
- Connect a context to zero or more CDP endpoints simultaneously.
- Observe targets that appear and disappear behind every endpoint in a context.
- Debug targets from multiple runtimes at the same time.
- Keep attachment separate from selection.
- Support browser, Node.js, and other CDP-compatible JavaScript runtimes.
- Try to activate debugging for an existing process when given a PID.
- Preserve desired state such as breakpoints, watches, and attachment rules
  across target recreation and reconnection.
- Expose every command through machine-readable JSON input and output.
- Expose state changes as queryable and followable events.
- Resolve authored and generated sources through source maps.
- Search projected sources and optionally materialize them to disk.
- Record, compare, and render precise JavaScript coverage.
- Record, source-map, render, and export sampled JavaScript CPU profiles.
- Explore and interact with page DOM state through raw CDP requests.
- Capture full-page, viewport, and DOM-element screenshots.
- Produce useful human-readable terminal output, including highlighted source
  context where applicable.

## 2. Non-goals of the core model

The core design does not require:

- A specific command-line parsing library.
- A specific terminal UI or syntax-highlighting library.
- A specific on-disk database or cache layout.
- Universal success when activating debugging in an arbitrary process.
- Treating every CDP target relationship as a strict tree.
- Exposing raw CDP session IDs as normal user-facing identifiers.
- Keeping ephemeral CDP handles valid after resume, reconnect, or target
  replacement.

## 3. Architectural overview

A CLI invocation must not own the CDP connection. CDP produces asynchronous
events and uses ephemeral identifiers that must survive between commands.

The architecture is:

```text
Non-interactive CLI ─┐
                     ├─ local RPC ─> Debugger agent
Future TUI ──────────┘                    │
                              ┌───────────┴───────────┐
                        Debug context A        Debug context B
                        ├─ connection 1        └─ disconnected
                        ├─ connection 2
                        └─ shared intent,
                           observations, and source graph
```

The long-lived debugger agent owns multiple durable contexts. Each context owns:

- Zero or more CDP WebSocket connections.
- Target discovery across those connections.
- Live target attachments and flattened CDP sessions.
- Context-wide event ingestion, ordering, and observation state.
- Pause state, call frames, scopes, and remote objects.
- Shared desired breakpoints, watch expressions, policies, and attachment rules.
- One shared source graph for discovery, projection, indexing, and caching.
- Per-connection generations and one context-wide state revision.

The CLI is a stateless RPC client except for ordinary user configuration. A TUI
must be another client of the same agent.

## 4. Terminology

### 4.1 Agent

The long-lived local process or service that owns all debug contexts and CDP
transports.

### 4.2 Debug context

A durable, user-facing scope that groups debugger intent and observations across
zero or more runtime connections. A path context uses its lexically normalized
absolute project path as identity. A named non-path context uses a normalized ID
entered with `:<id>` expression syntax.

A context owns:

- Its connection set and connection/target focus.
- Persistent attachment and target selectors.
- Breakpoint and watch specifications.
- Pause-on-exception and other debugger policies.
- Observation state, event history, and a context revision.
- Coverage recordings and captured artifacts.
- One provider-qualified, versioned source graph and its cache policy.

A context can be created before any runtime is available, disconnected from all
runtimes, and reconnected later without losing this state. For path contexts,
the normalized cwd or workspace folder is the context identity.

```text
--context :incident-42
```

Explicit context selection always wins. Otherwise the nearest explicit cwd
binding wins over the nearest registered path context at the cwd or a parent.
There is no global or sole-context fallback.

### 4.3 Connection

A durable child of a debug context describing one runtime transport endpoint or
launch recipe. Examples within context `shop` are `server` and `browser`.

A connection owns:

- Its endpoint or launch recipe.
- Its current transport state.
- Its transport generation.
- The observable target graph behind the endpoint.
- Live attachments.
- Live script endpoints contributed to the context source graph.

A browser-level endpoint can expose many targets. A direct page or Node endpoint
usually exposes one. A connection is not the primary user scope: selecting one
narrows runtime routing or focus within an already selected context.

### 4.4 Endpoint

The HTTP discovery address or WebSocket address used to establish the one CDP
transport owned by a connection.

Endpoint forms can include:

- An HTTP address exposing `/json/version` or `/json/list`.
- A browser-level WebSocket address.
- A direct target WebSocket address.
- An endpoint discovered from a launched process or PID.

### 4.5 Target

A runtime entity reported by CDP, such as:

- A page.
- A Node.js runtime.
- An out-of-process iframe.
- A dedicated or shared worker.
- A service worker.
- A worklet or another runtime-specific target type.

A target can be observed without being attached. While live, its canonical
target ID is unique across the entire context. Discovery retains the owning
connection and its generation as provenance. If two connections report the same
canonical ID, discovery rejects the collision explicitly rather than silently
qualifying, replacing, or selecting either target. IDs remain live protocol
identifiers and must not be treated as durable identities across all restarts.
Synthetic Node/process roots use `$node-root:<connection-id>` as their canonical
ID, so independent direct runtimes can coexist. `node` remains a friendly
selector and reports qualified candidates when more than one root matches.

### 4.6 Target graph

The changing set of targets and their known relationships on one connection.
The context exposes the union while retaining connection provenance.

Targets do not form one universally correct tree. The graph may include:

- Attachment-parent relationships.
- Page opener relationships.
- Browser-context membership.
- Worker or frame relationships.
- Other runtime-specific related-target relationships.

Text clients may render a best-effort tree, but JSON must preserve the explicit
relationship types rather than flattening them into one `parent` field.

### 4.7 Attachment

The debugger's active relationship with a target.

For a browser-level endpoint, an attachment normally corresponds to a flattened
CDP session created with `Target.attachToTarget`. For a direct target endpoint,
the root CDP transport acts as the attachment.

One connection may have many simultaneous attachments. The agent should keep at
most one attachment of its own to a given live target unless a protocol-specific
reason requires otherwise.

An attachment has a debugger-local attachment ID. The raw CDP session ID remains
an implementation detail unless explicitly requested for diagnostics.

### 4.8 Attachment rule

Persistent desired state describing which current and future targets should be
attached. Rules are context-owned and may include a connection selector.

For example, a rule may match:

- Pages with a particular URL.
- Workers related to a selected page.
- All targets of a set of types.

When a matching target disappears, its live attachment disappears. The rule
remains and can attach a future matching target.

### 4.9 Connection and target focus

The connection and target used by target-local interactive commands when a
request does not provide an explicit selector.

Focus is separate from attachment and durable intent:

- Many connections and targets can be attached.
- Changing focus does not detach anything.
- Pause, step, evaluation, DOM, screenshot, and raw target CDP commands may use
  unambiguous focus.
- Creating a persistent breakpoint without an explicit target selector does not
  inherit focus; it defaults to all eligible targets in the context.

Focus may be pinned to a live target or follow a selector. It can be unresolved
while disconnected. If target-local focus is ambiguous, the command fails
rather than silently choosing one match.

### 4.10 Execution context

A JavaScript realm within an attached target. One target can contain multiple
execution contexts, including isolated worlds.

### 4.11 Pause snapshot

The ephemeral state captured for one pause:

- Pause reason.
- Call frames.
- Scopes.
- Exception information.
- Remote object handles.

A pause snapshot becomes invalid when its target resumes, disconnects, or moves
to a new connection generation.

### 4.12 Source graph

The context-owned graph that connects immutable source snapshots and runtime
script endpoints through typed projection edges.

Each source snapshot has provider-qualified identity, a content identity, and a
version. Providers can represent runtime scripts, source-map entries, workspace
files, formatted views, editor buffers, or future source systems. Typed edges
include:

- Identity/equivalence projections.
- Source-map projections.
- Formatting projections.
- Edit/version projections.
- Offset/location projections.

Every connection contributes its live CDP script endpoints to this one graph.
An endpoint points to an exact generated snapshot and target/attachment; it is
removed on disconnect without deleting snapshots or projections that remain
useful offline. Graph traversal preserves provenance, direction, mapping
quality, ambiguity, and version boundaries.

### 4.13 Coverage recording

The active process that collects precise JavaScript coverage for a context.
A recording accumulates coverage across its applicable current and future
attachments. It is transient state and does not itself have a coverage object
ID.

At most one coverage recording is active per context. Starting a recording
does not create an empty coverage object.

### 4.14 Coverage object

An immutable cumulative snapshot captured from the active coverage recording.

A coverage object contains execution counts over canonical script, function, and
block ranges, together with the source identities and projection information
needed to render those ranges. Coverage objects remain useful after recording
stops and can be compared through exclusion.

### 4.15 CPU profile recording

The active target-scoped process that periodically samples the V8 call stack.
The optional sampling interval is expressed as a duration and must resolve to a
positive whole number of microseconds. A bare numeric CLI value means
milliseconds; omitting the option leaves the runtime default unchanged.

CDP returns the sample stream only when recording stops. CPU profiling therefore
does not have coverage-style intermediate capture or exclusion operations.

### 4.16 CPU profile object

An immutable profile produced when a CPU profile recording stops. It preserves
the raw node graph, sample node IDs, actual time deltas, timestamps, sampling
interval, and target provenance needed for lossless DevTools `.cpuprofile`
export. Source-mapped function and file summaries are derived views of this raw
value. Self time attributes each sample to its leaf frame; total time attributes
it to that frame and its ancestors.

CDP time deltas are signed timestamp differences, not sample durations. V8 can
emit samples out of timestamp order. Storage and export retain the original
sample order and signed deltas, including negative values. Analysis reconstructs
timestamps relative to the profile start, stably sorts sample/timestamp pairs,
and attributes each interval since the preceding timestamp (or profile start)
to its sample. Equal timestamps retain all samples with zero-length intervals
after the first. The sampled duration ends at the latest sample; the unsampled
tail to the profile end is not attributed. Ordered profiles keep their existing
attribution. Invalid array lengths or timestamp offsets outside the supported
nonnegative microsecond range are reported rather than clamped.

### 4.16a Stored capture catalog

Coverage, CPU-profile, and heap-snapshot names share one namespace per context.
A name is reserved atomically before capture output is written and cannot be
replaced by another capture kind or target while cataloged. The service catalog
maps `(context identity, capture name)` to the capture kind, canonical target
ID, owning connection and generation, and an opaque immutable storage ID.
Concurrent losers fail before receiving a storage path. Heap staging and final
paths are unique to that reservation.

Heap chunk and progress notifications are processed in CDP wire order before
the `takeHeapSnapshot` response is delivered to capture finalization. A progress
event reporting 100% is not stream completion: serialization chunks can follow
it. Finalization waits for all preceding chunk writes, flushes and synchronizes
the file, and only then publishes the capture. Chunk decoding or write failures
fail the capture instead of publishing a truncated snapshot.

If a completed capture cannot be added to the durable catalog, its reservation
retains the completed payload in memory. Repeating the same capture request
promotes that payload into the catalog without running the capture again;
`capture delete` explicitly discards the retained payload and its heap storage.

An omitted name generates a fresh immutable ID. Names such as `.` and `.1` are
read selectors, not writable capture names. They select the latest successfully
published capture of the requested kind in the context; `.2` selects the previous
one. Named and automatically named captures share the same persisted publication
history across targets. An explicit target or connection filter is applied before
relative selection; a selected live target does not implicitly scope stored reads.
The returned durable ID remains usable after a relative selector moves.

Capture lookup is context-scoped, not connection- or target-scoped. Catalog
listing, metadata lookup, coverage rendering, CPU-profile rendering/export, and
heap-class queries therefore continue to work with every connection
disconnected and after a daemon restart. Registration validates that the
capturing target still belongs to the recorded connection generation, so a
capture completing after reconnect cannot be attributed to the replacement
target.

Heap captures retain a mapping bundle in the durable catalog: generated script
URLs, CDP hashes and source, source-map URL/content or load diagnostics, connection
generation, execution-context auxiliary data, and owning frame IDs. Live and
stored class queries share the same pure projection over captured inputs; stored
analysis does not consult the current target. Per-script mapping availability is
explicit, including unavailable metadata for older captures.

`heap supply-map <capture> <script-id> <captured-script-hash> <map-file>` adds or
replaces only that script's mapping bundle, leaving the heap payload immutable.
It validates the captured hash, map structure, and any generated `file` name.
The caller must supply a map from the captured build; a source map does not
intrinsically prove which generated hash it belongs to. `heap classes --no-cache`
is rejected rather than silently ignored.

`capture delete` removes immutable heap storage before persisting catalog
removal. Filesystem failures are explicit and retain the catalog entry; retries
accept already-absent files, including after catalog persistence fails following
a successful file deletion. Context deletion persists context and catalog
removal before cleaning its heap storage, so a persistence failure leaves those
files untouched.

### 4.17 Breakpoint specification

Persistent desired state describing:

- An authored or generated location.
- A context-relative target selector, defaulting to all eligible targets.
- An optional condition, log expression, or enabled state.

One specification can have zero or more live CDP breakpoint resolutions across
attachments. Raw CDP breakpoint IDs are ephemeral implementation details.

### 4.18 Watch expression

Persistent desired state describing an expression to evaluate in an applicable
paused target and frame.

A watch has:

- A stable debugger-local ID.
- An expression and optional display name.
- A target selector.
- A frame-selection policy.
- An enabled state.
- A latest result or explicit unavailable/error state.

Watch results are tied to a pause snapshot and must not masquerade as current
after the target resumes.

### 4.19 Event

An ordered, structured record of a debugger state change or noteworthy protocol
observation.

Events are ordered in context scope and carry connection, target, and attachment
provenance where relevant.

### 4.20 Generation and revision

Each connection has a **generation**, incremented whenever a new underlying CDP
transport replaces the previous one. Each context has a monotonically
increasing **revision** for all observable state changes, including one atomic
change that affects multiple connections.

Ephemeral handles include context identity, connection identity and generation,
and, where relevant, the pause generation. Stale handles must fail explicitly.

## 5. State hierarchy

The conceptual state hierarchy is:

```text
Agent
└─ Debug context
   ├─ Connections (zero or more)
   │  ├─ Endpoint/launch recipe and connection generation
   │  ├─ Target graph
   │  │  ├─ Browser contexts
   │  │  └─ Targets and typed relationships
   │  └─ Attachments
   │     ├─ Execution contexts
   │     ├─ Live script endpoints
   │     └─ Optional pause snapshot
   │        ├─ Call frames
   │        ├─ Scopes
   │        └─ Remote-object handles
   ├─ Connection and target focus
   ├─ Attachment rules and debugger policies
   ├─ Breakpoint specifications and live resolutions
   ├─ Watch expressions and pause-scoped results
   ├─ Optional active coverage recording
   ├─ Immutable coverage objects
   ├─ Shared source graph and content index
   ├─ Observation state and event history
   └─ Context revision
```

Each target graph is not subordinate to attachment: unattached targets remain
observable members of their connection's graph. Context-owned objects refer to
live facts using fully qualified `(connection, target, attachment)` provenance.

## 6. Persistent and ephemeral state

### 6.1 Persistent state and artifacts

The agent should preserve:

- Context ID and optional workspace hints.
- Connection IDs and endpoint or launch recipes.
- Persistent target selectors and attachment rules.
- Connection and target focus.
- Breakpoint specifications.
- Watch expressions.
- The immutable stored-capture catalog and coverage/CPU-profile/heap payloads.
- Pause-on-exception policy.
- Source-resolution and path-mapping policy.
- Source cache or materialization policy.
- Offline source snapshots and typed projection metadata according to retention
  policy.
- Relevant recording and event-history settings.

### 6.2 Ephemeral state

The agent must not blindly restore:

- CDP session IDs.
- CDP request IDs.
- Target IDs that no longer exist.
- Script IDs.
- Execution-context IDs.
- Call-frame IDs.
- Remote-object IDs.
- Raw CDP breakpoint IDs.
- Pause snapshots and watch results from an earlier pause.
- Active V8 coverage counters.

After reconnecting any connection, the agent reconstructs that connection's live
state by rediscovering targets, applying context attachment rules, enabling
debugger domains, contributing scripts to the context source graph, and resolving
shared desired breakpoints again. Other connections continue unaffected.

## 7. Context, connection, and process lifecycle

### 7.1 Create or select a context

A path context can be created without connecting:

```text
cd <path>
dbgjs context create .
dbgjs status
```

Paths are resolved lexically, lowercased, and made absolute without requiring
filesystem existence or resolving symlinks. A named non-path context uses
explicit colon syntax, for example `dbgjs context create :incident-42`.
Resolution precedence is:

1. Resolve an explicit `--context <path|:id>` expression.
2. Use the nearest cwd binding created by `--set`.
3. Use the nearest registered path context at the cwd or one of its parents.
4. Fail with `context_required`.

Bindings always take precedence over automatic path matching. A stale binding
is an explicit error, and an unrelated context is never selected merely because
it is the only registered context.

### 7.2 Connect to an endpoint

Conceptually:

```text
dbgjs --context shop connection connect browser http://127.0.0.1:9222
```

For an HTTP discovery endpoint, the agent discovers and connects to the relevant
WebSocket. It must distinguish browser-level and direct-target endpoints.

For a browser endpoint, connecting starts target observation. If exactly one
ordinary page is available, it may become interactive focus and related targets
are attached automatically. If several pages are plausible, the connection
request needs a target selector or returns a concise ambiguity result. This may
establish interactive focus, but does not change breakpoint scope.

For a direct target endpoint, the root target is attached and focused
automatically.

Adding a connection does not replace existing context connections unless an
explicit replacement operation names that connection.

### 7.3 Launch a process

Launching creates or explicitly replaces one connection using a durable launch
recipe.

Examples:

```text
dbgjs --context shop connection launch server node --break -- app.js
dbgjs --context shop connection launch browser chrome --url http://localhost:5173/
```

The agent owns the launched process unless the launch request explicitly selects
a different lifecycle policy.

When launching Node with an initial break, the agent must not resume the runtime
before an explicit user command.

### 7.4 Connect to a PID

The debugger should try to discover or activate debugging for an existing
process:

```text
dbgjs --context shop connection connect server --pid 1234
```

Activation is implemented by runtime- and platform-specific providers. A
provider may:

1. Inspect process metadata.
2. Discover an already active inspector endpoint.
3. Find runtime endpoint files or listening ports.
4. Ask the runtime to activate its inspector using a supported mechanism.
5. Discover and connect to the new endpoint.

Safe, runtime-supported mechanisms should be attempted by default. Invasive
injection or elevation must require explicit consent and provide clear
diagnostics. Failure to activate debugging is an expected, structured error, not
a reason to omit PID attachment from the design.

### 7.5 Disconnect, stop, and delete

The model distinguishes:

- **Disconnect connection**: close one live CDP transport. Remove only that
  connection's live targets, attachments, script endpoints, pause snapshots,
  and breakpoint bindings.
- **Stop**: stop a launched process or active connection according to its
  lifecycle policy.
- **Delete connection**: remove its persisted endpoint/launch recipe and live
  facts, but retain context-owned intent and source snapshots subject to ordinary
  graph-retention policy.
- **Delete context**: explicitly remove the complete durable user scope,
  including intent and offline artifacts.

Disconnecting `browser` from a context containing `server` therefore leaves the
context, shared breakpoint/watch intent, offline graph data, the server
connection, and server breakpoint bindings intact.

## 8. Target observation, attachment, and selection

### 8.1 Observation

While connected, the agent continuously observes target creation, updates, and
destruction on every connection. The context publishes a revision-consistent
combined observation while preserving the connection of origin.

### 8.2 Multiple attachments

The agent must support simultaneous attachments to pages, workers, OOPIFs, and
other targets. Each event and result identifies its target and attachment.

After selecting a primary page or process target on a connection, the baseline
attachment policy
automatically attaches related current and future workers, OOPIFs, and other
runtime targets. This gives one connection visibility into their interaction
without requiring routine attachment commands. Explicit attachment rules remain
available to include, exclude, or override relationships.

Target-local commands operate on one target unless explicitly given a
multi-target mode. Commands must not accidentally pause, resume, or step all
attachments.

### 8.3 Context, connection, and target resolution

Context selection and target resolution are separate operations. A
`--target <target-id-or-selector>` scope is resolved only inside the selected
context. An exact canonical target ID resolves context-wide without requiring
`--connection`; exact ID equality takes precedence over friendly matching.

Target listings print copyable `connection/target@generation` selectors,
including for nested iframe targets; indentation describes hierarchy, not a
relative selector grammar. Commands also accept `connection/target` without a
generation. Qualified matches take precedence over unqualified IDs and friendly
matches. A generation-qualified selector does not follow a reconnected target.
Its generation remains part of the RPC selector until the service atomically
resolves it and selects the debugger handle; it is not discarded by CLI lookup.
Connection qualification is also preserved for generation-less selectors, so a
nested ID beginning with its own connection name cannot resolve to a different
target on the service's second lookup. `target list --target` uses the same
identity precedence as show, attach, eval, log, and raw CDP requests. Stale
qualified identities never fall back to matching titles or URLs. An explicit
selector with no discovered match fails with an error that acknowledges that
discovery may be incomplete rather than presenting an empty success.
An explicit `--connection` constrains resolution before ambiguity checking.

Friendly title, URL, and substring matching remains a convenience. It must
resolve exactly one target. Ambiguity is never hidden: an error lists every
candidate qualified as `connection/target@generation` with its type, title, and
URL. Set-valued selectors explicitly say that multiple matches are allowed; a
command that requires one target fails on zero or multiple matches.

The connection remains an internal lifecycle and provenance dimension: it owns
the transport and reconnect generation, but not a separate target-ID namespace.
It is not normally a third interactive selection step. The CLI infers it from
the canonical target and exposes `--connection <id>` as an optional provenance
constraint or when the connection itself is the command's subject.

Interactive focus can be changed explicitly:

```text
dbgjs focus set --connection browser --target page-1
dbgjs focus set --connection browser --target worker-checkout
```

Changing focus affects later target-local requests only. It does not alter
requests that carry selectors, attachment rules, durable breakpoint scope, or
the target associated with existing ephemeral handles.

Resolution depends on command semantics:

- Target-local interactive commands (`eval`, `pause`, stepping, stack, DOM,
  screenshot, and target-scoped raw CDP) require exactly one target and may use
  focus when their selector is omitted.
- Set-valued operations such as event queries, source search, coverage, and
  breakpoint binding may intentionally resolve zero, one, or many targets.
- Durable breakpoint creation with no target selector means all current and
  future eligible targets in the context, not the focused connection or target.
- Source resolution starts from source identity and graph policy; it does not
  require or imply target selection. A target filter can constrain live
  applicability without changing which graph node a source name denotes.

The resolved connection, target, and attachment are included in machine-readable results.
Human-readable results for evaluation, pause, stack, breakpoint, watch, DOM,
CDP, and screenshot operations identify the target whenever confusion with
another attached target is plausible.

A target can contain several execution contexts. Evaluation uses the target's
default execution context unless an explicit realm is supplied. Same-process
iframe or isolated-world evaluation may therefore additionally require
`--execution-context`;
workers and OOPIFs normally have distinct targets. Evaluation results always
identify connection, target, attachment, and execution context.

### 8.4 Following future targets

Attachment, focus, and durable intent selectors may follow future targets. A
selector that matches multiple live targets becomes ambiguous unless it
explicitly permits a set.

The agent must never silently choose the first target from an ambiguous match.

## 9. Debugger functionality

The core debugger service must model and eventually expose:

- Pause and resume.
- Step over, into, and out.
- Pause-on-exception policy.
- Call stacks and frame selection.
- Scopes and properties.
- Expression evaluation.
- Breakpoint creation, update, enable/disable, removal, and resolution.
- Script and source discovery.
- Target and attachment lifecycle.
- Raw target-scoped CDP requests.
- DOM and screenshot capture for page-like targets.

Pause-on-exception, blackboxing, and similar durable policies are context-owned.
They may carry connection/target selectors, but equivalent policy intent is not
duplicated merely because several runtimes are connected.

Commands that use call frames, scopes, or remote objects must reject handles
from another generation or pause snapshot.

Expression evaluation never searches other targets implicitly. The result
identifies the selected target and execution context, including evaluation
errors such as a missing global. When useful, diagnostics may point out that
other page, iframe, or worker targets exist, but they must not evaluate the
expression elsewhere because evaluation may have side effects.

### 9.1 Execution command completion

Execution commands combine the state mutation with a short observation window.
For example, `step over`:

1. Resolves one paused target from an explicit selector or interactive focus and
   sends the step command to it.
2. Observes that target for a subsequent pause for a default settling period,
   initially around 330 milliseconds.
3. If it pauses in that period, returns the new pause reason, location, source
   excerpt, and watch results in the same command response.
4. Otherwise, returns that the target is running. A later pause remains
   observable through events or `wait`.

`step into`, `step out`, `continue`, run-to-location, and similar execution
commands follow the same pattern. A continue that quickly hits a breakpoint
therefore prints the breakpoint and new source position directly.

The internal resumed state and normalized resume event still exist. Direct CLI
output and a TUI may suppress a transient running presentation when the target
pauses within the settling period, avoiding visible running/paused flicker.

The settling duration is policy, not a second execution mechanism. It may be
configured or disabled per request. Explicit `wait` remains useful for
long-running asynchronous operations and automation that starts an action in
another process.

### 9.2 Raw CDP, DOM exploration, and screenshots

The target-scoped raw CDP escape hatch is core functionality:

```text
dbgjs cdp Runtime.evaluate --params <json> [--target <target>]
dbgjs cdp DOM.getDocument --params <json> [--target <target>]
dbgjs cdp Page.captureScreenshot --params <json> [--target <target>]
```

It uses the same target selection, session routing, JSON contract, event
observation, recording, and generation checks as higher-level debugger commands.
It does not bypass the agent's ownership of the transport.

Raw CDP makes unsupported workflows immediately available, including inspecting
DOM nodes, reading element state, dispatching input, and invoking a button click.
A raw request that schedules execution or input can opt into the same short
settling policy as an execution command, allowing a resulting breakpoint to be
returned directly without imposing latency on ordinary DOM queries.
If a raw request causes the target to pause before its protocol response can
complete, the command reports the pause and retains a debugger-local pending
request handle rather than hanging indefinitely.

Screenshot capture is also a first-class convenience because binary output,
viewport metrics, element clipping, and artifact storage are cumbersome to
compose manually:

```text
dbgjs screenshot capture
dbgjs screenshot capture --output page.png
dbgjs screenshot capture --selector "#checkout" --output checkout.png
```

Without `--output`, dbgjs stores the image in its temporary screenshot
directory and returns the generated path.

It supports viewport, full-page, and DOM-element capture where the selected
target has the required page capabilities. A worker or Node target produces an
explicit capability error that names the selected target and available
page-like targets.

JSON screenshot results include target identity, dimensions, media type, and an
artifact reference or explicitly requested encoded bytes. Binary image data
must not corrupt a JSON or JSON Lines stream.

## 10. Breakpoints and source context

Breakpoint specifications are context-owned desired state. Target selection and
source resolution are independent:

- The requested location resolves through the context source graph, possibly
  while every connection is offline.
- The target selector controls eligible live binding endpoints.
- If no selector is supplied, persistent breakpoint scope is all current and
  future eligible targets in the selected context.
- An explicit selector may constrain by connection, target properties, runtime
  capabilities, or any intentional combination.

Interactive focus never silently becomes breakpoint scope. A focused browser
page does not prevent an unqualified breakpoint from binding in a Node target.
Conversely, `--target browser/page-1` is an explicit restriction, not a source
resolution hint.

Breakpoint output must distinguish:

- Requested logical location.
- Exact source snapshot and graph projection path used.
- Resolution status.
- Each live generated/runtime location.
- Mapping ambiguity or diagnostics.
- The target selector and qualified connection/target/attachment on which each
  binding is currently resolved.

One specification may bind simultaneously through different generated
projections. For example, a breakpoint in shared `src/validation.ts` can map to
a server bundle in a Node target on connection `server` and a frontend bundle in
a Chrome page on connection `browser`. These are peer live bindings of one
durable breakpoint, not copied specifications.

If no eligible target currently loads a resolvable generated source, breakpoint
creation still succeeds as pending context intent. Diagnostics separately report
source-graph resolution and target eligibility. An explicitly single-target
breakpoint reports when the source is known only on other targets, but never
silently broadens its selector.

When one connection disconnects, only bindings on that connection are removed.
The specification, its offline source resolution, and bindings on other
connections remain. Reconnection can create new bindings from the same intent.

Human-readable `breakpoint show` output must include source code around the
requested location when content is available. Interactive terminal output should
support syntax highlighting, line numbers, an active-line marker, and a column
or range marker.

Machine-readable output must contain structured source lines and highlight
ranges without ANSI escape sequences.

## 11. Watch expressions

Watch expressions are context-owned persistent intent and are evaluated whenever
their target selector and frame policy match a pause. Unlike breakpoints, an
omitted watch target policy may deliberately mean focused-pause evaluation for
interactive convenience; the chosen default must be explicit in the watch
specification and JSON, not inferred anew from later focus changes.

Conceptual commands:

```text
dbgjs watch add "user.id" --name user-id
dbgjs watch list
dbgjs watch show watch-1
dbgjs watch enable watch-1
dbgjs watch disable watch-1
dbgjs watch remove watch-1
dbgjs watch evaluate
```

On each applicable pause:

1. The pause snapshot is established.
2. The agent resolves each enabled watch's target and frame.
3. The expression is evaluated.
4. The latest result is stored with the pause generation.
5. A watch-result event is emitted.

A watch result can be:

- A primitive value.
- A remote-object handle tied to the pause snapshot.
- An evaluation exception.
- Unavailable because no target or frame matched.
- Stale because the target resumed.

Automatic evaluation must have an explicit side-effect policy. The agent must
not claim that an expression is side-effect-free when the runtime cannot enforce
that property.

## 12. Events, waiting, and continuous observation

Events are a core API, not terminal logging.

### 12.1 Normalized events

The agent translates important CDP and debugger state changes from all context
connections into a stable event model. Event categories include:

- Connection connected, disconnected, and reconnected.
- Target created, changed, and destroyed.
- Target attached and detached.
- Execution context created and destroyed.
- Script discovered and source indexed.
- Breakpoint resolved or unresolved.
- Target paused and resumed.
- Watch evaluated, failed, or became stale.
- Console output and runtime exceptions.
- Process started and exited.
- Source or protocol diagnostics.

Each event includes:

- Context ID and context revision.
- Connection ID where applicable.
- Connection generation where applicable.
- Event type.
- Timestamp.
- Relevant target and attachment IDs.
- A structured payload.

Raw CDP events may be available as an opt-in diagnostic stream, but they do not
replace normalized events.

### 12.2 Event history

The agent retains a bounded or persisted context event history so state changes
that occur between CLI invocations are not lost. One revision may describe an
atomic state transition with facts from several connections; consumers never
observe a partially applied revision.

Conceptual queries:

```text
dbgjs events
dbgjs events --since-revision 120
dbgjs events --type debugger.paused,target.created
dbgjs events --target t-worker
```

Clients should be able to resume consumption from a known revision. If requested
history is no longer available, the agent returns an explicit history-gap error
and a current state snapshot or revision from which to continue.

### 12.3 Following events

Continuous event output uses a streaming response:

```text
dbgjs events --follow
dbgjs events --follow --type debugger.paused,console.message
```

JSON streaming uses one JSON object per line. Cancellation must unsubscribe the
client without stopping target observation or the debugger agent.

### 12.4 Waiting for a condition

`wait` is a one-shot automation primitive, distinct from viewing an event
stream:

```text
dbgjs wait paused --timeout 30s
dbgjs wait target-created --type worker
dbgjs wait breakpoint-resolved --breakpoint bp-3
dbgjs wait process-exited
```

To avoid races, a wait request atomically:

1. Evaluates whether the condition is already true in current state.
2. Registers for future matching revisions if it is not.

A timeout is a structured result or error with a distinct exit status.

### 12.5 Getting and watching state

Continuous state watching is distinct from event history and watch expressions.
It reruns a query when relevant context revisions occur:

```text
dbgjs state get
dbgjs state watch
dbgjs status --watch
dbgjs stack --watch
dbgjs watch list --follow
```

This is a client convenience built on context snapshots plus revisioned events.
A get-and-watch subscription atomically returns a snapshot at revision `r` and
then changes after `r`; no connection event can be lost in the handoff. Filters
may reduce output to one connection or target, but revision and snapshot
consistency remain context-wide. Without a filter, `state get` and `state watch`
cover the complete selected context, including disconnected connection records,
all live connections, shared intent, observation state, and source graph
metadata. Exact CLI spellings for continuous state views may be selected later.

## 13. Source graph, projection, search, and materialization

### 13.1 Provider-qualified snapshots and live endpoints

All source operations address the selected context's one source graph. A source
snapshot identity is at least `(provider, provider-key, version)` and has an
immutable content identity. Human-friendly paths are selectors, not globally
unique source identities.

Providers include runtime CDP scripts, source-map authored entries, workspace
files, formatted content, editor revisions, and future integrations. Every live
script discovered through every connection contributes an endpoint with:

- Qualified connection, target, attachment, script, and generation provenance.
- The exact generated source snapshot.
- Source-map or other projection edges discovered from that script.
- Runtime location and breakpoint-binding capabilities.

Disconnect removes these live endpoints and their runtime-only facts. It does
not remove source snapshots, workspace nodes, mappings, search indexes, or
policy that remain valid without that connection.

### 13.2 Typed projections and resolution

The graph must support:

- Runtime-generated sources.
- Authored sources from source maps.
- Workspace candidates.
- Formatted fallback sources.
- Provider-qualified and versioned source snapshots.
- Forward and reverse traversal through identity, source-map, formatting,
  edit/version, and offset/location projections.
- Multiple candidates and explicit provenance.

Source resolution returns graph nodes and candidate paths before considering
where they are live. Target applicability is a separate query over runtime
endpoints. Commands can therefore:

```text
dbgjs source resolve src/shared/validation.ts
dbgjs source map src/shared/validation.ts:41 --to generated
dbgjs source endpoints src/shared/validation.ts --connection browser
```

The first two can work while disconnected. The last intentionally asks which
live endpoints can consume the resolved source. If a path names multiple
provider/version candidates, the command reports ambiguity and selectors rather
than choosing by current target focus.

Locations shown to users are one-based. CDP's zero-based locations are converted
at the protocol boundary.

`source resolve` requires an exact canonical URI (as printed by `source list`).
An unmatched abbreviation reports canonical substring candidates and explicitly
labels multiple candidates as ambiguous; only an empty inventory reports that
no sources are observed.

### 13.3 Automatic formatting

Formatting is a derived source projection and never mutates runtime content.
Each context owns a default `off`, `auto`, or `on` mode plus ordered rules that
may match canonical target IDs and source URLs with glob patterns. Rules are
evaluated in displayed order and the last matching rule wins:

```text
dbgjs source formatting set auto
dbgjs source formatting rule add --mode off --url "**/vendor/**"
dbgjs source formatting rule add --mode on --target "page-*" --url "**/*.min.js"
dbgjs source formatting get
```

`auto` deterministically recognizes conventional `.min.js` names and
high-density source layout. The formatter is parser-based; invalid JavaScript
remains available as original source with an explicit formatting diagnostic.
Formatted sources retain a UTF-16-aware bidirectional projection to runtime
locations.

Source display and search use the effective context policy by default. A
one-command override does not change that policy:

```text
dbgjs source show app.js --view original
dbgjs source show app.js --view formatted
dbgjs source grep checkout --view formatted
```

There is intentionally no `--view policy`; omitting `--view` is the policy
behavior.

On first access, a formatted URI acquires the original script before selecting
its formatting projection. Minified scripts do not need source maps to be
pretty-printed.

### 13.4 Source search

Searching logical/projected sources is a first-class operation:

Search acquires matching metadata-only runtime scripts before scanning them.
Map-bearing scripts are also acquired to discover matching authored filenames,
even when their generated bundle URL does not match `--path`. Acquisition shares
the source-show/tree path, observes search deadlines and cancellation, and keeps
individual fetch failures from preventing other sources from being searched.
JSON results include `skipped` entries with source identity and reason; human
output lists the same diagnostics after the searched/skipped counts.

Cancelling a source-map read closes its CDP stream without waiting for the read
response. If cancellation happens before the resource-load response, bounded
response tracking closes the stream when its handle arrives. At most four
resource loads per target remain outstanding; further loads fail explicitly
until a response or disconnect releases capacity. Load/read/close waits time out
after 30 seconds, and abandoned-stream close dispatch has a five-second bound.
No background task waits indefinitely for a late resource-load response.

```text
dbgjs source grep "validateUser"
dbgjs source grep "class\s+\w+Controller" --regex
dbgjs source grep "TODO" --glob "**/*.ts"
```

The search model supports:

- Fixed-string and regular-expression matching.
- Case sensitivity.
- Logical source globs or language filters.
- Provider and version selectors.
- Optional connection, target, and attachment applicability selectors.
- Primary sources, generated sources, and alternative content candidates.
- Context lines.
- Bounded result counts.

Matches contain:

- Logical source identity.
- Provider-qualified snapshot and version.
- Content identity.
- Provenance.
- Connection, target, and attachment applicability where live.
- Line, column, and match length.
- Structured source context.

Identical content can be deduplicated for searching while retaining all graph
identities, versions, projection paths, and live endpoint associations in
results.

### 13.5 Optional disk materialization

Source content storage must permit memory, disk, or hybrid implementations behind
the same content-addressed interface.

Disk materialization serves two different purposes:

1. A managed internal cache for memory reduction and efficient searching.
2. An explicit user export for inspection or use by external tools.

The managed cache is not assumed to be editable source. Explicit export includes
a manifest mapping safe local paths to exact source identities, provenance,
content hashes, projection metadata, and live endpoint associations when
available.

Materialization must:

- Prevent source URLs from escaping the destination directory.
- Resolve path collisions deterministically.
- Avoid partially visible updates.
- Preserve logical identity in a manifest.
- Permit content deduplication.

Search results must have the same schema whether search executes over in-memory
content or disk materialization.

## 14. Coverage

### 14.1 Model

Coverage is factored into three independent concepts:

1. A recording accumulates runtime coverage.
2. `capture` and `stop` produce immutable coverage objects.
3. `print` renders a coverage object, optionally excluding other coverage
   objects.

The recording lifecycle, coverage values, comparison, and presentation are
separate. Features such as file summaries, function lists, block-level source,
and interaction deltas are compositions of these primitives rather than
separate recording modes.

Precise block coverage is the intended default source of data. Coverage records
which functions and instrumented control-flow ranges executed and how often; it
is not a chronological statement trace.

### 14.2 Recording lifecycle

The core lifecycle is:

```text
dbgjs coverage start
dbgjs coverage capture [--id before-click]
dbgjs coverage capture [--id after-click]
dbgjs coverage stop [--id final]
```

`start` resets runtime coverage counters and starts accumulation. It does not
create an empty coverage object and therefore accepts no coverage object ID.
Starting while another recording is active fails explicitly.

`capture` takes a cumulative immutable snapshot while recording continues.
Although CDP resets its internal execution counters when precise coverage is
taken, the agent accumulates those deltas so every captured object represents
coverage since `start`.

`stop` performs a final capture, stops instrumentation, and returns the final
immutable coverage object. Omitting the final object is not part of normal stop
semantics.

The agent should collect a final delta from an attachment before detaching when
possible. A coverage object reports qualified connections and targets for which
data is incomplete rather than silently presenting partial data as complete.

### 14.3 Coverage object IDs and selectors

The `--id` option is optional when `capture` or `stop` writes a coverage object.
When omitted, the context generates a monotonically increasing name:

```text
cov-1
cov-2
cov-3
```

The counter is context-scoped and generated names are not silently reused.
An explicit ID that already exists fails unless a future explicit replacement
operation is requested.

Commands that read a coverage object use the latest coverage object in the
context when the positional capture selector is omitted. Relative selectors are:

```text
.      latest coverage object
.1     alias for .
.2     object before latest
.3     third-latest object
```

The numeric suffix is a positive, one-based index into publication history.
Relative-selector names are reserved. Reading past the available history fails
explicitly. History includes coverage from every target in the context, but not
heap snapshots or CPU profiles. Explicit `--target` and `--connection` filters
narrow history before indexing. Explicit IDs remain available for durable scripts:

```text
dbgjs coverage show before-click
dbgjs coverage show .2 --target renderer-4
```

Naming and enrichment are independent. Every capture made without `--raw` is
source-mapped and enriched before successful publication, including captures
with explicit IDs. `--raw` deliberately skips enrichment for inexpensive
baselines. Stored captures are immutable: `coverage show --no-cache` is rejected
rather than silently pretending to recompute them.

Coverage source filters are explicit:

```text
dbgjs coverage show . --path-prefix https://example.test/src/
dbgjs coverage show . --path-glob "**/contrib/issue/**"
```

Prefixes match the beginning of normalized source URLs, not arbitrary substrings.
Globs use `/` separators, `*` within a path segment, and `**` across segments.
The filters are mutually exclusive and apply to authored ranges inside bundles,
not just generated bundle names. `--path` remains a deprecated prefix alias.
An empty filtered result reports that no functions matched the specified filter,
distinct from a capture with no execution.

### 14.4 Exclusion

Coverage exclusion derives a view containing execution represented by the
selected object but not represented by the excluded baseline:

```text
dbgjs coverage print --id . --exclude .2 --style blocks
```

For counted coverage, exclusion subtracts aligned execution counts and clamps
negative results to zero:

```text
result.count = max(selected.count - excluded.count, 0)
```

For binary coverage, this is ordinary set difference. Zero-count ranges are
omitted from covered-only output.

Coverage ranges align by stable source content and canonical runtime range, not
by URL alone. A script at the same URL with different content is a different
source identity. Source projection is applied after exclusion so approximate or
ambiguous authored mappings do not corrupt the underlying comparison.

An implementation may allow `--exclude` more than once. Each exclusion applies
to the result of the preceding one.

### 14.5 Printing

`print` is the single presentation operation:

```text
dbgjs coverage print [--id <selector>] [--exclude <selector>] \
  --style files|functions|blocks
```

The initial styles are:

- `files`: print only files containing positive coverage.
- `functions`: group positively covered functions under their files and show
  execution counts.
- `blocks`: print positively covered block ranges as syntax-highlighted source
  code, grouped by file and function.

Covered-only output is the default. Rendering uncovered ranges, context size,
counts, target filters, source globs, color, and verbosity are independent
presentation or selection options rather than additional recording commands.

Machine-readable output uses the same semantic granularity selected by `style`
and returns structured files, functions, ranges, counts, source lines, and
highlight spans without ANSI escapes.

### 14.6 Targets, stepping, and source projection

Coverage recording can apply to one attachment or a selector covering multiple
current and future attachments across connections. Each captured object
preserves connection, target, attachment, source-snapshot, and connection-
generation provenance.

Precise coverage can remain active while targets pause and step through code.
Captures taken at successive pauses can therefore be excluded to show coverage
accumulated between those pauses. The result remains block coverage, not proof of
an exact line-by-line execution order.

Runtime coverage ranges refer to generated scripts. Coverage objects preserve
the generated ranges and source content required to project them into authored
sources. Renderers expose mapping quality and diagnostics instead of presenting
approximate projection as exact.

### 14.7 Events and JSON

Coverage lifecycle changes emit normalized events, including recording started,
object captured, recording stopped, and incomplete target data.

Coverage commands support the same JSON input and output contract as every
other command. A JSON coverage object is presentation-independent; `print`
returns a derived, optionally excluded representation at the requested
granularity.

## 15. Human-readable terminal output

Text output is intended for humans and may adapt to terminal capabilities.

Requirements:

- Useful default summaries.
- Stable debugger-local IDs.
- Source context for locations where content is available.
- Syntax highlighting when output is a capable TTY.
- Configurable color behavior.
- Clear indication of target and attachment when more than one is relevant.
- Clear distinction between requested and resolved source locations.
- No dependence on terminal formatting for semantic information.

Commands can offer concise and verbose views, but machine-readable output is the
authoritative automation contract.

## 16. JSON input and output

Every command must be available with JSON input and JSON output.

### 16.1 JSON output

Finite commands return one structured result envelope containing:

- Success or failure.
- Selected context ID and context revision.
- Connection ID and generation where applicable.
- Resolved scope, where applicable.
- Result data or a structured error.

For example:

```json
{
  "ok": true,
  "context": "shop",
  "revision": 142,
  "scope": {
    "connectionId": "browser",
    "connectionGeneration": 3,
    "targetId": "t-page",
    "attachmentId": "a-1"
  },
  "result": {}
}
```

Errors have stable codes, structured details, and a nonzero process exit status:

```json
{
  "ok": false,
  "context": "shop",
  "revision": 142,
  "error": {
    "code": "ambiguous_target",
    "message": "Command requires exactly one target",
    "details": {
      "candidates": [
        {"connectionId": "browser", "targetId": "t-page"},
        {"connectionId": "browser", "targetId": "t-worker"}
      ]
    }
  }
}
```

Context lookup failures use distinct `context_required`,
`context_not_found`, and `ambiguous_context` codes. An ambiguity error lists
context IDs and workspace hints; it never reports that one was selected.

Streaming commands use JSON Lines. JSON output never includes ANSI formatting.
Human-oriented diagnostics must not corrupt the JSON stream.

### 16.2 JSON input

Each command accepts its parameters as one JSON value from standard input or an
explicit input source.

Conceptually:

```text
dbgjs breakpoint set --input json --output json
```

Command-specific positional or flag parameters and JSON parameters should not be
silently merged. Conflicting input forms produce an explicit usage error.

### 16.3 Universal RPC form

In addition to command-specific JSON input, the CLI may expose one universal
request form:

```json
{
  "context": "shop",
  "command": "breakpoint.set",
  "params": {
    "location": {
      "url": "src/users.ts",
      "line": 81,
      "column": 5
    },
    "targetSelector": {"kind": "allEligible"}
  }
}
```

The command-specific and universal forms use the same underlying request and
response schemas. Clients may omit `targetSelector` for breakpoint creation, but
the normalized request and stored specification expose the required
`allEligible` default rather than capturing focus invisibly.

## 17. Concurrency and consistency

Several CLI processes and a TUI may use one context concurrently while events
arrive independently from several transports.

Therefore:

- The agent serializes or otherwise coordinates context state mutations.
- Each result identifies the context revision and applicable connection
  generations it observed.
- State reads are snapshots at exactly one context revision.
- Event and state-watch subscriptions resume from context revisions.
- A get/watch operation atomically couples its initial snapshot with subsequent
  revisions.
- One logical mutation may atomically add or remove facts and breakpoint
  bindings on several connections under one revision.
- Mutating requests may optionally assert an expected revision.
- Focus changes do not alter explicitly scoped commands.
- A command never silently retargets after resolving its target.
- Stale target, frame, scope, and object handles fail explicitly.
- Connection filters never weaken context-wide revision consistency.

## 18. Security and privacy

CDP access is equivalent to code execution in the target.

The implementation must account for:

- Endpoint credentials and authentication material.
- Source code and source maps.
- Console messages and evaluated values.
- Cookies, tokens, and other page data exposed through CDP.
- Process activation and injection permissions.
- Recordings and event logs containing sensitive data.

Human output should redact endpoint credentials. Persistence, recording, export,
and diagnostic commands must make sensitive-data behavior explicit.

---

# Part II: Deferred details and staged functionality

The following decisions and features can be added without changing the core
architecture above. Items explicitly marked required in Part I are not deferred
merely because their final CLI spelling appears here.

## 19. Exact command grammar

The final command names, aliases, and flag placement remain open. A likely shape
is:

```text
dbgjs [--context <id>] [--output text|json|jsonl] <command>
```

Potential command groups:

```text
context        list, create, select, status, delete
connection     list, status, connect, launch, disconnect, stop, delete
target         list and show live targets
focus          show or set interactive connection/target focus
attach         create attachment or following rule
attachment     list, show, detach
breakpoint     set, list, show, enable, disable, remove
watch          add, list, show, evaluate, enable, disable, remove
coverage       start, capture, stop, list, delete, print
source         list, show, map, resolve, grep, materialize, export
events         query and follow
wait           wait for a state or event condition
process        probe and attach by PID
screenshot     capture
cdp            send a target-scoped raw CDP request
```

Whether frequently used operations such as `status`, `pause`, and `continue`
also exist as top-level commands is a usability decision. This list is not a
requirement to create a command for every graph node: context selection,
connection/target selectors, source resolution, and rendering are orthogonal
parameters reusable by command groups.

The examples below use `-c` as an illustrative short form for `--context` and
use explicit connection names only when creating, replacing, or intentionally
filtering a connection. Whether `-c`, a shell-local selected context, or another
spelling is shipped remains deferred. Required semantics are durable context
identity, explicit-selection precedence, and explicit ambiguity failure.

## 20. Staged implementation

The implementation should stage capabilities without temporarily making a
connection the durable ownership root:

1. **Context and state foundation**
   - Long-lived local agent and RPC transport.
   - Durable named contexts that work with zero connections and resolve
     explicitly rather than by `cwd` identity.
   - One context revision, atomic state get/watch, normalized history, and JSON
     input/output for every implemented request.
2. **Multi-connection runtime foundation**
   - Multiple named connections per context, each with independent generation
     and lifecycle.
   - HTTP and WebSocket discovery, browser observation, direct-target and
     flattened attachments.
   - Multiple live attachments, related-target policy, interactive focus, pause,
     resume, stepping, stack, scopes, evaluation, events, and one-shot wait.
3. **Shared intent and source graph**
   - Provider-qualified/versioned source snapshots and typed projections.
   - Runtime script endpoints contributed by every connection, source-map
     projection, and projected grep.
   - Context-owned breakpoint specifications with the `allEligible` default and
     simultaneous cross-connection binding.
   - Persistent watches and debugger policies applied through explicit
     selectors.
4. **Cross-target artifacts**
   - Precise coverage start, capture, stop, exclusion, and covered-only printing
     with cross-connection provenance.
   - Source materialization and additional renderers over the same graph.

During migration from an older connection-rooted implementation, compatibility
adapters may map each old connection to a one-connection context. They must not
leak that temporary mapping into new IDs, JSON schemas, breakpoint defaults, or
revision semantics.

Process activation by PID can begin with discovery and Node-specific safe
activation, then gain additional providers.

## 21. Advanced attachment policy

Later work can add:

- Named reusable target selectors.
- Rich relationships such as "workers related to the focused page."
- Rule priority and exclusion.
- Browser-context-specific rules.
- Popup inheritance.
- Per-target debugger domain configuration.
- Automatic blackboxing or ignore lists.

The core per-connection target graphs, context attachment rules, and focus model
already permit these.

## 22. Advanced event processing

Later work can add:

- User-defined event filters and projections.
- Derived events and event correlation.
- Event log retention policies.
- Durable bookmarks or cursors.
- Event recording and deterministic replay.
- Hooks that invoke commands or external programs.
- Rate limiting and aggregation for noisy console or network events.

These features build on normalized revisioned events and resumable history.

## 23. Advanced watches

Later watch functionality can include:

- Watch groups.
- Per-frame or named-frame selectors.
- Watch history across pauses.
- Value diffs.
- Expansion policies for objects.
- Explicit side-effect-free evaluation where supported.
- Trigger conditions that emit derived events or pause/resume actions.
- Display formatting shared by CLI and TUI.

## 24. Advanced coverage

Later coverage functionality can include:

- Additional render styles and external report formats.
- Coverage filtering through projected-source search.
- Persisting explicitly derived coverage objects.
- Coverage history across process launches.
- CSS rule-usage coverage.
- Coverage-triggered events.
- Combining coverage with an explicit chronological step trace.
- Exporting coverage alongside materialized source trees.

These features build on immutable cumulative coverage objects, exclusion, source
projection, and rendering.

## 25. Source cache and export implementation

Deferred implementation choices include:

- Directory layout.
- Database and manifest formats.
- Memory thresholds.
- Compression.
- Memory mapping.
- Hard links or copy-on-write files.
- Garbage collection and quotas.
- Delegating disk search to ripgrep.
- Read-only virtual filesystems or editor integration.

The stable requirement is a provider-qualified, versioned, content-addressed
graph abstraction with equivalent resolution and search results across memory
and disk backends.

## 26. Presentation

Deferred presentation choices include:

- Syntax-highlighting library.
- Theme detection.
- Source excerpt width and truncation.
- Unicode versus ASCII markers.
- Table formatting.
- Pager integration.
- TUI layout and navigation.

The source excerpt and highlight data remain structured below the presentation
layer.

## 27. Runtime-specific process activation

PID attachment should evolve through independent providers:

- Node.js.
- Chrome and Chromium.
- Electron.
- Deno.
- Bun.
- Other CDP-compatible runtimes.

Each provider reports probe and activation capabilities rather than reducing all
failures to "not supported." Experimental injection, elevation, or target
modification remains opt-in.

For Electron, a discovered Chromium browser endpoint remains the preferred
transport because its native `Target.*` graph already multiplexes renderer
sessions. When only the main-process Node inspector is available, a process-tree
connection may expose renderer processes through an equivalent CDP transport
backed by `webContents.debugger`. The process-tree target ID identifies the
stable process instance, while `webContents.id` remains an internal live bridge
handle correlated by `webContents.getOSProcessId()`.

This fallback must preserve ordinary CDP envelopes, including nested
`sessionId` values, so evaluation, breakpoints, profiles, coverage, and heap
operations do not acquire Electron-specific variants. Zero or multiple
`WebContents` matches are explicit errors.

The main-process CDP connection bootstraps an authenticated loopback server but
does not carry renderer traffic. One control socket owns the bridge lifetime,
and one renderer socket carries newline-delimited CDP envelopes for each
attachment. Closing a renderer socket releases only its debugger attachment;
closing the control socket releases every attachment and the server. This also
makes process termination a cleanup signal enforced by the operating system
rather than by a remote JavaScript object finalizer.

The bridge owns only debugger attachments it created and never detaches an
unknown external debugger. For a recognized VS Code browser view whose root
debugger is already owned by VS Code, it borrows a flattened child session and
releases only that child session when the renderer socket closes.

## 28. Advanced raw CDP support

The core raw CDP command can later gain additional routing and diagnostic modes
for:

- The endpoint root.
- A selected attachment.
- An explicitly identified attachment.

Raw requests must still participate in request routing, recording, security
warnings, JSON output, and generation checks. They must not bypass the agent's
ownership of the transport.

## 29. TUI

The TUI is deliberately deferred. It consumes:

- State snapshots.
- Context-revisioned event streams.
- The same mutation requests as the CLI.
- Structured source excerpts and watch results.

It does not establish its own CDP connection and does not introduce a separate
debugger state model or context identity.

---

# Part III: Illustrative CLI interactions

The following scenarios are sketches, not a fixed command-line or output
specification. Exact command names, flags, generated IDs, tables, colors, source
excerpt layout, and wording may change.

What matters is that the workflows compose the core concepts consistently:

- One named context owns zero or more named runtime connections.
- Each connection can observe and attach many interacting targets.
- Context, connection, target selector, source graph traversal, and rendering are
  independent concepts.
- Interactive commands use an explicit target or focus; durable breakpoint
  intent defaults to every eligible target in the context.
- Breakpoints, logpoints, watches, policies, and offline source graph data are
  context-owned persistent state.
- Pause snapshots and remote-object handles are ephemeral.
- Events and waits provide race-free observation.
- Coverage recording produces immutable coverage objects.
- Coverage exclusion and printing operate on those objects independently.
- Every interaction has equivalent structured JSON input and output.

## 30. Bind one shared TypeScript source in Node and Chrome

An application uses `src/shared/validation.ts` in its Node.js API and browser
frontend. The user creates the durable context before either runtime is
available:

```text
> cd D:\src\shop
> dbgjs context create .
Created context d:\src\shop (disconnected)

> dbgjs breakpoint set src/shared/validation.ts:41
Created bp-1 [pending]
Scope: all eligible targets in context d:\src\shop
Source: workspace:src/shared/validation.ts@sha256:8ab4...
Live bindings: none
```

The normalized absolute project path is the identity. Commands from descendants
inherit this path context unless a nearer path context or any applicable cwd
binding takes precedence.

The API and browser are then added as independent connections:

```text
> dbgjs -c shop connection connect server --pid 18420
+ Connected server to Node.js server.js (PID 18420)
  Target: server/node-18420
  bp-1: bound dist/shared/validation.js:63:3

> dbgjs -c shop connection connect browser http://127.0.0.1:9222 \
    --target-url http://localhost:5173/
+ Connected browser to Chrome
  Focus: browser/page-1
  bp-1: bound assets/app.7d21.js:1884:17
```

The source graph explains both bindings without making either target the source
identity:

```text
> dbgjs -c shop source resolve src/shared/validation.ts:41
workspace:src/shared/validation.ts@sha256:8ab4...:41:1
├─ source-map -> runtime:server/script-27@sha256:97c1...:63:3
│  endpoint: server/node-18420 attachment a-server
└─ source-map -> runtime:browser/script-913@sha256:42ef...:1884:17
   endpoint: browser/page-1 attachment a-page

> dbgjs -c shop breakpoint show bp-1
Requested: src/shared/validation.ts:41:1
Scope:     all eligible targets in context shop
Bindings:
  server/node-18420  dist/shared/validation.js:63:3       verified
  browser/page-1     assets/app.7d21.js:1884:17           verified
```

Target-local commands still use focus or explicit selection:

```text
> dbgjs -c shop eval "document.title"
Connection: browser
Target: page-1
"Checkout"

> dbgjs -c shop eval "process.version" \
    --target server/node-18420
Connection: server
Target: node-18420
"v24.4.0"
```

Changing focus to the server would affect an unqualified `eval`, `step`, or
`stack`; it would not alter `bp-1`. When Chrome disconnects, only Chrome live
facts disappear:

```text
> dbgjs -c shop connection disconnect browser
Disconnected browser
Removed live endpoints: 37
Removed breakpoint bindings: bp-1 on browser/page-1
Remaining binding: bp-1 on server/node-18420
Context shop and offline source graph retained
```

The workspace and source-map snapshots can still be searched, the Node binding
remains active, and reconnecting `browser` can bind `bp-1` again. This is the
required full-stack behavior; exact output layout and connection command syntax
remain illustrative.

## 31. Attach to a running Express server

An Express server is already running without an inspector:

```text
node server.js
```

The user finds its PID and asks the debugger to activate or discover the Node
inspector:

```text
> dbgjs -c api connection connect server --pid 18420
Detected Node.js 24 in process 18420
Activated inspector at ws://127.0.0.1:9229/7f4c...
+ Connected server to node server.js (PID 18420)
  Focus: server/node-18420
```

If activation cannot be performed safely, the command fails with a structured
diagnostic explaining the available provider, attempted mechanism, required
permission, or conflicting port. The successful connection is immediately
usable; inspecting WebSocket and CDP details is an advanced diagnostic workflow.

They set a breakpoint in an authored TypeScript source and add watches:

```text
> dbgjs -c api breakpoint set src/routes/orders.ts:48
Created bp-1
Requested: src/routes/orders.ts:48:1
Resolved:  dist/routes/orders.js:71:3

  45 | router.post("/orders", async (req, res) => {
  46 |   const order = parseOrder(req.body);
  47 |   const user = req.user;
> 48 |   const result = await submitOrder(order, user);
     |   ^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^
  49 |   res.status(201).json(result);
  50 | });

> dbgjs -c api watch add "req.body" --name request-body
Created watch-1

> dbgjs -c api watch add "req.user?.id" --name user-id
Created watch-2
```

The context-owned breakpoint remains valid if the script is reloaded and receives
a new CDP script ID. The watches are evaluated only in an applicable pause
snapshot.

In one shell, the user waits for the request:

```text
> dbgjs -c api wait paused --timeout 60s
```

In another shell:

```text
> curl -X POST http://localhost:3000/orders \
    -H "content-type: application/json" \
    -d "{\"sku\":\"keyboard\",\"quantity\":2}"
```

The wait completes:

```text
Paused: breakpoint bp-1
Target: node-18420
At:     src/routes/orders.ts:48:3

Watches:
  request-body = { sku: "keyboard", quantity: 2 }
  user-id      = undefined
```

The user examines the stack and scopes:

```text
> dbgjs -c api stack
0  router.post callback       src/routes/orders.ts:48:3
1  Layer.handleRequest        node_modules/router/lib/layer.js:152:17
2  next                       node_modules/router/lib/route.js:157:13

> dbgjs -c api scopes --frame 0
Local
  req     = Object { ... }
  res     = ServerResponse { ... }
  order   = { sku: "keyboard", quantity: 2 }
  user    = undefined

> dbgjs -c api eval "req.headers.authorization" --frame 0
undefined
```

The missing authenticated user explains the failure. The user steps into the
order submission to inspect its defensive path:

```text
> dbgjs -c api step into
Paused: step
At: src/services/orders.ts:19:1

> 19 | export async function submitOrder(order: Order, user?: User) {
     | ^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^
  20 |   if (!user) {
  21 |     throw new AuthenticationError();

> dbgjs -c api stack
0  submitOrder                src/services/orders.ts:19:1
1  router.post callback       src/routes/orders.ts:48:24

> dbgjs -c api step over
Paused: step
At: src/services/orders.ts:20:3

  19 | export async function submitOrder(order: Order, user?: User) {
> 20 |   if (!user) {
     |   ^^^^^^^^^^^
  21 |     throw new AuthenticationError();

> dbgjs -c api eval "user"
undefined
```

They can resume without removing the breakpoint or watches:

```text
> dbgjs -c api continue
Resumed node-18420
```

## 32. Discover code executed by one HTTP request

The same server is running. The user wants to know which application code a
single request executes without stepping through every call.

First, start coverage and capture a baseline after incidental background work
has settled:

```text
> dbgjs -c api coverage start
Coverage recording started on node-18420

> dbgjs -c api coverage capture --id before-request
Captured before-request
```

Send the request:

```text
> curl -X POST http://localhost:3000/orders \
    -H "content-type: application/json" \
    -d "{\"sku\":\"keyboard\",\"quantity\":2}"
```

Stop coverage. With no explicit ID, the context generates the next name:

```text
> dbgjs -c api coverage stop
Captured cov-1
Coverage recording stopped
```

Print only files that gained coverage since the baseline:

```text
> dbgjs -c api coverage print --exclude before-request --style files
src/routes/orders.ts
src/services/orders.ts
src/db/orders.ts
src/telemetry/events.ts
```

The omitted `--id` selects the latest object, equivalent to `--id .`.

Print covered functions:

```text
> dbgjs -c api coverage print --id . --exclude before-request \
    --style functions
src/routes/orders.ts
  router.post callback(req, res)       x1
  parseOrder(value)                    x1

src/services/orders.ts
  submitOrder(order, user)             x1
  calculateTotal(order)                x1

src/db/orders.ts
  insertOrder(order)                   x1
```

Print covered blocks as projected, syntax-highlighted source:

```text
> dbgjs -c api coverage print --id . --exclude before-request \
    --style blocks
src/services/orders.ts

submitOrder(order, user) x1

  19 | export async function submitOrder(order: Order, user?: User) {
> 20 |   if (!user) {
> 21 |     throw new AuthenticationError();
> 22 |   }
     |   ... blocks not covered by this request omitted ...
  36 | }
```

`--style files`, `functions`, and `blocks` render the same derived coverage
value. They are not different recording modes.

## 33. Debug a website button that does nothing

Chrome is already running with remote debugging enabled. There is one ordinary
website page, so connecting focuses it and automatically attaches
its related service worker:

```text
> dbgjs -c frontend connection connect browser http://127.0.0.1:9222
+ Connected browser to http://localhost:5173/
  Focus: browser/page-1
  Also attached: service-worker-1
```

The browser connection still exposes both targets when needed:

```text
> dbgjs -c frontend target list
CONNECTION  TARGET             TYPE             FOCUSED  URL
browser     page-1             page             yes      http://localhost:5173/
browser     service-worker-1   service_worker   no       http://localhost:5173/sw.js
```

Target selection matters for globals. Evaluation identifies the selected target
and execution context instead of silently trying another realm:

```text
> dbgjs -c frontend eval "document.title" --target service-worker-1
Target:  service-worker-1 (service_worker)
Realm:   default worker realm
ReferenceError: document is not defined
Hint: browser/page-1 is a page target and is currently focused.

> dbgjs -c frontend eval "document.title"
Target:  page-1 (page)
Realm:   main frame
"Checkout"
```

Before debugging code, the user explores the rendered button through the CDP
escape hatch:

```text
> dbgjs -c frontend cdp DOM.getDocument \
    --params '{"depth":2,"pierce":true}'
root.nodeId = 1

> dbgjs -c frontend cdp DOM.querySelector \
    --params '{"nodeId":1,"selector":"button[data-testid=checkout]"}'
nodeId = 42

> dbgjs -c frontend cdp DOM.describeNode \
    --params '{"nodeId":42,"depth":1}'
BUTTON data-testid="checkout" disabled=false
  #text "Checkout"
```

The high-level screenshot convenience uses the same focused target and stores
the clipped image:

```text
> dbgjs -c frontend screenshot capture \
    --selector "button[data-testid=checkout]" \
    --output checkout-before.png
Captured checkout-before.png (164 x 38, target page-1)
```

The button is rendered by `CheckoutButton.tsx`, but activating it does nothing.
The user searches projected source contents:

```text
> dbgjs -c frontend source grep "onCheckout" --glob "src/**/*.{ts,tsx}"
src/components/CheckoutButton.tsx
  27 | export function CheckoutButton({ cart }: Props) {
> 28 |   const onCheckout = () => submitCheckout(cart);
  29 |
  30 |   return <Button onClick={onCheckout}>Checkout</Button>;

src/checkout/submitCheckout.ts
> 11 | export async function submitCheckout(cart: Cart) {
```

They place a breakpoint on the handler and a logpoint further downstream:

```text
> dbgjs -c frontend breakpoint set \
    src/components/CheckoutButton.tsx:28
Created bp-1 [verified on browser/page-1; pending on browser/service-worker-1]
Scope: all eligible targets in context frontend

> dbgjs -c frontend breakpoint set \
    src/checkout/submitCheckout.ts:18 \
    --log '"submitting cart", cart.id, cart.items.length'
Created bp-2 [logpoint, verified on browser/page-1]
```

A breakpoint accidentally scoped to the service worker remains explicit rather
than moving itself to the page:

```text
> dbgjs -c frontend breakpoint set \
    src/components/CheckoutButton.tsx:28 \
    --target service-worker-1
Created bp-3 [pending]
Warning: source is not loaded in selected target service-worker-1.
         It is loaded in page-1.
Hint: use --target page-1, or omit --target to use all eligible context targets.
```

A logpoint is a breakpoint specification whose action emits a structured console
event and resumes automatically. Its exact expression syntax is deferred.

The user watches relevant state and follows events:

```text
> dbgjs -c frontend watch add "cart.items.length" --name item-count
Created watch-1

> dbgjs -c frontend coverage start
Coverage recording started on page-1 and related attachments
```

The user activates the button without leaving the CLI. Scheduling the click
allows the raw evaluation request to complete before the event handler runs. The
command observes the resulting pause within its short settling period and prints
the new state directly:

```text
> dbgjs -c frontend cdp Runtime.evaluate --params '{
    "expression": "setTimeout(() => document.querySelector(\"button[data-testid=checkout]\").click(), 0)",
    "returnByValue": true
  }' --settle 330ms
CDP request completed on page-1
Paused: breakpoint bp-1
At: src/components/CheckoutButton.tsx:28:28

  27 | export function CheckoutButton({ cart }: Props) {
> 28 |   const onCheckout = () => submitCheckout(cart);
     |                            ^^^^^^^^^^^^^^^^^^^^^
  29 |

Watches:
  item-count = 2
```

The handler did execute. Continuing does not need a separate wait. If the target
pauses again within the settling period, `continue` prints that pause directly;
otherwise it reports the lasting running state:

```text
> dbgjs -c frontend continue
Running: page-1

> dbgjs -c frontend coverage stop --id after-handler
Captured after-handler
```

The covered functions reveal that validation returns before the logpoint:

```text
> dbgjs -c frontend coverage print --id after-handler --style functions
src/checkout/submitCheckout.ts
  submitCheckout(cart)                 x1
  validateCart(cart)                   x1

src/checkout/messages.ts
  showEmptyAddressMessage()            x1
```

Covered block source makes the branch visible:

```text
> dbgjs -c frontend coverage print --id after-handler --style blocks
src/checkout/submitCheckout.ts

submitCheckout(cart) x1

  11 | export async function submitCheckout(cart: Cart) {
> 12 |   if (!cart.shippingAddress) {
> 13 |     showEmptyAddressMessage();
> 14 |     return;
> 15 |   }
     |   ... remaining blocks omitted ...
  24 | }
```

The logpoint never fired because execution never reached line 18. The user can
move the breakpoint to the covered branch, inspect `cart.shippingAddress`, and
continue debugging without changing connections.

## 34. Compare two button interactions with relative coverage selectors

The user wants to compare a failing click with a successful click after entering
an address:

```text
> dbgjs -c frontend coverage start

# Click without an address.
> dbgjs -c frontend coverage capture
Captured cov-2

# Enter an address and click again.
> dbgjs -c frontend coverage stop
Captured cov-3
```

The most recent interaction is represented by the latest cumulative object
excluding the preceding cumulative object:

```text
> dbgjs -c frontend coverage print --id . --exclude .2 \
    --style functions
src/checkout/submitCheckout.ts
  submitCheckout(cart)                 x1
  createOrderRequest(cart)             x1

src/api/orders.ts
  postOrder(request)                   x1

src/navigation/routes.ts
  openConfirmation(orderId)            x1
```

The same comparison can be printed as files or source blocks without recording
again:

```text
> dbgjs -c frontend coverage print --id . --exclude .2 --style files
> dbgjs -c frontend coverage print --id . --exclude .2 --style blocks
```

The relative selectors are convenient for exploration. A script should normally
use explicit IDs if later captures could change what `.` and `.2` select.

## 35. Debug an algorithm in a test with logpoints

A test runner is launched under the debugger and held before user code starts:

```text
> dbgjs -c tests connection launch runner node --break -- \
    node_modules/vitest/vitest.mjs run tests/shortest-path.test.ts
Launched runner (PID 20916)
Focus: runner/node-20916
Waiting for debugger
```

The user sets a normal breakpoint at the algorithm entry and logpoints inside
the loop:

```text
> dbgjs -c tests breakpoint set src/graph/dijkstra.ts:12
Created bp-1 [verified]

> dbgjs -c tests breakpoint set src/graph/dijkstra.ts:24 \
    --log '"visit", current.id, "distance", distances.get(current.id)'
Created bp-2 [logpoint, verified]

> dbgjs -c tests breakpoint set src/graph/dijkstra.ts:31 \
    --condition 'candidate < distances.get(neighbor.id)' \
    --log '"relax", current.id, "->", neighbor.id, candidate'
Created bp-3 [conditional logpoint, verified]
```

Continue from Node's initial inspector pause. This test may take longer than the
default settling period to load, so the user extends the same command's settling
policy rather than issuing a separate wait:

```text
> dbgjs -c tests continue --settle 30s
Paused: breakpoint bp-1
At: src/graph/dijkstra.ts:12:1

  10 | export function shortestPath(graph: Graph, start: Node, end: Node) {
  11 |   const distances = initializeDistances(graph, start);
> 12 |   const queue = new PriorityQueue<Node>();
     |   ^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^
  13 |   queue.push(start, 0);
```

Add watches and step through initialization:

```text
> dbgjs -c tests watch add "start.id" --name start
Created watch-1

> dbgjs -c tests watch add "queue.size" --name queue-size
Created watch-2

> dbgjs -c tests watch add "[...distances.entries()]" --name distances
Created watch-3

> dbgjs -c tests watch list
start       = "A"
queue-size  = 1
distances   = [["A", 0], ["B", Infinity], ["C", Infinity]]

> dbgjs -c tests step over
Paused: step at src/graph/dijkstra.ts:15:3

> dbgjs -c tests watch list
start       = "A"
queue-size  = 1
distances   = [["A", 0], ["B", Infinity], ["C", Infinity]]
```

The user now lets the algorithm run. Logpoints observe the loop without
repeatedly stopping it:

```text
> dbgjs -c tests events --follow --type logpoint,test.finished
> dbgjs -c tests continue

logpoint bp-2  visit A distance 0
logpoint bp-3  relax A -> B 4
logpoint bp-3  relax A -> C 2
logpoint bp-2  visit C distance 2
logpoint bp-3  relax C -> B 3
logpoint bp-2  visit B distance 3
test.finished shortest-path.test.ts [failed]
```

The trace suggests that path relaxation works, so the user records coverage for
the failing test to see whether reconstruction executes:

```text
> dbgjs -c tests connection launch runner node --replace --break -- \
    node_modules/vitest/vitest.mjs run tests/shortest-path.test.ts
> dbgjs -c tests coverage start
> dbgjs -c tests continue
> dbgjs -c tests wait process-exited --timeout 30s
> dbgjs -c tests coverage stop --id failing-test

> dbgjs -c tests coverage print --id failing-test --style functions
src/graph/dijkstra.ts
  shortestPath(graph, start, end)       x1
  reconstructPath(previous, end)        x1
```

The replacement launch is held before user code, which lets coverage begin on
the new transport before the test runs. The exact launch spelling is deferred;
the important behavior is that incomplete coverage is reported explicitly.

The user places a breakpoint in reconstruction and reruns:

```text
> dbgjs -c tests breakpoint set src/graph/dijkstra.ts:52
Created bp-4 [verified for future matching scripts]

> dbgjs -c tests connection launch runner node --replace --break -- \
    node_modules/vitest/vitest.mjs run tests/shortest-path.test.ts

> dbgjs -c tests continue --settle 30s
Paused: breakpoint bp-4
At: src/graph/dijkstra.ts:52:3

> dbgjs -c tests eval "previous"
Map(2) { "B" => "C", "C" => "A" }

> dbgjs -c tests step over
Paused: step at src/graph/dijkstra.ts:53:3

> dbgjs -c tests eval "path"
["B"]
```

The normal breakpoint, stepping, watches, logpoint events, and coverage are
independent tools over the same context and source graph.

## 36. Use JSON for automation

Every scenario above can be driven without parsing terminal text. For example, a
script starts coverage:

```text
> echo '{"context":"frontend","command":"coverage.start","params":{}}' |
    dbgjs rpc --input json --output json
```

After performing an interaction, it stops coverage without choosing an ID:

```text
> echo '{"context":"frontend","command":"coverage.stop","params":{}}' |
    dbgjs rpc --input json --output json
{
  "ok": true,
  "context": "frontend",
  "revision": 127,
  "result": {
    "coverageId": "cov-4"
  }
}
```

It then prints newly covered functions relative to the preceding object:

```text
> echo '{
    "context": "frontend",
    "command": "coverage.print",
    "params": {
      "id": ".",
      "exclude": [".."],
      "style": "functions"
    }
  }' | dbgjs rpc --input json --output json
```

The response contains structured source identities, function ranges, execution
counts, projection quality, and target provenance. It does not contain the table
layout, source gutter markers, or terminal color shown in the human-readable
examples.

A race-free debugger script first inspects the execution command result. If
`continue` returns `stopped`, that result already contains the new pause and
location. Only a `running` result needs a longer wait:

```text
dbgjs -c api continue --output json
# Only if the returned state is "running":
dbgjs -c api wait paused --timeout 30s --output json
dbgjs -c api stack --output json
dbgjs -c api watch list --output json
```

The returned context revision and per-scope connection generation let the script
reject stale pause or object handles rather than accidentally applying them to a
later target state.
