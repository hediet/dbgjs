use std::env;
use std::fs;
use std::path::{Path, PathBuf};

use dbgjs::protocol_schema::import_typed_cdp_protocol;
use dbgjs::service_api::debugger_service_api;
use linkrpc::prelude::LinkRpcInterfaceSchema;
use serde::Serialize;

const CDP_INTERFACE_ID: &str = "cdp.protocol";
const DAEMON_INTERFACE_ID: &str = "dev.dbgjs.cdp-debugger";

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct InterfaceReference<'a> {
    interface_id: &'a str,
    interface_hash: &'a str,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct Service<'a> {
    service_id: &'a str,
    interfaces: Vec<InterfaceReference<'a>>,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct ContractBundle<'a> {
    services: Vec<Service<'a>>,
    default_interface: InterfaceReference<'a>,
    interface_schemas: Vec<LinkRpcInterfaceSchema>,
}

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let check = parse_args()?;
    let repository_root = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    let protocol_root = repository_root
        .join("node_modules")
        .join("devtools-protocol")
        .join("json");
    let browser_protocol = fs::read_to_string(protocol_root.join("browser_protocol.json"))?;
    let js_protocol = fs::read_to_string(protocol_root.join("js_protocol.json"))?;
    let cdp = import_typed_cdp_protocol(&browser_protocol, &js_protocol)?;
    assert_eq!(cdp.id, CDP_INTERFACE_ID);

    let daemon = debugger_service_api::interface().to_schema();
    assert_eq!(daemon.id, DAEMON_INTERFACE_ID);
    let daemon_hash = daemon.hash.clone();
    let bundle = ContractBundle {
        services: vec![Service {
            service_id: "",
            interfaces: vec![InterfaceReference {
                interface_id: DAEMON_INTERFACE_ID,
                interface_hash: &daemon_hash,
            }],
        }],
        default_interface: InterfaceReference {
            interface_id: DAEMON_INTERFACE_ID,
            interface_hash: &daemon_hash,
        },
        interface_schemas: vec![cdp, daemon],
    };
    let mut generated = serde_json::to_string_pretty(&bundle)?;
    generated.push('\n');

    let output = repository_root
        .join("schemas")
        .join("dbgjs.interfaces.json");
    if check {
        let checked_in = fs::read_to_string(&output).map_err(|error| {
            format!(
                "{} is missing or unreadable: {error}; run `cargo run --bin export_contracts`",
                output.display()
            )
        })?;
        if checked_in != generated {
            return Err(format!(
                "{} is stale; run `cargo run --bin export_contracts`",
                output.display()
            )
            .into());
        }
        println!("{} is up to date", output.display());
    } else {
        write_if_changed(&output, &generated)?;
        println!("wrote {}", output.display());
    }
    Ok(())
}

fn parse_args() -> Result<bool, String> {
    let mut check = false;
    for argument in env::args().skip(1) {
        match argument.as_str() {
            "--check" if !check => check = true,
            "-h" | "--help" => {
                println!("Usage: export_contracts [--check]");
                std::process::exit(0);
            }
            _ => return Err(format!("unexpected argument: {argument}")),
        }
    }
    Ok(check)
}

fn write_if_changed(path: &Path, contents: &str) -> Result<(), std::io::Error> {
    match fs::read_to_string(path) {
        Ok(existing) if existing == contents => return Ok(()),
        Ok(_) => {}
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(error) => return Err(error),
    }
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    fs::write(path, contents)
}
