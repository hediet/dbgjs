# jsdbg

Native JavaScript debugger CLI and TUI, distributed through npm. Requires Node.js
22 or newer. No Rust compiler or install-time build is needed.

## Install a CI build

Download and extract the `npm-<platform>` artifact from the CI run. It contains
two tarballs: the entry package and its matching native package. Install both
in the same command, for example on Windows x64:

```sh
npm install -g ./hediet-jsdbg-win32-x64-0.1.0.tgz ./hediet-jsdbg-0.1.0.tgz
jsdbg --help
```

For a project-local installation omit `-g` and use `npx --no-install jsdbg`.
These packages are not yet published to an npm registry; installing only the
entry tarball will not supply an unpublished native dependency.

Supported platforms: Windows x64, macOS x64/ARM64, Linux GNU x64/ARM64.
Linux requires glibc 2.35 or newer and OpenSSL 3. Musl/Alpine and Windows ARM64
are not supported. Platform dependencies must not be omitted.

The package supplies Playwright's JavaScript library, but does not install a
browser. Browser debugging requires a separately installed browser. `JSDBG_NODE`
and `JSDBG_PLAYWRIGHT_PACKAGE` can override the runtime and Playwright module.

Each native package keeps `jsdbg`, `jsdbg-service`, and `jsdbg-tui` together.
Use `jsdbg service stop` for the appropriate service instance before replacing
an installed package on Windows; running executables cannot be overwritten.
