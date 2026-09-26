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
- `--capture-only`: skip the later source explanation/display/reverse-mapping
  queries when profiling capture. Mapped-coverage assertions, workload freezing,
  cleanup, and offline replay still run.
- `--timeout-ms 300000`: timeout per terminal command, including building and replay.

The output directory must not already exist; a new capture never overwrites a
baseline. The runner creates its own Git fixture, VS Code user-data, shared-data,
agent-data and extension directories, source-map cache, and debugger service. It does not alter an existing
workspace or user profile. It removes inherited per-process Git configuration
overrides from its child environment so empty `GIT_CONFIG_VALUE_*` entries cannot
break the Git extension.
Each Playwright program installs the first-run welcome-dialog handler so a
dialog appearing after startup can also be dismissed during later interactions.

The workload is a two-file multi-diff. The runner clicks **Revert Block**, captures
raw coverage (`coverage capture`) and its enriched stored view (`coverage show`), and checks that `revertRangeMappings`
executed and has an authored TypeScript location. It also exercises source
explanation, source display, and position mapping. Raw captures must contain no
authored locations or breadcrumbs, and enrichment must preserve the measured
revert function's counts and runtime offsets.

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
[`SymbolIndex`](../../packages/dbgjs/src/source/language_intelligence.rs) by identity; no breadcrumb
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
| `logs/` | Untruncated command output, including the raw capture and enriched view |
| `runtime/` | Disposable workspace/profile/service state retained for diagnosis |

Compare `Capture without projection` with `View capture with mapping and breadcrumbs`
in the command log. Those times include CLI startup, IPC, serialization, and
output collection. The replay's `lookupSeconds` isolates production breadcrumb
lookup from browser startup, source-map loading, coverage collection, and output
serialization. No machine-dependent speed threshold is imposed yet: this is the
baseline against which an optimization can be developed.

## Native Rust sampling

The local `artifacts/rust-capture-profile/service.sleepy` capture was collected
with Very Sleepy 0.91 against the debug service and locally resolved Rust PDBs.
Symbol servers were disabled. The benchmark used `--skip-build --capture-only`;
sampling covered service setup, the historical raw/enriched capture workflow, and shutdown, but not the
later source queries or offline replay.

The profile contains 33,588 samples over 29.047 seconds. Very Sleepy samples
thread stacks, including blocked threads: its aggregated thread-time weights
are **not CPU time or command elapsed time**. The analysis excludes explicit
`Nt`/`Zw` wait, completion-port wait, delay, and signal-and-wait leaf functions.
This leaves 19.986 weighted seconds from 1,935.003 overall. The callstack leaf
totals agree with the separately saved flat IP counts within 0.002108 weighted
seconds of text-format rounding. Percentages below use the non-wait denominator;
inclusive rows must not be added to their children.

| Inclusive native stack | Share of non-wait weight |
|---|---:|
| `SymbolIndex::with_positions` | 24.21% |
| OXC `Parser::parse` (inside symbol construction) | 22.30% |
| SHA-256 compression, across callers | 23.68% |
| `sourcemap::decoder::decode_slice` | 13.27% |
| `serde_json::Value::clone` | 4.29% |

Before the digest-reuse follow-up below, the SHA-256 stacks showed a repeated
pass over the map bytes: cache writing contributed 7.03%, and interning the same
map contributed 8.56%.
Hashing embedded authored sources contributes 5.38%; generated source hashing
contributes 1.63%. The identified candidate was carrying the already-computed 32-byte
digest with immutable source-map data into content interning, without retaining
another map buffer or changing the digest algorithm. These sampled shares are
not a predicted wall-clock saving.

This is a debug-build profile, matching the existing baseline. Unoptimized SIMD
wrappers and Rust precondition checks are visible in self samples; an optimized
build needs its own profile before drawing release-performance conclusions.
The enriched command took 17.178 seconds while sampled, versus the previous
uninstrumented 12.370-second measurement. Profiler overhead makes those
inappropriate for a before/after performance comparison.

