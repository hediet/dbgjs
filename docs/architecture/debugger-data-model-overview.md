# Debugger Data Model: Short Overview

This is the high-level companion to the
[full debugger data model](./debugger-data-model.md).
It illustrates the intended model; watches, target-selection policies, and
observation history are not all implemented as durable context features.
See [current context identity and selection](./context-identity.md) for the
CLI's actual cwd/path behavior.

## The main decision

Run **one debugger agent**, but allow it to own **multiple debug contexts**.

A debug context is not the same thing as a process or CDP connection. It is the
durable place where one debugging activity keeps its intent:

- breakpoints;
- watch expressions;
- one shared source graph/catalog and path configuration;
- target-selection rules;
- policies and observation history;
- a map of durable named connection configurations.

The context continues to exist with no program running. It can also own several
live connections at once, such as a Node.js server and Chrome frontend.

```text
Debugger agent
├─ Context: web application / development
│  ├─ durable breakpoints, watches, policies, and source graph
│  └─ connections
│     ├─ server: Node.js (connected)
│     └─ browser: Chrome (connected)
└─ Context: test runner
   ├─ different intent and source graph
   └─ connections: {} (disconnected)
```

## Why not one global context?

One global context would mix unrelated intent:

- a test runner and web application may use the same source paths;
- two clients may debug unrelated repositories concurrently;
- breakpoint and target defaults would become ambiguous;
- disconnecting or reconfiguring one activity could affect another.

The agent is global so clients can discover and observe everything through one
service. Contexts provide explicit isolation inside that service.

## Is a context "per cwd"?

Not exactly. A path context is identified by its normalized absolute path;
the current working directory supplies a **default selector** that finds
either an explicit cwd binding or a registered path context at or above it.
The selected context need not equal the exact cwd.

This distinction matters because:

- commands run from subdirectories should usually find the same context;
- one workspace may have several debug profiles;
- one debug context may cover several workspace folders;
- VS Code, MCP, and other clients may not have a meaningful process cwd.

For a CLI, an explicit `--context` wins; otherwise the nearest cwd selection
wins over automatic path-context lookup. Named contexts (`:<id>`) cover
investigations not associated with a project path. Multi-root and untitled
VS Code policy is deferred.

## The state hierarchy

```text
DebuggerAgentState
└─ DebugContext
   ├─ Desired intent
   │  ├─ breakpoints
   │  ├─ watches
   │  └─ policies and configuration
   ├─ Connections (zero or more)
   │  └─ each has its own lifecycle and generation
   │     ├─ targets and attachments
   │     ├─ scripts
   │     └─ current pauses and frames
   ├─ Derived knowledge
   │  ├─ shared source graph and resolved projections
   │  ├─ mapped locations
   │  └─ breakpoint assessments and cross-connection applications
   ├─ Immutable details
   │  ├─ source contents
   │  ├─ source maps
   │  └─ scopes, properties, and other large artifacts
   └─ Progress and diagnostics
```

## The five kinds of state

### 1. Desired intent

What users want. It survives disconnects. A breakpoint can therefore exist
before any script or runtime exists.

### 2. Runtime facts

What each connected runtime reported. A connection owns its own generation,
targets, attachments, scripts, and lifecycle. Runtime references include context
ID, connection ID, and connection generation, plus attachment incarnation,
script version, or pause epoch where needed.

### 3. Derived knowledge

What the debugger concludes by combining intent with facts: for example, which
generated locations in both a Node.js server and Chrome correspond to one
authored TypeScript breakpoint.

The context's source graph connects provider-qualified, versioned source
snapshots through typed identity, source-map, formatting, edit, and offset
projections. Runtime scripts from every connection contribute live endpoints;
target selection filters endpoint applicability, not source identity.

Conclusions retain their evidence and uncertainty. "Unconfirmed",
"ambiguous", and corrected locations are real states.

### 4. Immutable details

Large or lazy values are stored as immutable artifacts. A state revision points
to an artifact; the artifact never changes.

Loading a missing detail creates a later state revision. An old revision never
changes underneath a client.

### 5. Progress and diagnostics

Reconciliation may be waiting for a script, loading a source map, installing a
breakpoint, or failing. These states are explicit rather than represented as
missing data.

## How state changes

There is one deterministic state transition path:

```text
user command ───────┐
runtime observation ├─> reducer ─> new immutable revision + effects
effect completion ──┘
```

- User commands express intent.
- Runtime observations report CDP facts.
- Effects perform asynchronous work.
- Effect completions report exact results back to the reducer.

The reducer is the only state writer. Record/replay can therefore reproduce and
test state changes without a live browser.

## How clients read state

Clients use one observation mechanism:

- consume one atomic snapshot for a one-shot `get`;
- continue consuming revisions for `watch`.

Every value in one snapshot comes from the same immutable context revision,
including changes spanning connections. Lazy detail requests produce later
revisions rather than mutating an already observed one.

## Example

1. A client creates a breakpoint in a context with no connections.
2. Its assessment is `unconfirmed`.
3. Connections named `server` and `browser` connect and observe their own targets
   and scripts.
4. The shared source graph identifies both generated projections.
5. One breakpoint specification installs on matching Node.js and Chrome
   attachments.
6. The breakpoint may be active on one connection and still waiting on another.
7. Disconnecting `browser` removes only its live facts and breakpoint
   applications. The intent, offline graph data, `server`, and its binding remain.

## Essential rules

1. One global service, multiple explicitly isolated contexts.
2. Context identity is stable and explicit; cwd only helps select one.
3. A context owns zero or more durable named connection states/configurations.
4. Durable intent, policies, source knowledge, and observation revision are
   shared across those connections.
5. Runtime references include connection identity and enough generation
   information to reject staleness.
6. Focus helps interactive commands but never silently scopes durable breakpoint
   intent, whose default is all eligible context targets.
7. Requested locations are never silently replaced by corrected locations.
8. Old state revisions and artifacts are immutable.
9. Unknown, ambiguous, partial, and failed states remain explicit.
10. Multiple clients observe coherent revisions and cannot bypass the reducer.
