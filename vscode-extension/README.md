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
- Current-editor overlay tracking with immutable document versions and
  UTF-16 `LengthEdit` records.
- Unit tests and an Electron integration test that activates the extension
  against a real `jsdbg-service`.

The target explorer is authoritative for the complete context. The DAP adapter
currently projects attached targets as threads; a later VS Code-specific layer
can replace this with parent/child debug sessions.

## Transport finding

`@vscode/hubrpc@0.0.2-0` contains the compatible HubRPC connection core, but its
published Node connector sends the newer `hubrpc::initialize` handshake. The
daemon currently expects the older one-line `{"hello":1,"token":"..."}` preamble.
`legacyHubTransport.ts` is the small compatibility bridge.

## Daemon API gaps exposed by the prototype

- Source APIs are path-based rather than provider-, snapshot-, and
  version-qualified.
- Runtime scripts expose URLs but no connection/attachment/script incarnation.
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
the fixture workspace, launches and attaches a real Playwright browser target,
verifies context creation, and checks that the inline jsdbg debug session
exposes the target. On a headless Linux host, run it as
`xvfb-run -a npm run test:vscode`.