Very Sleepy 0.91 attaches to threads already present. For this recording only,
an environment-gated `rayon::broadcast(|_| ())` temporarily prewarmed workers at
service startup; that instrumentation was removed afterward. Later-created
threads can still be missed. The saved `.sleepy` file contains resolved symbol
names and source locations and does not require keeping that temporary build.

For a collection-only lower bound outside the harness:

```powershell
dbgjs coverage start
dbgjs --json coverage capture
```

Capture records raw ranges without source fetching, source-map lookup, or symbol
enrichment. Runtime script identifiers/URLs, UTF-16 source offsets, and
execution counts remain available. To project a capture, use
`dbgjs --json coverage show <capture-id>`. Capturing and stopping always retain
full immutable coverage; derive an excluded view with
`coverage show <capture> --exclude <baseline>` instead.

Coverage commands that have not completed after 20 seconds print a one-time
hint on **stderr** describing the current capture or view. The original operation
continues; the hint neither restarts nor cancels it. JSON remains on stdout.
The harness preserves separate stdout/stderr streams so the hint cannot corrupt
JSON parsing.

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

## Indexed lookup verification

The optimization was compared against the separate, unchanged
`artifacts/vscode-coverage-baseline-20260914-final` workload from the same VS Code
build, again using debug binaries:

| Measurement | Baseline | Indexed |
|---|---:|---:|
| Frozen lookup results | 646, including two `null` results | All 646 identical |
| Index construction | 2.051 s | 2.395 s |
| Complete lookup pass 1 | 52.441 s | 0.317 ms |
| Complete lookup pass 2 | 46.974 s | 0.099 ms |
| Complete lookup pass 3 | Not measured | 0.092 ms |

The position index and static interval tree replace repeated source-prefix and
symbol-list scans. Cached indexes are immutable: the cache-map mutex only
retrieves a per-source cell, while parsing and breadcrumb lookup run outside it.
The result comparison preserves original tie-breaking, boundary behavior,
duplicate positions, and missing breadcrumbs.

These historical timings used separate raw and enriched captures, before
projection moved to `coverage show`; they are not timings of the current
capture-then-view workflow. A fresh installed-Code capture in `artifacts/vscode-coverage-optimized-20260914`
measured **0.909 s raw** and **16.356 s enriched**, compared with **0.868 s**
and **66.216 s** in the final baseline capture. The new capture mapped the
executed revert function to TypeScript and verified identical raw/enriched
counts and offsets. Its live workload contained 548 generated-fallback lookups;
the old capture contained 646, so these live timings are not an identical-event
comparison. The frozen replay above is the exact before/after comparison.

Index construction, source acquisition, mapping, and other enrichment costs
remain; sub-millisecond lookup passes do not imply sub-millisecond captures.

## Reusing validated source maps

Map acquisition now carries its decoded validation result into source-view
construction through `SourceMapData`. The view consumes this one-shot result
instead of decoding the bytes again. Ordinary/indexed shape and nesting depth
come from the decoded map, without building another full JSON value.

The handoff preserves the existing raw-byte serialization and byte-based
equality of engine inputs. Clones share the handoff rather than cloning token
arrays. Once consumed, captured state retains the encoded bytes but does not
pin the decoded map. Views continue sharing maps through the existing weak
context cache, and dropping the views releases decoded storage. Disk-cache
loading consumes its input buffer before decoding, avoiding an additional
encoded-map buffer during that phase. Invalid cache entries are still checked
for both content integrity and supported map structure.

Fresh Windows debug-build measurements against the same installed VS Code:

| Measurement | Before | Shared decode |
|---|---:|---:|
| Cold enriched capture | 27.708 s | 12.628 s |
| Service peak working set through capture | 836.6 MiB | 834.7 MiB |
| Sampled peak service private memory through capture | 826.8 MiB | 820.6 MiB |
| Live generated-fallback lookups | 687 | 513 |

