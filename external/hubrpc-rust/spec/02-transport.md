# 02 — Transport

This chapter defines the abstract transport hubrpc requires, three concrete bindings (NDJSON over a byte stream, WebSocket, and stdio), and the **endpoint URI** vocabulary that names where a node listens and how to reach or start it. The message layer (chapter 01) is binding-independent; this chapter is the only place that touches bytes-on-the-wire framing.

## 1. Abstract transport

A hubrpc transport is a reliable, ordered, bidirectional channel that carries discrete **messages**, each a single JSON-RPC value (chapter 01). A conformant transport binding MUST:

- deliver each message whole — a reader receives exactly the JSON values a writer sent, with message boundaries preserved;
- preserve order within each direction — messages a writer sends in order arrive in that order;
- be bidirectional — both ends may send at any time after the connection is established.

A transport MUST NOT be assumed to order messages *across* directions. A transport MAY close in either direction; on close, a node MUST fail every in-flight request it originated that has not been answered with `peerDisconnected` (chapter 01 §4).

> **Note.** The transport carries opaque JSON messages; it has no knowledge of methods, ids, or layers. All correlation is at the message layer.

## 2. NDJSON binding (byte streams)

The newline-delimited JSON binding runs over any reliable ordered byte stream — a Unix-domain socket, a Windows named pipe, or a process's stdin/stdout pair. It is the primary binding.

- Each message is encoded as its JSON text ([RFC 8259](https://www.rfc-editor.org/rfc/rfc8259), UTF-8) containing no unescaped U+000A, followed by a single `\n` (U+000A).
- A reader MUST split the stream on `\n`, trim surrounding whitespace from each line, ignore empty lines, and parse each non-empty line as one message.
- A reader MUST silently ignore a line that is not valid JSON.

> **Rationale.** Length-prefixed framing (LSP-style headers) adds nothing for a private link between two cooperating processes and forces a stateful framer; one JSON value per line is sufficient and human-readable.

### 2.1 Initialize handshake

On a connection that authenticates or negotiates transport details — e.g. a socket or named-pipe connection that carries an attribution **token** — the dialing node MUST send, as the very first message before any other JSON-RPC message, a `hubrpc::initialize` **request**:

```
{"jsonrpc":"2.0","id":0,"method":"hubrpc::initialize","params":{"protocolVersion":1,"token":"<token>"}}\n
```

where `protocolVersion` is the transport protocol version (currently `1`) and `token` is the attribution token (the field is omitted when none). The accepting node MUST require `hubrpc::initialize` as the first message, validate the token, and reply on the same `id` with the result `{"protocolVersion":1}`. If the token is not accepted, the accepting node MUST reply with an error (`invalidRequest`, message `"unauthenticated"`) and then drop the connection.

`hubrpc::initialize` is handled entirely by the transport layer: it is never forwarded to the hub or any service and is not a routed call. A node that requires the handshake MUST complete it before exchanging any other message. The handshake MAY be omitted for trusted links that need neither authentication nor negotiation (e.g. stdio, §4).

> **Note.** Provenance-authenticated deployments may ignore the token; an omitted (or empty) token is therefore valid. The handshake is an *ordering predicate* (chapter 00 §4): the `hubrpc::initialize` request MUST be the first message on the stream and its reply the first message back.

## 3. WebSocket binding

Over a WebSocket, each message is one text frame containing the message's JSON text; framing is the WebSocket frame itself, with no `\n` delimiter. An attribution token, when present, MUST be sent in the `Authorization: Bearer <token>` request header of the opening handshake, never in the URL or a preamble.

## 4. stdio binding

A node MAY be reached by spawning a child process and speaking the NDJSON binding (§2) over the child's stdin (node → child) and stdout (child → node). The child's stderr is not part of the transport. No `hubrpc::initialize` handshake is used on stdio; a spawned child receives its attribution out of band (§5.3).

## 5. Endpoint URIs

An **endpoint URI** names where a hubrpc node lives and how to reach or start it. Every endpoint URI is a valid [RFC 3986](https://www.rfc-editor.org/rfc/rfc3986) URI and is safe to place in environment variables, logs, and configuration.

### 5.1 Schemes

| Scheme | Resolves to | Token |
|---|---|---|
| `unix:` | socket at the decoded path | `?token=` query |
| `npipe:` | Windows named pipe (see §5.2) | `?token=` query |
| `ws:` / `wss:` | WebSocket at the URL | `?token=` query → `Authorization` header |
| `cmd-stdio:` | spawn a child, speak stdio (§4) | — |
| `cmd:` | spawn a child against a freshly-started local node, passing it `HUBRPC_ENDPOINT`/`HUBRPC_TOKEN` | — |

- For `ws:`/`wss:`, a `token` query parameter MUST be removed from the URL and carried in the `Authorization` header (§3).
- For `unix:`, the socket path is the percent-decoded URI path.
- For `npipe://<host>/<tail>`, the path is `\\<host>\<tail>` with `/` in `<tail>` replaced by `\`, and `<host>` defaulting to `.` when empty.
- For `cmd-stdio:` and `cmd:`, the command is `?command=<verbatim line>` (split by the OS shell) **or** one-or-more repeated `?argv=<arg>` parameters (structure-preserving); one of the two MUST be present. Repeated `?env=KEY=VALUE` parameters inject environment variables.

### 5.2 Bare (scheme-less) form

A bare string with no parseable scheme is auto-detected: a value matching `^wss?://` (case-insensitive) is a WebSocket URL; any other value is a socket path.

> **Note.** This is the legacy `HUBRPC_ENDPOINT` form. It carries no token in the string.

### 5.3 Environment and token precedence

A node that starts from the environment reads its endpoint from `HUBRPC_ENDPOINT` and its token from `HUBRPC_TOKEN`. When resolving the token to use for a connection, a node MUST apply this precedence (first defined wins):

1. an explicit token supplied by the caller;
2. the `token` query parameter of the endpoint URI;
3. `HUBRPC_TOKEN`;
4. the empty string.

> **Rationale.** The north-star deployment is exactly this: a node reads `HUBRPC_ENDPOINT` + `HUBRPC_TOKEN`, dials a `unix:` socket, performs the `hubrpc::initialize` handshake, and speaks NDJSON.
