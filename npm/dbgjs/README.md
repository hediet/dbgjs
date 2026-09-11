# dbgjs

Native JavaScript debugger CLI and TUI, distributed through npm. Requires Node.js
22 or newer. No Rust compiler or install-time build is needed.

Previously named `@hediet/jsdbg`. Commands now use `dbgjs`, environment variables
use `DBGJS_*`, and the daemon uses a fresh `dbgjs` state location. Existing
prototype state is left untouched. On Windows the new endpoint is
`%LOCALAPPDATA%\hediet\dbgjs\service.json`; `DBGJS_SERVICE_STATE` overrides it.

## Install a CI build

Download and extract the `npm-<platform>` artifact from the CI run. It contains
two tarballs: the entry package and its matching native package. Install both
in the same command, for example on Windows x64:

```sh
npm install -g ./hediet-dbgjs-win32-x64-0.1.0.tgz ./hediet-dbgjs-0.1.0.tgz
dbgjs --help
```

For a project-local installation omit `-g` and use `npx --no-install dbgjs`.
These packages are not yet published to an npm registry; installing only the
entry tarball will not supply an unpublished native dependency.

Supported platforms: Windows x64, macOS x64/ARM64, Linux GNU x64/ARM64.
Linux requires glibc 2.35 or newer and OpenSSL 3. Musl/Alpine and Windows ARM64
are not supported. Platform dependencies must not be omitted.

The package supplies Playwright's JavaScript library, but does not install a
browser. Browser debugging requires a separately installed browser. `DBGJS_NODE`
and `DBGJS_PLAYWRIGHT_PACKAGE` can override the runtime and Playwright module.

Each native package keeps `dbgjs`, `dbgjs-service`, and `dbgjs-tui` together.
Use `dbgjs service stop` for the appropriate service instance before replacing
an installed package on Windows; running executables cannot be overwritten.