These runs are in `artifacts/vscode-source-map-before-v2` and
`artifacts/vscode-source-map-after`. The before executables were verified
byte-identical to the earlier `85b4b03` capture. Working-set peaks use the
Windows process high-water mark; private-memory peaks were sampled every
100 ms, only through the enriched capture (not the later reverse-index query).

The earlier before capture took 16.356 s, so cold-run variability is substantial.
Do not interpret the fresh pair as a controlled twofold algorithmic speedup:
background coverage activity and host/network timing differ. The implementation
removes one full decode and the shape-only JSON parse; measured overall peak
memory stayed effectively flat rather than showing a large reduction.

Validation checks decoded-storage identity, shared-map reuse, release while
captured bytes remain alive, serialization compatibility, invalid-map fallback,
and empty/nested indexed maps. The installed-Code capture also checks authored
TypeScript mapping and preserved raw/enriched counts. Replaying the original
646-position workload still matches every frozen result exactly.

## Graph and position-index follow-up

Further redundant work was removed without changing the capture's output:

- Source-map conflict checks use a per-generated-source identity/reference index
  instead of scanning every existing projection for each inserted edge. The
  index is updated on unique edge insertion/removal, including shared
  contributions and release, and retains one entry per mapped generated source.
- Position indexes store only non-ASCII character exceptions, not a boundary
  record for every ASCII character. Strict UTF-8/UTF-16 boundaries, CRLF handling,
  surrogate rounding, and end-of-source clamping remain unchanged.
- Coverage offsets and generated breadcrumbs share the same immutable position
  index. This replaces the separate checkpoint index that rescanned source text
  for each lookup. The legacy byte-offset fallback is now evaluated only when
  UTF-16 projection fails.
- Parser scratch storage is released before constructing the final symbol and
  position indexes.

The unchanged-input component probe measured repeated view construction at
**2.718 s before** and **1.921-2.022 s after**. Cold view construction and symbol
index construction varied materially across runs, so their best samples should
not be presented as guaranteed speedups.

The final installed-Code run in `artifacts/vscode-shared-offset-after` measured
**12.370 s** for cold enriched capture and **826.1 MiB** peak service working set.
The preceding single-decode run measured **12.628 s** and **834.7 MiB**. An
intermediate run measured **13.478 s**: these observations do not establish a
material end-to-end latency improvement below roughly 12 seconds.

All 646 original frozen breadcrumb results still match. Tests exhaustively
check short combinations of ASCII, BMP and non-BMP Unicode, CR and LF at every
byte/UTF-16 boundary, including invalid interior offsets and clamping. A
400,001-character mostly-ASCII line stores just one Unicode exception record. The full
library suite passes with serial test scheduling (343 passed, one ignored);
one unrelated handshake-timeout test failed during an earlier parallel run
and passed both in isolation and serially.

## Sharing digests and parallel map preparation

The disk cache and content store both identify source-map bytes with SHA-256.
Previously, each consumer computed the digest independently. Immutable
`HashedBytes` now shares one lazy `OnceLock<ContentHash>` across clones.
Source-map cache writes and content interning reuse that digest. Cache reads
still verify the actual bytes against the disk checksum before accepting the
entry, and retain the verified digest for interning. The private interning
helper only receives hashes computed from its text or from immutable bytes;
callers cannot supply an unrelated digest through the public interning API.

Independent work now runs in parallel:

- For maps of at least 64 KiB, decoding and hashing use `rayon::join`, sharing
  the same encoded buffer. Small maps avoid the scheduling overhead.
- Authored-source discovery, hashing, and candidate construction use an indexed
  parallel iterator. Diagnostics are collected in original source order before
  graph registration, which remains sequential. This preserves source indexes,
  conflict behavior, content deduplication, and diagnostic ordering.

There is no additional map-sized buffer or unbounded task spawning. The added
persistent metadata is a 32-byte digest plus its synchronization/allocation
overhead. Work uses the existing Rayon pool and also works with a single worker.
It does not introduce concurrent large AST builds.

