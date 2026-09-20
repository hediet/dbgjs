import { InterfaceDefinition, notificationType, requestType, type LinkRpcInterfaceSchema } from "@hediet/linkrpc";
import { z } from "zod";

const AgentSessionSnapshotSchema = z.object({
    chatUri: z.union([
        z.string(),
        z.null(),
    ]).optional(),
    disconnected: z.union([
        z.boolean(),
        z.null(),
    ]).optional(),
    internalId: z.string(),
    title: z.union([
        z.string(),
        z.null(),
    ]).optional(),
    workingDirectories: z.array(z.string()),
});

const BreakpointMappingSnapshotSchema = z.object({
    generatedColumn: z.int(),
    generatedLine: z.int(),
    generatedUrl: z.string(),
    projection: z.array(z.string()),
    quality: z.string(),
    requestedColumn: z.int(),
    requestedLine: z.int(),
    sourceUrl: z.string(),
});

const BreakpointApplicationStatusSchema = z.discriminatedUnion("kind", [
    z.object({
        kind: z.literal("installing"),
    }),
    z.object({
        backend_id: z.string(),
        kind: z.literal("installed"),
    }),
    z.object({
        kind: z.literal("removing"),
    }),
    z.object({
        kind: z.literal("failed"),
        message: z.string(),
    }),
]);

const BreakpointApplicationSnapshotSchema = z.object({
    connectionGeneration: z.int(),
    connectionId: z.string(),
    generatedColumn: z.int(),
    generatedLine: z.int(),
    mapping: z.union([
        BreakpointMappingSnapshotSchema,
        z.null(),
    ]).optional(),
    scriptId: z.string(),
    scriptUrl: z.string(),
    scriptVersion: z.int(),
    status: BreakpointApplicationStatusSchema,
    targetId: z.string(),
});

const BreakpointSourceCandidateSnapshotSchema = z.object({
    contentHash: z.string(),
    provenance: z.string(),
    sourceUrl: z.string(),
});

const BreakpointPendingReasonSchema = z.discriminatedUnion("kind", [
    z.object({
        kind: z.literal("waitingForTarget"),
    }),
    z.object({
        kind: z.literal("waitingForScript"),
    }),
    z.object({
        diagnostics: z.array(z.string()),
        kind: z.literal("sourceNotFound"),
    }),
    z.object({
        candidates: z.array(BreakpointSourceCandidateSnapshotSchema),
        kind: z.literal("ambiguousSource"),
        omitted_candidate_count: z.int(),
    }),
    z.object({
        diagnostics: z.array(z.string()),
        kind: z.literal("unmapped"),
    }),
    z.object({
        kind: z.literal("applicable"),
    }),
    z.object({
        kind: z.literal("installing"),
    }),
    z.object({
        kind: z.literal("failed"),
        message: z.string(),
    }),
]);

const BreakpointScriptAssessmentStatusSchema = z.discriminatedUnion("kind", [
    z.object({
        kind: z.literal("waitingForScript"),
    }),
    z.object({
        diagnostics: z.array(z.string()),
        kind: z.literal("sourceNotFound"),
    }),
    z.object({
        candidates: z.array(BreakpointSourceCandidateSnapshotSchema),
        kind: z.literal("ambiguousSource"),
        omitted_candidate_count: z.int(),
    }),
    z.object({
        candidate: BreakpointSourceCandidateSnapshotSchema,
        kind: z.literal("mapping"),
    }),
    z.object({
        candidate: BreakpointSourceCandidateSnapshotSchema,
        diagnostics: z.array(z.string()),
        kind: z.literal("unmapped"),
    }),
    z.object({
        candidate: BreakpointSourceCandidateSnapshotSchema,
        kind: z.literal("applicable"),
        mappings: z.array(BreakpointMappingSnapshotSchema),
    }),
    z.object({
        kind: z.literal("failed"),
        message: z.string(),
    }),
]);

const BreakpointScriptAssessmentSnapshotSchema = z.object({
    connectionGeneration: z.int(),
    connectionId: z.string(),
    scriptId: z.string(),
    scriptUrl: z.string(),
    scriptVersion: z.int(),
    status: BreakpointScriptAssessmentStatusSchema,
    targetId: z.string(),
});

const BreakpointStatusSchema = z.union([
    z.enum(["unconfirmed", "disabled", "pending"]),
    z.object({
        partiallyBound: z.object({
            application_count: z.int(),
        }),
    }),
    z.object({
        bound: z.object({
            application_count: z.int(),
        }),
    }),
    z.object({
        failed: z.object({
            message: z.string(),
        }),
    }),
]);

const SourceExcerptLineSchema = z.object({
    line: z.int(),
    text: z.string(),
});

const SourceExcerptSchema = z.object({
    breadcrumb: z.union([
        z.string(),
        z.null(),
    ]).optional(),
    currentLine: z.int(),
    highlightLength: z.int(),
    highlightStart: z.int(),
    lines: z.array(SourceExcerptLineSchema),
    sourceUrl: z.string(),
});

const TargetBreakpointStatusSchema = z.discriminatedUnion("kind", [
    z.object({
        kind: z.literal("waitingForScript"),
    }),
    z.object({
        diagnostics: z.array(z.string()),
        kind: z.literal("sourceNotFound"),
    }),
    z.object({
        candidates: z.array(BreakpointSourceCandidateSnapshotSchema),
        kind: z.literal("ambiguousSource"),
        omitted_candidate_count: z.int(),
    }),
    z.object({
        diagnostics: z.array(z.string()),
        kind: z.literal("unmapped"),
    }),
    z.object({
        kind: z.literal("applicable"),
        mapping_count: z.int(),
    }),
    z.object({
        application_count: z.int(),
        kind: z.literal("installing"),
    }),
    z.object({
        binding_count: z.int(),
        kind: z.literal("installed"),
    }),
    z.object({
        kind: z.literal("failed"),
        message: z.string(),
    }),
]);

const TargetBreakpointSnapshotSchema = z.object({
    applications: z.array(BreakpointApplicationSnapshotSchema).optional(),
    assessments: z.array(BreakpointScriptAssessmentSnapshotSchema).optional(),
    column: z.int(),
    id: z.string(),
    line: z.int(),
    source: z.union([
        SourceExcerptSchema,
        z.null(),
    ]).optional(),
    sourceUrl: z.string(),
    status: TargetBreakpointStatusSchema,
});

const BreakpointSnapshotSchema = z.object({
    applications: z.array(BreakpointApplicationSnapshotSchema).optional(),
    column: z.int(),
    condition: z.union([
        z.string(),
        z.null(),
    ]).optional(),
    enabled: z.boolean(),
    id: z.string(),
    line: z.int(),
    pendingReason: z.union([
        BreakpointPendingReasonSchema,
        z.null(),
    ]).optional(),
    sourcePath: z.string(),
    status: BreakpointStatusSchema,
    targetSelector: z.union([
        z.string(),
        z.null(),
    ]).optional(),
    targets: z.array(TargetBreakpointSnapshotSchema).optional(),
});

const BreakpointSpecSchema = z.object({
    column: z.int(),
    condition: z.union([
        z.string(),
        z.null(),
    ]).optional(),
    enabled: z.boolean(),
    line: z.int(),
    sourcePath: z.string(),
    targetSelector: z.union([
        z.string(),
        z.null(),
    ]).optional(),
});

const TargetSnapshotSchema = z.object({
    attached: z.boolean(),
    browserContextId: z.union([
        z.string(),
        z.null(),
    ]).optional(),
    openerId: z.union([
        z.string(),
        z.null(),
    ]).optional(),
    parentId: z.union([
        z.string(),
        z.null(),
    ]).optional(),
    subtype: z.union([
        z.string(),
        z.null(),
    ]).optional(),
    targetId: z.string(),
    targetType: z.string(),
    title: z.string(),
    url: z.string(),
});

const CanonicalTargetSnapshotSchema = z.object({
    connectionGeneration: z.int(),
    connectionId: z.string(),
    contextId: z.string(),
    resourceId: z.string(),
    target: TargetSnapshotSchema,
    targetId: z.string(),
});

const CaptureKindSchema = z.enum(["coverage", "cpuProfile", "heapSnapshot"]);

const CaptureSnapshotSchema = z.object({
    connectionGeneration: z.int(),
    connectionId: z.string(),
    contextId: z.string(),
    kind: CaptureKindSchema,
    name: z.string(),
    storageId: z.string(),
    targetId: z.string(),
});

const CdpStdioTopologySchema = z.enum(["browser", "target"]);

const SourceSuffixRewriteSnapshotSchema = z.object({
    from: z.string(),
    to: z.string(),
});

const CompactedSourceEdgeSnapshotSchema = z.object({
    basis: z.int(),
    derived: z.int(),
    fanOut: z.boolean().optional(),
    kind: z.string(),
    mappingCount: z.int(),
    suffixRewrite: z.union([
        SourceSuffixRewriteSnapshotSchema,
        z.null(),
    ]).optional(),
});

const CompactedSourceNodeSnapshotSchema = z.object({
    id: z.int(),
    listedSourcePaths: z.array(z.string()).optional(),
    prefix: z.string(),
    runtimeInternal: z.boolean(),
    snapshotCount: z.int().optional(),
    sourceCount: z.int(),
});

const CompactedSourceGraphSnapshotSchema = z.object({
    edges: z.array(CompactedSourceEdgeSnapshotSchema),
    nodes: z.array(CompactedSourceNodeSnapshotSchema),
    roots: z.array(z.int()),
});

const PlaywrightChannelSchema = z.enum(["bundled", "chrome", "chromeBeta", "chromeDev", "chromeCanary", "msedge", "msedgeBeta", "msedgeDev", "msedgeCanary"]);

const ConnectionConfigurationSchema = z.discriminatedUnion("kind", [
    z.object({
        endpoint: z.string(),
        kind: z.literal("directCdp"),
    }),
    z.object({
        endpoint: z.string(),
        kind: z.literal("nodeInspector"),
    }),
    z.object({
        kind: z.literal("process"),
        processId: z.int(),
    }),
    z.object({
        kind: z.literal("processTree"),
        rootPid: z.int(),
    }),
    z.object({
        kind: z.literal("scopedProcessTree"),
        rootPid: z.int(),
        targetId: z.string(),
    }).describe("Uses the process tree rooted at `root_pid` as the access path while exposing only `target_id` and its descendants as this connection's public target scope."),
    z.object({
        channel: PlaywrightChannelSchema,
        headless: z.boolean(),
        ignoreHttpsErrors: z.boolean().optional(),
        kind: z.literal("playwright"),
        playwrightPackage: z.union([
            z.string(),
            z.null(),
        ]).optional(),
        url: z.string(),
    }),
    z.object({
        args: z.array(z.string()),
        executable: z.string(),
        headless: z.boolean(),
        kind: z.literal("chrome"),
        url: z.string(),
        userDataDir: z.union([
            z.string(),
            z.null(),
        ]).optional(),
    }),
    z.object({
        args: z.array(z.string()),
        cwd: z.string(),
        env: z.looseObject({}).catchall(z.string()),
        kind: z.literal("node"),
        program: z.string(),
        runtimeArgs: z.array(z.string()).optional(),
        runtimeExecutable: z.string(),
    }),
    z.object({
        args: z.array(z.string()),
        command: z.string(),
        cwd: z.string(),
        env: z.looseObject({}).catchall(z.string()),
        kind: z.literal("stdio"),
        topology: CdpStdioTopologySchema.optional(),
    }),
]);

const ConnectionStatusSchema = z.discriminatedUnion("kind", [
    z.object({
        kind: z.literal("disconnected"),
    }),
    z.object({
        kind: z.literal("connecting"),
    }),
    z.object({
        kind: z.literal("disconnecting"),
    }),
    z.object({
        kind: z.literal("connected"),
        product: z.string(),
        protocolVersion: z.string(),
    }),
    z.object({
        kind: z.literal("failed"),
        message: z.string(),
    }),
]);

const ConnectionSnapshotSchema = z.object({
    configuration: ConnectionConfigurationSchema,
    generation: z.int(),
    id: z.string(),
    status: ConnectionStatusSchema,
    targets: z.array(TargetSnapshotSchema),
});

const ConsoleMessageSnapshotSchema = z.object({
    index: z.int(),
    params: z.unknown().optional(),
    values: z.array(z.string()),
});

const ContextEventSnapshotSchema = z.object({
    kind: z.string(),
    revision: z.int(),
    subjectId: z.union([
        z.string(),
        z.null(),
    ]).optional(),
});

const ContextKindSchema = z.enum(["path", "named"]);

const SourceFormattingModeSchema = z.enum(["off", "auto", "on"]);

const SourceFormattingRuleSchema = z.object({
    id: z.string(),
    mode: SourceFormattingModeSchema,
    targetPattern: z.union([
        z.string(),
        z.null(),
    ]).optional(),
    urlPattern: z.union([
        z.string(),
        z.null(),
    ]).optional(),
});

const SourceFormattingSettingsSchema = z.object({
    defaultMode: SourceFormattingModeSchema,
    rules: z.array(SourceFormattingRuleSchema),
});

const TargetAttachmentStateSchema = z.enum(["detached", "external", "debugger"]);

const TargetNodeSnapshotSchema = z.object({
    attachment: TargetAttachmentStateSchema,
    connectionGeneration: z.int(),
    connectionId: z.string(),
    parentTargetId: z.union([
        z.string(),
        z.null(),
    ]).optional(),
    target: TargetSnapshotSchema,
});

const ContextSnapshotSchema = z.object({
    agentInstanceId: z.string(),
    breakpoints: z.array(BreakpointSnapshotSchema),
    connections: z.array(ConnectionSnapshotSchema),
    displayName: z.string(),
    id: z.string(),
    resourceRevision: z.int().optional(),
    revision: z.int(),
    sourceFormatting: SourceFormattingSettingsSchema.optional(),
    targetForest: z.array(TargetNodeSnapshotSchema),
});

const ContextObservationSchema = z.object({
    events: z.array(ContextEventSnapshotSchema),
    snapshot: ContextSnapshotSchema,
});

const ContextSummarySchema = z.object({
    agentInstanceId: z.string(),
    breakpointCount: z.int(),
    connectionCount: z.int(),
    displayName: z.string(),
    id: z.string(),
    kind: ContextKindSchema,
    pathAncestor: z.union([
        z.boolean(),
        z.null(),
    ]).optional(),
    pathDistance: z.union([
        z.uint32(),
        z.null(),
    ]).optional(),
    revision: z.int(),
});

const CoverageAnalysisSnapshotSchema = z.object({
    durationMicros: z.int(),
    sourceMapCacheBypasses: z.int(),
    sourceMapCacheHits: z.int(),
    sourceMapCacheMisses: z.int(),
});

const SourceLocationSchema = z.object({
    column: z.int(),
    line: z.int(),
    sourceUrl: z.string(),
});

const CoverageRangeSnapshotSchema = z.object({
    authoredEnd: z.union([
        SourceLocationSchema,
        z.null(),
    ]).optional(),
    authoredStart: z.union([
        SourceLocationSchema,
        z.null(),
    ]).optional(),
    count: z.int(),
    endOffset: z.int(),
    startOffset: z.int(),
});

const CoverageFunctionSnapshotSchema = z.object({
    authoredLocation: z.union([
        SourceLocationSchema,
        z.null(),
    ]).optional(),
    blockCoverage: z.boolean(),
    breadcrumb: z.union([
        z.string(),
        z.null(),
    ]).optional(),
    effectiveRanges: z.array(CoverageRangeSnapshotSchema).optional(),
    generatedLocation: z.union([
        SourceLocationSchema,
        z.null(),
    ]).optional(),
    name: z.string(),
    ranges: z.array(CoverageRangeSnapshotSchema),
    rootEndOffset: z.int(),
    rootStartOffset: z.int(),
});

const CoverageSourceSnapshotSchema = z.object({
    associatedAuthoredSource: z.union([
        z.string(),
        z.null(),
    ]).optional(),
    functions: z.array(CoverageFunctionSnapshotSchema),
    generatedUrl: z.string(),
    scriptId: z.string(),
});

const CoverageSnapshotSchema = z.object({
    analysis: z.union([
        CoverageAnalysisSnapshotSchema,
        z.null(),
    ]).optional(),
    captureId: z.union([
        z.string(),
        z.null(),
    ]).optional(),
    sources: z.array(CoverageSourceSnapshotSchema),
    timestampMicros: z.int(),
});

const CpuProfileAnalysisSnapshotSchema = z.object({
    durationMicros: z.int(),
    sourceMapCacheBypasses: z.int(),
    sourceMapCacheHits: z.int(),
    sourceMapCacheMisses: z.int(),
});

const CpuProfileCallFrameSnapshotSchema = z.object({
    columnNumber: z.int(),
    functionName: z.string(),
    lineNumber: z.int(),
    scriptId: z.string(),
    url: z.string(),
});

const CpuProfileFunctionSnapshotSchema = z.object({
    authoredLocation: z.union([
        SourceLocationSchema,
        z.null(),
    ]).optional(),
    breadcrumb: z.union([
        z.string(),
        z.null(),
    ]).optional(),
    generatedLocation: SourceLocationSchema,
    name: z.string(),
    sampleCount: z.int(),
    selfTimeMicros: z.int(),
    totalTimeMicros: z.int(),
});

const CpuProfilePositionTickSnapshotSchema = z.object({
    line: z.int(),
    ticks: z.int(),
});

const CpuProfileNodeSnapshotSchema = z.object({
    authoredLocation: z.union([
        SourceLocationSchema,
        z.null(),
    ]).optional(),
    breadcrumb: z.union([
        z.string(),
        z.null(),
    ]).optional(),
    callFrame: CpuProfileCallFrameSnapshotSchema,
    children: z.array(z.int()),
    deoptReason: z.union([
        z.string(),
        z.null(),
    ]).optional(),
    hitCount: z.union([
        z.int(),
        z.null(),
    ]).optional(),
    id: z.int(),
    positionTicks: z.array(CpuProfilePositionTickSnapshotSchema),
    sampleCount: z.int(),
    selfTimeMicros: z.int(),
    totalTimeMicros: z.int(),
});

const CpuProfileSnapshotSchema = z.object({
    analysis: z.union([
        CpuProfileAnalysisSnapshotSchema,
        z.null(),
    ]).optional(),
    captureId: z.string(),
    endTimeMicros: z.number(),
    functions: z.array(CpuProfileFunctionSnapshotSchema).optional(),
    nodes: z.array(CpuProfileNodeSnapshotSchema),
    samples: z.array(z.int()),
    samplingIntervalMicros: z.union([
        z.int().nonnegative(),
        z.null(),
    ]).optional(),
    startTimeMicros: z.number(),
    /**
     * Raw CDP timestamp differences in sample order, which need not be chronological.
     */
    timeDeltasMicros: z.array(z.int()).describe("Raw CDP timestamp differences in sample order, which need not be chronological."),
});

/**
 * Best available source coordinates. Both locations use 1-based lines and UTF-16 columns; URLs are never shortened for display.
 */
const ResolvedSourcePositionSchema = z.object({
    breadcrumb: z.union([
        z.string(),
        z.null(),
    ]).optional(),
    diagnostic: z.union([
        z.string(),
        z.null(),
    ]).optional(),
    generated: SourceLocationSchema,
    mapping: z.string(),
    resolved: SourceLocationSchema,
}).describe("Best available source coordinates. Both locations use 1-based lines and UTF-16 columns; URLs are never shortened for display.");

const ObjectLocationSnapshotSchema = z.object({
    kind: z.string(),
    origin: z.string(),
    position: ResolvedSourcePositionSchema,
    scriptId: z.string(),
});

const ObjectSourceSnapshotSchema = z.object({
    diagnostics: z.array(z.string()),
    locations: z.array(ObjectLocationSnapshotSchema),
});

const ValuePreviewSnapshotSchema = z.object({
    kind: z.string(),
    preview: z.union([
        z.string(),
        z.null(),
    ]).optional(),
    reference: z.union([
        z.string(),
        z.null(),
    ]).optional(),
    source: ObjectSourceSnapshotSchema.optional(),
    truncated: z.boolean(),
});

const EvaluationSnapshotSchema = z.object({
    description: z.union([
        z.string(),
        z.null(),
    ]).optional(),
    expression: z.string(),
    kind: z.string(),
    objectId: z.union([
        z.string(),
        z.null(),
    ]).optional(),
    preview: ValuePreviewSnapshotSchema,
    unserializableValue: z.union([
        z.string(),
        z.null(),
    ]).optional(),
    value: z.unknown().optional(),
});

const FrameProjectionSnapshotSchema = z.discriminatedUnion("kind", [
    z.object({
        kind: z.literal("raw"),
    }),
    z.object({
        kind: z.literal("pending"),
    }),
    z.object({
        kind: z.literal("resolved"),
        location: SourceLocationSchema,
    }),
    z.object({
        kind: z.literal("failed"),
        message: z.string(),
    }),
]);

const ScopeSnapshotSchema = z.object({
    index: z.int(),
    kind: z.string(),
    name: z.union([
        z.string(),
        z.null(),
    ]).optional(),
});

const FrameSnapshotSchema = z.object({
    breadcrumb: z.union([
        z.string(),
        z.null(),
    ]).optional(),
    functionName: z.string(),
    index: z.int(),
    projected: FrameProjectionSnapshotSchema,
    raw: SourceLocationSchema,
    scopes: z.array(ScopeSnapshotSchema),
});

const HeapAggregateBySchema = z.enum(["nodeType", "name", "stringValue"]);

const HeapAggregateEntrySnapshotSchema = z.object({
    count: z.int(),
    key: z.string(),
    keyTruncated: z.boolean(),
    shallowSize: z.int(),
});

const HeapAggregateSnapshotSchema = z.object({
    by: HeapAggregateBySchema,
    captureId: z.string(),
    entries: z.array(HeapAggregateEntrySnapshotSchema),
    incompleteStringCount: z.int().optional(),
    omittedEntryCount: z.int(),
});

const HeapSnapshotTimingSchema = z.object({
    retrievingDurationMicros: z.int(),
    takingDurationMicros: z.int(),
});

const HeapCaptureResultSchema = z.object({
    bytesWritten: z.int(),
    captureId: z.string(),
    timing: HeapSnapshotTimingSchema,
});

const HeapMappingStatusSchema = z.enum(["notAttempted", "noMapSupplied", "mapLoadingFailed", "mapped"]);

const ScriptProvenanceSchema = z.object({
    executionContextAuxData: z.unknown().optional(),
    executionContextId: z.union([
        z.int(),
        z.null(),
    ]).optional(),
    frameId: z.union([
        z.string(),
        z.null(),
    ]).optional(),
});

const HeapScriptMappingDiagnosticSchema = z.object({
    diagnostic: z.union([
        z.string(),
        z.null(),
    ]).optional(),
    hash: z.string(),
    provenance: ScriptProvenanceSchema,
    scriptId: z.string(),
    status: HeapMappingStatusSchema,
    url: z.string(),
});

const HeapClassAnalysisSnapshotSchema = z.object({
    constructorGroupCount: z.int(),
    mappingStatus: HeapMappingStatusSchema.optional(),
    parseDurationMicros: z.int(),
    projectionDurationMicros: z.int(),
    scriptMappings: z.array(HeapScriptMappingDiagnosticSchema).optional(),
    snapshotTiming: z.union([
        HeapSnapshotTimingSchema,
        z.null(),
    ]).optional(),
    sourceMapHydrationDurationMicros: z.int(),
    usedCachedGroups: z.boolean(),
});

const HeapInstanceSnapshotSchema = z.object({
    alias: z.string(),
    heapObjectId: z.string(),
    shallowSize: z.int(),
});

const HeapClassSnapshotEntrySchema = z.object({
    generatedName: z.string(),
    instanceCount: z.int(),
    instances: z.array(HeapInstanceSnapshotSchema),
    location: SourceLocationSchema,
    name: z.string(),
    omittedInstanceCount: z.int(),
    provenance: ScriptProvenanceSchema.optional(),
    scriptId: z.string().optional(),
    shallowSize: z.int(),
    sourceUrl: z.string(),
});

const HeapClassSnapshotSchema = z.object({
    analysis: HeapClassAnalysisSnapshotSchema,
    captureId: z.string(),
    classes: z.array(HeapClassSnapshotEntrySchema),
    totalInstances: z.int(),
    totalShallowSize: z.int(),
});

const HeapDiffEntrySnapshotSchema = z.object({
    countDelta: z.int(),
    key: z.string(),
    keyTruncated: z.boolean(),
    shallowSizeDelta: z.int(),
});

const HeapDiffSnapshotSchema = z.object({
    by: HeapAggregateBySchema,
    entries: z.array(HeapDiffEntrySnapshotSchema),
    newerCaptureId: z.string(),
    newerIncompleteStringCount: z.int().optional(),
    olderCaptureId: z.string(),
    olderIncompleteStringCount: z.int().optional(),
});

const HeapNodeLocationSnapshotSchema = z.object({
    column: z.int(),
    line: z.int(),
    scriptId: z.int(),
});

