//! A `Calculator` server that **connects out** to a hub over the platform's local IPC endpoint — a
//! Unix-domain socket on Unix, a Windows named pipe on Windows. It is the dialer, not the listener:
//! the endpoint already exists, and this process reaches it entirely through the environment:
//!
//! - `LINKRPC_ENDPOINT` — a `unix:/path` or `npipe://./pipe/name` URI naming the hub to dial, and
//! - `LINKRPC_TOKEN`    — the auth token written in the `hello` preamble on connect.
//!
//! That's the standard `LINKRPC_ENDPOINT` / `LINKRPC_TOKEN` contract a hub uses to hand a child
//! process its connection details.
//! [`connect_to_env_endpoint`](linkrpc_tokio::connect_to_env_endpoint) reads the env, dials (picking
//! the transport from the URI scheme and retrying while the listener comes up), and hands us a ready
//! connection; we register the `Calculator` service + reflection and drive it — see
//! `examples/calc_endpoint_client.rs` and `tests/endpoint.rs`. linkrpc is bidirectional, so the side
//! that *accepts* the endpoint is free to *call* the service this side *registers*. There is nothing
//! platform-specific here: the same source runs on Unix and Windows.
//!
//! The connection is the RPC channel, so diagnostics go to stderr.

use std::sync::Arc;

use async_trait::async_trait;
use linkrpc::prelude::*;
use linkrpc_examples::calc::{Calculator, CalculatorServer, DivResult};
use linkrpc_tokio::connect_to_env_endpoint;

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

#[tokio::main]
async fn main() {
    connect_to_env_endpoint(|conn| async move {
        conn.register_service(
            Arc::new(CalculatorServer::new(Arc::new(CalcService))),
            RegisterOptions::default(),
        )
        .expect("register calculator");
        conn.enable_reflection();
        eprintln!("calc_endpoint_server connected to hub; serving calculator");
        conn.run().await;
    })
    .await
    .expect("connect to hub endpoint");
}
