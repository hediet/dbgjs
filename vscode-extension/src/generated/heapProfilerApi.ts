import { InterfaceDefinition, notificationType, requestType, type LinkRpcInterfaceSchema } from "@hediet/linkrpc";
import { z } from "zod";

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

const SourceLocationSchema = z.object({
    column: z.int(),
    line: z.int(),
    sourceUrl: z.string(),
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

const PromiseClassificationSchema = z.literal("indeterminate");

const PromiseOriginSchema = z.enum(["live", "heapSnapshot"]);

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

const wireSchema: LinkRpcInterfaceSchema = JSON.parse("{\"id\":\"dev.dbgjs.heap-profiler\",\"hash\":\"747736fee3ab8651\",\"methods\":{\"aggregate_heap_snapshot\":{\"params\":{\"type\":\"object\",\"required\":[\"by\",\"captureId\",\"connectionId\",\"contextId\",\"limit\",\"targetId\"],\"properties\":{\"by\":{\"$ref\":\"#/components/schemas/HeapAggregateBy\"},\"captureId\":{\"type\":\"string\"},\"connectionId\":{\"type\":\"string\"},\"contextId\":{\"type\":\"string\"},\"limit\":{\"type\":\"integer\"},\"maxStringLength\":{\"type\":[\"integer\",\"null\"],\"format\":\"uint32\"},\"targetId\":{\"type\":\"string\"}},\"additionalProperties\":false},\"result\":{\"$ref\":\"#/components/schemas/HeapAggregateSnapshot\"}},\"capture_heap_snapshot\":{\"params\":{\"type\":\"object\",\"required\":[\"captureNumericValue\",\"connectionId\",\"contextId\",\"exposeInternals\",\"targetId\"],\"properties\":{\"captureId\":{\"type\":[\"string\",\"null\"]},\"captureNumericValue\":{\"type\":\"boolean\"},\"connectionId\":{\"type\":\"string\"},\"contextId\":{\"type\":\"string\"},\"exposeInternals\":{\"type\":\"boolean\"},\"targetId\":{\"type\":\"string\"}},\"additionalProperties\":false},\"result\":{\"$ref\":\"#/components/schemas/HeapCaptureResult\"},\"serverStream\":{\"$ref\":\"#/components/schemas/HeapSnapshotProgress\"}},\"diff_heap_snapshots\":{\"params\":{\"type\":\"object\",\"required\":[\"by\",\"connectionId\",\"contextId\",\"limit\",\"newerCaptureId\",\"olderCaptureId\",\"targetId\"],\"properties\":{\"by\":{\"$ref\":\"#/components/schemas/HeapAggregateBy\"},\"connectionId\":{\"type\":\"string\"},\"contextId\":{\"type\":\"string\"},\"limit\":{\"type\":\"integer\"},\"maxStringLength\":{\"type\":[\"integer\",\"null\"],\"format\":\"uint32\"},\"newerCaptureId\":{\"type\":\"string\"},\"olderCaptureId\":{\"type\":\"string\"},\"targetId\":{\"type\":\"string\"}},\"additionalProperties\":false},\"result\":{\"$ref\":\"#/components/schemas/HeapDiffSnapshot\"}},\"get_heap_classes\":{\"params\":{\"type\":\"object\",\"required\":[\"captureId\",\"connectionId\",\"contextId\",\"noCache\",\"targetId\"],\"properties\":{\"captureId\":{\"type\":\"string\"},\"connectionId\":{\"type\":\"string\"},\"contextId\":{\"type\":\"string\"},\"filter\":{\"type\":[\"string\",\"null\"]},\"noCache\":{\"type\":\"boolean\"},\"targetId\":{\"type\":\"string\"}},\"additionalProperties\":false},\"result\":{\"$ref\":\"#/components/schemas/HeapClassSnapshot\"}},\"get_heap_dominator_chain\":{\"params\":{\"type\":\"object\",\"required\":[\"connectionId\",\"contextId\",\"reference\",\"targetId\"],\"properties\":{\"connectionId\":{\"type\":\"string\"},\"contextId\":{\"type\":\"string\"},\"maxStringLength\":{\"type\":[\"integer\",\"null\"],\"format\":\"uint32\"},\"reference\":{\"type\":\"string\"},\"targetId\":{\"type\":\"string\"}},\"additionalProperties\":false},\"result\":{\"$ref\":\"#/components/schemas/HeapDominatorSnapshot\"}},\"get_heap_path\":{\"params\":{\"type\":\"object\",\"required\":[\"connectionId\",\"contextId\",\"from\",\"options\",\"targetId\",\"to\"],\"properties\":{\"connectionId\":{\"type\":\"string\"},\"contextId\":{\"type\":\"string\"},\"from\":{\"type\":\"string\"},\"maxStringLength\":{\"type\":[\"integer\",\"null\"],\"format\":\"uint32\"},\"options\":{\"$ref\":\"#/components/schemas/HeapPathOptions\"},\"targetId\":{\"type\":\"string\"},\"to\":{\"type\":\"string\"}},\"additionalProperties\":false},\"result\":{\"anyOf\":[{\"$ref\":\"#/components/schemas/HeapPathSnapshot\"},{\"type\":\"null\"}]}},\"get_heap_references\":{\"params\":{\"type\":\"object\",\"required\":[\"connectionId\",\"contextId\",\"direction\",\"edgePolicy\",\"limit\",\"reference\",\"targetId\"],\"properties\":{\"connectionId\":{\"type\":\"string\"},\"contextId\":{\"type\":\"string\"},\"direction\":{\"$ref\":\"#/components/schemas/HeapReferenceDirection\"},\"edgePolicy\":{\"$ref\":\"#/components/schemas/HeapEdgePolicy\"},\"limit\":{\"type\":\"integer\"},\"maxStringLength\":{\"type\":[\"integer\",\"null\"],\"format\":\"uint32\"},\"reference\":{\"type\":\"string\"},\"targetId\":{\"type\":\"string\"}},\"additionalProperties\":false},\"result\":{\"$ref\":\"#/components/schemas/HeapReferencesSnapshot\"}},\"get_heap_snapshot_progress\":{\"params\":{\"type\":\"object\",\"required\":[\"connectionId\",\"contextId\",\"targetId\"],\"properties\":{\"connectionId\":{\"type\":\"string\"},\"contextId\":{\"type\":\"string\"},\"targetId\":{\"type\":\"string\"}},\"additionalProperties\":false},\"result\":{\"anyOf\":[{\"$ref\":\"#/components/schemas/HeapSnapshotProgress\"},{\"type\":\"null\"}]}},\"select_heap_nodes\":{\"params\":{\"type\":\"object\",\"required\":[\"captureId\",\"connectionId\",\"contextId\",\"includeDominators\",\"selector\",\"targetId\"],\"properties\":{\"captureId\":{\"type\":\"string\"},\"connectionId\":{\"type\":\"string\"},\"contextId\":{\"type\":\"string\"},\"includeDominators\":{\"type\":\"boolean\"},\"maxStringLength\":{\"type\":[\"integer\",\"null\"],\"format\":\"uint32\"},\"selector\":{\"$ref\":\"#/components/schemas/HeapNodeSelector\"},\"targetId\":{\"type\":\"string\"}},\"additionalProperties\":false},\"result\":{\"$ref\":\"#/components/schemas/HeapNodeSelectionSnapshot\"}},\"select_promises\":{\"params\":{\"type\":\"object\",\"required\":[\"captureId\",\"connectionId\",\"contextId\",\"limit\",\"maxPreviewLength\",\"targetId\"],\"properties\":{\"captureId\":{\"type\":\"string\"},\"connectionId\":{\"type\":\"string\"},\"contextId\":{\"type\":\"string\"},\"limit\":{\"type\":\"integer\"},\"maxPreviewLength\":{\"type\":\"integer\"},\"state\":{\"anyOf\":[{\"$ref\":\"#/components/schemas/PromiseState\"},{\"type\":\"null\"}]},\"targetId\":{\"type\":\"string\"}},\"additionalProperties\":false},\"result\":{\"$ref\":\"#/components/schemas/PromiseSelectionSnapshot\"}},\"take_heap_snapshot\":{\"params\":{\"type\":\"object\",\"required\":[\"captureNumericValue\",\"connectionId\",\"contextId\",\"exposeInternals\",\"path\",\"targetId\"],\"properties\":{\"captureNumericValue\":{\"type\":\"boolean\"},\"connectionId\":{\"type\":\"string\"},\"contextId\":{\"type\":\"string\"},\"exposeInternals\":{\"type\":\"boolean\"},\"path\":{\"type\":\"string\"},\"targetId\":{\"type\":\"string\"}},\"additionalProperties\":false},\"result\":{\"$ref\":\"#/components/schemas/HeapSnapshotResult\"},\"serverStream\":{\"$ref\":\"#/components/schemas/HeapSnapshotProgress\"}}},\"components\":{\"schemas\":{\"HeapAggregateBy\":{\"type\":\"string\",\"enum\":[\"nodeType\",\"name\",\"stringValue\"]},\"HeapAggregateEntrySnapshot\":{\"type\":\"object\",\"required\":[\"count\",\"key\",\"keyTruncated\",\"shallowSize\"],\"properties\":{\"count\":{\"type\":\"integer\"},\"key\":{\"type\":\"string\"},\"keyTruncated\":{\"type\":\"boolean\"},\"shallowSize\":{\"type\":\"integer\"}},\"additionalProperties\":false},\"HeapAggregateSnapshot\":{\"type\":\"object\",\"required\":[\"by\",\"captureId\",\"entries\",\"omittedEntryCount\"],\"properties\":{\"by\":{\"$ref\":\"#/components/schemas/HeapAggregateBy\"},\"captureId\":{\"type\":\"string\"},\"entries\":{\"type\":\"array\",\"items\":{\"$ref\":\"#/components/schemas/HeapAggregateEntrySnapshot\"}},\"incompleteStringCount\":{\"type\":\"integer\"},\"omittedEntryCount\":{\"type\":\"integer\"}},\"additionalProperties\":false},\"HeapCaptureResult\":{\"type\":\"object\",\"required\":[\"bytesWritten\",\"captureId\",\"timing\"],\"properties\":{\"bytesWritten\":{\"type\":\"integer\"},\"captureId\":{\"type\":\"string\"},\"timing\":{\"$ref\":\"#/components/schemas/HeapSnapshotTiming\"}},\"additionalProperties\":false},\"HeapClassAnalysisSnapshot\":{\"type\":\"object\",\"required\":[\"constructorGroupCount\",\"parseDurationMicros\",\"projectionDurationMicros\",\"sourceMapHydrationDurationMicros\",\"usedCachedGroups\"],\"properties\":{\"constructorGroupCount\":{\"type\":\"integer\"},\"mappingStatus\":{\"$ref\":\"#/components/schemas/HeapMappingStatus\"},\"parseDurationMicros\":{\"type\":\"integer\"},\"projectionDurationMicros\":{\"type\":\"integer\"},\"scriptMappings\":{\"type\":\"array\",\"items\":{\"$ref\":\"#/components/schemas/HeapScriptMappingDiagnostic\"}},\"snapshotTiming\":{\"anyOf\":[{\"$ref\":\"#/components/schemas/HeapSnapshotTiming\"},{\"type\":\"null\"}]},\"sourceMapHydrationDurationMicros\":{\"type\":\"integer\"},\"usedCachedGroups\":{\"type\":\"boolean\"}},\"additionalProperties\":false},\"HeapClassSnapshot\":{\"type\":\"object\",\"required\":[\"analysis\",\"captureId\",\"classes\",\"totalInstances\",\"totalShallowSize\"],\"properties\":{\"analysis\":{\"$ref\":\"#/components/schemas/HeapClassAnalysisSnapshot\"},\"captureId\":{\"type\":\"string\"},\"classes\":{\"type\":\"array\",\"items\":{\"$ref\":\"#/components/schemas/HeapClassSnapshotEntry\"}},\"totalInstances\":{\"type\":\"integer\"},\"totalShallowSize\":{\"type\":\"integer\"}},\"additionalProperties\":false},\"HeapClassSnapshotEntry\":{\"type\":\"object\",\"required\":[\"generatedName\",\"instanceCount\",\"instances\",\"location\",\"name\",\"omittedInstanceCount\",\"shallowSize\",\"sourceUrl\"],\"properties\":{\"generatedName\":{\"type\":\"string\"},\"instanceCount\":{\"type\":\"integer\"},\"instances\":{\"type\":\"array\",\"items\":{\"$ref\":\"#/components/schemas/HeapInstanceSnapshot\"}},\"location\":{\"$ref\":\"#/components/schemas/SourceLocation\"},\"name\":{\"type\":\"string\"},\"omittedInstanceCount\":{\"type\":\"integer\"},\"provenance\":{\"$ref\":\"#/components/schemas/ScriptProvenance\"},\"scriptId\":{\"type\":\"string\"},\"shallowSize\":{\"type\":\"integer\"},\"sourceUrl\":{\"type\":\"string\"}},\"additionalProperties\":false},\"HeapDiffEntrySnapshot\":{\"type\":\"object\",\"required\":[\"countDelta\",\"key\",\"keyTruncated\",\"shallowSizeDelta\"],\"properties\":{\"countDelta\":{\"type\":\"integer\"},\"key\":{\"type\":\"string\"},\"keyTruncated\":{\"type\":\"boolean\"},\"shallowSizeDelta\":{\"type\":\"integer\"}},\"additionalProperties\":false},\"HeapDiffSnapshot\":{\"type\":\"object\",\"required\":[\"by\",\"entries\",\"newerCaptureId\",\"olderCaptureId\"],\"properties\":{\"by\":{\"$ref\":\"#/components/schemas/HeapAggregateBy\"},\"entries\":{\"type\":\"array\",\"items\":{\"$ref\":\"#/components/schemas/HeapDiffEntrySnapshot\"}},\"newerCaptureId\":{\"type\":\"string\"},\"newerIncompleteStringCount\":{\"type\":\"integer\"},\"olderCaptureId\":{\"type\":\"string\"},\"olderIncompleteStringCount\":{\"type\":\"integer\"}},\"additionalProperties\":false},\"HeapDominatorSnapshot\":{\"type\":\"object\",\"required\":[\"captureId\",\"chain\",\"node\"],\"properties\":{\"captureId\":{\"type\":\"string\"},\"chain\":{\"type\":\"array\",\"items\":{\"$ref\":\"#/components/schemas/HeapNodeSnapshot\"}},\"node\":{\"$ref\":\"#/components/schemas/HeapNodeSnapshot\"}},\"additionalProperties\":false},\"HeapEdgePolicy\":{\"type\":\"string\",\"enum\":[\"strong\",\"all\"]},\"HeapInstanceSnapshot\":{\"type\":\"object\",\"required\":[\"alias\",\"heapObjectId\",\"shallowSize\"],\"properties\":{\"alias\":{\"type\":\"string\"},\"heapObjectId\":{\"type\":\"string\"},\"shallowSize\":{\"type\":\"integer\"}},\"additionalProperties\":false},\"HeapMappingStatus\":{\"type\":\"string\",\"enum\":[\"notAttempted\",\"noMapSupplied\",\"mapLoadingFailed\",\"mapped\"]},\"HeapNodeLocationSnapshot\":{\"type\":\"object\",\"required\":[\"column\",\"line\",\"scriptId\"],\"properties\":{\"column\":{\"type\":\"integer\"},\"line\":{\"type\":\"integer\"},\"scriptId\":{\"type\":\"integer\"}},\"additionalProperties\":false},\"HeapNodeSelectionSnapshot\":{\"type\":\"object\",\"required\":[\"captureId\",\"graphParseDurationMicros\",\"nodes\",\"totalEdges\",\"totalNodes\",\"usedCachedGraph\"],\"properties\":{\"captureId\":{\"type\":\"string\"},\"graphParseDurationMicros\":{\"type\":\"integer\"},\"incompleteStringCount\":{\"type\":\"integer\"},\"nodes\":{\"type\":\"array\",\"items\":{\"$ref\":\"#/components/schemas/HeapNodeSnapshot\"}},\"totalEdges\":{\"type\":\"integer\"},\"totalNodes\":{\"type\":\"integer\"},\"usedCachedGraph\":{\"type\":\"boolean\"}},\"additionalProperties\":false},\"HeapNodeSelector\":{\"type\":\"object\",\"properties\":{\"heapObjectId\":{\"type\":[\"string\",\"null\"]},\"limit\":{\"type\":[\"integer\",\"null\"],\"format\":\"uint32\"},\"maxShallowSize\":{\"type\":[\"integer\",\"null\"],\"format\":\"uint64\"},\"minShallowSize\":{\"type\":[\"integer\",\"null\"],\"format\":\"uint64\"},\"name\":{\"type\":[\"string\",\"null\"]},\"nameRegex\":{\"type\":[\"string\",\"null\"]},\"nodeType\":{\"type\":[\"string\",\"null\"]},\"stringContains\":{\"type\":[\"string\",\"null\"]},\"stringRegex\":{\"type\":[\"string\",\"null\"]}},\"additionalProperties\":false},\"HeapNodeSnapshot\":{\"type\":\"object\",\"required\":[\"heapObjectId\",\"incomingReferenceCount\",\"locations\",\"name\",\"nodeIndex\",\"nodeType\",\"outgoingReferenceCount\",\"reference\",\"shallowSize\",\"stringTruncated\"],\"properties\":{\"heapObjectId\":{\"type\":\"string\"},\"immediateDominator\":{\"type\":[\"string\",\"null\"]},\"incomingReferenceCount\":{\"type\":\"integer\"},\"locations\":{\"type\":\"array\",\"items\":{\"$ref\":\"#/components/schemas/HeapNodeLocationSnapshot\"}},\"name\":{\"type\":\"string\"},\"nodeIndex\":{\"type\":\"integer\"},\"nodeType\":{\"type\":\"string\"},\"outgoingReferenceCount\":{\"type\":\"integer\"},\"preview\":{\"type\":[\"string\",\"null\"]},\"reference\":{\"type\":\"string\"},\"retainedSize\":{\"type\":[\"integer\",\"null\"],\"format\":\"uint64\"},\"shallowSize\":{\"type\":\"integer\"},\"source\":{\"$ref\":\"#/components/schemas/ObjectSourceSnapshot\"},\"stringTruncated\":{\"type\":\"boolean\"},\"stringValue\":{\"type\":[\"string\",\"null\"]}},\"additionalProperties\":false},\"HeapPathCost\":{\"type\":\"string\",\"enum\":[\"edges\",\"readable\"]},\"HeapPathDirection\":{\"type\":\"string\",\"enum\":[\"outgoing\",\"incoming\",\"either\"]},\"HeapPathOptions\":{\"type\":\"object\",\"required\":[\"cost\",\"direction\",\"edgePolicy\"],\"properties\":{\"cost\":{\"$ref\":\"#/components/schemas/HeapPathCost\"},\"direction\":{\"$ref\":\"#/components/schemas/HeapPathDirection\"},\"edgePolicy\":{\"$ref\":\"#/components/schemas/HeapEdgePolicy\"}},\"additionalProperties\":false},\"HeapPathSnapshot\":{\"type\":\"object\",\"required\":[\"captureId\",\"cost\",\"from\",\"nodes\",\"steps\",\"to\"],\"properties\":{\"captureId\":{\"type\":\"string\"},\"cost\":{\"type\":\"integer\"},\"from\":{\"type\":\"string\"},\"nodes\":{\"type\":\"array\",\"items\":{\"$ref\":\"#/components/schemas/HeapNodeSnapshot\"}},\"steps\":{\"type\":\"array\",\"items\":{\"$ref\":\"#/components/schemas/HeapPathStepSnapshot\"}},\"to\":{\"type\":\"string\"}},\"additionalProperties\":false},\"HeapPathStepSnapshot\":{\"type\":\"object\",\"required\":[\"direction\",\"edgeIndex\",\"edgeType\",\"from\",\"nameOrIndex\",\"to\"],\"properties\":{\"direction\":{\"$ref\":\"#/components/schemas/HeapTraversalDirection\"},\"edgeIndex\":{\"type\":\"integer\"},\"edgeType\":{\"type\":\"string\"},\"from\":{\"type\":\"string\"},\"name\":{\"type\":[\"string\",\"null\"]},\"nameOrIndex\":{\"type\":\"integer\"},\"to\":{\"type\":\"string\"}},\"additionalProperties\":false},\"HeapReferenceDirection\":{\"type\":\"string\",\"enum\":[\"incoming\",\"outgoing\",\"both\"]},\"HeapReferenceSnapshot\":{\"type\":\"object\",\"required\":[\"edgeIndex\",\"edgeType\",\"nameOrIndex\",\"source\",\"target\"],\"properties\":{\"edgeIndex\":{\"type\":\"integer\"},\"edgeType\":{\"type\":\"string\"},\"name\":{\"type\":[\"string\",\"null\"]},\"nameOrIndex\":{\"type\":\"integer\"},\"source\":{\"type\":\"string\"},\"sourceLocations\":{\"$ref\":\"#/components/schemas/ObjectSourceSnapshot\"},\"sourcePreview\":{\"type\":[\"string\",\"null\"]},\"target\":{\"type\":\"string\"},\"targetLocations\":{\"$ref\":\"#/components/schemas/ObjectSourceSnapshot\"},\"targetPreview\":{\"type\":[\"string\",\"null\"]}},\"additionalProperties\":false},\"HeapReferencesSnapshot\":{\"type\":\"object\",\"required\":[\"captureId\",\"direction\",\"edgePolicy\",\"node\",\"omittedReferenceCount\",\"references\"],\"properties\":{\"captureId\":{\"type\":\"string\"},\"direction\":{\"$ref\":\"#/components/schemas/HeapReferenceDirection\"},\"edgePolicy\":{\"$ref\":\"#/components/schemas/HeapEdgePolicy\"},\"node\":{\"$ref\":\"#/components/schemas/HeapNodeSnapshot\"},\"omittedReferenceCount\":{\"type\":\"integer\"},\"references\":{\"type\":\"array\",\"items\":{\"$ref\":\"#/components/schemas/HeapReferenceSnapshot\"}}},\"additionalProperties\":false},\"HeapScriptMappingDiagnostic\":{\"type\":\"object\",\"required\":[\"hash\",\"provenance\",\"scriptId\",\"status\",\"url\"],\"properties\":{\"diagnostic\":{\"type\":[\"string\",\"null\"]},\"hash\":{\"type\":\"string\"},\"provenance\":{\"$ref\":\"#/components/schemas/ScriptProvenance\"},\"scriptId\":{\"type\":\"string\"},\"status\":{\"$ref\":\"#/components/schemas/HeapMappingStatus\"},\"url\":{\"type\":\"string\"}},\"additionalProperties\":false},\"HeapSnapshotProgress\":{\"type\":\"object\",\"required\":[\"bytesWritten\",\"done\",\"total\"],\"properties\":{\"bytesWritten\":{\"type\":\"integer\"},\"done\":{\"type\":\"integer\"},\"finished\":{\"type\":[\"boolean\",\"null\"]},\"total\":{\"type\":\"integer\"}},\"additionalProperties\":false},\"HeapSnapshotResult\":{\"type\":\"object\",\"required\":[\"bytesWritten\",\"path\",\"timing\"],\"properties\":{\"bytesWritten\":{\"type\":\"integer\"},\"path\":{\"type\":\"string\"},\"timing\":{\"$ref\":\"#/components/schemas/HeapSnapshotTiming\"}},\"additionalProperties\":false},\"HeapSnapshotTiming\":{\"type\":\"object\",\"required\":[\"retrievingDurationMicros\",\"takingDurationMicros\"],\"properties\":{\"retrievingDurationMicros\":{\"type\":\"integer\"},\"takingDurationMicros\":{\"type\":\"integer\"}},\"additionalProperties\":false},\"HeapTraversalDirection\":{\"type\":\"string\",\"enum\":[\"outgoing\",\"incoming\"]},\"ObjectLocationSnapshot\":{\"type\":\"object\",\"required\":[\"kind\",\"origin\",\"position\",\"scriptId\"],\"properties\":{\"kind\":{\"type\":\"string\"},\"origin\":{\"type\":\"string\"},\"position\":{\"$ref\":\"#/components/schemas/ResolvedSourcePosition\"},\"scriptId\":{\"type\":\"string\"}},\"additionalProperties\":false},\"ObjectSourceSnapshot\":{\"type\":\"object\",\"required\":[\"diagnostics\",\"locations\"],\"properties\":{\"diagnostics\":{\"type\":\"array\",\"items\":{\"type\":\"string\"}},\"locations\":{\"type\":\"array\",\"items\":{\"$ref\":\"#/components/schemas/ObjectLocationSnapshot\"}}},\"additionalProperties\":false},\"PromiseClassification\":{\"type\":\"string\",\"const\":\"indeterminate\"},\"PromiseOrigin\":{\"type\":\"string\",\"enum\":[\"live\",\"heapSnapshot\"]},\"PromiseSelectionSnapshot\":{\"type\":\"object\",\"required\":[\"captureId\",\"graphParseDurationMicros\",\"omittedPromiseCount\",\"promises\",\"totalPromises\",\"usedCachedGraph\"],\"properties\":{\"captureId\":{\"type\":\"string\"},\"graphParseDurationMicros\":{\"type\":\"integer\"},\"omittedPromiseCount\":{\"type\":\"integer\"},\"promises\":{\"type\":\"array\",\"items\":{\"$ref\":\"#/components/schemas/PromiseSnapshot\"}},\"totalPromises\":{\"type\":\"integer\"},\"usedCachedGraph\":{\"type\":\"boolean\"}},\"additionalProperties\":false},\"PromiseSnapshot\":{\"type\":\"object\",\"required\":[\"classification\",\"origin\",\"state\"],\"properties\":{\"classification\":{\"$ref\":\"#/components/schemas/PromiseClassification\"},\"origin\":{\"$ref\":\"#/components/schemas/PromiseOrigin\"},\"reference\":{\"type\":[\"string\",\"null\"]},\"retained\":{\"type\":[\"boolean\",\"null\"]},\"settlement\":{\"anyOf\":[{\"$ref\":\"#/components/schemas/ValuePreviewSnapshot\"},{\"type\":\"null\"}]},\"state\":{\"$ref\":\"#/components/schemas/PromiseState\"}},\"additionalProperties\":false},\"PromiseState\":{\"type\":\"string\",\"enum\":[\"pending\",\"fulfilled\",\"rejected\",\"unknown\"]},\"ResolvedSourcePosition\":{\"description\":\"Best available source coordinates. Both locations use 1-based lines and UTF-16 columns; URLs are never shortened for display.\",\"type\":\"object\",\"required\":[\"generated\",\"mapping\",\"resolved\"],\"properties\":{\"breadcrumb\":{\"type\":[\"string\",\"null\"]},\"diagnostic\":{\"type\":[\"string\",\"null\"]},\"generated\":{\"$ref\":\"#/components/schemas/SourceLocation\"},\"mapping\":{\"type\":\"string\"},\"resolved\":{\"$ref\":\"#/components/schemas/SourceLocation\"}},\"additionalProperties\":false},\"ScriptProvenance\":{\"type\":\"object\",\"properties\":{\"executionContextAuxData\":true,\"executionContextId\":{\"type\":[\"integer\",\"null\"],\"format\":\"int64\"},\"frameId\":{\"type\":[\"string\",\"null\"]}},\"additionalProperties\":false},\"SourceLocation\":{\"type\":\"object\",\"required\":[\"column\",\"line\",\"sourceUrl\"],\"properties\":{\"column\":{\"type\":\"integer\"},\"line\":{\"type\":\"integer\"},\"sourceUrl\":{\"type\":\"string\"}},\"additionalProperties\":false},\"ValuePreviewSnapshot\":{\"type\":\"object\",\"required\":[\"kind\",\"truncated\"],\"properties\":{\"kind\":{\"type\":\"string\"},\"preview\":{\"type\":[\"string\",\"null\"]},\"reference\":{\"type\":[\"string\",\"null\"]},\"source\":{\"$ref\":\"#/components/schemas/ObjectSourceSnapshot\"},\"truncated\":{\"type\":\"boolean\"}},\"additionalProperties\":false}}}}");

export const HeapProfilerApi = new InterfaceDefinition(
    {
        id: "dev.dbgjs.heap-profiler",
        hash: "747736fee3ab8651",
    },
    {
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
    },
    { frozenSchema: wireSchema },
);
