# Refactoring: one trait-centered HubRPC codegen path for CDP

Status: desired design direction, not yet implemented. The exact HubRPC
generator API and the representation of server-to-client notifications still
need to be decided.

## Motivation

The current CDP pipeline has two overlapping HubRPC generation models:

1. Normal application services define a Rust trait annotated with
   `#[hub_rpc_interface]`. The macro rewrites that trait and generates the
   provider adapter, typed client, and interface descriptor.
2. CDP JSON is imported into a `HubRpcInterfaceSchema`, then the standalone
   `generate_rust_interface` function directly emits data types and a separate
   generic `CdpClient<C: RpcCall>`.

The second path bypasses the standard trait-centered model. It independently
implements client generation and introduces constructors for interface,
service, root, and arbitrary-prefix addressing even though CDP only uses flat
wire names through `CdpClient::root`.

This creates several problems:

- There is no generated service trait that a CDP provider, proxy, fake, or test
  implementation can implement.
- HubRPC client behavior is generated in two places.
- CDP-specific addressing concerns leak into generated interface code.
- Server-to-client CDP events are handled as a codegen special case rather than
  as an explicit protocol direction.
- The generated source is hidden under Cargo's `OUT_DIR`.
- Building the protocol crate requires Node-installed protocol JSON and the
  code generator, even when the generated contract has not changed.
- Development tools, product binaries, live tests, and ordinary tests all
  participate in broad `--all-targets` builds.
- Cargo debug symbols, test harnesses, stale hashes, and incremental state can
  grow `target` to many gigabytes.

The intended end state is one shared contract crate whose generated traits and
types can be used from both sides:

```text
CDP protocol JSON
  -> HubRPC interface schema
  -> standard Rust trait/type generator
  -> standard HubRPC trait expansion
       -> provider trait
       -> server adapter
       -> typed client
       -> interface descriptor and hash
```

The generated trait should be the single Rust representation of the contract.
CDP should not have a second handwritten or separately generated client model.

## Capabilities that must remain

- Consume a browser or Node CDP endpoint with typed command parameters and
  results.
- Address CDP commands by their exact flat wire names, such as
  `Debugger.enable`, without adding a HubRPC interface prefix.
- Open independent clients over the root CDP channel and flattened target
  session channels.
- Decode browser-to-client CDP events into generated payload types.
- Implement the CDP command surface in a fake, proxy, recorder, or alternate
  provider by implementing a generated trait.
- Use the same generated contract from a provider and a consumer.
- Preserve all current JSON names, optional fields, recursive types, open
  objects, compatibility overrides, and intentional `serde_json::Value`
  fallbacks.
- Preserve deterministic interface hashing and detect protocol changes that
  introduce new unsupported constructs.
- Build the repository without requiring protocol regeneration.
- Regenerate explicitly and deterministically when the pinned CDP protocol
  changes.
- Keep ordinary unit tests small while retaining explicit integration and live
  test workflows.

## Concepts to keep independent

### Contract schema

The normalized `HubRpcInterfaceSchema` describes methods, payloads,
documentation, annotations, and the interface hash. It must not encode how a
particular connection addresses that contract.

### Generated Rust contract

The generated contract consists of serializable data types and one or more
annotated Rust trait specifications. It is shared by providers and consumers.

The command trait can use the existing `#[params]` support so each CDP method
accepts its generated parameter object directly:

```rust
#[hub_rpc_interface(id = "cdp.protocol")]
pub trait Cdp {
    #[hubrpc(name = "Debugger.enable")]
    async fn debugger_enable(
        #[params] params: DebuggerEnableParams,
    ) -> Result<DebuggerEnableResult, JsonRpcError>;
}
```

The exact wire-name attribute is illustrative. The standard generator and
macro need one supported spelling rather than a CDP-only interpretation.

### Caller

`RpcCall` is the transport-independent ability to issue a JSON-RPC request or
notification. `HubRpcConnection` and a session `Channel` are callers. Generated
clients should depend on this capability rather than hardcoding one concrete
connection type.

