# jsdbg-tui

`jsdbg-tui` is a leaf terminal client for the `cdp-client` debugger service. It
uses only the public service API and never opens CDP connections itself.

Build the service before running the TUI so that the sibling `jsdbg-service`
executable uses the same service API:

```console
cargo build --bin jsdbg-service
cargo run -p cdp-tui --bin jsdbg-tui
```

Select a context explicitly when the current directory does not identify one:

```console
cargo run -p cdp-tui --bin jsdbg-tui -- --context :my-context
```

## Keys

- `Tab` / `Shift+Tab`, or `1` through `6`: switch tabs.
- `j` / `k`, or arrow keys: move through the outline.
- `h` / `l`, or left/right arrows: collapse or expand the selected node.
- `Space`: attach a process, connect/disconnect a connection, or attach/detach a target.
- `f`: force attachment of an externally attached target.
- `d` / `Delete`: remove the selected inactive connection.
- `m`: cycle source maps, formatted code, and no projection on the Sources tab.
- `[` / `]`: switch context.
- `r`: refresh the active demand-driven view.
- `q`: quit.

Processes, sources, and captures are loaded only while their tab is active.
Sources default to the source-mapped projection and are grouped into an
expandable URI folder hierarchy. Changing the projection policy keeps that
same file/folder view; files without the selected projection remain visible in
their loaded form.
The selected context is observed continuously. Detailed target state is
observed only for the selected debugger-owned target.
