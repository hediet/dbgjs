# Debugger Data Model

## Status

This document describes the target semantic data model for the debugger agent.
It is intentionally broader than the currently implemented Rust prototype.
For a shorter introduction, see
[Debugger Data Model: Short Overview](./debugger-data-model-overview.md).

The TypeScript interfaces are conceptual:

- `readonly` means a committed debugger-state revision never changes.
- `ReadonlyMap` documents keyed identity. A JSON/HubRPC representation may use
  arrays of entries instead.
- Function-bearing interfaces describe behavior without prescribing storage.
  For example, source-map data is represented by mapping functions rather than
  by exposing a particular source-map parser.
- Large values are represented by immutable artifact references rather than
  embedded directly in every state revision.

The debugger state is not a mirror of CDP. It is the agent's coherent knowledge
of:

1. Durable user intent.
2. Facts observed from zero or more current runtime connections.
3. Knowledge derived from intent and facts across those connections.
4. Materialized immutable details.
5. Reconciliation progress and diagnostics.

## 1. Core principles

```ts
/**
 * One committed state revision is an immutable value.
 *
 * Old revisions never gain details later. Fetching or deriving additional
 * information creates a new revision.
 */
interface StateRef {
	readonly agentInstanceId: AgentInstanceId;
	readonly revision: StateRevision;
}
```

Focus is useful only for commands requiring one live target. It may move between
connections. It never rewrites a persistent selector: in particular, the
normalized default for a new breakpoint is `{ kind: "all" }`, not the focused
connection or target.

The important laws are:

```text
same StateRef => same compact debugger state
same ArtifactRef => same artifact value
detail hydration => a later StateRef
disconnect(connection) => removes only that connection's live facts/bindings
runtime references => include connection identity and generation/version/epoch
breakpoint correction => preserves the originally requested location
unknown/unconfirmed => explicit state, not an empty successful result
```

## 2. Identity types

```ts
declare const brand: unique symbol;

type Brand<T, Name extends string> = T & {
	readonly [brand]: Name;
};

type AgentInstanceId = Brand<string, "AgentInstanceId">;
type StateRevision = Brand<number, "StateRevision">;
type CommandId = Brand<string, "CommandId">;
type EffectId = Brand<string, "EffectId">;
type ClientId = Brand<string, "ClientId">;

type DebugContextId = Brand<string, "DebugContextId">;
type ContextRevision = Brand<number, "ContextRevision">;
type ConnectionId = Brand<string, "ConnectionId">;
type ConnectionGeneration = Brand<number, "ConnectionGeneration">;
type TargetId = Brand<string, "TargetId">;
type AttachmentId = Brand<string, "AttachmentId">;
type AttachmentIncarnation = Brand<number, "AttachmentIncarnation">;
type ExecutionContextId = Brand<string, "ExecutionContextId">;
type ScriptId = Brand<string, "ScriptId">;
type ScriptVersion = Brand<number, "ScriptVersion">;
type PauseEpoch = Brand<number, "PauseEpoch">;
type CallFrameIndex = Brand<number, "CallFrameIndex">;

type AttachmentRuleId = Brand<string, "AttachmentRuleId">;
type BreakpointId = Brand<string, "BreakpointId">;
type WatchId = Brand<string, "WatchId">;
type CoverageObjectId = Brand<string, "CoverageObjectId">;
type DiagnosticId = Brand<string, "DiagnosticId">;
type EventSequence = Brand<number, "EventSequence">;

type LogicalSourceId = Brand<string, "LogicalSourceId">;
type SourceSnapshotId = Brand<string, "SourceSnapshotId">;
type SourceViewId = Brand<string, "SourceViewId">;
type ContentId = Brand<string, "ContentId">;
type ArtifactId = Brand<string, "ArtifactId">;
```

Durable IDs such as `BreakpointId` remain meaningful while disconnected.
Runtime references carry enough incarnation information to reject stale use:

```ts
interface RuntimeRef {
	readonly contextId: DebugContextId;
	readonly connectionId: ConnectionId;
	readonly connectionGeneration: ConnectionGeneration;
}

interface TargetRef extends RuntimeRef {
	readonly targetId: TargetId;
}

interface AttachmentRef extends RuntimeRef {
	readonly attachmentId: AttachmentId;
	readonly incarnation: AttachmentIncarnation;
}

interface ScriptRef {
	readonly attachment: AttachmentRef;
	readonly scriptId: ScriptId;
	readonly version: ScriptVersion;
}

interface PauseRef {
	readonly attachment: AttachmentRef;
	readonly epoch: PauseEpoch;
}

interface CallFrameRef {
	readonly pause: PauseRef;
	readonly index: CallFrameIndex;
}

interface PendingEffectRef {
	/** Correlates the exact semantic effect and rejects duplicate/stale completion. */
	readonly effectId: EffectId;
	/** Optional provenance; runtime-triggered effects need not originate at a command. */
	readonly initiatedBy?: CommandId;
}
```

The nested application map makes simultaneous binding explicit. One specification
can be active in a Node attachment on connection `server` and a Chrome attachment
on connection `browser`. `AttachmentRef` in every application repeats the
qualified connection and generation needed to reject stale completions.

`ConnectionGeneration` is scoped to one `(contextId, connectionId)` pair.
Raw CDP session IDs, request IDs, breakpoint IDs, call-frame IDs, and object IDs
are adapter implementation details. They are not durable public identity.

## 3. Root state

```ts
interface DebuggerAgentState {
	readonly ref: StateRef;
	readonly contexts: ReadonlyMap<DebugContextId, DebugContextState>;
	readonly diagnostics: readonly DiagnosticSummary[];
}
```

