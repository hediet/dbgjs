# Executable README

The [README](../README.md) is a feature overview, with examples selected from
real CLI recordings. The separate [VS Code](./walkthroughs/vscode.md),
[website](./walkthroughs/website.md), and [Node](./walkthroughs/node.md)
walkthroughs show the complete sequences. They all use the same recording;
there is no separately maintained expected terminal output.

[The runner](../tests/readme/run.mjs) downloads VS Code 1.137.0, starts a
separate profile and workspace, and attaches only to its renderer. Additional
scenarios launch a local website through Playwright and installed Chrome, then
attach to an already running Node application.

## Regenerate or replay

On Windows, with the repository's npm dependencies, Playwright Chromium, and
installed Chrome available:

```powershell
cargo build --locked --bin dbgjs --bin dbgjs-service
npm run generate:readme
npm run test:readme
```

`generate:readme` executes every command, verifies the evidence, and only then
updates [recording.json](../tests/readme/recording.json), the README, and all
three walkthroughs. `test:readme` launches fresh runtimes and replays the same
flows without updating any of those files. A failed command or missing evidence
fails the run. It is a "does this walkthrough still work?" check, not a unit-test
replacement.

Use `--bin-dir` to select another built CLI/service pair:

```powershell
npm run test:readme -- --bin-dir target/release
```

`npm run test:readme:render` is the fast, platform-independent check that the
README and walkthroughs match their templates and recorded CLI output.
For prose-only edits, `npm run render:readme` renders the existing recording
without rerunning the applications or changing sample measurements.
Every displayed command references a unique recorded command ID. All recorded
commands must appear somewhere in the generated documentation; the README can
select a smaller set than the walkthroughs.

## What is and is not deterministic

- **Commands:** Recorded from the exact argument arrays passed to `dbgjs`.
  The replay must execute the same commands in the same order. PowerShell
  quoting is generated, including doubled single quotes inside arguments.
- **Identities:** The isolated process IDs, selected target ID, temporary
  directory, installation path, local website address, artifact directory, and authored-source URL
  prefix get named replacements. The same replacement is used in commands and
  outputs. Numbers elsewhere (including coverage counts) are never blanked out.
  `$VSCODE_SOURCE/` abbreviates the discovered CDN prefix ending in `src/vs/`.
- **Stable output:** Acknowledgements, known editor state, and evaluation
  results must match the recording exactly after those replacements. The
  displayed authored-source excerpt is also compared exactly, without requiring
  unrelated source-map diagnostics below that excerpt to stay identical.
- **Live output:** Process inventories, revisions, timing, coverage, profiles,
  heap measurements, and bounded tree membership can vary. Each command has an
  explicit evidence check in the runner. Further checks require executed
  authored coverage ranges, mapped CPU samples, a real PNG, actual heap buffer
  instances, and unchanged coverage after disconnecting.
- **Readability:** Prefer CLI bounds such as `--max-lines` and `--max-results`.
  A few long source/breakpoint excerpts use a recorded `maxOutputLines`; omitted
  lines are explicitly counted. Verbose heap source-map diagnostics are folded
  into a details section. Rendered lines omit trailing whitespace; the full
  output remains in the recording.
  Stderr is retained, not silently discarded.

The checked-in README is a sample from one successful run, not a claim that
timings or heap object IDs recur. Do not update it by typing plausible output
into the recording. Change the runner or prose template and regenerate.

## Isolation and diagnostics

The runner uses its own authenticated service state, VS Code profile, and
workspace. The window is selected by the PID of the process it launched, never
by the first existing user window. Cleanup stops its service and that specific
process tree, then removes its temporary directory.

`artifacts/readme/*-commands.jsonl` contain raw argument vectors, stdout, stderr,
exit statuses, and durations, including setup and verification queries.
`artifacts/readme/recording.json` holds the normalized transcript.
`artifacts/readme/editor.png` is the real screenshot.
`artifacts/readme/vscode.log` contains Electron diagnostics.
These are uploaded even when hosted CI fails.

VS Code and its source maps are downloaded from Microsoft's servers. The
walkthrough intentionally fails rather than silently substituting a mock,
skipping unavailable source maps, or publishing an incomplete investigation.
Source-map diagnostics about unrelated scripts remain in the recording.
