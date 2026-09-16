# dbgjs

Native JavaScript debugger CLI and TUI, distributed through npm. Requires Node.js
22 or newer. No Rust compiler or install-time build is needed.

Commands use `dbgjs`, environment variables use `DBGJS_*`, and the daemon uses
a fresh `dbgjs` state location. Existing prototype state is left untouched.
On Windows the new endpoint is
`%LOCALAPPDATA%\dbgjs\service.json`; `DBGJS_SERVICE_STATE` overrides it.

## Install a release build

Install the current development build from npm:

```sh
npm install -g @hediet/dbgjs@next
dbgjs --help
```

npm selects the native package for the Node.js process's OS and architecture.
Windows ARM64 Node.js installs `@hediet/dbgjs-win32-arm64`; it does not need
`--force` or x64 emulation.

Download and extract `npm-stable-dbgjs` and `npm-stable-<platform>` from the
Release packages workflow. Each artifact contains one tarball. Install the
entry package and its matching native package in the same command, for example
on Windows x64:

```sh
npm install -g ./hediet-dbgjs-win32-x64-0.1.0.tgz ./hediet-dbgjs-0.1.0.tgz
dbgjs --help
```

For a project-local installation omit `-g` and use `npx --no-install dbgjs`.
Installing only the entry tarball will not supply an unpublished native
dependency.

Nightly artifacts are named `npm-nightly-dbgjs` and `npm-nightly-<platform>`.
Their versions are `X.Y.Z-next.<YYYYMMDD>.<index>`, for example
`0.1.0-next.20260915.2`. The date is the source CI run's UTC creation date;
the daily index starts at 1 and retries reuse the same version. Install both
matching nightly tarballs together. Release packages carry `publishConfig.tag`:
`next` for nightlies and `latest` for stable packages, for the external npm
publisher.

CI itself uploads private `.tar.gz` candidates for smoke testing, not publishable
`.tgz` releases. Only successful main builds are promoted to release artifacts.

Supported platforms: Windows x64/ARM64, macOS ARM64, Linux GNU x64/ARM64.
Linux requires glibc 2.35 or newer and OpenSSL 3. Intel macOS and Musl/Alpine
are not supported. Platform dependencies must not be omitted.

The package supplies Playwright's JavaScript library, but does not install a
browser. Browser debugging requires a separately installed browser. `DBGJS_NODE`
and `DBGJS_PLAYWRIGHT_PACKAGE` can override the runtime and Playwright module.

Each native package keeps `dbgjs`, `dbgjs-service`, and `dbgjs-tui` together.
Use `dbgjs service stop` for the appropriate service instance before replacing
an installed package on Windows; running executables cannot be overwritten.
