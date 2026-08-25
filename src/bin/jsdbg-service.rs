#![cfg_attr(windows, windows_subsystem = "windows")]

use std::env;
use std::path::PathBuf;

use cdp_client::local_rpc::{default_state_file, ensure_service, serve_local, write_startup_error};
use tokio::sync::watch;

#[tokio::main]
async fn main() {
    if let Err(error) = run().await {
        eprintln!("jsdbg-service: {error}");
        std::process::exit(1);
    }
}

async fn run() -> Result<(), Box<dyn std::error::Error>> {
    let mut arguments = env::args_os().skip(1);
    let mut ensure = false;
    let mut state_file = None;
    while let Some(argument) = arguments.next() {
        if argument == "--ensure" {
            ensure = true;
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
