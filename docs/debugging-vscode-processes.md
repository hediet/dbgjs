# Debugging VS Code Processes with dbgjs

This guide shows how to attach dbgjs to an already-running VS Code renderer,
extension host, or agent host from scratch.

Existing VS Code process discovery and inspector activation are currently
implemented on Windows.

The workflow is the same for all three runtime types:

1. List the VS Code process tree.
2. Choose a process by its window and role.
3. Create a debug context.
4. Attach the process PID.
5. Debug the selected target.

dbgjs handles the transport differences automatically.

## 1. Build dbgjs

From the dbgjs repository:

```powershell
cargo build --bins
$env:PATH = "$PWD\target\debug;$env:PATH"
```

The first command that needs the debugger service starts `dbgjs-service`
automatically. By default, its endpoint and persistent contexts are stored
under:

```text
%LOCALAPPDATA%\dbgjs\
```

## Reading logs and capture coverage

```powershell
dbgjs log
dbgjs log --after 0 --limit 100 --json
```

`log` reports capture status even when there are no entries. **No captured
entries does not mean no errors occurred.** The current collector retains only
`Runtime.consoleAPICalled` events (including `console.error`), not browser
diagnostics (`Log.entryAdded`), uncaught exceptions (`Runtime.exceptionThrown`),
or network failures. Attaching now cannot recover past network failures.

Capture is target-local and begins when dbgjs starts configuring that target's
CDP session with `Runtime.enable`. `startedAtUnixMs` is the observed local start
of that configuration request, not an event timestamp or the start of the
page/process. CDP may replay buffered console messages from before this time;
there is no guarantee of complete earlier history. Reading logs never attaches
the target or its descendants. A known but unattached target reports `inactive`
with unknown start/loss metadata and no collected categories.

The service's `get_logs` response and CLI JSON contain context, connection,
canonical target, connection generation, and `capture` metadata. Attached
target snapshots also expose this metadata as `logCapture`. `captureId`
identifies this in-memory collector, and `sessionId` identifies its CDP session
(an empty string denotes a root session). Each entry keeps its original CDP
console parameters in `params`, including type, timestamp, execution context,
and any stack trace/locations supplied by the runtime. Missing frame information
is not inferred, and child-target logs are not implicitly aggregated.

The ring retains at most 100 entries. `evictedCount` counts entries removed from
this collector's ring since its start; `droppedCount: null` means upstream loss
is not measurable, **not zero loss**. `stopped` means its event-processing loop
ended; `unknown` supports snapshots lacking capture metadata. Captures and their
messages are not persisted across detach, reconnect, or service restart.

The CLI shows the newest 20 unseen entries by default. `skipped` retains its
existing meaning (ring eviction plus the display limit); `evictedSinceCursor`
and `omittedByLimit` separate those causes. `nextCursor` advances past all
observed entries, including those omitted by the display limit. Normal reads
persist this cursor; explicit `--after` reads do not. Persisted cursors are
scoped to the canonical target, connection generation, and capture identity,
so reconnecting or reattaching starts a new cursor. Explicit cursors must belong
to the current capture.

## 2. Discover the VS Code process tree

```powershell
dbgjs process list --root vscode --no-cmd-line --no-trim
```

`--root` recognizes `vscode`, `node`, `electron`, and `browser` roots and lists
every matching subprocess tree. `--vscode` remains an alias for
`--root vscode`.

For `--root node`, nested matching Node processes remain inside the topmost
matching Node tree instead of being repeated as separate roots. The result is a
covering forest: every discovered subprocess appears in at most one tree.

Root discovery is passive by default. Add `--full` to run the same process-tree
target discovery used by a connection:

```powershell
dbgjs process list --root vscode --full --no-cmd-line
```

The full view temporarily enables demand-driven discovery and nests Electron
WebContents, pages, OOPIFs, and workers beneath their backing OS processes.
Node inspectors and browser DevTools endpoints are attachment capabilities of
their OS process, so they are not repeated as synthetic child targets. When the
command exits, it releases the temporary discovery demand.
For Node and Electron roots, `--full` may activate the root inspector in the
same way as a process-tree connection.

A typical result contains:

```text
VS Code process tree 33508
`- p:33508  Code - Insiders.exe  [vscode-main]
   |- w:33508/7  window  linkrpc
   |  |- p:43336  renderer  [renderer]
   |  |  `- renderer-3  [page]
   |  |     `- renderer-3/target/57B3B333337F3E49DC315EEC50F11741  [iframe]
   |  `- p:15388  extension-host  [extension-host]
   `- p:24664  agent-host  [agent-host]
      `- p:41552  Code - Insiders.exe  [copilot]
```

