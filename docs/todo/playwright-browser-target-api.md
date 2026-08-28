# Playwright API over browser targets

## Goal

Offer an optional JavaScript API for Playwright-style page automation on a
browser target already selected and owned by `jsdbg`.

## Capabilities

- Open a short-lived automation session for an unambiguous browser target.
- Run ordinary Playwright page operations and return structured results.
- Compose automation with existing target selection, raw CDP, waits, and
  debugger operations.
- Reject disconnected, replaced-generation, non-page, and ambiguous targets.

## Concepts

- **Target selector:** the existing context/connection/target scope.
- **Target lease:** an ephemeral, generation-bound capability for one live
  browser target.
- **Automation session:** a live adapter created from a lease and closed
  independently of the debugger attachment.
- **User program:** JavaScript supplied to the adapter.
- **Result:** a JSON-serializable value or an explicit execution error.

Selectors are durable intent. Leases and automation sessions are live handles
and must never be persisted or reused after reconnection.

## Primitive basis

1. `resolveTarget(selector) -> lease`
2. `openPlaywright(lease) -> session`
3. `evaluate(session, program, input) -> result`
4. `close(session)`

This keeps target selection, lifecycle, execution, and presentation
independent. It avoids separate commands for click, fill, screenshot, or each
future Playwright operation; those are compositions inside the user program.
Raw CDP remains a separate lower-level primitive rather than a hidden fallback.

## Composition

```text
selector -> lease -> Playwright session -> evaluate program -> JSON result
```

A one-shot CLI convenience may perform all four steps. A service API may keep a
session open for several calls, but must expose the same lifecycle and safety
rules.

## Laws and edge cases

- A lease is valid only for its exact connection generation and target ID.
- Ambiguity and absence are errors; no first-match selection is allowed.
- Closing a session is idempotent and does not detach the debugger target.
- Program failure preserves Playwright name, message, stack, and cause details.
- Non-JSON results are rejected explicitly rather than stringified silently.
- Client cancellation closes the automation session and aborts pending work.
- Concurrent sessions require an explicit policy; they must not silently share
  mutable page defaults or event handlers.

## Initial implementation

`jsdbg page playwright -` reads one program from stdin, while
`jsdbg page playwright --eval <program>` executes inline source. The program
receives `page` in global scope and can return one JSON value.

The control plane remains on the daemon's authenticated Unix socket (Unix) or
named pipe (Windows). Playwright's public `connectOverCDP` API accepts HTTP or
WebSocket URLs, not those native IPC transports, so the daemon creates a
single-use authenticated capability URL on a random loopback-only WebSocket
listener. The virtual endpoint filters discovery and attachment to the exact
selected page and rejects requests naming another target.

Sessions are bound to the selected connection generation, cancel when their
control lease closes or the connection changes, wait at most 15 seconds for a
client, and live at most 45 seconds. CLI execution is limited to 30 seconds;
program input and JSON result are each limited to 1 MiB, protocol messages to
16 MiB, and diagnostics to 64 KiB.

The CDP source is an explicit adapter (`PlaywrightCdpSource`). Only
browser-root CDP connections are implemented initially. Direct Node targets
and Electron renderer bridge targets are rejected rather than exposed as fake
browser roots; a future Electron adapter can implement the same source
boundary without depending on Playwright internals.

## Deferred choices

- Sandboxing and module loading.
- Long-lived session syntax and structured streaming output.
