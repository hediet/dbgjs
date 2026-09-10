//! Endpoint conformance: load `conformance/vectors/endpoint.json` (generated from the verbatim TS
//! `endpointUri.ts` in `gen-endpoint.mjs`) and assert the Rust port matches parse results, error
//! strings, format output, and `is_hub_endpoint`.
//!
//! A red test here means the Rust endpoint logic has diverged from TypeScript.

use std::collections::BTreeMap;
use std::path::PathBuf;

use hubrpc::prelude::*;
use serde_json::{json, Value};

fn vectors_dir() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("..")
        .join("..")
        .join("conformance")
        .join("vectors")
}

fn load() -> Value {
    let path = vectors_dir().join("endpoint.json");
    let text =
        std::fs::read_to_string(&path).unwrap_or_else(|e| panic!("read {}: {e}", path.display()));
    serde_json::from_str(&text).expect("endpoint.json parses")
}

fn command_to_value(command: &EndpointCommand) -> Value {
    match command {
        EndpointCommand::Command(c) => json!({ "command": c }),
        EndpointCommand::Argv(argv) => json!({ "argv": argv }),
    }
}

fn env_to_value(env: &Option<BTreeMap<String, String>>) -> Option<Value> {
    env.as_ref().map(|m| {
        let mut obj = serde_json::Map::new();
        for (k, v) in m {
            obj.insert(k.clone(), Value::String(v.clone()));
        }
        Value::Object(obj)
    })
}

/// Convert a [`ResolvedEndpoint`] into the TS JSON shape (omitting absent optionals).
fn spec_to_value(spec: &ResolvedEndpoint) -> Value {
    match spec {
        ResolvedEndpoint::Socket { path, token } => {
            let mut obj = json!({ "kind": "socket", "path": path });
            if let Some(token) = token {
                obj["token"] = json!(token);
            }
            obj
        }
        ResolvedEndpoint::Ws { url, token } => {
            let mut obj = json!({ "kind": "ws", "url": url });
            if let Some(token) = token {
                obj["token"] = json!(token);
            }
            obj
        }
        ResolvedEndpoint::CmdStdio { command, env } => {
            let mut obj = json!({ "kind": "cmd-stdio", "command": command_to_value(command) });
            if let Some(env) = env_to_value(env) {
                obj["env"] = env;
            }
            obj
        }
        ResolvedEndpoint::CmdEnv {
            command,
            provision_slot,
            env,
        } => {
            let mut obj = json!({ "kind": "cmd-env", "command": command_to_value(command) });
            if let Some(slot) = provision_slot {
                obj["provisionSlot"] = json!(slot);
            }
            if let Some(env) = env_to_value(env) {
                obj["env"] = env;
            }
            obj
        }
    }
}

fn command_from_value(v: &Value) -> EndpointCommand {
    if let Some(argv) = v.get("argv") {
        EndpointCommand::Argv(
            argv.as_array()
                .unwrap()
                .iter()
                .map(|a| a.as_str().unwrap().to_string())
                .collect(),
        )
    } else {
        EndpointCommand::Command(v["command"].as_str().unwrap().to_string())
    }
}

fn env_from_value(v: &Value) -> Option<BTreeMap<String, String>> {
    v.get("env").map(|env| {
        env.as_object()
            .unwrap()
            .iter()
            .map(|(k, val)| (k.clone(), val.as_str().unwrap().to_string()))
            .collect()
    })
}

fn opt_string(v: &Value, key: &str) -> Option<String> {
    v.get(key).and_then(|t| t.as_str()).map(|s| s.to_string())
}

/// Build a [`ResolvedEndpoint`] from the TS JSON shape.
fn spec_from_value(v: &Value) -> ResolvedEndpoint {
    match v["kind"].as_str().unwrap() {
        "socket" => ResolvedEndpoint::Socket {
            path: v["path"].as_str().unwrap().to_string(),
            token: opt_string(v, "token"),
        },
        "ws" => ResolvedEndpoint::Ws {
            url: v["url"].as_str().unwrap().to_string(),
            token: opt_string(v, "token"),
        },
        "cmd-stdio" => ResolvedEndpoint::CmdStdio {
            command: command_from_value(&v["command"]),
            env: env_from_value(v),
        },
        "cmd-env" => ResolvedEndpoint::CmdEnv {
            command: command_from_value(&v["command"]),
            provision_slot: opt_string(v, "provisionSlot"),
            env: env_from_value(v),
        },
        other => panic!("unknown kind {other:?}"),
    }
}

#[test]
fn parse_matches_reference() {
    let cases = load();
    for case in cases["parse"].as_array().unwrap() {
        let input = case["input"].as_str().unwrap();
        match parse_endpoint_uri(input) {
            Ok(spec) => {
                let expected = case
                    .get("result")
                    .unwrap_or_else(|| panic!("expected an error for {input:?}, got Ok"));
                assert_eq!(
                    spec_to_value(&spec),
                    *expected,
                    "parse result mismatch for {input:?}"
                );
            }
            Err(e) => {
                let expected = case["error"]
                    .as_str()
                    .unwrap_or_else(|| panic!("expected Ok for {input:?}, got error {e}"));
                assert_eq!(
                    e.to_string(),
                    expected,
                    "error string mismatch for {input:?}"
                );
            }
        }
    }
}

#[test]
fn format_matches_reference() {
    let cases = load();
    for case in cases["format"].as_array().unwrap() {
        let spec = spec_from_value(&case["spec"]);
        let reveal = case["options"]
            .get("revealToken")
            .and_then(|r| r.as_bool())
            .unwrap_or(false);
        let options = FormatEndpointOptions {
            reveal_token: reveal,
        };
        let got = format_endpoint_uri(&spec, options);
        let expected = case["formatted"].as_str().unwrap();
        assert_eq!(got, expected, "format mismatch for spec {:?}", case["spec"]);
    }
}

#[test]
fn is_hub_endpoint_matches_reference() {
    let cases = load();
    for case in cases["isHubEndpoint"].as_array().unwrap() {
        let spec = spec_from_value(&case["spec"]);
        let expected = case["expected"].as_bool().unwrap();
        assert_eq!(
            is_hub_endpoint(&spec),
            expected,
            "isHubEndpoint mismatch for {:?}",
            case["spec"]
        );
    }
}