Every state-producing reducer transition creates a new root. Unchanged maps and
nodes may reuse the same allocations internally.

## 4. Durable debug contexts and unconnected mode

A debug context is the durable owner of debugger intent and shared derived
knowledge. It can exist with zero, one, or many named connections; each
connection may independently be disconnected, connecting, connected, or failed.

```ts
interface DebugContextState {
	readonly id: DebugContextId;
	readonly displayName: string;

	readonly configuration: DebugContextConfiguration;
	readonly connections: ReadonlyMap<ConnectionId, DebugConnectionState>;

	readonly attachmentRules: ReadonlyMap<
		AttachmentRuleId,
		AttachmentRuleState
	>;
	readonly focus: FocusState;
	readonly breakpoints: ReadonlyMap<BreakpointId, BreakpointState>;
	readonly watches: ReadonlyMap<WatchId, WatchState>;
	readonly sources: SourceGraphState;
	readonly coverage: CoverageState;
	readonly policies: DebuggerPolicies;
	readonly observation: ContextObservationState;
}

interface DebugContextConfiguration {
	readonly sourceResolution: SourceResolutionPolicy;
	readonly pathMappings: readonly PathMapping[];
	readonly workspaceHints: readonly string[];
}

interface DebugConnectionState {
	readonly id: ConnectionId;
	readonly displayName: string;
	readonly configuration: ConnectionConfiguration;
	readonly lifecycle: ConnectionLifecycleState;
}

interface ConnectionConfiguration {
	readonly endpoint?: EndpointConfiguration;
	readonly launch?: LaunchConfiguration;
}

type EndpointConfiguration =
	| {
			readonly kind: "http-discovery";
			readonly url: string;
	  }
	| {
			readonly kind: "websocket";
			readonly url: string;
	  }
	| {
			readonly kind: "process-discovery";
			readonly processId: number;
	  };

interface LaunchConfiguration {
	readonly provider: string;
	readonly executable: string;
	readonly arguments: readonly string[];
	readonly workingDirectory?: string;
	readonly environment: ReadonlyMap<string, string>;
	readonly initialBreak: boolean;
}
```

The connection lifecycle is explicit. Absence of entries in
`DebugContextState.connections` represents a context with no configured
connections:

```ts
type ConnectionLifecycleState =
	| {
			readonly status: "disconnected";
			readonly lastGeneration?: ConnectionGeneration;
			readonly lastDiagnostic?: DiagnosticId;
	  }
	| {
			readonly status: "connecting";
			readonly commandId: CommandId;
			readonly nextGeneration: ConnectionGeneration;
	  }
	| ConnectedConnectionState
	| {
			readonly status: "failed";
			readonly attemptedGeneration: ConnectionGeneration;
			readonly diagnostic: DiagnosticId;
	  };

interface ConnectedConnectionState {
	readonly status: "connected";
	readonly generation: ConnectionGeneration;
	readonly endpointKind: "browser" | "direct-target";
	readonly targets: TargetGraphState;
	readonly attachments: ReadonlyMap<AttachmentId, AttachmentState>;
}

interface ContextObservationState {
	/** Increments with every committed change visible in this context. */
	readonly revision: ContextRevision;
	readonly lastEventSequence?: EventSequence;
	readonly diagnostics: readonly DiagnosticId[];
}
```

Setting a breakpoint while the connection map is empty, or while every
connection is disconnected or failed, is valid. Connecting any entry later
reconciles existing context intent with its newly observed targets and scripts.
Adding or reconnecting one connection does not replace another.
Disconnect retains the named connection and its configuration. Removing that
durable map entry is a separate explicit operation and still does not delete
context-owned intent or retained graph data.

All changes committed for one logical observation use one new context revision,
even when reconciliation adds or removes applications on several connections.

## 5. Target graph, attachment rules, and focus

CDP targets form a graph, not one universally correct tree.

Each connected `DebugConnectionState` owns one `TargetGraphState`. The context
may present their union, but target identity and every relation retain
`connectionId` through `TargetRef`.

```ts
interface TargetGraphState {
	readonly targets: ReadonlyMap<TargetId, TargetState>;
	readonly relations: readonly TargetRelation[];
}

interface TargetState {
	readonly ref: TargetRef;
	readonly type: string;
	readonly title: string;
	readonly url: string;
	readonly browserContextId?: string;
	readonly lifecycle: "present" | "destroying";
}

interface TargetRelation {
	readonly kind:
		| "attachment-parent"
		| "opener"
		| "browser-context"
		| "worker-owner"
		| "frame-owner"
		| "runtime-specific";
	readonly from: TargetRef;
	readonly to: TargetRef;
	readonly runtimeSpecificKind?: string;
}
```

Selectors are durable descriptions, not live target handles:

```ts
type TargetSelector =
	| {
			readonly kind: "target";
			readonly target: TargetRef;
	  }
	| {
			readonly kind: "connection";
			readonly connectionId: ConnectionId;
			readonly target: TargetSelector;
	  }
	| {
			readonly kind: "url";
			readonly pattern: string;
	  }
	| {
			readonly kind: "types";
			readonly types: readonly string[];
	  }
	| {
			readonly kind: "related-to";
			readonly target: TargetSelector;
			readonly relationKinds: readonly TargetRelation["kind"][];
	  }
	| {
			/** All eligible current and future targets in the context. */
			readonly kind: "all";
	  };

interface AttachmentRuleState {
	readonly id: AttachmentRuleId;
	readonly selector: TargetSelector;
	readonly enabled: boolean;
	readonly cardinality: "one" | "many";
	readonly status: SelectorResolutionState;
}

type SelectorResolutionState =
	| {
			readonly status: "unresolved";
			readonly reason: "no-connections" | "no-match";
	  }
	| {
			readonly status: "resolved";
			readonly targets: readonly TargetRef[];
	  }
	| {
			readonly status: "ambiguous";
			readonly candidates: readonly TargetRef[];
	  };
```

