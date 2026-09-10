//! Endpoint URI parsing/formatting — a byte-for-byte port of the TS reference
//! (`hubrpc/src/connection/endpointUri.ts`).
//!
//! [`parse_endpoint_uri`] resolves a `HUBRPC_ENDPOINT` string into a [`ResolvedEndpoint`];
//! [`format_endpoint_uri`] is the inverse (redacting the token by default); [`is_hub_endpoint`]
//! reports whether a spec connects to existing infrastructure (socket/ws). Error strings are kept
//! identical to TypeScript so the shared `endpoint.json` conformance vectors pass on both runtimes.
//!
//! Parsing uses the WHATWG-compatible `url` crate, which agrees with Node's `URL` on every case the
//! connector cares about: bare paths (`/tmp/x.sock`) and Windows pipe paths (`\\.\pipe\foo`) fail to
//! parse and fall back to a socket path, while `ws://…` parses as a URL.

use std::collections::BTreeMap;

use percent_encoding::percent_decode_str;
use url::Url;

const TOKEN_PARAM: &str = "token";
const COMMAND_PARAM: &str = "command";
const ARGV_PARAM: &str = "argv";
const PROVISION_SLOT_PARAM: &str = "provisionSlot";
const ENV_PARAM: &str = "env";

const REDACTED: &str = "***";

/// A command to spawn, either verbatim (shell-split later) or as pre-split argv.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum EndpointCommand {
    /// A single command line to be shell-split by the spawner.
    Command(String),
    /// Pre-split argument vector (structure preserved).
    Argv(Vec<String>),
}

/// A resolved `HUBRPC_ENDPOINT`, mirroring the TS `ResolvedEndpoint` union.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ResolvedEndpoint {
    /// A named pipe / Unix-domain socket the hub already listens on.
    Socket {
        /// Filesystem path (UDS) or Windows pipe path.
        path: String,
        /// Auth token written in the `hello` preamble.
        token: Option<String>,
    },
    /// A running WebSocket hub; the token rides in the `Authorization` header.
    Ws {
        /// The cleaned URL (any `?token=` has been stripped out).
        url: String,
        /// Auth token (never travels in the URL).
        token: Option<String>,
    },
    /// Spawn a child and talk over its stdio.
    CmdStdio {
        /// Command to spawn.
        command: EndpointCommand,
        /// Extra environment variables for the child.
        env: Option<BTreeMap<String, String>>,
    },
    /// Start a local hub and inject `HUBRPC_ENDPOINT`/`HUBRPC_TOKEN` into the child.
    CmdEnv {
        /// Command to spawn.
        command: EndpointCommand,
        /// Optional provision slot.
        provision_slot: Option<String>,
        /// Extra environment variables for the child.
        env: Option<BTreeMap<String, String>>,
    },
}

/// Error returned by [`parse_endpoint_uri`]; its `Display` matches the TS error strings verbatim.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EndpointError(String);

impl EndpointError {
    fn new(msg: impl Into<String>) -> Self {
        EndpointError(msg.into())
    }
}

impl std::fmt::Display for EndpointError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::error::Error for EndpointError {}

fn looks_like_ws_url(uri: &str) -> bool {
    let lower = uri.to_ascii_lowercase();
    lower.starts_with("ws://") || lower.starts_with("wss://")
}

/// True for a Windows named-pipe path (`\\.\pipe\…` or `\\?\pipe\…`).
fn is_windows_pipe_path(path: &str) -> bool {
    let bytes = path.as_bytes();
    bytes.len() >= 9
        && &bytes[0..2] == b"\\\\"
        && (bytes[2] == b'.' || bytes[2] == b'?')
        && bytes[3] == b'\\'
        && path[4..].to_ascii_lowercase().starts_with("pipe\\")
}

/// The TS `url.protocol` form (scheme plus trailing colon, e.g. `cmd-stdio:`).
fn protocol(url: &Url) -> String {
    format!("{}:", url.scheme())
}

fn get_param(url: &Url, name: &str) -> Option<String> {
    url.query_pairs()
        .find(|(k, _)| k == name)
        .map(|(_, v)| v.into_owned())
}

fn get_all_params(url: &Url, name: &str) -> Vec<String> {
    url.query_pairs()
        .filter(|(k, _)| k == name)
        .map(|(_, v)| v.into_owned())
        .collect()
}

fn parse_command(url: &Url) -> Result<EndpointCommand, EndpointError> {
    let argv = get_all_params(url, ARGV_PARAM);
    if !argv.is_empty() {
        return Ok(EndpointCommand::Argv(argv));
    }
    if let Some(command) = get_param(url, COMMAND_PARAM) {
        return Ok(EndpointCommand::Command(command));
    }
    Err(EndpointError::new(format!(
        "endpoint '{}' requires a '{COMMAND_PARAM}' or '{ARGV_PARAM}' query parameter",
        protocol(url)
    )))
}