const HeapNodeSnapshotSchema = z.object({
    heapObjectId: z.string(),
    immediateDominator: z.union([
        z.string(),
        z.null(),
    ]).optional(),
    incomingReferenceCount: z.int(),
    locations: z.array(HeapNodeLocationSnapshotSchema),
    name: z.string(),
    nodeIndex: z.int(),
    nodeType: z.string(),
    outgoingReferenceCount: z.int(),
    preview: z.union([
        z.string(),
        z.null(),
    ]).optional(),
    reference: z.string(),
    retainedSize: z.union([
        z.int().nonnegative(),
        z.null(),
    ]).optional(),
    shallowSize: z.int(),
    source: ObjectSourceSnapshotSchema.optional(),
    stringTruncated: z.boolean(),
    stringValue: z.union([
        z.string(),
        z.null(),
    ]).optional(),
});

const HeapDominatorSnapshotSchema = z.object({
    captureId: z.string(),
    chain: z.array(HeapNodeSnapshotSchema),
    node: HeapNodeSnapshotSchema,
});

const HeapEdgePolicySchema = z.enum(["strong", "all"]);

const HeapNodeSelectionSnapshotSchema = z.object({
    captureId: z.string(),
    graphParseDurationMicros: z.int(),
    incompleteStringCount: z.int().optional(),
    nodes: z.array(HeapNodeSnapshotSchema),
    totalEdges: z.int(),
    totalNodes: z.int(),
    usedCachedGraph: z.boolean(),
});

const HeapNodeSelectorSchema = z.object({
    heapObjectId: z.union([
        z.string(),
        z.null(),
    ]).optional(),
    limit: z.union([
        z.uint32(),
        z.null(),
    ]).optional(),
    maxShallowSize: z.union([
        z.int().nonnegative(),
        z.null(),
    ]).optional(),
    minShallowSize: z.union([
        z.int().nonnegative(),
        z.null(),
    ]).optional(),
    name: z.union([
        z.string(),
        z.null(),
    ]).optional(),
    nameRegex: z.union([
        z.string(),
        z.null(),
    ]).optional(),
    nodeType: z.union([
        z.string(),
        z.null(),
    ]).optional(),
    stringContains: z.union([
        z.string(),
        z.null(),
    ]).optional(),
    stringRegex: z.union([
        z.string(),
        z.null(),
    ]).optional(),
});

const HeapPathCostSchema = z.enum(["edges", "readable"]);

const HeapPathDirectionSchema = z.enum(["outgoing", "incoming", "either"]);

const HeapPathOptionsSchema = z.object({
    cost: HeapPathCostSchema,
    direction: HeapPathDirectionSchema,
    edgePolicy: HeapEdgePolicySchema,
});

const HeapTraversalDirectionSchema = z.enum(["outgoing", "incoming"]);

const HeapPathStepSnapshotSchema = z.object({
    direction: HeapTraversalDirectionSchema,
    edgeIndex: z.int(),
    edgeType: z.string(),
    from: z.string(),
    name: z.union([
        z.string(),
        z.null(),
    ]).optional(),
    nameOrIndex: z.int(),
    to: z.string(),
});

const HeapPathSnapshotSchema = z.object({
    captureId: z.string(),
    cost: z.int(),
    from: z.string(),
    nodes: z.array(HeapNodeSnapshotSchema),
    steps: z.array(HeapPathStepSnapshotSchema),
    to: z.string(),
});

const HeapReferenceDirectionSchema = z.enum(["incoming", "outgoing", "both"]);

const HeapReferenceSnapshotSchema = z.object({
    edgeIndex: z.int(),
    edgeType: z.string(),
    name: z.union([
        z.string(),
        z.null(),
    ]).optional(),
    nameOrIndex: z.int(),
    source: z.string(),
    sourceLocations: ObjectSourceSnapshotSchema.optional(),
    sourcePreview: z.union([
        z.string(),
        z.null(),
    ]).optional(),
    target: z.string(),
    targetLocations: ObjectSourceSnapshotSchema.optional(),
    targetPreview: z.union([
        z.string(),
        z.null(),
    ]).optional(),
});

const HeapReferencesSnapshotSchema = z.object({
    captureId: z.string(),
    direction: HeapReferenceDirectionSchema,
    edgePolicy: HeapEdgePolicySchema,
    node: HeapNodeSnapshotSchema,
    omittedReferenceCount: z.int(),
    references: z.array(HeapReferenceSnapshotSchema),
});

const HeapSnapshotProgressSchema = z.object({
    bytesWritten: z.int(),
    done: z.int(),
    finished: z.union([
        z.boolean(),
        z.null(),
    ]).optional(),
    total: z.int(),
});

const HeapSnapshotResultSchema = z.object({
    bytesWritten: z.int(),
    path: z.string(),
    timing: HeapSnapshotTimingSchema,
});

const HeapSourceMapSupplySchema = z.object({
    scriptHash: z.string(),
    scriptId: z.string(),
    sourceMap: z.string(),
    sourceMapUrl: z.string(),
});

const LogCaptureStatusSchema = z.enum(["active", "inactive", "stopped", "unknown"]);

const LogCaptureSnapshotSchema = z.object({
    captureId: z.union([
        z.string(),
        z.null(),
    ]).optional(),
    collectedEvents: z.array(z.string()),
    droppedCount: z.union([
        z.int().nonnegative(),
        z.null(),
    ]).optional(),
    evictedCount: z.union([
        z.int().nonnegative(),
        z.null(),
    ]).optional(),
    sessionId: z.union([
        z.string(),
        z.null(),
    ]).optional(),
    startedAtUnixMs: z.union([
        z.int().nonnegative(),
        z.null(),
    ]).optional(),
    status: LogCaptureStatusSchema,
});

const LogpointSpecSchema = z.object({
    column: z.int(),
    expression: z.string(),
    id: z.string(),
    line: z.int(),
    sourceUrl: z.string(),
});

const MutationOptionsSchema = z.object({
    expectedRevision: z.union([
        z.int().nonnegative(),
        z.null(),
    ]).optional(),
    requestId: z.union([
        z.string(),
        z.null(),
    ]).optional(),
});

const ObservationCursorSchema = z.discriminatedUnion("kind", [
    z.object({
        kind: z.literal("current"),
    }),
    z.object({
        kind: z.literal("after"),
        revision: z.int(),
    }),
]);

const ObservationResultSchema = z.discriminatedUnion("kind", [
    z.object({
        items: z.array(ContextObservationSchema),
        kind: z.literal("items"),
    }),
    z.object({
        current: ContextSnapshotSchema,
        kind: z.literal("historyGap"),
        oldest_available_revision: z.int(),
        requested_revision: z.int(),
    }),
]);

const PauseSnapshotSchema = z.object({
    epoch: z.int(),
    frames: z.array(FrameSnapshotSchema),
    reason: z.string(),
    source: z.union([
        SourceExcerptSchema,
        z.null(),
    ]).optional(),
});

const PlaywrightProxyEndpointSchema = z.object({
    connectionGeneration: z.int(),
    id: z.string(),
    websocketUrl: z.string(),
});

const ProcessRoleSchema = z.enum(["vscode-main", "electron-main", "browser-main", "renderer", "extension-host", "node-utility", "node", "type-script-server", "type-script-installer", "language-server", "pty-host", "file-watcher", "agent-host", "copilot", "claude", "codex", "agent", "gpu", "network-service", "audio-service", "crashpad", "utility", "other"]);

const ProcessRootKindSchema = z.enum(["vscode", "node", "electron", "browser"]);

const ProcessSnapshotSchema = z.object({
    agentSessions: z.array(AgentSessionSnapshotSchema).optional(),
    attachable: z.boolean().optional(),
    commandLine: z.string(),
    cpuPercent: z.union([
        z.uint32(),
        z.null(),
    ]).optional(),
    creationDate: z.string(),
    debugTargetId: z.union([
        z.string(),
        z.null(),
    ]).optional(),
    displayName: z.union([
        z.string(),
        z.null(),
    ]).optional(),
    memoryBytes: z.union([
        z.int().nonnegative(),
        z.null(),
    ]).optional(),
    name: z.string(),
    parentProcessId: z.union([
        z.uint32(),
        z.null(),
    ]).optional(),
    processId: z.int(),
    role: ProcessRoleSchema,
    windowId: z.union([
        z.uint32(),
        z.null(),
    ]).optional(),
    windowTitle: z.union([
        z.string(),
        z.null(),
    ]).optional(),
});

const ProcessTargetSnapshotSchema = z.object({
    attached: z.boolean(),
    browserContextId: z.union([
        z.string(),
        z.null(),
    ]).optional(),
    openerId: z.union([
        z.string(),
        z.null(),
    ]).optional(),
    parentId: z.union([
        z.string(),
        z.null(),
    ]).optional(),
    processId: z.union([
        z.uint32(),
        z.null(),
    ]).optional(),
    subtype: z.union([
        z.string(),
        z.null(),
    ]).optional(),
    targetId: z.string(),
    targetType: z.string(),
    title: z.string(),
    url: z.string(),
});

const ProcessTreeSnapshotSchema = z.object({
    processes: z.array(ProcessSnapshotSchema),
    rootKind: ProcessRootKindSchema.optional(),
    rootProcessId: z.int(),
    runtimeMetadataAvailable: z.boolean(),
    targetDiscoveryError: z.union([
        z.string(),
        z.null(),
    ]).optional(),
    targets: z.array(ProcessTargetSnapshotSchema).optional(),
    /**
     * Whether runtime children were queried for this root. An empty `targets` collection is only authoritative when this is true.
     */
    targetsObserved: z.boolean().describe("Whether runtime children were queried for this root. An empty `targets` collection is only authoritative when this is true.").optional(),
});

const PromiseClassificationSchema = z.literal("indeterminate");

const PromiseOriginSchema = z.enum(["live", "heapSnapshot"]);

const PromiseStateSchema = z.enum(["pending", "fulfilled", "rejected", "unknown"]);

const PromiseSnapshotSchema = z.object({
    classification: PromiseClassificationSchema,
    origin: PromiseOriginSchema,
    reference: z.union([
        z.string(),
        z.null(),
    ]).optional(),
    retained: z.union([
        z.boolean(),
        z.null(),
    ]).optional(),
    settlement: z.union([
        ValuePreviewSnapshotSchema,
        z.null(),
    ]).optional(),
    state: PromiseStateSchema,
});

const PromiseSelectionSnapshotSchema = z.object({
    captureId: z.string(),
    graphParseDurationMicros: z.int(),
    omittedPromiseCount: z.int(),
    promises: z.array(PromiseSnapshotSchema),
    totalPromises: z.int(),
    usedCachedGraph: z.boolean(),
});

/**
 * A short-lived, authenticated loopback CDP endpoint exposed by `dbgjs context relay` or `dbgjs target relay`. `id` identifies the relay for `close_relay`; `websocket_url` carries its own random capability token and must not be reused once the relay closes.
 */
const RelayEndpointSchema = z.object({
    id: z.string(),
    websocketUrl: z.string(),
}).describe("A short-lived, authenticated loopback CDP endpoint exposed by `dbgjs context relay` or `dbgjs target relay`. `id` identifies the relay for `close_relay`; `websocket_url` carries its own random capability token and must not be reused once the relay closes.");

const ResourceCapabilitySnapshotSchema = z.object({
    detail: z.looseObject({}),
    handle: z.int(),
    kind: z.string(),
    source: z.string(),
    title: z.string(),
});

const ResourceFrontierSnapshotSchema = z.object({
    relation: z.string(),
    state: z.unknown(),
});

const ResourceRelationSnapshotSchema = z.object({
    contributors: z.array(z.string()),
    from: z.string(),
    kind: z.string(),
    to: z.string(),
});

const ResourceSnapshotSchema = z.object({
    attributes: z.looseObject({}),
    capabilities: z.array(ResourceCapabilitySnapshotSchema),
    contributors: z.array(z.string()),
    frontiers: z.array(ResourceFrontierSnapshotSchema),
    id: z.string(),
    kinds: z.array(z.string()),
    label: z.union([
        z.string(),
        z.null(),
    ]).optional(),
});

const ResourceGraphSnapshotSchema = z.object({
    relations: z.array(ResourceRelationSnapshotSchema),
    resources: z.array(ResourceSnapshotSchema),
    revision: z.int(),
});

const ScreenshotSnapshotSchema = z.object({
    dataBase64: z.string(),
    mediaType: z.string(),
});

const ServiceInfoSchema = z.object({
    agentInstanceId: z.string(),
    processId: z.int(),
});

const SourceContentSnapshotSchema = z.object({
    content: z.string(),
    endLine: z.int(),
    path: z.string(),
    startLine: z.int(),
    totalLines: z.int(),
});

const SourceViewPreferenceSchema = z.enum(["policy", "original", "formatted"]);

const SourceDisplayOptionsSchema = z.object({
    contextLines: z.int(),
    line: z.union([
        z.uint32(),
        z.null(),
    ]).optional(),
    view: SourceViewPreferenceSchema.optional(),
});

const SourceProjectionPathSnapshotSchema = z.object({
    generatedUrl: z.string(),
    steps: z.array(z.string()),
});

const SourceGraphViewSnapshotSchema = z.object({
    alternativeProvenance: z.array(z.string()),
    connectionId: z.string(),
    diagnostics: z.array(z.string()),
    generatedUrl: z.string(),
    kind: z.string(),
    primaryProvenance: z.string(),
    projectionPaths: z.array(SourceProjectionPathSnapshotSchema),
    resolvedSourceCount: z.int(),
    role: z.string(),
    sourcePath: z.string(),
    targetId: z.string(),
});

const SourceMappingSnapshotSchema = z.object({
    column: z.int(),
    connectionId: z.string(),
    direction: z.string(),
    line: z.int(),
    quality: z.string(),
    sourceUrl: z.string(),
    targetId: z.string(),
});

const SourceMatchSnapshotSchema = z.object({
    afterContext: z.array(z.string()),
    beforeContext: z.array(z.string()),
    column: z.int(),
    connectionId: z.union([
        z.string(),
        z.null(),
    ]).optional(),
    contentHash: z.string(),
    kind: z.string(),
    line: z.int(),
    matchLength: z.int(),
    path: z.string(),
    provenance: z.string(),
    targetId: z.union([
        z.string(),
        z.null(),
    ]).optional(),
    text: z.string(),
});

const SourceSearchOptionsSchema = z.object({
    caseSensitive: z.boolean(),
    contextLines: z.int(),
    maxResults: z.int(),
    path: z.union([
        z.string(),
        z.null(),
    ]).optional(),
    pattern: z.string(),
    regex: z.boolean(),
    timeoutMs: z.union([
        z.int().nonnegative(),
        z.null(),
    ]).optional(),
    view: SourceViewPreferenceSchema.optional(),
});

const SourceSearchSkipSchema = z.object({
    connectionId: z.union([
        z.string(),
        z.null(),
    ]).optional(),
    kind: z.string(),
    path: z.string(),
    reason: z.string(),
    targetId: z.union([
        z.string(),
        z.null(),
    ]).optional(),
});

const SourceSearchSnapshotSchema = z.object({
    matches: z.array(SourceMatchSnapshotSchema),
    omittedMatches: z.int(),
    searchedContents: z.int(),
    searchedSources: z.int(),
    skipped: z.array(SourceSearchSkipSchema).optional(),
    skippedSources: z.int(),
});

const SourceSnapshotInfoSchema = z.object({
    connectionId: z.union([
        z.string(),
        z.null(),
    ]).optional(),
    kind: z.string(),
    path: z.string(),
    sourceMapUrl: z.union([
        z.string(),
        z.null(),
    ]).optional(),
    status: z.string(),
    targetId: z.union([
        z.string(),
        z.null(),
    ]).optional(),
});

const SourceTreeKindSchema = z.enum(["loaded", "sourceMapped", "formatted", "resolved"]);

const UncompactedSourceRevisionSnapshotSchema = z.discriminatedUnion("kind", [
    z.object({
        hash: z.string(),
        kind: z.literal("content"),
    }),
    z.object({
        kind: z.literal("version"),
        namespace: z.string(),
        value: z.string(),
    }),
]);

const UncompactedSourceNodeSnapshotSchema = z.object({
    id: z.int(),
    revision: UncompactedSourceRevisionSnapshotSchema,
    uri: z.string(),
});

const SourceTreeSnapshotSchema = z.object({
    kind: SourceTreeKindSchema,
    sources: z.array(UncompactedSourceNodeSnapshotSchema),
});

const StepKindSchema = z.enum(["into", "over", "out"]);

const TargetAttachOptionsSchema = z.object({
    expectedConnectionGeneration: z.union([
        z.int().nonnegative(),
        z.null(),
    ]).optional(),
    force: z.boolean().optional(),
});

const TargetAttachmentOutcomeSchema = z.enum(["created", "stolen"]);

const TargetDebuggerPhaseSchema = z.discriminatedUnion("kind", [
    z.object({
        kind: z.literal("running"),
    }),
    z.object({
        epoch: z.int(),
        kind: z.literal("paused"),
    }),
    z.object({
        epoch: z.int(),
        kind: z.literal("resuming"),
    }),
    z.object({
        kind: z.literal("failed"),
        message: z.string(),
    }),
]);

const TargetScriptStatusSchema = z.discriminatedUnion("kind", [
    z.object({
        kind: z.literal("unresolved"),
    }),
    z.object({
        kind: z.literal("pending"),
    }),
    z.object({
        authored_sources: z.array(z.string()),
        kind: z.literal("resolved"),
    }),
    z.object({
        kind: z.literal("failed"),
        message: z.string(),
    }),
]);

const TargetScriptSnapshotSchema = z.object({
    sourceMapUrl: z.union([
        z.string(),
        z.null(),
    ]).optional(),
    status: TargetScriptStatusSchema,
    url: z.string(),
});

const TargetDebuggerSnapshotSchema = z.object({
    breakpoints: z.array(TargetBreakpointSnapshotSchema),
    connectionGeneration: z.int(),
    connectionId: z.string(),
    contextId: z.string(),
    logCapture: LogCaptureSnapshotSchema.optional(),
    logs: z.array(ConsoleMessageSnapshotSchema),
    pause: z.union([
        PauseSnapshotSchema,
        z.null(),
    ]).optional(),
    phase: TargetDebuggerPhaseSchema,
    revision: z.int(),
    scripts: z.array(TargetScriptSnapshotSchema),
    targetId: z.string(),
});

const TargetAttachmentResultSchema = z.object({
    outcome: TargetAttachmentOutcomeSchema,
    target: TargetDebuggerSnapshotSchema,
});

const TargetLogSnapshotSchema = z.object({
    capture: LogCaptureSnapshotSchema,
    connectionGeneration: z.int(),
    connectionId: z.string(),
    contextId: z.string(),
    messages: z.array(ConsoleMessageSnapshotSchema),
    targetId: z.string(),
});

const TargetWaitPredicateSchema = z.discriminatedUnion("kind", [
    z.object({
        afterRevision: z.int(),
        kind: z.literal("changed"),
    }),
    z.object({
        kind: z.literal("running"),
    }),
    z.object({
        breakpointId: z.string(),
        kind: z.literal("breakpointInstalled"),
    }),
    z.object({
        afterEpoch: z.int(),
        kind: z.literal("paused"),
    }),
]);

const UncompactedProjectionSnapshotSchema = z.discriminatedUnion("kind", [
    z.object({
        contentHash: z.string(),
        kind: z.literal("identityEqualContent"),
    }),
    z.object({
        kind: z.literal("identityDeclaredByProvider"),
        provider: z.string(),
    }),
    z.object({
        kind: z.literal("sourceMap"),
        mapHash: z.string(),
        sourceIndex: z.int(),
    }),
    z.object({
        formatter: z.string(),
        kind: z.literal("format"),
    }),
    z.object({
        edit: z.string(),
        kind: z.literal("edit"),
    }),
    z.object({
        columnDelta: z.int(),
        kind: z.literal("offset"),
        lineDelta: z.int(),
    }),
]);

const UncompactedSourceEdgeSnapshotSchema = z.object({
    basis: z.int(),
    derived: z.int(),
    id: z.int(),
    projection: UncompactedProjectionSnapshotSchema,
});

const UncompactedSourceGraphSnapshotSchema = z.object({
    edges: z.array(UncompactedSourceEdgeSnapshotSchema),
    nodes: z.array(UncompactedSourceNodeSnapshotSchema),
    roots: z.array(z.int()),
});

const ValueInspectionOptionsSchema = z.object({
    maxPreviewLength: z.int(),
    maxProperties: z.int(),
    retainReferences: z.boolean(),
});

const ValuePropertySnapshotSchema = z.object({
    name: z.string(),
    value: ValuePreviewSnapshotSchema,
});

const ValueSelectorSchema = z.discriminatedUnion("kind", [
    z.object({
        allowSideEffects: z.boolean(),
        expression: z.string(),
        kind: z.literal("expression"),
    }),
    z.object({
        kind: z.literal("remoteObject"),
        object_id: z.string(),
    }),
]);

const ValueSnapshotSchema = z.object({
    className: z.union([
        z.string(),
        z.null(),
    ]).optional(),
    omittedPropertyCount: z.int().optional(),
    preview: ValuePreviewSnapshotSchema,
    promise: z.union([
        PromiseSnapshotSchema,
        z.null(),
    ]).optional(),
    properties: z.array(ValuePropertySnapshotSchema),
    propertiesTruncated: z.boolean().optional(),
    selector: ValueSelectorSchema,
    subtype: z.union([
        z.string(),
        z.null(),
    ]).optional(),
});

const VariableSnapshotSchema = z.object({
    description: z.union([
        z.string(),
        z.null(),
    ]).optional(),
    kind: z.string(),
    name: z.string(),
    objectId: z.union([
        z.string(),
        z.null(),
    ]).optional(),
    preview: ValuePreviewSnapshotSchema,
    unserializableValue: z.union([
        z.string(),
        z.null(),
    ]).optional(),
    value: z.unknown().optional(),
});