These references are examples. `p:<pid>` selects a process and
`w:<vscode-main-pid>/<window-id>` selects a VS Code window. Raw PIDs remain
accepted for compatibility. JSON output also includes the full discovery
locator for each process. Full process and window locators are accepted by the
same attach command:

```powershell
dbgjs process attach vscode://33508/process/43336 --set
dbgjs process attach vscode://33508/window/7 --set
```

Non-VS Code roots use the corresponding process-tree locator:

```powershell
dbgjs process attach process-tree://12496/process/12496 --set
```

Use the role label, window grouping, and PID together:

- `[renderer]`: the workbench DOM and browser JavaScript for that window.
- `[extension-host]`: extensions running for that window.
- `[agent-host]`: VS Code's shared agent-host runtime.
- `[copilot]`: a separate runner below the agent host; attach its own PID if
  that process, rather than the agent host, is the intended target.
- `session ...`: virtual metadata, not a process. Attach the containing
  `[agent-host]` or `[copilot]` PID instead.

Use a separate context for each process in the introductory workflows below.
Each root is published canonically as `$node-root:<connection-id>`. The friendly
selector `node` remains convenient when only one Node target matches.

Target-local commands use the scope selected by `--set`. Override any part with
`--context <id>` or `--target <selector>`. The owning connection is inferred
when that selector matches exactly one target in the context; add
`--connection <id>` only when the selector is ambiguous across connections.

## 3. Debug a renderer

Suppose process discovery shows:

```text
window 7  linkrpc
`- 43336  renderer  [renderer]
```

Create a context and attach the window's primary renderer:

```powershell
dbgjs context create --context linkrpc-renderer "LinkRPC renderer" --set
dbgjs process attach w:33508/7 --set
```

Window attachment keeps the window identity through live Electron discovery and
selects the BrowserWindow's primary `webContents`. It does not select the first
renderer PID listed under the window: that PID can belong to an out-of-process
iframe, and multiple `webContents` can share a renderer PID. Stale process-list
window metadata is not used to choose the target.

Use `dbgjs process attach p:43336 --set` to explicitly select by PID instead.
PID attachment succeeds only when it identifies exactly one live Electron
`webContents`; it never prefers a primary window over other matching contents.

When Electron exposes a Chromium remote-debugging endpoint, renderer attachment
prefers a flattened browser-CDP session. The main-process Node inspector supplies
the exact PID/window-to-page mapping through `webContents.fromDevToolsTargetId`.
It does not attach `webContents.debugger`: separate browser-CDP clients can inspect
the same renderer without evicting one another. Each connection owns its metadata
bridge and browser session; disconnecting one leaves the others intact.

Without a browser endpoint, attachment falls back to `webContents.debugger`.
Renderers explicitly startup-blocked by the Electron bridge also stay on that
bridge to preserve startup-block ownership and resume behavior. Bridge
installation errors are reported instead of being hidden by a later
renderer-discovery timeout.

Capture the current renderer viewport in the system temporary directory:

```powershell
dbgjs screenshot capture
```

Use `--output <path>` to choose the destination.

### Renderer attachment failures

`Electron webContents ... is already attached by another debugger`

- Stop another active renderer debugger, such as
  `Developer: Debug Renderer in New Window`, and retry.
- Or explicitly use `process attach p:<pid> --force`. Only this opt-in path calls
  Electron's debugger detach before dbgjs attaches.

`renderer process ... has no live Electron webContents`

- Re-run process discovery. The window may have reloaded or closed.
- Use the new renderer PID and process target ID.

`renderer process ... maps to multiple Electron webContents`

- dbgjs refuses to guess. The error prints qualified target attachment commands;
  choose the intended target using the title and URL printed with each candidate.

`VS Code window w:... has no live Electron webContents matching its identity`

- Window resolution waits at most 10 seconds for live discovery, including an
  incomplete initial inventory. Re-run `dbgjs process list --full` and retry.
- A closed window, missing live ownership metadata, or failed discovery does not
  fall back to another window or a renderer PID. Window ambiguity also fails
  with titles, URLs, and copyable target attachment commands.

### Inspect nested renderer targets

While a process-tree connection is being observed, dbgjs correlates each
Electron WebContents with its native CDP target and enables related-target
auto-attach on that page. Its OOPIFs and workers therefore appear beneath the
correct renderer without copying the browser-global target inventory beneath
every renderer:

```powershell
dbgjs target list --type iframe
dbgjs target graph
dbgjs target attach --target <printed-target-id> --set
dbgjs screenshot capture --output iframe.png
```

Tree output prints complete selectors rather than parent-relative fragments.
`target list` qualifies each selector with its connection and generation, for
example `process-tree-33508/renderer-2/target/F2CDE85C@1`. You can copy it directly
into `--target`, or omit `@1` to follow the current connection generation.
`process list --full` prints canonical IDs such as
`renderer-2/target/F2CDE85C`, since discovery need not belong to a persistent
connection yet. `target attach`, `target cdp`, evaluation, source inspection,
and screenshot capture accept those IDs once the corresponding connection
is present. An OOPIF cannot execute `Page.captureScreenshot` directly, so the
screenshot capability follows the frame-owner relation and clips a temporary
capture from the embedding page; it does not user-attach the rest of the tree.

Playwright does not yet expose a selected OOPIF as a standalone `Page`.
Playwright requires a page-root CDP session with coherent target and main-frame
identity; use the raw target operations above for nested iframe automation.

Browser debug ports compose the same way: the endpoint is an attachment route
for its OS process, while its pages, OOPIFs, and workers are contributed as
resources.

The virtual Target facade and `process list --full` observe the same revisioned
target inventory. A full list performs one refresh, holds a temporary discovery
lease while the inventory settles, then renders an immutable observation.
Future process and target watchers can subscribe to the same revision stream
instead of introducing another polling or discovery path.

### Pause a future renderer before startup

Attach the VS Code main process through its process-tree connection, then hold
the pause-on-start lease before opening the window that fails:

```powershell
dbgjs context create --context vscode-startup "VS Code startup" --set
dbgjs process attach p:33508 --set
dbgjs connection pause-future on
```

New Electron WebContents are discovered from the main process event stream and
blocked in `Page.waitForDebugger` before page startup continues. After opening
the Agents window, list processes again and attach its `p:` or `w:` reference:

```powershell
dbgjs process list --root vscode --no-cmd-line
dbgjs process attach w:33508/9 --set
```

Attaching adopts the reserved renderer transport. Disable the policy when no
more future renderers should be blocked:

```powershell
dbgjs connection pause-future off
```

Disconnecting the process-tree connection or deleting its context also releases
the lease automatically.

## 4. Debug an extension host

Find the `[extension-host]` directly below the desired window:

```text
window 7  linkrpc
`- 15388  extension-host  [extension-host]
```