fn parse_env(url: &Url) -> Result<Option<BTreeMap<String, String>>, EndpointError> {
    let entries = get_all_params(url, ENV_PARAM);
    if entries.is_empty() {
        return Ok(None);
    }
    let mut env = BTreeMap::new();
    for entry in entries {
        match entry.find('=') {
            Some(eq) => {
                env.insert(entry[..eq].to_string(), entry[eq + 1..].to_string());
            }
            None => {
                return Err(EndpointError::new(format!(
                    "endpoint '{ENV_PARAM}' param must be 'KEY=VALUE', got '{entry}'"
                )))
            }
        }
    }
    Ok(Some(env))
}

/// Strip `?token=` from a `ws:`/`wss:` URL and return the cleaned URL string, matching Node's
/// `URLSearchParams.delete` + `URL.toString()` re-serialization.
fn clean_ws_url(url: &Url) -> String {
    let mut cleaned = url.clone();
    let pairs: Vec<(String, String)> = url
        .query_pairs()
        .filter(|(k, _)| k != TOKEN_PARAM)
        .map(|(k, v)| (k.into_owned(), v.into_owned()))
        .collect();
    if pairs.is_empty() {
        cleaned.set_query(None);
    } else {
        let mut serializer = cleaned.query_pairs_mut();
        serializer.clear();
        for (k, v) in &pairs {
            serializer.append_pair(k, v);
        }
        drop(serializer);
    }
    cleaned.to_string()
}

/// Parse a `HUBRPC_ENDPOINT` URI into a [`ResolvedEndpoint`].
pub fn parse_endpoint_uri(uri: &str) -> Result<ResolvedEndpoint, EndpointError> {
    let trimmed = uri.trim();
    if trimmed.is_empty() {
        return Err(EndpointError::new("endpoint URI is empty"));
    }

    let url = match Url::parse(trimmed) {
        Ok(url) => url,
        // Bare form (no scheme): legacy HUBRPC_ENDPOINT.
        Err(_) => {
            if looks_like_ws_url(trimmed) {
                return Ok(ResolvedEndpoint::Ws {
                    url: trimmed.to_string(),
                    token: None,
                });
            }
            return Ok(ResolvedEndpoint::Socket {
                path: trimmed.to_string(),
                token: None,
            });
        }
    };

    match url.scheme() {
        "ws" | "wss" => {
            let token = get_param(&url, TOKEN_PARAM);
            // The token travels in the Authorization header, never the URL.
            let clean_url = clean_ws_url(&url);
            Ok(ResolvedEndpoint::Ws {
                url: clean_url,
                token,
            })
        }
        "unix" => {
            let token = get_param(&url, TOKEN_PARAM);
            let path = percent_decode_str(url.path()).decode_utf8_lossy().into_owned();
            Ok(ResolvedEndpoint::Socket { path, token })
        }
        "npipe" => {
            let token = get_param(&url, TOKEN_PARAM);
            // `npipe://./pipe/foo` → `\\.\pipe\foo`.
            let host = match url.host_str() {
                Some(h) if !h.is_empty() => h.to_string(),
                _ => ".".to_string(),
            };
            let tail = percent_decode_str(url.path())
                .decode_utf8_lossy()
                .replace('/', "\\");
            let path = format!("\\\\{host}{tail}");
            Ok(ResolvedEndpoint::Socket { path, token })
        }
        "cmd-stdio" => {
            let env = parse_env(&url)?;
            let command = parse_command(&url)?;
            Ok(ResolvedEndpoint::CmdStdio { command, env })
        }
        "cmd" => {
            let command = parse_command(&url)?;
            let provision_slot = get_param(&url, PROVISION_SLOT_PARAM);
            let env = parse_env(&url)?;
            Ok(ResolvedEndpoint::CmdEnv {
                command,
                provision_slot,
                env,
            })
        }
        _ => Err(EndpointError::new(format!(
            "unsupported endpoint scheme '{}' (expected unix:, npipe:, ws:, wss:, cmd:, or cmd-stdio:)",
            protocol(&url)
        ))),
    }
}

/// Options for [`format_endpoint_uri`].
#[derive(Debug, Clone, Copy, Default)]
pub struct FormatEndpointOptions {
    /// When true, the token is written verbatim instead of being redacted as `***`.
    pub reveal_token: bool,
}

fn encode_query(pairs: &[(String, String)]) -> String {
    let mut serializer = url::form_urlencoded::Serializer::new(String::new());
    for (k, v) in pairs {
        serializer.append_pair(k, v);
    }
    serializer.finish()
}

