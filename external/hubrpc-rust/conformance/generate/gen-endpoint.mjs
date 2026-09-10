// Generates conformance/vectors/endpoint.json.
//
// This is a faithful, dependency-free reimplementation of the TS endpoint logic in
// `hubrpc/src/connection/endpointUri.ts` (the real functions are tree-shaken out of the dist,
// so we port the ~70 lines verbatim using Node's `URL`, exactly like the source). The Rust
// `parse_endpoint_uri`/`format_endpoint_uri`/`is_hub_endpoint` (crates/hubrpc/src/connection/
// endpoint.rs) are asserted against these vectors in `endpoint_conformance.rs`.

import { writeFileSync, mkdirSync } from "node:fs";
import { fileURLToPath } from "node:url";
import { dirname, join } from "node:path";

// ── verbatim port of endpointUri.ts ───────────────────────────────────────────
const TOKEN_PARAM = "token";
const COMMAND_PARAM = "command";
const ARGV_PARAM = "argv";
const PROVISION_SLOT_PARAM = "provisionSlot";
const ENV_PARAM = "env";

function _tryParseUrl(uri) {
  try {
    return new URL(uri);
  } catch {
    return undefined;
  }
}

function _looksLikeWsUrl(uri) {
  return /^wss?:\/\//i.test(uri);
}

function _isWindowsPipePath(path) {
  return /^\\\\[.?]\\pipe\\/i.test(path);
}

function _parseCommand(url) {
  const argv = url.searchParams.getAll(ARGV_PARAM);
  if (argv.length > 0) {
    return { argv };
  }
  const command = url.searchParams.get(COMMAND_PARAM);
  if (command !== null) {
    return { command };
  }
  throw new Error(
    `endpoint '${url.protocol}' requires a '${COMMAND_PARAM}' or '${ARGV_PARAM}' query parameter`
  );
}

function _parseEnv(url) {
  const entries = url.searchParams.getAll(ENV_PARAM);
  if (entries.length === 0) {
    return undefined;
  }
  const env = {};
  for (const entry of entries) {
    const eq = entry.indexOf("=");
    if (eq === -1) {
      throw new Error(`endpoint '${ENV_PARAM}' param must be 'KEY=VALUE', got '${entry}'`);
    }
    env[entry.slice(0, eq)] = entry.slice(eq + 1);
  }
  return env;
}

