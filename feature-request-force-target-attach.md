# Feature request: force target attachment

## Problem

Electron permits only one debugger client per `webContents`. If a jsdbg service exits or is replaced
while it owns `webContents.debugger`, a subsequent service can discover the renderer but attachment
may fail with:

```text
Electron webContents <id> already has a jsdbg client
```

Recreating a context or connection does not resolve this because ownership remains in the live
Electron process. Reloading the renderer clears the attachment, but it also disrupts the state being
investigated.

## Proposal

Add an explicit force flag:

```text
jsdbg target attach --force [target scope]
```

When normal attachment fails because an Electron `webContents` already has a debugger client,
`--force` should:

1. Resolve the target through the normal context, connection, and target selectors.
2. Ask the Electron process-tree provider to detach the debugger currently attached to that
   `webContents`.
3. Attach the requesting jsdbg target debugger.
4. Return whether it reused an existing session, detached an existing client, or attached normally.

The operation should also handle a stale attachment left by a previous jsdbg-service process. If the
current service already owns a compatible debugger session, it should reuse it rather than detach and
reattach.

## Safety

This is intentionally destructive to the existing debugger client and must never be implicit:

- Require `--force` for the detach-and-steal behavior.
- Limit it to Electron targets discovered through the process-tree provider.
- Print which target and `webContents` were detached.
- Return a clear error if the provider cannot prove that it is acting on the selected target.
- Preserve normal attachment behavior when `--force` is absent.

## Acceptance criteria

- A renderer reporting `already has a jsdbg client` can be attached without reloading its window.
- Raw CDP requests work immediately after forced attachment.
- An existing compatible session owned by the current daemon is reused.
- Without `--force`, the existing ownership error and behavior remain unchanged.
- CLI and service tests cover normal attachment, compatible-session reuse, stale-client replacement,
  and unsupported target types.
