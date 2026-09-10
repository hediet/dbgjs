# Reference TS source for the M6 endpoint connector

The Codespace does **not** have the TypeScript monorepo checked out. These are the
two authoritative source excerpts the Rust connector must reproduce
**byte-identically** (error strings + parse behaviour). Port these — do NOT try to
import the built dist (the endpoint functions are tree-shaken out of it).

Source of truth in the TS repo:
- `vscode-team-tools/packages/hubrpc/hubrpc/src/connection/endpointUri.ts`
- `vscode-team-tools/packages/hubrpc/hubrpc/src/node/hubClient.ts` (connector half)

---

## `connection/endpointUri.ts` (verbatim)

```ts
/** Verbatim command line (`{ command }`) or pre-split argv (`{ argv }`). */
export type EndpointCommand =
    | { readonly command: string }
    | { readonly argv: readonly string[] };

/** Named pipe / unix-domain-socket the server already listens on. */
export interface SocketEndpoint {
    readonly kind: 'socket';
    readonly path: string;
    readonly token?: string;
}

/** A running WebSocket hub; `token` rides in the `Authorization` header. */
export interface WsEndpoint {
    readonly kind: 'ws';
    readonly url: string;
    readonly token?: string;
}

export interface CmdStdioEndpoint {
    readonly kind: 'cmd-stdio';
    readonly command: EndpointCommand;
    readonly env?: Readonly<Record<string, string>>;
}

export interface CmdEnvEndpoint {
    readonly kind: 'cmd-env';
    readonly command: EndpointCommand;
    readonly provisionSlot?: string;
    readonly env?: Readonly<Record<string, string>>;
}

export type ResolvedEndpoint =
    | SocketEndpoint
    | WsEndpoint
    | CmdStdioEndpoint
    | CmdEnvEndpoint;

const TOKEN_PARAM = 'token';
const COMMAND_PARAM = 'command';
const ARGV_PARAM = 'argv';
const PROVISION_SLOT_PARAM = 'provisionSlot';
const ENV_PARAM = 'env';

function _tryParseUrl(uri: string): URL | undefined {
    try {
        return new URL(uri);
    } catch {
        return undefined;
    }
}

function _looksLikeWsUrl(uri: string): boolean {
    return /^wss?:\/\//i.test(uri);
}

/** True for a Windows named-pipe path (`\\.\pipe\…` or `\\?\pipe\…`). */
function _isWindowsPipePath(path: string): boolean {
    return /^\\\\[.?]\\pipe\\/i.test(path);
}

function _parseCommand(url: URL): EndpointCommand {
    const argv = url.searchParams.getAll(ARGV_PARAM);
    if (argv.length > 0) {
        return { argv };
    }
    const command = url.searchParams.get(COMMAND_PARAM);
    if (command !== null) {
        return { command };
    }
    throw new Error(
        `endpoint '${url.protocol}' requires a '${COMMAND_PARAM}' or '${ARGV_PARAM}' query parameter`,
    );
}

/** Parse repeated `env=KEY=VALUE` params into a record (first `=` splits). */
function _parseEnv(url: URL): Readonly<Record<string, string>> | undefined {
    const entries = url.searchParams.getAll(ENV_PARAM);
    if (entries.length === 0) {
        return undefined;
    }
    const env: Record<string, string> = {};
    for (const entry of entries) {
        const eq = entry.indexOf('=');
        if (eq === -1) {
            throw new Error(`endpoint '${ENV_PARAM}' param must be 'KEY=VALUE', got '${entry}'`);
        }
        env[entry.slice(0, eq)] = entry.slice(eq + 1);
    }
    return env;
}

export function parseEndpointUri(uri: string): ResolvedEndpoint {
    const trimmed = uri.trim();
    if (trimmed === '') {
        throw new Error('endpoint URI is empty');
    }

    const url = _tryParseUrl(trimmed);
    if (!url) {
        // Bare form (no scheme): legacy HUBRPC_ENDPOINT.
        if (_looksLikeWsUrl(trimmed)) {
            return { kind: 'ws', url: trimmed };
        }
        return { kind: 'socket', path: trimmed };
    }

    switch (url.protocol) {
        case 'ws:':
        case 'wss:': {
            const token = url.searchParams.get(TOKEN_PARAM) ?? undefined;
            // The token travels in the Authorization header, never the URL.
            url.searchParams.delete(TOKEN_PARAM);
            const cleanUrl = url.toString();
            return token !== undefined ?
                { kind: 'ws', url: cleanUrl, token } :
                { kind: 'ws', url: cleanUrl };
        }
        case 'unix:': {
            const token = url.searchParams.get(TOKEN_PARAM) ?? undefined;
            const path = decodeURIComponent(url.pathname);
            return token !== undefined ?
                { kind: 'socket', path, token } :
                { kind: 'socket', path };
        }
        case 'npipe:': {
            const token = url.searchParams.get(TOKEN_PARAM) ?? undefined;
            // `npipe://./pipe/foo` → `\\.\pipe\foo`.
            const host = url.host === '' ? '.' : url.host;
            const tail = decodeURIComponent(url.pathname).replace(/\//g, '\\');
            const path = `\\\\${host}${tail}`;
            return token !== undefined ?
                { kind: 'socket', path, token } :
                { kind: 'socket', path };
        }
        case 'cmd-stdio:': {
            const env = _parseEnv(url);
            return env !== undefined ?
                { kind: 'cmd-stdio', command: _parseCommand(url), env } :
                { kind: 'cmd-stdio', command: _parseCommand(url) };
        }
        case 'cmd:': {
            const command = _parseCommand(url);
            const provisionSlot = url.searchParams.get(PROVISION_SLOT_PARAM) ?? undefined;
            const env = _parseEnv(url);
            return {
                kind: 'cmd-env',
                command,
                ...(provisionSlot !== undefined ? { provisionSlot } : {}),
                ...(env !== undefined ? { env } : {}),
            };
        }
        default:
            throw new Error(
                `unsupported endpoint scheme '${url.protocol}' (expected unix:, npipe:, ws:, wss:, cmd:, or cmd-stdio:)`,
            );
    }
}

