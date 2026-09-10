//! Shared `Calculator` contract: a tiny arithmetic interface defined once with
//! `#[hub_rpc_interface]`. Both the server (`examples/calc_server.rs`) and the consumer
//! (`examples/calc_client.rs` / `tests/stdio_process.rs`) `use` this module, so the contract — and
//! its `id@hash` — has a single source of truth. The macro derives the typed `CalculatorClient`
//! and `CalculatorServer`, so neither side hand-writes wire plumbing.

use hubrpc::prelude::*;
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

/// Result of an integer division.
#[derive(Serialize, Deserialize, JsonSchema, Debug, Clone, PartialEq, Eq)]
pub struct DivResult {
    /// Whole quotient (`a / b`).
    pub quotient: i64,
    /// Remainder (`a % b`).
    pub remainder: i64,
}

/// A minimal arithmetic service.
#[hub_rpc_interface(id = "dev.hubrpc.calculator")]
pub trait Calculator {
    /// Add two integers.
    async fn add(a: i64, b: i64) -> Result<i64, JsonRpcError>;

    /// Integer-divide `a` by `b`, returning quotient and remainder. Errors when `b` is zero.
    async fn divide(a: i64, b: i64) -> Result<DivResult, JsonRpcError>;
}