/// Format a [`ResolvedEndpoint`] back into a URI (inverse of [`parse_endpoint_uri`]).
///
/// The token is redacted as `***` unless [`FormatEndpointOptions::reveal_token`] is set.
pub fn format_endpoint_uri(spec: &ResolvedEndpoint, options: FormatEndpointOptions) -> String {
    let reveal = options.reveal_token;
    match spec {
        ResolvedEndpoint::Socket { path, token } => {
            let mut pairs = Vec::new();
            if let Some(token) = token {
                let value = if reveal {
                    token.clone()
                } else {
                    REDACTED.to_string()
                };
                pairs.push((TOKEN_PARAM.to_string(), value));
            }
            let query = encode_query(&pairs);
            let suffix = if query.is_empty() {
                String::new()
            } else {
                format!("?{query}")
            };
            if is_windows_pipe_path(path) {
                // `\\.\pipe\foo` → `npipe://./pipe/foo`.
                let host = path.as_bytes()[2] as char;
                let tail = path[3..].replace('\\', "/");
                format!("npipe://{host}{tail}{suffix}")
            } else {
                format!("unix:{path}{suffix}")
            }
        }
        ResolvedEndpoint::Ws { url, token } => {
            let mut parsed = Url::parse(url).expect("ws endpoint url is valid");
            if let Some(token) = token {
                let value = if reveal {
                    token.clone()
                } else {
                    REDACTED.to_string()
                };
                parsed.query_pairs_mut().append_pair(TOKEN_PARAM, &value);
            }
            parsed.to_string()
        }
        ResolvedEndpoint::CmdStdio { command, env } => {
            let mut pairs = command_pairs(command);
            append_env_pairs(&mut pairs, env);
            format!("cmd-stdio:?{}", encode_query(&pairs))
        }
        ResolvedEndpoint::CmdEnv {
            command,
            provision_slot,
            env,
        } => {
            let mut pairs = command_pairs(command);
            if let Some(slot) = provision_slot {
                pairs.push((PROVISION_SLOT_PARAM.to_string(), slot.clone()));
            }
            append_env_pairs(&mut pairs, env);
            format!("cmd:?{}", encode_query(&pairs))
        }
    }
}

fn command_pairs(command: &EndpointCommand) -> Vec<(String, String)> {
    match command {
        EndpointCommand::Argv(argv) => argv
            .iter()
            .map(|a| (ARGV_PARAM.to_string(), a.clone()))
            .collect(),
        EndpointCommand::Command(command) => {
            vec![(COMMAND_PARAM.to_string(), command.clone())]
        }
    }
}

fn append_env_pairs(pairs: &mut Vec<(String, String)>, env: &Option<BTreeMap<String, String>>) {
    if let Some(env) = env {
        for (k, v) in env {
            pairs.push((ENV_PARAM.to_string(), format!("{k}={v}")));
        }
    }
}

/// True for endpoints that connect to existing, possibly-remote infrastructure (socket/ws).
pub fn is_hub_endpoint(spec: &ResolvedEndpoint) -> bool {
    matches!(
        spec,
        ResolvedEndpoint::Socket { .. } | ResolvedEndpoint::Ws { .. }
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bare_string_autodetects_socket_and_ws() {
        assert_eq!(
            parse_endpoint_uri("/tmp/bare.sock").unwrap(),
            ResolvedEndpoint::Socket {
                path: "/tmp/bare.sock".to_string(),
                token: None
            }
        );
        assert_eq!(
            parse_endpoint_uri(r"\\.\pipe\foo").unwrap(),
            ResolvedEndpoint::Socket {
                path: r"\\.\pipe\foo".to_string(),
                token: None
            }
        );
        match parse_endpoint_uri("ws://bare-detect").unwrap() {
            ResolvedEndpoint::Ws { url, token } => {
                assert_eq!(url, "ws://bare-detect/");
                assert_eq!(token, None);
            }
            other => panic!("expected ws, got {other:?}"),
        }
    }

    #[test]
    fn ws_strips_token() {
        assert_eq!(
            parse_endpoint_uri("ws://h/rpc?token=abc&x=1").unwrap(),
            ResolvedEndpoint::Ws {
                url: "ws://h/rpc?x=1".to_string(),
                token: Some("abc".to_string())
            }
        );
    }

    #[test]
    fn errors_match_ts() {
        assert_eq!(
            parse_endpoint_uri("").unwrap_err().to_string(),
            "endpoint URI is empty"
        );
        assert_eq!(
            parse_endpoint_uri("http://h/rpc").unwrap_err().to_string(),
            "unsupported endpoint scheme 'http:' (expected unix:, npipe:, ws:, wss:, cmd:, or cmd-stdio:)"
        );
        assert_eq!(
            parse_endpoint_uri("cmd:?provisionSlot=s")
                .unwrap_err()
                .to_string(),
            "endpoint 'cmd:' requires a 'command' or 'argv' query parameter"
        );
        assert_eq!(
            parse_endpoint_uri("cmd-stdio:?env=BROKEN")
                .unwrap_err()
                .to_string(),
            "endpoint 'env' param must be 'KEY=VALUE', got 'BROKEN'"
        );
    }

    #[test]
    fn is_hub_endpoint_only_socket_ws() {
        assert!(is_hub_endpoint(
            &parse_endpoint_uri("unix:/tmp/x.sock").unwrap()
        ));
        assert!(is_hub_endpoint(&parse_endpoint_uri("ws://h/p").unwrap()));
        assert!(!is_hub_endpoint(
            &parse_endpoint_uri("cmd-stdio:?command=x").unwrap()
        ));
    }
}