export interface FormatEndpointOptions {
    readonly revealToken?: boolean;
}

const REDACTED = '***';

function _appendToken(params: URLSearchParams, token: string | undefined, reveal: boolean): void {
    if (token === undefined) return;
    params.set(TOKEN_PARAM, reveal ? token : REDACTED);
}

function _appendCommand(params: URLSearchParams, command: EndpointCommand): void {
    if ('argv' in command) {
        for (const a of command.argv) params.append(ARGV_PARAM, a);
    } else {
        params.set(COMMAND_PARAM, command.command);
    }
}

function _appendEnv(params: URLSearchParams, env: Readonly<Record<string, string>> | undefined): void {
    if (env === undefined) return;
    for (const [key, value] of Object.entries(env)) {
        params.append(ENV_PARAM, `${key}=${value}`);
    }
}

export function formatEndpointUri(spec: ResolvedEndpoint, options?: FormatEndpointOptions): string {
    const reveal = options?.revealToken === true;
    switch (spec.kind) {
        case 'socket': {
            const params = new URLSearchParams();
            _appendToken(params, spec.token, reveal);
            const query = params.toString();
            const suffix = query === '' ? '' : `?${query}`;
            if (_isWindowsPipePath(spec.path)) {
                // `\\.\pipe\foo` → `npipe://./pipe/foo`.
                const m = spec.path.match(/^\\\\([.?])\\(.*)$/);
                const host = m ? m[1] : '.';
                const tail = (m ? m[2] : spec.path).replace(/\\/g, '/');
                return `npipe://${host}/${tail}${suffix}`;
            }
            return `unix:${spec.path}${suffix}`;
        }
        case 'ws': {
            const url = new URL(spec.url);
            _appendToken(url.searchParams, spec.token, reveal);
            return url.toString();
        }
        case 'cmd-stdio': {
            const params = new URLSearchParams();
            _appendCommand(params, spec.command);
            _appendEnv(params, spec.env);
            return `cmd-stdio:?${params.toString()}`;
        }
        case 'cmd-env': {
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

/** True for endpoints that connect to existing, possibly-remote infrastructure. */
export function isHubEndpoint(spec: ResolvedEndpoint): spec is SocketEndpoint | WsEndpoint {
    return spec.kind === 'socket' || spec.kind === 'ws';
}
```

---

## `node/hubClient.ts` — connector half (the bits Rust M6 must reproduce)

```ts
export const HUBRPC_ENDPOINT_VAR = 'HUBRPC_ENDPOINT';
export const HUBRPC_TOKEN_VAR = 'HUBRPC_TOKEN';

// openHubChannel: resolve endpoint + token from env, error if endpoint missing.
const endpoint = options.endpoint ?? process.env[HUBRPC_ENDPOINT_VAR];
const token = options.token ?? process.env[HUBRPC_TOKEN_VAR] ?? '';
if (!endpoint) {
    throw new Error(`${HUBRPC_ENDPOINT_VAR} is not set; cannot connect to hubrpc hub.`);
}

// Transport selection (_openHubTransport):
function _isWebSocketEndpoint(endpoint: string): boolean {
    return /^wss?:\/\//i.test(endpoint);
}

// ws/wss  → WebSocket; token rides in `Authorization: Bearer <token>` header; NO handshake message.
// anything else (UDS / named pipe) → open socket, then send ONE newline-terminated
//   `hubrpc::initialize` request `{"jsonrpc":"2.0","id":0,"method":"hubrpc::initialize",
//   "params":{"protocolVersion":1,"token":"<token>"}}\n` BEFORE any RPC, await its reply,
//   then NDJSON framing. An immediate socket close == auth failure.
socket.write(JSON.stringify({ jsonrpc: '2.0', id: 0, method: 'hubrpc::initialize', params: { protocolVersion: 1, token } }) + '\n');
```

Token precedence for the env-var connector path (`hubrpc-cli/src/endpoint.ts`):
`--endpoint-token ?? token-from-URI ?? HUBRPC_TOKEN ?? ""`.

### Conformance generator note
`conformance/generate/gen-endpoint.mjs` must reimplement `parseEndpointUri` /
`formatEndpointUri` from the verbatim TS above using Node's `URL`, then emit
`conformance/vectors/endpoint.json`. The Rust `parse_endpoint_uri` (using the
`url` crate) is asserted against those vectors in
`crates/hubrpc/tests/endpoint_conformance.rs`. Verify WHATWG parity edge cases:
bare `/tmp/x.sock` and `\\.\pipe\foo` must FAIL `url::Url::parse` (→ bare path),
while `ws://bare-detect` must parse as a URL.
