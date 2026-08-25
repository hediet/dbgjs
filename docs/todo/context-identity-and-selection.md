# Refactoring: path-based context identity and cwd-local selection

Status: agreed design direction, not yet implemented. Exact CLI spelling and
the persisted registry schema may still change.

## Motivation

The current model gives contexts short daemon-local names, keeps every context
in one daemon, and stores one machine-global CLI selection. This makes a command
run in one repository affect commands run in another repository, and it makes
the common project-local workflow depend on an explicit selection even when the
project directory itself is the natural context identity.

The refactoring should make context identity deterministic from user input,
make implicit selection local to a cwd, and keep all known contexts globally
discoverable.

The common workflow should require no explicit selection:

```text
cd D:\src\shop
jsdbg context create .
jsdbg status
```

The second command resolves the context whose identity is `d:\src\shop`.

## Capabilities

The design must support:

- Creating a context from a relative or absolute path without requiring that
  the path exists.
- Creating a non-path context through explicit `:<id>` syntax.
- Selecting a context explicitly for one command.
- Overriding implicit selection for one cwd with `--set`.
- Inheriting cwd selections and path contexts from parent directories.
- Using the current cwd as an implicit context when that context exists.
- Listing every known context, ordered by distance from the current cwd.
- Sharing a folder context between a terminal and a single-folder VS Code
  workspace.
- Keeping one daemon for all contexts for now, without making context identity
  depend on that deployment choice.

## Concepts

### Context expression

A context expression is the string accepted from the CLI or another client. It
has one of two forms:

```text
<path>
:<id>
```

The expression is resolved before calling the daemon. The daemon and registry
operate on the resulting context identity.

### Path context

An expression that does not begin with `:` denotes a path context.

- A relative expression is combined with the command's current cwd.
- An absolute expression is used directly.
- `.` and `..` components are resolved lexically so `context create .` denotes
  the current cwd.
- The resulting absolute path is lowercased.
- The path does not need to exist.
- Resolution does not read the filesystem.
- Resolution does not resolve symlinks or use file identity.
- The path does not designate a context config file or daemon socket.

For example, from `D:\src\shop`:

```text
.                         -> d:\src\shop
frontend                  -> d:\src\shop\frontend
D:\Contexts\Incident      -> d:\contexts\incident
```

The normalized path string is the context identity. Lowercasing is intentional
even on a case-sensitive filesystem; differently cased expressions therefore
name the same context.

### Named context

An expression beginning with `:` denotes a named, non-path context:

```text
:incident-42 -> incident-42
```

The leading colon is expression syntax and is not part of the stored identity.
Named contexts bypass cwd combination and path normalization. Their IDs should
still be validated and normalized according to one documented ID grammar.

### Global context registry

The registry contains every context known to this installation, including both
path and named contexts. It is the source used for global context listing and
implicit path-context lookup.

At minimum, each entry must retain:

- the context identity;
- whether it is a path or named context;
- its display name and durable context state already owned by the service;
- any metadata needed for stable display and migration.

The existing persisted daemon context map can remain the authoritative registry.
A second catalog should not be introduced unless it serves an independent need.

### Cwd selection

A cwd selection is an explicit override created by `--set`:

```text
normalized cwd -> context identity
```

Writing a selection only changes the entry for the command's exact cwd. It does
not change a global current context.

Selection lookup considers the cwd and then each parent directory. The nearest
entry wins.

Other interactive client state that is currently stored beside the context
selection, such as target selection, watches, and log cursors, must not remain
one machine-global value. It should be scoped by the cwd and selected context so
activity in one project does not alter another project's view state.

### Daemon

There remains one global daemon containing all contexts:

```text
daemon
  context A
  context B
  context C
```

No per-context endpoint or `--endpoint` option is needed. The current service
API may retain explicit context parameters and the service may retain its
context map.

Context identity must not encode or depend on the daemon endpoint. This leaves
open a later change that assigns contexts to multiple daemons without changing
context expressions or stored identities.

## Primitive operations

The design can be expressed with these independent operations:

1. **Resolve expression**: convert a path or `:<id>` expression into a context
   identity.
2. **Register context**: create or update durable context state under that
   identity.
3. **Bind cwd**: associate the exact current cwd with a context identity.
4. **Resolve implicit context**: apply cwd-binding and path-context lookup.
5. **List contexts**: query the global registry and rank entries relative to a
   cwd.
6. **Forget or delete context**: remove registry/state deliberately, separately
   from selection lookup.

`context create . --set` is a convenience composition:

```text
resolve "." + register context + bind current cwd
```

It must not introduce a second identity or selection mechanism.

## Context resolution

When a command needs a context, it uses this precedence:

1. An explicit context expression supplied to the command.
2. The nearest explicit cwd selection created by `--set`.
3. The nearest registered path context matching the cwd or one of its parents.
4. A context-required error.

Conceptually:

```text
resolve_context(explicit, cwd):
    if explicit is present:
        return resolve_expression(explicit, cwd)

    for directory in cwd_and_parents(cwd):
        if a cwd selection exists for directory:
            return its context identity

    for directory in cwd_and_parents(cwd):
        if a path context exists whose identity is directory:
            return that context identity

    fail with context_required
```