const wireSchema: LinkRpcInterfaceSchema = JSON.parse("{\"id\":\"dev.dbgjs.cdp-debugger\",\"hash\":\"87cd6ce8c9f328fe\",\"methods\":{\"add_source_formatting_rule\":{\"params\":{\"type\":\"object\",\"required\":[\"contextId\",\"mode\"],\"properties\":{\"contextId\":{\"type\":\"string\"},\"mode\":{\"$ref\":\"#/components/schemas/SourceFormattingMode\"},\"targetPattern\":{\"type\":[\"string\",\"null\"]},\"urlPattern\":{\"type\":[\"string\",\"null\"]}},\"additionalProperties\":false},\"result\":{\"$ref\":\"#/components/schemas/SourceFormattingSettings\"}},\"aggregate_heap_snapshot\":{\"params\":{\"type\":\"object\",\"required\":[\"by\",\"captureId\",\"connectionId\",\"contextId\",\"limit\",\"targetId\"],\"properties\":{\"by\":{\"$ref\":\"#/components/schemas/HeapAggregateBy\"},\"captureId\":{\"type\":\"string\"},\"connectionId\":{\"type\":\"string\"},\"contextId\":{\"type\":\"string\"},\"limit\":{\"type\":\"integer\"},\"maxStringLength\":{\"type\":[\"integer\",\"null\"],\"format\":\"uint32\"},\"targetId\":{\"type\":\"string\"}},\"additionalProperties\":false},\"result\":{\"$ref\":\"#/components/schemas/HeapAggregateSnapshot\"}},\"attach_target\":{\"params\":{\"type\":\"object\",\"required\":[\"connectionId\",\"contextId\",\"options\",\"targetId\"],\"properties\":{\"connectionId\":{\"type\":\"string\"},\"contextId\":{\"type\":\"string\"},\"options\":{\"$ref\":\"#/components/schemas/TargetAttachOptions\"},\"targetId\":{\"type\":\"string\"}},\"additionalProperties\":false},\"result\":{\"$ref\":\"#/components/schemas/TargetAttachmentResult\"}},\"capture_heap_snapshot\":{\"params\":{\"type\":\"object\",\"required\":[\"captureNumericValue\",\"connectionId\",\"contextId\",\"exposeInternals\",\"targetId\"],\"properties\":{\"captureId\":{\"type\":[\"string\",\"null\"]},\"captureNumericValue\":{\"type\":\"boolean\"},\"connectionId\":{\"type\":\"string\"},\"contextId\":{\"type\":\"string\"},\"exposeInternals\":{\"type\":\"boolean\"},\"targetId\":{\"type\":\"string\"}},\"additionalProperties\":false},\"result\":{\"$ref\":\"#/components/schemas/HeapCaptureResult\"},\"serverStream\":{\"$ref\":\"#/components/schemas/HeapSnapshotProgress\"}},\"capture_screenshot\":{\"params\":{\"type\":\"object\",\"required\":[\"connectionId\",\"contextId\",\"targetId\"],\"properties\":{\"connectionId\":{\"type\":\"string\"},\"contextId\":{\"type\":\"string\"},\"targetId\":{\"type\":\"string\"}},\"additionalProperties\":false},\"result\":{\"$ref\":\"#/components/schemas/ScreenshotSnapshot\"}},\"click_target\":{\"params\":{\"type\":\"object\",\"required\":[\"connectionId\",\"contextId\",\"selector\",\"targetId\"],\"properties\":{\"connectionId\":{\"type\":\"string\"},\"contextId\":{\"type\":\"string\"},\"selector\":{\"type\":\"string\"},\"targetId\":{\"type\":\"string\"}},\"additionalProperties\":false},\"result\":{\"type\":\"boolean\"}},\"close_playwright_proxy\":{\"params\":{\"type\":\"object\",\"required\":[\"proxyId\"],\"properties\":{\"proxyId\":{\"type\":\"string\"}},\"additionalProperties\":false},\"result\":{\"type\":\"boolean\"}},\"close_relay\":{\"params\":{\"type\":\"object\",\"required\":[\"relayId\"],\"properties\":{\"relayId\":{\"type\":\"string\"}},\"additionalProperties\":false},\"result\":{\"type\":\"boolean\"},\"description\":\"Closes a relay opened by `open_context_relay` or `open_target_relay`, restoring ordinary\\nlocal access to its context. Returns `false` if the relay was already closed.\"},\"connect_connection\":{\"params\":{\"type\":\"object\",\"required\":[\"connectionId\",\"contextId\"],\"properties\":{\"connectionId\":{\"type\":\"string\"},\"contextId\":{\"type\":\"string\"}},\"additionalProperties\":false},\"result\":{\"$ref\":\"#/components/schemas/ContextSnapshot\"}},\"delete_breakpoint\":{\"params\":{\"type\":\"object\",\"required\":[\"breakpointId\",\"contextId\",\"options\"],\"properties\":{\"breakpointId\":{\"type\":\"string\"},\"contextId\":{\"type\":\"string\"},\"options\":{\"$ref\":\"#/components/schemas/MutationOptions\"}},\"additionalProperties\":false},\"result\":{\"$ref\":\"#/components/schemas/ContextSnapshot\"}},\"delete_capture\":{\"params\":{\"type\":\"object\",\"required\":[\"captureName\",\"contextId\"],\"properties\":{\"captureName\":{\"type\":\"string\"},\"contextId\":{\"type\":\"string\"}},\"additionalProperties\":false},\"result\":{\"type\":\"boolean\"}},\"delete_connection\":{\"params\":{\"type\":\"object\",\"required\":[\"connectionId\",\"contextId\",\"options\"],\"properties\":{\"connectionId\":{\"type\":\"string\"},\"contextId\":{\"type\":\"string\"},\"options\":{\"$ref\":\"#/components/schemas/MutationOptions\"}},\"additionalProperties\":false},\"result\":{\"$ref\":\"#/components/schemas/ContextSnapshot\"}},\"delete_context\":{\"params\":{\"type\":\"object\",\"required\":[\"contextId\",\"options\"],\"properties\":{\"contextId\":{\"type\":\"string\"},\"options\":{\"$ref\":\"#/components/schemas/MutationOptions\"}},\"additionalProperties\":false},\"result\":{\"type\":\"boolean\"}},\"delete_source_formatting_rule\":{\"params\":{\"type\":\"object\",\"required\":[\"contextId\",\"ruleId\"],\"properties\":{\"contextId\":{\"type\":\"string\"},\"ruleId\":{\"type\":\"string\"}},\"additionalProperties\":false},\"result\":{\"$ref\":\"#/components/schemas/SourceFormattingSettings\"}},\"detach_target\":{\"params\":{\"type\":\"object\",\"required\":[\"connectionId\",\"contextId\",\"targetId\"],\"properties\":{\"connectionId\":{\"type\":\"string\"},\"contextId\":{\"type\":\"string\"},\"expectedConnectionGeneration\":{\"type\":[\"integer\",\"null\"],\"format\":\"uint64\"},\"targetId\":{\"type\":\"string\"}},\"additionalProperties\":false},\"result\":{\"$ref\":\"#/components/schemas/ContextSnapshot\"}},\"diff_heap_snapshots\":{\"params\":{\"type\":\"object\",\"required\":[\"by\",\"connectionId\",\"contextId\",\"limit\",\"newerCaptureId\",\"olderCaptureId\",\"targetId\"],\"properties\":{\"by\":{\"$ref\":\"#/components/schemas/HeapAggregateBy\"},\"connectionId\":{\"type\":\"string\"},\"contextId\":{\"type\":\"string\"},\"limit\":{\"type\":\"integer\"},\"maxStringLength\":{\"type\":[\"integer\",\"null\"],\"format\":\"uint32\"},\"newerCaptureId\":{\"type\":\"string\"},\"olderCaptureId\":{\"type\":\"string\"},\"targetId\":{\"type\":\"string\"}},\"additionalProperties\":false},\"result\":{\"$ref\":\"#/components/schemas/HeapDiffSnapshot\"}},\"disconnect_connection\":{\"params\":{\"type\":\"object\",\"required\":[\"connectionId\",\"contextId\"],\"properties\":{\"connectionId\":{\"type\":\"string\"},\"contextId\":{\"type\":\"string\"}},\"additionalProperties\":false},\"result\":{\"$ref\":\"#/components/schemas/ContextSnapshot\"}},\"discover_vscode_process_trees\":{\"params\":{\"type\":\"object\",\"properties\":{},\"additionalProperties\":false},\"result\":{\"type\":\"array\",\"items\":{\"$ref\":\"#/components/schemas/ProcessTreeSnapshot\"}}},\"evaluate_target\":{\"params\":{\"type\":\"object\",\"required\":[\"connectionId\",\"contextId\",\"expression\",\"frameIndex\",\"targetId\"],\"properties\":{\"connectionId\":{\"type\":\"string\"},\"contextId\":{\"type\":\"string\"},\"expression\":{\"type\":\"string\"},\"frameIndex\":{\"type\":\"integer\"},\"pauseEpoch\":{\"type\":[\"integer\",\"null\"],\"format\":\"uint64\"},\"targetId\":{\"type\":\"string\"}},\"additionalProperties\":false},\"result\":{\"$ref\":\"#/components/schemas/EvaluationSnapshot\"}},\"evict_source_caches\":{\"params\":{\"type\":\"object\",\"required\":[\"contextId\"],\"properties\":{\"contextId\":{\"type\":\"string\"}},\"additionalProperties\":false},\"result\":{\"type\":\"integer\"}},\"explain_source\":{\"params\":{\"type\":\"object\",\"required\":[\"contextId\",\"path\"],\"properties\":{\"contextId\":{\"type\":\"string\"},\"path\":{\"type\":\"string\"}},\"additionalProperties\":false},\"result\":{\"type\":\"array\",\"items\":{\"$ref\":\"#/components/schemas/SourceGraphViewSnapshot\"}}},\"export_sources\":{\"params\":{\"type\":\"object\",\"required\":[\"contextId\",\"destination\"],\"properties\":{\"contextId\":{\"type\":\"string\"},\"destination\":{\"type\":\"string\"}},\"additionalProperties\":false},\"result\":{\"type\":\"array\",\"items\":{\"type\":\"string\"}}},\"finish_coverage\":{\"params\":{\"type\":\"object\",\"required\":[\"connectionId\",\"contextId\",\"targetId\"],\"properties\":{\"captureId\":{\"type\":[\"string\",\"null\"]},\"connectionId\":{\"type\":\"string\"},\"contextId\":{\"type\":\"string\"},\"targetId\":{\"type\":\"string\"}},\"additionalProperties\":false},\"result\":{\"type\":\"boolean\"}},\"get_capture\":{\"params\":{\"type\":\"object\",\"required\":[\"captureName\",\"contextId\"],\"properties\":{\"captureName\":{\"type\":\"string\"},\"contextId\":{\"type\":\"string\"}},\"additionalProperties\":false},\"result\":{\"$ref\":\"#/components/schemas/CaptureSnapshot\"}},\"get_context\":{\"params\":{\"type\":\"object\",\"required\":[\"contextId\"],\"properties\":{\"contextId\":{\"type\":\"string\"}},\"additionalProperties\":false},\"result\":{\"$ref\":\"#/components/schemas/ContextSnapshot\"}},\"get_coverage\":{\"params\":{\"type\":\"object\",\"required\":[\"captureId\",\"connectionId\",\"contextId\",\"noCache\",\"targetId\"],\"properties\":{\"captureId\":{\"type\":\"string\"},\"connectionId\":{\"type\":\"string\"},\"contextId\":{\"type\":\"string\"},\"noCache\":{\"type\":\"boolean\"},\"sourcePath\":{\"type\":[\"string\",\"null\"]},\"targetId\":{\"type\":\"string\"}},\"additionalProperties\":false},\"result\":{\"$ref\":\"#/components/schemas/CoverageSnapshot\"}},\"get_cpu_profile\":{\"params\":{\"type\":\"object\",\"required\":[\"captureId\",\"connectionId\",\"contextId\",\"noCache\",\"project\",\"targetId\"],\"properties\":{\"captureId\":{\"type\":\"string\"},\"connectionId\":{\"type\":\"string\"},\"contextId\":{\"type\":\"string\"},\"noCache\":{\"type\":\"boolean\"},\"project\":{\"type\":\"boolean\"},\"sourcePath\":{\"type\":[\"string\",\"null\"]},\"targetId\":{\"type\":\"string\"}},\"additionalProperties\":false},\"result\":{\"$ref\":\"#/components/schemas/CpuProfileSnapshot\"}},\"get_heap_classes\":{\"params\":{\"type\":\"object\",\"required\":[\"captureId\",\"connectionId\",\"contextId\",\"noCache\",\"targetId\"],\"properties\":{\"captureId\":{\"type\":\"string\"},\"connectionId\":{\"type\":\"string\"},\"contextId\":{\"type\":\"string\"},\"filter\":{\"type\":[\"string\",\"null\"]},\"noCache\":{\"type\":\"boolean\"},\"targetId\":{\"type\":\"string\"}},\"additionalProperties\":false},\"result\":{\"$ref\":\"#/components/schemas/HeapClassSnapshot\"}},\"get_heap_dominator_chain\":{\"params\":{\"type\":\"object\",\"required\":[\"connectionId\",\"contextId\",\"reference\",\"targetId\"],\"properties\":{\"connectionId\":{\"type\":\"string\"},\"contextId\":{\"type\":\"string\"},\"maxStringLength\":{\"type\":[\"integer\",\"null\"],\"format\":\"uint32\"},\"reference\":{\"type\":\"string\"},\"targetId\":{\"type\":\"string\"}},\"additionalProperties\":false},\"result\":{\"$ref\":\"#/components/schemas/HeapDominatorSnapshot\"}},\"get_heap_path\":{\"params\":{\"type\":\"object\",\"required\":[\"connectionId\",\"contextId\",\"from\",\"options\",\"targetId\",\"to\"],\"properties\":{\"connectionId\":{\"type\":\"string\"},\"contextId\":{\"type\":\"string\"},\"from\":{\"type\":\"string\"},\"maxStringLength\":{\"type\":[\"integer\",\"null\"],\"format\":\"uint32\"},\"options\":{\"$ref\":\"#/components/schemas/HeapPathOptions\"},\"targetId\":{\"type\":\"string\"},\"to\":{\"type\":\"string\"}},\"additionalProperties\":false},\"result\":{\"anyOf\":[{\"$ref\":\"#/components/schemas/HeapPathSnapshot\"},{\"type\":\"null\"}]}},\"get_heap_references\":{\"params\":{\"type\":\"object\",\"required\":[\"connectionId\",\"contextId\",\"direction\",\"edgePolicy\",\"limit\",\"reference\",\"targetId\"],\"properties\":{\"connectionId\":{\"type\":\"string\"},\"contextId\":{\"type\":\"string\"},\"direction\":{\"$ref\":\"#/components/schemas/HeapReferenceDirection\"},\"edgePolicy\":{\"$ref\":\"#/components/schemas/HeapEdgePolicy\"},\"limit\":{\"type\":\"integer\"},\"maxStringLength\":{\"type\":[\"integer\",\"null\"],\"format\":\"uint32\"},\"reference\":{\"type\":\"string\"},\"targetId\":{\"type\":\"string\"}},\"additionalProperties\":false},\"result\":{\"$ref\":\"#/components/schemas/HeapReferencesSnapshot\"}},\"get_heap_snapshot_progress\":{\"params\":{\"type\":\"object\",\"required\":[\"connectionId\",\"contextId\",\"targetId\"],\"properties\":{\"connectionId\":{\"type\":\"string\"},\"contextId\":{\"type\":\"string\"},\"targetId\":{\"type\":\"string\"}},\"additionalProperties\":false},\"result\":{\"anyOf\":[{\"$ref\":\"#/components/schemas/HeapSnapshotProgress\"},{\"type\":\"null\"}]}},\"get_logs\":{\"params\":{\"type\":\"object\",\"required\":[\"connectionId\",\"contextId\",\"targetId\"],\"properties\":{\"connectionId\":{\"type\":\"string\"},\"contextId\":{\"type\":\"string\"},\"targetId\":{\"type\":\"string\"}},\"additionalProperties\":false},\"result\":{\"$ref\":\"#/components/schemas/TargetLogSnapshot\"}},\"get_object_properties\":{\"params\":{\"type\":\"object\",\"required\":[\"connectionId\",\"contextId\",\"objectId\",\"targetId\"],\"properties\":{\"connectionId\":{\"type\":\"string\"},\"contextId\":{\"type\":\"string\"},\"objectId\":{\"type\":\"string\"},\"pauseEpoch\":{\"type\":[\"integer\",\"null\"],\"format\":\"uint64\"},\"targetId\":{\"type\":\"string\"}},\"additionalProperties\":false},\"result\":{\"type\":\"array\",\"items\":{\"$ref\":\"#/components/schemas/VariableSnapshot\"}}},\"get_process_projection\":{\"params\":{\"type\":\"object\",\"required\":[\"contextId\",\"expandedRootProcessIds\"],\"properties\":{\"contextId\":{\"type\":\"string\"},\"expandedRootProcessIds\":{\"type\":\"array\",\"items\":{\"type\":\"integer\"}}},\"additionalProperties\":false},\"result\":{\"type\":\"array\",\"items\":{\"$ref\":\"#/components/schemas/ProcessTreeSnapshot\"}},\"description\":\"Returns a process-oriented resource projection. Runtime target discovery is performed only\\nfor the roots named in `expanded_root_process_ids`.\"},\"get_resource_graph\":{\"params\":{\"type\":\"object\",\"required\":[\"contextId\"],\"properties\":{\"contextId\":{\"type\":\"string\"}},\"additionalProperties\":false},\"result\":{\"$ref\":\"#/components/schemas/ResourceGraphSnapshot\"}},\"get_scope_variables\":{\"params\":{\"type\":\"object\",\"required\":[\"connectionId\",\"contextId\",\"frameIndex\",\"pauseEpoch\",\"scopeIndex\",\"targetId\"],\"properties\":{\"connectionId\":{\"type\":\"string\"},\"contextId\":{\"type\":\"string\"},\"frameIndex\":{\"type\":\"integer\"},\"pauseEpoch\":{\"type\":\"integer\"},\"scopeIndex\":{\"type\":\"integer\"},\"targetId\":{\"type\":\"string\"}},\"additionalProperties\":false},\"result\":{\"type\":\"array\",\"items\":{\"$ref\":\"#/components/schemas/VariableSnapshot\"}}},\"get_stored_coverage\":{\"params\":{\"type\":\"object\",\"required\":[\"captureName\",\"contextId\"],\"properties\":{\"captureName\":{\"type\":\"string\"},\"connectionId\":{\"type\":[\"string\",\"null\"]},\"contextId\":{\"type\":\"string\"},\"excludeCaptureId\":{\"type\":[\"string\",\"null\"]},\"pathGlob\":{\"type\":[\"string\",\"null\"]},\"sourcePath\":{\"type\":[\"string\",\"null\"]},\"targetId\":{\"type\":[\"string\",\"null\"]}},\"additionalProperties\":false},\"result\":{\"$ref\":\"#/components/schemas/CoverageSnapshot\"}},\"get_stored_cpu_profile\":{\"params\":{\"type\":\"object\",\"required\":[\"captureName\",\"contextId\"],\"properties\":{\"captureName\":{\"type\":\"string\"},\"connectionId\":{\"type\":[\"string\",\"null\"]},\"contextId\":{\"type\":\"string\"},\"sourcePath\":{\"type\":[\"string\",\"null\"]},\"targetId\":{\"type\":[\"string\",\"null\"]}},\"additionalProperties\":false},\"result\":{\"$ref\":\"#/components/schemas/CpuProfileSnapshot\"}},\"get_stored_heap_classes\":{\"params\":{\"type\":\"object\",\"required\":[\"captureName\",\"contextId\"],\"properties\":{\"captureName\":{\"type\":\"string\"},\"connectionId\":{\"type\":[\"string\",\"null\"]},\"contextId\":{\"type\":\"string\"},\"filter\":{\"type\":[\"string\",\"null\"]},\"targetId\":{\"type\":[\"string\",\"null\"]}},\"additionalProperties\":false},\"result\":{\"$ref\":\"#/components/schemas/HeapClassSnapshot\"}},\"get_target\":{\"params\":{\"type\":\"object\",\"required\":[\"connectionId\",\"contextId\",\"targetId\"],\"properties\":{\"connectionId\":{\"type\":\"string\"},\"contextId\":{\"type\":\"string\"},\"targetId\":{\"type\":\"string\"}},\"additionalProperties\":false},\"result\":{\"$ref\":\"#/components/schemas/TargetDebuggerSnapshot\"}},\"grep_sources\":{\"params\":{\"type\":\"object\",\"required\":[\"contextId\",\"options\"],\"properties\":{\"contextId\":{\"type\":\"string\"},\"options\":{\"$ref\":\"#/components/schemas/SourceSearchOptions\"}},\"additionalProperties\":false},\"result\":{\"$ref\":\"#/components/schemas/SourceSearchSnapshot\"}},\"inspect_value\":{\"params\":{\"type\":\"object\",\"required\":[\"connectionId\",\"contextId\",\"options\",\"selector\",\"targetId\"],\"properties\":{\"connectionId\":{\"type\":\"string\"},\"contextId\":{\"type\":\"string\"},\"options\":{\"$ref\":\"#/components/schemas/ValueInspectionOptions\"},\"pauseEpoch\":{\"type\":[\"integer\",\"null\"],\"format\":\"uint64\"},\"selector\":{\"$ref\":\"#/components/schemas/ValueSelector\"},\"targetId\":{\"type\":\"string\"}},\"additionalProperties\":false},\"result\":{\"$ref\":\"#/components/schemas/ValueSnapshot\"}},\"list_captures\":{\"params\":{\"type\":\"object\",\"required\":[\"contextId\"],\"properties\":{\"contextId\":{\"type\":\"string\"}},\"additionalProperties\":false},\"result\":{\"type\":\"array\",\"items\":{\"$ref\":\"#/components/schemas/CaptureSnapshot\"}}},\"list_contexts\":{\"params\":{\"type\":\"object\",\"properties\":{\"cwd\":{\"type\":[\"string\",\"null\"]}},\"additionalProperties\":false},\"result\":{\"type\":\"array\",\"items\":{\"$ref\":\"#/components/schemas/ContextSummary\"}}},\"list_sources\":{\"params\":{\"type\":\"object\",\"required\":[\"contextId\"],\"properties\":{\"contextId\":{\"type\":\"string\"},\"path\":{\"type\":[\"string\",\"null\"]}},\"additionalProperties\":false},\"result\":{\"type\":\"array\",\"items\":{\"$ref\":\"#/components/schemas/SourceSnapshotInfo\"}}},\"map_source\":{\"params\":{\"type\":\"object\",\"required\":[\"column\",\"contextId\",\"line\",\"path\"],\"properties\":{\"column\":{\"type\":\"integer\"},\"contextId\":{\"type\":\"string\"},\"line\":{\"type\":\"integer\"},\"path\":{\"type\":\"string\"}},\"additionalProperties\":false},\"result\":{\"type\":\"array\",\"items\":{\"$ref\":\"#/components/schemas/SourceMappingSnapshot\"}}},\"observe_context\":{\"params\":{\"type\":\"object\",\"required\":[\"contextId\",\"cursor\",\"timeoutMs\"],\"properties\":{\"contextId\":{\"type\":\"string\"},\"cursor\":{\"$ref\":\"#/components/schemas/ObservationCursor\"},\"timeoutMs\":{\"type\":\"integer\"}},\"additionalProperties\":false},\"result\":{\"$ref\":\"#/components/schemas/ObservationResult\"}},\"observe_target\":{\"params\":{\"type\":\"object\",\"required\":[\"afterRevision\",\"connectionId\",\"contextId\",\"targetId\",\"timeoutMs\"],\"properties\":{\"afterRevision\":{\"type\":\"integer\"},\"connectionId\":{\"type\":\"string\"},\"contextId\":{\"type\":\"string\"},\"targetId\":{\"type\":\"string\"},\"timeoutMs\":{\"type\":\"integer\"}},\"additionalProperties\":false},\"result\":{\"anyOf\":[{\"$ref\":\"#/components/schemas/TargetDebuggerSnapshot\"},{\"type\":\"null\"}]}},\"open_context_relay\":{\"params\":{\"type\":\"object\",\"required\":[\"contextId\"],\"properties\":{\"contextId\":{\"type\":\"string\"}},\"additionalProperties\":false},\"result\":{\"$ref\":\"#/components/schemas/RelayEndpoint\"},\"description\":\"Opens a virtual browser-root CDP relay exposing every target across every connection in\\n`context_id` as one endpoint. Takes exclusive relay ownership of the context immediately:\\nordinary local target debugging commands fail until the relay closes. Does not restart\\nany underlying connection; existing attachments and future ones stay lazy.\"},\"open_playwright_proxy\":{\"params\":{\"type\":\"object\",\"required\":[\"connectionId\",\"contextId\",\"expectedGeneration\",\"targetId\"],\"properties\":{\"connectionId\":{\"type\":\"string\"},\"contextId\":{\"type\":\"string\"},\"expectedGeneration\":{\"type\":\"integer\"},\"targetId\":{\"type\":\"string\"}},\"additionalProperties\":false},\"result\":{\"$ref\":\"#/components/schemas/PlaywrightProxyEndpoint\"}},\"open_target_relay\":{\"params\":{\"type\":\"object\",\"required\":[\"connectionId\",\"contextId\",\"targetId\"],\"properties\":{\"connectionId\":{\"type\":\"string\"},\"contextId\":{\"type\":\"string\"},\"targetId\":{\"type\":\"string\"}},\"additionalProperties\":false},\"result\":{\"$ref\":\"#/components/schemas/RelayEndpoint\"},\"description\":\"Opens a direct-root CDP relay exposing exactly one target. Takes the same exclusive\\nrelay ownership of the target's owning context as `open_context_relay`.\"},\"put_breakpoint\":{\"params\":{\"type\":\"object\",\"required\":[\"breakpointId\",\"column\",\"contextId\",\"line\",\"sourcePath\"],\"properties\":{\"breakpointId\":{\"type\":\"string\"},\"column\":{\"type\":\"integer\"},\"contextId\":{\"type\":\"string\"},\"line\":{\"type\":\"integer\"},\"sourcePath\":{\"type\":\"string\"}},\"additionalProperties\":false},\"result\":{\"$ref\":\"#/components/schemas/ContextSnapshot\"}},\"put_breakpoint_spec\":{\"params\":{\"type\":\"object\",\"required\":[\"breakpointId\",\"contextId\",\"options\",\"specification\"],\"properties\":{\"breakpointId\":{\"type\":\"string\"},\"contextId\":{\"type\":\"string\"},\"options\":{\"$ref\":\"#/components/schemas/MutationOptions\"},\"specification\":{\"$ref\":\"#/components/schemas/BreakpointSpec\"}},\"additionalProperties\":false},\"result\":{\"$ref\":\"#/components/schemas/ContextSnapshot\"}},\"put_connection\":{\"params\":{\"type\":\"object\",\"required\":[\"configuration\",\"connectionId\",\"contextId\"],\"properties\":{\"configuration\":{\"$ref\":\"#/components/schemas/ConnectionConfiguration\"},\"connectionId\":{\"type\":\"string\"},\"contextId\":{\"type\":\"string\"}},\"additionalProperties\":false},\"result\":{\"$ref\":\"#/components/schemas/ContextSnapshot\"}},\"put_context\":{\"params\":{\"type\":\"object\",\"required\":[\"contextId\",\"kind\"],\"properties\":{\"contextId\":{\"type\":\"string\"},\"displayName\":{\"type\":[\"string\",\"null\"]},\"kind\":{\"$ref\":\"#/components/schemas/ContextKind\"}},\"additionalProperties\":false},\"result\":{\"$ref\":\"#/components/schemas/ContextSnapshot\"}},\"raw_cdp_request\":{\"params\":{\"type\":\"object\",\"required\":[\"connectionId\",\"contextId\",\"method\",\"params\",\"targetId\",\"validate\"],\"properties\":{\"connectionId\":{\"type\":\"string\"},\"contextId\":{\"type\":\"string\"},\"method\":{\"type\":\"string\"},\"params\":true,\"targetId\":{\"type\":\"string\"},\"validate\":{\"type\":\"boolean\"}},\"additionalProperties\":false},\"result\":true},\"raw_cdp_session_request\":{\"params\":{\"type\":\"object\",\"required\":[\"connectionId\",\"contextId\",\"method\",\"params\",\"sessionId\",\"targetId\",\"validate\"],\"properties\":{\"connectionId\":{\"type\":\"string\"},\"contextId\":{\"type\":\"string\"},\"method\":{\"type\":\"string\"},\"params\":true,\"sessionId\":{\"type\":\"string\"},\"targetId\":{\"type\":\"string\"},\"validate\":{\"type\":\"boolean\"}},\"additionalProperties\":false},\"result\":true},\"release_target\":{\"params\":{\"type\":\"object\",\"required\":[\"connectionId\",\"contextId\",\"targetId\"],\"properties\":{\"connectionId\":{\"type\":\"string\"},\"contextId\":{\"type\":\"string\"},\"targetId\":{\"type\":\"string\"}},\"additionalProperties\":false},\"result\":{\"$ref\":\"#/components/schemas/TargetDebuggerSnapshot\"}},\"resolve_sources\":{\"params\":{\"type\":\"object\",\"required\":[\"contextId\",\"source\"],\"properties\":{\"contextId\":{\"type\":\"string\"},\"source\":{\"type\":\"string\"}},\"additionalProperties\":false},\"result\":{\"$ref\":\"#/components/schemas/UncompactedSourceGraphSnapshot\"}},\"resolve_target\":{\"params\":{\"type\":\"object\",\"required\":[\"contextId\",\"selector\"],\"properties\":{\"contextId\":{\"type\":\"string\"},\"selector\":{\"type\":\"string\"}},\"additionalProperties\":false},\"result\":{\"$ref\":\"#/components/schemas/CanonicalTargetSnapshot\"}},\"resume_target\":{\"params\":{\"type\":\"object\",\"required\":[\"connectionId\",\"contextId\",\"pauseEpoch\",\"targetId\"],\"properties\":{\"connectionId\":{\"type\":\"string\"},\"contextId\":{\"type\":\"string\"},\"pauseEpoch\":{\"type\":\"integer\"},\"targetId\":{\"type\":\"string\"}},\"additionalProperties\":false},\"result\":{\"$ref\":\"#/components/schemas/TargetDebuggerSnapshot\"}},\"select_heap_nodes\":{\"params\":{\"type\":\"object\",\"required\":[\"captureId\",\"connectionId\",\"contextId\",\"includeDominators\",\"selector\",\"targetId\"],\"properties\":{\"captureId\":{\"type\":\"string\"},\"connectionId\":{\"type\":\"string\"},\"contextId\":{\"type\":\"string\"},\"includeDominators\":{\"type\":\"boolean\"},\"maxStringLength\":{\"type\":[\"integer\",\"null\"],\"format\":\"uint32\"},\"selector\":{\"$ref\":\"#/components/schemas/HeapNodeSelector\"},\"targetId\":{\"type\":\"string\"}},\"additionalProperties\":false},\"result\":{\"$ref\":\"#/components/schemas/HeapNodeSelectionSnapshot\"}},\"select_promises\":{\"params\":{\"type\":\"object\",\"required\":[\"captureId\",\"connectionId\",\"contextId\",\"limit\",\"maxPreviewLength\",\"targetId\"],\"properties\":{\"captureId\":{\"type\":\"string\"},\"connectionId\":{\"type\":\"string\"},\"contextId\":{\"type\":\"string\"},\"limit\":{\"type\":\"integer\"},\"maxPreviewLength\":{\"type\":\"integer\"},\"state\":{\"anyOf\":[{\"$ref\":\"#/components/schemas/PromiseState\"},{\"type\":\"null\"}]},\"targetId\":{\"type\":\"string\"}},\"additionalProperties\":false},\"result\":{\"$ref\":\"#/components/schemas/PromiseSelectionSnapshot\"}},\"service_info\":{\"params\":{\"type\":\"object\",\"properties\":{},\"additionalProperties\":false},\"result\":{\"$ref\":\"#/components/schemas/ServiceInfo\"}},\"set_logpoint\":{\"params\":{\"type\":\"object\",\"required\":[\"column\",\"connectionId\",\"contextId\",\"expression\",\"line\",\"logpointId\",\"sourceUrl\",\"targetId\"],\"properties\":{\"column\":{\"type\":\"integer\"},\"connectionId\":{\"type\":\"string\"},\"contextId\":{\"type\":\"string\"},\"expression\":{\"type\":\"string\"},\"line\":{\"type\":\"integer\"},\"logpointId\":{\"type\":\"string\"},\"sourceUrl\":{\"type\":\"string\"},\"targetId\":{\"type\":\"string\"}},\"additionalProperties\":false},\"result\":{\"$ref\":\"#/components/schemas/TargetDebuggerSnapshot\"}},\"set_logpoints\":{\"params\":{\"type\":\"object\",\"required\":[\"connectionId\",\"contextId\",\"logpoints\",\"targetId\"],\"properties\":{\"connectionId\":{\"type\":\"string\"},\"contextId\":{\"type\":\"string\"},\"logpoints\":{\"type\":\"array\",\"items\":{\"$ref\":\"#/components/schemas/LogpointSpec\"}},\"targetId\":{\"type\":\"string\"}},\"additionalProperties\":false},\"result\":{\"$ref\":\"#/components/schemas/TargetDebuggerSnapshot\"}},\"set_pause_future_children\":{\"params\":{\"type\":\"object\",\"required\":[\"connectionId\",\"contextId\",\"enabled\"],\"properties\":{\"connectionId\":{\"type\":\"string\"},\"contextId\":{\"type\":\"string\"},\"enabled\":{\"type\":\"boolean\"}},\"additionalProperties\":false},\"result\":{\"type\":\"boolean\"}},\"set_source_formatting\":{\"params\":{\"type\":\"object\",\"required\":[\"contextId\",\"mode\"],\"properties\":{\"contextId\":{\"type\":\"string\"},\"mode\":{\"$ref\":\"#/components/schemas/SourceFormattingMode\"}},\"additionalProperties\":false},\"result\":{\"$ref\":\"#/components/schemas/SourceFormattingSettings\"}},\"show_source\":{\"params\":{\"type\":\"object\",\"required\":[\"contextId\",\"options\",\"path\"],\"properties\":{\"contextId\":{\"type\":\"string\"},\"options\":{\"$ref\":\"#/components/schemas/SourceDisplayOptions\"},\"path\":{\"type\":\"string\"}},\"additionalProperties\":false},\"result\":{\"$ref\":\"#/components/schemas/SourceContentSnapshot\"}},\"show_source_graph\":{\"params\":{\"type\":\"object\",\"required\":[\"contextId\"],\"properties\":{\"contextId\":{\"type\":\"string\"}},\"additionalProperties\":false},\"result\":{\"$ref\":\"#/components/schemas/CompactedSourceGraphSnapshot\"}},\"show_source_tree\":{\"params\":{\"type\":\"object\",\"required\":[\"contextId\",\"kind\"],\"properties\":{\"contextId\":{\"type\":\"string\"},\"kind\":{\"$ref\":\"#/components/schemas/SourceTreeKind\"}},\"additionalProperties\":false},\"result\":{\"$ref\":\"#/components/schemas/SourceTreeSnapshot\"}},\"show_uncompacted_source_graph\":{\"params\":{\"type\":\"object\",\"required\":[\"contextId\"],\"properties\":{\"contextId\":{\"type\":\"string\"}},\"additionalProperties\":false},\"result\":{\"$ref\":\"#/components/schemas/UncompactedSourceGraphSnapshot\"}},\"shutdown\":{\"params\":{\"type\":\"object\",\"properties\":{},\"additionalProperties\":false},\"result\":{\"type\":\"boolean\"}},\"start_coverage\":{\"params\":{\"type\":\"object\",\"required\":[\"connectionId\",\"contextId\",\"targetId\"],\"properties\":{\"connectionId\":{\"type\":\"string\"},\"contextId\":{\"type\":\"string\"},\"targetId\":{\"type\":\"string\"}},\"additionalProperties\":false},\"result\":{\"type\":\"boolean\"}},\"start_cpu_profile\":{\"params\":{\"type\":\"object\",\"required\":[\"connectionId\",\"contextId\",\"targetId\"],\"properties\":{\"connectionId\":{\"type\":\"string\"},\"contextId\":{\"type\":\"string\"},\"samplingIntervalMicros\":{\"type\":[\"integer\",\"null\"],\"format\":\"uint64\"},\"targetId\":{\"type\":\"string\"}},\"additionalProperties\":false},\"result\":{\"type\":\"boolean\"}},\"step_target\":{\"params\":{\"type\":\"object\",\"required\":[\"connectionId\",\"contextId\",\"kind\",\"pauseEpoch\",\"targetId\"],\"properties\":{\"connectionId\":{\"type\":\"string\"},\"contextId\":{\"type\":\"string\"},\"kind\":{\"$ref\":\"#/components/schemas/StepKind\"},\"pauseEpoch\":{\"type\":\"integer\"},\"targetId\":{\"type\":\"string\"}},\"additionalProperties\":false},\"result\":{\"$ref\":\"#/components/schemas/TargetDebuggerSnapshot\"}},\"stop_coverage\":{\"params\":{\"type\":\"object\",\"required\":[\"connectionId\",\"contextId\",\"targetId\"],\"properties\":{\"captureId\":{\"type\":[\"string\",\"null\"]},\"connectionId\":{\"type\":\"string\"},\"contextId\":{\"type\":\"string\"},\"targetId\":{\"type\":\"string\"}},\"additionalProperties\":false},\"result\":{\"$ref\":\"#/components/schemas/CoverageSnapshot\"}},\"stop_cpu_profile\":{\"params\":{\"type\":\"object\",\"required\":[\"connectionId\",\"contextId\",\"targetId\"],\"properties\":{\"captureId\":{\"type\":[\"string\",\"null\"]},\"connectionId\":{\"type\":\"string\"},\"contextId\":{\"type\":\"string\"},\"targetId\":{\"type\":\"string\"}},\"additionalProperties\":false},\"result\":{\"$ref\":\"#/components/schemas/CpuProfileSnapshot\"}},\"supply_stored_heap_source_map\":{\"params\":{\"type\":\"object\",\"required\":[\"captureName\",\"contextId\",\"supply\"],\"properties\":{\"captureName\":{\"type\":\"string\"},\"contextId\":{\"type\":\"string\"},\"supply\":{\"$ref\":\"#/components/schemas/HeapSourceMapSupply\"}},\"additionalProperties\":false},\"result\":{\"type\":\"null\"}},\"take_coverage\":{\"params\":{\"type\":\"object\",\"required\":[\"connectionId\",\"contextId\",\"targetId\"],\"properties\":{\"captureId\":{\"type\":[\"string\",\"null\"]},\"connectionId\":{\"type\":\"string\"},\"contextId\":{\"type\":\"string\"},\"raw\":{\"type\":[\"boolean\",\"null\"]},\"targetId\":{\"type\":\"string\"}},\"additionalProperties\":false},\"result\":{\"$ref\":\"#/components/schemas/CoverageSnapshot\"}},\"take_heap_snapshot\":{\"params\":{\"type\":\"object\",\"required\":[\"captureNumericValue\",\"connectionId\",\"contextId\",\"exposeInternals\",\"path\",\"targetId\"],\"properties\":{\"captureNumericValue\":{\"type\":\"boolean\"},\"connectionId\":{\"type\":\"string\"},\"contextId\":{\"type\":\"string\"},\"exposeInternals\":{\"type\":\"boolean\"},\"path\":{\"type\":\"string\"},\"targetId\":{\"type\":\"string\"}},\"additionalProperties\":false},\"result\":{\"$ref\":\"#/components/schemas/HeapSnapshotResult\"},\"serverStream\":{\"$ref\":\"#/components/schemas/HeapSnapshotProgress\"}},\"type_target\":{\"params\":{\"type\":\"object\",\"required\":[\"connectionId\",\"contextId\",\"targetId\",\"text\"],\"properties\":{\"connectionId\":{\"type\":\"string\"},\"contextId\":{\"type\":\"string\"},\"targetId\":{\"type\":\"string\"},\"text\":{\"type\":\"string\"}},\"additionalProperties\":false},\"result\":{\"type\":\"boolean\"}},\"wait_target\":{\"params\":{\"type\":\"object\",\"required\":[\"connectionId\",\"contextId\",\"predicate\",\"targetId\",\"timeoutMs\"],\"properties\":{\"connectionId\":{\"type\":\"string\"},\"contextId\":{\"type\":\"string\"},\"predicate\":{\"$ref\":\"#/components/schemas/TargetWaitPredicate\"},\"targetId\":{\"type\":\"string\"},\"timeoutMs\":{\"type\":\"integer\"}},\"additionalProperties\":false},\"result\":{\"$ref\":\"#/components/schemas/TargetDebuggerSnapshot\"}}},\"components\":{\"schemas\":{\"AgentSessionSnapshot\":{\"type\":\"object\",\"required\":[\"internalId\",\"workingDirectories\"],\"properties\":{\"chatUri\":{\"type\":[\"string\",\"null\"]},\"disconnected\":{\"type\":[\"boolean\",\"null\"]},\"internalId\":{\"type\":\"string\"},\"title\":{\"type\":[\"string\",\"null\"]},\"workingDirectories\":{\"type\":\"array\",\"items\":{\"type\":\"string\"}}},\"additionalProperties\":false},\"BreakpointApplicationSnapshot\":{\"type\":\"object\",\"required\":[\"connectionGeneration\",\"connectionId\",\"generatedColumn\",\"generatedLine\",\"scriptId\",\"scriptUrl\",\"scriptVersion\",\"status\",\"targetId\"],\"properties\":{\"connectionGeneration\":{\"type\":\"integer\"},\"connectionId\":{\"type\":\"string\"},\"generatedColumn\":{\"type\":\"integer\"},\"generatedLine\":{\"type\":\"integer\"},\"mapping\":{\"anyOf\":[{\"$ref\":\"#/components/schemas/BreakpointMappingSnapshot\"},{\"type\":\"null\"}]},\"scriptId\":{\"type\":\"string\"},\"scriptUrl\":{\"type\":\"string\"},\"scriptVersion\":{\"type\":\"integer\"},\"status\":{\"$ref\":\"#/components/schemas/BreakpointApplicationStatus\"},\"targetId\":{\"type\":\"string\"}},\"additionalProperties\":false},\"BreakpointApplicationStatus\":{\"oneOf\":[{\"type\":\"object\",\"required\":[\"kind\"],\"properties\":{\"kind\":{\"type\":\"string\",\"const\":\"installing\"}},\"additionalProperties\":false},{\"type\":\"object\",\"required\":[\"backend_id\",\"kind\"],\"properties\":{\"backend_id\":{\"type\":\"string\"},\"kind\":{\"type\":\"string\",\"const\":\"installed\"}},\"additionalProperties\":false},{\"type\":\"object\",\"required\":[\"kind\"],\"properties\":{\"kind\":{\"type\":\"string\",\"const\":\"removing\"}},\"additionalProperties\":false},{\"type\":\"object\",\"required\":[\"kind\",\"message\"],\"properties\":{\"kind\":{\"type\":\"string\",\"const\":\"failed\"},\"message\":{\"type\":\"string\"}},\"additionalProperties\":false}],\"discriminator\":{\"propertyName\":\"kind\"}},\"BreakpointMappingSnapshot\":{\"type\":\"object\",\"required\":[\"generatedColumn\",\"generatedLine\",\"generatedUrl\",\"projection\",\"quality\",\"requestedColumn\",\"requestedLine\",\"sourceUrl\"],\"properties\":{\"generatedColumn\":{\"type\":\"integer\"},\"generatedLine\":{\"type\":\"integer\"},\"generatedUrl\":{\"type\":\"string\"},\"projection\":{\"type\":\"array\",\"items\":{\"type\":\"string\"}},\"quality\":{\"type\":\"string\"},\"requestedColumn\":{\"type\":\"integer\"},\"requestedLine\":{\"type\":\"integer\"},\"sourceUrl\":{\"type\":\"string\"}},\"additionalProperties\":false},\"BreakpointPendingReason\":{\"oneOf\":[{\"type\":\"object\",\"required\":[\"kind\"],\"properties\":{\"kind\":{\"type\":\"string\",\"const\":\"waitingForTarget\"}},\"additionalProperties\":false},{\"type\":\"object\",\"required\":[\"kind\"],\"properties\":{\"kind\":{\"type\":\"string\",\"const\":\"waitingForScript\"}},\"additionalProperties\":false},{\"type\":\"object\",\"required\":[\"diagnostics\",\"kind\"],\"properties\":{\"diagnostics\":{\"type\":\"array\",\"items\":{\"type\":\"string\"}},\"kind\":{\"type\":\"string\",\"const\":\"sourceNotFound\"}},\"additionalProperties\":false},{\"type\":\"object\",\"required\":[\"candidates\",\"kind\",\"omitted_candidate_count\"],\"properties\":{\"candidates\":{\"type\":\"array\",\"items\":{\"$ref\":\"#/components/schemas/BreakpointSourceCandidateSnapshot\"}},\"kind\":{\"type\":\"string\",\"const\":\"ambiguousSource\"},\"omitted_candidate_count\":{\"type\":\"integer\"}},\"additionalProperties\":false},{\"type\":\"object\",\"required\":[\"diagnostics\",\"kind\"],\"properties\":{\"diagnostics\":{\"type\":\"array\",\"items\":{\"type\":\"string\"}},\"kind\":{\"type\":\"string\",\"const\":\"unmapped\"}},\"additionalProperties\":false},{\"type\":\"object\",\"required\":[\"kind\"],\"properties\":{\"kind\":{\"type\":\"string\",\"const\":\"applicable\"}},\"additionalProperties\":false},{\"type\":\"object\",\"required\":[\"kind\"],\"properties\":{\"kind\":{\"type\":\"string\",\"const\":\"installing\"}},\"additionalProperties\":false},{\"type\":\"object\",\"required\":[\"kind\",\"message\"],\"properties\":{\"kind\":{\"type\":\"string\",\"const\":\"failed\"},\"message\":{\"type\":\"string\"}},\"additionalProperties\":false}],\"discriminator\":{\"propertyName\":\"kind\"}},\"BreakpointScriptAssessmentSnapshot\":{\"type\":\"object\",\"required\":[\"connectionGeneration\",\"connectionId\",\"scriptId\",\"scriptUrl\",\"scriptVersion\",\"status\",\"targetId\"],\"properties\":{\"connectionGeneration\":{\"type\":\"integer\"},\"connectionId\":{\"type\":\"string\"},\"scriptId\":{\"type\":\"string\"},\"scriptUrl\":{\"type\":\"string\"},\"scriptVersion\":{\"type\":\"integer\"},\"status\":{\"$ref\":\"#/components/schemas/BreakpointScriptAssessmentStatus\"},\"targetId\":{\"type\":\"string\"}},\"additionalProperties\":false},\"BreakpointScriptAssessmentStatus\":{\"oneOf\":[{\"type\":\"object\",\"required\":[\"kind\"],\"properties\":{\"kind\":{\"type\":\"string\",\"const\":\"waitingForScript\"}},\"additionalProperties\":false},{\"type\":\"object\",\"required\":[\"diagnostics\",\"kind\"],\"properties\":{\"diagnostics\":{\"type\":\"array\",\"items\":{\"type\":\"string\"}},\"kind\":{\"type\":\"string\",\"const\":\"sourceNotFound\"}},\"additionalProperties\":false},{\"type\":\"object\",\"required\":[\"candidates\",\"kind\",\"omitted_candidate_count\"],\"properties\":{\"candidates\":{\"type\":\"array\",\"items\":{\"$ref\":\"#/components/schemas/BreakpointSourceCandidateSnapshot\"}},\"kind\":{\"type\":\"string\",\"const\":\"ambiguousSource\"},\"omitted_candidate_count\":{\"type\":\"integer\"}},\"additionalProperties\":false},{\"type\":\"object\",\"required\":[\"candidate\",\"kind\"],\"properties\":{\"candidate\":{\"$ref\":\"#/components/schemas/BreakpointSourceCandidateSnapshot\"},\"kind\":{\"type\":\"string\",\"const\":\"mapping\"}},\"additionalProperties\":false},{\"type\":\"object\",\"required\":[\"candidate\",\"diagnostics\",\"kind\"],\"properties\":{\"candidate\":{\"$ref\":\"#/components/schemas/BreakpointSourceCandidateSnapshot\"},\"diagnostics\":{\"type\":\"array\",\"items\":{\"type\":\"string\"}},\"kind\":{\"type\":\"string\",\"const\":\"unmapped\"}},\"additionalProperties\":false},{\"type\":\"object\",\"required\":[\"candidate\",\"kind\",\"mappings\"],\"properties\":{\"candidate\":{\"$ref\":\"#/components/schemas/BreakpointSourceCandidateSnapshot\"},\"kind\":{\"type\":\"string\",\"const\":\"applicable\"},\"mappings\":{\"type\":\"array\",\"items\":{\"$ref\":\"#/components/schemas/BreakpointMappingSnapshot\"}}},\"additionalProperties\":false},{\"type\":\"object\",\"required\":[\"kind\",\"message\"],\"properties\":{\"kind\":{\"type\":\"string\",\"const\":\"failed\"},\"message\":{\"type\":\"string\"}},\"additionalProperties\":false}],\"discriminator\":{\"propertyName\":\"kind\"}},\"BreakpointSnapshot\":{\"type\":\"object\",\"required\":[\"column\",\"enabled\",\"id\",\"line\",\"sourcePath\",\"status\"],\"properties\":{\"applications\":{\"type\":\"array\",\"items\":{\"$ref\":\"#/components/schemas/BreakpointApplicationSnapshot\"}},\"column\":{\"type\":\"integer\"},\"condition\":{\"type\":[\"string\",\"null\"]},\"enabled\":{\"type\":\"boolean\"},\"id\":{\"type\":\"string\"},\"line\":{\"type\":\"integer\"},\"pendingReason\":{\"anyOf\":[{\"$ref\":\"#/components/schemas/BreakpointPendingReason\"},{\"type\":\"null\"}]},\"sourcePath\":{\"type\":\"string\"},\"status\":{\"$ref\":\"#/components/schemas/BreakpointStatus\"},\"targetSelector\":{\"type\":[\"string\",\"null\"]},\"targets\":{\"type\":\"array\",\"items\":{\"$ref\":\"#/components/schemas/TargetBreakpointSnapshot\"}}},\"additionalProperties\":false},\"BreakpointSourceCandidateSnapshot\":{\"type\":\"object\",\"required\":[\"contentHash\",\"provenance\",\"sourceUrl\"],\"properties\":{\"contentHash\":{\"type\":\"string\"},\"provenance\":{\"type\":\"string\"},\"sourceUrl\":{\"type\":\"string\"}},\"additionalProperties\":false},\"BreakpointSpec\":{\"type\":\"object\",\"required\":[\"column\",\"enabled\",\"line\",\"sourcePath\"],\"properties\":{\"column\":{\"type\":\"integer\"},\"condition\":{\"type\":[\"string\",\"null\"]},\"enabled\":{\"type\":\"boolean\"},\"line\":{\"type\":\"integer\"},\"sourcePath\":{\"type\":\"string\"},\"targetSelector\":{\"type\":[\"string\",\"null\"]}},\"additionalProperties\":false},\"BreakpointStatus\":{\"oneOf\":[{\"type\":\"string\",\"enum\":[\"unconfirmed\",\"disabled\",\"pending\"]},{\"type\":\"object\",\"required\":[\"partiallyBound\"],\"properties\":{\"partiallyBound\":{\"type\":\"object\",\"required\":[\"application_count\"],\"properties\":{\"application_count\":{\"type\":\"integer\"}},\"additionalProperties\":false}},\"additionalProperties\":false},{\"type\":\"object\",\"required\":[\"bound\"],\"properties\":{\"bound\":{\"type\":\"object\",\"required\":[\"application_count\"],\"properties\":{\"application_count\":{\"type\":\"integer\"}},\"additionalProperties\":false}},\"additionalProperties\":false},{\"type\":\"object\",\"required\":[\"failed\"],\"properties\":{\"failed\":{\"type\":\"object\",\"required\":[\"message\"],\"properties\":{\"message\":{\"type\":\"string\"}},\"additionalProperties\":false}},\"additionalProperties\":false}]},\"CanonicalTargetSnapshot\":{\"type\":\"object\",\"required\":[\"connectionGeneration\",\"connectionId\",\"contextId\",\"resourceId\",\"target\",\"targetId\"],\"properties\":{\"connectionGeneration\":{\"type\":\"integer\"},\"connectionId\":{\"type\":\"string\"},\"contextId\":{\"type\":\"string\"},\"resourceId\":{\"type\":\"string\"},\"target\":{\"$ref\":\"#/components/schemas/TargetSnapshot\"},\"targetId\":{\"type\":\"string\"}},\"additionalProperties\":false},\"CaptureKind\":{\"type\":\"string\",\"enum\":[\"coverage\",\"cpuProfile\",\"heapSnapshot\"]},\"CaptureSnapshot\":{\"type\":\"object\",\"required\":[\"connectionGeneration\",\"connectionId\",\"contextId\",\"kind\",\"name\",\"storageId\",\"targetId\"],\"properties\":{\"connectionGeneration\":{\"type\":\"integer\"},\"connectionId\":{\"type\":\"string\"},\"contextId\":{\"type\":\"string\"},\"kind\":{\"$ref\":\"#/components/schemas/CaptureKind\"},\"name\":{\"type\":\"string\"},\"storageId\":{\"type\":\"string\"},\"targetId\":{\"type\":\"string\"}},\"additionalProperties\":false},\"CdpStdioTopology\":{\"type\":\"string\",\"enum\":[\"browser\",\"target\"]},\"CompactedSourceEdgeSnapshot\":{\"type\":\"object\",\"required\":[\"basis\",\"derived\",\"kind\",\"mappingCount\"],\"properties\":{\"basis\":{\"type\":\"integer\"},\"derived\":{\"type\":\"integer\"},\"fanOut\":{\"type\":\"boolean\"},\"kind\":{\"type\":\"string\"},\"mappingCount\":{\"type\":\"integer\"},\"suffixRewrite\":{\"anyOf\":[{\"$ref\":\"#/components/schemas/SourceSuffixRewriteSnapshot\"},{\"type\":\"null\"}]}},\"additionalProperties\":false},\"CompactedSourceGraphSnapshot\":{\"type\":\"object\",\"required\":[\"edges\",\"nodes\",\"roots\"],\"properties\":{\"edges\":{\"type\":\"array\",\"items\":{\"$ref\":\"#/components/schemas/CompactedSourceEdgeSnapshot\"}},\"nodes\":{\"type\":\"array\",\"items\":{\"$ref\":\"#/components/schemas/CompactedSourceNodeSnapshot\"}},\"roots\":{\"type\":\"array\",\"items\":{\"type\":\"integer\"}}},\"additionalProperties\":false},\"CompactedSourceNodeSnapshot\":{\"type\":\"object\",\"required\":[\"id\",\"prefix\",\"runtimeInternal\",\"sourceCount\"],\"properties\":{\"id\":{\"type\":\"integer\"},\"listedSourcePaths\":{\"type\":\"array\",\"items\":{\"type\":\"string\"}},\"prefix\":{\"type\":\"string\"},\"runtimeInternal\":{\"type\":\"boolean\"},\"snapshotCount\":{\"type\":\"integer\"},\"sourceCount\":{\"type\":\"integer\"}},\"additionalProperties\":false},\"ConnectionConfiguration\":{\"oneOf\":[{\"type\":\"object\",\"required\":[\"endpoint\",\"kind\"],\"properties\":{\"endpoint\":{\"type\":\"string\"},\"kind\":{\"type\":\"string\",\"const\":\"directCdp\"}},\"additionalProperties\":false},{\"type\":\"object\",\"required\":[\"endpoint\",\"kind\"],\"properties\":{\"endpoint\":{\"type\":\"string\"},\"kind\":{\"type\":\"string\",\"const\":\"nodeInspector\"}},\"additionalProperties\":false},{\"type\":\"object\",\"required\":[\"kind\",\"processId\"],\"properties\":{\"kind\":{\"type\":\"string\",\"const\":\"process\"},\"processId\":{\"type\":\"integer\"}},\"additionalProperties\":false},{\"type\":\"object\",\"required\":[\"kind\",\"rootPid\"],\"properties\":{\"kind\":{\"type\":\"string\",\"const\":\"processTree\"},\"rootPid\":{\"type\":\"integer\"}},\"additionalProperties\":false},{\"description\":\"Uses the process tree rooted at `root_pid` as the access path while exposing only `target_id` and its descendants as this connection's public target scope.\",\"type\":\"object\",\"required\":[\"kind\",\"rootPid\",\"targetId\"],\"properties\":{\"kind\":{\"type\":\"string\",\"const\":\"scopedProcessTree\"},\"rootPid\":{\"type\":\"integer\"},\"targetId\":{\"type\":\"string\"}},\"additionalProperties\":false},{\"type\":\"object\",\"required\":[\"channel\",\"headless\",\"kind\",\"url\"],\"properties\":{\"channel\":{\"$ref\":\"#/components/schemas/PlaywrightChannel\"},\"headless\":{\"type\":\"boolean\"},\"ignoreHttpsErrors\":{\"type\":\"boolean\"},\"kind\":{\"type\":\"string\",\"const\":\"playwright\"},\"playwrightPackage\":{\"type\":[\"string\",\"null\"]},\"url\":{\"type\":\"string\"}},\"additionalProperties\":false},{\"type\":\"object\",\"required\":[\"args\",\"executable\",\"headless\",\"kind\",\"url\"],\"properties\":{\"args\":{\"type\":\"array\",\"items\":{\"type\":\"string\"}},\"executable\":{\"type\":\"string\"},\"headless\":{\"type\":\"boolean\"},\"kind\":{\"type\":\"string\",\"const\":\"chrome\"},\"url\":{\"type\":\"string\"},\"userDataDir\":{\"type\":[\"string\",\"null\"]}},\"additionalProperties\":false},{\"type\":\"object\",\"required\":[\"args\",\"cwd\",\"env\",\"kind\",\"program\",\"runtimeExecutable\"],\"properties\":{\"args\":{\"type\":\"array\",\"items\":{\"type\":\"string\"}},\"cwd\":{\"type\":\"string\"},\"env\":{\"type\":\"object\",\"additionalProperties\":{\"type\":\"string\"},\"properties\":{}},\"kind\":{\"type\":\"string\",\"const\":\"node\"},\"program\":{\"type\":\"string\"},\"runtimeArgs\":{\"type\":\"array\",\"items\":{\"type\":\"string\"}},\"runtimeExecutable\":{\"type\":\"string\"}},\"additionalProperties\":false},{\"type\":\"object\",\"required\":[\"args\",\"command\",\"cwd\",\"env\",\"kind\"],\"properties\":{\"args\":{\"type\":\"array\",\"items\":{\"type\":\"string\"}},\"command\":{\"type\":\"string\"},\"cwd\":{\"type\":\"string\"},\"env\":{\"type\":\"object\",\"additionalProperties\":{\"type\":\"string\"},\"properties\":{}},\"kind\":{\"type\":\"string\",\"const\":\"stdio\"},\"topology\":{\"$ref\":\"#/components/schemas/CdpStdioTopology\"}},\"additionalProperties\":false}],\"discriminator\":{\"propertyName\":\"kind\"}},\"ConnectionSnapshot\":{\"type\":\"object\",\"required\":[\"configuration\",\"generation\",\"id\",\"status\",\"targets\"],\"properties\":{\"configuration\":{\"$ref\":\"#/components/schemas/ConnectionConfiguration\"},\"generation\":{\"type\":\"integer\"},\"id\":{\"type\":\"string\"},\"status\":{\"$ref\":\"#/components/schemas/ConnectionStatus\"},\"targets\":{\"type\":\"array\",\"items\":{\"$ref\":\"#/components/schemas/TargetSnapshot\"}}},\"additionalProperties\":false},\"ConnectionStatus\":{\"oneOf\":[{\"type\":\"object\",\"required\":[\"kind\"],\"properties\":{\"kind\":{\"type\":\"string\",\"const\":\"disconnected\"}},\"additionalProperties\":false},{\"type\":\"object\",\"required\":[\"kind\"],\"properties\":{\"kind\":{\"type\":\"string\",\"const\":\"connecting\"}},\"additionalProperties\":false},{\"type\":\"object\",\"required\":[\"kind\"],\"properties\":{\"kind\":{\"type\":\"string\",\"const\":\"disconnecting\"}},\"additionalProperties\":false},{\"type\":\"object\",\"required\":[\"kind\",\"product\",\"protocolVersion\"],\"properties\":{\"kind\":{\"type\":\"string\",\"const\":\"connected\"},\"product\":{\"type\":\"string\"},\"protocolVersion\":{\"type\":\"string\"}},\"additionalProperties\":false},{\"type\":\"object\",\"required\":[\"kind\",\"message\"],\"properties\":{\"kind\":{\"type\":\"string\",\"const\":\"failed\"},\"message\":{\"type\":\"string\"}},\"additionalProperties\":false}],\"discriminator\":{\"propertyName\":\"kind\"}},\"ConsoleMessageSnapshot\":{\"type\":\"object\",\"required\":[\"index\",\"values\"],\"properties\":{\"index\":{\"type\":\"integer\"},\"params\":true,\"values\":{\"type\":\"array\",\"items\":{\"type\":\"string\"}}},\"additionalProperties\":false},\"ContextEventSnapshot\":{\"type\":\"object\",\"required\":[\"kind\",\"revision\"],\"properties\":{\"kind\":{\"type\":\"string\"},\"revision\":{\"type\":\"integer\"},\"subjectId\":{\"type\":[\"string\",\"null\"]}},\"additionalProperties\":false},\"ContextKind\":{\"type\":\"string\",\"enum\":[\"path\",\"named\"]},\"ContextObservation\":{\"type\":\"object\",\"required\":[\"events\",\"snapshot\"],\"properties\":{\"events\":{\"type\":\"array\",\"items\":{\"$ref\":\"#/components/schemas/ContextEventSnapshot\"}},\"snapshot\":{\"$ref\":\"#/components/schemas/ContextSnapshot\"}},\"additionalProperties\":false},\"ContextSnapshot\":{\"type\":\"object\",\"required\":[\"agentInstanceId\",\"breakpoints\",\"connections\",\"displayName\",\"id\",\"revision\",\"targetForest\"],\"properties\":{\"agentInstanceId\":{\"type\":\"string\"},\"breakpoints\":{\"type\":\"array\",\"items\":{\"$ref\":\"#/components/schemas/BreakpointSnapshot\"}},\"connections\":{\"type\":\"array\",\"items\":{\"$ref\":\"#/components/schemas/ConnectionSnapshot\"}},\"displayName\":{\"type\":\"string\"},\"id\":{\"type\":\"string\"},\"resourceRevision\":{\"type\":\"integer\"},\"revision\":{\"type\":\"integer\"},\"sourceFormatting\":{\"$ref\":\"#/components/schemas/SourceFormattingSettings\"},\"targetForest\":{\"type\":\"array\",\"items\":{\"$ref\":\"#/components/schemas/TargetNodeSnapshot\"}}},\"additionalProperties\":false},\"ContextSummary\":{\"type\":\"object\",\"required\":[\"agentInstanceId\",\"breakpointCount\",\"connectionCount\",\"displayName\",\"id\",\"kind\",\"revision\"],\"properties\":{\"agentInstanceId\":{\"type\":\"string\"},\"breakpointCount\":{\"type\":\"integer\"},\"connectionCount\":{\"type\":\"integer\"},\"displayName\":{\"type\":\"string\"},\"id\":{\"type\":\"string\"},\"kind\":{\"$ref\":\"#/components/schemas/ContextKind\"},\"pathAncestor\":{\"type\":[\"boolean\",\"null\"]},\"pathDistance\":{\"type\":[\"integer\",\"null\"],\"format\":\"uint32\"},\"revision\":{\"type\":\"integer\"}},\"additionalProperties\":false},\"CoverageAnalysisSnapshot\":{\"type\":\"object\",\"required\":[\"durationMicros\",\"sourceMapCacheBypasses\",\"sourceMapCacheHits\",\"sourceMapCacheMisses\"],\"properties\":{\"durationMicros\":{\"type\":\"integer\"},\"sourceMapCacheBypasses\":{\"type\":\"integer\"},\"sourceMapCacheHits\":{\"type\":\"integer\"},\"sourceMapCacheMisses\":{\"type\":\"integer\"}},\"additionalProperties\":false},\"CoverageFunctionSnapshot\":{\"type\":\"object\",\"required\":[\"blockCoverage\",\"name\",\"ranges\",\"rootEndOffset\",\"rootStartOffset\"],\"properties\":{\"authoredLocation\":{\"anyOf\":[{\"$ref\":\"#/components/schemas/SourceLocation\"},{\"type\":\"null\"}]},\"blockCoverage\":{\"type\":\"boolean\"},\"breadcrumb\":{\"type\":[\"string\",\"null\"]},\"effectiveRanges\":{\"type\":\"array\",\"items\":{\"$ref\":\"#/components/schemas/CoverageRangeSnapshot\"}},\"generatedLocation\":{\"anyOf\":[{\"$ref\":\"#/components/schemas/SourceLocation\"},{\"type\":\"null\"}]},\"name\":{\"type\":\"string\"},\"ranges\":{\"type\":\"array\",\"items\":{\"$ref\":\"#/components/schemas/CoverageRangeSnapshot\"}},\"rootEndOffset\":{\"type\":\"integer\"},\"rootStartOffset\":{\"type\":\"integer\"}},\"additionalProperties\":false},\"CoverageRangeSnapshot\":{\"type\":\"object\",\"required\":[\"count\",\"endOffset\",\"startOffset\"],\"properties\":{\"authoredEnd\":{\"anyOf\":[{\"$ref\":\"#/components/schemas/SourceLocation\"},{\"type\":\"null\"}]},\"authoredStart\":{\"anyOf\":[{\"$ref\":\"#/components/schemas/SourceLocation\"},{\"type\":\"null\"}]},\"count\":{\"type\":\"integer\"},\"endOffset\":{\"type\":\"integer\"},\"startOffset\":{\"type\":\"integer\"}},\"additionalProperties\":false},\"CoverageSnapshot\":{\"type\":\"object\",\"required\":[\"sources\",\"timestampMicros\"],\"properties\":{\"analysis\":{\"anyOf\":[{\"$ref\":\"#/components/schemas/CoverageAnalysisSnapshot\"},{\"type\":\"null\"}]},\"captureId\":{\"type\":[\"string\",\"null\"]},\"sources\":{\"type\":\"array\",\"items\":{\"$ref\":\"#/components/schemas/CoverageSourceSnapshot\"}},\"timestampMicros\":{\"type\":\"integer\"}},\"additionalProperties\":false},\"CoverageSourceSnapshot\":{\"type\":\"object\",\"required\":[\"functions\",\"generatedUrl\",\"scriptId\"],\"properties\":{\"associatedAuthoredSource\":{\"type\":[\"string\",\"null\"]},\"functions\":{\"type\":\"array\",\"items\":{\"$ref\":\"#/components/schemas/CoverageFunctionSnapshot\"}},\"generatedUrl\":{\"type\":\"string\"},\"scriptId\":{\"type\":\"string\"}},\"additionalProperties\":false},\"CpuProfileAnalysisSnapshot\":{\"type\":\"object\",\"required\":[\"durationMicros\",\"sourceMapCacheBypasses\",\"sourceMapCacheHits\",\"sourceMapCacheMisses\"],\"properties\":{\"durationMicros\":{\"type\":\"integer\"},\"sourceMapCacheBypasses\":{\"type\":\"integer\"},\"sourceMapCacheHits\":{\"type\":\"integer\"},\"sourceMapCacheMisses\":{\"type\":\"integer\"}},\"additionalProperties\":false},\"CpuProfileCallFrameSnapshot\":{\"type\":\"object\",\"required\":[\"columnNumber\",\"functionName\",\"lineNumber\",\"scriptId\",\"url\"],\"properties\":{\"columnNumber\":{\"type\":\"integer\"},\"functionName\":{\"type\":\"string\"},\"lineNumber\":{\"type\":\"integer\"},\"scriptId\":{\"type\":\"string\"},\"url\":{\"type\":\"string\"}},\"additionalProperties\":false},\"CpuProfileFunctionSnapshot\":{\"type\":\"object\",\"required\":[\"generatedLocation\",\"name\",\"sampleCount\",\"selfTimeMicros\",\"totalTimeMicros\"],\"properties\":{\"authoredLocation\":{\"anyOf\":[{\"$ref\":\"#/components/schemas/SourceLocation\"},{\"type\":\"null\"}]},\"breadcrumb\":{\"type\":[\"string\",\"null\"]},\"generatedLocation\":{\"$ref\":\"#/components/schemas/SourceLocation\"},\"name\":{\"type\":\"string\"},\"sampleCount\":{\"type\":\"integer\"},\"selfTimeMicros\":{\"type\":\"integer\"},\"totalTimeMicros\":{\"type\":\"integer\"}},\"additionalProperties\":false},\"CpuProfileNodeSnapshot\":{\"type\":\"object\",\"required\":[\"callFrame\",\"children\",\"id\",\"positionTicks\",\"sampleCount\",\"selfTimeMicros\",\"totalTimeMicros\"],\"properties\":{\"authoredLocation\":{\"anyOf\":[{\"$ref\":\"#/components/schemas/SourceLocation\"},{\"type\":\"null\"}]},\"breadcrumb\":{\"type\":[\"string\",\"null\"]},\"callFrame\":{\"$ref\":\"#/components/schemas/CpuProfileCallFrameSnapshot\"},\"children\":{\"type\":\"array\",\"items\":{\"type\":\"integer\"}},\"deoptReason\":{\"type\":[\"string\",\"null\"]},\"hitCount\":{\"type\":[\"integer\",\"null\"],\"format\":\"int64\"},\"id\":{\"type\":\"integer\"},\"positionTicks\":{\"type\":\"array\",\"items\":{\"$ref\":\"#/components/schemas/CpuProfilePositionTickSnapshot\"}},\"sampleCount\":{\"type\":\"integer\"},\"selfTimeMicros\":{\"type\":\"integer\"},\"totalTimeMicros\":{\"type\":\"integer\"}},\"additionalProperties\":false},\"CpuProfilePositionTickSnapshot\":{\"type\":\"object\",\"required\":[\"line\",\"ticks\"],\"properties\":{\"line\":{\"type\":\"integer\"},\"ticks\":{\"type\":\"integer\"}},\"additionalProperties\":false},\"CpuProfileSnapshot\":{\"type\":\"object\",\"required\":[\"captureId\",\"endTimeMicros\",\"nodes\",\"samples\",\"startTimeMicros\",\"timeDeltasMicros\"],\"properties\":{\"analysis\":{\"anyOf\":[{\"$ref\":\"#/components/schemas/CpuProfileAnalysisSnapshot\"},{\"type\":\"null\"}]},\"captureId\":{\"type\":\"string\"},\"endTimeMicros\":{\"type\":\"number\"},\"functions\":{\"type\":\"array\",\"items\":{\"$ref\":\"#/components/schemas/CpuProfileFunctionSnapshot\"}},\"nodes\":{\"type\":\"array\",\"items\":{\"$ref\":\"#/components/schemas/CpuProfileNodeSnapshot\"}},\"samples\":{\"type\":\"array\",\"items\":{\"type\":\"integer\"}},\"samplingIntervalMicros\":{\"type\":[\"integer\",\"null\"],\"format\":\"uint64\"},\"startTimeMicros\":{\"type\":\"number\"},\"timeDeltasMicros\":{\"description\":\"Raw CDP timestamp differences in sample order, which need not be chronological.\",\"type\":\"array\",\"items\":{\"type\":\"integer\"}}},\"additionalProperties\":false},\"EvaluationSnapshot\":{\"type\":\"object\",\"required\":[\"expression\",\"kind\",\"preview\"],\"properties\":{\"description\":{\"type\":[\"string\",\"null\"]},\"expression\":{\"type\":\"string\"},\"kind\":{\"type\":\"string\"},\"objectId\":{\"type\":[\"string\",\"null\"]},\"preview\":{\"$ref\":\"#/components/schemas/ValuePreviewSnapshot\"},\"unserializableValue\":{\"type\":[\"string\",\"null\"]},\"value\":true},\"additionalProperties\":false},\"FrameProjectionSnapshot\":{\"oneOf\":[{\"type\":\"object\",\"required\":[\"kind\"],\"properties\":{\"kind\":{\"type\":\"string\",\"const\":\"raw\"}},\"additionalProperties\":false},{\"type\":\"object\",\"required\":[\"kind\"],\"properties\":{\"kind\":{\"type\":\"string\",\"const\":\"pending\"}},\"additionalProperties\":false},{\"type\":\"object\",\"required\":[\"kind\",\"location\"],\"properties\":{\"kind\":{\"type\":\"string\",\"const\":\"resolved\"},\"location\":{\"$ref\":\"#/components/schemas/SourceLocation\"}},\"additionalProperties\":false},{\"type\":\"object\",\"required\":[\"kind\",\"message\"],\"properties\":{\"kind\":{\"type\":\"string\",\"const\":\"failed\"},\"message\":{\"type\":\"string\"}},\"additionalProperties\":false}],\"discriminator\":{\"propertyName\":\"kind\"}},\"FrameSnapshot\":{\"type\":\"object\",\"required\":[\"functionName\",\"index\",\"projected\",\"raw\",\"scopes\"],\"properties\":{\"breadcrumb\":{\"type\":[\"string\",\"null\"]},\"functionName\":{\"type\":\"string\"},\"index\":{\"type\":\"integer\"},\"projected\":{\"$ref\":\"#/components/schemas/FrameProjectionSnapshot\"},\"raw\":{\"$ref\":\"#/components/schemas/SourceLocation\"},\"scopes\":{\"type\":\"array\",\"items\":{\"$ref\":\"#/components/schemas/ScopeSnapshot\"}}},\"additionalProperties\":false},\"HeapAggregateBy\":{\"type\":\"string\",\"enum\":[\"nodeType\",\"name\",\"stringValue\"]},\"HeapAggregateEntrySnapshot\":{\"type\":\"object\",\"required\":[\"count\",\"key\",\"keyTruncated\",\"shallowSize\"],\"properties\":{\"count\":{\"type\":\"integer\"},\"key\":{\"type\":\"string\"},\"keyTruncated\":{\"type\":\"boolean\"},\"shallowSize\":{\"type\":\"integer\"}},\"additionalProperties\":false},\"HeapAggregateSnapshot\":{\"type\":\"object\",\"required\":[\"by\",\"captureId\",\"entries\",\"omittedEntryCount\"],\"properties\":{\"by\":{\"$ref\":\"#/components/schemas/HeapAggregateBy\"},\"captureId\":{\"type\":\"string\"},\"entries\":{\"type\":\"array\",\"items\":{\"$ref\":\"#/components/schemas/HeapAggregateEntrySnapshot\"}},\"incompleteStringCount\":{\"type\":\"integer\"},\"omittedEntryCount\":{\"type\":\"integer\"}},\"additionalProperties\":false},\"HeapCaptureResult\":{\"type\":\"object\",\"required\":[\"bytesWritten\",\"captureId\",\"timing\"],\"properties\":{\"bytesWritten\":{\"type\":\"integer\"},\"captureId\":{\"type\":\"string\"},\"timing\":{\"$ref\":\"#/components/schemas/HeapSnapshotTiming\"}},\"additionalProperties\":false},\"HeapClassAnalysisSnapshot\":{\"type\":\"object\",\"required\":[\"constructorGroupCount\",\"parseDurationMicros\",\"projectionDurationMicros\",\"sourceMapHydrationDurationMicros\",\"usedCachedGroups\"],\"properties\":{\"constructorGroupCount\":{\"type\":\"integer\"},\"mappingStatus\":{\"$ref\":\"#/components/schemas/HeapMappingStatus\"},\"parseDurationMicros\":{\"type\":\"integer\"},\"projectionDurationMicros\":{\"type\":\"integer\"},\"scriptMappings\":{\"type\":\"array\",\"items\":{\"$ref\":\"#/components/schemas/HeapScriptMappingDiagnostic\"}},\"snapshotTiming\":{\"anyOf\":[{\"$ref\":\"#/components/schemas/HeapSnapshotTiming\"},{\"type\":\"null\"}]},\"sourceMapHydrationDurationMicros\":{\"type\":\"integer\"},\"usedCachedGroups\":{\"type\":\"boolean\"}},\"additionalProperties\":false},\"HeapClassSnapshot\":{\"type\":\"object\",\"required\":[\"analysis\",\"captureId\",\"classes\",\"totalInstances\",\"totalShallowSize\"],\"properties\":{\"analysis\":{\"$ref\":\"#/components/schemas/HeapClassAnalysisSnapshot\"},\"captureId\":{\"type\":\"string\"},\"classes\":{\"type\":\"array\",\"items\":{\"$ref\":\"#/components/schemas/HeapClassSnapshotEntry\"}},\"totalInstances\":{\"type\":\"integer\"},\"totalShallowSize\":{\"type\":\"integer\"}},\"additionalProperties\":false},\"HeapClassSnapshotEntry\":{\"type\":\"object\",\"required\":[\"generatedName\",\"instanceCount\",\"instances\",\"location\",\"name\",\"omittedInstanceCount\",\"shallowSize\",\"sourceUrl\"],\"properties\":{\"generatedName\":{\"type\":\"string\"},\"instanceCount\":{\"type\":\"integer\"},\"instances\":{\"type\":\"array\",\"items\":{\"$ref\":\"#/components/schemas/HeapInstanceSnapshot\"}},\"location\":{\"$ref\":\"#/components/schemas/SourceLocation\"},\"name\":{\"type\":\"string\"},\"omittedInstanceCount\":{\"type\":\"integer\"},\"provenance\":{\"$ref\":\"#/components/schemas/ScriptProvenance\"},\"scriptId\":{\"type\":\"string\"},\"shallowSize\":{\"type\":\"integer\"},\"sourceUrl\":{\"type\":\"string\"}},\"additionalProperties\":false},\"HeapDiffEntrySnapshot\":{\"type\":\"object\",\"required\":[\"countDelta\",\"key\",\"keyTruncated\",\"shallowSizeDelta\"],\"properties\":{\"countDelta\":{\"type\":\"integer\"},\"key\":{\"type\":\"string\"},\"keyTruncated\":{\"type\":\"boolean\"},\"shallowSizeDelta\":{\"type\":\"integer\"}},\"additionalProperties\":false},\"HeapDiffSnapshot\":{\"type\":\"object\",\"required\":[\"by\",\"entries\",\"newerCaptureId\",\"olderCaptureId\"],\"properties\":{\"by\":{\"$ref\":\"#/components/schemas/HeapAggregateBy\"},\"entries\":{\"type\":\"array\",\"items\":{\"$ref\":\"#/components/schemas/HeapDiffEntrySnapshot\"}},\"newerCaptureId\":{\"type\":\"string\"},\"newerIncompleteStringCount\":{\"type\":\"integer\"},\"olderCaptureId\":{\"type\":\"string\"},\"olderIncompleteStringCount\":{\"type\":\"integer\"}},\"additionalProperties\":false},\"HeapDominatorSnapshot\":{\"type\":\"object\",\"required\":[\"captureId\",\"chain\",\"node\"],\"properties\":{\"captureId\":{\"type\":\"string\"},\"chain\":{\"type\":\"array\",\"items\":{\"$ref\":\"#/components/schemas/HeapNodeSnapshot\"}},\"node\":{\"$ref\":\"#/components/schemas/HeapNodeSnapshot\"}},\"additionalProperties\":false},\"HeapEdgePolicy\":{\"type\":\"string\",\"enum\":[\"strong\",\"all\"]},\"HeapInstanceSnapshot\":{\"type\":\"object\",\"required\":[\"alias\",\"heapObjectId\",\"shallowSize\"],\"properties\":{\"alias\":{\"type\":\"string\"},\"heapObjectId\":{\"type\":\"string\"},\"shallowSize\":{\"type\":\"integer\"}},\"additionalProperties\":false},\"HeapMappingStatus\":{\"type\":\"string\",\"enum\":[\"notAttempted\",\"noMapSupplied\",\"mapLoadingFailed\",\"mapped\"]},\"HeapNodeLocationSnapshot\":{\"type\":\"object\",\"required\":[\"column\",\"line\",\"scriptId\"],\"properties\":{\"column\":{\"type\":\"integer\"},\"line\":{\"type\":\"integer\"},\"scriptId\":{\"type\":\"integer\"}},\"additionalProperties\":false},\"HeapNodeSelectionSnapshot\":{\"type\":\"object\",\"required\":[\"captureId\",\"graphParseDurationMicros\",\"nodes\",\"totalEdges\",\"totalNodes\",\"usedCachedGraph\"],\"properties\":{\"captureId\":{\"type\":\"string\"},\"graphParseDurationMicros\":{\"type\":\"integer\"},\"incompleteStringCount\":{\"type\":\"integer\"},\"nodes\":{\"type\":\"array\",\"items\":{\"$ref\":\"#/components/schemas/HeapNodeSnapshot\"}},\"totalEdges\":{\"type\":\"integer\"},\"totalNodes\":{\"type\":\"integer\"},\"usedCachedGraph\":{\"type\":\"boolean\"}},\"additionalProperties\":false},\"HeapNodeSelector\":{\"type\":\"object\",\"properties\":{\"heapObjectId\":{\"type\":[\"string\",\"null\"]},\"limit\":{\"type\":[\"integer\",\"null\"],\"format\":\"uint32\"},\"maxShallowSize\":{\"type\":[\"integer\",\"null\"],\"format\":\"uint64\"},\"minShallowSize\":{\"type\":[\"integer\",\"null\"],\"format\":\"uint64\"},\"name\":{\"type\":[\"string\",\"null\"]},\"nameRegex\":{\"type\":[\"string\",\"null\"]},\"nodeType\":{\"type\":[\"string\",\"null\"]},\"stringContains\":{\"type\":[\"string\",\"null\"]},\"stringRegex\":{\"type\":[\"string\",\"null\"]}},\"additionalProperties\":false},\"HeapNodeSnapshot\":{\"type\":\"object\",\"required\":[\"heapObjectId\",\"incomingReferenceCount\",\"locations\",\"name\",\"nodeIndex\",\"nodeType\",\"outgoingReferenceCount\",\"reference\",\"shallowSize\",\"stringTruncated\"],\"properties\":{\"heapObjectId\":{\"type\":\"string\"},\"immediateDominator\":{\"type\":[\"string\",\"null\"]},\"incomingReferenceCount\":{\"type\":\"integer\"},\"locations\":{\"type\":\"array\",\"items\":{\"$ref\":\"#/components/schemas/HeapNodeLocationSnapshot\"}},\"name\":{\"type\":\"string\"},\"nodeIndex\":{\"type\":\"integer\"},\"nodeType\":{\"type\":\"string\"},\"outgoingReferenceCount\":{\"type\":\"integer\"},\"preview\":{\"type\":[\"string\",\"null\"]},\"reference\":{\"type\":\"string\"},\"retainedSize\":{\"type\":[\"integer\",\"null\"],\"format\":\"uint64\"},\"shallowSize\":{\"type\":\"integer\"},\"source\":{\"$ref\":\"#/components/schemas/ObjectSourceSnapshot\"},\"stringTruncated\":{\"type\":\"boolean\"},\"stringValue\":{\"type\":[\"string\",\"null\"]}},\"additionalProperties\":false},\"HeapPathCost\":{\"type\":\"string\",\"enum\":[\"edges\",\"readable\"]},\"HeapPathDirection\":{\"type\":\"string\",\"enum\":[\"outgoing\",\"incoming\",\"either\"]},\"HeapPathOptions\":{\"type\":\"object\",\"required\":[\"cost\",\"direction\",\"edgePolicy\"],\"properties\":{\"cost\":{\"$ref\":\"#/components/schemas/HeapPathCost\"},\"direction\":{\"$ref\":\"#/components/schemas/HeapPathDirection\"},\"edgePolicy\":{\"$ref\":\"#/components/schemas/HeapEdgePolicy\"}},\"additionalProperties\":false},\"HeapPathSnapshot\":{\"type\":\"object\",\"required\":[\"captureId\",\"cost\",\"from\",\"nodes\",\"steps\",\"to\"],\"properties\":{\"captureId\":{\"type\":\"string\"},\"cost\":{\"type\":\"integer\"},\"from\":{\"type\":\"string\"},\"nodes\":{\"type\":\"array\",\"items\":{\"$ref\":\"#/components/schemas/HeapNodeSnapshot\"}},\"steps\":{\"type\":\"array\",\"items\":{\"$ref\":\"#/components/schemas/HeapPathStepSnapshot\"}},\"to\":{\"type\":\"string\"}},\"additionalProperties\":false},\"HeapPathStepSnapshot\":{\"type\":\"object\",\"required\":[\"direction\",\"edgeIndex\",\"edgeType\",\"from\",\"nameOrIndex\",\"to\"],\"properties\":{\"direction\":{\"$ref\":\"#/components/schemas/HeapTraversalDirection\"},\"edgeIndex\":{\"type\":\"integer\"},\"edgeType\":{\"type\":\"string\"},\"from\":{\"type\":\"string\"},\"name\":{\"type\":[\"string\",\"null\"]},\"nameOrIndex\":{\"type\":\"integer\"},\"to\":{\"type\":\"string\"}},\"additionalProperties\":false},\"HeapReferenceDirection\":{\"type\":\"string\",\"enum\":[\"incoming\",\"outgoing\",\"both\"]},\"HeapReferenceSnapshot\":{\"type\":\"object\",\"required\":[\"edgeIndex\",\"edgeType\",\"nameOrIndex\",\"source\",\"target\"],\"properties\":{\"edgeIndex\":{\"type\":\"integer\"},\"edgeType\":{\"type\":\"string\"},\"name\":{\"type\":[\"string\",\"null\"]},\"nameOrIndex\":{\"type\":\"integer\"},\"source\":{\"type\":\"string\"},\"sourceLocations\":{\"$ref\":\"#/components/schemas/ObjectSourceSnapshot\"},\"sourcePreview\":{\"type\":[\"string\",\"null\"]},\"target\":{\"type\":\"string\"},\"targetLocations\":{\"$ref\":\"#/components/schemas/ObjectSourceSnapshot\"},\"targetPreview\":{\"type\":[\"string\",\"null\"]}},\"additionalProperties\":false},\"HeapReferencesSnapshot\":{\"type\":\"object\",\"required\":[\"captureId\",\"direction\",\"edgePolicy\",\"node\",\"omittedReferenceCount\",\"references\"],\"properties\":{\"captureId\":{\"type\":\"string\"},\"direction\":{\"$ref\":\"#/components/schemas/HeapReferenceDirection\"},\"edgePolicy\":{\"$ref\":\"#/components/schemas/HeapEdgePolicy\"},\"node\":{\"$ref\":\"#/components/schemas/HeapNodeSnapshot\"},\"omittedReferenceCount\":{\"type\":\"integer\"},\"references\":{\"type\":\"array\",\"items\":{\"$ref\":\"#/components/schemas/HeapReferenceSnapshot\"}}},\"additionalProperties\":false},\"HeapScriptMappingDiagnostic\":{\"type\":\"object\",\"required\":[\"hash\",\"provenance\",\"scriptId\",\"status\",\"url\"],\"properties\":{\"diagnostic\":{\"type\":[\"string\",\"null\"]},\"hash\":{\"type\":\"string\"},\"provenance\":{\"$ref\":\"#/components/schemas/ScriptProvenance\"},\"scriptId\":{\"type\":\"string\"},\"status\":{\"$ref\":\"#/components/schemas/HeapMappingStatus\"},\"url\":{\"type\":\"string\"}},\"additionalProperties\":false},\"HeapSnapshotProgress\":{\"type\":\"object\",\"required\":[\"bytesWritten\",\"done\",\"total\"],\"properties\":{\"bytesWritten\":{\"type\":\"integer\"},\"done\":{\"type\":\"integer\"},\"finished\":{\"type\":[\"boolean\",\"null\"]},\"total\":{\"type\":\"integer\"}},\"additionalProperties\":false},\"HeapSnapshotResult\":{\"type\":\"object\",\"required\":[\"bytesWritten\",\"path\",\"timing\"],\"properties\":{\"bytesWritten\":{\"type\":\"integer\"},\"path\":{\"type\":\"string\"},\"timing\":{\"$ref\":\"#/components/schemas/HeapSnapshotTiming\"}},\"additionalProperties\":false},\"HeapSnapshotTiming\":{\"type\":\"object\",\"required\":[\"retrievingDurationMicros\",\"takingDurationMicros\"],\"properties\":{\"retrievingDurationMicros\":{\"type\":\"integer\"},\"takingDurationMicros\":{\"type\":\"integer\"}},\"additionalProperties\":false},\"HeapSourceMapSupply\":{\"type\":\"object\",\"required\":[\"scriptHash\",\"scriptId\",\"sourceMap\",\"sourceMapUrl\"],\"properties\":{\"scriptHash\":{\"type\":\"string\"},\"scriptId\":{\"type\":\"string\"},\"sourceMap\":{\"type\":\"string\"},\"sourceMapUrl\":{\"type\":\"string\"}},\"additionalProperties\":false},\"HeapTraversalDirection\":{\"type\":\"string\",\"enum\":[\"outgoing\",\"incoming\"]},\"LogCaptureSnapshot\":{\"type\":\"object\",\"required\":[\"collectedEvents\",\"status\"],\"properties\":{\"captureId\":{\"type\":[\"string\",\"null\"]},\"collectedEvents\":{\"type\":\"array\",\"items\":{\"type\":\"string\"}},\"droppedCount\":{\"type\":[\"integer\",\"null\"],\"format\":\"uint64\"},\"evictedCount\":{\"type\":[\"integer\",\"null\"],\"format\":\"uint64\"},\"sessionId\":{\"type\":[\"string\",\"null\"]},\"startedAtUnixMs\":{\"type\":[\"integer\",\"null\"],\"format\":\"uint64\"},\"status\":{\"$ref\":\"#/components/schemas/LogCaptureStatus\"}},\"additionalProperties\":false},\"LogCaptureStatus\":{\"type\":\"string\",\"enum\":[\"active\",\"inactive\",\"stopped\",\"unknown\"]},\"LogpointSpec\":{\"type\":\"object\",\"required\":[\"column\",\"expression\",\"id\",\"line\",\"sourceUrl\"],\"properties\":{\"column\":{\"type\":\"integer\"},\"expression\":{\"type\":\"string\"},\"id\":{\"type\":\"string\"},\"line\":{\"type\":\"integer\"},\"sourceUrl\":{\"type\":\"string\"}},\"additionalProperties\":false},\"MutationOptions\":{\"type\":\"object\",\"properties\":{\"expectedRevision\":{\"type\":[\"integer\",\"null\"],\"format\":\"uint64\"},\"requestId\":{\"type\":[\"string\",\"null\"]}},\"additionalProperties\":false},\"ObjectLocationSnapshot\":{\"type\":\"object\",\"required\":[\"kind\",\"origin\",\"position\",\"scriptId\"],\"properties\":{\"kind\":{\"type\":\"string\"},\"origin\":{\"type\":\"string\"},\"position\":{\"$ref\":\"#/components/schemas/ResolvedSourcePosition\"},\"scriptId\":{\"type\":\"string\"}},\"additionalProperties\":false},\"ObjectSourceSnapshot\":{\"type\":\"object\",\"required\":[\"diagnostics\",\"locations\"],\"properties\":{\"diagnostics\":{\"type\":\"array\",\"items\":{\"type\":\"string\"}},\"locations\":{\"type\":\"array\",\"items\":{\"$ref\":\"#/components/schemas/ObjectLocationSnapshot\"}}},\"additionalProperties\":false},\"ObservationCursor\":{\"oneOf\":[{\"type\":\"object\",\"required\":[\"kind\"],\"properties\":{\"kind\":{\"type\":\"string\",\"const\":\"current\"}},\"additionalProperties\":false},{\"type\":\"object\",\"required\":[\"kind\",\"revision\"],\"properties\":{\"kind\":{\"type\":\"string\",\"const\":\"after\"},\"revision\":{\"type\":\"integer\"}},\"additionalProperties\":false}],\"discriminator\":{\"propertyName\":\"kind\"}},\"ObservationResult\":{\"oneOf\":[{\"type\":\"object\",\"required\":[\"items\",\"kind\"],\"properties\":{\"items\":{\"type\":\"array\",\"items\":{\"$ref\":\"#/components/schemas/ContextObservation\"}},\"kind\":{\"type\":\"string\",\"const\":\"items\"}},\"additionalProperties\":false},{\"type\":\"object\",\"required\":[\"current\",\"kind\",\"oldest_available_revision\",\"requested_revision\"],\"properties\":{\"current\":{\"$ref\":\"#/components/schemas/ContextSnapshot\"},\"kind\":{\"type\":\"string\",\"const\":\"historyGap\"},\"oldest_available_revision\":{\"type\":\"integer\"},\"requested_revision\":{\"type\":\"integer\"}},\"additionalProperties\":false}],\"discriminator\":{\"propertyName\":\"kind\"}},\"PauseSnapshot\":{\"type\":\"object\",\"required\":[\"epoch\",\"frames\",\"reason\"],\"properties\":{\"epoch\":{\"type\":\"integer\"},\"frames\":{\"type\":\"array\",\"items\":{\"$ref\":\"#/components/schemas/FrameSnapshot\"}},\"reason\":{\"type\":\"string\"},\"source\":{\"anyOf\":[{\"$ref\":\"#/components/schemas/SourceExcerpt\"},{\"type\":\"null\"}]}},\"additionalProperties\":false},\"PlaywrightChannel\":{\"type\":\"string\",\"enum\":[\"bundled\",\"chrome\",\"chromeBeta\",\"chromeDev\",\"chromeCanary\",\"msedge\",\"msedgeBeta\",\"msedgeDev\",\"msedgeCanary\"]},\"PlaywrightProxyEndpoint\":{\"type\":\"object\",\"required\":[\"connectionGeneration\",\"id\",\"websocketUrl\"],\"properties\":{\"connectionGeneration\":{\"type\":\"integer\"},\"id\":{\"type\":\"string\"},\"websocketUrl\":{\"type\":\"string\"}},\"additionalProperties\":false},\"ProcessRole\":{\"type\":\"string\",\"enum\":[\"vscode-main\",\"electron-main\",\"browser-main\",\"renderer\",\"extension-host\",\"node-utility\",\"node\",\"type-script-server\",\"type-script-installer\",\"language-server\",\"pty-host\",\"file-watcher\",\"agent-host\",\"copilot\",\"claude\",\"codex\",\"agent\",\"gpu\",\"network-service\",\"audio-service\",\"crashpad\",\"utility\",\"other\"]},\"ProcessRootKind\":{\"type\":\"string\",\"enum\":[\"vscode\",\"node\",\"electron\",\"browser\"]},\"ProcessSnapshot\":{\"type\":\"object\",\"required\":[\"commandLine\",\"creationDate\",\"name\",\"processId\",\"role\"],\"properties\":{\"agentSessions\":{\"type\":\"array\",\"items\":{\"$ref\":\"#/components/schemas/AgentSessionSnapshot\"}},\"attachable\":{\"type\":\"boolean\"},\"commandLine\":{\"type\":\"string\"},\"cpuPercent\":{\"type\":[\"integer\",\"null\"],\"format\":\"uint32\"},\"creationDate\":{\"type\":\"string\"},\"debugTargetId\":{\"type\":[\"string\",\"null\"]},\"displayName\":{\"type\":[\"string\",\"null\"]},\"memoryBytes\":{\"type\":[\"integer\",\"null\"],\"format\":\"uint64\"},\"name\":{\"type\":\"string\"},\"parentProcessId\":{\"type\":[\"integer\",\"null\"],\"format\":\"uint32\"},\"processId\":{\"type\":\"integer\"},\"role\":{\"$ref\":\"#/components/schemas/ProcessRole\"},\"windowId\":{\"type\":[\"integer\",\"null\"],\"format\":\"uint32\"},\"windowTitle\":{\"type\":[\"string\",\"null\"]}},\"additionalProperties\":false},\"ProcessTargetSnapshot\":{\"type\":\"object\",\"required\":[\"attached\",\"targetId\",\"targetType\",\"title\",\"url\"],\"properties\":{\"attached\":{\"type\":\"boolean\"},\"browserContextId\":{\"type\":[\"string\",\"null\"]},\"openerId\":{\"type\":[\"string\",\"null\"]},\"parentId\":{\"type\":[\"string\",\"null\"]},\"processId\":{\"type\":[\"integer\",\"null\"],\"format\":\"uint32\"},\"subtype\":{\"type\":[\"string\",\"null\"]},\"targetId\":{\"type\":\"string\"},\"targetType\":{\"type\":\"string\"},\"title\":{\"type\":\"string\"},\"url\":{\"type\":\"string\"}},\"additionalProperties\":false},\"ProcessTreeSnapshot\":{\"type\":\"object\",\"required\":[\"processes\",\"rootProcessId\",\"runtimeMetadataAvailable\"],\"properties\":{\"processes\":{\"type\":\"array\",\"items\":{\"$ref\":\"#/components/schemas/ProcessSnapshot\"}},\"rootKind\":{\"$ref\":\"#/components/schemas/ProcessRootKind\"},\"rootProcessId\":{\"type\":\"integer\"},\"runtimeMetadataAvailable\":{\"type\":\"boolean\"},\"targetDiscoveryError\":{\"type\":[\"string\",\"null\"]},\"targets\":{\"type\":\"array\",\"items\":{\"$ref\":\"#/components/schemas/ProcessTargetSnapshot\"}},\"targetsObserved\":{\"description\":\"Whether runtime children were queried for this root. An empty `targets` collection is only authoritative when this is true.\",\"type\":\"boolean\"}},\"additionalProperties\":false},\"PromiseClassification\":{\"type\":\"string\",\"const\":\"indeterminate\"},\"PromiseOrigin\":{\"type\":\"string\",\"enum\":[\"live\",\"heapSnapshot\"]},\"PromiseSelectionSnapshot\":{\"type\":\"object\",\"required\":[\"captureId\",\"graphParseDurationMicros\",\"omittedPromiseCount\",\"promises\",\"totalPromises\",\"usedCachedGraph\"],\"properties\":{\"captureId\":{\"type\":\"string\"},\"graphParseDurationMicros\":{\"type\":\"integer\"},\"omittedPromiseCount\":{\"type\":\"integer\"},\"promises\":{\"type\":\"array\",\"items\":{\"$ref\":\"#/components/schemas/PromiseSnapshot\"}},\"totalPromises\":{\"type\":\"integer\"},\"usedCachedGraph\":{\"type\":\"boolean\"}},\"additionalProperties\":false},\"PromiseSnapshot\":{\"type\":\"object\",\"required\":[\"classification\",\"origin\",\"state\"],\"properties\":{\"classification\":{\"$ref\":\"#/components/schemas/PromiseClassification\"},\"origin\":{\"$ref\":\"#/components/schemas/PromiseOrigin\"},\"reference\":{\"type\":[\"string\",\"null\"]},\"retained\":{\"type\":[\"boolean\",\"null\"]},\"settlement\":{\"anyOf\":[{\"$ref\":\"#/components/schemas/ValuePreviewSnapshot\"},{\"type\":\"null\"}]},\"state\":{\"$ref\":\"#/components/schemas/PromiseState\"}},\"additionalProperties\":false},\"PromiseState\":{\"type\":\"string\",\"enum\":[\"pending\",\"fulfilled\",\"rejected\",\"unknown\"]},\"RelayEndpoint\":{\"description\":\"A short-lived, authenticated loopback CDP endpoint exposed by `dbgjs context relay` or `dbgjs target relay`. `id` identifies the relay for `close_relay`; `websocket_url` carries its own random capability token and must not be reused once the relay closes.\",\"type\":\"object\",\"required\":[\"id\",\"websocketUrl\"],\"properties\":{\"id\":{\"type\":\"string\"},\"websocketUrl\":{\"type\":\"string\"}},\"additionalProperties\":false},\"ResolvedSourcePosition\":{\"description\":\"Best available source coordinates. Both locations use 1-based lines and UTF-16 columns; URLs are never shortened for display.\",\"type\":\"object\",\"required\":[\"generated\",\"mapping\",\"resolved\"],\"properties\":{\"breadcrumb\":{\"type\":[\"string\",\"null\"]},\"diagnostic\":{\"type\":[\"string\",\"null\"]},\"generated\":{\"$ref\":\"#/components/schemas/SourceLocation\"},\"mapping\":{\"type\":\"string\"},\"resolved\":{\"$ref\":\"#/components/schemas/SourceLocation\"}},\"additionalProperties\":false},\"ResourceCapabilitySnapshot\":{\"type\":\"object\",\"required\":[\"detail\",\"handle\",\"kind\",\"source\",\"title\"],\"properties\":{\"detail\":{\"type\":\"object\",\"additionalProperties\":true,\"properties\":{}},\"handle\":{\"type\":\"integer\"},\"kind\":{\"type\":\"string\"},\"source\":{\"type\":\"string\"},\"title\":{\"type\":\"string\"}},\"additionalProperties\":false},\"ResourceFrontierSnapshot\":{\"type\":\"object\",\"required\":[\"relation\",\"state\"],\"properties\":{\"relation\":{\"type\":\"string\"},\"state\":true},\"additionalProperties\":false},\"ResourceGraphSnapshot\":{\"type\":\"object\",\"required\":[\"relations\",\"resources\",\"revision\"],\"properties\":{\"relations\":{\"type\":\"array\",\"items\":{\"$ref\":\"#/components/schemas/ResourceRelationSnapshot\"}},\"resources\":{\"type\":\"array\",\"items\":{\"$ref\":\"#/components/schemas/ResourceSnapshot\"}},\"revision\":{\"type\":\"integer\"}},\"additionalProperties\":false},\"ResourceRelationSnapshot\":{\"type\":\"object\",\"required\":[\"contributors\",\"from\",\"kind\",\"to\"],\"properties\":{\"contributors\":{\"type\":\"array\",\"items\":{\"type\":\"string\"}},\"from\":{\"type\":\"string\"},\"kind\":{\"type\":\"string\"},\"to\":{\"type\":\"string\"}},\"additionalProperties\":false},\"ResourceSnapshot\":{\"type\":\"object\",\"required\":[\"attributes\",\"capabilities\",\"contributors\",\"frontiers\",\"id\",\"kinds\"],\"properties\":{\"attributes\":{\"type\":\"object\",\"additionalProperties\":true,\"properties\":{}},\"capabilities\":{\"type\":\"array\",\"items\":{\"$ref\":\"#/components/schemas/ResourceCapabilitySnapshot\"}},\"contributors\":{\"type\":\"array\",\"items\":{\"type\":\"string\"}},\"frontiers\":{\"type\":\"array\",\"items\":{\"$ref\":\"#/components/schemas/ResourceFrontierSnapshot\"}},\"id\":{\"type\":\"string\"},\"kinds\":{\"type\":\"array\",\"items\":{\"type\":\"string\"}},\"label\":{\"type\":[\"string\",\"null\"]}},\"additionalProperties\":false},\"ScopeSnapshot\":{\"type\":\"object\",\"required\":[\"index\",\"kind\"],\"properties\":{\"index\":{\"type\":\"integer\"},\"kind\":{\"type\":\"string\"},\"name\":{\"type\":[\"string\",\"null\"]}},\"additionalProperties\":false},\"ScreenshotSnapshot\":{\"type\":\"object\",\"required\":[\"dataBase64\",\"mediaType\"],\"properties\":{\"dataBase64\":{\"type\":\"string\"},\"mediaType\":{\"type\":\"string\"}},\"additionalProperties\":false},\"ScriptProvenance\":{\"type\":\"object\",\"properties\":{\"executionContextAuxData\":true,\"executionContextId\":{\"type\":[\"integer\",\"null\"],\"format\":\"int64\"},\"frameId\":{\"type\":[\"string\",\"null\"]}},\"additionalProperties\":false},\"ServiceInfo\":{\"type\":\"object\",\"required\":[\"agentInstanceId\",\"processId\"],\"properties\":{\"agentInstanceId\":{\"type\":\"string\"},\"processId\":{\"type\":\"integer\"}},\"additionalProperties\":false},\"SourceContentSnapshot\":{\"type\":\"object\",\"required\":[\"content\",\"endLine\",\"path\",\"startLine\",\"totalLines\"],\"properties\":{\"content\":{\"type\":\"string\"},\"endLine\":{\"type\":\"integer\"},\"path\":{\"type\":\"string\"},\"startLine\":{\"type\":\"integer\"},\"totalLines\":{\"type\":\"integer\"}},\"additionalProperties\":false},\"SourceDisplayOptions\":{\"type\":\"object\",\"required\":[\"contextLines\"],\"properties\":{\"contextLines\":{\"type\":\"integer\"},\"line\":{\"type\":[\"integer\",\"null\"],\"format\":\"uint32\"},\"view\":{\"$ref\":\"#/components/schemas/SourceViewPreference\"}},\"additionalProperties\":false},\"SourceExcerpt\":{\"type\":\"object\",\"required\":[\"currentLine\",\"highlightLength\",\"highlightStart\",\"lines\",\"sourceUrl\"],\"properties\":{\"breadcrumb\":{\"type\":[\"string\",\"null\"]},\"currentLine\":{\"type\":\"integer\"},\"highlightLength\":{\"type\":\"integer\"},\"highlightStart\":{\"type\":\"integer\"},\"lines\":{\"type\":\"array\",\"items\":{\"$ref\":\"#/components/schemas/SourceExcerptLine\"}},\"sourceUrl\":{\"type\":\"string\"}},\"additionalProperties\":false},\"SourceExcerptLine\":{\"type\":\"object\",\"required\":[\"line\",\"text\"],\"properties\":{\"line\":{\"type\":\"integer\"},\"text\":{\"type\":\"string\"}},\"additionalProperties\":false},\"SourceFormattingMode\":{\"type\":\"string\",\"enum\":[\"off\",\"auto\",\"on\"]},\"SourceFormattingRule\":{\"type\":\"object\",\"required\":[\"id\",\"mode\"],\"properties\":{\"id\":{\"type\":\"string\"},\"mode\":{\"$ref\":\"#/components/schemas/SourceFormattingMode\"},\"targetPattern\":{\"type\":[\"string\",\"null\"]},\"urlPattern\":{\"type\":[\"string\",\"null\"]}},\"additionalProperties\":false},\"SourceFormattingSettings\":{\"type\":\"object\",\"required\":[\"defaultMode\",\"rules\"],\"properties\":{\"defaultMode\":{\"$ref\":\"#/components/schemas/SourceFormattingMode\"},\"rules\":{\"type\":\"array\",\"items\":{\"$ref\":\"#/components/schemas/SourceFormattingRule\"}}},\"additionalProperties\":false},\"SourceGraphViewSnapshot\":{\"type\":\"object\",\"required\":[\"alternativeProvenance\",\"connectionId\",\"diagnostics\",\"generatedUrl\",\"kind\",\"primaryProvenance\",\"projectionPaths\",\"resolvedSourceCount\",\"role\",\"sourcePath\",\"targetId\"],\"properties\":{\"alternativeProvenance\":{\"type\":\"array\",\"items\":{\"type\":\"string\"}},\"connectionId\":{\"type\":\"string\"},\"diagnostics\":{\"type\":\"array\",\"items\":{\"type\":\"string\"}},\"generatedUrl\":{\"type\":\"string\"},\"kind\":{\"type\":\"string\"},\"primaryProvenance\":{\"type\":\"string\"},\"projectionPaths\":{\"type\":\"array\",\"items\":{\"$ref\":\"#/components/schemas/SourceProjectionPathSnapshot\"}},\"resolvedSourceCount\":{\"type\":\"integer\"},\"role\":{\"type\":\"string\"},\"sourcePath\":{\"type\":\"string\"},\"targetId\":{\"type\":\"string\"}},\"additionalProperties\":false},\"SourceLocation\":{\"type\":\"object\",\"required\":[\"column\",\"line\",\"sourceUrl\"],\"properties\":{\"column\":{\"type\":\"integer\"},\"line\":{\"type\":\"integer\"},\"sourceUrl\":{\"type\":\"string\"}},\"additionalProperties\":false},\"SourceMappingSnapshot\":{\"type\":\"object\",\"required\":[\"column\",\"connectionId\",\"direction\",\"line\",\"quality\",\"sourceUrl\",\"targetId\"],\"properties\":{\"column\":{\"type\":\"integer\"},\"connectionId\":{\"type\":\"string\"},\"direction\":{\"type\":\"string\"},\"line\":{\"type\":\"integer\"},\"quality\":{\"type\":\"string\"},\"sourceUrl\":{\"type\":\"string\"},\"targetId\":{\"type\":\"string\"}},\"additionalProperties\":false},\"SourceMatchSnapshot\":{\"type\":\"object\",\"required\":[\"afterContext\",\"beforeContext\",\"column\",\"contentHash\",\"kind\",\"line\",\"matchLength\",\"path\",\"provenance\",\"text\"],\"properties\":{\"afterContext\":{\"type\":\"array\",\"items\":{\"type\":\"string\"}},\"beforeContext\":{\"type\":\"array\",\"items\":{\"type\":\"string\"}},\"column\":{\"type\":\"integer\"},\"connectionId\":{\"type\":[\"string\",\"null\"]},\"contentHash\":{\"type\":\"string\"},\"kind\":{\"type\":\"string\"},\"line\":{\"type\":\"integer\"},\"matchLength\":{\"type\":\"integer\"},\"path\":{\"type\":\"string\"},\"provenance\":{\"type\":\"string\"},\"targetId\":{\"type\":[\"string\",\"null\"]},\"text\":{\"type\":\"string\"}},\"additionalProperties\":false},\"SourceProjectionPathSnapshot\":{\"type\":\"object\",\"required\":[\"generatedUrl\",\"steps\"],\"properties\":{\"generatedUrl\":{\"type\":\"string\"},\"steps\":{\"type\":\"array\",\"items\":{\"type\":\"string\"}}},\"additionalProperties\":false},\"SourceSearchOptions\":{\"type\":\"object\",\"required\":[\"caseSensitive\",\"contextLines\",\"maxResults\",\"pattern\",\"regex\"],\"properties\":{\"caseSensitive\":{\"type\":\"boolean\"},\"contextLines\":{\"type\":\"integer\"},\"maxResults\":{\"type\":\"integer\"},\"path\":{\"type\":[\"string\",\"null\"]},\"pattern\":{\"type\":\"string\"},\"regex\":{\"type\":\"boolean\"},\"timeoutMs\":{\"type\":[\"integer\",\"null\"],\"format\":\"uint64\"},\"view\":{\"$ref\":\"#/components/schemas/SourceViewPreference\"}},\"additionalProperties\":false},\"SourceSearchSkip\":{\"type\":\"object\",\"required\":[\"kind\",\"path\",\"reason\"],\"properties\":{\"connectionId\":{\"type\":[\"string\",\"null\"]},\"kind\":{\"type\":\"string\"},\"path\":{\"type\":\"string\"},\"reason\":{\"type\":\"string\"},\"targetId\":{\"type\":[\"string\",\"null\"]}},\"additionalProperties\":false},\"SourceSearchSnapshot\":{\"type\":\"object\",\"required\":[\"matches\",\"omittedMatches\",\"searchedContents\",\"searchedSources\",\"skippedSources\"],\"properties\":{\"matches\":{\"type\":\"array\",\"items\":{\"$ref\":\"#/components/schemas/SourceMatchSnapshot\"}},\"omittedMatches\":{\"type\":\"integer\"},\"searchedContents\":{\"type\":\"integer\"},\"searchedSources\":{\"type\":\"integer\"},\"skipped\":{\"type\":\"array\",\"items\":{\"$ref\":\"#/components/schemas/SourceSearchSkip\"}},\"skippedSources\":{\"type\":\"integer\"}},\"additionalProperties\":false},\"SourceSnapshotInfo\":{\"type\":\"object\",\"required\":[\"kind\",\"path\",\"status\"],\"properties\":{\"connectionId\":{\"type\":[\"string\",\"null\"]},\"kind\":{\"type\":\"string\"},\"path\":{\"type\":\"string\"},\"sourceMapUrl\":{\"type\":[\"string\",\"null\"]},\"status\":{\"type\":\"string\"},\"targetId\":{\"type\":[\"string\",\"null\"]}},\"additionalProperties\":false},\"SourceSuffixRewriteSnapshot\":{\"type\":\"object\",\"required\":[\"from\",\"to\"],\"properties\":{\"from\":{\"type\":\"string\"},\"to\":{\"type\":\"string\"}},\"additionalProperties\":false},\"SourceTreeKind\":{\"type\":\"string\",\"enum\":[\"loaded\",\"sourceMapped\",\"formatted\",\"resolved\"]},\"SourceTreeSnapshot\":{\"type\":\"object\",\"required\":[\"kind\",\"sources\"],\"properties\":{\"kind\":{\"$ref\":\"#/components/schemas/SourceTreeKind\"},\"sources\":{\"type\":\"array\",\"items\":{\"$ref\":\"#/components/schemas/UncompactedSourceNodeSnapshot\"}}},\"additionalProperties\":false},\"SourceViewPreference\":{\"type\":\"string\",\"enum\":[\"policy\",\"original\",\"formatted\"]},\"StepKind\":{\"type\":\"string\",\"enum\":[\"into\",\"over\",\"out\"]},\"TargetAttachOptions\":{\"type\":\"object\",\"properties\":{\"expectedConnectionGeneration\":{\"type\":[\"integer\",\"null\"],\"format\":\"uint64\"},\"force\":{\"type\":\"boolean\"}},\"additionalProperties\":false},\"TargetAttachmentOutcome\":{\"type\":\"string\",\"enum\":[\"created\",\"stolen\"]},\"TargetAttachmentResult\":{\"type\":\"object\",\"required\":[\"outcome\",\"target\"],\"properties\":{\"outcome\":{\"$ref\":\"#/components/schemas/TargetAttachmentOutcome\"},\"target\":{\"$ref\":\"#/components/schemas/TargetDebuggerSnapshot\"}},\"additionalProperties\":false},\"TargetAttachmentState\":{\"type\":\"string\",\"enum\":[\"detached\",\"external\",\"debugger\"]},\"TargetBreakpointSnapshot\":{\"type\":\"object\",\"required\":[\"column\",\"id\",\"line\",\"sourceUrl\",\"status\"],\"properties\":{\"applications\":{\"type\":\"array\",\"items\":{\"$ref\":\"#/components/schemas/BreakpointApplicationSnapshot\"}},\"assessments\":{\"type\":\"array\",\"items\":{\"$ref\":\"#/components/schemas/BreakpointScriptAssessmentSnapshot\"}},\"column\":{\"type\":\"integer\"},\"id\":{\"type\":\"string\"},\"line\":{\"type\":\"integer\"},\"source\":{\"anyOf\":[{\"$ref\":\"#/components/schemas/SourceExcerpt\"},{\"type\":\"null\"}]},\"sourceUrl\":{\"type\":\"string\"},\"status\":{\"$ref\":\"#/components/schemas/TargetBreakpointStatus\"}},\"additionalProperties\":false},\"TargetBreakpointStatus\":{\"oneOf\":[{\"type\":\"object\",\"required\":[\"kind\"],\"properties\":{\"kind\":{\"type\":\"string\",\"const\":\"waitingForScript\"}},\"additionalProperties\":false},{\"type\":\"object\",\"required\":[\"diagnostics\",\"kind\"],\"properties\":{\"diagnostics\":{\"type\":\"array\",\"items\":{\"type\":\"string\"}},\"kind\":{\"type\":\"string\",\"const\":\"sourceNotFound\"}},\"additionalProperties\":false},{\"type\":\"object\",\"required\":[\"candidates\",\"kind\",\"omitted_candidate_count\"],\"properties\":{\"candidates\":{\"type\":\"array\",\"items\":{\"$ref\":\"#/components/schemas/BreakpointSourceCandidateSnapshot\"}},\"kind\":{\"type\":\"string\",\"const\":\"ambiguousSource\"},\"omitted_candidate_count\":{\"type\":\"integer\"}},\"additionalProperties\":false},{\"type\":\"object\",\"required\":[\"diagnostics\",\"kind\"],\"properties\":{\"diagnostics\":{\"type\":\"array\",\"items\":{\"type\":\"string\"}},\"kind\":{\"type\":\"string\",\"const\":\"unmapped\"}},\"additionalProperties\":false},{\"type\":\"object\",\"required\":[\"kind\",\"mapping_count\"],\"properties\":{\"kind\":{\"type\":\"string\",\"const\":\"applicable\"},\"mapping_count\":{\"type\":\"integer\"}},\"additionalProperties\":false},{\"type\":\"object\",\"required\":[\"application_count\",\"kind\"],\"properties\":{\"application_count\":{\"type\":\"integer\"},\"kind\":{\"type\":\"string\",\"const\":\"installing\"}},\"additionalProperties\":false},{\"type\":\"object\",\"required\":[\"binding_count\",\"kind\"],\"properties\":{\"binding_count\":{\"type\":\"integer\"},\"kind\":{\"type\":\"string\",\"const\":\"installed\"}},\"additionalProperties\":false},{\"type\":\"object\",\"required\":[\"kind\",\"message\"],\"properties\":{\"kind\":{\"type\":\"string\",\"const\":\"failed\"},\"message\":{\"type\":\"string\"}},\"additionalProperties\":false}],\"discriminator\":{\"propertyName\":\"kind\"}},\"TargetDebuggerPhase\":{\"oneOf\":[{\"type\":\"object\",\"required\":[\"kind\"],\"properties\":{\"kind\":{\"type\":\"string\",\"const\":\"running\"}},\"additionalProperties\":false},{\"type\":\"object\",\"required\":[\"epoch\",\"kind\"],\"properties\":{\"epoch\":{\"type\":\"integer\"},\"kind\":{\"type\":\"string\",\"const\":\"paused\"}},\"additionalProperties\":false},{\"type\":\"object\",\"required\":[\"epoch\",\"kind\"],\"properties\":{\"epoch\":{\"type\":\"integer\"},\"kind\":{\"type\":\"string\",\"const\":\"resuming\"}},\"additionalProperties\":false},{\"type\":\"object\",\"required\":[\"kind\",\"message\"],\"properties\":{\"kind\":{\"type\":\"string\",\"const\":\"failed\"},\"message\":{\"type\":\"string\"}},\"additionalProperties\":false}],\"discriminator\":{\"propertyName\":\"kind\"}},\"TargetDebuggerSnapshot\":{\"type\":\"object\",\"required\":[\"breakpoints\",\"connectionGeneration\",\"connectionId\",\"contextId\",\"logs\",\"phase\",\"revision\",\"scripts\",\"targetId\"],\"properties\":{\"breakpoints\":{\"type\":\"array\",\"items\":{\"$ref\":\"#/components/schemas/TargetBreakpointSnapshot\"}},\"connectionGeneration\":{\"type\":\"integer\"},\"connectionId\":{\"type\":\"string\"},\"contextId\":{\"type\":\"string\"},\"logCapture\":{\"$ref\":\"#/components/schemas/LogCaptureSnapshot\"},\"logs\":{\"type\":\"array\",\"items\":{\"$ref\":\"#/components/schemas/ConsoleMessageSnapshot\"}},\"pause\":{\"anyOf\":[{\"$ref\":\"#/components/schemas/PauseSnapshot\"},{\"type\":\"null\"}]},\"phase\":{\"$ref\":\"#/components/schemas/TargetDebuggerPhase\"},\"revision\":{\"type\":\"integer\"},\"scripts\":{\"type\":\"array\",\"items\":{\"$ref\":\"#/components/schemas/TargetScriptSnapshot\"}},\"targetId\":{\"type\":\"string\"}},\"additionalProperties\":false},\"TargetLogSnapshot\":{\"type\":\"object\",\"required\":[\"capture\",\"connectionGeneration\",\"connectionId\",\"contextId\",\"messages\",\"targetId\"],\"properties\":{\"capture\":{\"$ref\":\"#/components/schemas/LogCaptureSnapshot\"},\"connectionGeneration\":{\"type\":\"integer\"},\"connectionId\":{\"type\":\"string\"},\"contextId\":{\"type\":\"string\"},\"messages\":{\"type\":\"array\",\"items\":{\"$ref\":\"#/components/schemas/ConsoleMessageSnapshot\"}},\"targetId\":{\"type\":\"string\"}},\"additionalProperties\":false},\"TargetNodeSnapshot\":{\"type\":\"object\",\"required\":[\"attachment\",\"connectionGeneration\",\"connectionId\",\"target\"],\"properties\":{\"attachment\":{\"$ref\":\"#/components/schemas/TargetAttachmentState\"},\"connectionGeneration\":{\"type\":\"integer\"},\"connectionId\":{\"type\":\"string\"},\"parentTargetId\":{\"type\":[\"string\",\"null\"]},\"target\":{\"$ref\":\"#/components/schemas/TargetSnapshot\"}},\"additionalProperties\":false},\"TargetScriptSnapshot\":{\"type\":\"object\",\"required\":[\"status\",\"url\"],\"properties\":{\"sourceMapUrl\":{\"type\":[\"string\",\"null\"]},\"status\":{\"$ref\":\"#/components/schemas/TargetScriptStatus\"},\"url\":{\"type\":\"string\"}},\"additionalProperties\":false},\"TargetScriptStatus\":{\"oneOf\":[{\"type\":\"object\",\"required\":[\"kind\"],\"properties\":{\"kind\":{\"type\":\"string\",\"const\":\"unresolved\"}},\"additionalProperties\":false},{\"type\":\"object\",\"required\":[\"kind\"],\"properties\":{\"kind\":{\"type\":\"string\",\"const\":\"pending\"}},\"additionalProperties\":false},{\"type\":\"object\",\"required\":[\"authored_sources\",\"kind\"],\"properties\":{\"authored_sources\":{\"type\":\"array\",\"items\":{\"type\":\"string\"}},\"kind\":{\"type\":\"string\",\"const\":\"resolved\"}},\"additionalProperties\":false},{\"type\":\"object\",\"required\":[\"kind\",\"message\"],\"properties\":{\"kind\":{\"type\":\"string\",\"const\":\"failed\"},\"message\":{\"type\":\"string\"}},\"additionalProperties\":false}],\"discriminator\":{\"propertyName\":\"kind\"}},\"TargetSnapshot\":{\"type\":\"object\",\"required\":[\"attached\",\"targetId\",\"targetType\",\"title\",\"url\"],\"properties\":{\"attached\":{\"type\":\"boolean\"},\"browserContextId\":{\"type\":[\"string\",\"null\"]},\"openerId\":{\"type\":[\"string\",\"null\"]},\"parentId\":{\"type\":[\"string\",\"null\"]},\"subtype\":{\"type\":[\"string\",\"null\"]},\"targetId\":{\"type\":\"string\"},\"targetType\":{\"type\":\"string\"},\"title\":{\"type\":\"string\"},\"url\":{\"type\":\"string\"}},\"additionalProperties\":false},\"TargetWaitPredicate\":{\"oneOf\":[{\"type\":\"object\",\"required\":[\"afterRevision\",\"kind\"],\"properties\":{\"afterRevision\":{\"type\":\"integer\"},\"kind\":{\"type\":\"string\",\"const\":\"changed\"}},\"additionalProperties\":false},{\"type\":\"object\",\"required\":[\"kind\"],\"properties\":{\"kind\":{\"type\":\"string\",\"const\":\"running\"}},\"additionalProperties\":false},{\"type\":\"object\",\"required\":[\"breakpointId\",\"kind\"],\"properties\":{\"breakpointId\":{\"type\":\"string\"},\"kind\":{\"type\":\"string\",\"const\":\"breakpointInstalled\"}},\"additionalProperties\":false},{\"type\":\"object\",\"required\":[\"afterEpoch\",\"kind\"],\"properties\":{\"afterEpoch\":{\"type\":\"integer\"},\"kind\":{\"type\":\"string\",\"const\":\"paused\"}},\"additionalProperties\":false}],\"discriminator\":{\"propertyName\":\"kind\"}},\"UncompactedProjectionSnapshot\":{\"oneOf\":[{\"type\":\"object\",\"required\":[\"contentHash\",\"kind\"],\"properties\":{\"contentHash\":{\"type\":\"string\"},\"kind\":{\"type\":\"string\",\"const\":\"identityEqualContent\"}},\"additionalProperties\":false},{\"type\":\"object\",\"required\":[\"kind\",\"provider\"],\"properties\":{\"kind\":{\"type\":\"string\",\"const\":\"identityDeclaredByProvider\"},\"provider\":{\"type\":\"string\"}},\"additionalProperties\":false},{\"type\":\"object\",\"required\":[\"kind\",\"mapHash\",\"sourceIndex\"],\"properties\":{\"kind\":{\"type\":\"string\",\"const\":\"sourceMap\"},\"mapHash\":{\"type\":\"string\"},\"sourceIndex\":{\"type\":\"integer\"}},\"additionalProperties\":false},{\"type\":\"object\",\"required\":[\"formatter\",\"kind\"],\"properties\":{\"formatter\":{\"type\":\"string\"},\"kind\":{\"type\":\"string\",\"const\":\"format\"}},\"additionalProperties\":false},{\"type\":\"object\",\"required\":[\"edit\",\"kind\"],\"properties\":{\"edit\":{\"type\":\"string\"},\"kind\":{\"type\":\"string\",\"const\":\"edit\"}},\"additionalProperties\":false},{\"type\":\"object\",\"required\":[\"columnDelta\",\"kind\",\"lineDelta\"],\"properties\":{\"columnDelta\":{\"type\":\"integer\"},\"kind\":{\"type\":\"string\",\"const\":\"offset\"},\"lineDelta\":{\"type\":\"integer\"}},\"additionalProperties\":false}],\"discriminator\":{\"propertyName\":\"kind\"}},\"UncompactedSourceEdgeSnapshot\":{\"type\":\"object\",\"required\":[\"basis\",\"derived\",\"id\",\"projection\"],\"properties\":{\"basis\":{\"type\":\"integer\"},\"derived\":{\"type\":\"integer\"},\"id\":{\"type\":\"integer\"},\"projection\":{\"$ref\":\"#/components/schemas/UncompactedProjectionSnapshot\"}},\"additionalProperties\":false},\"UncompactedSourceGraphSnapshot\":{\"type\":\"object\",\"required\":[\"edges\",\"nodes\",\"roots\"],\"properties\":{\"edges\":{\"type\":\"array\",\"items\":{\"$ref\":\"#/components/schemas/UncompactedSourceEdgeSnapshot\"}},\"nodes\":{\"type\":\"array\",\"items\":{\"$ref\":\"#/components/schemas/UncompactedSourceNodeSnapshot\"}},\"roots\":{\"type\":\"array\",\"items\":{\"type\":\"integer\"}}},\"additionalProperties\":false},\"UncompactedSourceNodeSnapshot\":{\"type\":\"object\",\"required\":[\"id\",\"revision\",\"uri\"],\"properties\":{\"id\":{\"type\":\"integer\"},\"revision\":{\"$ref\":\"#/components/schemas/UncompactedSourceRevisionSnapshot\"},\"uri\":{\"type\":\"string\"}},\"additionalProperties\":false},\"UncompactedSourceRevisionSnapshot\":{\"oneOf\":[{\"type\":\"object\",\"required\":[\"hash\",\"kind\"],\"properties\":{\"hash\":{\"type\":\"string\"},\"kind\":{\"type\":\"string\",\"const\":\"content\"}},\"additionalProperties\":false},{\"type\":\"object\",\"required\":[\"kind\",\"namespace\",\"value\"],\"properties\":{\"kind\":{\"type\":\"string\",\"const\":\"version\"},\"namespace\":{\"type\":\"string\"},\"value\":{\"type\":\"string\"}},\"additionalProperties\":false}],\"discriminator\":{\"propertyName\":\"kind\"}},\"ValueInspectionOptions\":{\"type\":\"object\",\"required\":[\"maxPreviewLength\",\"maxProperties\",\"retainReferences\"],\"properties\":{\"maxPreviewLength\":{\"type\":\"integer\"},\"maxProperties\":{\"type\":\"integer\"},\"retainReferences\":{\"type\":\"boolean\"}},\"additionalProperties\":false},\"ValuePreviewSnapshot\":{\"type\":\"object\",\"required\":[\"kind\",\"truncated\"],\"properties\":{\"kind\":{\"type\":\"string\"},\"preview\":{\"type\":[\"string\",\"null\"]},\"reference\":{\"type\":[\"string\",\"null\"]},\"source\":{\"$ref\":\"#/components/schemas/ObjectSourceSnapshot\"},\"truncated\":{\"type\":\"boolean\"}},\"additionalProperties\":false},\"ValuePropertySnapshot\":{\"type\":\"object\",\"required\":[\"name\",\"value\"],\"properties\":{\"name\":{\"type\":\"string\"},\"value\":{\"$ref\":\"#/components/schemas/ValuePreviewSnapshot\"}},\"additionalProperties\":false},\"ValueSelector\":{\"oneOf\":[{\"type\":\"object\",\"required\":[\"allowSideEffects\",\"expression\",\"kind\"],\"properties\":{\"allowSideEffects\":{\"type\":\"boolean\"},\"expression\":{\"type\":\"string\"},\"kind\":{\"type\":\"string\",\"const\":\"expression\"}},\"additionalProperties\":false},{\"type\":\"object\",\"required\":[\"kind\",\"object_id\"],\"properties\":{\"kind\":{\"type\":\"string\",\"const\":\"remoteObject\"},\"object_id\":{\"type\":\"string\"}},\"additionalProperties\":false}],\"discriminator\":{\"propertyName\":\"kind\"}},\"ValueSnapshot\":{\"type\":\"object\",\"required\":[\"preview\",\"properties\",\"selector\"],\"properties\":{\"className\":{\"type\":[\"string\",\"null\"]},\"omittedPropertyCount\":{\"type\":\"integer\"},\"preview\":{\"$ref\":\"#/components/schemas/ValuePreviewSnapshot\"},\"promise\":{\"anyOf\":[{\"$ref\":\"#/components/schemas/PromiseSnapshot\"},{\"type\":\"null\"}]},\"properties\":{\"type\":\"array\",\"items\":{\"$ref\":\"#/components/schemas/ValuePropertySnapshot\"}},\"propertiesTruncated\":{\"type\":\"boolean\"},\"selector\":{\"$ref\":\"#/components/schemas/ValueSelector\"},\"subtype\":{\"type\":[\"string\",\"null\"]}},\"additionalProperties\":false},\"VariableSnapshot\":{\"type\":\"object\",\"required\":[\"kind\",\"name\",\"preview\"],\"properties\":{\"description\":{\"type\":[\"string\",\"null\"]},\"kind\":{\"type\":\"string\"},\"name\":{\"type\":\"string\"},\"objectId\":{\"type\":[\"string\",\"null\"]},\"preview\":{\"$ref\":\"#/components/schemas/ValuePreviewSnapshot\"},\"unserializableValue\":{\"type\":[\"string\",\"null\"]},\"value\":true},\"additionalProperties\":false}}}}");