### Member addressing

Addressing maps an interface, optional service, and member to a wire method
name. It is independent from serialization and from the caller:

```text
interface-qualified: cdp.protocol::Debugger.enable
service-mounted:      browser::cdp.protocol::Debugger.enable
bare CDP:             Debugger.enable
```

CDP needs bare addressing. Ordinary HubRPC services need interface-qualified or
service-mounted addressing. These should be policies supplied to one client
implementation, not separate generated client implementations.

### Session transport

CDP's root and target sessions select a channel. Once selected, each channel is
an ordinary `RpcCall`. Session selection should remain in the CDP runtime and
must not alter the generated contract.

### Direction

CDP commands travel client to provider. CDP events travel provider to client.
An ignored result-less method is not enough to model this distinction.

The design must decide whether events are:

- a generated companion notification trait, implemented by the consumer;
- a generated event enum plus typed decoder; or
- a standard HubRPC bidirectional/callback-interface feature.

Whichever representation is selected must be part of the general HubRPC model,
not an `x-hubrpc-codegen.kind = "serverNotification"` branch that only the
standalone client generator understands.

### Generation lifecycle

Updating generated source is a repository maintenance operation. Compiling a
consumer is a normal Rust build. These lifecycles should not be coupled.

## Primitive basis

The implementation should converge on these independent primitives:

1. **Import contract**: convert the pinned CDP JSON documents into a normalized
   `HubRpcInterfaceSchema`.
2. **Render Rust contract**: deterministically render types and annotated trait
   specifications from an interface schema.
3. **Expand trait contract**: use the standard HubRPC macro/backend to produce a
   provider trait, server adapter, client proxy, and interface descriptor.
4. **Call member**: serialize params, ask an `RpcCall` to invoke an addressed
   member, and deserialize the result.
5. **Address member**: apply a reusable addressing policy independently from
   the generated method body.
6. **Dispatch member**: decode an addressed request and invoke a provider trait
   implementation.
7. **Decode event**: classify an inbound notification and deserialize its typed
   payload.
8. **Select channel**: choose a root or target-session caller before constructing
   the generated client.

The standard workflows then compose from the basis:

```text
normal HubRPC consumer
  = generated client + HubRpcConnection + interface-qualified addressing

mounted HubRPC consumer
  = generated client + HubRpcConnection + service-mounted addressing

CDP consumer
  = generated client + session Channel + bare addressing

CDP provider or fake
  = generated provider trait implementation + generated server adapter

CDP target session
  = select target channel + construct the same bare-addressed CDP client
```

No workflow requires a separate schema-generated client implementation.

## Required HubRPC work

- [ ] Make the standard generated client operate over a generic `C: RpcCall`,
      or factor its method implementation through a reusable generic typed
      caller.
- [ ] Introduce one explicit member-addressing abstraction.
- [ ] Express root/bare addressing as a normal addressing policy.
- [ ] Keep interface and service addressing as policies over the same client.
- [ ] Add standard support for explicit wire member names that differ from Rust
      identifiers.
- [ ] Ensure `#[params]` methods preserve an existing params object instead of
      synthesizing a second wrapper.
- [ ] Define a general representation for provider-to-consumer notifications or
      callback interfaces.
- [ ] Make the schema-to-Rust generator emit annotated trait specifications
      rather than directly emitting another client proxy.
- [ ] Share one macro/codegen backend for client method serialization, calling,
      result decoding, and provider dispatch.
- [ ] Verify that generated Rust traits reconstruct the same normalized schema
      and interface hash as the imported contract.
- [ ] Add generator tests for bare, interface-qualified, and service-mounted
      addressing without embedding those combinations into generated methods.

The standard HubRPC API should remain conceptually:

```text
trait contract
  -> provider trait + server adapter + typed client + descriptor
```

CDP should extend only the inputs and policies, not add another output model.

## CDP contract generation work