Focus is separate from attachment:

```ts
type FocusState =
	| {
			readonly status: "none";
	  }
	| {
			readonly status: "following";
			readonly selector: TargetSelector;
			readonly resolution: SelectorResolutionState;
	  }
	| {
			readonly status: "pinned";
			readonly target: TargetRef;
	  };
```

## 6. Attachments and execution contexts

```ts
interface AttachmentState {
	readonly ref: AttachmentRef;
	readonly target: TargetRef;
	readonly phase:
		| "configuring"
		| "running"
		| "paused"
		| "resuming"
		| "detaching"
		| "failed";
	readonly waitingForDebugger: boolean;
	readonly executionContexts: ReadonlyMap<
		ExecutionContextId,
		ExecutionContextState
	>;
	readonly scripts: ReadonlyMap<ScriptId, RuntimeScriptState>;
	readonly pause?: PauseSnapshot;
	readonly diagnostic?: DiagnosticId;
}

interface ExecutionContextState {
	readonly id: ExecutionContextId;
	readonly attachment: AttachmentRef;
	readonly name: string;
	readonly origin: string;
	readonly isDefault: boolean;
	readonly worldKind: "default" | "isolated" | "worker" | "other";
}
```

An attachment disappearing invalidates all scripts, pauses, frames, scopes, and
remote objects scoped to its incarnation.

## 7. Breakpoint specifications, assessment, and live application

A breakpoint has three independent dimensions:

1. Durable requested intent.
2. Logical assessment of that intent.
3. Zero or more live applications to runtime attachments.

```ts
interface BreakpointState {
	readonly spec: BreakpointSpec;
	readonly assessment: BreakpointAssessment;
	readonly applications: ReadonlyMap<
		ConnectionId,
		ReadonlyMap<AttachmentId, AttachmentBreakpointApplication>
	>;
}

interface AttachmentBreakpointApplication {
	readonly attachment: AttachmentRef;
	readonly application: BreakpointApplication;
}

interface BreakpointSpec {
	readonly id: BreakpointId;
	readonly owner: ClientId;
	readonly targetSelector: TargetSelector;
	readonly requestedLocation: LogicalLocation;
	readonly enabled: boolean;
	readonly behavior: BreakpointBehavior;
}

type BreakpointBehavior =
	| {
			readonly kind: "pause";
			readonly condition?: string;
	  }
	| {
			readonly kind: "log";
			readonly expression: string;
			readonly condition?: string;
	  };
```

Logical assessment does not require a live connection when workspace or cached
source evidence is available:

```ts
type BreakpointAssessment =
	| {
			readonly status: "unconfirmed";
			readonly reason:
				| "no-connections"
				| "source-catalog-unavailable"
				| "source-content-unavailable"
				| "applicable-target-unknown";
	  }
	| {
			readonly status: "confirmed";
			readonly requested: LogicalLocation;
			readonly effective: LogicalLocation;
			readonly basis: readonly ConfirmationBasis[];
			readonly correction?: LocationCorrection;
	  }
	| {
			readonly status: "ambiguous";
			readonly requested: LogicalLocation;
			readonly candidates: readonly BreakpointCandidate[];
	  }
	| {
			readonly status: "rejected";
			readonly requested: LogicalLocation;
			readonly diagnostic: DiagnosticId;
	  };

type ConfirmationBasis =
	| {
			readonly kind: "workspace-content";
			readonly content: ContentId;
	  }
	| {
			readonly kind: "source-map";
			readonly generatedContent: ContentId;
			readonly sourceMapContent: ContentId;
	  }
	| {
			readonly kind: "runtime";
			readonly script: ScriptRef;
	  };

interface BreakpointCandidate {
	readonly location: LogicalLocation;
	readonly quality: MappingQuality;
	readonly basis: readonly ConfirmationBasis[];
}

interface LocationCorrection {
	readonly from: LogicalLocation;
	readonly to: LogicalLocation;
	readonly reason:
		| "nearest-breakable-location"
		| "source-map-lower-bound"
		| "column-normalization"
		| "runtime-adjustment";
	readonly quality: MappingQuality;
}
```

The requested location is never silently overwritten. A later correction adds
an effective location and provenance.

Live application is connection- and attachment-specific:

```ts
type BreakpointApplication =
	| {
			readonly status: "not-applicable";
			readonly reason: string;
	  }
	| {
			readonly status: "waiting-for-script";
	  }
	| {
			readonly status: "hydrating-source";
			readonly operation: PendingEffectRef;
	  }
	| {
			readonly status: "mapping";
			readonly operation: PendingEffectRef;
	  }
	| {
			readonly status: "installing";
			readonly resolutions: readonly GeneratedBreakpointResolution[];
			readonly operation: PendingEffectRef;
	  }
	| {
			readonly status: "active";
			readonly bindings: readonly PhysicalBreakpointBinding[];
	  }
	| {
			readonly status: "partial";
			readonly bindings: readonly PhysicalBreakpointBinding[];
			readonly diagnostics: readonly DiagnosticId[];
	  }
	| {
			readonly status: "failed";
			readonly diagnostic: DiagnosticId;
	  };

interface GeneratedBreakpointResolution {
	readonly script: ScriptRef;
	readonly requestedLogicalLocation: LogicalLocation;
	readonly effectiveLogicalLocation: LogicalLocation;
	readonly generatedLocation: GeneratedLocation;
	readonly quality: MappingQuality;
}

interface PhysicalBreakpointBinding {
	readonly resolution: GeneratedBreakpointResolution;
	readonly backendBindingRef: string;
}
```

