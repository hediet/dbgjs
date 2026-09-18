//! End-to-end multiplexing over a single base transport.
//!
//! Two logical channels (`alpha`, `beta`) are multiplexed over one base
//! transport pair. A [`Channel`] is layered on each mux channel, giving every
//! channel its **own independent JSON-RPC id space** (both start numbering at
//! 1). This proves the Rust multiplexer matches the canonical TS semantics:
//! per-channel Channels with independent id spaces over one wire.

use std::sync::Arc;

use async_trait::async_trait;
use linkrpc::connection::channel::{Channel, RejectingHandler, RequestHandler};
use linkrpc::protocol::json_value::JsonValue;
use linkrpc::protocol::jsonrpc::{error_codes, JsonRpcError};
use linkrpc::transport::memory::transport_pair_of;
use linkrpc::transport::multiplexed::{MultiplexedTransport, MuxEnvelope};
use serde_json::json;

struct Adder;

#[async_trait]
impl RequestHandler for Adder {
    async fn handle_request(
        &self,
        method: String,
        params: JsonValue,
    ) -> Result<JsonValue, JsonRpcError> {
        if method != "add" {
            return Err(JsonRpcError::new(error_codes::METHOD_NOT_FOUND, method));
        }
        let a = params["a"].as_i64().unwrap_or(0);
        let b = params["b"].as_i64().unwrap_or(0);
        Ok(json!(a + b))
    }
}

struct Echo;

#[async_trait]
impl RequestHandler for Echo {
    async fn handle_request(
        &self,
        _method: String,
        params: JsonValue,
    ) -> Result<JsonValue, JsonRpcError> {
        Ok(params)
    }
}

#[tokio::test]
async fn two_channels_multiplex_over_one_base_with_independent_id_spaces() {
    // One base wire carrying MuxEnvelope; a multiplexer on each end.
    let (base_client, base_server) = transport_pair_of::<MuxEnvelope>();
    let mux_client = Arc::new(MultiplexedTransport::canonical(Arc::new(base_client)));
    let mux_server = Arc::new(MultiplexedTransport::canonical(Arc::new(base_server)));

    // Matching channel ids on both ends.
    let client_alpha = mux_client.add_channel("alpha").unwrap();
    let client_beta = mux_client.add_channel("beta").unwrap();
    let server_alpha = mux_server.add_channel("alpha").unwrap();
    let server_beta = mux_server.add_channel("beta").unwrap();

    // A JSON-RPC Channel per logical channel — each with its own id counter.
    let client_alpha_ch = Channel::new(Box::new(client_alpha), Box::new(RejectingHandler));
    let client_beta_ch = Channel::new(Box::new(client_beta), Box::new(RejectingHandler));
    let server_alpha_ch = Channel::new(Box::new(server_alpha), Box::new(Adder));
    let server_beta_ch = Channel::new(Box::new(server_beta), Box::new(Echo));

    // Drive both multiplexer pumps and all four channel read loops.
    let mc = mux_client.clone();
    let ms = mux_server.clone();
    tokio::spawn(async move { mc.run().await });
    tokio::spawn(async move { ms.run().await });
    for ch in [
        client_alpha_ch.clone(),
        client_beta_ch.clone(),
        server_alpha_ch.clone(),
        server_beta_ch.clone(),
    ] {
        tokio::spawn(async move { ch.run().await });
    }

    // alpha routes to Adder; beta routes to Echo — over the same wire.
    let sum = client_alpha_ch
        .call("add", json!({ "a": 2, "b": 40 }))
        .await
        .unwrap();
    assert_eq!(sum, json!(42));

    let echoed = client_beta_ch
        .call("echo", json!({ "hello": "world" }))
        .await
        .unwrap();
    assert_eq!(echoed, json!({ "hello": "world" }));

    // A second call on alpha reuses id 1 in alpha's own space without colliding
    // with beta's id 1 — routing stays correct.
    let sum2 = client_alpha_ch
        .call("add", json!({ "a": 1, "b": 1 }))
        .await
        .unwrap();
    assert_eq!(sum2, json!(2));

    mux_client.dispose();
    mux_server.dispose();
}

/// Regression: after the base transport disconnects, a pending call on a
/// `Channel` layered over a mux child must **fail rather than hang forever**.
/// `MultiplexedTransport::run` disposes live channels on base close, ending the
/// child read loop and failing outstanding calls.
#[tokio::test]
async fn pending_call_fails_when_base_disconnects_rather_than_hanging() {
    let (base_client, base_server) = transport_pair_of::<MuxEnvelope>();
    let mux_client = Arc::new(MultiplexedTransport::canonical(Arc::new(base_client)));

    let client_ch = mux_client.add_channel("alpha").unwrap();
    // A JSON-RPC Channel over the child. The server side is never wired up, so
    // the call below has no responder — it stays pending until the wire dies.
    let client = Channel::new(Box::new(client_ch), Box::new(RejectingHandler));

    let mc = mux_client.clone();
    tokio::spawn(async move { mc.run().await });
    let cl = client.clone();
    tokio::spawn(async move { cl.run().await });

    // Issue a call that no one will answer, then sever the wire.
    let pending = tokio::spawn(async move { client.call("noresponder", json!({})).await });

    // Give the call a moment to be sent and registered as pending.
    tokio::time::sleep(std::time::Duration::from_millis(20)).await;

    // Disconnect: drop the remote end of the base wire.
    drop(base_server);

    // The pending call must resolve with an error (not hang).
    let result = tokio::time::timeout(std::time::Duration::from_secs(5), pending)
        .await
        .expect("pending call must not hang after disconnect")
        .expect("call task joins");
    let err = result.expect_err("call must fail after the transport disconnects");
    assert_eq!(err.code, error_codes::PEER_DISCONNECTED);
}
