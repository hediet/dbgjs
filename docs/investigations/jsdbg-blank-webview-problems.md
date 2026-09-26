# jsdbg problems encountered during the blank-webview investigation

Date: 2026-09-10  
CLI: `C:\Users\hdieterichs\.cargo\bin\jsdbg.exe`  
Related: [investigation report](jsdbg-blank-webview-investigation.md)

These are observed behaviors, not independently reproduced or source-level
diagnoses of jsdbg bugs. No jsdbg code was changed. The CLI version/commit was
not captured, so this report does not establish whether current source already
fixes any of these problems.

## 1. Window attachment selected a renderer without live webContents

Discovery showed the requested window as `w:62364/1`, with renderer processes
34908 and 51268. Attaching the window returned:

```text
renderer process 34908 has no live Electron webContents
```

Attaching its primary renderer, `p:51268`, then returned:

```text
renderer process 51268 maps to multiple Electron webContents targets:
renderer-1, renderer-8; attach one with `jsdbg target attach --target <target-id>`
```

**Workaround:** `target list` distinguished the desired `renderer-1` by its
window title from `renderer-8`, a different `about:blank` page. Explicit
`target attach --target renderer-1` worked.

**Impact/opportunity:** a window reference did not reliably select that window's
live webContents. Resolve from window identity when possible; otherwise show
candidate titles/URLs directly in the attachment error. Refusing ambiguous PID
attachment was safer than choosing silently.

## 2. The printed connection-qualified nested selector did not resolve

`target list --type iframe` printed:

```text
process-tree-62364/renderer-1/target/813B560AF240615D60C61730F455BF30
```

Passing that string to `target cdp` and `log`, with the same context, returned
`target selector '...' does not exist`.

**Workaround:** omitting the connection prefix and explicitly attaching worked:

```text
renderer-1/target/813B560AF240615D60C61730F455BF30
```

**Impact/opportunity:** copy/pasting the displayed identifier failed, despite
documentation describing connection-qualified selectors as usable directly.
The nested target became observable during this session, so an inventory timing
issue cannot be excluded; the selector parser was not isolated as the cause.

## 3. Raw flattened-session routing hung, then reported channel reuse

I called `Target.attachToTarget` for the OOPIF with `flatten: true` through the
parent renderer. It returned a session ID. Next, I passed that ID to
`target cdp Runtime.evaluate --session-id ...`.

The command produced no output for over 120 seconds. While it was pending, a
separate `DOM.getDocument` call using the same session ID failed with:

```text
failed to open CDP session: multiplexed transport channel id
"4BD5C6D11469DC47F2589CC95CEAEA31" has already been used
```

**Workaround:** stopped the hung CLI invocation and used jsdbg's discovered
nested target instead of the raw session ID. That route returned DOM results
promptly. The manually attached CDP session was detached during cleanup.

**Impact/opportunity:** raw CDP session IDs and jsdbg's multiplexed channels were
not clear at the CLI boundary. If this invocation is unsupported, fail fast with
the supported routing pattern; if supported, investigate the hang/channel
ownership. The channel error occurred during overlapping calls and does not
prove sequential session reuse is broken.

## 4. Evaluation previews hid the useful result

`target eval 'JSON.stringify(...)'` returned an escaped, abbreviated string
ending in `...`, omitting the DOM facts needed for diagnosis.

**Workaround:** used `target cdp Runtime.evaluate` with `returnByValue: true`
and smaller result objects.

**Impact/opportunity:** bounded previews are useful interactively, but structured
diagnostics need an obvious full-value mode or a hint to the appropriate
`value`/CDP command. Larger CDP results were also truncated by the surrounding
agent tool; that second truncation was not a jsdbg defect.

## 5. Source search did not explain a skipped bundle

Searching for the resource-loading trace string in the live workbench bundle:

```text
jsdbg source grep 'Webview.loadLocalResource - trying'
  --path workbench.desktop.main --max-results 2 --context-lines 0 ...
```

returned:

```text
0 source(s) searched, 1 skipped
```

A prior `source list --path webview-pr-description` also returned no entries.
For the latter, the script's denied load makes absence unsurprising; it is not
proof that source discovery malfunctioned.

**Workaround:** read the installed workbench bundle from disk, found the string's
line/column and local variables, then set a raw CDP breakpoint by URL. CDP
resolved that location successfully.

**Impact/opportunity:** source search should expose why a source was skipped and
the next action needed. The invocation was not retried after explicit
`Debugger.enable`, so the role of lazy debugger/source acquisition remains
unknown.

## 6. Empty logs did not recover the original failure

`jsdbg log` returned no entries for the parent renderer or nested webview.
Enabling `Log` and reading again did not surface the original 401.

**Workaround:** read the inner frame's existing Resource Timing entries. Those
preserved the original script's 401, MIME type, and zero-byte response.

**Impact/opportunity:** empty output did not distinguish no recorded events,
historical events unavailable, or unsupported event categories. This was a late
attachment; no claim is made that jsdbg should recover every earlier browser
error. Clearer capture/history semantics would avoid treating empty logs as
evidence that nothing failed.

## Setup mistake, not a confirmed product defect

My first `process attach --context <new-path>` failed because the context did
not exist. Explicit `context create <path> <name>` fixed it. The error was
accurate; I had incorrectly assumed attachment would create the context.

## What worked well

- Screenshot capture preserved the original UI before diagnostic changes.
- Explicit webContents selection avoided attaching to the wrong page.
- Nested-target evaluation exposed both the webview wrapper and inner document.
- Raw CDP access provided structured values and a non-pausing authorization
  probe without requiring an application reload.
- Connection disconnection gave a verifiable cleanup state.

The largest avoidable costs were attachment/selector discovery and the raw
session-routing hang. The final diagnosis relied on actual live authorization
inputs rather than inferring the cause from extension-version timestamps.
