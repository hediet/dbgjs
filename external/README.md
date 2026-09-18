# External dependencies

## LinkRPC

- Upstream: https://github.com/hediet/linkrpc
- Commit: `ec5ba98edce9a6a119db03844f53b8bdefc2dda9`
- Rust source: [linkrpc/rust](./linkrpc/rust/)
- License: [linkrpc/LICENSE](./linkrpc/LICENSE)
- npm package: [linkrpc/npm/hediet-linkrpc.tgz](./linkrpc/npm/hediet-linkrpc.tgz)
- npm package SHA-256: `e8cc0f9c314f527ffee27eaa038e56404097eb0d2e0a2850974b064177822f6c`

The Rust directory and license are unmodified tracked files exported from the
commit above with `git archive`. The npm tarball is the public
`@hediet/linkrpc` package built from the same commit, using Node `24.19.0` and
pnpm `11.25.0`. Its manifest version is `0.0.1`; the commit and digest above
identify this exact snapshot, not a registry release.

Treat `linkrpc` as **read-only vendored source and artifacts**. Make changes
upstream, then refresh both Rust and npm inputs from one validated commit and
update this record. Do not patch the vendor tree locally.

The vendored repository retains its own Cargo workspace and is excluded from
the parent workspace. The project's path dependencies point into this snapshot,
and the VS Code extension installs the checked-in npm tarball. Normal builds
do not require a sibling LinkRPC checkout, a registry release of LinkRPC, or
downloads of CI artifacts.

To refresh, use a clean checkout of the desired LinkRPC commit. Export its
`rust` directory and `LICENSE` with `git archive` into `external/linkrpc`, then
run these commands in the upstream `typescript` workspace:

```sh
pnpm install --frozen-lockfile
pnpm --filter @hediet/linkrpc build
pnpm --filter @hediet/linkrpc pack --pack-destination /path/to/dbgjs/external/linkrpc/npm
```

Replace the previous snapshot rather than overlaying it. Rename the resulting
`hediet-linkrpc-<version>.tgz` to `hediet-linkrpc.tgz`, record its SHA-256, and
refresh the root Cargo lockfile and extension npm lockfile. Run the Rust and
extension tests before committing the snapshot.

LinkRPC's canonical reflection wire identifiers still include `hubrpc.directory`.
These upstream protocol identifiers are preserved; they are not dependencies on
the retired HubRPC libraries.
