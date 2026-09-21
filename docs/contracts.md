# RPC contracts and code generation

## Sources of truth

There are two authored protocol inputs, combined into one checked-in contract
bundle. Do not maintain separate Rust and TypeScript descriptions of a wire
interface.

| Contract | Authored source of truth | Derived consumer input |
| --- | --- | --- |
| Chrome DevTools Protocol | The `devtools-protocol` version pinned in the root npm lockfile, plus explicit compatibility overrides in [`protocol_schema.rs`](../crates/cdp-protocol/src/protocol_schema.rs) | The `cdp.protocol` interface in [`schemas/dbgjs.interfaces.json`](../schemas/dbgjs.interfaces.json) |
| CLI, daemon, and extension RPC | The annotated Rust trait and serializable models in [`service_api.rs`](../src/service_api.rs) | The `dev.dbgjs.cdp-debugger` interface in the same bundle |

The bundle is the canonical input for consumers and generators, not an
independently authored schema. Its interface list includes application-owned
contracts; framework discovery interfaces remain LinkRPC's responsibility.
Advertising the daemon service does not advertise a CDP service on the daemon's
authenticated local transport.

## Generated consumers

```text
pinned devtools JSON + compatibility overrides
  -> CDP interface schema -------------------------+
                                                  |
Rust daemon trait + models -> exported schema -----+
                                                  |
                            schemas/dbgjs.interfaces.json
                                      |
                   +------------------+-------------------+
                   |                                      |
          LinkRPC Rust generator                 LinkRPC CLI codegen
                   |                                      |
       CDP types + macro-annotated trait         extension TypeScript
                   |                            contract and typed client
       LinkRPC trait macro (preserves JSON/hash)
                   |
       client, provider, server adapter,
       notification and streaming methods
```

The Rust daemon client and server are generated directly from the same annotated
Rust trait by LinkRPC's macro. The CLI uses that generated client, rather than
constructing RPC method names or request objects independently.

The CDP crate's build reads the checked-in bundle. It does not re-import npm
protocol JSON during an ordinary build. Generated Rust is placed in Cargo's
`OUT_DIR`; do not edit that output. CDP providers implement supported methods of
the generated trait. Unsupported commands retain an explicit method-not-found
response, rather than fabricated successful results.

The extension's generated module is
[`src/generated/debuggerService.ts`](../vscode-extension/src/generated/debuggerService.ts).
It is produced by the **LinkRPC CLI**, preserving the exported wire schema and
interface identity. The extension uses its generated client and derives any
convenience aliases from generated types. Presentation models and endpoint-file
parsing are separate concerns; they must not become duplicate RPC schemas.

## Regeneration and drift checks

From the repository root:

```sh
npm run generate:contracts
npm run check:contracts
```

Generation exports both source contracts into the bundle, then invokes
`linkrpc codegen` to generate TypeScript. The check command recomputes the
contracts and compares the generated output without overwriting stale files.
Commit the bundle and generated TypeScript together with their source changes.

The TypeScript generation step is the installed CLI, not a second generator
inside dbgjs:

```sh
npx --no-install linkrpc codegen \
  --input schemas/dbgjs.interfaces.json \
  --interface dev.dbgjs.cdp-debugger \
  --name DebuggerService \
  --output vscode-extension/src/generated/debuggerService.ts \
  --preserve-wire-schema
```

Append `--check` to check that module without writing it. Use the root
`check:contracts` command to check both the source bundle and generated module.

When changing daemon RPC, edit the Rust trait/models first. When changing CDP
support, update the pinned upstream protocol or an explicit compatibility
override first. Regenerate, then compile and test the consumers; do not repair a
type error by manually editing generated files or weakening a wire type.

The checked-in bundle bootstraps the Rust exporter itself. This is why it is
versioned even though the authored inputs above remain authoritative.

## Registry dependencies and local LinkRPC development

