//! A `Calculator` server that speaks hubrpc over **stdio** (stdin/stdout), one JSON-RPC message
//! per line. A consumer spawns this example as a child process and drives it with the generated
//! `CalculatorClient` — see `examples/calc_client.rs` and `tests/stdio_process.rs`. Run it
//! directly with `cargo run -p hubrpc-examples --example calc_server`.
//!
//! stdout is the RPC channel, so the server must never print anything else there; diagnostics go
//! to stderr.

use std::sync::Arc;

use async_trait::async_trait;
use hubrpc::prelude::*;
use hubrpc_examples::calc::{Calculator, CalculatorServer, DivResult};
use hubrpc_tokio::NdjsonTransport;

struct CalcService;

#[async_trait]
impl Calculator for CalcService {
    async fn add(&self, _ctx: &CallCtx, a: i64, b: i64) -> Result<i64, JsonRpcError> {
        Ok(a + b)
    }

    async fn divide(&self, _ctx: &CallCtx, a: i64, b: i64) -> Result<DivResult, JsonRpcError> {
        if b == 0 {
            return Err(JsonRpcError::new(
                error_codes::INVALID_PARAMS,
                "division by zero",
            ));
        }
        Ok(DivResult {
            quotient: a / b,
            remainder: a % b,
        })
    }
}

#[tokio::main(flavor = "current_thread")]
async fn main() {
    // stdin carries inbound requests, stdout carries outbound responses.
    let transport = NdjsonTransport::new(tokio::io::stdin(), tokio::io::stdout());
    let conn = HubRpcConnection::new(Box::new(transport));
    conn.register_service(
        Arc::new(CalculatorServer::new(Arc::new(CalcService))),
        RegisterOptions::default(),
    )
    .expect("register calculator");
    conn.enable_reflection();
    conn.run().await;
}
