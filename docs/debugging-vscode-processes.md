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
jsdbg process list --vscode --no-cmd-line --no-trim
```

A typical result contains:

```text
VS Code process tree 33508
`- p:33508  Code - Insiders.exe  [vscode-main]
   |- w:33508/7  window  linkrpc
   |  |- p:43336  renderer  [renderer]
   |  `- p:15388  extension-host  [extension-host]
   `- p:24664  agent-host  [agent-host]
      `- p:41552  Code - Insiders.exe  [copilot]
```

These references are examples. `p:<pid>` selects a process and
`w:<vscode-main-pid>/<window-id>` selects a VS Code window. Raw PIDs remain
accepted for compatibility. JSON output also includes the full discovery
locator for each process.

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

- jsdbg refuses to guess. Inspect the live `WebContents` set and select an
  explicit target when that workflow is available.

### Inspect nested renderer targets

While a process-tree connection is being observed, jsdbg subscribes to each
renderer endpoint's `Target` domain. OOPIFs and workers therefore appear in the
same target inventory instead of requiring a separate raw-CDP query:

```powershell
jsdbg target list --type iframe
jsdbg target graph
jsdbg target attach --target <printed-target-id> --set
```

Browser debug ports compose the same way: the browser endpoint is published
once and its pages, OOPIFs, and workers are contributed as nested targets.

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
jsdbg process list --vscode --no-cmd-line
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
