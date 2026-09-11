# dbgjs-tui

`dbgjs-tui` is a leaf terminal client for the `dbgjs` debugger service. It
uses only the public service API and never opens CDP connections itself.

Build the service before running the TUI so that the sibling `dbgjs-service`
executable uses the same service API:

```console
cargo build --bin dbgjs-service
cargo run -p dbgjs-tui --bin dbgjs-tui
```

Select a context explicitly when the current directory does not identify one:

```console
cargo run -p dbgjs-tui --bin dbgjs-tui -- --context :my-context
```

## Keys

- `Tab` / `Shift+Tab`, or `1` / `2`: switch between the Runtime and Debug workspaces.
- `j` / `k`, or arrow keys: move through section headers and visible tree rows.
- `h` / left: collapse the parent and move to it.
- `l` / right: expand the selected node and enter its last selected child, or its first child.
- `Space`: add/remove the selected process or window as a configured connection; elsewhere,
  collapse or expand the selected section or tree row.
- `Enter`: activate a selected context, inspect an item, or open a selected source.
- `c`: add and connect the selected process/window, or connect/disconnect its configured
  connection.
- `a`: attach/detach the selected target.
- `f`: force attachment of an externally attached target.
- `d` / `Delete`: remove the selected inactive connection.
- `m`: cycle source maps, formatted code, and no projection in Sources.
- `b`: toggle a breakpoint at the selected line while the source document has focus.
- `Escape`: return from the source document to the sidebar; otherwise quit.
- `[` / `]`: switch context.
- `r`: query the debugger service again for expanded demand-driven sections.
- `q`: quit.

## Workspaces

The Runtime workspace contains stacked Contexts, Processes, Connections, and
Targets sections. Contexts starts expanded on the context selected by an
explicit `--context`, the nearest CLI cwd binding, or path inference. Targets
separates available resources from the debugger-owned attached subset. The
Debug workspace contains Attention, Sources, Breakpoints, Captures, and Call
Stacks. Section headers stay visible while each expanded tree scrolls
independently. Selection uses only a background highlight so tree chevrons are
unambiguous.

The Runtime workspace communicates one causal model:

1. **Contexts** selects the durable debugger context that owns connection,
   breakpoint, and capture intent.
2. **Processes** is a process-oriented projection of live resources. VS Code
   windows group their processes, and Copilot processes expose their observed
   agent sessions. Expanding an attachable process asks the service to discover
   only that process tree's runtime children. `[ ]`, `[+]`, and `[+] ●` show
   unconfigured, configured, and connected access paths. `Space` changes durable
   configuration; `c` changes live connectivity.
3. **Connections** has disjoint `Available` and `Configured` roots. Available
   rows are live access paths. Pressing `c` persists one as context intent and
   connects it. Configured rows show that durable recipe and its independent
   live lifecycle.
4. **Targets** has disjoint `Available` and `Attached` roots. Available targets
   are resources in configured connection scopes that this debugger does not
   own. Attached targets are the resources reached by debugger-session
   relationships.

Configured connection rows use `[+] ○`, `[+] ◐`, `[+] ●`, and `[+] !` for
disconnected, transitioning, connected, and failed state. Connections expose
their scoped resource hierarchy as a collapsible tree. A renderer connection
can use the main process tree as its access path while exposing only the
renderer and its descendants as its visible scope; sibling renderers are not
shown. Targets can be attached or detached directly, with explicit force
attachment for targets owned by another debugger.

The TUI queries the process projection when Processes or Connections needs it,
and queries runtime children only for expanded process roots. Independent root
queries run concurrently, and every TUI data/source request has a 15-second
deadline, except source trees and source content, which allow 30 seconds for
large source-map hydration. Process, VS Code window, agent-session, and
discovered-target nodes are contributed to the same context resource graph.
Connection resources, sources, and captures are likewise loaded on demand.
Selecting the source-map projection hydrates maps declared by loaded scripts;
opening or mapping one source hydrates only the matching script. Before the first query,
the header explicitly says `TUI: expand to query`; this describes TUI activity
and does not imply that the debugger or runtime has no sources. Sources default to the source-mapped
projection and are grouped into an expandable URI folder hierarchy. Changing
the projection policy keeps the same file/folder view; files without the
selected projection remain visible in their runtime-loaded form.

Selecting a source loads its contents into the document pane. Open the document
with `Enter`, navigate lines with the usual movement keys, and toggle
breakpoints with `b`. The selected context is observed continuously. Detailed
target state is observed for debugger-owned targets so every paused target can
appear as a collapsible root in Call Stacks. Attention is a derived view of
paused targets, failed connections and target debuggers, external ownership
conflicts, and partially bound or failed breakpoints; it does not introduce a
second state model.
