# Local investigation transcripts

Record an investigation explicitly; ordinary `dbgjs` invocations are **not recorded**
unless a recording has been started for the current service state. This is local
evidence, not an upload or a debugger event capture.

```sh
dbgjs record start --output ./private-investigation --max-entries 500 --max-bytes 33554432
dbgjs context create :investigation --set
dbgjs target eval '1 + 1'
dbgjs record stop
dbgjs record export --input ./private-investigation --format markdown --output investigation.md
```

The output directory must not exist yet. `record start` and `record stop` themselves
are not investigation entries. All commands between them, including unsuccessful
commands and scope-selection changes, are recorded across CLI processes and
contexts, except `daemon view` attached to a terminal: it stays interactive and
runs without recording, with an explicit warning. A noninteractive `daemon view`
is recorded normally. Recording never serializes independent debugger commands
behind a long-running one. `record stop` blocks new recordings but does not
cancel or wait for commands already running; wait for those commands to finish
before exporting a complete transcript.

`--max-entries` and `--max-bytes` bound started command slots and total
captured stdin/stdout/stderr; the defaults are 500 and 32 MiB. When the limit is
reached, dbgjs warns on stderr but the debugger command's output and exit status
remain unchanged. Commands beyond the entry limit run without recording.
An interrupted command can consume a slot without producing an entry; completed
commands are exported in their original invocation order.

The manifest contains each invocation's argument vector, CLI start/end times,
duration, exit code, build identity, and resolved context/connection/target and
generation when available. It also describes whether stdout is JSON and whether
recorded streams are incomplete. `-` scripts read from stdin retain real newlines.
The Markdown document links to local binary attachments containing the complete
*retained* stream bytes and embeds a clearly marked 4096-byte excerpt. A bounded
record cannot reconstruct any bytes discarded at the recording limit. CLI exit
does not prove a target promise, background task, or persistent runtime edit has
completed.

Treat the recording directory as sensitive. To avoid retaining selected data, pass
`--redact LITERAL` (repeatable), `--omit-args`, `--omit-stdin`, or `--omit-output`
to `record start`. Redaction applies to saved arguments, stdin, stdout, stderr,
and exported attachments; the literal itself is stored in the private manifest
for later exports. Authentication/cookie/API-key headers and common credential
flags are masked automatically. These controls do not change what the original
command prints to your terminal. No automatic publication occurs. Review the
Markdown and its attachments before sharing: arbitrary secrets outside these
patterns cannot be identified reliably. Export requires an explicit destination
and creates a new `.attachments` directory beside it; choose a location that
does not already exist.