export const DebuggerService = new InterfaceDefinition(
    {
        id: "dev.dbgjs.cdp-debugger",
        hash: "87cd6ce8c9f328fe",
    },
    {
        add_source_formatting_rule: requestType(
            z.object({
                contextId: z.string(),
                mode: SourceFormattingModeSchema,
                targetPattern: z.union([
                    z.string(),
                    z.null(),
                ]).optional(),
                urlPattern: z.union([
                    z.string(),
                    z.null(),
                ]).optional(),
            }),
            SourceFormattingSettingsSchema,
        ),
        aggregate_heap_snapshot: requestType(
            z.object({
                by: HeapAggregateBySchema,
                captureId: z.string(),
                connectionId: z.string(),
                contextId: z.string(),
                limit: z.int(),
                maxStringLength: z.union([
                    z.uint32(),
                    z.null(),
                ]).optional(),
                targetId: z.string(),
            }),
            HeapAggregateSnapshotSchema,
        ),
        attach_target: requestType(
            z.object({
                connectionId: z.string(),
                contextId: z.string(),
                options: TargetAttachOptionsSchema,
                targetId: z.string(),
            }),
            TargetAttachmentResultSchema,
        ),
        capture_heap_snapshot: requestType(
            z.object({
                captureId: z.union([
                    z.string(),
                    z.null(),
                ]).optional(),
                captureNumericValue: z.boolean(),
                connectionId: z.string(),
                contextId: z.string(),
                exposeInternals: z.boolean(),
                targetId: z.string(),
            }),
            HeapCaptureResultSchema,
        ).withStream({
            server: HeapSnapshotProgressSchema,
        }),
        capture_screenshot: requestType(
            z.object({
                connectionId: z.string(),
                contextId: z.string(),
                targetId: z.string(),
            }),
            ScreenshotSnapshotSchema,
        ),
        click_target: requestType(
            z.object({
                connectionId: z.string(),
                contextId: z.string(),
                selector: z.string(),
                targetId: z.string(),
            }),
            z.boolean(),
        ),
        close_playwright_proxy: requestType(
            z.object({
                proxyId: z.string(),
            }),
            z.boolean(),
        ),
        /**
         * Closes a relay opened by `open_context_relay` or `open_target_relay`, restoring ordinary
         * local access to its context. Returns `false` if the relay was already closed.
         */
        close_relay: requestType(
            z.object({
                relayId: z.string(),
            }),
            z.boolean(),
            {
                description: "Closes a relay opened by `open_context_relay` or `open_target_relay`, restoring ordinary\nlocal access to its context. Returns `false` if the relay was already closed.",
            },
        ),
        connect_connection: requestType(
            z.object({
                connectionId: z.string(),
                contextId: z.string(),
            }),
            ContextSnapshotSchema,
        ),
        delete_breakpoint: requestType(
            z.object({
                breakpointId: z.string(),
                contextId: z.string(),
                options: MutationOptionsSchema,
            }),
            ContextSnapshotSchema,
        ),
        delete_capture: requestType(
            z.object({
                captureName: z.string(),
                contextId: z.string(),
            }),
            z.boolean(),
        ),
        delete_connection: requestType(
            z.object({
                connectionId: z.string(),
                contextId: z.string(),
                options: MutationOptionsSchema,
            }),
            ContextSnapshotSchema,
        ),
        delete_context: requestType(
            z.object({
                contextId: z.string(),
                options: MutationOptionsSchema,
            }),
            z.boolean(),
        ),
        delete_source_formatting_rule: requestType(
            z.object({
                contextId: z.string(),
                ruleId: z.string(),
            }),
            SourceFormattingSettingsSchema,
        ),
        detach_target: requestType(
            z.object({
                connectionId: z.string(),
                contextId: z.string(),
                expectedConnectionGeneration: z.union([
                    z.int().nonnegative(),
                    z.null(),
                ]).optional(),
                targetId: z.string(),
            }),
            ContextSnapshotSchema,
        ),
        diff_heap_snapshots: requestType(
            z.object({
                by: HeapAggregateBySchema,
                connectionId: z.string(),
                contextId: z.string(),
                limit: z.int(),
                maxStringLength: z.union([
                    z.uint32(),
                    z.null(),
                ]).optional(),
                newerCaptureId: z.string(),
                olderCaptureId: z.string(),
                targetId: z.string(),
            }),
            HeapDiffSnapshotSchema,
        ),
        disconnect_connection: requestType(
            z.object({
                connectionId: z.string(),
                contextId: z.string(),
            }),
            ContextSnapshotSchema,
        ),
        discover_vscode_process_trees: requestType(
            z.object({}),
            z.array(ProcessTreeSnapshotSchema),
        ),
        evaluate_target: requestType(
            z.object({
                connectionId: z.string(),
                contextId: z.string(),
                expression: z.string(),
                frameIndex: z.int(),
                pauseEpoch: z.union([
                    z.int().nonnegative(),
                    z.null(),
                ]).optional(),
                targetId: z.string(),
            }),
            EvaluationSnapshotSchema,
        ),
        evict_source_caches: requestType(
            z.object({
                contextId: z.string(),
            }),
            z.int(),
        ),
        explain_source: requestType(
            z.object({
                contextId: z.string(),
                path: z.string(),
            }),
            z.array(SourceGraphViewSnapshotSchema),
        ),
        export_sources: requestType(
            z.object({
                contextId: z.string(),
                destination: z.string(),
            }),
            z.array(z.string()),
        ),
        finish_coverage: requestType(
            z.object({
                captureId: z.union([
                    z.string(),
                    z.null(),
                ]).optional(),
                connectionId: z.string(),
                contextId: z.string(),
                targetId: z.string(),
            }),
            z.boolean(),
        ),
        get_capture: requestType(
            z.object({
                captureName: z.string(),
                contextId: z.string(),
            }),
            CaptureSnapshotSchema,
        ),
        get_context: requestType(
            z.object({
                contextId: z.string(),
            }),
            ContextSnapshotSchema,
        ),
        get_coverage: requestType(
            z.object({
                captureId: z.string(),
                connectionId: z.string(),
                contextId: z.string(),
                noCache: z.boolean(),
                sourcePath: z.union([
                    z.string(),
                    z.null(),
                ]).optional(),
                targetId: z.string(),
            }),
            CoverageSnapshotSchema,
        ),
        get_cpu_profile: requestType(
            z.object({
                captureId: z.string(),
                connectionId: z.string(),
                contextId: z.string(),
                noCache: z.boolean(),
                project: z.boolean(),
                sourcePath: z.union([
                    z.string(),
                    z.null(),
                ]).optional(),
                targetId: z.string(),
            }),
            CpuProfileSnapshotSchema,
        ),
        get_heap_classes: requestType(
            z.object({
                captureId: z.string(),
                connectionId: z.string(),
                contextId: z.string(),
                filter: z.union([
                    z.string(),
                    z.null(),
                ]).optional(),
                noCache: z.boolean(),
                targetId: z.string(),
            }),
            HeapClassSnapshotSchema,
        ),
        get_heap_dominator_chain: requestType(
            z.object({
                connectionId: z.string(),
                contextId: z.string(),
                maxStringLength: z.union([
                    z.uint32(),
                    z.null(),
                ]).optional(),
                reference: z.string(),
                targetId: z.string(),
            }),
            HeapDominatorSnapshotSchema,
        ),
        get_heap_path: requestType(
            z.object({
                connectionId: z.string(),
                contextId: z.string(),
                from: z.string(),
                maxStringLength: z.union([
                    z.uint32(),
                    z.null(),
                ]).optional(),
                options: HeapPathOptionsSchema,
                targetId: z.string(),
                to: z.string(),
            }),
            z.union([
                HeapPathSnapshotSchema,
                z.null(),
            ]),
        ),
        get_heap_references: requestType(
            z.object({
                connectionId: z.string(),
                contextId: z.string(),
                direction: HeapReferenceDirectionSchema,
                edgePolicy: HeapEdgePolicySchema,
                limit: z.int(),
                maxStringLength: z.union([
                    z.uint32(),
                    z.null(),
                ]).optional(),
                reference: z.string(),
                targetId: z.string(),
            }),
            HeapReferencesSnapshotSchema,
        ),
        get_heap_snapshot_progress: requestType(
            z.object({
                connectionId: z.string(),
                contextId: z.string(),
                targetId: z.string(),
            }),
            z.union([
                HeapSnapshotProgressSchema,
                z.null(),
            ]),
        ),
        get_logs: requestType(
            z.object({
                connectionId: z.string(),
                contextId: z.string(),
                targetId: z.string(),
            }),
            TargetLogSnapshotSchema,
        ),
        get_object_properties: requestType(
            z.object({
                connectionId: z.string(),
                contextId: z.string(),
                objectId: z.string(),
                pauseEpoch: z.union([
                    z.int().nonnegative(),
                    z.null(),
                ]).optional(),
                targetId: z.string(),
            }),
            z.array(VariableSnapshotSchema),
        ),
        /**
         * Returns a process-oriented resource projection. Runtime target discovery is performed only
         * for the roots named in `expanded_root_process_ids`.
         */
        get_process_projection: requestType(
            z.object({
                contextId: z.string(),
                expandedRootProcessIds: z.array(z.int()),
            }),
            z.array(ProcessTreeSnapshotSchema),
            {
                description: "Returns a process-oriented resource projection. Runtime target discovery is performed only\nfor the roots named in `expanded_root_process_ids`.",
            },
        ),
        get_resource_graph: requestType(
            z.object({
                contextId: z.string(),
            }),
            ResourceGraphSnapshotSchema,
        ),
        get_scope_variables: requestType(
            z.object({
                connectionId: z.string(),
                contextId: z.string(),
                frameIndex: z.int(),
                pauseEpoch: z.int(),
                scopeIndex: z.int(),
                targetId: z.string(),
            }),
            z.array(VariableSnapshotSchema),
        ),
        get_stored_coverage: requestType(
            z.object({
                captureName: z.string(),
                connectionId: z.union([
                    z.string(),
                    z.null(),
                ]).optional(),
                contextId: z.string(),
                excludeCaptureId: z.union([
                    z.string(),
                    z.null(),
                ]).optional(),
                pathGlob: z.union([
                    z.string(),
                    z.null(),
                ]).optional(),
                sourcePath: z.union([
                    z.string(),
                    z.null(),
                ]).optional(),
                targetId: z.union([
                    z.string(),
                    z.null(),
                ]).optional(),
            }),
            CoverageSnapshotSchema,
        ),
        get_stored_cpu_profile: requestType(
            z.object({
                captureName: z.string(),
                connectionId: z.union([
                    z.string(),
                    z.null(),
                ]).optional(),
                contextId: z.string(),
                sourcePath: z.union([
                    z.string(),
                    z.null(),
                ]).optional(),
                targetId: z.union([
                    z.string(),
                    z.null(),
                ]).optional(),
            }),
            CpuProfileSnapshotSchema,
        ),
        get_stored_heap_classes: requestType(
            z.object({
                captureName: z.string(),
                connectionId: z.union([
                    z.string(),
                    z.null(),
                ]).optional(),
                contextId: z.string(),
                filter: z.union([
                    z.string(),
                    z.null(),
                ]).optional(),
                targetId: z.union([
                    z.string(),
                    z.null(),
                ]).optional(),
            }),
            HeapClassSnapshotSchema,
        ),
        get_target: requestType(
            z.object({
                connectionId: z.string(),
                contextId: z.string(),
                targetId: z.string(),
            }),
            TargetDebuggerSnapshotSchema,
        ),
        grep_sources: requestType(
            z.object({
                contextId: z.string(),
                options: SourceSearchOptionsSchema,
            }),
            SourceSearchSnapshotSchema,
        ),
        inspect_value: requestType(
            z.object({
                connectionId: z.string(),
                contextId: z.string(),
                options: ValueInspectionOptionsSchema,
                pauseEpoch: z.union([
                    z.int().nonnegative(),
                    z.null(),
                ]).optional(),
                selector: ValueSelectorSchema,
                targetId: z.string(),
            }),
            ValueSnapshotSchema,
        ),
        list_captures: requestType(
            z.object({
                contextId: z.string(),
            }),
            z.array(CaptureSnapshotSchema),
        ),
        list_contexts: requestType(
            z.object({
                cwd: z.union([
                    z.string(),
                    z.null(),
                ]).optional(),
            }),
            z.array(ContextSummarySchema),
        ),
        list_sources: requestType(
            z.object({
                contextId: z.string(),
                path: z.union([
                    z.string(),
                    z.null(),
                ]).optional(),
            }),
            z.array(SourceSnapshotInfoSchema),
        ),
        map_source: requestType(
            z.object({
                column: z.int(),
                contextId: z.string(),
                line: z.int(),
                path: z.string(),
            }),
            z.array(SourceMappingSnapshotSchema),
        ),
        observe_context: requestType(
            z.object({
                contextId: z.string(),
                cursor: ObservationCursorSchema,
                timeoutMs: z.int(),
            }),
            ObservationResultSchema,
        ),
        observe_target: requestType(
            z.object({
                afterRevision: z.int(),
                connectionId: z.string(),
                contextId: z.string(),
                targetId: z.string(),
                timeoutMs: z.int(),
            }),
            z.union([
                TargetDebuggerSnapshotSchema,
                z.null(),
            ]),
        ),
        /**
         * Opens a virtual browser-root CDP relay exposing every target across every connection in
         * `context_id` as one endpoint. Takes exclusive relay ownership of the context immediately:
         * ordinary local target debugging commands fail until the relay closes. Does not restart
         * any underlying connection; existing attachments and future ones stay lazy.
         */
        open_context_relay: requestType(
            z.object({
                contextId: z.string(),
            }),
            RelayEndpointSchema,
            {
                description: "Opens a virtual browser-root CDP relay exposing every target across every connection in\n`context_id` as one endpoint. Takes exclusive relay ownership of the context immediately:\nordinary local target debugging commands fail until the relay closes. Does not restart\nany underlying connection; existing attachments and future ones stay lazy.",
            },
        ),
        open_playwright_proxy: requestType(
            z.object({
                connectionId: z.string(),
                contextId: z.string(),
                expectedGeneration: z.int(),
                targetId: z.string(),
            }),
            PlaywrightProxyEndpointSchema,
        ),
        /**
         * Opens a direct-root CDP relay exposing exactly one target. Takes the same exclusive
         * relay ownership of the target's owning context as `open_context_relay`.
         */
        open_target_relay: requestType(
            z.object({
                connectionId: z.string(),
                contextId: z.string(),
                targetId: z.string(),
            }),
            RelayEndpointSchema,
            {
                description: "Opens a direct-root CDP relay exposing exactly one target. Takes the same exclusive\nrelay ownership of the target's owning context as `open_context_relay`.",
            },
        ),
        put_breakpoint: requestType(
            z.object({
                breakpointId: z.string(),
                column: z.int(),
                contextId: z.string(),
                line: z.int(),
                sourcePath: z.string(),
            }),
            ContextSnapshotSchema,
        ),
        put_breakpoint_spec: requestType(
            z.object({
                breakpointId: z.string(),
                contextId: z.string(),
                options: MutationOptionsSchema,
                specification: BreakpointSpecSchema,
            }),
            ContextSnapshotSchema,
        ),
        put_connection: requestType(
            z.object({
                configuration: ConnectionConfigurationSchema,
                connectionId: z.string(),
                contextId: z.string(),
            }),
            ContextSnapshotSchema,
        ),
        put_context: requestType(
            z.object({
                contextId: z.string(),
                displayName: z.union([
                    z.string(),
                    z.null(),
                ]).optional(),
                kind: ContextKindSchema,
            }),
            ContextSnapshotSchema,
        ),
        raw_cdp_request: requestType(
            z.object({
                connectionId: z.string(),
                contextId: z.string(),
                method: z.string(),
                params: z.unknown(),
                targetId: z.string(),
                validate: z.boolean(),
            }),
            z.unknown(),
        ),
        raw_cdp_session_request: requestType(
            z.object({
                connectionId: z.string(),
                contextId: z.string(),
                method: z.string(),
                params: z.unknown(),
                sessionId: z.string(),
                targetId: z.string(),
                validate: z.boolean(),
            }),
            z.unknown(),
        ),
        release_target: requestType(
            z.object({
                connectionId: z.string(),
                contextId: z.string(),
                targetId: z.string(),
            }),
            TargetDebuggerSnapshotSchema,
        ),
        resolve_sources: requestType(
            z.object({
                contextId: z.string(),
                source: z.string(),
            }),
            UncompactedSourceGraphSnapshotSchema,
        ),
        resolve_target: requestType(
            z.object({
                contextId: z.string(),
                selector: z.string(),
            }),
            CanonicalTargetSnapshotSchema,
        ),
        resume_target: requestType(
            z.object({
                connectionId: z.string(),
                contextId: z.string(),
                pauseEpoch: z.int(),
                targetId: z.string(),
            }),
            TargetDebuggerSnapshotSchema,
        ),
        select_heap_nodes: requestType(
            z.object({
                captureId: z.string(),
                connectionId: z.string(),
                contextId: z.string(),
                includeDominators: z.boolean(),
                maxStringLength: z.union([
                    z.uint32(),
                    z.null(),
                ]).optional(),
                selector: HeapNodeSelectorSchema,
                targetId: z.string(),
            }),
            HeapNodeSelectionSnapshotSchema,
        ),
        select_promises: requestType(
            z.object({
                captureId: z.string(),
                connectionId: z.string(),
                contextId: z.string(),
                limit: z.int(),
                maxPreviewLength: z.int(),
                state: z.union([
                    PromiseStateSchema,
                    z.null(),
                ]).optional(),
                targetId: z.string(),
            }),
            PromiseSelectionSnapshotSchema,
        ),
        service_info: requestType(
            z.object({}),
            ServiceInfoSchema,
        ),
        set_logpoint: requestType(
            z.object({
                column: z.int(),
                connectionId: z.string(),
                contextId: z.string(),
                expression: z.string(),
                line: z.int(),
                logpointId: z.string(),
                sourceUrl: z.string(),
                targetId: z.string(),
            }),
            TargetDebuggerSnapshotSchema,
        ),
        set_logpoints: requestType(
            z.object({
                connectionId: z.string(),
                contextId: z.string(),
                logpoints: z.array(LogpointSpecSchema),
                targetId: z.string(),
            }),
            TargetDebuggerSnapshotSchema,
        ),
        set_pause_future_children: requestType(
            z.object({
                connectionId: z.string(),
                contextId: z.string(),
                enabled: z.boolean(),
            }),
            z.boolean(),
        ),
        set_source_formatting: requestType(
            z.object({
                contextId: z.string(),
                mode: SourceFormattingModeSchema,
            }),
            SourceFormattingSettingsSchema,
        ),
        show_source: requestType(
            z.object({
                contextId: z.string(),
                options: SourceDisplayOptionsSchema,
                path: z.string(),
            }),
            SourceContentSnapshotSchema,
        ),
        show_source_graph: requestType(
            z.object({
                contextId: z.string(),
            }),
            CompactedSourceGraphSnapshotSchema,
        ),
        show_source_tree: requestType(
            z.object({
                contextId: z.string(),
                kind: SourceTreeKindSchema,
            }),
            SourceTreeSnapshotSchema,
        ),
        show_uncompacted_source_graph: requestType(
            z.object({
                contextId: z.string(),
            }),
            UncompactedSourceGraphSnapshotSchema,
        ),
        shutdown: requestType(
            z.object({}),
            z.boolean(),
        ),
        start_coverage: requestType(
            z.object({
                connectionId: z.string(),
                contextId: z.string(),
                targetId: z.string(),
            }),
            z.boolean(),
        ),
        start_cpu_profile: requestType(
            z.object({
                connectionId: z.string(),
                contextId: z.string(),
                samplingIntervalMicros: z.union([
                    z.int().nonnegative(),
                    z.null(),
                ]).optional(),
                targetId: z.string(),
            }),
            z.boolean(),
        ),
        step_target: requestType(
            z.object({
                connectionId: z.string(),
                contextId: z.string(),
                kind: StepKindSchema,
                pauseEpoch: z.int(),
                targetId: z.string(),
            }),
            TargetDebuggerSnapshotSchema,
        ),
        stop_coverage: requestType(
            z.object({
                captureId: z.union([
                    z.string(),
                    z.null(),
                ]).optional(),
                connectionId: z.string(),
                contextId: z.string(),
                targetId: z.string(),
            }),
            CoverageSnapshotSchema,
        ),
        stop_cpu_profile: requestType(
            z.object({
                captureId: z.union([
                    z.string(),
                    z.null(),
                ]).optional(),
                connectionId: z.string(),
                contextId: z.string(),
                targetId: z.string(),
            }),
            CpuProfileSnapshotSchema,
        ),
        supply_stored_heap_source_map: requestType(
            z.object({
                captureName: z.string(),
                contextId: z.string(),
                supply: HeapSourceMapSupplySchema,
            }),
            z.null(),
        ),
        take_coverage: requestType(
            z.object({
                captureId: z.union([
                    z.string(),
                    z.null(),
                ]).optional(),
                connectionId: z.string(),
                contextId: z.string(),
                raw: z.union([
                    z.boolean(),
                    z.null(),
                ]).optional(),
                targetId: z.string(),
            }),
            CoverageSnapshotSchema,
        ),
        take_heap_snapshot: requestType(
            z.object({
                captureNumericValue: z.boolean(),
                connectionId: z.string(),
                contextId: z.string(),
                exposeInternals: z.boolean(),
                path: z.string(),
                targetId: z.string(),
            }),
            HeapSnapshotResultSchema,
        ).withStream({
            server: HeapSnapshotProgressSchema,
        }),
        type_target: requestType(
            z.object({
                connectionId: z.string(),
                contextId: z.string(),
                targetId: z.string(),
                text: z.string(),
            }),
            z.boolean(),
        ),
        wait_target: requestType(
            z.object({
                connectionId: z.string(),
                contextId: z.string(),
                predicate: TargetWaitPredicateSchema,
                targetId: z.string(),
                timeoutMs: z.int(),
            }),
            TargetDebuggerSnapshotSchema,
        ),
    },
    { frozenSchema: wireSchema },
);