- [ ] Generate a `Cdp` command trait with one method per CDP command.
- [ ] Emit exact wire names as standard method metadata.
- [ ] Reuse generated params/result objects through `#[params]`.
- [ ] Generate the selected event contract or typed event decoder.
- [ ] Preserve the existing seven reviewed fallback types until the upstream
      schemas or standard generator can represent them precisely.
- [ ] Preserve compatibility overrides for older Node and browser protocol
      variants.
- [ ] Keep a golden test for method count, component count, interface hash,
      fallback set, and representative generated signatures.
- [ ] Add a compile test where a fake implements the generated CDP provider
      trait.
- [ ] Add a round-trip test where the generated client calls that fake through
      an in-memory channel.
- [ ] Add event-direction tests proving that events cannot accidentally be sent
      as commands and commands cannot be decoded as events.

## Make generation explicit

Prefer checking deterministic generated Rust into the protocol crate:

```text
crates/cdp-protocol/
  src/
    lib.rs
    generated.rs
  codegen/
    importer and compatibility policy
```

Suggested workflow:

```text
cargo xtask generate-cdp
cargo xtask check-generated
```

- [ ] Move generation out of `build.rs`.
- [ ] Check the generated trait/types into
      `crates/cdp-protocol/src/generated.rs`.
- [ ] Make ordinary builds independent of `node_modules`.
- [ ] Keep `devtools-protocol` pinned as the source of regeneration, not as a
      build-time requirement.
- [ ] Make CI regenerate into a temporary location and fail on a diff.
- [ ] Ensure regeneration is byte-for-byte deterministic.
- [ ] Remove generator-only dependencies from the runtime dependency graph.
- [ ] Remove the root `cdp_codegen` product binary once the explicit generator
      command replaces it.

Checking in a large generated file has a repository-size cost, but it makes the
contract visible, reviewable, searchable, and independent from Cargo build
cache hashes. The generated file must be treated as an artifact: never edited
by hand, always reproducible, and verified in CI.

## Runtime migration

- [ ] Replace `CdpClient::root(channel)` with construction of the standard
      generated client using bare addressing.
- [ ] Keep channel selection in `CdpConnection` and `CdpSessionMux`.
- [ ] Migrate root and target-session call sites without changing wire traffic.
- [ ] Replace event-name helper methods with the standard generated event
      contract.
- [ ] Preserve the public `cdp_client::cdp` re-export during migration.
- [ ] Decide whether `cdp_client::protocol_schema` remains public tooling or
      moves entirely behind the generation command.
- [ ] Remove `client_name: Some("CdpClient")` and the standalone client writer
      after all call sites use the standard generated client.
- [ ] Remove CDP-only `x-hubrpc-codegen` directives that have standard trait
      metadata equivalents.

## Cargo target cleanup

The repository currently exposes four automatic binaries and three automatic
integration-test targets. Broad `cargo test --all-targets` builds more than the
ordinary test workflow needs.

### Binaries

- [ ] Keep `jsdbg` as the user-facing CLI.
- [ ] Keep `jsdbg-service` as the daemon used by the CLI and VS Code extension.
- [ ] Replace `cdp_codegen` with the explicit standard generation workflow.
- [ ] Move `source_memory` to a benchmark, an opt-in tooling crate, or remove it
      after its investigation is complete.
- [ ] Declare product binaries explicitly with `test = false` and
      `bench = false` where appropriate.
- [ ] Feature-gate or isolate development tools so ordinary workspace tests do
      not build them.

### Tests

- [ ] Keep `cli_service` as an explicit black-box integration test because it
      must launch the CLI and service binaries.
- [ ] Move schema and generated-contract tests into `cdp-protocol` where they do
      not require building every application binary.
- [ ] Keep only runtime/client integration behavior in
      `generated_cdp_client`, or merge it with the smallest suitable runtime
      test target.
- [ ] Feature-gate `live_generated_cdp` so ignored live tests are not compiled
      during ordinary tests.
