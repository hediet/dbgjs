# Handoff — hubrpc Rust port (continue in Codespace)

This file is the working context for continuing the Rust port of `hubrpc` (the
TypeScript RPC framework at
`vscode-team-tools/packages/hubrpc/hubrpc`). It exists so an agent (Copilot CLI)
running **inside the Codespace** can pick up exactly where the Windows host
session left off. Delete it once M6 lands if you like — it is a scratch handoff,
not permanent docs.

> **Resume (2026-06-22):** Codespace `bookish-spork-7rxjp5jv9v3xqwx` on
> `hediet/hubrpc-rust@main` (rustc 1.96, Node 24). Baseline verified green on
> Linux: `cargo fmt --check`, `cargo clippy --workspace --all-targets -D warnings`,
> `cargo test --workspace` → **50 tests**. The TS reference repo is **not** present
> in the Codespace, so the authoritative endpoint logic to port is captured inline
> below (see "NEXT TASK — M6") and in `executionPlan.md` Appendix A. Start the
> Copilot CLI here and continue with **M6 endpoint connector**.

## Environment facts
- Codespace toolchain: **rustc 1.96**, Node 24 (host was pinned to 1.85, which is
  why the `url` crate was previously avoided — on the Codespace `url = "2"` is
  fine).
- `.devcontainer/devcontainer.json` provides Rust + Node LTS + sshd.
- Verify baseline before changing anything:
  ```bash
  cargo fmt --all -- --check
  cargo clippy --workspace --all-targets -- -D warnings
  cargo test --workspace        # 50 tests green as of this handoff
  ```
- Conformance vectors: `cd conformance/generate && node gen.mjs` regenerates
  `conformance/vectors/*.json`; CI fails if they drift. `gen-pizza-ref.mjs`
  imports the real TS dist by absolute file URL.

## Milestone status
| id | milestone | status |
|---|---|---|
| M0 | workspace + interop harness | ✅ done |
| M1 | wire core | ✅ done |
| M2 | schema & hashing (interop canary) | ✅ done |
| M3 | connection + reflection + `#[hub_rpc_interface]` macro | ✅ done |
| M3 | NDJSON socket transport (`hubrpc-tokio`) | ✅ done |
| **M6** | **WebSocket transport + env-var connector (endpoint layer)** | ✅ done |
| M4 | identity core (Ed25519, optional but native) | pending |
| M5 | capability gate + consent (hubAccess) | pending |
| M7 | end-to-end north-star scenario | pending |
| M8 | Rust hub server (optional) | pending |

Streaming (incoming/outgoing/bidirectional) in the macro + runtime is deferred;
`incoming_stream`/`outgoing_stream` attrs currently emit a compile error.

## Current crate layout
- `crates/hubrpc/src/connection/` — `channel.rs`, `dispatch.rs`,
  `hub_connection.rs` (the `HubRpcConnection` runtime), `interface_def.rs`,
  `reflection.rs`, `mod.rs`.
- `crates/hubrpc-tokio/src/` — `ndjson.rs` (`NdjsonTransport` + `Preamble`),
  `unix.rs` (`#[cfg(unix)]` `connect_unix` / `UnixHubListener` — **now compiles
  on Linux**), `lib.rs`.
- `crates/hubrpc-macros/src/lib.rs` — `#[hub_rpc_interface]`.
- `conformance/generate/{gen.mjs,gen-pizza-ref.mjs}` → `conformance/vectors/`.

## NEXT TASK — M6 endpoint connector

Goal: a Rust connector that reads `HUBRPC_ENDPOINT` / `HUBRPC_TOKEN`, parses the
endpoint URI **identically to TS**, opens the right transport (UDS/pipe NDJSON
with preamble, or WebSocket with `Authorization: Bearer`), and wraps it in a
`HubRpcConnection`. Authoritative TS sources:
- `hubrpc/src/connection/endpointUri.ts` — `parseEndpointUri`/`formatEndpointUri`.
- `hubrpc/src/node/hubClient.ts` — `HUBRPC_ENDPOINT_VAR`/`HUBRPC_TOKEN_VAR`,
  `openHubChannel`, transport selection + preamble.
See also `executionPlan.md` Appendix A.

### Subtask 1 — `parse_endpoint_uri` + `ResolvedEndpoint`
New module (suggest `crates/hubrpc/src/connection/endpoint.rs`, re-export from
`connection/mod.rs` + prelude). On the Codespace, `url = "2"` is available and
gives WHATWG parity with Node's `URL`. Add it to `crates/hubrpc/Cargo.toml` and
the workspace `Cargo.toml` `[workspace.dependencies]`.

```rust
pub enum ResolvedEndpoint {
    Socket { path: String, token: Option<String> },
    Ws { url: String, token: Option<String> },
    CmdStdio { command: EndpointCommand, env: Option<BTreeMap<String, String>> },
    CmdEnv { command: EndpointCommand, provision_slot: Option<String>, env: Option<BTreeMap<String, String>> },
}
pub enum EndpointCommand { Command(String), Argv(Vec<String>) }
```

Algorithm (port verbatim — keep error strings byte-identical for conformance):
1. `trim()`; empty → `Err("endpoint URI is empty")`.
2. Try `url::Url::parse`. **If it fails (bare string):** matches
   `^wss?://`i → `Ws { url: trimmed, token: None }`; else
   `Socket { path: trimmed, token: None }`.
