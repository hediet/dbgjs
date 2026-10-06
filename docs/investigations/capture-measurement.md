# Capture persistence and deferred-view measurement

The former `capture_persistence` benchmark compared two synthetic serde structs
and asserted a guessed tenfold improvement. It did not exercise dbgjs storage.
Its replacement is an ignored, bounded measurement alongside a normal regression:
`service::debugger_service::tests::capture_measurement`.

## Running

From the repository root:

```sh
export CARGO_BUILD_JOBS=2 CARGO_INCREMENTAL=0
export CARGO_TARGET_DIR="$PWD/target/capture-measurement"
export DBGJS_SOURCE_MAP_CACHE="$PWD/target/measurement-map-cache"

cargo test -p dbgjs --lib capture_measurement_roundtrip -- --test-threads=1
cargo test -p dbgjs --lib capture_measurement_baseline -- \
  --ignored --nocapture --test-threads=1
```

For repeated observations, run the **same compiled lib-test executable** in
separate processes (Cargo's `--message-format=json` identifies its `executable`):

```sh
"$TEST_EXECUTABLE" capture_measurement_baseline \
  --ignored --nocapture --test-threads=1
```

No browser, package installation, or optional profiler is needed. The test starts
and exercises a real loopback HTTP fixture server; its served response-body
counts are the source/map request and byte observations. Fixture directories and
their individual map-cache entries are removed on successful completion. The
server task is aborted when its owner is dropped.

## What is measured

Fixtures are prepared **outside** publication timing:

- Coverage: 128 block-covered functions, two ranges each, one script/provenance.
- CPU: 128 call-tree nodes, 16,384 ordered samples and timestamp deltas, one
  script/provenance. Aggregation and mapping occur only when viewed.
- Heap: 1,024 located objects plus a root, 1,024 root-to-object edges, varied
  self sizes, and cheap script provenance without source/map text.
- Generated source: 19,467 UTF-8 bytes; compact external v3 maps with embedded
  authored source. Deliberately small maps keep this an endpoint/storage
  measurement, not a claim about large-map decoding.

Publication calls production `reserve_capture`, `store_capture` /
`store_heap_capture`, atomic payload storage, and catalog persistence. Heap
input is emitted to the production staging/final paths with the existing
storage synchronization methods, simulating the completed raw CDP stream.
It does **not** measure the browser heap-snapshot producer.

After dropping the writer, production `DebuggerService::load` reconstructs the
service from the persisted catalog and validates payloads. Production
`load_capture_payload` measures the raw read/verification/decode; for heap it
verifies bytes and returns the raw path rather than parsing a graph. Stored
coverage, CPU, and heap-class endpoints then perform the first and repeated
projected views. No target debugger is present after reconstruction.

The test asserts zero HTTP requests during publication, reconstruction, and raw
read; no eager source/map text persistence; raw-byte and catalog immutability
after views; successful mapped views; and explicit unavailable projection with
raw measurements preserved for both changed source and HTTP 404. These failure
cases evict only the fixture's verified map first: a historical valid cached
map may legitimately support CPU/heap views without the current source.

Byte totals are **logical application bytes**, observed from actual committed
file lengths at each publication, not estimates from a comparison model.
Named reservations do not persist a catalog; each completed publication writes
one catalog, which is included cumulatively. Coverage/CPU payload bytes are
serialized and written once; heap payload bytes are written but are not
serialized by the production publication path. Atomic renames, filesystem
metadata, journal/block writes, and map-cache writes are not in these totals.
Request bytes count response bodies, not HTTP headers or transport framing.

`elapsed_us` uses `Instant` wall-clock time; there are no performance assertions.
`rss_kib` reads Linux `/proc/self/status` `VmRSS` after each phase, or prints
`None` when unavailable. It is current **whole test-process** RSS, including the
runtime, HTTP server, fixtures, and retained allocator pages—not per-capture
allocation, a delta, or a peak. Assertions/re-serialization used to verify raw
fixtures occur outside the raw-read timer.

## Observed baseline, 2026-10-06

Linux x86-64 host, 12 logical CPUs, 31 GiB RAM, Rust/Cargo 1.90.0, default Cargo
test profile (`debug = 1`, not release), two build jobs, incremental disabled.
Measurement used the owning `oct6-measurement` worktree and
`CARGO_TARGET_DIR=/root/.copilot/session-state/3542cd20-76bf-4298-8bc5-dd12c8f2e227/files/oct6-measurement-target`.
Three fresh test processes ran the same executable; unrelated compilation on
the shared host was active, so ranges describe observed runs, not budgets.

| Phase | Wall time range (µs) | Current process RSS range (KiB) |
| --- | ---: | ---: |
| Coverage publication | 12,333–17,595 | 35,316–35,420 |
| CPU publication | 22,864–32,130 | 36,572–36,676 |
| Heap publication | 7,951–16,050 | 37,116–37,220 |
| Service reconstruction | 7,538–12,474 | 38,140–38,244 |
| Coverage raw read | 2,480–3,066 | 38,140–38,244 |
| Coverage first view | 60,062–73,595 | 48,348–48,464 |
| Coverage repeated view | 53,775–56,190 | 48,536–48,648 |
| CPU raw read | 10,385–10,797 | 49,272–49,388 |
| CPU first view | 120,115–137,124 | 50,720–50,836 |
| CPU repeated view | 109,351–118,048 | 50,720–50,836 |
| Heap raw verification | 948–11,369 | 50,720–50,836 |
| Heap first class view | 63,684–70,438 | 53,144–53,260 |
| Heap repeated class view | 51,569–63,435 | 53,172–53,288 |

| Publication | Payload serialized | Payload written | Catalog serialized/written |
| --- | ---: | ---: | ---: |
| Coverage | 44,303 B | 44,303 B | 1,282 B |
| CPU | 156,073 B | 156,073 B | 2,004 B |
| Heap | 0 B | 34,260 B | 3,427 B |
| Cumulative | **207,089 B including catalogs** | **241,349 B including catalogs** | **6,713 B** |

Catalog and payload lengths include absolute fixture URLs/paths and will change
with checkout path and HTTP port length. No threshold depends on these lengths.

| View | First source requests/body bytes | First map requests/body bytes | Repeated source requests/body bytes | Repeated map requests/body bytes |
| --- | --- | --- | --- | --- |
| Coverage | 1 / 19,467 B | 1 / 130 B | 1 / 19,467 B | 0 / 0 B |
| CPU | 1 / 19,467 B | 1 / 125 B | 0 / 0 B | 0 / 0 B |
| Heap | 1 / 19,467 B | 1 / 126 B | 0 / 0 B | 0 / 0 B |

Coverage needs generated text to translate UTF-16 offsets, so its repeated view
revalidates/refetches source while reusing the verified map. CPU/heap views reuse
the existing map cache without HTTP. Both changed-source and 404 fixtures retain
raw captures, report unavailable projection, and do not request a map.

## Boundaries

“First” means the fixture map cache was cleared; “repeated” means that map is
available. Neither means a cold OS page cache. Reconstruction is a new service
object in the same test process, **not** an OS process restart or daemon-launch
latency. Each baseline observation itself starts a fresh test process.

Browser/CDP acquisition latency and source/map CDP calls, full-sized V8 profiles,
network throughput, OS-cold disk access, peak RSS/allocation attribution,
filesystem physical writes, service-process restart, and platform-specific
behavior are **unmeasured**. This baseline establishes the real durable storage
and deferred projection boundary; it is not an embedded-vs-external speedup
claim or a performance SLA. Broader live-browser measurements should reuse the
existing live CDP tests rather than replacing this pipeline with a fake one.
