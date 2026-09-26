# CI and npm distribution

[CI](../../.github/workflows/ci.yml) runs on pull requests, pushes to `main`, and
manual dispatch. It does not publish to an npm registry.

Completed work is delivered to `main`; keep the primary checkout on `main` and
preserve unrelated work when moving or committing changes.

## Change detection and caches

The small change-detection job always runs. Changes confined to `docs/`,
root Markdown files or agent skills do not
start Rust or packaging jobs. The VS Code extension has a separate build and
unit-test job, including its LinkRPC socket transport tests, on every CI run.
The separate required contract-check job installs the LinkRPC CLI from the npm
registry, checks the generated CDP Rust sources, then builds an isolated stdio
daemon and checks generated TypeScript against its reflected contracts.
CI never checks out a sibling LinkRPC repository.
Extension-only changes still skip the native build and packaging jobs.
Documentation inside an npm package still triggers packaging. Unknown paths,
Rust sources, embedded JavaScript, test fixtures, contract schemas, dependency
manifests, and workflow changes conservatively trigger both jobs.

Set **CI result** as the required branch-protection check, not individual matrix
jobs. It accepts intentional skips but fails when a required job fails or is
cancelled. Manual dispatch forces build inputs to be considered changed,
including Windows and macOS ARM64 tests.

`Swatinem/rust-cache` caches Cargo downloads and compiled dependencies, keyed by
the Rust environment and the target/profile. It also caches workspace crate
outputs so unchanged `dbgjs` binaries and tests can be reused. Main-branch runs
save caches; pull requests and manually dispatched feature branches are
restore-only, preventing unmerged changes from modifying caches consumed by
`main`. The Rust version is pinned in
[rust-toolchain.toml](../../rust-toolchain.toml). `actions/setup-node` caches npm
downloads; `npm ci` installs the locked registry dependencies. Cargo likewise
uses the packages selected by the checked-in manifests and lockfile, including
the immutable public Git revision for the unreleased Rust LinkRPC generator.
It compiles checked-in generated Rust rather than importing npm CDP schemas or
running a generator during ordinary builds. See
[RPC contracts and code generation](../architecture/contracts.md) for source ownership,
regeneration checks, and the opt-in local LinkRPC development workflow.

An npm-only change skips Linux/Windows x64 Rust unit tests but still verifies and
packs release binaries, reusing unchanged workspace outputs from the trusted
`main` cache. Windows and macOS ARM64 run their full suites for npm-only changes.
Ordinary docs-only changes, including a root `README.md` change, schedule no
Rust test matrix or packaging jobs. The dedicated README replay below still
builds its two binaries (using the Rust cache) and runs the real investigation.
Cache misses only affect speed, never correctness.

## Checks and candidate artifacts

Windows x64 and Linux x64 run workspace builds, Rust tests, and offline recording
tests for Rust changes. Windows and macOS ARM64 run workspace builds, Rust tests,
and offline recording tests on every package-producing CI run, including PRs,
nightly builds, stable builds, and feature-branch manual dispatches. Stable-tag
existence no longer gates macOS tests. Installed-package smoke checks remain
enabled on both ARM64 platforms as well. Intel macOS is not supported or built.