Two fresh debug-build captures before editing, followed by two captures after
building, used the same installed VS Code commit and bundle hash:

| Measurement | Before 1 | Before 2 | After 1 | After 2 |
|---|---:|---:|---:|---:|
| Enriched capture | 12.461 s | 14.776 s | 11.259 s | 12.248 s |
| Peak service working set through capture | 817.5 MiB | 820.3 MiB | 828.6 MiB | 813.2 MiB |
| Live generated-fallback lookups | 610 | 566 | 566 | 610 |

Mean capture time was **13.619 s before / 11.753 s after**, about 13.7% lower
in these four runs. The earlier 12.370-second result remains part of the
history; it is not replaced by the slower fresh before run. These are live
captures, not an identical-event or statistically established speedup.
Raw capture also varied (0.765-0.813 s before, 1.482-1.789 s after). Peak memory
remained in approximately the same range rather than increasing by a map-sized
allocation.

Evidence is in `artifacts/vscode-digest-before-{1,2}` and
`artifacts/vscode-digest-after-{1,2}`. The latter's `comparison.json` in run 2
records all measurements and binary hashes. The unchanged original
646-position workload still matches every result, and both installed-Code
captures verify authored TypeScript, reverse mapping, and unchanged runtime
counts/offsets without raw-CDP fallback.

Validation: 349 library tests passed, one ignored; all workspace targets checked.
New tests cover shared digest/buffer identity, unchanged serialization and
disk-cache format, corrupt cache rejection, invalid UTF-8, concurrent content
deduplication, and deterministic preparation with one and four Rayon workers.

## Optimized release measurements

The release CLI, service, and replay were built with:

```powershell
cargo build --release -j 1 --bin dbgjs --bin dbgjs-service --example vscode_breadcrumbs
```

The following historical run also used separate raw and enriched captures,
not the current capture-then-view workflow. Cargo confirmed `release` / `optimized`, and the replay confirmed
`debugAssertions: false`. Three successful fresh-cache captures used identical
release executable hashes and the same VS Code commit/bundle as above:

| Capture directory suffix | Raw capture | Enriched capture | Peak service working set |
|---|---:|---:|---:|
| `release-1` | 0.498 s | 6.363 s | 803.3 MiB |
| `release-3` | 4.365 s | 8.457 s | 805.4 MiB |
| `release-4` | 0.488 s | 5.260 s | 809.6 MiB |

The median was **0.498 s raw / 6.363 s enriched**. All successful runs are
included, including the raw outlier; this is not a low-noise benchmark.
Collection alone completed in roughly half a second twice. That is a useful
reference for the remaining cost, not proof that cold enriched coverage can
reach the same latency: it still needs source acquisition, map decoding,
projection, and symbol enrichment. These wall times also include CLI startup,
IPC, CDP collection, and output.

The original frozen replay still matched all **646 results**, with index
construction at **0.368 s** and complete lookup passes at **0.209/0.060 ms**.
All three live captures verified mapped TypeScript, reverse position mapping,
and preserved raw/enriched runtime counts and offsets through public commands.
The earlier debug captures came from an earlier dirty-checkout state; they are
context, not an isolated compiler-profile A/B comparison.

`artifacts/vscode-coverage-release-2` failed during UI setup, before coverage
started: a late welcome dialog blocked the multi-diff interaction until the
30-second program limit. Its diagnostics were preserved. Moving the existing
welcome handler into the shared per-program wrapper allowed both subsequent
captures to complete. The six benchmark unit tests also passed.

The complete successful runs are under
`artifacts/vscode-coverage-release-{1,3,4}`. The last directory contains
`release-summary.json` with executable hashes, every sample, failed-setup
details, and `frozen-original-replay.json` with the exact original results.
The session-local memory sampler was extended with a `debug|release` parameter;
its default remains `debug`.
