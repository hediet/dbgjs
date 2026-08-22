# jsdbg VS Code prototype

This extension is an architectural prototype for a lightweight TypeScript DAP
projection over the long-lived Rust `jsdbg-service`.

## Implemented

- One stable jsdbg context per VS Code workspace.
- Direct HubRPC connection to the daemon's endpoint file.
- A target explorer that shows every connection and a canonical parent/opener
  hierarchy for its discovered targets.
- An inline DAP adapter with threads, stack traces, stepping, resume,
  evaluation, breakpoints, loaded sources, and virtual source content.
- Launch configurations for:
  - Node.js programs, using a daemon-owned inspector process and a
    synthetic root debugger target.
  - Playwright browsers, resolving `playwright/index.mjs` from the workspace's
    `node_modules` by default.
  - Installed Chrome or Chromium, discovered from standard platform locations
    or selected explicitly with `executablePath`.
- Current-editor overlay tracking with immutable document versions and
  UTF-16 `LengthEdit` records.
- Unit tests and an Electron integration test that activates the extension
  against a real `jsdbg-service`.

The target explorer is authoritative for the complete context. The DAP adapter
currently projects attached targets as threads; a later VS Code-specific layer
can replace this with parent/child debug sessions.

## Launch configurations

```jsonc
{
  "type": "jsdbg",
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
  "type": "jsdbg",
  "request": "launch",
  "name": "Playwright",
  "runtime": "playwright",
  "url": "http://localhost:3000",
  "headless": true
}
```

```jsonc
{
  "type": "jsdbg",
  "request": "launch",
  "name": "Chrome",
  "runtime": "chrome",
  "url": "http://localhost:3000",
  "headless": false
}
```

Every launched runtime is represented by an ephemeral connection in the
workspace context. The daemon owns its process tree; DAP disconnect stops the
process and removes the connection. An `attach` configuration with
`"runtime": "context"` continues to expose pre-existing context targets.

Node launches preload a small discovery hook through `NODE_OPTIONS`. Descendant
Node processes that inherit the launch environment open loopback inspectors and
appear as child targets under their spawning process. The daemon connects and
auto-attaches each reported process, so VS Code exposes it as another DAP
thread. Processes that discard the inherited environment remain ordinary,
undiscovered OS processes.

## Transport finding

`@vscode/hubrpc@0.0.2-0` contains the compatible HubRPC connection core, but its
published Node connector sends the newer `hubrpc::initialize` handshake. The
daemon currently expects the older one-line `{"hello":1,"token":"..."}` preamble.
`legacyHubTransport.ts` is the small compatibility bridge.

## Daemon API gaps exposed by the prototype

- Source APIs are path-based rather than provider-, snapshot-, and
  version-qualified.
- Runtime scripts expose URLs but no complete
  connection/attachment/script-incarnation identity.
- No APIs publish filesystem/editor snapshots or edit projections.
- No source-presentation revision/delta API.
- No scope, variable, explicit pause, exception-policy, or structured output
  APIs needed for complete DAP support.
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
Playwright, and Chrome sessions. It verifies that each session exposes an
attached DAP thread and removes its daemon connection on disconnect. On a
headless Linux host, run it as `xvfb-run -a npm run test:vscode`.
