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

## Source-mapped breakpoints and live inspection

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

{{example:coverage-start,coverage-baseline,coverage-type,coverage-stop,coverage-show}}

## CPU profiling

Record a V8 sampling profile, then inspect the hottest functions with authored
locations. Filter to editor code instead of unrelated runtime activity.
These are real sample measurements, not a fixed ranking.

{{example:profile-start,profile-type,profile-stop,profile-show}}

## Heap snapshots and instances

Capture the heap and find classes by authored name, even in minified code.
Instance IDs let you inspect objects and follow references within that capture.

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
