# dbgjs VS Code prototype

This extension is an architectural prototype for a lightweight TypeScript DAP
projection over the long-lived Rust `dbgjs-service`.

## Implemented

- One path-identified dbgjs context per single-folder VS Code workspace, shared
  with terminal commands run from that folder.
- Automatic startup and direct HubRPC connection to the daemon's authenticated
  local endpoint.
- A target explorer that shows every connection and a canonical parent/opener
  hierarchy for its discovered targets.
- An inline DAP adapter with stack traces, stepping, resume, breakpoints,
  loaded sources, virtual source content, expandable frame scopes, Variables,
  and Watch/Debug Console evaluation.
- One VS Code debug session per dbgjs target, with descendant targets represented
  as nested debug sessions. Each concrete session exposes one synthetic DAP thread.
- Launch configurations for:
  - Node.js programs, using a daemon-owned inspector process and a
    synthetic root debugger target.
  - Playwright browsers, resolving `playwright/index.mjs` from the workspace's
    `node_modules` by default.
  - Installed Chrome or Chromium, discovered from standard platform locations
    or selected explicitly with `executablePath`.
- Windows process-tree discovery for an already-running Electron or Node process,
  including inspector activation for descendant Node processes and Chromium
  renderer, worker, and webview discovery when the root exposes a browser CDP
  endpoint.
- Current-editor overlay tracking with immutable document versions and
  UTF-16 `LengthEdit` records.
- Unit tests and an Electron integration test that activates the extension
  against a real `dbgjs-service`.

The daemon publishes the authoritative target forest. The extension reconciles
root sessions for the workspace, while each DAP adapter starts nested sessions
for its direct descendants.

On activation, the extension requires exactly one workspace folder, ensures that
`dbgjs-service` is running, and creates or retrieves the context whose identity
is that folder's lexically normalized, lowercased absolute path. It does not
query filesystem identity or resolve symlinks. Multi-root and untitled workspace
selection is intentionally deferred and reported as an error. The
endpoint state file is only used internally to discover the daemon's local
transport and authentication token.

## Diagnostics

Select **dbgjs** in VS Code's **Output** panel to inspect daemon startup,
transport lifecycle, and raw HubRPC messages in both directions. The
authentication preamble is intentionally excluded. Request and response payloads
may include evaluated expressions or source content.

## Launch configurations

```jsonc
{
  "type": "dbgjs",
  "request": "launch",
  "name": "Node.js",
  "runtime": "node",
  "program": "${workspaceFolder}/server.js",
  "args": ["--port", "3000"],
  "runtimeArgs": [],
  "env": { "NODE_ENV": "development" }
}
```

```jsonc
{
  "type": "dbgjs",
  "request": "launch",
  "name": "Compiled TypeScript",
  "runtime": "node",
  "program": "${workspaceFolder}/dist/server.js",
  "preLaunchTask": "Compile TypeScript"
}
```

```jsonc
{
  "type": "dbgjs",
  "request": "launch",
  "name": "TypeScript with tsx",
  "runtime": "node",
  "runtimeArgs": ["--import", "tsx"],
  "program": "${workspaceFolder}/server.ts"
}
```

```jsonc
{
  "type": "dbgjs",
  "request": "launch",
  "name": "Playwright",
  "runtime": "playwright",
  "url": "http://localhost:3000",
  "headless": true
}
```

```jsonc
{
  "type": "dbgjs",
  "request": "launch",
  "name": "Chrome",
  "runtime": "chrome",
  "url": "http://localhost:3000",
  "headless": false
}
```

For a hosted page, the same Chrome runtime can launch `"url":
"https://vscode.dev"`. Local absolute paths are normalized to `file:` URLs.
Runnable examples, including their TypeScript tasks and dependencies, are in
`test/workspace/.vscode`.

Every launched runtime is represented by an ephemeral connection in the
workspace context. The daemon owns its process tree. DAP disconnect and window
reload only detach the VS Code presentation; an explicit DAP terminate stops the
owned process and removes the connection. An `attach` configuration with
`"runtime": "context"` restores pre-existing context targets.

Node launches preload a small discovery hook through `NODE_OPTIONS`. Descendant
Node processes that inherit the launch environment open loopback inspectors and
appear as child targets under their spawning process. The daemon reports each
process, and an explicit target or process attach claims its debugger.
Processes that discard the inherited environment remain ordinary, undiscovered
OS processes.

An already-running Node-compatible inspector can be added to a context without
launching another process:

```powershell
dbgjs connection add --node-inspector <ws-endpoint> --context <context> --connection <connection> --connect
dbgjs target attach --context <context> --connection <connection> --target node
```

Attachments have one owner. A second attach reports an ownership conflict;
`--force` explicitly detaches the prior owner where supported and reports a
`stolen` outcome.