Create a context and attach:

```powershell
dbgjs context create --context linkrpc-ext-host "LinkRPC extension host" --set
dbgjs process attach p:15388 --set
```

## 5. Debug the agent host

Find the process marked `[agent-host]`:

```text
24664  agent-host  [agent-host]
```

Create a context and attach:

```powershell
dbgjs context create --context vscode-agent-host "VS Code agent host" --set
dbgjs process attach p:24664 --set
```

## 6. Inspect sources and set a breakpoint

List the sources known to the selected context:

```powershell
dbgjs source list
```

Search projected authored and generated sources:

```powershell
dbgjs source grep 'hubRpcConnection'
```

Human output shows each occurrence as a source excerpt: one `path:line:column`
header, followed by numbered lines with `>` marking the matching line. Use
`--context-lines <count>` to include surrounding lines. A blank line separates
occurrences, including distinct matches on the same source line.

Use `--path` to select logical paths before content is loaded and
`--timeout-ms` to bound the complete search. JSON matches include content
identity, provenance, endpoint applicability, and match length; identical
content is searched once and then fanned out to each logical source. Skipped
sources include their path, kind, target identity, and reason in JSON and human
output. If every candidate source is skipped, human output explicitly reports
the search as incomplete rather than presenting an empty result as a successful
no-match.

Use a source URL or projected source path from those results:

```powershell
$sourceUrl = 'file:///path/from/source-list/hubRpcConnection.ts'
dbgjs breakpoint set connection-send $sourceUrl 120 --column 1
```

Inspect the current target after the breakpoint binds or pauses:

```powershell
dbgjs target show
dbgjs target eval 'someExpression'
dbgjs target watch 'someExpression'
dbgjs target step over
dbgjs target resume
```

If a source map points to an authored file whose content is unavailable, dbgjs
keeps the target paused and usable. The frame falls back to its generated
JavaScript location and includes a warning with the mapped authored location.
This is a presentation-layer limitation; evaluation, stepping, and resume
continue to use the live CDP session.

## 7. Coverage, CPU profiles, and heap analysis

These commands are transport-independent and work for renderer, extension-host,
and agent-host targets:

