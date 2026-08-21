use std::env;
use std::io;

use cdp_client::local_rpc::{connect_existing, default_state_file, ensure_service};
use cdp_client::service_api::{ConnectionConfiguration, PlaywrightChannel, TargetWaitPredicate};

#[path = "jsdbg/output.rs"]
mod output;

use output::OutputFormat;

#[tokio::main]
async fn main() {
    if let Err(error) = run().await {
        eprintln!("jsdbg: {error}");
        std::process::exit(1);
    }
}

async fn run() -> Result<(), Box<dyn std::error::Error>> {
    let mut arguments = env::args().skip(1).collect::<Vec<_>>();
    let output = OutputFormat::from_arguments(&mut arguments);
    let state_file = default_state_file();
    match arguments.as_slice() {
        [service, status] if service == "service" && status == "status" => {
            let client = connect_existing(&state_file).await?;
            output.print(&rpc(client.service_info().await)?)?;
        }
        [service, stop] if service == "service" && stop == "stop" => {
            let client = connect_existing(&state_file).await?;
            output.print(&rpc(client.shutdown().await)?)?;
        }
        [context, list] if context == "context" && list == "list" => {
            let client = ensure_service(&state_file).await?;
            output.print(&rpc(client.list_contexts().await)?)?;
        }
        [context, create, context_id] if context == "context" && create == "create" => {
            let client = ensure_service(&state_file).await?;
            output.print(&rpc(client.put_context(context_id.clone(), None).await)?)?;
        }
        [context, create, context_id, display_name]
            if context == "context" && create == "create" =>
        {
            let client = ensure_service(&state_file).await?;
            output.print(&rpc(client
                .put_context(context_id.clone(), Some(display_name.clone()))
                .await)?)?;
        }
        [context, show, context_id] if context == "context" && show == "show" => {
            let client = ensure_service(&state_file).await?;
            output.print(&rpc(client.get_context(context_id.clone()).await)?)?;
        }
        [
            connection,
            add,
            context_id,
            connection_id,
            endpoint,
            connect_now,
        ] if connection == "connection" && add == "add" && connect_now == "--connect" => {
            add_connection(
                context_id,
                connection_id,
                ConnectionConfiguration::DirectCdp {
                    endpoint: endpoint.clone(),
                },
                true,
                &state_file,
                output,
            )
            .await?;
        }
        [connection, add, context_id, connection_id, endpoint]
            if connection == "connection" && add == "add" =>
        {
            add_connection(
                context_id,
                connection_id,
                ConnectionConfiguration::DirectCdp {
                    endpoint: endpoint.clone(),
                },
                false,
                &state_file,
                output,
            )
            .await?;
        }
        [
            connection,
            add,
            context_id,
            connection_id,
            playwright,
            url,
            options @ ..,
        ] if connection == "connection" && add == "add" && playwright == "--playwright" => {
            let options = parse_playwright_options(options)?;
            add_connection(
                context_id,
                connection_id,
                ConnectionConfiguration::Playwright {
                    url: url.clone(),
                    channel: options.channel,
                    headless: options.headless,
                },
                options.connect,
                &state_file,
                output,
            )
            .await?;
        }
        [connection, connect, context_id, connection_id]
            if connection == "connection" && connect == "connect" =>
        {
            let client = ensure_service(&state_file).await?;
            output.print(&rpc(client
                .connect_connection(context_id.clone(), connection_id.clone())
                .await)?)?;
        }
        [connection, disconnect, context_id, connection_id]
            if connection == "connection" && disconnect == "disconnect" =>
        {
            let client = ensure_service(&state_file).await?;
            output.print(&rpc(client
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
                output,
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
                output,
            )
            .await?;
        }
        [target, attach, context_id, connection_id, target_id]
            if target == "target" && attach == "attach" =>
        {
            let client = ensure_service(&state_file).await?;
            output.print(&rpc(client
                .attach_target(context_id.clone(), connection_id.clone(), target_id.clone())
                .await)?)?;
        }
        [target, show, context_id, connection_id, target_id]
            if target == "target" && show == "show" =>
        {
            let client = ensure_service(&state_file).await?;
            output.print(&rpc(client
                .get_target(context_id.clone(), connection_id.clone(), target_id.clone())
                .await)?)?;
        }
        [
            target,
            wait,
            context_id,
            connection_id,
            target_id,
            installed,
            breakpoint_id,
        ] if target == "target" && wait == "wait" && installed == "breakpoint-installed" => {
            wait_target(
                context_id,
                connection_id,
                target_id,
                TargetWaitPredicate::BreakpointInstalled {
                    breakpoint_id: breakpoint_id.clone(),
                },
                "30000",
                &state_file,
                output,
            )
            .await?;
        }
        [
            target,
            wait,
            context_id,
            connection_id,
            target_id,
            installed,
            breakpoint_id,
            timeout_ms,
        ] if target == "target" && wait == "wait" && installed == "breakpoint-installed" => {
            wait_target(
                context_id,
                connection_id,
                target_id,
                TargetWaitPredicate::BreakpointInstalled {
                    breakpoint_id: breakpoint_id.clone(),
                },
                timeout_ms,
                &state_file,
                output,
            )
            .await?;
        }
        [
            target,
            wait,
            context_id,
            connection_id,
            target_id,
            paused,
            after_epoch,
        ] if target == "target" && wait == "wait" && paused == "paused" => {
            wait_target(
                context_id,
                connection_id,
                target_id,
                TargetWaitPredicate::Paused {
                    after_epoch: parse_u64("pause epoch", after_epoch)?,
                },
                "30000",
                &state_file,
                output,
            )
            .await?;
        }
        [
            target,
            wait,
            context_id,
            connection_id,
            target_id,
            paused,
            after_epoch,
            timeout_ms,
        ] if target == "target" && wait == "wait" && paused == "paused" => {
            wait_target(
                context_id,
                connection_id,
                target_id,
                TargetWaitPredicate::Paused {
                    after_epoch: parse_u64("pause epoch", after_epoch)?,
                },
                timeout_ms,
                &state_file,
                output,
            )
            .await?;
        }
        [target, wait, context_id, connection_id, target_id, running]
            if target == "target" && wait == "wait" && running == "running" =>
        {
            wait_target(
                context_id,
                connection_id,
                target_id,
                TargetWaitPredicate::Running,
                "30000",
                &state_file,
                output,
            )
            .await?;
        }
        [
            target,
            resume,
            context_id,
            connection_id,
            target_id,
            pause_epoch,
        ] if target == "target" && resume == "resume" => {
            let pause_epoch = parse_u64("pause epoch", pause_epoch)?;
            let client = ensure_service(&state_file).await?;
            output.print(&rpc(client
                .resume_target(
                    context_id.clone(),
                    connection_id.clone(),
                    target_id.clone(),
                    pause_epoch,
                )
                .await)?)?;
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
    output: OutputFormat,
) -> Result<(), Box<dyn std::error::Error>> {
    let line = line.parse::<u32>()?;
    let column = column.parse::<u32>()?;
    let client = ensure_service(state_file).await?;
    output.print(&rpc(client
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

async fn add_connection(
    context_id: &str,
    connection_id: &str,
    configuration: ConnectionConfiguration,
    connect_now: bool,
    state_file: &std::path::Path,
    output: OutputFormat,
) -> Result<(), Box<dyn std::error::Error>> {
    let client = ensure_service(state_file).await?;
    let configured = rpc(client
        .put_connection(
            context_id.to_owned(),
            connection_id.to_owned(),
            configuration,
        )
        .await)?;
    if connect_now {
        output.print(&rpc(client
            .connect_connection(context_id.to_owned(), connection_id.to_owned())
            .await)?)?;
    } else {
        output.print(&configured)?;
    }
    Ok(())
}

struct PlaywrightOptions {
    channel: PlaywrightChannel,
    headless: bool,
    connect: bool,
}

fn parse_playwright_options(options: &[String]) -> Result<PlaywrightOptions, io::Error> {
    let mut parsed = PlaywrightOptions {
        channel: PlaywrightChannel::Bundled,
        headless: true,
        connect: false,
    };
    let mut index = 0;
    while index < options.len() {
        match options[index].as_str() {
            "--connect" => parsed.connect = true,
            "--headed" => parsed.headless = false,
            "--channel" => {
                index += 1;
                let channel = options.get(index).ok_or_else(|| {
                    io::Error::new(io::ErrorKind::InvalidInput, "--channel requires a value")
                })?;
                parsed.channel = parse_playwright_channel(channel)?;
            }
            option => {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidInput,
                    format!("unknown Playwright connection option '{option}'"),
                ));
            }
        }
        index += 1;
    }
    Ok(parsed)
}

fn parse_playwright_channel(value: &str) -> Result<PlaywrightChannel, io::Error> {
    match value {
        "bundled" => Ok(PlaywrightChannel::Bundled),
        "chrome" => Ok(PlaywrightChannel::Chrome),
        "chrome-beta" => Ok(PlaywrightChannel::ChromeBeta),
        "chrome-dev" => Ok(PlaywrightChannel::ChromeDev),
        "chrome-canary" => Ok(PlaywrightChannel::ChromeCanary),
        "msedge" => Ok(PlaywrightChannel::Msedge),
        "msedge-beta" => Ok(PlaywrightChannel::MsedgeBeta),
        "msedge-dev" => Ok(PlaywrightChannel::MsedgeDev),
        "msedge-canary" => Ok(PlaywrightChannel::MsedgeCanary),
        _ => Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            format!("unsupported Playwright channel '{value}'"),
        )),
    }
}

async fn wait_target(
    context_id: &str,
    connection_id: &str,
    target_id: &str,
    predicate: TargetWaitPredicate,
    timeout_ms: &str,
    state_file: &std::path::Path,
    output: OutputFormat,
) -> Result<(), Box<dyn std::error::Error>> {
    let timeout_ms = parse_u64("timeout", timeout_ms)?;
    let client = ensure_service(state_file).await?;
    output.print(&rpc(client
        .wait_target(
            context_id.to_owned(),
            connection_id.to_owned(),
            target_id.to_owned(),
            predicate,
            timeout_ms,
        )
        .await)?)?;
    Ok(())
}

fn parse_u64(name: &str, value: &str) -> Result<u64, io::Error> {
    value.parse().map_err(|error| {
        io::Error::new(
            io::ErrorKind::InvalidInput,
            format!("invalid {name} '{value}': {error}"),
        )
    })
}

fn rpc<T>(result: Result<T, hubrpc::prelude::JsonRpcError>) -> Result<T, io::Error> {
    result.map_err(|error| io::Error::other(format!("{error:?}")))
}

fn usage() -> &'static str {
    "usage: jsdbg [--json] <command>

commands:
  jsdbg service status|stop
  jsdbg context list
  jsdbg context create <context-id> [display-name]
  jsdbg context show <context-id>
  jsdbg connection add <context-id> <connection-id> <ws-endpoint> [--connect]
  jsdbg connection add <context-id> <connection-id> --playwright <url> [--channel <channel>] [--headed] [--connect]
  jsdbg connection connect|disconnect <context-id> <connection-id>
  jsdbg breakpoint set <context-id> <breakpoint-id> <source-url> <line> [column]
  jsdbg target attach|show <context-id> <connection-id> <target-id>
  jsdbg target wait <context-id> <connection-id> <target-id> breakpoint-installed <breakpoint-id> [timeout-ms]
  jsdbg target wait <context-id> <connection-id> <target-id> paused <after-epoch> [timeout-ms]
  jsdbg target wait <context-id> <connection-id> <target-id> running
  jsdbg target resume <context-id> <connection-id> <target-id> <pause-epoch>"
}
