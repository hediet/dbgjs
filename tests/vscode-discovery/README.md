# Deterministic VS Code discovery and source maps

This short desktop E2E downloads pinned VS Code **1.137.0**, launches an isolated
profile with a tiny TypeScript development extension, and exercises the native
`dbgjs` CLI. It never connects to the developer's existing VS Code session.

The scenario proves:

1. OS process discovery finds the launched VS Code forest and extension host.
2. Attaching the discovered PID reaches the extension host that activated the
   fixture (verified against a separate readiness file and runtime PID).
3. A breakpoint requested in authored TypeScript installs through a source map.
4. The original TypeScript content is recovered from the map.
5. Executing the fixture pauses at the exact authored line and column.
6. A local variable evaluates correctly while paused, and execution completes
   after resume.

`expected.json` is a checked-in golden transcript shared by every platform.
It preserves actual roles, values, source text, mapping requests, and authored
stack locations. It deliberately excludes timestamps, runtime IDs, PIDs,
installation paths, and unrelated VS Code scripts. The runtime PID and binding
invariants are asserted before selecting transcript fields, not normalized into
success-shaped placeholders.

Source maps belong to the local fixture, so this does not depend on changing
VS Code CDN maps, user extensions, coverage counts, or arbitrary timing sleeps.
Readiness and debugger state are awaited with bounded deadlines.

## Run

```sh
npm ci
npm run test:e2e:vscode-discovery -- --bin-dir target/release
```

On Linux, run through `xvfb-run -a` when no display is available.
The supplied directory must contain native `dbgjs` and `dbgjs-service`
executables. The test does not compile Rust. CI downloads the matching
candidate artifact and runs on Windows x64, Linux x64/ARM64, and macOS
x64/ARM64. This is a small routine sanity test, not the full macOS Rust suite.

The downloaded VS Code installation is cached locally under
`artifacts/vscode-download`. Each run uses separate temporary user data,
extensions, workspace, copied CLI binaries, service state, and source-map cache.
Only processes launched by the test are terminated during cleanup.

Diagnostics go to `artifacts/vscode-discovery`: raw `commands.jsonl`,
`vscode.log`, and deterministic `actual.json`. CI uploads these diagnostics
even when a test fails.

To intentionally regenerate the golden after reviewing a behavior change:

```sh
npm run test:e2e:vscode-discovery -- --bin-dir target/release --update
```

Run once again without `--update` to verify the new golden on a fresh instance.