This direct-debugger connection is also used for VS Code extension hosts after
their inspector has been activated. It intentionally differs from the ordinary
WebSocket form, which expects a browser-level `Target` endpoint.

Running VS Code instances can first be discovered from OS process metadata,
without creating a debugger context or opening any inspector:

```powershell
dbgjs process list --vscode
dbgjs process list --vscode --no-cmd-line
dbgjs process list --vscode --stats
dbgjs process list --vscode --filter "window 3"
dbgjs process list --vscode --no-trim
dbgjs --json process list --vscode
```

The result is a snapshot of every detected VS Code root and all of its
descendants, including renderers, extension hosts, Node utilities, language
servers, TypeScript servers, agent processes, terminals, and deeper children.
The tree includes non-JavaScript processes when they connect attachable
descendants to their actual launcher. These ancestry-only nodes are explicitly
non-attachable and highlighted on interactive color terminals. Window-owned
processes are grouped under virtual window nodes. `--filter`
selects a matching tree path while retaining its ancestors and descendants.
`--stats` adds a sampled CPU percentage and resident-memory size.
`--no-trim` disables terminal-width trimming.
This is the fast investigation entry point; it does not attach to anything or
start the debugger service.

The older process-tree connection imports one selected VS Code instance into a
debugger context:

```powershell
dbgjs connection add --process-tree <electron-main-pid> --context <context> --connection vscode --connect
```

Unlike `process list`, this connection currently activates every compatible
descendant. Existing inspector endpoints are reused. For Node processes without
an endpoint, the provider serially activates the inspector through
`process._debugProcess(pid)`, uses the default port only for the activation
handshake, and immediately moves the inspector to an ephemeral loopback port.
Per-target activation is the intended replacement for this bulk-activation
behavior.

If the root process exposes Chromium remote debugging, its renderers, webviews,
workers, and shared workers join the same target forest. Target discovery and
CDP transport establishment do not by themselves install breakpoints; a target
debugger must still be attached before breakpoints or logpoints bind.

The process-tree provider currently requires Windows and a Node.js runtime with
built-in WebSocket support.

## Transport

The extension uses `@hediet/linkrpc` for its JSON-RPC connection and public
Node NDJSON transport, with typed interfaces generated from daemon reflection.
The generation script starts an isolated `dbgjs-service --stdio`, uses the
LinkRPC CLI to export its reflected endpoint contract, then generates the checked-in
TypeScript modules. The extension itself still uses the persistent authenticated
local daemon; stdio does not change its connection lifecycle.
`DbgServiceClient` groups these generated clients into readonly `service`,
`contexts`, `sources`, `captures`, `targets`, `cdp`, `relay`, `browser`,
`coverage`, `cpu`, and `heap` facets on one caller-owned `LinkRpcConnection`.
For example, `client.contexts.list_contexts({ cwd: null })` and
`client.heap.capture_heap_snapshot(params, { onMessage })` share the same
connection; streamed calls retain their generated cancellation support.
See [RPC contracts and code generation](../docs/architecture/contracts.md) for ownership,
regeneration, drift checks, and development links. The daemon still expects the one-line
`{"hello":1,"token":"..."}` preamble before JSON-RPC traffic, so the extension
writes that preamble once and creates the NDJSON transport without LinkRPC's
optional initialize handshake.

The upstream NDJSON transport deliberately skips malformed JSON lines. Valid
JSON-RPC traffic is traced through LinkRPC's public trace callback; the
authentication preamble (and therefore its token) is not traced.

## Daemon API gaps exposed by the prototype

- Source APIs are path-based rather than provider-, snapshot-, and
  version-qualified.
- Runtime scripts expose URLs but no complete
  connection/attachment/script-incarnation identity.
- No APIs publish filesystem/editor snapshots or edit projections.
- No source-presentation revision/delta API.
- No explicit pause, exception-policy, or structured output APIs needed for
  complete DAP support.
- The context target snapshot contains only parent/opener/browser-context
  fields, not the full typed relation graph from the data model.

`EditorOverlayTracker` records the data that should eventually be sent through
an atomic source-graph mutation API. It intentionally does not invent a
path-based daemon call for data the current API cannot represent correctly.

## Validation

```console
npm install
npm test
npm run test:vscode
```

The Electron test builds and starts the real Rust daemon, launches VS Code with
an isolated fixture workspace, and sequentially launches Node.js, workspace
TypeScript through `tsc` and `tsx`, Playwright, and Chrome sessions. It verifies
nested Node child sessions, one synthetic thread per target session, frame
scopes, recursively expandable Watch values, cleanup, and daemon survival
after the VS Code window exits. On a headless Linux host, run it as
`xvfb-run -a npm run test:vscode`.
