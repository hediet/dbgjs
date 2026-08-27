# Source graph next steps

Status: source graph inspection is implemented; provider rules, conflict
arbitration, and breakpoint anchoring remain.

## Implemented foundation

The debugger context owns one global graph of immutable, URI-addressed source
snapshots and authoritative `derived -> basis` projections.

The current implementation includes:

- content-addressed and provider-versioned source revisions;
- cross-target snapshot and source-map deduplication;
- contribution-based ownership, reference counting, and in-memory cleanup;
- cycle and self-projection rejection;
- compacted and uncompacted graph dumps;
- corresponding-path and exclusive source-map fan-out compaction;
- loaded-source and terminal-resolved-source URI trees;
- source-level graph traversal through `jsdbg source resolve`;
- position-level traversal through `jsdbg source map`;
- runtime scripts without source maps as first-class loaded snapshots.

## Next milestone: lazy projection rules

Add first-class rules such as:

```text
https://main.vscode-cdn.net/sourcemaps/<revision>/src/vs/*
  -> file:///c:/dev/vscode/src/vs/*
```

A rule describes a URI/path transformation. It must not recursively scan the
destination filesystem or eagerly add every possible source. Instead, it should
materialize snapshots and projections only for source URIs already present in
the graph or requested by a source demand.

Rules must preserve the graph's corresponding-path invariant and produce normal
authoritative projections once materialized.

## Projection conflict arbitration

Introduce explicit precedence for projection candidates. If multiple providers
map the same source range to different targets, one applicable candidate must
win deterministically.

The current behavior rejects conflicting source maps for one exact derived
snapshot. The new behavior should retain enough diagnostics to explain rejected
candidates without making the active graph ambiguous.

Exact duplicate projections should continue to deduplicate. Self-projections
and cycles should continue to fail.

Supporting two targets that assign different source maps to the same exact
source snapshot remains out of scope. Such targets must expose distinct source
identities or revisions.

## Providers, contributors, and demands

Keep these responsibilities distinct:

- A **contributor** owns graph facts and determines their lifetime.
- A **provider** resolves source selectors, observes revisions, retrieves
  content, or applies projection rules.
- A **demand** records interest in a source that is not currently materialized.

Targets, workspaces, breakpoints, caches, and user-defined rules may act as
providers or contributors in different combinations. Provider capability must
not create a second ownership or endpoint registry.

The existing contribution-level `loaded` role should remain the source of truth
for live runtime endpoints. Add further roles only when concrete behavior
requires them.

## Breakpoint snapshot anchoring

Represent a durable breakpoint location with both URI intent and an optional
resolved snapshot:

```rust
struct BreakpointLocation {
    uri: SourceUri,
    snapshot: Option<SourceSnapshotId>,
    line: u32,
    column: u32,
}
```

When the direct source is available, bind a breakpoint to its current
content-addressed snapshot. When it is unavailable, preserve the URI location
as unresolved intent rather than inventing a fake snapshot.

An unresolved breakpoint should create a source demand. Providers and lazy
projection rules may satisfy that demand without scanning unrelated files.

## Breakpoint migration across edits

When a local file changes from one content revision to another, record or obtain
an edit projection between the revisions and move anchored breakpoints through
it using simple best-effort position mapping.

If no exact mapping exists, use a practical nearby-position fallback. Do not add
affinity or mapping-quality taxonomies solely for breakpoint migration.

The current file revision should become the breakpoint's resolved snapshot
without losing its durable URI intent.

## Persistence and memory

Contribution cleanup currently removes unreferenced graph, source-map, and
content-addressed data from memory. A later increment may:

- persist the content-addressed store to disk;
- compress source and source-map content;
- evict cold in-memory content while retaining identities and provenance;
- reuse persisted content across debugger contexts or processes.

Persistence must preserve content identity and must not change graph authority
or contribution lifetime semantics.

## Inspection follow-up

`jsdbg source resolve <uri>` currently returns the selected concrete subgraph
using the uncompacted graph renderer. A dedicated route-oriented human view
could make individual resolution chains easier to read, while JSON should
continue to expose the complete selected subgraph.

This is presentation work and is not a prerequisite for provider rules or
breakpoint anchoring.
