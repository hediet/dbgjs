# Improve jsdbg heap and source debugging

Findings from debugging Markdown editor iframes in a running VS Code Insiders
window on 2026-09-07. Implementation references were inspected at commit
`b1628e6`; line numbers may move.

## High priority

### Make stored heap analysis source-map aware

- [ ] Persist script URLs, hashes, source-map metadata/content, connection
  generation, and execution-context/frame provenance with heap captures.
- [ ] Reuse the mapping-aware projection for stored captures rather than always
  using the unmapped analyzer.
- [ ] Implement `heap classes --no-cache` or reject the unsupported option;
  currently the CLI discards it.
- [ ] Report mapping availability explicitly: not attempted, no map supplied,
  map loading failed, or mapped. Do not make a hardcoded zero hydration duration
  look like a completed mapping attempt.
- [ ] Allow matching source maps to be supplied separately for production
  bundles that do not ship them.

**Observed:** `heap classes` showed minified constructors grouped under
`script:200` and `script:325`, with `source-map hydration: 0.000s`. The CLI calls
the stored-capture analyzer, which does not attempt mapping. The inspected
Insiders Markdown editor distribution also had no `sourceMappingURL` directives
in its 107 JavaScript files and no adjacent `.map` files. These are independent
limitations; there was no evidence of a source-map CDN fetch failure.

**Implementation:** [CLI dispatch](../src/bin/jsdbg.rs) around lines 689-711,
[stored service path](../src/debugger_service.rs) around lines 3339-3373,
[unmapped and live analyzers](../src/target_debugger.rs) around lines 2934-3043,
and [capture persistence](../src/debugger_service.rs) around lines 4650-4657.

**Verification:** Capture a minified, source-mapped fixture, disconnect from the
target, and resolve constructor locations from the stored capture. Repeat
without maps and with invalid maps, checking explicit diagnostics. Verify that
`--no-cache` has the documented effect.

### Hydrate loaded sources before searching

- [ ] Fetch content for matching metadata-only scripts before `source grep`.
- [ ] Report why a source was skipped, including fetch errors, instead of only
  reporting a skipped count.
- [ ] Keep `source list` and source-search behavior consistent for a debugger
  attached after scripts have already loaded.

**Observed:** `source list` showed the live Markdown editor bundle as
`runtime:loaded`, but `source grep` reported `0 source(s) searched, 1 skipped`.
Loaded-source metadata can exist without content, and grep does not request it.

**Implementation:** [metadata-only sources](../src/source_effects.rs) around
lines 346-403, [grep dispatch](../src/target_debugger.rs) around lines 1413-1425,
and [skipping missing content](../src/source_effects.rs) around lines 742-769.

**Verification:** Attach to an already-running page containing an external
script, list it, and grep it without any preceding source-show/tree command.

### Resolve the original script before requesting a formatted view

- [ ] Normalize the formatted-view suffix before source hydration.
- [ ] Load the original script and then generate its formatted representation.
- [ ] Support formatting scripts without source maps; pretty-printing must not
  depend on original TypeScript mappings.

**Observed:** After `source formatting set on`, `source show <exact-url>
--view formatted` reported that the formatted source did not exist. The service
appends `?formatted` before hydration, preventing an exact script match; the
fallback only considers map-bearing scripts.

**Implementation:** [formatted lookup](../src/debugger_service.rs) around lines
2674-2684 and [source hydration matching](../src/target_debugger.rs) around
lines 3614-3662.

**Verification:** Attach to an already-running page with an unmapped minified
script, immediately request its formatted view, and search that view.

## Medium priority

### Make displayed target selectors round-trip

- [ ] Accept the connection-qualified and relative selectors printed by human
  target listings, or print an explicit canonical selector accepted by commands.
- [ ] Add copyable selectors to ambiguous process-attachment errors.
- [ ] Test selectors for nested Electron iframe targets, not just top-level
  pages.

**Observed:** Human output displayed
`process-tree-119164/renderer-11/target/<id>`, but that value was rejected by
`--target`. JSON output exposed the accepted canonical `targetId`:
`renderer-11/target/<id>`, without the connection prefix.

**Implementation:** [target display](../src/bin/jsdbg/output.rs) around lines
1749-1768 and [selector resolution](../src/debugger_service.rs) around lines
6034-6060.

**Verification:** Feed every displayed selector back into `target show`, `target
attach`, and `target eval`, including duplicate target names across connections.

### Provide configurable or full evaluation output

- [ ] Add `--full`, `--max-preview-length`, or a raw-output mode to `target eval`.
- [ ] Keep machine-readable output lossless when full output is requested.
- [ ] Make truncation and the command for retrieving the full value obvious.

**Observed:** `target eval` uses a fixed 120-character preview. Wrapping a DOM
inspection in `JSON.stringify(...)` does not bypass the limit, requiring repeated
small evaluations to inspect iframe attributes and script URLs.

**Implementation:** [evaluation options](../src/bin/jsdbg.rs) around lines
265-295 and [preview limit](../src/promise_debugging.rs) around line 7.

**Verification:** Evaluate a long string and a JSON-serialized object; confirm
default previews are marked as truncated and full mode returns the entire value.

### Distinguish unmatched sources from an empty source inventory

- [ ] Explain that `source resolve` requires an exact URI, or support substring
  resolution with explicit ambiguity handling.
- [ ] Suggest canonical matches when an abbreviated name does not resolve.
- [ ] Reserve "No sources are currently observed" for a genuinely empty
  inventory.

**Observed:** `source resolve editor.js` reported no observed sources while
`source list --path editor.js` still listed the loaded bundle. Resolve uses an
exact URI, unlike the substring filtering used by list and grep.

**Implementation:** [source resolution](../src/context_source_model.rs) around
lines 305-310.

**Verification:** With one and then multiple loaded `editor.js` URLs, exercise
abbreviated names, exact URLs, no matches, and a genuinely empty target.

## Additional investigation: frame provenance

- [ ] Preserve execution-context metadata from script events.
- [ ] Investigate using the script's owning frame rather than the cached root
  frame when loading source maps through CDP.
- [ ] Label heap constructor groups with their frame/execution context when
  several webviews share a renderer.

The heap contained objects from two Markdown webviews. Grouping by script ID was
useful, but frame labels would have made it easier to select the intended
instances. Script-event conversion drops execution-context metadata, and CDP
map loading uses the cached root frame. These are investigation leads, not
demonstrated causes of the missing mappings in this incident.

**Implementation:** [script events and map loading](../src/cdp_runtime.rs)
around lines 902-908 and 744-767.

**Verification:** Use two same-process frames with separately mapped scripts and
confirm that captured objects and source-map requests retain the correct frame
provenance.

## What worked well

Keep the fast attach-and-inspect workflow: a roughly 28 MiB heap capture took
about 0.6 seconds. Even without source maps, `heap classes` and `heap show`
exposed the decisive object relationship:

1. The iframe factory options contained the correct bootstrap URL.
2. The physical iframe pool also stored the URL.
3. The physical iframe object had no bootstrap URL property.

Following that missing property across the constructor boundary identified the
bug. The Markdown editor fix is tracked separately in
[vscode-packages#289](https://github.com/microsoft/vscode-packages/pull/289).