```powershell
# Precise JavaScript coverage
dbgjs coverage start
# Collection-only lower bound: no source/map lookup or symbol enrichment
dbgjs --json coverage capture --raw
dbgjs coverage capture --id baseline
dbgjs coverage stop --id after-click --exclude baseline
dbgjs coverage show after-click --all
dbgjs coverage show . --path-glob '**/contrib/issue/**'
dbgjs coverage show .2 --target renderer-4

# CPU profile
dbgjs profile start
dbgjs profile stop --id startup
dbgjs profile show startup --view functions --sort self

# Heap classes and retention
dbgjs heap classes --capture --sort-by-instances --max-lines 40
dbgjs heap select --name HubRpcConnection --limit 20 --dominators
$objectRef = '.#12345'
dbgjs heap refs $objectRef --both --all-edges --limit 30
dbgjs heap retainer-path $objectRef
dbgjs heap dominators $objectRef
```

Heap object references are capture-qualified. `.` selects the latest heap capture,
so `.#12345` means heap object `12345` in the latest capture.

Object inspection (`value`, `target eval`, and heap node views) includes source
locations when available. The shared source resolver prefers authored source-map
positions, otherwise the current formatted projection or generated source. Paths
are printed in full so they can be passed back to `source show`. JSON retains both
generated and resolved positions, breadcrumbs, provenance, and mapping diagnostics
under `source`.

Heap locations use captured script/source-map data and follow bound-function and
prototype/constructor links within a bounded traversal. For directly selected heap
objects, the debugger also attempts a live comparison on the same target. Live
inspection reads `[[FunctionLocation]]`, follows bound targets, and inspects
constructors without invoking property getters. These extra lookups share a budget
of eight lookup requests and 750 ms per inspection; source acquisition also respects
the deadline. Temporary remote handles are released. Unavailable objects, exhausted
budgets, and mapping failures are reported without discarding snapshot evidence.
Disagreeing positions are retained and explicitly marked as a source conflict.
Matching heap/live locations are printed once, preferring the snapshot entry;
JSON retains both pieces of evidence. Live-edited code or changed source maps can
make captured and live locations disagree, so live comparison is not skipped.
Function and constructor locations are distinguished; neither is an allocation stack.
Reference endpoints also include captured locations (`sourceLocations` and
`targetLocations` in JSON), without additional live lookups for every edge.
Heap object previews show a few shallow properties, string prefixes, and prototype
information alongside the original capture-qualified IDs; they never invoke getters.

Capture IDs are immutable. Omitted IDs generate fresh names; `.` and `.1` select
the latest capture of the requested kind across the context, and `.2` selects the
previous one. An explicit target filter narrows that history before selection.
Stored reads do not inherit the selected live target.

Coverage `--path-prefix` matches from the beginning of normalized source URLs.
Use `--path-glob '**/issue/**'` for a directory anywhere in a URL. These filters
also inspect authored ranges in bundled scripts. The old `--path` spelling is a
deprecated prefix alias, not a substring search.

`coverage capture --raw` retains execution counts and runtime ranges while
skipping capture-time source acquisition and enrichment. It also accepts
`--id` and `--exclude`. Without `--raw`, named and unnamed captures are both
enriched. Stored captures are immutable; `coverage show --no-cache` is not
supported. Coverage operations still pending after 20 seconds print
a one-time stderr hint about this mode without interrupting the operation or
mixing progress text into JSON stdout.

For a reproducible installed-VS-Code workload and independent breadcrumb replay,
see [the coverage benchmark](vscode-coverage-benchmark.md).

Heap captures also retain the observed script URLs and CDP hashes, generated
source, source-map URLs/content, connection generation, and execution-context
and owning-frame metadata. `heap classes <name>` uses these captured inputs,
including after disconnecting or restarting the service; it never substitutes
scripts from the current target. Constructor groups remain separate across
scripts and display frame/context labels when available.

JSON analysis includes per-script `scriptMappings` with the captured hash and
one of `notAttempted`, `noMapSupplied`, `mapLoadingFailed`, or `mapped`, plus
failure reasons. Legacy captures without metadata remain readable and explicitly
report mapping as not attempted. Missing production maps are not diagnosed as
network failures. Source-map hydration timing records capture-time work, not a
new fetch during offline analysis.

For bundles distributed without maps, supply a matching build artifact:

```powershell
dbgjs heap classes baseline --json
dbgjs heap supply-map baseline 200 <captured-script-hash> .\build\editor.js.map
dbgjs heap classes baseline
```

