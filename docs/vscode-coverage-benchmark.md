# Installed VS Code coverage baseline

This benchmark reproduces slow coverage projection against a **globally installed
desktop VS Code**, then freezes the generated-source breadcrumb workload for
offline replay. It does not require a VS Code checkout, vscode.dev, or a browser
download. It uses the public `dbgjs coverage` and `dbgjs source` commands, with
`dbgjs playwright` for the UI interaction. It does not fall back to raw CDP
coverage.

## Capture once

From the repository root:

```powershell
npm run benchmark:vscode-coverage -- --output artifacts\vscode-coverage-before
```

Requirements: Node.js 22+, Git, the repository's Rust toolchain and npm
dependencies, and an installed desktop VS Code. The runner builds the debug
CLI, service, and replay example. A desktop session is required.

Stable VS Code is discovered in common installation locations. For another
installation, pass the **native executable**, not the `code.cmd` shell wrapper:

```powershell
npm run benchmark:vscode-coverage -- --code "C:\Users\you\AppData\Local\Programs\Microsoft VS Code\Code.exe" --output artifacts\vscode-coverage-before
```

`DBGJS_CODE_EXECUTABLE` is an alternative to `--code`. Other options:

- `--profile debug|release`: default `debug`, matching the original slow run.
- `--skip-build`: use already-built CLI, service, and example binaries.
- `--timeout-ms 300000`: timeout per terminal command, including building and replay.

The output directory must not already exist; a new capture never overwrites a
baseline. The runner creates its own Git fixture, VS Code user-data, shared-data,
agent-data and extension directories, source-map cache, and debugger service. It does not alter an existing
workspace or user profile. It removes inherited per-process Git configuration
overrides from its child environment so empty `GIT_CONFIG_VALUE_*` entries cannot
break the Git extension.

The workload is a two-file multi-diff. The runner clicks **Revert Block**, captures
both unprojected and projected coverage, and checks that `revertRangeMappings`
executed and has an authored TypeScript location. It also exercises source
explanation, source display, and position mapping.

The owned VS Code process and debugger service are stopped, including on failure.
The runtime directory is deliberately retained for diagnosis; it can be removed
once no longer needed. It contains local service credentials: **do not publish
`runtime/` or commit generated output**. The default `artifacts/` directory is
gitignored.

## Replay without VS Code

The capture command automatically runs the first isolated replay after closing
VS Code. To measure a code change using exactly that frozen workload:

```powershell
cargo run --example vscode_breadcrumbs -- artifacts\vscode-coverage-before\workload.json --baseline artifacts\vscode-coverage-before\replay-baseline.json --iterations 2
```

For a release-to-release comparison, capture with `--profile release` and replay
with `cargo run --release --example vscode_breadcrumbs`. Do not compare debug
timings to release timings as evidence of an algorithmic improvement.

The replay calls the production
[`SymbolIndex`](../src/language_intelligence.rs) by identity; no breadcrumb
implementation is copied into the benchmark. It builds the index once, times
each complete lookup pass separately, and verifies that all passes return
identical strings and `null` results. `--baseline` additionally checks those
results against the frozen baseline, in order. The bundle SHA-256 and workload
SHA-256 prevent accidentally replaying or comparing a different build.

The workload preserves repeated positions and zero-hit functions: live coverage
projection can request generated breadcrumbs for those too. It is specifically
the generated-fallback subset, not a benchmark of every part of coverage
projection.

## Output

| File | Purpose |
|---|---|
| `workload.json` | Versioned lookup positions, VS Code commit, and bundle SHA-256 |
| `workbench.desktop.main.js` | Exact installed bundle, frozen against VS Code auto-updates |
| `replay-baseline.json` | Index-build time, per-pass times, build mode, and exact lookup results |
| `capture-report.json` | Installed VS Code identity, dbgjs revision/binary hashes, machine information, workload size, and mapped revert evidence |
| `commands.jsonl` | Exact executable/argument vectors, terminal wall times, exit codes, and output log paths |
| `timings.json` | Completed run's command timing records |
| `logs/` | Untruncated command output, including both coverage captures |
| `runtime/` | Disposable workspace/profile/service state retained for diagnosis |

Compare `Capture without projection` with `Capture with mapping and breadcrumbs`
in the command log. Those times include CLI startup, IPC, serialization, and
output collection. The replay's `lookupSeconds` isolates production breadcrumb
lookup from browser startup, source-map loading, coverage collection, and output
serialization. No machine-dependent speed threshold is imposed yet: this is the
baseline against which an optimization can be developed.

Live captures vary with VS Code version and background activity. For controlled
before/after comparisons, **reuse one frozen workload**. A fresh capture can be
used to check representativeness, but its timing is not an identical workload.

## Fast checks

```powershell
npm run test:vscode-coverage-benchmark
cargo test --example vscode_breadcrumbs
```

These validate workload selection, preserved duplicates, environment isolation,
bundle integrity, UTF-16 positions, and exact-result comparison without launching
VS Code. The desktop capture is deliberately opt-in, not part of the default
test suite.

## Initial measured baseline

On 2026-09-14, installed VS Code **1.137.0**, commit
`645f29cc3176500b4b5762ba887cf2a7f0ffdf2c`, reproduced this workload on Windows
x64 with an Intel Core i9-14900K, using the dbgjs debug build:

| Measurement | Result |
|---|---:|
| Capture without projection | 0.689 s |
| Capture with mapping and breadcrumbs | 57.001 s |
| Frozen generated-fallback lookups | 545 |
| Replay index construction | 1.929 s |
| Replay lookup pass 1 | 35.521 s |
| Replay lookup pass 2, same index | 35.837 s |
| Separate replay with exact baseline comparison | Passed; 545 identical results |

These are observations, not performance assertions. The measured artifacts were
written to `artifacts/vscode-coverage-baseline-20260914-v3`; use a new output
directory for a new capture.
