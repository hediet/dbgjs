//! End-to-end: a consumer spawns the `calc_server` example as a child process, talks to it over
//! the child's **stdio** with the NDJSON transport, and drives it through the macro-generated
//! typed `CalculatorClient` — no hand-written wire plumbing on either side. It also introspects
//! the server through the typed `DirectoryServiceClient` reflection proxy.

use std::process::Stdio;

use hubrpc::prelude::*;
use hubrpc_examples::calc::CalculatorClient;
use hubrpc_examples::example_server_command;
use hubrpc_tokio::NdjsonTransport;
use tokio::process::Command;

#[tokio::test]
async fn consumer_spawns_calc_server_over_stdio() {
    // Build + spawn the server example with its stdin/stdout piped; stderr is inherited.
    let mut child = Command::from(example_server_command("calc_server"))
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::inherit())
        .spawn()
        .expect("spawn calc_server");

    let to_server = child.stdin.take().expect("child stdin");
    let from_server = child.stdout.take().expect("child stdout");

    // Read responses from the child's stdout; write requests to its stdin.
    let transport = NdjsonTransport::new(from_server, to_server);
    let conn = HubRpcConnection::new(Box::new(transport));
    let driver = conn.clone();
    tokio::spawn(async move { driver.run().await });

    let calc = CalculatorClient::new(conn.clone());

    // Typed request/response across the process boundary.
    assert_eq!(calc.add(2, 40).await.expect("add"), 42);

    // Struct result — quotient + remainder come back fully typed.
    let div = calc.divide(17, 5).await.expect("divide");
    assert_eq!((div.quotient, div.remainder), (3, 2));

    // Typed error path: divide-by-zero surfaces as a JSON-RPC error.
    let err = calc
        .divide(1, 0)
        .await
        .expect_err("divide by zero must fail");
    assert_eq!(err.code, error_codes::INVALID_PARAMS);

    // Reflection is served by the same process; introspect it through the typed directory client
    // (no raw JSON) — the calculator shows up in the listing, fully typed.
    let directory = DirectoryServiceClient::new(conn);
    let listing = directory
        .list(None, None, None, None, None)
        .await
        .expect("directory list");
    assert!(listing
        .items
        .iter()
        .any(|i| i.interface_id == "dev.hubrpc.calculator"));

    child.kill().await.ok();
}
