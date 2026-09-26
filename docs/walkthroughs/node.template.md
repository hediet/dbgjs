# Investigate an Express server with curl

Attach to an already running Node.js process, measure a real HTTP request, and
pause inside the handler for the next one. The commands and responses below
come from an actual run, including the PIDs and allocated URLs.
Commands use PowerShell quoting.

## The application

This small [Express server](../../tests/readme/express-server.mjs) calculates
a notebook quote. Ordering three notebooks exercises the discount branch;
ordering two does not. The code below is generated from the file used by replay.

{{file:tests/readme/express-server.mjs}}

With the repository dependencies installed, start it in another terminal:

```powershell
node --inspect=127.0.0.1:0 tests\readme\express-server.mjs
```

Use your server's PID and HTTP address in place of the recorded values below.
The application is not launched through dbgjs.

## Attach to the process

Create a context for this investigation, then attach and select the process in
one command. There is no separate connection setup or second target attachment.

{{example:node-context,node-attach}}

## Measure a request

Start precise coverage, send a request with curl, and save the capture.
Filter the view to the server's source file instead of Express internals.

{{example:node-coverage-start,node-coverage-request,node-coverage-stop,node-coverage-show}}

The response and coverage come from the same HTTP request. The saved capture
retains the complete data; the source filter only affects this view.

## Pause inside the next request

Set a breakpoint in the request handler.

{{example:node-breakpoint-set}}

In another terminal, start this request. It waits at the breakpoint; the
response shown here is what curl receives **after** the resume command below.

{{example:node-breakpoint-request}}

Back in the debugger terminal, wait for the pause and inspect the live request
state before allowing the handler to finish.

{{example:node-breakpoint-wait,node-breakpoint-eval,node-breakpoint-resume}}

The request now completes. Remove the breakpoint when finished.

{{example:node-breakpoint-delete}}

## Disconnect without stopping the server

{{example:node-disconnect}}

Replay checks that the server remains responsive after disconnecting. Only
the generator's final cleanup terminates the application it launched.

[Back to the feature overview](../../README.md) ·
[Generation and replay rules](../development/readme-generation.md)
