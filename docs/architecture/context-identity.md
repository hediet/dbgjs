# Context identity and selection

The CLI and service use path-based contexts for ordinary project work, plus
explicit named contexts for investigations that do not belong to a directory.
There is one daemon with a global context registry; a context's identity is
independent of the endpoint used to access that daemon.

## Identity

An expression beginning with `:` selects a named context (`:incident-42`).
Named IDs are lowercased and accept ASCII letters, digits, `.`, `_`, and `-`.
Otherwise an expression denotes a path context: combine relative paths with
the command's current working directory, lexically eliminate `.` and `..`,
normalize separators and lowercase the resulting absolute path. Resolution
does not require the path to exist, read the filesystem, or follow symlinks.
Unix absolute paths use `/`; Windows drive and UNC paths use `\`. Drive-relative
Windows paths (`C:foo`) are rejected; root-relative Windows paths use the
current working directory's drive root.

## Implicit selection

When a command needs a context, it chooses, in order:

1. An explicit context expression (`--context`).
2. The nearest cwd or ancestor binding established with `--set`.
3. The nearest registered path context at the cwd or an ancestor.
4. A context-required error.

The two ancestor searches are separate: even an inherited explicit binding
wins over a nearer automatic path match. A stale binding fails explicitly
instead of silently selecting another context. `--set` changes only the entry
for the exact cwd; per-context interactive view state is scoped to cwd/context,
not shared globally across projects.

`context list` is global but orders path contexts by distance from cwd,
ancestor first on ties, then normalized identity; named contexts follow.
Human and JSON views expose context kind, path distance and ancestor status.
Persistence migrates schema 1/2 contexts as named contexts and migrates a
legacy global CLI selection once into a cwd binding.

The single-folder VS Code extension uses its folder path as the context
expression, sharing the CLI's project context. Multi-root and untitled
workspace selection, optional extension suffixes, and multiple daemon
assignment remain **deferred, not approved implementations**. The earlier
[design and edge cases](https://github.com/hediet/dbgjs/blob/44cdfc0d59f4112c85c24e8b744f8bec96c69274/docs/todo/context-identity-and-selection.md)
are historical context, not a second source of truth.

## Idle connection timeout

Contexts default to `inf` (no automatic disconnect). Each connection inherits
its context's timeout unless it has an explicit override:

```sh
dbgjs context create :investigation "Investigation" --idle-timeout 2h --set
dbgjs context configure --idle-timeout 30m
dbgjs connection add <ws-endpoint> --connection browser --idle-timeout 1h --connect
dbgjs connection configure --connection browser --idle-timeout inf
dbgjs connection configure --connection browser --idle-timeout inherit
```

Durations are positive integers with `ms`, `s`, `m`, `h`, or `d` units.
Timeout policies are persisted and can change while connected. A policy change
uses the connection's existing last-use time; reconnecting starts a new clock.
RPC clients can use `set_context_idle_timeout` and `set_connection_idle_timeout`;
`null` clears a connection override. Snapshots expose the context default and
each connection's override and effective timeout.

Activity is tracked independently per connection. Live target commands reset
only their connection's clock. Context-wide source and breakpoint operations
keep the connections they address active. Status queries, target/context
observation subscriptions, incoming CDP events, and offline stored-capture
analysis do not reset live connection clocks.

In-flight operations, paused targets, coverage/CPU recordings, and open
relay/Playwright proxy sessions defer automatic disconnect. The idle clock
restarts when an operation finishes; other blockers are checked once per
second and restart the clock while present. Timeout checks run once per second,
so short durations are not precise scheduling deadlines.

Expiry uses the ordinary disconnect behavior for every connection type:
externally owned applications are detached, and providers launched by dbgjs
are stopped. The context, connection configuration, breakpoints, and saved
captures remain. Reconnection is explicit. Cache eviction and automatic
service-process shutdown are separate future work.