The backend binding reference is an opaque, generation-scoped implementation
reference. It is not persisted as desired state.

Disconnecting one connection removes only that connection's nested application
map. The specification, assessment, and applications on other connections
remain. Reconnecting may create new applications without copying or changing
the durable specification.

For DAP, `Breakpoint.verified` is an adapter projection of this richer model.
The core model does not reduce assessment and multi-target application to one
boolean.

## 8. Shared source graph, catalog, materialization, and projection

The context owns one source graph shared by all connections. The graph
distinguishes provider-qualified, versioned source snapshots from live runtime
endpoints and from human-friendly logical paths. Its catalog is an index over
the graph, not a separate per-connection source model.

Live generated scripts remain owned by their connection and attachment. Each
contributes an endpoint to the shared graph. Workspace, source-map, formatted,
edited, and cached evidence can remain in the graph while all connections are
offline.

```ts
interface SourceGraphState {
	readonly catalog: SourceCatalogState;
	readonly snapshots: ReadonlyMap<SourceSnapshotId, SourceSnapshotState>;
	readonly projections: readonly SourceProjectionEdgeState[];
	readonly liveEndpoints: readonly RuntimeSourceEndpoint[];
}

interface SourceCatalogState {
	readonly logicalSources: ReadonlyMap<LogicalSourceId, LogicalSourceState>;
}

interface SourceSnapshotRef {
	readonly id: SourceSnapshotId;
	readonly provider: string;
	readonly providerKey: string;
	readonly version: string;
}

interface SourceSnapshotState {
	readonly ref: SourceSnapshotRef;
	readonly content: DetailState<ContentArtifactRef>;
}

interface SourceProjectionEdgeState {
	readonly kind: "identity" | "source-map" | "format" | "edit" | "offset";
	readonly from: SourceSnapshotRef;
	readonly to: SourceSnapshotRef;
	readonly quality: MappingQuality;
}

interface RuntimeSourceEndpoint {
	readonly script: ScriptRef;
	readonly generatedSnapshot: DetailState<SourceSnapshotRef>;
}

interface RuntimeScriptState {
	readonly ref: ScriptRef;
	readonly generatedUrl: string;
	readonly contentHashReportedByRuntime?: string;
	readonly sourceMapUrl?: string;
	readonly mapDiscovery: SourceMapDiscoveryState;
	readonly sourceView: MaterializationState<SourceViewRef>;
}

type SourceMapDiscoveryState =
	| {
			readonly status: "unknown";
	  }
	| {
			readonly status: "discovering";
			readonly operation: PendingEffectRef;
	  }
	| {
			readonly status: "discovered";
			readonly sourceMapContent: ContentId;
			readonly logicalSources: readonly LogicalSourceId[];
	  }
	| {
			readonly status: "unavailable";
			readonly diagnostic: DiagnosticId;
	  };

interface LogicalSourceState {
	readonly id: LogicalSourceId;
	readonly logicalUrl: string;
	readonly roles: readonly (
		| "runtime-generated"
		| "source-map-authored"
		| "workspace"
		| "formatted"
	)[];
	readonly applicableScripts: readonly ScriptRef[];
	readonly primaryContent: DetailState<ContentArtifactRef>;
	readonly alternativeContents: readonly ContentCandidate[];
	readonly provenance: readonly SourceProvenance[];
}

interface ContentCandidate {
	readonly content: DetailState<ContentArtifactRef>;
	readonly provenance: SourceProvenance;
	readonly freshness: "exact" | "stale" | "unknown";
}

type SourceProvenance =
	| {
			readonly kind: "runtime-generated";
			readonly script: ScriptRef;
	  }
	| {
			readonly kind: "source-map-content";
			readonly script: ScriptRef;
			readonly sourceMapContent: ContentId;
	  }
	| {
			readonly kind: "workspace";
			readonly workspacePath: string;
			readonly content: ContentId;
	  }
	| {
			readonly kind: "formatted";
			readonly generatedContent: ContentId;
	  };
```

Materialization is explicit:

```ts
type MaterializationState<T> =
	| {
			readonly status: "not-requested";
	  }
	| {
			readonly status: "pending";
			readonly phase:
				| "fetching-generated-source"
				| "fetching-source-map"
				| "building-projection"
				| "indexing";
			readonly operation: PendingEffectRef;
	  }
	| {
			readonly status: "ready";
			readonly value: T;
	  }
	| {
			readonly status: "failed";
			readonly diagnostic: DiagnosticId;
			readonly retryable: boolean;
	  };

interface SourceViewRef {
	readonly id: SourceViewId;
	readonly script: ScriptRef;
	readonly snapshots: readonly SourceSnapshotRef[];
	readonly generatedContent: ContentId;
	readonly sourceMapContent?: ContentId;
}
```

Snapshot identity is provider-qualified and versioned; neither a path nor a
content hash alone is assumed to identify it. Content identity still enables
deduplication. Projection edges are typed so identity, source-map, formatting,
edit/version, and offset transforms retain their semantics and provenance.
Their dependency orientation forms a DAG, while location queries may traverse
an edge in either direction. Generated, authored, workspace, and formatted are
roles a snapshot can have in particular relationships, not exclusive source
kinds.
Target selection does not participate in graph identity or traversal. A target
selector filters `RuntimeSourceEndpoint` applicability only after source
resolution.

`AttachmentState.scripts` is keyed by `ScriptId` only inside one attachment
incarnation. A context-wide table must never use bare CDP `ScriptId` as an
identity. Disconnecting a connection removes only its scripts, runtime
endpoints, and applicability links. Source snapshots, typed projections,
logical identities, and immutable content evidence remain when still supported
by workspace, cache, edit, or artifact provenance. Other connections' endpoints
remain live.

