# Values, evaluation, and display

Value acquisition is separate from presentation. The shared `ValueDescription`
contains a kind, bounded summary/scalar, labeled children, optional identity and
reference, promise/evaluation state, completeness, and source evidence. Text and
JSON rendering are pure operations on that description. Live objects and heap
nodes use the same vocabulary; heap edges, retained sizes, and dominators remain
heap-specific.

## Evaluate once, inspect afterward

```sh
dbgjs target eval 'app.createEditor()' --retain
dbgjs value show @v...
dbgjs value children @v... --max-properties 10 --max-depth 1
dbgjs value children @v... --max-properties 10 --start 10 --max-depth 1
dbgjs value release @v...
```

`target eval` allows side effects. `-` reads one expression from stdin.
Ordinary results use ephemeral object groups, unless `--retain` is requested.
Promises and unfinished evaluations are retained automatically. Opaque `@v...`
references belong to one attached target incarnation, not to the CLI process.
They are not CDP object IDs and do not survive daemon restart or reconnect.
Execution-context destruction conservatively invalidates the target's owned
references. References acquired during a pause additionally expire on resume.
Running-context references do not expire just because execution pauses/resumes.

Expansion never reevaluates the expression. `children` publishes references for
the displayed object children. `nextStart` in `--describe`, or the human
`--start` hint, identifies the next page. Pages are fresh live observations, not
an atomic snapshot. Accessors are represented without invoking getters.

At most 1024 owned references are retained per target. Capacity exhaustion is an
explicit error; release references when finished. References in the same object
group share its lifetime until the last reference is released. Releasing a
reference does not cancel application work.

## Awaiting and continuation

```sh
dbgjs target eval 'app.save()'
# Promise (pending) [@v...]
# Still running; continue with: dbgjs value await @v... --timeout 2s
dbgjs value await @v... --timeout 10s

dbgjs target eval 'app.save()' --await --timeout 2s
# On timeout: a pending reference, not a cancellation or reevaluation.
```

`--timeout` bounds the wait (default 2s; accepts milliseconds, `ms`, or `s`).
Pending completion is a successful inspection outcome with a reusable reference.
Rejection exits nonzero and preserves a useful reason. `--json-expect` emits no
stdout on rejection or an incomplete export; ordinary `--json` can still emit
the rejection description before the nonzero exit.

The parser detects top-level `await`, excluding strings, comments, and nested
functions. Evaluation uses an async arrow only when necessary and boxes its
result so implicit promise adoption does not change explicit JavaScript awaits:

| Expression | Without `--await` |
| --- | --- |
| `foo()` | Return foo's value, possibly a promise |
| `foo(await bar())` | Await bar, call foo, return foo's value without awaiting it |
| `await foo(await bar())` | Await bar and foo, return the fulfilled value |
| `({p: foo()})` | Return an object containing a promise; no recursive awaiting |

While an expression is suspended, its reference is `@e...` (evaluation), not an
application promise that the expression has not produced yet. `value await`
continues that same evaluation. If `--await` was requested, the continuation also
awaits a promise returned by the expression. Otherwise it returns that promise
as a separate `@v...` value.

Promise waiting polls short inspections outside the target command loop, leaving
other debugger commands available between polls. Explicit awaiting attaches a
rejection handler; it is not entirely invisible to application rejection
reporting. Plain promise inspection does not install that handler. Only native
promises with available runtime settlement evidence are supported by
`value await`; arbitrary thenables are not silently executed.

Top-level await while paused is rejected before execution. Waiting on an already
settled promise while paused is possible; waiting on a pending one is rejected.
Nothing implicitly resumes the target.

The initial evaluation has a separate 1s synchronous execution deadline.
That is not a cancellation guarantee for asynchronous code resumed later.
Transport errors can leave execution outcome unknown: no automatic retry runs
the expression again.

## JSON modes and limits

```sh
dbgjs target eval '({count: 2, items: [true, null]})' --json
# {"count":2,"items":[true,null]}
dbgjs target eval 'Promise.resolve({count: 2})' --await --json-expect
# {"count":2}
dbgjs value show @v... --describe
# Structured debugger description, including references and source evidence.
```

`--json` is best-effort inspection. Within the configured limits, plain JSON
values have exactly their application JSON value, without a debugger envelope.
Unsupported values, cycles, accessors, promise state, and truncated data use
synthetic `$dbgjs` metadata. Ordinary application `$dbgjs` properties are
preserved. When mixed unsupported data would collide with a metadata-shaped
application object, its properties are represented as named entries instead.
Shared objects without cycles are duplicated in JSON, not mislabeled circular.

`--json-expect` is strict export: cycles, unsupported/lossy values, class
instances, holes, nonfinite numbers, and incomplete inspection fail instead of
being silently dropped or replaced. It does not call getters or `toJSON`.
This deliberately differs from the lossy conversions performed by
`JSON.stringify`. Neither JSON flag implicitly awaits a promise.

Defaults:

| Limit | Default | Override |
| --- | --- | --- |
| Object depth | 8 | `--max-depth` (0..64) |
| Properties per object | 100 | `--max-properties` |
| Visited values | 1000 | `--max-nodes` |
| String characters | 10000 | `--max-string-length` or `--max-preview-length` |
| CLI output bytes, including newline | 65536 | `--max-output-bytes` (minimum 128) |

`--full` removes the string limit, not the other limits. Inspection also has a
2s traversal budget; exhausted traversal produces incomplete data and strict
export fails. Individual owned-value protocol requests have a 2s deadline.
These bound traversal and rendered output, not the size of a raw CDP
`getProperties` response from the target.

If the output byte limit is exceeded, best-effort JSON returns a small explicit
limit marker, including the retained reference when available. Strict export and
`--describe` fail rather than truncate their output into invalid JSON.

## Heap evidence

`heap show <capture>#<id>` and heap reference tables retain their existing
capture-qualified identities. Heap nodes additionally expose a structured
`description`; their shallow text previews derive from the same description
vocabulary. Strings show up to 20 Unicode characters before escaping; uncertain
reconstructed strings remain marked. Objects show up to three data properties,
scanning at most 64 edges. Previews remain bounded to 512 characters.

Captured object descriptions are explicitly incomplete: V8 heap graphs do not
necessarily encode every JavaScript primitive/property. They are not exact
JSON exports. Cyclic objects are summarized, not recursively traversed.
Prototype links, internal edges, and accessors are not ordinary data properties.

Live heap hydration can add separately attributed source evidence. It does not
overwrite captured properties or turn an old pending promise into a current one.
Captured descriptions remain available after disconnect/restart when the CLI
has an explicit or saved target scope (`target attach --set`).

## Compatibility

`target eval --json` now emits application inspection JSON instead of the older
`ValueSnapshot` debugger envelope. Use `--describe` for the new structured
description. Existing `value <expression>`/`value --object-id` and legacy service
evaluation/inspection APIs retain their contracts. Their raw references are not
accepted as owned references by `value show` or `value await`.

The generated service contract includes the new value operation. VS Code/DAP
behavior is unchanged; adopting the descriptions in those clients is separate.
