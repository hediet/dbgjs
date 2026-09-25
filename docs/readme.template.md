# dbgjs

A command-line JavaScript debugger for **VS Code, Electron, browsers, and
Node.js**. Explore a running application, set source-mapped breakpoints, automate
its UI, and inspect its coverage, CPU profiles, and heap without leaving the
terminal.

```sh
npm install --global @hediet/dbgjs@next
```

The examples below are **recorded from real CLI runs**, not handwritten output.
They use desktop VS Code {{vscode-version}}, a local website, and a running Node
process. Process IDs, URLs, paths, and measurements are from that run, so use
your own when following along. Commands use PowerShell quoting. These are independent
feature examples; the [walkthroughs](#walkthroughs-and-documentation) include
their setup and full execution order.

## Discover and attach to VS Code / Electron

Find windows, renderers, extension hosts, and agent hosts in the process tree.
Select the renderer you want to investigate, rather than guessing a debug port.

{{example:discover}}

Create a context and attach to the discovered renderer. `--set` selects it for
subsequent commands.

{{example:context,attach}}

## Open a website with Playwright or installed Chrome

Use Playwright's bundled browser, or point dbgjs at an installed Chrome
executable. Both expose the same debugger commands.

### Playwright Chromium

{{example:web-playwright-connect}}

### Installed Chrome

Launch an installed Chrome executable in a separate context:

{{example:web-chrome-connect}}

Both examples open the local demo website. See the [website walkthrough](docs/walkthroughs/website.md) for
the full launch, interaction, and disconnect sequence.

## Attach to a running Node.js process

Attach to an existing process and inspect its live application state.
You do not have to launch the application through dbgjs. This example's process
was started with Node's Inspector enabled (`--inspect=127.0.0.1:0`).

{{example:node-attach}}

No second target attachment is needed. The [Express walkthrough](docs/walkthroughs/node.md)
uses HTTP requests to collect coverage and pause inside a request handler.

## Evaluate JavaScript

Evaluate in the selected runtime, or in the current stack frame when paused.

{{example:evaluate}}

## Automate keyboard and mouse input with Playwright

Run locator, keyboard, and mouse operations against the **selected page**,
including Electron renderers. Here, real keyboard input opens a new VS Code
editor; the locator waits until it is ready.

{{example:playwright}}

## Search authored sources through source maps

Search original TypeScript inside a bundled application without a local source
checkout.

{{example:source}}

`source grep` limits displayed output to 8192 UTF-8 bytes and individual
source excerpts to 256 bytes by default. Excerpts of minified lines are
centered on the match; line/column and match length remain absolute in
`--json`, alongside `textTruncated`, `excerptStartColumn`,
`contextTruncated`, `outputTruncated`, and `outputOmittedMatches` (independent
of `omittedMatches` from `--max-results`). Use
`source show <path> --line <line>` to inspect the complete source.
`--max-results` separately controls
match count; `--max-output-bytes` and `--max-line-bytes` adjust display
limits. Skipped map/source diagnostics have unknown relevance to a path-filtered
search, so a summary warns about incompleteness; use `--verbose-diagnostics`
and a larger output budget for details. Searches time out after 30 seconds by
default (override with `--timeout-ms`).
Breakpoint failures similarly summarize ordinary nonmatching scripts and bound
human-readable diagnostics. Use `--json` for complete breakpoint assessments
or `source explain <path>` to inspect source candidates.

## Source-mapped breakpoints and live inspection

Target logpoints are scoped to the selected **attached target**, not to
persistent context breakpoints. Use an authored URL returned by `source list`
or `source show`, a `?formatted` projection, or the original runtime script
URL with one-based *raw* coordinates. A formatted view does not change raw
runtime coordinates. `target logpoint` accepts configuration by default, even
if its JSON `breakpoints[].status.kind` is `waitingForScript`,
`sourceNotFound`, `ambiguousSource`, `unmapped`, or `failed`; only `installed`
confirms a live binding. Add `--require-installed [--timeout-ms 30000]` to
require a binding before returning successfully (maximum 300000 ms). A
not-yet-loaded script can become installed while the command waits; unresolved
or ambiguous mapping states are retried until the deadline. Configuration
remains after a timed-out attempt.

```sh
dbgjs target logpoint proof 'file:///app.js' 12 3 '({value})' --require-installed
dbgjs log --after 0 --json
dbgjs target logpoint delete proof
```

`target logpoint delete` reports whether the ID existed and the number of live
bindings removed; `breakpoint delete log:proof` does **not** remove a
target-local logpoint. Context-owned breakpoints named `log:<id>` are not
target logpoints and cannot be replaced or removed by target logpoint commands.
Logpoints use a debugger-installed `Runtime.addBinding`
instead of application `console.log`. `log` reports successes, expression and
serialization errors (including cycles and BigInt), bounded retention and
eviction counts. `capture.logpoints[]` distinguishes hits from successful
evaluations, errors and recorded events for active target logpoints; deleting
one retires its counters without removing already captured log messages.
Installing a probe fails explicitly
if its CDP binding transport is unavailable; the binding is scoped to the
attached debugging session. Detaching removes the live instrumentation.

Set a breakpoint in authored TypeScript. dbgjs resolves it to the generated
script and shows the actual source location.

{{example:breakpoint-set}}

When the edit pauses, evaluate on the live text model:

{{example:breakpoint-eval}}

The [VS Code walkthrough](docs/walkthroughs/vscode.md) includes triggering the
breakpoint, waiting for the pause, and resuming.

## Precise code coverage

Record function and block execution, then exclude a background capture when
viewing the source-mapped tree. The stored capture stays intact; `--exclude` is
a query option, not a recording option. `HL` means hit lines; `RL` means run lines.
Capture persists cumulative raw ranges and cheap script provenance; mapping,
exclusion and path filtering happen when viewing. When original generated source
or its map is unavailable or has changed, the raw ranges remain available with
explicit projection diagnostics rather than an empty authored result.
At view time, verified local files, cached maps, and bounded HTTP(S) source/map
requests can reconstruct authored locations without attaching to the target.
An omitted inline map can be recovered from the `sourceMappingURL` directive
when the generated source is available and matches its captured SHA-256.
Inline `data:` and oversized script/map URLs are omitted from capture
provenance so URLs cannot smuggle source or map bytes into raw payloads.
The legacy `raw` take option now produces the same raw capture in either mode;
`noCache` applies only to live source acquisition, not stored views. Stored
views reuse the verified source-map cache when available and may populate it
after fetching a map; capture payloads never contain source or map bytes.

{{example:coverage-start,coverage-baseline,coverage-type,coverage-stop,coverage-show}}

## CPU profiling

Record a V8 sampling profile, then inspect the hottest functions with authored
locations. Filter to editor code instead of unrelated runtime activity.
These are real sample measurements, not a fixed ranking.
Raw nodes, sample IDs and signed time deltas are stored without capture-time
aggregation. Views rebuild function groups from the available source files and
maps, or report why authored mapping is unavailable.

{{example:profile-start,profile-type,profile-stop,profile-show}}

## Heap snapshots and instances

Capture the heap and find classes by authored name, even in minified code.
Instance IDs let you inspect objects and follow references within that capture.
Heap capture stores the original snapshot and lightweight script identity (URL,
hash, map URL and execution provenance), not copies of source files or maps.
Authored names are resolved when viewing if a map is available; otherwise the
generated name remains visible with an unavailable-map diagnostic. You can
explicitly supply a matching map for an existing stored capture.

{{example:heap-capture,heap-classes}}

Follow incoming references to one returned instance using its capture-qualified
reference, `editor#<instance-id>`.

{{example:heap-refs}}

### Run JavaScript on an object found in the heap

Use the instance ID from the snapshot to obtain a live object handle, then run
JavaScript with that object as `this`. Here we call the discovered text buffer's
methods to read the text typed earlier, without needing a global variable that
points to it.

{{example:heap-object,heap-eval}}

These are raw CDP calls through dbgjs. The first `objectId` is the heap instance
ID; the second is the remote handle returned by that call. This requires the
original target to remain connected and the object to still be alive; an
offline snapshot alone cannot execute JavaScript.

## Screenshots

Capture the selected page as a PNG.

{{example:screenshot}}

## Raw CDP

Use any Chrome DevTools Protocol method through the same managed target session.

{{example:raw-cdp}}

Native child sessions share bounded routing for raw CDP and virtual-browser
clients. Notification overflow or exhausted request bookkeeping fails the
affected session explicitly instead of silently losing lifecycle events.
Healthy siblings and the parent remain usable.

To keep late replies from reaching a reused session ID, each endpoint's
multiplexer retains up to 4,096 native routes. Each route permits up to 256
unresolved request correlations and bounds its notification queues. Exhausted
routes require reconnecting the owning endpoint; retrying the same failed
session ID does not reset these limits.

## Durable contexts and offline evidence

Disconnect without losing the investigation. Named captures remain queryable
after the live runtime has gone.

{{example:disconnect,offline-coverage}}

## Walkthroughs and documentation

- [Investigate typing in desktop VS Code](docs/walkthroughs/vscode.md)
- [Launch and automate a website](docs/walkthroughs/website.md)
- [Investigate an Express server with curl](docs/walkthroughs/node.md)
- [CLI guide and command model](docs/cli-design.md)
- [Architecture](docs/dbgjs-architecture-presentation.md)

## These examples are executable

The README and walkthroughs are rendered from the
[same CLI recording](tests/readme/recording.json). CI replays the flows in
isolated runtimes: stable results must match, while variable measurements must
still supply the promised evidence. No mocks or handwritten terminal output.

```sh
cargo build --locked --bin dbgjs --bin dbgjs-service
npm run generate:readme
npm run test:readme
```

See [generation and replay rules](docs/readme-generation.md).