The source-map representation is an implementation detail. Its semantic
contract is:

```ts
interface SourceProjection {
	readonly ref: SourceViewRef;

	mapGeneratedToLogical(
		location: GeneratedLocation,
	): readonly LogicalLocationCandidate[];

	mapLogicalToGenerated(
		location: LogicalLocation,
	): readonly GeneratedLocationCandidate[];
}

interface LogicalLocation {
	readonly source: LogicalSourceId;
	readonly line: number;
	readonly column: number;
}

interface GeneratedLocation {
	readonly script: ScriptRef;
	readonly line: number;
	readonly column: number;
}

interface LogicalLocationCandidate {
	readonly location: LogicalLocation;
	readonly quality: MappingQuality;
	readonly path: readonly ProjectionStep[];
}

interface GeneratedLocationCandidate {
	readonly location: GeneratedLocation;
	readonly quality: MappingQuality;
	readonly path: readonly ProjectionStep[];
}

type MappingQuality =
	| "exact"
	| "greatest-lower-bound"
	| "formatted"
	| "ambiguous";

interface ProjectionStep {
	readonly kind:
		| "identity"
		| "source-map"
		| "workspace"
		| "format"
		| "edit"
		| "offset";
	readonly inputContent: ContentId;
	readonly outputContent: ContentId;
}
```

Columns use UTF-16 code units at the debugger protocol boundary.

## 9. Pause snapshots, frames, scopes, and remote values

A pause is an ephemeral snapshot scoped to one attachment incarnation and pause
epoch.

```ts
interface PauseSnapshot {
	readonly ref: PauseRef;
	readonly reason: PauseReason;
	readonly frames: readonly CallFrameState[];
	readonly exception: PauseExceptionState;
}

type PauseExceptionState =
	| {
			readonly status: "none";
	  }
	| {
			readonly status: "present";
			readonly value: DetailState<RemoteValue>;
	  };

type PauseReason =
	| "breakpoint"
	| "exception"
	| "debug-command"
	| "step"
	| "instrumentation"
	| "other";

interface CallFrameState {
	readonly ref: CallFrameRef;
	readonly functionName: string;
	readonly rawLocation: GeneratedLocation;
	readonly projectedLocation: FrameProjectionState;
	readonly scopes: DetailState<ScopeListArtifactRef>;
	readonly thisValue: DetailState<RemoteValue>;
}

type FrameProjectionState =
	| {
			readonly status: "raw";
	  }
	| {
			readonly status: "pending";
			readonly operation: PendingEffectRef;
	  }
	| {
			readonly status: "resolved";
			readonly location: LogicalLocation;
			readonly quality: MappingQuality;
	  }
	| {
			readonly status: "failed";
			readonly diagnostic: DiagnosticId;
	  };
```

Detail availability is explicit:

```ts
type DetailState<T> =
	| {
			readonly status: "not-requested";
	  }
	| {
			readonly status: "pending";
			readonly operation: PendingEffectRef;
	  }
	| {
			readonly status: "ready";
			readonly value: T;
	  }
	| {
			readonly status: "unavailable";
			readonly reason: string;
	  }
	| {
			readonly status: "failed";
			readonly diagnostic: DiagnosticId;
			readonly retryable: boolean;
	  };
```

Remote handles are pause-scoped:

```ts
interface RemoteObjectRef {
	readonly pause: PauseRef;
	readonly opaqueId: string;
}

type RemoteValue =
	| {
			readonly kind: "primitive";
			readonly value: null | boolean | number | string;
	  }
	| {
			readonly kind: "unserializable";
			readonly description: string;
	  }
	| {
			readonly kind: "object";
			readonly className?: string;
			readonly description: string;
			readonly object: RemoteObjectRef;
			readonly properties: DetailState<PropertyListArtifactRef>;
	  };
```

After resume, remote-object operations using the old `PauseRef` fail explicitly.
Previously materialized immutable property artifacts may remain readable.

## 10. Watch expressions

Watch specifications survive disconnects. Results are tied to a pause.

```ts
interface WatchState {
	readonly spec: WatchSpec;
	readonly result: WatchResultState;
}

interface WatchSpec {
	readonly id: WatchId;
	readonly owner: ClientId;
	readonly expression: string;
	readonly displayName?: string;
	readonly targetSelector: TargetSelector;
	readonly frameSelector: FrameSelector;
	readonly enabled: boolean;
	readonly sideEffectPolicy: "allow" | "throw-on-side-effect";
}

type FrameSelector =
	| {
			readonly kind: "top";
	  }
	| {
			readonly kind: "index";
			readonly index: number;
	  }
	| {
			readonly kind: "function";
			readonly pattern: string;
	  };

type WatchResultState =
	| {
			readonly status: "not-evaluated";
			readonly reason:
				| "disabled"
				| "no-matching-connection"
				| "not-paused";
	  }
	| {
			readonly status: "pending";
			readonly pause: PauseRef;
			readonly operation: PendingEffectRef;
	  }
	| {
			readonly status: "ready";
			readonly pause: PauseRef;
			readonly value: RemoteValue;
	  }
	| {
			readonly status: "exception";
			readonly pause: PauseRef;
			readonly exception: RemoteValue;
	  }
	| {
			readonly status: "unavailable";
			readonly pause?: PauseRef;
			readonly reason: string;
	  }
	| {
			readonly status: "stale";
			readonly previousPause: PauseRef;
	  };
```

## 11. Coverage state

Coverage recording is live state. Coverage objects are immutable values.

