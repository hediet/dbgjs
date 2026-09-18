//! End-to-end with **inverted roles**: the consumer *hosts* the endpoint and the
//! `calc_endpoint_server` *connects out* to it, over the platform's local IPC primitive — a
//! Unix-domain socket on Unix, a Windows named pipe on Windows. The test binds a [`HubListener`],
//! publishes `LINKRPC_ENDPOINT` (a `unix:` / `npipe://` URI) + `LINKRPC_TOKEN`, spawns the server
//! example (which inherits them and dials back in via
//! [`connect_to_hub`](linkrpc_tokio::connect_to_hub)), accepts the connection, verifies the `hello`
//! preamble token, then drives the remote service through the typed `CalculatorClient` plus the
//! `DirectoryServiceClient` reflection proxy. linkrpc is bidirectional, so the accepting side calls
//! the service the dialing side registered. Nothing here is platform-specific.

use std::process::Stdio;

use linkrpc::prelude::*;
use linkrpc_examples::calc::CalculatorClient;
use linkrpc_examples::{demo_token, example_server_command, unique_endpoint_address};
use linkrpc_tokio::{HubListener, LINKRPC_ENDPOINT_VAR, LINKRPC_TOKEN_VAR};
use tokio::process::Command;

#[tokio::test]
async fn server_dials_into_consumer_hosted_endpoint_via_env() {
    // Bind the endpoint and publish it through the environment. The endpoint is a clean `unix:` /
    // `npipe://` URI; the token rides in LINKRPC_TOKEN. The spawned server reads both and dials in.
    let address = unique_endpoint_address("calc-endpoint");
    let _ = std::fs::remove_file(&address);
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

    // Spawn the server; it inherits LINKRPC_ENDPOINT/LINKRPC_TOKEN and dials back in.
    let mut child = Command::from(example_server_command("calc_endpoint_server"))
        .stdin(Stdio::null())
        .stdout(Stdio::inherit())
        .stderr(Stdio::inherit())
        .spawn()
        .expect("spawn calc_endpoint_server");

    // Accept the server's connection and verify its `hello` preamble token.
    let transport = listener.accept().await.expect("accept server connection");
    let preamble = transport
        .read_preamble()
        .await
        .expect("read preamble")
        .expect("preamble present");
    assert_eq!(preamble.token.as_deref(), Some(token.as_str()));

    let conn = LinkRpcConnection::new(Box::new(transport));
    let driver = conn.clone();
    tokio::spawn(async move { driver.run().await });

    let calc = CalculatorClient::new(conn.clone());

    // Typed request/response across the endpoint.
    assert_eq!(calc.add(2, 40).await.expect("add"), 42);

    let div = calc.divide(17, 5).await.expect("divide");
    assert_eq!((div.quotient, div.remainder), (3, 2));

    let err = calc
        .divide(1, 0)
        .await
        .expect_err("divide by zero must fail");
    assert_eq!(err.code, error_codes::INVALID_PARAMS);

    // Reflection over the same connection, through the typed directory client (no raw JSON).
    let directory = DirectoryServiceClient::new(conn);
    let listing = directory
        .list(None, None, None, None, None)
        .await
        .expect("directory list");
    assert!(listing
        .items
        .iter()
        .any(|i| i.interface_id == "dev.linkrpc.calculator"));

    child.kill().await.ok();
    let _ = std::fs::remove_file(&address);
}