Ordinary installs and CI resolve LinkRPC from the public npm and Cargo
registries. The checked-in manifests and lockfiles must contain only registry
dependencies; CI does not check out a sibling repository, use relative
`file:`/`path` links, or vendor LinkRPC sources. Required LinkRPC releases must
be published before their versions are selected in this repository.

The Rust dependencies require LinkRPC 0.2.1 or newer for schema-preserving,
macro-backed CDP bindings, typed heap-progress streams, and shared trait schemas.
The Rust generator emits a trait annotated with
`#[link_rpc_interface(schema_json = "...")]`; the same macro used by the daemon
trait generates the CDP client and provider infrastructure. The imported CDP
schema and hash are preserved rather than re-derived from generated Rust types.
The published TypeScript runtime and CLI
dependencies are pinned to 0.0.2.
Rust trait models use the same schemars 0.8 version as LinkRPC's shared-schema
collector; schemars 1 derives implement a different `JsonSchema` trait.

For opt-in development across both repositories, a developer may use a local
sibling checkout by temporarily overriding the relevant npm and Cargo
dependencies in their working tree. Build any required LinkRPC package outputs
in that checkout before installing this repository. Keep those local manifest
overrides and any resulting lockfile changes uncommitted and out of staging,
then restore the registry manifests and lockfiles before running the ordinary
install and contract drift check. Never commit local `file:`/`path` links or
copy package sources or tarballs into this repository.

For the isolated sibling development layout, an **uncommitted**
`.cargo/config.toml` can contain:

```toml
[patch.crates-io]
linkrpc = { path = "../linkrpc/rust/crates/linkrpc" }
linkrpc-macros = { path = "../linkrpc/rust/crates/linkrpc-macros" }
linkrpc-tokio = { path = "../linkrpc/rust/crates/linkrpc-tokio" }
```

Resolve the local patch once without `--locked`, then run the locked build,
generation, and tests. Remove this config and restore the registry lockfile
before committing. Ordinary builds, contract generation, and CI use only the
published registry dependencies.

## Typed heap-progress streams

`capture_heap_snapshot` and `take_heap_snapshot` declare
`#[output_stream(HeapSnapshotProgress)]` on the Rust trait. The annotation
describes the method's output: it exports `serverStream`, generates a
provider-side `StreamSender<HeapSnapshotProgress>`, and gives Rust and TypeScript
clients typed progress on the capture call alongside its final result.
The CLI consumes this stream rather than issuing 100 ms snapshot polls.

Progress comes from the existing CDP capture watch and is scoped to the actual
capture operation. The final progress update precedes the RPC response; the
underlying CDP snapshot chunks still finish writing before success is reported.
`get_heap_snapshot_progress` remains an independent last-known snapshot query,
not the transport for capture progress.

Cancellation is advisory: CDP has no interoperable heap-capture cancellation
command. A cancelled or disconnected LinkRPC caller must not interrupt raw CDP
chunk ingestion or leave a writer or capture reservation behind. The active CDP
operation is drained safely; the cancelled LinkRPC call reports cancellation.
Cancelled named captures are cleaned up before their reservation is released.
Subsequent captures can then run with their own progress. Cancellation is not
transaction rollback: `take_heap_snapshot` can have already atomically replaced
the requested destination file, which is retained after cancellation.

The generated TypeScript integration test starts the real Rust daemon and a
Node inspector, exercises completion/cancellation/disconnect/recovery, and runs
both CLI heap capture and snapshot commands:

```sh
cargo build --locked --bin dbgjs --bin dbgjs-service
npm --prefix vscode-extension ci
npm --prefix vscode-extension run test:heap-streaming
npm run check:contracts
```

## Type-safety boundaries

Known RPC methods, parameters, results, and notifications come from the shared
contracts. Raw CDP proxying, arbitrary evaluated JavaScript values, and
protocol-defined open JSON payloads remain explicit dynamic boundaries. Such
payloads are not evidence that another handwritten wire schema is needed.

Heap snapshot ordering, raw-event forwarding, older protocol compatibility, and
daemon authentication remain runtime behavior and are covered by their tests;
generation must not silently change them.
