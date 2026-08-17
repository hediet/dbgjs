use std::env;
use std::path::PathBuf;

use cdp_client::local_rpc::{default_state_file, serve_local, write_startup_error};
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
    let state_file = match arguments.next() {
        Some(flag) if flag == "--state-file" => arguments
            .next()
            .map(PathBuf::from)
            .ok_or("--state-file requires a path")?,
        Some(argument) => {
            return Err(format!("unknown argument: {}", argument.to_string_lossy()).into());
        }
        None => default_state_file(),
    };
    if arguments.next().is_some() {
        return Err("unexpected extra arguments".into());
    }

    let (shutdown_sender, shutdown_receiver) = watch::channel(false);
    if let Err(error) = serve_local(&state_file, shutdown_sender, shutdown_receiver).await {
        let _ = write_startup_error(&state_file, &error.to_string());
        return Err(error.into());
    }
    Ok(())
}