3. If it parses, switch on scheme:
   - `ws:`/`wss:` → read `?token=`, **delete it from the URL**, `url` = cleaned
     `url.to_string()`. (Token never travels in the URL.)
   - `unix:` → `path = percent-decode(url.path())`; `?token=`.
   - `npipe:` → `npipe://host/tail` → `\\<host>\<tail>`: host defaults `.`,
     `percent-decode(path)`, replace `/` → `\`. So `npipe://./pipe/foo` →
     `\\.\pipe\foo`. `?token=`.
   - `cmd-stdio:` → `CmdStdio { command: parse_command, env: parse_env }`.
   - `cmd:` → `CmdEnv { command, provision_slot: ?provisionSlot, env }`.
   - else → `Err("unsupported endpoint scheme '<proto>' (expected unix:, npipe:, ws:, wss:, cmd:, or cmd-stdio:)")`.
   - `parse_command`: repeated `?argv=` → `Argv`; else `?command=` → `Command`;
     else `Err("endpoint '<proto>' requires a 'command' or 'argv' query parameter")`.
   - `parse_env`: repeated `?env=KEY=VALUE` (first `=` splits); none → `None`;
     malformed → `Err("endpoint 'env' param must be 'KEY=VALUE', got '<entry>'")`.

**WHATWG/`url` parity gotchas to verify against `gen-endpoint.mjs`:**
- `url::Url::parse` and Node `URL` mostly agree, but **probe edge cases**: bare
  `/tmp/bare.sock` and `\\.\pipe\foo` must FAIL to parse (→ bare-string path);
  `ws://bare-detect` must parse as a URL. The `url` crate may parse some bare
  paths that Node rejects (e.g. it can accept `unix:/x` but check whether it
  treats scheme-less inputs the same). If divergence appears, gate the
  bare-string fallback on the same conditions Node uses.
- `unix:` is a non-special scheme in WHATWG; `url.path()` for `unix:/tmp/x.sock`
  is `/tmp/x.sock`. Confirm `url` crate matches (it should).
- Also implement `format_endpoint_uri` (redacts token as `***` unless reveal) and
  `is_hub_endpoint` (true for Socket/Ws) for round-trip vectors.

### Subtask 2 — conformance vectors
Write `conformance/generate/gen-endpoint.mjs` that **faithfully ports** the TS
`parseEndpointUri`/`formatEndpointUri` using Node's `URL` (the real TS endpoint
fns are tree-shaken out of the dist — do NOT try to import them; reimplement the
~70 lines using `URL`, exactly like `endpointUri.ts`). Emit
`conformance/vectors/endpoint.json` with cases covering: ws token-strip, npipe
backslashing, unix path decode, bare-string ws/socket auto-detect, unsupported
scheme error, cmd/cmd-stdio command+argv+env, and format round-trip. Wire it
into `gen.mjs` (or have `gen.mjs` call it) so CI regenerates it. Add a Rust test
`crates/hubrpc/tests/endpoint_conformance.rs` that loads the vectors and asserts
parse + format + error-message parity.

### Subtask 3 — env-var resolution + token precedence
`HUBRPC_ENDPOINT` required (unset → `Err("HUBRPC_ENDPOINT is not set; cannot
connect to hubrpc hub.")`). `HUBRPC_TOKEN` optional, defaults to `""`.
Token precedence for the env-var connector path:
`--endpoint-token ?? token-from-URI ?? HUBRPC_TOKEN ?? ""`.

### Subtask 4 — connector entrypoint (`hubrpc-tokio`)
`connect_to_hub(options) -> HubClientHandle`-ish in `hubrpc-tokio`:
- `Socket { path, token }` → `connect_unix(path)` (or TCP for tests), write one
  `{"hello":1,"token":"<token>"}\n` preamble line, then `NdjsonTransport`, then
  `HubRpcConnection`. Immediate socket close = auth failure.
- `Ws { url, token }` → WebSocket; token in `Authorization: Bearer <token>`
  header; **no preamble**; one JSON-RPC frame per WS message. (Needs a WS dep —
  `tokio-tungstenite`. This is the M6 "WebSocket transport" half; if scoping
  down, land Socket first and stub Ws.)
- `CmdStdio`/`CmdEnv` are spawn modes — out of scope for v1 "connect to existing
  hub"; return an explicit "not supported in connector v1" error.

### Definition of done for M6
- `parse_endpoint_uri` + `format_endpoint_uri` + `is_hub_endpoint` ported,
  matching `endpoint.json` vectors and TS error strings.
- Env-var + token-precedence resolution.
- `connect_to_hub` over UDS with preamble (WS optional in same PR or follow-up).
- `cargo fmt`/`clippy`/`test` green; `node gen.mjs` clean (no vector drift).
- Do **not** commit unless the user asks — they direct commits. Use plain `git`.

## Conventions / gotchas
- Generated macro code references everything by absolute path (`::hubrpc`,
  `::serde`, `::serde_json`, `::schemars`, `::async_trait`).
- `schemars = "0.8"` (draft-07, `definitions`); `serde_json` has `preserve_order`.
- Registry key = `format!("{}::{}", service_id.unwrap_or(""), interface_id)`.
- `cargo fmt` can reformat and break later `edit` matches — re-read before edits.
- Keep TS↔Rust wire interop as a first-class requirement; every algorithm gets a
  conformance vector.