```ts
interface CoverageState {
	readonly recording?: CoverageRecordingState;
	readonly objects: ReadonlyMap<CoverageObjectId, CoverageObjectSummary>;
}

interface CoverageRecordingState {
	readonly status: "starting" | "active" | "stopping" | "failed";
	readonly targetSelector: TargetSelector;
	readonly startedAt: StateRef;
	readonly incompleteTargets: readonly TargetRef[];
	readonly diagnostic?: DiagnosticId;
}

interface CoverageObjectSummary {
	readonly id: CoverageObjectId;
	readonly capturedAt: StateRef;
	readonly artifact: CoverageArtifactRef;
	readonly incompleteTargets: readonly TargetRef[];
}
```

Exclusion and rendering are pure operations over immutable coverage artifacts:

```ts
interface CoverageOperations {
	exclude(
		selected: CoverageArtifactRef,
		baselines: readonly CoverageArtifactRef[],
	): Promise<CoverageArtifactRef>;

	project(
		coverage: CoverageArtifactRef,
		sourceView: SourceViewRef,
	): Promise<CoverageProjectionArtifactRef>;
}
```

## 12. Immutable artifacts

Large values do not belong directly in the compact state root.

```ts
interface ArtifactRef<Kind extends string = string> {
	readonly id: ArtifactId;
	readonly kind: Kind;
	readonly contentId: ContentId;
	readonly byteLength: number;
}

type ContentArtifactRef = ArtifactRef<"source-content">;
type SourceMapArtifactRef = ArtifactRef<"source-map">;
type ScopeListArtifactRef = ArtifactRef<"scope-list">;
type PropertyListArtifactRef = ArtifactRef<"property-list">;
type CoverageArtifactRef = ArtifactRef<"coverage">;
type CoverageProjectionArtifactRef = ArtifactRef<"coverage-projection">;
```

Artifact reading is asynchronous I/O but semantically pure:

```ts
interface ArtifactStore {
	/**
	 * Reading the same reference returns the same bytes or an explicit
	 * artifact-unavailable error. It never consults the current live runtime.
	 */
	read(ref: ArtifactRef): Promise<Uint8Array>;
}
```

Fetching new live details is not an artifact read. It is an effectful request
that creates a later state revision containing a new `ArtifactRef`.

## 13. Diagnostics and normalized events

Diagnostics are stable structured values:

```ts
interface DiagnosticSummary {
	readonly id: DiagnosticId;
	readonly severity: "info" | "warning" | "error";
	readonly code: string;
	readonly message: string;
	readonly relatedEntities: readonly EntityRef[];
}

type EntityRef =
	| {
			readonly kind: "context";
			readonly context: DebugContextId;
	  }
	| {
			readonly kind: "connection";
			readonly context: DebugContextId;
			readonly connection: ConnectionId;
	  }
	| {
			readonly kind: "target";
			readonly target: TargetRef;
	  }
	| {
			readonly kind: "attachment";
			readonly attachment: AttachmentRef;
	  }
	| {
			readonly kind: "script";
			readonly script: ScriptRef;
	  }
	| {
			readonly kind: "breakpoint";
			readonly breakpoint: BreakpointId;
	  }
	| {
			readonly kind: "pause";
			readonly pause: PauseRef;
	  };
```

Events record occurrences. They are not used to reconstruct current state:

```ts
interface DebuggerEvent {
	readonly sequence: EventSequence;
	readonly timestamp: string;
	readonly before: StateRef;
	readonly after: StateRef;
	readonly contextId?: DebugContextId;
	readonly payload: DebuggerEventPayload;
}

type DebuggerEventPayload =
	| {
			readonly kind: "console-message";
			readonly target: TargetRef;
			readonly message: string;
	  }
	| {
			readonly kind: "runtime-exception";
			readonly target: TargetRef;
			readonly value: RemoteValue;
	  }
	| {
			readonly kind: "process-output";
			readonly contextId: DebugContextId;
			readonly connectionId: ConnectionId;
			readonly generation: ConnectionGeneration;
			readonly stream: "stdout" | "stderr";
			readonly text: string;
	  }
	| {
			readonly kind: "state-change";
			readonly categories: readonly string[];
	  };
```

## 14. Public commands, runtime observations, and effect completions

These three input classes must remain separate.

### 14.1 Public user commands

```ts
type UserCommand =
	| {
			readonly kind: "put-connection";
			readonly contextId: DebugContextId;
			readonly connectionId: ConnectionId;
			readonly displayName: string;
			readonly configuration: ConnectionConfiguration;
	  }
	| {
			readonly kind: "remove-connection";
			readonly contextId: DebugContextId;
			readonly connectionId: ConnectionId;
	  }
	| {
			readonly kind: "put-breakpoint";
			readonly contextId: DebugContextId;
			readonly breakpoint: BreakpointSpec;
	  }
	| {
			readonly kind: "remove-breakpoint";
			readonly contextId: DebugContextId;
			readonly breakpointId: BreakpointId;
	  }
	| {
			readonly kind: "ensure-details";
			readonly basis: StateRef;
			readonly requests: readonly DetailRequest[];
	  }
	| {
			readonly kind: "connect";
			readonly contextId: DebugContextId;
			readonly connectionId: ConnectionId;
	  }
	| {
			readonly kind: "disconnect";
			readonly contextId: DebugContextId;
			readonly connectionId: ConnectionId;
	  }
	| {
			readonly kind: "resume";
			readonly pause: PauseRef;
	  }
	| {
			readonly kind: "step";
			readonly pause: PauseRef;
			readonly direction: "into" | "over" | "out";
	  };

interface CommandEnvelope {
	readonly commandId: CommandId;
	readonly clientId: ClientId;
	readonly expectedState?: StateRef;
	readonly idempotencyKey?: string;
	readonly command: UserCommand;
}
```

