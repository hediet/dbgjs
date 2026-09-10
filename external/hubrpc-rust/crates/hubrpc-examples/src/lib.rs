//! Runnable hubrpc examples sharing one `#[hub_rpc_interface]` contract per example.
//!
//! Layout (the "shared crate" pattern from `docs/examples.md`):
//! - the **contract** lives here in the library (e.g. [`calc`]),
//! - the **server** and **client** are `examples/` binaries that `use` it,
//! - an **e2e test** in `tests/` spawns the server and drives it with the generated client.

pub mod calc;

use std::process::Command;
use std::time::{SystemTime, UNIX_EPOCH};

/// Build the named example binary in this crate and return a [`Command`] that runs it.
///
/// Cargo only exposes `CARGO_BIN_EXE_*` for `[[bin]]` targets, not examples, so the client
/// examples and the e2e tests use this to locate a freshly built server binary. It shells out to
/// `cargo` (via `escargot`), so only call it from examples/tests, never on a hot path.
pub fn example_server_command(example: &str) -> Command {
    escargot::CargoBuild::new()
        .example(example)
        .package("hubrpc-examples")
        .run()
        .unwrap_or_else(|e| panic!("build `{example}` example: {e}"))
        .command()
}

/// A unique, process-local **endpoint address** for the platform's local IPC primitive: a
/// Unix-domain socket path under the temp dir on Unix, or a `\\.\pipe\…` name on Windows.
///
/// Used by the endpoint example/test so concurrent runs don't collide.
/// [`format_endpoint_uri`](hubrpc::prelude::format_endpoint_uri) turns the result into the matching
/// `unix:` / `npipe://` URI, and [`HubListener::bind`](hubrpc_tokio::HubListener::bind) accepts it
/// as-is — so callers never branch on the platform themselves.
pub fn unique_endpoint_address(label: &str) -> String {
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    let pid = std::process::id();
    #[cfg(windows)]
    {
        format!(r"\\.\pipe\hubrpc-{label}-{pid}-{nanos}")
    }
    #[cfg(not(windows))]
    {
        std::env::temp_dir()
            .join(format!("hubrpc-{label}-{pid}-{nanos}.sock"))
            .to_string_lossy()
            .into_owned()
    }
}

/// A throwaway auth token for the demos. Not cryptographically strong — just unique enough that a
/// stray client with the wrong token is rejected.
pub fn demo_token() -> String {
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    format!("tok-{}-{nanos}", std::process::id())
}
