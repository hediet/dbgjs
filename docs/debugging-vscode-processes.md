# Debugging VS Code Processes with jsdbg

This guide shows how to attach jsdbg to an already-running VS Code renderer,
extension host, or agent host from scratch.

Existing VS Code process discovery and inspector activation are currently
implemented on Windows.

The workflow is the same for all three runtime types:

1. List the VS Code process tree.
2. Choose a process by its window and role.
3. Create a debug context.
4. Attach the process PID.
5. Debug the selected target.

jsdbg handles the transport differences automatically.

## 1. Build jsdbg

From the cdp-client repository:

```powershell
cargo build --bins
$env:PATH = "$PWD\target\debug;$env:PATH"
```

The first command that needs the debugger service starts `jsdbg-service`
automatically. By default, its endpoint and persistent contexts are stored
under:

```text
%LOCALAPPDATA%\hediet\cdp-client\
```

## 2. Discover the VS Code process tree

```powershell
jsdbg process list --root vscode --no-cmd-line --no-trim
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
jsdbg process list --root vscode --full --no-cmd-line
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
jsdbg process attach vscode://33508/process/43336 --set
jsdbg process attach vscode://33508/window/7 --set
```

Non-VS Code roots use the corresponding process-tree locator:

```powershell
jsdbg process attach process-tree://12496/process/12496 --set
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

Create a context and attach the renderer PID:

```powershell
jsdbg context create --context linkrpc-renderer "LinkRPC renderer" --set
jsdbg process attach p:43336 --set
```

Capture the current renderer viewport in the system temporary directory:

```powershell
jsdbg screenshot capture
```

Use `--output <path>` to choose the destination.

### Renderer attachment failures

`Electron webContents ... is already attached by another debugger`

- Stop another active renderer debugger, such as
  `Developer: Debug Renderer in New Window`, and retry.
- Or explicitly use `process attach p:<pid> --force`. Only this opt-in path calls
  Electron's debugger detach before jsdbg attaches.

`renderer process ... has no live Electron webContents`

- Re-run process discovery. The window may have reloaded or closed.
- Use the new renderer PID and process target ID.

`renderer process ... maps to multiple Electron webContents`

- jsdbg refuses to guess. The error prints qualified target attachment commands;
  choose the intended target from those candidates.

### Inspect nested renderer targets

While a process-tree connection is being observed, jsdbg correlates each
Electron WebContents with its native CDP target and enables related-target
auto-attach on that page. Its OOPIFs and workers therefore appear beneath the
correct renderer without copying the browser-global target inventory beneath
every renderer:

```powershell
jsdbg target list --type iframe
jsdbg target graph
jsdbg target attach --target <printed-target-id> --set
jsdbg screenshot capture --output iframe.png
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
jsdbg context create --context vscode-startup "VS Code startup" --set
jsdbg process attach p:33508 --set
jsdbg connection pause-future on
```

New Electron WebContents are discovered from the main process event stream and
blocked in `Page.waitForDebugger` before page startup continues. After opening
the Agents window, list processes again and attach its `p:` or `w:` reference:

```powershell
jsdbg process list --root vscode --no-cmd-line
jsdbg process attach w:33508/9 --set
```

Attaching adopts the reserved renderer transport. Disable the policy when no
more future renderers should be blocked:

```powershell
jsdbg connection pause-future off
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
jsdbg context create --context linkrpc-ext-host "LinkRPC extension host" --set
jsdbg process attach p:15388 --set
```

## 5. Debug the agent host

Find the process marked `[agent-host]`:

```text
24664  agent-host  [agent-host]
```

Create a context and attach:

```powershell
jsdbg context create --context vscode-agent-host "VS Code agent host" --set
jsdbg process attach p:24664 --set
```

## 6. Inspect sources and set a breakpoint

List the sources known to the selected context:

```powershell
jsdbg source list
```

Search projected authored and generated sources:

```powershell
jsdbg source grep 'hubRpcConnection'
```

Use `--path` to select logical paths before content is loaded and
`--timeout-ms` to bound the complete search. JSON matches include content
identity, provenance, endpoint applicability, and match length; identical
content is searched once and then fanned out to each logical source.

Use a source URL or projected source path from those results:

```powershell
$sourceUrl = 'file:///path/from/source-list/hubRpcConnection.ts'
jsdbg breakpoint set connection-send $sourceUrl 120 --column 1
```

Inspect the current target after the breakpoint binds or pauses:

```powershell
jsdbg target show
jsdbg target eval 'someExpression'
jsdbg target watch 'someExpression'
jsdbg target step over
jsdbg target resume
```

If a source map points to an authored file whose content is unavailable, jsdbg
keeps the target paused and usable. The frame falls back to its generated
JavaScript location and includes a warning with the mapped authored location.
This is a presentation-layer limitation; evaluation, stepping, and resume
continue to use the live CDP session.

## 7. Coverage, CPU profiles, and heap analysis

These commands are transport-independent and work for renderer, extension-host,
and agent-host targets:

```powershell
# Precise JavaScript coverage
jsdbg coverage start
jsdbg coverage capture --id baseline
jsdbg coverage stop --exclude baseline
jsdbg coverage show --all

# CPU profile
jsdbg profile start
jsdbg profile stop --id startup
jsdbg profile show startup --view functions --sort self

# Heap classes and retention
jsdbg heap classes --capture --sort-by-instances --max-lines 40
jsdbg heap select --name HubRpcConnection --limit 20 --dominators
$objectRef = '.#12345'
jsdbg heap refs $objectRef --both --all-edges --limit 30
jsdbg heap retainer-path $objectRef
jsdbg heap dominators $objectRef
```

Heap object references are capture-qualified. `.` selects the latest capture,
so `.#12345` means heap object `12345` in the latest capture.

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
jsdbg heap classes baseline --json
jsdbg heap supply-map baseline 200 <captured-script-hash> .\build\editor.js.map
jsdbg heap classes baseline
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
jsdbg connection disconnect --context linkrpc-renderer --connection process-tree-33508
jsdbg connection disconnect --context linkrpc-ext-host --connection process-15388
jsdbg connection disconnect --context vscode-agent-host --connection process-24664
```

Delete durable context state when it is no longer useful:

```powershell
jsdbg context delete --context linkrpc-renderer
jsdbg context delete --context linkrpc-ext-host
jsdbg context delete --context vscode-agent-host
```

Stop the complete local debugger service and close all live transports:

```powershell
jsdbg service stop
```

Each renderer attachment is owned by a loopback socket. Normal shutdown waits
for debugger cleanup before closing the socket; forced service termination also
releases the attachment when the operating system closes the socket.

## 9. How a process-tree connection works

`jsdbg process attach <pid>` connects to a *virtual browser root*: a CDP
endpoint jsdbg synthesizes for the process tree below `<pid>`. It speaks the
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
the lifetime of a connection so that `jsdbg target list` stays live. A connected
process tree therefore does poll continuously in practice — but the polling is
now a consequence of an explicit CDP demand, and it stops as soon as discovery
is turned off or the connection closes.

Electron renderer discovery is never polled. The bridge jsdbg installs in the
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