### 14.2 Runtime observations

Only runtime adapters create these:

```ts
type RuntimeObservation =
	| {
			readonly kind: "transport-connected";
			readonly contextId: DebugContextId;
			readonly connectionId: ConnectionId;
			readonly generation: ConnectionGeneration;
	  }
	| {
			readonly kind: "target-created";
			readonly target: TargetState;
	  }
	| {
			readonly kind: "script-parsed";
			readonly script: RuntimeScriptState;
	  }
	| {
			readonly kind: "paused";
			readonly pause: PauseSnapshot;
	  }
	| {
			readonly kind: "resumed";
			readonly pause: PauseRef;
	  }
	| {
			readonly kind: "transport-disconnected";
			readonly contextId: DebugContextId;
			readonly connectionId: ConnectionId;
			readonly generation: ConnectionGeneration;
			readonly reason: string;
	  };
```

Every runtime observation belongs to exactly one connection. Transport
observations carry `connectionId` directly; target, script, pause, and resume
observations carry it through their qualified refs. The reducer rejects an
observation whose connection generation is no longer current without consulting
focus or another connection. Consequences may still be committed atomically
across the context, such as one source-graph change reconciling breakpoint
applications on several connections.

### 14.3 Internal effect completions

Only effect executors create these:

```ts
interface EffectError {
	readonly code: string;
	readonly message: string;
	readonly retryable: boolean;
}

type EffectResult<T> =
	| {
			readonly ok: true;
			readonly value: T;
	  }
	| {
			readonly ok: false;
			readonly error: EffectError;
	  };

type EffectCompletion =
	| {
			readonly kind: "script-hydrated";
			readonly effectId: EffectId;
			readonly script: ScriptRef;
			readonly result: EffectResult<HydratedScript>;
	  }
	| {
			readonly kind: "source-view-built";
			readonly effectId: EffectId;
			readonly script: ScriptRef;
			readonly result: EffectResult<SourceViewRef>;
	  }
	| {
			readonly kind: "breakpoint-installed";
			readonly effectId: EffectId;
			readonly result: EffectResult<PhysicalBreakpointBinding>;
	  }
	| {
			readonly kind: "frame-mapped";
			readonly effectId: EffectId;
			readonly frame: CallFrameRef;
			readonly result: EffectResult<LogicalLocationCandidate>;
	  };

interface HydratedScript {
	readonly generatedContent: ContentArtifactRef;
	readonly sourceMapContent?: SourceMapArtifactRef;
	readonly sourceMapDiagnostic?: DiagnosticId;
}
```

The engine input is a tagged union, but only `UserCommand` is public HubRPC
input:

```ts
type EngineInput =
	| {
			readonly source: "user";
			readonly envelope: CommandEnvelope;
	  }
	| {
			readonly source: "runtime";
			readonly observation: RuntimeObservation;
	  }
	| {
			readonly source: "effect";
			readonly completion: EffectCompletion;
	  };
```

`CommandId` identifies and deduplicates public intent. `EffectId` identifies one
semantic asynchronous operation. A pending state stores both when the effect
originated from a command; completion matching uses `EffectId`, while
`initiatedBy` exists only for provenance and user-facing progress.

## 15. Consistent state observation

One observation primitive powers both one-shot reads and continuous views.

```ts
interface ObserveStateRequest {
	readonly queries: readonly NamedStateQuery[];
	readonly resumeAfter?: StateRef;
	readonly delivery: "latest" | "every-revision";
}

interface NamedStateQuery {
	readonly name: string;
	readonly query: StateQuery;
}

type StateQuery =
	| {
			readonly kind: "agent-summary";
	  }
	| {
			readonly kind: "context";
			readonly contextId: DebugContextId;
	  }
	| {
			readonly kind: "connections";
			readonly contextId: DebugContextId;
	  }
	| {
			readonly kind: "target-graphs";
			readonly contextId: DebugContextId;
			/** Omit to read every connection at the same context revision. */
			readonly connectionId?: ConnectionId;
	  }
	| {
			readonly kind: "pause";
			readonly attachment: AttachmentRef;
	  }
	| {
			readonly kind: "breakpoints";
			readonly contextId: DebugContextId;
	  }
	| {
			readonly kind: "sources";
			readonly contextId: DebugContextId;
	  };

interface StateFrame {
	readonly kind: "snapshot";
	readonly state: StateRef;
	readonly values: readonly NamedStateValue[];
}

interface NamedStateValue {
	readonly name: string;
	readonly value: unknown;
}

interface StateObservation {
	readonly frames: AsyncIterable<StateFrame>;
	cancel(reason?: string): void;
}

interface DebuggerStateService {
	/**
	 * Atomically captures the initial immutable root and registers for later
	 * roots. No state revision can occur between the initial read and
	 * subscription registration.
	 */
	observe(request: ObserveStateRequest): StateObservation;
}
```

A one-shot CLI command consumes the first frame and cancels. A `--watch` command
keeps consuming the same stream. Every named query in one frame is evaluated
against the same `StateRef`. Context queries include all connection records and
shared intent by default. Filtering a query to one connection does not weaken
the atomic context-revision boundary; a commit spanning several connections is
never exposed partially.

## 16. Detail materialization

```ts
type DetailRequest =
	| {
			readonly kind: "source-map-catalog";
			readonly script: ScriptRef;
	  }
	| {
			readonly kind: "source-content";
			readonly source: SourceSnapshotRef;
	  }
	| {
			readonly kind: "frame-scopes";
			readonly frame: CallFrameRef;
	  }
	| {
			readonly kind: "object-properties";
			readonly object: RemoteObjectRef;
	  };

interface EnsureDetailsRequest {
	readonly basis: StateRef;
	readonly details: readonly DetailRequest[];
}

interface CommitReceipt {
	readonly commandId: CommandId;
	readonly committedAt: StateRef;
}

interface DebuggerDetailsService {
	/**
	 * Ensuring details is effectful. It may commit pending state immediately
	 * and commit ready/failed state later.
	 */
	ensure(request: EnsureDetailsRequest): Promise<CommitReceipt>;
}
```