The Linux Rust job also runs the generated TypeScript heap-streaming client
against the freshly built daemon and a real Node inspector, including CLI
capture/snapshot progress, cancellation, disconnect, and recovery. It installs
only the registry dependencies from the extension lockfile. The pinned Rust
Git dependencies and local-only development overrides are documented in
[the contract integration notes](../architecture/contracts.md#pinned-dependencies-and-local-linkrpc-development);
CI never substitutes a sibling checkout for the pinned public sources.

The native package matrix builds Windows x64/ARM64, Linux
x64/ARM64 (GNU), and macOS ARM64. Each npm package has its own GitHub
artifact: `npm-dbgjs` contains the entry-package candidate, while each
`npm-<platform>` artifact contains only its matching native-package candidate.
Candidates use `.tar.gz` filenames and `private: true` manifests. They can be
installed for testing but cannot be published by npm, and the external `.tgz`
scanner does not mistake untested or PR artifacts for releases.

Separate artifact-consumer jobs download the shared entry artifact and matching
native artifact, install both tarballs, exercise the `dbgjs` and `dbgjs-tui`
launchers and native executables, start `dbgjs-service`, and evaluate an
expression in a Node inspector target. This ensures the uploaded packages,
rather than binaries left in the build tree, are tested. Windows smoke tests
also verify every executable's PE machine type, preventing an x64 binary
running under emulation from passing as a native ARM64 package.

Linux x64 and ARM64 are built on Ubuntu 22.04 (glibc 2.35). These GNU packages
require compatible glibc and OpenSSL 3
runtime libraries. Their artifact-consumer jobs run in the non-development
`ubuntu:22.04` container after installing only `ca-certificates` and `libssl3`;
they also report each executable's dynamic library dependencies and fail on
unresolved libraries. Windows and macOS packages run in fresh jobs on GitHub's
hosted development images because GitHub does not provide runtime-only hosted
images for those platforms. Windows ARM64 builds, full tests, installed-package
smoke tests, and VS Code E2E run natively on `windows-11-arm`.
Alpine/musl is not supported.

A focused [desktop VS Code E2E](../../tests/vscode-discovery/README.md) runs on all
five native platforms using downloaded candidate binaries and pinned VS Code.
It discovers an isolated extension host, attaches by its discovered PID, and
verifies an authored TypeScript breakpoint, mapped stack location, and local
evaluation against one deterministic golden transcript. It uses a local fixture,
not live vscode.dev or a user's existing editor. These sanity checks run in
addition to the full Windows and macOS ARM64 Rust suites on nightly and stable builds.

The [executable README](./readme-generation.md) also runs on every change in a
dedicated Windows x64 job, including documentation-only changes. It launches
isolated desktop VS Code, Playwright Chromium, installed Chrome, and Node,
replays the commands shown in the README and walkthroughs, compares
stable output, and checks real evidence for variable measurements. The
change-detection job checks the deterministic Markdown render on Linux.
Both checks are required by **CI result**; the README job uploads raw commands,
outputs, its screenshot, and Electron diagnostics even on failure.

## Nightly and stable packages

[Release packages](../../.github/workflows/release.yml) runs after successful CI
on this repository's `main`, including manual CI dispatch on `main`. It rejects
PRs, forks, other workflows, and failed runs. Documentation-only CI with no
package artifacts produces no release; only the README replay builds Rust.
Missing parts of a package set and expired artifacts are errors, not releases.

Every eligible package-producing run creates six nightly packages. When the
version in `npm/dbgjs/package.json` has no corresponding `vX.Y.Z` tag, it also
creates six stable packages. Bump the npm entry manifest, its native optional
dependency versions, and `Cargo.toml` together. A failed bump build does not
claim the version; a subsequent green package-producing main build can release
it. There is no strict commit-order queue.

Stable promotion additionally verifies that the selected source CI run passed
the full macOS ARM64 workspace suite. Historical smoke-only runs cannot silently
become stable releases without the full tests. Interrupted stable candidates
use their original run's tests.

| Channel | Version | `publishConfig.tag` | Artifact names |
| --- | --- | --- | --- |
| Development | `X.Y.Z-next.<YYYYMMDD>.<index>` | `next` | `npm-nightly-dbgjs`, `npm-nightly-<platform>` |
| Stable | `X.Y.Z` | `latest` | `npm-stable-dbgjs`, `npm-stable-<platform>` |

Each release artifact contains one `.tgz`. Version and publication tag are set
inside every package, and native optional dependencies are rewritten to the
same exact version. The checked-in manifests are not changed by CI. The tested
binaries are repacked without rebuilding or changing their contents.

Nightly dates use the source CI run's creation date in UTC, not the release or
retry time. The daily index starts at **1**, shared across base versions:
for example, `0.1.0-next.20260915.2`, then `0.1.0-next.20260915.3`.
These development builds are published under npm's `next` dist-tag; the
version label is also `next`. Artifact names retain `npm-nightly-` for compatibility.

The external publisher should run `npm publish <tarball>`, honoring the embedded
`publishConfig.tag`, rather than overriding the tag or always updating `latest`.
It must retry partial sets and skip already-published package versions. This
workflow only prepares artifacts; it neither needs npm credentials nor confirms
registry publication.

### Serialization and retry safety

The release workflow has a single `npm-release` concurrency group with
`cancel-in-progress: false`. It rechecks stable-tag existence inside that
serialized workflow; the first green build to acquire the lock wins. It is
separate from cancellable CI builds, so a new main push does not cancel an
ongoing release. GitHub may replace a pending run; this is not a FIFO queue.

Before exposing a stable artifact set, the coordinator creates an immutable
annotated `release-candidates/vX.Y.Z` tag recording the chosen commit and source
CI run. After uploading the complete set, it creates the immutable `vX.Y.Z`
tag. The candidate tag is retained as a small durable retry record.

If publication is interrupted, rerun the release workflow. A later green build
with the same base version also resumes the recorded candidate, not its own
binaries, while still producing its own nightly packages. An expired candidate
requires operator intervention; it must not silently select different contents
for an already-exposed npm version.

Before packing nightlies, an immutable annotated
`nightly-builds/YYYYMMDD/<index>` tag reserves the version, source CI run ID,
and commit. Retries find and reuse that run's reservation, even after midnight.
New runs reserve one more than the day's maximum index. Git ref creation is
atomic: if another run wins that index, the loser rereads the reservations and
retries instead of overwriting the tag. Persistent contention and API errors
fail explicitly. Failed or interrupted releases may leave gaps; reserved
indices are never recycled. Keep these tags as durable allocation records.

Existing `-nightly.` reservations retain their original versions on retries.
New runs generate `-next.` versions, continuing the same daily counter and
retaining the `nightly-builds/` reservation namespace.

A stable tag means **the complete stable artifact set was uploaded**, not that
the external publisher successfully uploaded all packages to npm. Tags are
never force-updated. The GitHub concurrency lock covers this workflow only,
not the independent external uploader.

## Development debug information

The workspace's `dev` and `test` profiles use `debug = 1` in
[`Cargo.toml`](../../Cargo.toml) to reduce debug-symbol storage, particularly Windows
PDB files. This retains line information for stack traces and source-level
stepping, but omits full type and variable debug information. Incremental
compilation and release profile settings are unchanged.

For a debugging session that needs full debug information, set
`CARGO_PROFILE_DEV_DEBUG=2` or `CARGO_PROFILE_TEST_DEBUG=2` for the corresponding
Cargo invocation.

## Windows executable locks during development

Windows normally prevents replacing an executable while a process is running
from it. Cargo cannot disable that protection.

Do not run long-lived services directly from the directory Cargo writes to.
After building, copy `dbgjs.exe` and `dbgjs-service.exe` together (and
`dbgjs-tui.exe` if used) into a separate, uniquely named run directory, and
launch that copy. Keep the CLI and service together because service discovery
is relative to the CLI executable. Run a new build from a new run directory
rather than overwriting a running copy.

For independent development instances, set `DBGJS_SERVICE_STATE` to a unique
state-file path and ensure `DBGJS_SERVICE_EXE` does not point back into Cargo's
output directory. An npm-installed copy similarly separates runtime binaries
from local build outputs, although replacing that installed copy while it is
running can still encounter the Windows lock.

Alternatively, build into a separate `CARGO_TARGET_DIR` while a previous build
is running. This avoids the lock but uses another build cache and more disk.
Stop only the service instance you own before removing its run directory.

Service endpoint removal does not guarantee that the daemon process has finished
exiting and released its Windows file handles. The log-capture integration test
retries temporary-directory cleanup for up to five seconds on Windows sharing
or lock violations; other errors and persistent locks still fail the test.