The two parent traversals are intentionally separate. Any applicable `--set`
binding has precedence over automatic path matching, including an inherited
binding on a parent directory.

For example, given:

```text
cwd selection:
  d:\src\shop -> incident-42

path contexts:
  d:\src\shop
  d:\src\shop\packages\ui
```

a command in `d:\src\shop\packages\ui` selects `incident-42` unless a nearer cwd
selection overrides the one at `d:\src\shop`. The exact path context does not
override an inherited explicit selection.

If there is no cwd selection, the same command selects
`d:\src\shop\packages\ui`. A command in another descendant of `d:\src\shop`
selects the parent context `d:\src\shop`.

An unrelated context must never be selected merely because it is the only
context in the registry.

### Stale selections

If the nearest cwd selection refers to a context that no longer exists,
resolution should return an explicit stale-selection error. It must not silently
fall through to a path context, because that could route a command to a
different debugger context than the user selected.

The diagnostic should identify:

- the cwd binding that was used;
- the missing context identity;
- how to replace or remove that binding.

## Context listing

`context list` queries all registered contexts and sorts path contexts by their
distance from the current cwd.

For two absolute paths, distance is:

```text
segments from cwd up to the common ancestor
+ segments from the common ancestor down to the context path
```

This makes the exact cwd, ancestors, descendants, and nearby sibling contexts
naturally appear near the top.

Ordering must be deterministic:

1. Path contexts before named contexts.
2. Lower path distance first.
3. An ancestor context before a non-ancestor when distance is equal.
4. Normalized context identity as the final tie-breaker.

Named contexts have no path distance. They appear after path contexts and are
sorted by normalized ID.

Human and structured output should expose enough information to explain the
order, including context kind and path distance where applicable.

## VS Code behavior

When exactly one folder is open, the extension uses that folder path directly
as its context expression.

For:

```text
D:\src\shop
```

the extension uses:

```text
d:\src\shop
```

There is no `vscode-context` suffix and no suffix setting in this refactoring.
A suffix setting may be considered later.

This deliberately makes VS Code share the context created by:

```text
cd D:\src\shop
jsdbg context create .
```

The extension should stop deriving a hashed daemon-local context name from the
workspace URI. Behavior for multi-root and untitled workspaces is deferred.

## Laws and edge cases

- Resolving the same path expression from the same cwd always yields the same
  context identity.
- Context path resolution is independent of whether the path exists.
- Creating or deleting files at the context path does not change its identity.
- An explicit context expression always wins over implicit resolution.
- The nearest cwd selection wins among cwd selections.
- Any cwd selection wins over every automatic path-context match.
- Without a cwd selection, the nearest path context at the cwd or a parent wins.
- Writing `--set` changes only the exact current cwd entry.
- Deleting a context does not silently retarget cwd selections that reference
  it.
- Listing contexts does not start, stop, or otherwise mutate a context.
- Renderer or daemon topology changes do not affect context identity.
- A path context and a named context remain distinguishable in registry data,
  even if their display strings could otherwise look similar.

## Refactoring outline

### 1. Introduce context expression resolution

- Add one shared resolver for path and `:<id>` expressions.
- Resolve expressions at client boundaries before RPC calls.
- Replace direct use of short context names in CLI code.
- Add tests for relative paths, absolute paths, `.`, `..`, lowercasing, missing
  paths, and named IDs.

### 2. Replace the global selection

- Change selection persistence from one `workspace` value to cwd-keyed entries.
- Normalize cwd keys with the same path rules used for path contexts.
- Implement nearest-parent binding lookup.
- Preserve target, watch, and log state under an appropriate cwd/context scope.
- Report stale bindings explicitly.

### 3. Add automatic path-context lookup

- When no explicit context or cwd binding applies, search cwd and parents in the
  registry.
- Keep `--set` lookup as a separate higher-precedence pass.
- Remove the fallback that selects the sole globally registered context.

### 4. Sort global context listing by distance

- Annotate path contexts with distance from the caller's cwd.
- Define deterministic ordering and structured output.
- Keep named contexts in the same global list after path contexts.

### 5. Align VS Code

- Replace the hashed workspace context ID with the single folder's path.
- Use the shared normalization semantics.
- Ensure the extension and terminal select the same context for the same folder.
- Leave multi-root and untitled workspace policy explicit and unresolved.

### 6. Update terminology and documentation

- Replace the old claim that cwd is only discovery metadata.
- Describe path identity, named identity, cwd bindings, and resolution
  precedence consistently.
- Keep the current one-daemon/many-contexts architecture documented.
- Avoid introducing endpoint selection or per-context daemon concepts.

## Deferred choices

- The exact on-disk schema and location for cwd selections.
- The validation and normalization grammar for `:<id>`.
- Cross-platform treatment of drive-relative Windows paths such as `C:foo`.
- UNC path normalization and separator normalization.
- Whether structured `context list` output is sorted by default or includes raw
  registry order as an option.
- Multi-root and untitled VS Code workspace context expressions.
- A possible future VS Code context suffix setting.
- A possible future mapping of contexts to multiple daemons.