Source-content materialization addresses an exact provider-qualified snapshot.
It does not choose content from the focused target or silently advance to a
newer provider version.

If the runtime reference was already stale at `basis`, `ensure` fails
explicitly. It never silently resolves the same selector against a newer target,
script, or pause.

## 17. State transition examples

### 17.1 Breakpoint before any connection exists

```text
revision 1
  context.connections = {}
  breakpoint = absent

put breakpoint src/app.ts:17:5

revision 2
  context.connections = {}
  breakpoint.spec.requestedLocation = src/app.ts:17:5
  breakpoint.spec.targetSelector = all
  breakpoint.assessment = unconfirmed(no-connections)
  breakpoint.applications = {}
```

### 17.2 Two connections apply one breakpoint

```text
revision 10
  connections.server = connected(generation 3)
  connections.browser = connected(generation 8)
  breakpoint = unconfirmed(source-catalog-unavailable)

revision 11
  server matching script discovered
  browser matching script discovered
  breakpoint applications =
    server/a-node -> hydrating-source
    browser/a-page -> hydrating-source

revision 12
  shared source graph projections ready
  breakpoint assessment =
    confirmed(
      requested = src/app.ts:17:5,
      effective = src/app.ts:18:1,
      correction = nearest-breakable-location
    )
  breakpoint applications =
    server/a-node -> installing
    browser/a-page -> installing

revision 13
  breakpoint applications =
    server/a-node -> active(server.js:4:291)
    browser/a-page -> active(browser.js:9:117)
```

### 17.3 Disconnect one connection

```text
revision 20
  connections.server = connected(generation 3)
  connections.browser = disconnected(lastGeneration = 8)
  breakpoint applications =
    server/a-node -> active(server.js:4:291)
  breakpoint specification remains
  browser live targets, attachments, scripts, endpoints, and bindings are gone
  server live facts and binding remain
  shared offline source snapshots and projection edges remain
  immutable source-based assessment may remain with its recorded basis
```

### 17.4 Lazy details

```text
revision 30
  frame.scopes = not-requested

ensure frame scopes at revision 30

revision 31
  frame.scopes = pending

revision 32
  frame.scopes = ready(scope-artifact-7)

read scope-artifact-7
  pure immutable artifact read
```

Revision 30 never changes.

## 18. Required invariants

1. There is exactly one state writer per agent.
2. Reducer commits are synchronous and never hold a lock across `await`.
3. Effects run outside the state writer and return typed completions.
4. Every runtime reference and completion is validated against connection
   identity, generation, incarnation, script version, pause epoch, and effect
   identity as applicable.
5. User commands never target internal effect IDs.
6. Desired state survives connection disconnect and replacement; disconnecting
   one connection does not remove another connection's live facts or bindings.
7. A command resolves its target scope once and never silently retargets.
8. Breakpoint requested and effective locations remain distinct.
9. A derived fact records the immutable/runtime basis that justified it.
10. Old state revisions and artifacts are immutable.
11. Slow observers cannot block the state writer.
12. History loss, stale references, ambiguity, and unavailable details are
    explicit errors or states.
13. Mutation requests support idempotency and optional expected-revision checks.
14. State snapshots are authoritative for current truth; events are occurrence
    history.
15. A context owns one source graph, policy set, intent set, and observation
    revision across all of its connections.
16. A breakpoint specification can have applications on several connections;
    focus never silently limits its durable selector.
17. A commit that changes facts across connections is atomic at one context
    revision.

## 19. Current implementation correspondence

The Rust prototype currently implements important subsets of this model:

- A pure revisioned reducer.
- Immutable `Arc`-reused state nodes.
- Runtime sessions, scripts, pauses, breakpoints, and physical bindings for the
  prototype's currently active runtime.
- Connection generations, script versions, pause epochs, and effect IDs.
- Lazy script source hydration.
- Source views and forward/reverse mapping.
- Content-addressed source storage.
- Strict reducer recording and replay.
- Real Chrome and `vscode.dev` end-to-end tests.
- Durable contexts with named connection configurations, lifecycle state,
  generation-safe target topology, and restart persistence.
- Context-wide breakpoint intent with enabled state, conditions, target
  selectors, removal, and aggregate application assessments.
- Bounded context revision/event history with atomic snapshot-to-revision
  observation, long-poll watching, and explicit history-gap recovery.
- Expected-revision mutation checks and restart-persistent idempotency keys for
  managed lifecycle and breakpoint operations.
- Public source listing, generated-to-authored mapping, content display, grep,
  safe atomic export, and reloadable-cache eviction.

The following target-model pieces are not yet implemented:

- Unconnected HubRPC/DAP breakpoint workflows.
- Source-map path catalog discovery independent of full source hydration.
- The shared provider-qualified, versioned source graph with typed projections
  and live endpoints from every connection.
- Immutable detail artifacts for scopes, properties, and coverage.
- Native HubRPC server streaming and cancellation. The debugger currently
  exposes the same cursor semantics through cancellation-safe unary long
  polling.
- Explicit attachment-incarnation identities and selector policies spanning
  related workers and OOPIFs.
- A standalone per-context coordinator replacing the service-wide serialization
  lock.

The target model should guide these refactors without requiring the public
protocol to expose internal CDP or reducer implementation details.
