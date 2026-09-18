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
       CDP types, client, provider trait,        extension TypeScript
       server adapter, notification methods      contract and typed client
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

The Rust generator requires LinkRPC 0.1.1 or newer. The published 0.1.0 crate
predates the provider-generation options used by this project.

For opt-in development across both repositories, a developer may use a local
sibling checkout by temporarily overriding the relevant npm and Cargo
dependencies in their working tree. Build any required LinkRPC package outputs
in that checkout before installing this repository. Keep those local manifest
overrides and any resulting lockfile changes uncommitted and out of staging,
then restore the registry manifests and lockfiles before running the ordinary
install and contract drift check. Never commit local `file:`/`path` links or
copy package sources or tarballs into this repository.

## Type-safety boundaries

Known RPC methods, parameters, results, and notifications come from the shared
contracts. Raw CDP proxying, arbitrary evaluated JavaScript values, and
protocol-defined open JSON payloads remain explicit dynamic boundaries. Such
payloads are not evidence that another handwritten wire schema is needed.

Heap snapshot ordering, raw-event forwarding, older protocol compatibility, and
daemon authentication remain runtime behavior and are covered by their tests;
generation must not silently change them.
