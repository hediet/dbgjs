#![cfg_attr(windows, windows_subsystem = "windows")]

use std::env;
use std::fs::OpenOptions;
use std::path::PathBuf;

use dbgjs::api::service_api::service_description;
use dbgjs::connection::transport::local_rpc::{
    default_service_directory, default_state_file, ensure_service, serve_local, serve_stdio,
    write_startup_error,
};
use dbgjs::connection::transport::service_discovery::{check_startup_policy, list_services};
use fs2::FileExt;
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
    let mut state_file = None;
    let mut list = false;
    let mut fingerprint = false;
    let mut refuse_incompatible = false;
    let mut expected_contract = None;
    let mut registry_directory = None;
    while let Some(argument) = arguments.next() {
        if argument == "--stdio" {
            if stdio {
                return Err("--stdio may only be specified once".into());
            }
            stdio = true;
        } else if argument == "--ensure" {
            ensure = true;
        } else if argument == "--list" {
            list = true;
        } else if argument == "--contract-fingerprint" {
            fingerprint = true;
        } else if argument == "--refuse-incompatible" {
            refuse_incompatible = true;
        } else if argument == "--expected-contract" {
            expected_contract = Some(
                arguments
                    .next()
                    .ok_or("--expected-contract requires a fingerprint")?
                    .into_string()
                    .map_err(|_| "contract fingerprint must be UTF-8")?,
            );
        } else if argument == "--registry-directory" {
            registry_directory = Some(PathBuf::from(
                arguments
                    .next()
                    .ok_or("--registry-directory requires a path")?,
            ));
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
    let description = service_description();
    if fingerprint {
        if ensure
            || stdio
            || list
            || state_file.is_some()
            || expected_contract.is_some()
            || refuse_incompatible
            || registry_directory.is_some()
        {
            return Err("--contract-fingerprint cannot be combined with other options".into());
        }
        println!("{}", description.contract_fingerprint);
        return Ok(());
    }
    if stdio {
        if ensure
            || state_file.is_some()
            || list
            || refuse_incompatible
            || expected_contract.is_some()
            || registry_directory.is_some()
        {
            return Err("--stdio runs an isolated service and cannot be combined with --ensure or --state-file".into());
        }
        serve_stdio().await?;
        // Tokio's blocking stdin read cannot be cancelled when shutdown arrives before EOF.
        // State and runtimes have been cleaned up before leaving the child process.
        std::process::exit(0);
    }
    if list && (ensure || refuse_incompatible) {
        return Err("--list cannot be combined with --ensure or --refuse-incompatible".into());
    }
    if refuse_incompatible && !ensure {
        return Err("--refuse-incompatible requires --ensure".into());
    }
    let registry_directory = registry_directory.unwrap_or_else(default_service_directory);
    if list {
        let services = list_services(
            &registry_directory,
            state_file.as_deref(),
            expected_contract
                .as_deref()
                .unwrap_or(&description.contract_fingerprint),
        )
        .await?;
        println!("{}", serde_json::to_string_pretty(&services)?);
        return Ok(());
    }
    if let Some(expected) = expected_contract
        && expected != description.contract_fingerprint
    {
        return Err(format!("dbgjs-service has contract {}, client expects {expected}; rebuild or update the service binary",
            description.contract_fingerprint).into());
    }
    let state_file = state_file.unwrap_or_else(default_state_file);

    if ensure {
        // Serialize discovery and automatic startup across contract namespaces.
        let _registry_lock = if refuse_incompatible {
            std::fs::create_dir_all(&registry_directory)?;
            let lock = OpenOptions::new()
                .create(true)
                .truncate(false)
                .read(true)
                .write(true)
                .open(registry_directory.join("service-startup.lock"))?;
            lock.try_lock_exclusive().map_err(|error| {
                format!("Cannot acquire service discovery startup lock: {error}")
            })?;
            let services = list_services(
                &registry_directory,
                Some(&state_file),
                &description.contract_fingerprint,
            )
            .await?;
            check_startup_policy(&services, &state_file, true)?;
            Some(lock)
        } else {
            None
        };
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
