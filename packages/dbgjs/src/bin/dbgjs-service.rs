#![cfg_attr(windows, windows_subsystem = "windows")]

use std::env;
use std::path::PathBuf;

use dbgjs::connection::transport::local_rpc::{
    default_state_file, ensure_service, serve_hub, serve_local, serve_stdio, write_startup_error,
};
use tokio::sync::watch;

#[tokio::main]
async fn main() {
    if let Err(error) = run().await {
        eprintln!("dbgjs-service: {error}");
        std::process::exit(1);
    }
}

async fn run() -> Result<(), Box<dyn std::error::Error>> {
    let mut arguments = env::args_os().skip(1);
    let mut ensure = false;
    let mut stdio = false;
    let mut hub = false;
    let mut state_file = None;
    let mut service_id = None;
    while let Some(argument) = arguments.next() {
        if argument == "--stdio" {
            if stdio {
                return Err("--stdio may only be specified once".into());
            }
            stdio = true;
        } else if argument == "--hub" {
            if hub {
                return Err("--hub may only be specified once".into());
            }
            hub = true;
        } else if argument == "--ensure" {
            ensure = true;
        } else if argument == "--service-id" {
            if service_id.is_some() {
                return Err("--service-id may only be specified once".into());
            }
            service_id = Some(
                arguments
                    .next()
                    .and_then(|value| value.into_string().ok())
                    .filter(|value| !value.is_empty())
                    .ok_or("--service-id requires a service id")?,
            );
        } else if argument == "--state-file" {
            if state_file.is_some() {
                return Err("--state-file may only be specified once".into());
            }
            state_file = Some(
                arguments
                    .next()
                    .map(PathBuf::from)
                    .ok_or("--state-file requires a path")?,
            );
        } else {
            return Err(format!("unknown argument: {}", argument.to_string_lossy()).into());
        }
    }
    if stdio && hub {
        return Err("--stdio and --hub are mutually exclusive".into());
    }
    if service_id.is_some() && !stdio && !hub {
        return Err("--service-id requires --stdio or --hub".into());
    }
    if stdio {
        if ensure || state_file.is_some() {
            return Err("--stdio runs an isolated service and cannot be combined with --ensure or --state-file".into());
        }
        serve_stdio(service_id).await?;
        // Tokio's blocking stdin read cannot be cancelled when shutdown arrives before EOF.
        // State and runtimes have been cleaned up before leaving the child process.
        std::process::exit(0);
    }
    if hub {
        if ensure {
            return Err("--hub cannot be combined with --ensure".into());
        }
        // A separate default keeps hub contexts apart from the local daemon's.
        let state_file =
            state_file.unwrap_or_else(|| default_state_file().with_extension("hub.json"));
        serve_hub(service_id, &state_file).await?;
        return Ok(());
    }
    let state_file = state_file.unwrap_or_else(default_state_file);

    if ensure {
        ensure_service(&state_file).await?;
        return Ok(());
    }

    let (shutdown_sender, shutdown_receiver) = watch::channel(false);
    if let Err(error) = serve_local(&state_file, shutdown_sender, shutdown_receiver).await {
        let _ = write_startup_error(&state_file, &error.to_string());
        return Err(error.into());
    }
    Ok(())
}
