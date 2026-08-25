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
`- 33508  Code - Insiders.exe  [vscode-main]
   |- window 7  linkrpc
   |  |- 43336  renderer  [renderer]
   |  `- 15388  extension-host  [extension-host]
   `- 24664  agent-host  [agent-host]
      `- 41552  Code - Insiders.exe  [copilot]
```

These PIDs are examples. Always use the values from the current process list.

Use the role label, window grouping, and PID together:

- `[renderer]`: the workbench DOM and browser JavaScript for that window.
- `[extension-host]`: extensions running for that window.
- `[agent-host]`: VS Code's shared agent-host runtime.
- `[copilot]`: a separate runner below the agent host; attach its own PID if
  that process, rather than the agent host, is the intended target.
- `session ...`: virtual metadata, not a process. Attach the containing
  `[agent-host]` or `[copilot]` PID instead.

Use a separate context for each process in the introductory workflows below.
That keeps `$node-root` unambiguous for shorthand commands.

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
jsdbg process attach 43336 --set
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
- If the owner is unknown, do not force-detach it.

`renderer process ... has no live Electron webContents`

- Re-run process discovery. The window may have reloaded or closed.
- Use the new renderer PID and process target ID.

`renderer process ... maps to multiple Electron webContents`

- jsdbg refuses to guess. Inspect the live `WebContents` set and select an
  explicit target when that workflow is available.

## 4. Debug an extension host

Find the `[extension-host]` directly below the desired window:

```text
window 7  linkrpc
`- 15388  extension-host  [extension-host]
```

Create a context and attach:

```powershell
jsdbg context create --context linkrpc-ext-host "LinkRPC extension host" --set
jsdbg process attach 15388 --set
```

## 5. Debug the agent host

Find the process marked `[agent-host]`:

```text
24664  agent-host  [agent-host]
```

Create a context and attach:

```powershell
jsdbg context create --context vscode-agent-host "VS Code agent host" --set
jsdbg process attach 24664 --set
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