The supply command persists the map with that capture, rejects mismatched or
missing captured hashes, malformed/unsupported maps, and a `file` field naming
a different generated bundle. The hash is an explicit association to the
captured script: source maps do not themselves cryptographically attest their
generated input. Use a map from the same build. No map or source is fetched from
the network during offline analysis. Maps without `sourcesContent` can still
project positions, but cannot recover authored constructor names.

`heap classes --no-cache` is explicitly rejected. Capture a new snapshot to
refresh metadata; supplying a map updates only the selected stored capture.
New captures retry previously failed or invalid source-map acquisitions with
the map cache bypassed, even if the generated source was already resolved.
This does not change the metadata or mapping results of older captures.

## 8. Disconnect and clean up

`process attach` creates deterministic connection IDs:

- Renderer: `process-tree-<vscode-main-pid>`
- Extension host or agent host: `process-<pid>`

Repeating an attach reports an ownership conflict. Use `--force` only when the
existing debugger owner should be detached and replaced; successful output
distinguishes `created` from `stolen`.

Disconnect only one connection:

```powershell
dbgjs connection disconnect --context linkrpc-renderer --connection process-tree-33508
dbgjs connection disconnect --context linkrpc-ext-host --connection process-15388
dbgjs connection disconnect --context vscode-agent-host --connection process-24664
```

Delete durable context state when it is no longer useful:

```powershell
dbgjs context delete --context linkrpc-renderer
dbgjs context delete --context linkrpc-ext-host
dbgjs context delete --context vscode-agent-host
```

Stop the complete local debugger service and close all live transports:

```powershell
dbgjs service stop
```

Each renderer attachment is owned by a loopback socket. Normal shutdown waits
for debugger cleanup before closing the socket; forced service termination also
releases the attachment when the operating system closes the socket.

## 9. How a process-tree connection works

`dbgjs process attach <pid>` connects to a *virtual browser root*: a CDP
endpoint dbgjs synthesizes for the process tree below `<pid>`. It speaks the
same `Browser`/`Target` subset a real Chrome browser endpoint speaks, so nothing
above the transport needs to know whether a connection is backed by Chrome, by
an OS process tree, or by an Electron application:

- `Browser.getVersion` reports `Process <pid>` as its product.
- `Target.getTargets` enumerates the main Node target (`$node-root`), every
  debuggable Node descendant, and — for Electron applications — every live
  renderer.
- `Target.setDiscoverTargets` streams `Target.targetCreated`,
  `Target.targetInfoChanged`, and `Target.targetDestroyed`.
- `Target.attachToTarget` returns a flattened session id; all target-scoped
  traffic is then plain CDP.

Chrome connections keep using Chrome's own browser endpoint; only hosts without
one get a virtual root.

### Demand-driven discovery

Discovery costs real work (polling the OS for descendants, activating
inspectors), so it only runs while a client asks for it:

- `Target.getTargets` is one-shot. It scans once and leaves discovery off.
- `Target.setDiscoverTargets(discover=true)` and
  `Target.setAutoAttach(autoAttach=true)` each raise demand; descendant polling
  starts when the first one does and stops when the last one drops.

Note that the debugger service itself enables `Target.setDiscoverTargets` for
the lifetime of a connection so that `dbgjs target list` stays live. A connected
process tree therefore does poll continuously in practice — but the polling is
now a consequence of an explicit CDP demand, and it stops as soon as discovery
is turned off or the connection closes.

Electron renderer discovery is never polled. The bridge dbgjs installs in the
Electron main process subscribes to `app`'s `web-contents-created` and each
`webContents`' own navigation, title, and `destroyed` events, and pushes them to
the virtual root. Renderer target ids are `renderer-<webContentsId>`.

### Waiting for the debugger

`Target.setAutoAttach(waitForDebuggerOnStart=true)` arms genuine startup
blocking for renderers created from that point on. The bridge attaches
Electron's debugger from the `web-contents-created` event — before the page runs
any script — and issues `Page.waitForDebugger`. Chromium only answers that
command once the renderer is resumed, so a still-pending response is proof that
startup is actually paused; `waitingForDebugger` is reported as `true` only in
that case, and a command that is rejected instead reports `false`. Attaching a
debugger session and then resuming, disarming the flag, or disposing the bridge
all continue the renderer through `Runtime.runIfWaitingForDebugger`; a safety
timer also releases renderers that nobody attached to.

Limitations: blocking applies to renderers created after the flag is armed —
existing renderers are already running — and Electron must reach the
`web-contents-created` listener before the renderer's first script, which is
guaranteed for webContents created by the application but not for a renderer
that is already mid-navigation when the bridge is installed.
