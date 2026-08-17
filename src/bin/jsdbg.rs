use std::env;
use std::io;

use cdp_client::local_rpc::{connect_existing, default_state_file, ensure_service};
use serde::Serialize;

#[tokio::main]
async fn main() {
    if let Err(error) = run().await {
        eprintln!("jsdbg: {error}");
        std::process::exit(1);
    }
}

async fn run() -> Result<(), Box<dyn std::error::Error>> {
    let arguments = env::args().skip(1).collect::<Vec<_>>();
    let state_file = default_state_file();
    match arguments.as_slice() {
        [service, status] if service == "service" && status == "status" => {
            let client = connect_existing(&state_file).await?;
            print_json(&rpc(client.service_info().await)?)?;
        }
        [service, stop] if service == "service" && stop == "stop" => {
            let client = connect_existing(&state_file).await?;
            print_json(&rpc(client.shutdown().await)?)?;
        }
        [context, list] if context == "context" && list == "list" => {
            let client = ensure_service(&state_file).await?;
            print_json(&rpc(client.list_contexts().await)?)?;
        }
        [context, create, context_id] if context == "context" && create == "create" => {
            let client = ensure_service(&state_file).await?;
            print_json(&rpc(client.put_context(context_id.clone(), None).await)?)?;
        }
        [context, create, context_id, display_name]
            if context == "context" && create == "create" =>
        {
            let client = ensure_service(&state_file).await?;
            print_json(&rpc(client
                .put_context(context_id.clone(), Some(display_name.clone()))
                .await)?)?;
        }
        [context, show, context_id] if context == "context" && show == "show" => {
            let client = ensure_service(&state_file).await?;
            print_json(&rpc(client.get_context(context_id.clone()).await)?)?;
        }
        [connection, add, context_id, connection_id, endpoint]
            if connection == "connection" && add == "add" =>
        {
            let client = ensure_service(&state_file).await?;
            print_json(&rpc(client
                .put_connection(context_id.clone(), connection_id.clone(), endpoint.clone())
                .await)?)?;
        }
        [connection, connect, context_id, connection_id]
            if connection == "connection" && connect == "connect" =>
        {
            let client = ensure_service(&state_file).await?;
            print_json(&rpc(client
                .connect_connection(context_id.clone(), connection_id.clone())
                .await)?)?;
        }
        [connection, disconnect, context_id, connection_id]
            if connection == "connection" && disconnect == "disconnect" =>
        {
            let client = ensure_service(&state_file).await?;
            print_json(&rpc(client
                .disconnect_connection(context_id.clone(), connection_id.clone())
                .await)?)?;
        }
        [
            breakpoint,
            set,
            context_id,
            breakpoint_id,
            source_path,
            line,
        ] if breakpoint == "breakpoint" && set == "set" => {
            put_breakpoint(
                context_id,
                breakpoint_id,
                source_path,
                line,
                "1",
                &state_file,
            )
            .await?;
        }
        [
            breakpoint,
            set,
            context_id,
            breakpoint_id,
            source_path,
            line,
            column,
        ] if breakpoint == "breakpoint" && set == "set" => {
            put_breakpoint(
                context_id,
                breakpoint_id,
                source_path,
                line,
                column,
                &state_file,
            )
            .await?;
        }
        _ => {
            return Err(usage().into());
        }
    }
    Ok(())
}

async fn put_breakpoint(
    context_id: &str,
    breakpoint_id: &str,
    source_path: &str,
    line: &str,
    column: &str,
    state_file: &std::path::Path,
) -> Result<(), Box<dyn std::error::Error>> {
    let line = line.parse::<u32>()?;
    let column = column.parse::<u32>()?;
    let client = ensure_service(state_file).await?;
    print_json(&rpc(client
        .put_breakpoint(
            context_id.to_owned(),
            breakpoint_id.to_owned(),
            source_path.to_owned(),
            line,
            column,
        )
        .await)?)?;
    Ok(())
}

fn rpc<T>(result: Result<T, hubrpc::prelude::JsonRpcError>) -> Result<T, io::Error> {
    result.map_err(|error| io::Error::other(format!("{error:?}")))
}

fn print_json(value: &impl Serialize) -> Result<(), serde_json::Error> {
    println!("{}", serde_json::to_string_pretty(value)?);
    Ok(())
}

fn usage() -> &'static str {
    "usage:
  jsdbg service status|stop
  jsdbg context list
  jsdbg context create <context-id> [display-name]
  jsdbg context show <context-id>
  jsdbg connection add <context-id> <connection-id> <ws-endpoint>
  jsdbg connection connect|disconnect <context-id> <connection-id>
  jsdbg breakpoint set <context-id> <breakpoint-id> <source-path> <line> [column]"
}