function parseEndpointUri(uri) {
  const trimmed = uri.trim();
  if (trimmed === "") {
    throw new Error("endpoint URI is empty");
  }

  const url = _tryParseUrl(trimmed);
  if (!url) {
    if (_looksLikeWsUrl(trimmed)) {
      return { kind: "ws", url: trimmed };
    }
    return { kind: "socket", path: trimmed };
  }

  switch (url.protocol) {
    case "ws:":
    case "wss:": {
      const token = url.searchParams.get(TOKEN_PARAM) ?? undefined;
      url.searchParams.delete(TOKEN_PARAM);
      const cleanUrl = url.toString();
      return token !== undefined
        ? { kind: "ws", url: cleanUrl, token }
        : { kind: "ws", url: cleanUrl };
    }
    case "unix:": {
      const token = url.searchParams.get(TOKEN_PARAM) ?? undefined;
      const path = decodeURIComponent(url.pathname);
      return token !== undefined ? { kind: "socket", path, token } : { kind: "socket", path };
    }
    case "npipe:": {
      const token = url.searchParams.get(TOKEN_PARAM) ?? undefined;
      const host = url.host === "" ? "." : url.host;
      const tail = decodeURIComponent(url.pathname).replace(/\//g, "\\");
      const path = `\\\\${host}${tail}`;
      return token !== undefined ? { kind: "socket", path, token } : { kind: "socket", path };
    }
    case "cmd-stdio:": {
      const env = _parseEnv(url);
      return env !== undefined
        ? { kind: "cmd-stdio", command: _parseCommand(url), env }
        : { kind: "cmd-stdio", command: _parseCommand(url) };
    }
    case "cmd:": {
      const command = _parseCommand(url);
      const provisionSlot = url.searchParams.get(PROVISION_SLOT_PARAM) ?? undefined;
      const env = _parseEnv(url);
      return {
        kind: "cmd-env",
        command,
        ...(provisionSlot !== undefined ? { provisionSlot } : {}),
        ...(env !== undefined ? { env } : {}),
      };
    }
    default:
      throw new Error(
        `unsupported endpoint scheme '${url.protocol}' (expected unix:, npipe:, ws:, wss:, cmd:, or cmd-stdio:)`
      );
  }
}

const REDACTED = "***";

function _appendToken(params, token, reveal) {
  if (token === undefined) return;
  params.set(TOKEN_PARAM, reveal ? token : REDACTED);
}

function _appendCommand(params, command) {
  if ("argv" in command) {
    for (const a of command.argv) params.append(ARGV_PARAM, a);
  } else {
    params.set(COMMAND_PARAM, command.command);
  }
}

function _appendEnv(params, env) {
  if (env === undefined) return;
  for (const [key, value] of Object.entries(env)) {
    params.append(ENV_PARAM, `${key}=${value}`);
  }
}

function formatEndpointUri(spec, options) {
  const reveal = options?.revealToken === true;
  switch (spec.kind) {
    case "socket": {
      const params = new URLSearchParams();
      _appendToken(params, spec.token, reveal);
      const query = params.toString();
      const suffix = query === "" ? "" : `?${query}`;
      if (_isWindowsPipePath(spec.path)) {
        const m = spec.path.match(/^\\\\([.?])\\(.*)$/);
        const host = m ? m[1] : ".";
        const tail = (m ? m[2] : spec.path).replace(/\\/g, "/");
        return `npipe://${host}/${tail}${suffix}`;
      }
      return `unix:${spec.path}${suffix}`;
    }
    case "ws": {
      const url = new URL(spec.url);
      _appendToken(url.searchParams, spec.token, reveal);
      return url.toString();
    }
    case "cmd-stdio": {
      const params = new URLSearchParams();
      _appendCommand(params, spec.command);
      _appendEnv(params, spec.env);
      return `cmd-stdio:?${params.toString()}`;
    }
    case "cmd-env": {
      const params = new URLSearchParams();
      _appendCommand(params, spec.command);
      if (spec.provisionSlot !== undefined) {
        params.set(PROVISION_SLOT_PARAM, spec.provisionSlot);
      }
      _appendEnv(params, spec.env);
      return `cmd:?${params.toString()}`;
    }
  }
}

function isHubEndpoint(spec) {
  return spec.kind === "socket" || spec.kind === "ws";
}

// ── fixtures ───────────────────────────────────────────────────────────────────
const parseInputs = [
  // bare-string auto-detect
  "/tmp/bare.sock",
  "\\\\.\\pipe\\foo",
  "ws://bare-detect",
  "WSS://Bare-Detect/p",
  // ws / wss with token strip
  "ws://hub.example/rpc?token=abc123",
  "ws://hub.example/rpc?token=abc123&x=1",
  "wss://hub.example:8443/rpc",
  "ws://hub.example/rpc?",
  // unix
  "unix:/tmp/hub.sock",
  "unix:/tmp/hub.sock?token=t0ken",
  "unix:/tmp/a%20b.sock",
  // npipe
  "npipe://./pipe/foo",
  "npipe://./pipe/hub?token=zz",
  // cmd-stdio
  "cmd-stdio:?command=run-hub",
  "cmd-stdio:?argv=run-hub&argv=--flag",
  "cmd-stdio:?command=run-hub&env=A=1&env=B=2",
  // cmd
  "cmd:?command=run-hub",
  "cmd:?command=run-hub&provisionSlot=slot-7",
  "cmd:?argv=run&argv=--x&provisionSlot=s&env=K=V",
  // errors
  "",
  "   ",
  "http://hub.example/rpc",
  "cmd:?provisionSlot=s",
  "cmd-stdio:?env=BROKEN",
];

const parse = parseInputs.map((input) => {
  try {
    return { input, result: parseEndpointUri(input) };
  } catch (e) {
    return { input, error: e.message };
  }
});

const formatSpecs = [
  { spec: { kind: "socket", path: "/tmp/hub.sock" } },
  { spec: { kind: "socket", path: "/tmp/hub.sock", token: "secret" } },
  { spec: { kind: "socket", path: "/tmp/hub.sock", token: "secret" }, options: { revealToken: true } },
  { spec: { kind: "socket", path: "\\\\.\\pipe\\foo" } },
  { spec: { kind: "socket", path: "\\\\.\\pipe\\foo", token: "abc" } },
  { spec: { kind: "ws", url: "ws://hub.example/rpc" } },
  { spec: { kind: "ws", url: "ws://hub.example/rpc", token: "abc123" } },
  { spec: { kind: "ws", url: "ws://hub.example/rpc", token: "abc123" }, options: { revealToken: true } },
  { spec: { kind: "ws", url: "wss://hub.example:8443/rpc?x=1", token: "abc123" }, options: { revealToken: true } },
  { spec: { kind: "cmd-stdio", command: { command: "run-hub" } } },
  { spec: { kind: "cmd-stdio", command: { argv: ["run-hub", "--flag"] } } },
  { spec: { kind: "cmd-stdio", command: { command: "run-hub" }, env: { A: "1", B: "2" } } },
  { spec: { kind: "cmd-env", command: { command: "run-hub" }, provisionSlot: "slot-7" } },
  { spec: { kind: "cmd-env", command: { argv: ["run", "--x"] }, env: { K: "V" } } },
];

const format = formatSpecs.map(({ spec, options }) => ({
  spec,
  options: options ?? null,
  formatted: formatEndpointUri(spec, options),
}));

const isHub = [
  { kind: "socket", path: "/tmp/x.sock" },
  { kind: "ws", url: "ws://h/p" },
  { kind: "cmd-stdio", command: { command: "x" } },
  { kind: "cmd-env", command: { command: "x" } },
].map((spec) => ({ spec, expected: isHubEndpoint(spec) }));

const here = dirname(fileURLToPath(import.meta.url));
const outDir = join(here, "..", "vectors");
mkdirSync(outDir, { recursive: true });
const path = join(outDir, "endpoint.json");
const data = { parse, format, isHubEndpoint: isHub };
writeFileSync(path, JSON.stringify(data, null, 2) + "\n");
console.log(
  `wrote endpoint.json (${parse.length} parse, ${format.length} format, ${isHub.length} isHub cases)`
);