- [ ] Provide a dedicated command for live CDP tests that enables the feature
      and passes `--ignored`.
- [ ] Replace `cargo test --workspace --all-targets` with explicit ordinary,
      integration, and live test commands.

One possible command split is:

```text
test:rust
  cargo test --workspace --lib
  cargo test -p cdp_client --test generated_cdp_client
  cargo test -p cdp_client --test cli_service

test:rust:live
  cargo test -p cdp_client --features live-cdp \
    --test live_generated_cdp -- --ignored
```

The exact scripts may change, but ordinary tests must not implicitly compile
live environments or prototype tools.

### Build-cache policy

- [ ] Measure clean and incremental build time, artifact size, and test-target
      size after the target cleanup.
- [ ] Consider `debug = "line-tables-only"` for development and test profiles.
- [ ] Consider disabling incremental compilation if its disk cost remains
      disproportionate.
- [ ] Document a deliberate cache cleanup command for local development.
- [ ] Avoid running broad target matrices merely to validate a localized
      change.

Profile changes should be based on measurements after redundant targets are
removed. They should not hide an unnecessarily broad build graph.

## Migration sequence

### Phase 1: pin behavior

- [ ] Record representative generated types, command signatures, event
      payloads, method names, and the interface hash.
- [ ] Add a fake-provider test that describes the desired trait-based API.
- [ ] Record clean-build and target-size baselines.

### Phase 2: generalize HubRPC

- [ ] Introduce generic calling and member-addressing primitives.
- [ ] Add exact wire-name and params-object support to standard trait codegen.
- [ ] Add the general event/callback direction model.
- [ ] Prove ordinary HubRPC services retain current behavior.

### Phase 3: generate the CDP trait contract

- [ ] Render CDP types and traits with the standard generator.
- [ ] Expand them through the standard HubRPC path.
- [ ] Verify schema/hash parity and compile provider/client examples.

### Phase 4: migrate the runtime

- [ ] Switch root and target clients to the standard generated client.
- [ ] Switch event handling to the standard event contract.
- [ ] Preserve all existing runtime and live behavior.

### Phase 5: remove duplicate machinery

- [ ] Delete the standalone schema-generated client path.
- [ ] Delete obsolete CDP-specific codegen directives.
- [ ] Move generation out of Cargo build scripts.
- [ ] Remove redundant binaries and default test targets.
- [ ] Re-measure clean build size and local incremental growth.

## Laws and acceptance criteria

- The generated trait contract is the sole Rust contract source.
- Provider and consumer artifacts come from the same generated trait.
- Addressing changes wire names but never changes the interface schema or hash.
- Session selection changes the caller but never changes the generated
  contract.
- A CDP command serializes to the same JSON object before and after migration.
- A CDP result or event deserializes to the same Rust representation before and
  after migration.
- Commands and provider-to-consumer events remain directionally distinct.
- Regeneration with unchanged inputs produces no diff.
- Ordinary builds do not execute CDP generation and do not require
  `node_modules`.
- Ordinary tests do not compile live CDP tests or prototype tools.
- A clean ordinary test build produces only the contract, library, required
  product binaries, and explicitly selected test harnesses.
- Existing public call sites either remain source-compatible or have one
  mechanical migration with a documented compatibility period.

## Deferred choices

- The exact Rust attribute spelling for an explicit wire member name.
- Whether provider-to-consumer events use a companion trait, callback
  interface, or typed event enum.
- Whether the generated client exposes the addressing policy as a type
  parameter, constructor argument, or erased runtime value.
- Whether generated Rust is one file or several domain-based modules.
- Whether the generation command lives in an `xtask` crate or a standard
  HubRPC codegen CLI.
- Whether `cdp-protocol` exposes the imported schema at runtime or only the
  generated descriptor.
- Development/test debug-info and incremental profile settings after target
  cleanup measurements.

These choices should not reintroduce separate client models, hidden addressing
rules, or build-time regeneration.
