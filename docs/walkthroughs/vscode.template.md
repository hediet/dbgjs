# What happens when you type in VS Code?

This is a recorded investigation of **desktop VS Code {{vscode-version}}
(Electron)**. The generator launches a separate profile and empty workspace;
it never attaches to an existing user window. Commands use PowerShell quoting.
`$VARIABLES` stand for discovered identities and long path prefixes.

## Discover, select, and inspect

Find the isolated process tree. When following along manually, omit the filter
to discover your own windows, then select a renderer PID from that output.

{{example:discover}}

Create a durable context, attach to that renderer, and evaluate inside it.

{{example:context,attach,evaluate}}

## Open an editor with real keyboard input

Playwright clicks the workbench, presses Ctrl+N, and waits for the new editor.

{{example:playwright}}

## Find the implementation

Search VS Code's loaded authored sources for the edit implementation.
The source map provides the TypeScript; no VS Code checkout is needed.

{{example:source}}

The excerpt supplies the source path and first executable line used below.
Long diagnostics remain in the [full recording](../../tests/readme/recording.json).

## Measure the first edit

Start precise coverage and capture a baseline. Type into the editor, then
preserve only the coverage that remains after excluding the baseline.

{{example:coverage-start,coverage-baseline,coverage-type,coverage-stop,coverage-show}}

The hit tree points into the real text model, its PieceTree buffer, and related
editor machinery. The counts can vary between runs.

## Pause inside the next edit

Set a breakpoint at the statement discovered by the source search.

{{example:breakpoint-set}}

Type another character and wait for the resulting pause. The wait observes
pauses after epoch 0, so it also works if the typing command already hit the
breakpoint.

{{example:breakpoint-type,breakpoint-wait}}

Inspect the buffer while paused. It still contains the previous text because
the new edit has not yet been applied.

{{example:breakpoint-eval}}

Resume, then remove the breakpoint before profiling more input.

{{example:resume,breakpoint-delete}}

## Profile a burst of typing

Record a V8 sampling profile while Playwright sends keyboard input. Render the
hot functions with their authored locations, filtered to editor code.

{{example:profile-start,profile-type,profile-stop,profile-show}}

## Capture the screen and inspect through CDP

Save a real screenshot, then read the document title through a raw CDP call.

{{example:screenshot,raw-cdp}}

## Find the buffers in the heap

Capture the V8 heap and ask for live `PieceTreeTextBuffer` instances. Names are
resolved through source maps, rather than guessed from minified constructors.

{{example:heap-capture,heap-classes}}

Follow the incoming edges of a discovered instance to see who holds the buffer.
`$BUFFER` abbreviates `editor#<instance-id>` using the first ID printed above.

{{example:heap-refs}}

## Disconnect and retain the evidence

Disconnect the runtime. The coverage capture is still queryable and contains
the same data as before disconnecting.

{{example:disconnect,offline-coverage}}

[Back to the feature overview](../../README.md) ·
[How this walkthrough is generated and checked](../readme-generation.md)
