# CI and npm distribution

[CI](../.github/workflows/ci.yml) runs on pull requests, pushes to `main`, and
manual dispatch. It does not publish to an npm registry.

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
the Rust environment and the target/profile. Main-branch runs save caches; PRs
restore them. The Rust version is pinned in
[rust-toolchain.toml](../rust-toolchain.toml). `actions/setup-node` caches npm
downloads; `npm ci` still installs the locked build dependencies before Cargo
embeds the protocol schemas.

These are dependency caches, not a cache of completed project binaries.
An npm-only change skips Rust unit tests but still builds release binaries for
packing. Ordinary docs-only changes skip compilation entirely. Cache misses
only affect speed, never correctness.

## Checks and artifacts

Windows x64, Linux x64, and macOS ARM64 run workspace builds, Rust tests, and
offline recording tests. The native package matrix builds Windows x64, Linux
x64/ARM64 (GNU), and macOS x64/ARM64. Each npm package has its own GitHub
artifact: `npm-dbgjs` contains the entry-package tarball, while each
`npm-<platform>` artifact contains only its matching native-package tarball.

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
