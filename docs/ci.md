# CI and npm distribution

[CI](../.github/workflows/ci.yml) runs on pull requests, pushes to `main`, and
manual dispatch. It does not publish to an npm registry.

Completed work is delivered to `main`; keep the primary checkout on `main` and
preserve unrelated work when moving or committing changes.

## Change detection and caches

The small change-detection job always runs. Changes confined to `docs/`, `todo/`,
root Markdown files, the external-dependency README, or agent skills do not
start Rust or packaging jobs. The VS Code extension is outside this pipeline.
Documentation inside an npm package still triggers packaging. Unknown paths,
Rust sources, embedded JavaScript, test fixtures, vendored sources, dependency
manifests, and workflow changes conservatively trigger both jobs.

Set **CI result** as the required branch-protection check, not individual matrix
jobs. It accepts intentional skips but fails when a required job fails or is
cancelled. Manual dispatch runs the full pipeline.

`Swatinem/rust-cache` caches Cargo downloads and compiled dependencies, keyed by
the Rust environment and the target/profile. It also caches workspace crate
outputs so unchanged `dbgjs` binaries and tests can be reused. Main-branch runs
save caches; pull requests and manually dispatched feature branches are
restore-only, preventing unmerged changes from modifying caches consumed by
`main`. The Rust version is pinned in
[rust-toolchain.toml](../rust-toolchain.toml). `actions/setup-node` caches npm
downloads; `npm ci` still installs the locked build dependencies before Cargo
embeds the protocol schemas.

An npm-only change skips Rust unit tests but still verifies and packs release
binaries, reusing unchanged workspace outputs from the trusted `main` cache.
Ordinary docs-only changes, including a root `README.md` change, schedule no
Rust or packaging jobs, so their total Rust compilation and test time is zero.
Cache misses only affect speed, never correctness.

## Checks and candidate artifacts

Windows x64, Linux x64, and macOS ARM64 run workspace builds, Rust tests, and
offline recording tests. The native package matrix builds Windows x64, Linux
x64/ARM64 (GNU), and macOS x64/ARM64. Each npm package has its own GitHub
artifact: `npm-dbgjs` contains the entry-package candidate, while each
`npm-<platform>` artifact contains only its matching native-package candidate.
Candidates use `.tar.gz` filenames and `private: true` manifests. They can be
installed for testing but cannot be published by npm, and the external `.tgz`
scanner does not mistake untested or PR artifacts for releases.

Separate artifact-consumer jobs download the shared entry artifact and matching
native artifact, install both tarballs, exercise the `dbgjs` and `dbgjs-tui`
launchers and native executables, start `dbgjs-service`, and evaluate an
expression in a Node inspector target. This ensures the uploaded packages,
rather than binaries left in the build tree, are tested.

Linux x64 and ARM64 are built on Ubuntu 22.04 (glibc 2.35). These GNU packages
require compatible glibc and OpenSSL 3
runtime libraries. Their artifact-consumer jobs run in the non-development
`ubuntu:22.04` container after installing only `ca-certificates` and `libssl3`;
they also report each executable's dynamic library dependencies and fail on
unresolved libraries. Windows and macOS packages run in fresh jobs on GitHub's
hosted development images because GitHub does not provide runtime-only hosted
images for those platforms. Alpine/musl and Windows ARM64 are not supported
yet.

Real browser and desktop VS Code scenarios are not part of this initial CI;
the workspace tests and installed-package smoke tests cover local service and
Node debugging without depending on a live external website.

## Nightly and stable packages

[Release packages](../.github/workflows/release.yml) runs after successful CI
on this repository's `main`, including manual CI dispatch on `main`. It rejects
PRs, forks, other workflows, and failed runs. Documentation-only CI with no
package artifacts produces no release and still spends zero time on Rust.
Missing parts of a package set and expired artifacts are errors, not releases.

Every eligible package-producing run creates six nightly packages. When the
version in `npm/dbgjs/package.json` has no corresponding `vX.Y.Z` tag, it also
creates six stable packages. Bump the npm entry manifest, its native optional
dependency versions, and `Cargo.toml` together. A failed bump build does not
claim the version; a subsequent green package-producing main build can release
it. There is no strict commit-order queue.

| Channel | Version | `publishConfig.tag` | Artifact names |
| --- | --- | --- | --- |
| Nightly | `X.Y.Z-nightly.<CI-run-id>` | `nightly` | `npm-nightly-dbgjs`, `npm-nightly-<platform>` |
| Stable | `X.Y.Z` | `latest` | `npm-stable-dbgjs`, `npm-stable-<platform>` |

Each release artifact contains one `.tgz`. Version and publication tag are set
inside every package, and native optional dependencies are rewritten to the
same exact version. The checked-in manifests are not changed by CI. The tested
binaries are repacked without rebuilding or changing their contents.

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
for an already-exposed npm version. Nightly versions use the source CI run ID
(not the retry attempt), so retries retain their version.

A stable tag means **the complete stable artifact set was uploaded**, not that
the external publisher successfully uploaded all packages to npm. Tags are
never force-updated. The GitHub concurrency lock covers this workflow only,
not the independent external uploader.

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
