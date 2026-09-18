//! A `Calculator` consumer that **hosts the endpoint** the server dials into, over the platform's
//! local IPC primitive — a Unix-domain socket on Unix, a Windows named pipe on Windows. Roles are
//! inverted from a classic client/server: here the consumer *listens* and the server *connects
//! out*. The wiring is env-driven and identical on every platform:
//!
//! 1. bind a [`HubListener`] and publish it as `LINKRPC_ENDPOINT` (a `unix:` / `npipe://` URI) plus
//!    `LINKRPC_TOKEN`;
//! 2. spawn the `calc_endpoint_server` example, which **inherits** those env vars and dials back in
//!    with [`connect_to_hub`](linkrpc_tokio::connect_to_hub), writing the `hello` preamble;
//! 3. accept the connection, verify the preamble token, and wrap it in a `LinkRpcConnection`;
//! 4. drive the (remote) service through the typed `CalculatorClient` — linkrpc is bidirectional, so
//!    the accepting side calls the service the dialing side registered.
//!
//! Run it with `cargo run -p linkrpc-examples --example calc_endpoint_client`. This is the runnable
//! companion to `tests/endpoint.rs`: same flow, but it prints results instead of asserting. Nothing
//! here is platform-specific — `HubListener` and the address helper hide the Unix/Windows split.

use std::process::Stdio;

use linkrpc::prelude::*;
use linkrpc_examples::calc::CalculatorClient;
use linkrpc_examples::{demo_token, example_server_command, unique_endpoint_address};
use linkrpc_tokio::{HubListener, LINKRPC_ENDPOINT_VAR, LINKRPC_TOKEN_VAR};
use tokio::process::Command;

#[tokio::main]
async fn main() {
    // 1. Bind the endpoint and publish where it lives + the token guarding it. The endpoint is a
    //    clean `unix:` / `npipe://` URI; the token rides in LINKRPC_TOKEN.
    let address = unique_endpoint_address("calc-endpoint");
    let _ = std::fs::remove_file(&address); // clear a stale Unix socket; a harmless no-op for pipes
    let token = demo_token();
    let mut listener = HubListener::bind(&address).expect("bind endpoint");
    let endpoint = format_endpoint_uri(
        &ResolvedEndpoint::Socket {
            path: address.clone(),
            token: None,
        },
        FormatEndpointOptions::default(),
    );
    std::env::set_var(LINKRPC_ENDPOINT_VAR, &endpoint);
    std::env::set_var(LINKRPC_TOKEN_VAR, &token);
    println!("{LINKRPC_ENDPOINT_VAR}={endpoint}");
    println!("{LINKRPC_TOKEN_VAR}={token}");

    // 2. Spawn the server; it inherits LINKRPC_ENDPOINT/LINKRPC_TOKEN and dials back in.
    let mut child = Command::from(example_server_command("calc_endpoint_server"))
        .stdin(Stdio::null())
        .stdout(Stdio::inherit())
        .stderr(Stdio::inherit())
        .spawn()
        .expect("spawn calc_endpoint_server");

    // 3. Accept the server's connection and verify its `hello` preamble token.
    let transport = listener.accept().await.expect("accept server connection");
    let preamble = transport
        .read_preamble()
        .await
        .expect("read preamble")
        .expect("preamble present");
    assert_eq!(
        preamble.token.as_deref(),
        Some(token.as_str()),
        "server presented the wrong token"
    );

    let conn = LinkRpcConnection::new(Box::new(transport));
    let driver = conn.clone();
    tokio::spawn(async move { driver.run().await });

    // 4. Typed calls across the endpoint — no wire plumbing by hand.
    let calc = CalculatorClient::new(conn.clone());
    let sum = calc.add(2, 40).await.expect("add");
    println!("add(2, 40) = {sum}");

    let div = calc.divide(17, 5).await.expect("divide");
    println!(
        "divide(17, 5) = {} remainder {}",
        div.quotient, div.remainder
    );

    match calc.divide(1, 0).await {
        Ok(_) => println!("divide(1, 0) unexpectedly succeeded"),
        Err(e) => println!("divide(1, 0) -> error {}: {}", e.code, e.message),
    }

    let directory = DirectoryServiceClient::new(conn);
    let listing = directory
        .list(None, None, None, None, None)
        .await
        .expect("directory list");
    println!("services exposed by the connected server:");
    for item in &listing.items {
        println!("  - {} ({})", item.interface_id, item.interface_hash);
    }

    child.kill().await.ok();
    let _ = std::fs::remove_file(&address);
}
