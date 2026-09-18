# Attach to a running Node.js process

This example attaches to an already running Node application. The generator
starts a small local process with Node's Inspector enabled
(`--inspect=127.0.0.1:0`), discovers its PID, and only then attaches with dbgjs.
`$NODE_PID` is that process's real PID, replaced for readability.
Commands use PowerShell quoting.

To reproduce the setup, run the included
[Node application](../../tests/readme/node-target.mjs) in another terminal:

```powershell
node --inspect=127.0.0.1:0 tests\readme\node-target.mjs
```

Use that application's PID for `$NODE_PID` below.

## Select a context and attach

{{example:node-context,node-attach,node-target-attach}}

## Inspect the live application

The result comes from the application, not a debugger fixture response.

{{example:node-eval}}

## Disconnect without terminating the application

{{example:node-disconnect}}

The replay verifies that the Node process remains alive after disconnecting;
only the generator's final cleanup terminates the process it owns.

[Back to the feature overview](../../README.md) ·
[Generation and replay rules](../readme-generation.md)
