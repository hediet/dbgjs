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
