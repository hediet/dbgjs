//! A `Calculator` consumer: spawn the `calc_server` example as a child process, talk to it over
//! the child's **stdio**, and drive it through the macro-generated typed `CalculatorClient` — no
//! hand-written wire plumbing. Run it with `cargo run -p hubrpc-examples --example calc_client`.
//!
//! This is the runnable companion to `tests/stdio_process.rs`: same flow, but it prints results
//! for a human instead of asserting.

use std::process::Stdio;

use hubrpc::prelude::*;
use hubrpc_examples::calc::CalculatorClient;
use hubrpc_examples::example_server_command;
use hubrpc_tokio::NdjsonTransport;
use tokio::process::Command;

#[tokio::main]
async fn main() {
    // Build + spawn the server example with its stdin/stdout piped; stderr is inherited.
    let mut child = Command::from(example_server_command("calc_server"))
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::inherit())
        .spawn()
        .expect("spawn calc_server");

    let to_server = child.stdin.take().expect("child stdin");
    let from_server = child.stdout.take().expect("child stdout");

    let transport = NdjsonTransport::new(from_server, to_server);
    let conn = HubRpcConnection::new(Box::new(transport));
    let driver = conn.clone();
    tokio::spawn(async move { driver.run().await });

    let calc = CalculatorClient::new(conn.clone());

    // Typed request/response across the process boundary.
    let sum = calc.add(2, 40).await.expect("add");
    println!("add(2, 40) = {sum}");

    // Struct result — quotient + remainder come back fully typed.
    let div = calc.divide(17, 5).await.expect("divide");
    println!(
        "divide(17, 5) = {} remainder {}",
        div.quotient, div.remainder
    );

    // Typed error path: divide-by-zero surfaces as a JSON-RPC error.
    match calc.divide(1, 0).await {
        Ok(_) => println!("divide(1, 0) unexpectedly succeeded"),
        Err(e) => println!("divide(1, 0) -> error {}: {}", e.code, e.message),
    }

    // Introspect the server through the typed directory reflection client.
    let directory = DirectoryServiceClient::new(conn);
    let listing = directory
        .list(None, None, None, None, None)
        .await
        .expect("directory list");
    println!("services exposed by the server:");
    for item in &listing.items {
        println!("  - {} ({})", item.interface_id, item.interface_hash);
    }

    child.kill().await.ok();
}
