import { InterfaceDefinition, notificationType, requestType, type LinkRpcInterfaceSchema } from "@hediet/linkrpc";
import { z } from "zod";

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

const HeapSnapshotTimingSchema = z.object({
    retrievingDurationMicros: z.int(),
    takingDurationMicros: z.int(),
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

const HeapSourceMapSupplySchema = z.object({
    scriptHash: z.string(),
    scriptId: z.string(),
    sourceMap: z.string(),
    sourceMapUrl: z.string(),
});

const wireSchema: LinkRpcInterfaceSchema = JSON.parse("{\"id\":\"dev.dbgjs.capture\",\"hash\":\"f20d251702d54ffe\",\"methods\":{\"delete_capture\":{\"params\":{\"type\":\"object\",\"required\":[\"captureName\",\"contextId\"],\"properties\":{\"captureName\":{\"type\":\"string\"},\"contextId\":{\"type\":\"string\"}},\"additionalProperties\":false},\"result\":{\"type\":\"boolean\"}},\"get_capture\":{\"params\":{\"type\":\"object\",\"required\":[\"captureName\",\"contextId\"],\"properties\":{\"captureName\":{\"type\":\"string\"},\"contextId\":{\"type\":\"string\"}},\"additionalProperties\":false},\"result\":{\"$ref\":\"#/components/schemas/CaptureSnapshot\"}},\"get_stored_coverage\":{\"params\":{\"type\":\"object\",\"required\":[\"captureName\",\"contextId\"],\"properties\":{\"captureName\":{\"type\":\"string\"},\"connectionId\":{\"type\":[\"string\",\"null\"]},\"contextId\":{\"type\":\"string\"},\"excludeCaptureId\":{\"type\":[\"string\",\"null\"]},\"pathGlob\":{\"type\":[\"string\",\"null\"]},\"sourcePath\":{\"type\":[\"string\",\"null\"]},\"targetId\":{\"type\":[\"string\",\"null\"]}},\"additionalProperties\":false},\"result\":{\"$ref\":\"#/components/schemas/CoverageSnapshot\"}},\"get_stored_cpu_profile\":{\"params\":{\"type\":\"object\",\"required\":[\"captureName\",\"contextId\"],\"properties\":{\"captureName\":{\"type\":\"string\"},\"connectionId\":{\"type\":[\"string\",\"null\"]},\"contextId\":{\"type\":\"string\"},\"sourcePath\":{\"type\":[\"string\",\"null\"]},\"targetId\":{\"type\":[\"string\",\"null\"]}},\"additionalProperties\":false},\"result\":{\"$ref\":\"#/components/schemas/CpuProfileSnapshot\"}},\"get_stored_heap_classes\":{\"params\":{\"type\":\"object\",\"required\":[\"captureName\",\"contextId\"],\"properties\":{\"captureName\":{\"type\":\"string\"},\"connectionId\":{\"type\":[\"string\",\"null\"]},\"contextId\":{\"type\":\"string\"},\"filter\":{\"type\":[\"string\",\"null\"]},\"targetId\":{\"type\":[\"string\",\"null\"]}},\"additionalProperties\":false},\"result\":{\"$ref\":\"#/components/schemas/HeapClassSnapshot\"}},\"list_captures\":{\"params\":{\"type\":\"object\",\"required\":[\"contextId\"],\"properties\":{\"contextId\":{\"type\":\"string\"}},\"additionalProperties\":false},\"result\":{\"type\":\"array\",\"items\":{\"$ref\":\"#/components/schemas/CaptureSnapshot\"}}},\"supply_stored_heap_source_map\":{\"params\":{\"type\":\"object\",\"required\":[\"captureName\",\"contextId\",\"supply\"],\"properties\":{\"captureName\":{\"type\":\"string\"},\"contextId\":{\"type\":\"string\"},\"supply\":{\"$ref\":\"#/components/schemas/HeapSourceMapSupply\"}},\"additionalProperties\":false},\"result\":{\"type\":\"null\"}}},\"components\":{\"schemas\":{\"CaptureKind\":{\"type\":\"string\",\"enum\":[\"coverage\",\"cpuProfile\",\"heapSnapshot\"]},\"CaptureSnapshot\":{\"type\":\"object\",\"required\":[\"connectionGeneration\",\"connectionId\",\"contextId\",\"kind\",\"name\",\"storageId\",\"targetId\"],\"properties\":{\"connectionGeneration\":{\"type\":\"integer\"},\"connectionId\":{\"type\":\"string\"},\"contextId\":{\"type\":\"string\"},\"kind\":{\"$ref\":\"#/components/schemas/CaptureKind\"},\"name\":{\"type\":\"string\"},\"storageId\":{\"type\":\"string\"},\"targetId\":{\"type\":\"string\"}},\"additionalProperties\":false},\"CoverageAnalysisSnapshot\":{\"type\":\"object\",\"required\":[\"durationMicros\",\"sourceMapCacheBypasses\",\"sourceMapCacheHits\",\"sourceMapCacheMisses\"],\"properties\":{\"durationMicros\":{\"type\":\"integer\"},\"sourceMapCacheBypasses\":{\"type\":\"integer\"},\"sourceMapCacheHits\":{\"type\":\"integer\"},\"sourceMapCacheMisses\":{\"type\":\"integer\"}},\"additionalProperties\":false},\"CoverageFunctionSnapshot\":{\"type\":\"object\",\"required\":[\"blockCoverage\",\"name\",\"ranges\",\"rootEndOffset\",\"rootStartOffset\"],\"properties\":{\"authoredLocation\":{\"anyOf\":[{\"$ref\":\"#/components/schemas/SourceLocation\"},{\"type\":\"null\"}]},\"blockCoverage\":{\"type\":\"boolean\"},\"breadcrumb\":{\"type\":[\"string\",\"null\"]},\"effectiveRanges\":{\"type\":\"array\",\"items\":{\"$ref\":\"#/components/schemas/CoverageRangeSnapshot\"}},\"generatedLocation\":{\"anyOf\":[{\"$ref\":\"#/components/schemas/SourceLocation\"},{\"type\":\"null\"}]},\"name\":{\"type\":\"string\"},\"ranges\":{\"type\":\"array\",\"items\":{\"$ref\":\"#/components/schemas/CoverageRangeSnapshot\"}},\"rootEndOffset\":{\"type\":\"integer\"},\"rootStartOffset\":{\"type\":\"integer\"}},\"additionalProperties\":false},\"CoverageRangeSnapshot\":{\"type\":\"object\",\"required\":[\"count\",\"endOffset\",\"startOffset\"],\"properties\":{\"authoredEnd\":{\"anyOf\":[{\"$ref\":\"#/components/schemas/SourceLocation\"},{\"type\":\"null\"}]},\"authoredStart\":{\"anyOf\":[{\"$ref\":\"#/components/schemas/SourceLocation\"},{\"type\":\"null\"}]},\"count\":{\"type\":\"integer\"},\"endOffset\":{\"type\":\"integer\"},\"startOffset\":{\"type\":\"integer\"}},\"additionalProperties\":false},\"CoverageSnapshot\":{\"type\":\"object\",\"required\":[\"sources\",\"timestampMicros\"],\"properties\":{\"analysis\":{\"anyOf\":[{\"$ref\":\"#/components/schemas/CoverageAnalysisSnapshot\"},{\"type\":\"null\"}]},\"captureId\":{\"type\":[\"string\",\"null\"]},\"sources\":{\"type\":\"array\",\"items\":{\"$ref\":\"#/components/schemas/CoverageSourceSnapshot\"}},\"timestampMicros\":{\"type\":\"integer\"}},\"additionalProperties\":false},\"CoverageSourceSnapshot\":{\"type\":\"object\",\"required\":[\"functions\",\"generatedUrl\",\"scriptId\"],\"properties\":{\"associatedAuthoredSource\":{\"type\":[\"string\",\"null\"]},\"functions\":{\"type\":\"array\",\"items\":{\"$ref\":\"#/components/schemas/CoverageFunctionSnapshot\"}},\"generatedUrl\":{\"type\":\"string\"},\"scriptId\":{\"type\":\"string\"}},\"additionalProperties\":false},\"CpuProfileAnalysisSnapshot\":{\"type\":\"object\",\"required\":[\"durationMicros\",\"sourceMapCacheBypasses\",\"sourceMapCacheHits\",\"sourceMapCacheMisses\"],\"properties\":{\"durationMicros\":{\"type\":\"integer\"},\"sourceMapCacheBypasses\":{\"type\":\"integer\"},\"sourceMapCacheHits\":{\"type\":\"integer\"},\"sourceMapCacheMisses\":{\"type\":\"integer\"}},\"additionalProperties\":false},\"CpuProfileCallFrameSnapshot\":{\"type\":\"object\",\"required\":[\"columnNumber\",\"functionName\",\"lineNumber\",\"scriptId\",\"url\"],\"properties\":{\"columnNumber\":{\"type\":\"integer\"},\"functionName\":{\"type\":\"string\"},\"lineNumber\":{\"type\":\"integer\"},\"scriptId\":{\"type\":\"string\"},\"url\":{\"type\":\"string\"}},\"additionalProperties\":false},\"CpuProfileFunctionSnapshot\":{\"type\":\"object\",\"required\":[\"generatedLocation\",\"name\",\"sampleCount\",\"selfTimeMicros\",\"totalTimeMicros\"],\"properties\":{\"authoredLocation\":{\"anyOf\":[{\"$ref\":\"#/components/schemas/SourceLocation\"},{\"type\":\"null\"}]},\"breadcrumb\":{\"type\":[\"string\",\"null\"]},\"generatedLocation\":{\"$ref\":\"#/components/schemas/SourceLocation\"},\"name\":{\"type\":\"string\"},\"sampleCount\":{\"type\":\"integer\"},\"selfTimeMicros\":{\"type\":\"integer\"},\"totalTimeMicros\":{\"type\":\"integer\"}},\"additionalProperties\":false},\"CpuProfileNodeSnapshot\":{\"type\":\"object\",\"required\":[\"callFrame\",\"children\",\"id\",\"positionTicks\",\"sampleCount\",\"selfTimeMicros\",\"totalTimeMicros\"],\"properties\":{\"authoredLocation\":{\"anyOf\":[{\"$ref\":\"#/components/schemas/SourceLocation\"},{\"type\":\"null\"}]},\"breadcrumb\":{\"type\":[\"string\",\"null\"]},\"callFrame\":{\"$ref\":\"#/components/schemas/CpuProfileCallFrameSnapshot\"},\"children\":{\"type\":\"array\",\"items\":{\"type\":\"integer\"}},\"deoptReason\":{\"type\":[\"string\",\"null\"]},\"hitCount\":{\"type\":[\"integer\",\"null\"],\"format\":\"int64\"},\"id\":{\"type\":\"integer\"},\"positionTicks\":{\"type\":\"array\",\"items\":{\"$ref\":\"#/components/schemas/CpuProfilePositionTickSnapshot\"}},\"sampleCount\":{\"type\":\"integer\"},\"selfTimeMicros\":{\"type\":\"integer\"},\"totalTimeMicros\":{\"type\":\"integer\"}},\"additionalProperties\":false},\"CpuProfilePositionTickSnapshot\":{\"type\":\"object\",\"required\":[\"line\",\"ticks\"],\"properties\":{\"line\":{\"type\":\"integer\"},\"ticks\":{\"type\":\"integer\"}},\"additionalProperties\":false},\"CpuProfileSnapshot\":{\"type\":\"object\",\"required\":[\"captureId\",\"endTimeMicros\",\"nodes\",\"samples\",\"startTimeMicros\",\"timeDeltasMicros\"],\"properties\":{\"analysis\":{\"anyOf\":[{\"$ref\":\"#/components/schemas/CpuProfileAnalysisSnapshot\"},{\"type\":\"null\"}]},\"captureId\":{\"type\":\"string\"},\"endTimeMicros\":{\"type\":\"number\"},\"functions\":{\"type\":\"array\",\"items\":{\"$ref\":\"#/components/schemas/CpuProfileFunctionSnapshot\"}},\"nodes\":{\"type\":\"array\",\"items\":{\"$ref\":\"#/components/schemas/CpuProfileNodeSnapshot\"}},\"samples\":{\"type\":\"array\",\"items\":{\"type\":\"integer\"}},\"samplingIntervalMicros\":{\"type\":[\"integer\",\"null\"],\"format\":\"uint64\"},\"startTimeMicros\":{\"type\":\"number\"},\"timeDeltasMicros\":{\"description\":\"Raw CDP timestamp differences in sample order, which need not be chronological.\",\"type\":\"array\",\"items\":{\"type\":\"integer\"}}},\"additionalProperties\":false},\"HeapClassAnalysisSnapshot\":{\"type\":\"object\",\"required\":[\"constructorGroupCount\",\"parseDurationMicros\",\"projectionDurationMicros\",\"sourceMapHydrationDurationMicros\",\"usedCachedGroups\"],\"properties\":{\"constructorGroupCount\":{\"type\":\"integer\"},\"mappingStatus\":{\"$ref\":\"#/components/schemas/HeapMappingStatus\"},\"parseDurationMicros\":{\"type\":\"integer\"},\"projectionDurationMicros\":{\"type\":\"integer\"},\"scriptMappings\":{\"type\":\"array\",\"items\":{\"$ref\":\"#/components/schemas/HeapScriptMappingDiagnostic\"}},\"snapshotTiming\":{\"anyOf\":[{\"$ref\":\"#/components/schemas/HeapSnapshotTiming\"},{\"type\":\"null\"}]},\"sourceMapHydrationDurationMicros\":{\"type\":\"integer\"},\"usedCachedGroups\":{\"type\":\"boolean\"}},\"additionalProperties\":false},\"HeapClassSnapshot\":{\"type\":\"object\",\"required\":[\"analysis\",\"captureId\",\"classes\",\"totalInstances\",\"totalShallowSize\"],\"properties\":{\"analysis\":{\"$ref\":\"#/components/schemas/HeapClassAnalysisSnapshot\"},\"captureId\":{\"type\":\"string\"},\"classes\":{\"type\":\"array\",\"items\":{\"$ref\":\"#/components/schemas/HeapClassSnapshotEntry\"}},\"totalInstances\":{\"type\":\"integer\"},\"totalShallowSize\":{\"type\":\"integer\"}},\"additionalProperties\":false},\"HeapClassSnapshotEntry\":{\"type\":\"object\",\"required\":[\"generatedName\",\"instanceCount\",\"instances\",\"location\",\"name\",\"omittedInstanceCount\",\"shallowSize\",\"sourceUrl\"],\"properties\":{\"generatedName\":{\"type\":\"string\"},\"instanceCount\":{\"type\":\"integer\"},\"instances\":{\"type\":\"array\",\"items\":{\"$ref\":\"#/components/schemas/HeapInstanceSnapshot\"}},\"location\":{\"$ref\":\"#/components/schemas/SourceLocation\"},\"name\":{\"type\":\"string\"},\"omittedInstanceCount\":{\"type\":\"integer\"},\"provenance\":{\"$ref\":\"#/components/schemas/ScriptProvenance\"},\"scriptId\":{\"type\":\"string\"},\"shallowSize\":{\"type\":\"integer\"},\"sourceUrl\":{\"type\":\"string\"}},\"additionalProperties\":false},\"HeapInstanceSnapshot\":{\"type\":\"object\",\"required\":[\"alias\",\"heapObjectId\",\"shallowSize\"],\"properties\":{\"alias\":{\"type\":\"string\"},\"heapObjectId\":{\"type\":\"string\"},\"shallowSize\":{\"type\":\"integer\"}},\"additionalProperties\":false},\"HeapMappingStatus\":{\"type\":\"string\",\"enum\":[\"notAttempted\",\"noMapSupplied\",\"mapLoadingFailed\",\"mapped\"]},\"HeapScriptMappingDiagnostic\":{\"type\":\"object\",\"required\":[\"hash\",\"provenance\",\"scriptId\",\"status\",\"url\"],\"properties\":{\"diagnostic\":{\"type\":[\"string\",\"null\"]},\"hash\":{\"type\":\"string\"},\"provenance\":{\"$ref\":\"#/components/schemas/ScriptProvenance\"},\"scriptId\":{\"type\":\"string\"},\"status\":{\"$ref\":\"#/components/schemas/HeapMappingStatus\"},\"url\":{\"type\":\"string\"}},\"additionalProperties\":false},\"HeapSnapshotTiming\":{\"type\":\"object\",\"required\":[\"retrievingDurationMicros\",\"takingDurationMicros\"],\"properties\":{\"retrievingDurationMicros\":{\"type\":\"integer\"},\"takingDurationMicros\":{\"type\":\"integer\"}},\"additionalProperties\":false},\"HeapSourceMapSupply\":{\"type\":\"object\",\"required\":[\"scriptHash\",\"scriptId\",\"sourceMap\",\"sourceMapUrl\"],\"properties\":{\"scriptHash\":{\"type\":\"string\"},\"scriptId\":{\"type\":\"string\"},\"sourceMap\":{\"type\":\"string\"},\"sourceMapUrl\":{\"type\":\"string\"}},\"additionalProperties\":false},\"ScriptProvenance\":{\"type\":\"object\",\"properties\":{\"executionContextAuxData\":true,\"executionContextId\":{\"type\":[\"integer\",\"null\"],\"format\":\"int64\"},\"frameId\":{\"type\":[\"string\",\"null\"]}},\"additionalProperties\":false},\"SourceLocation\":{\"type\":\"object\",\"required\":[\"column\",\"line\",\"sourceUrl\"],\"properties\":{\"column\":{\"type\":\"integer\"},\"line\":{\"type\":\"integer\"},\"sourceUrl\":{\"type\":\"string\"}},\"additionalProperties\":false}}}}");

export const CaptureApi = new InterfaceDefinition(
    {
        id: "dev.dbgjs.capture",
        hash: "f20d251702d54ffe",
    },
    {
        delete_capture: requestType(
            z.object({
                captureName: z.string(),
                contextId: z.string(),
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
        list_captures: requestType(
            z.object({
                contextId: z.string(),
            }),
            z.array(CaptureSnapshotSchema),
        ),
        supply_stored_heap_source_map: requestType(
            z.object({
                captureName: z.string(),
                contextId: z.string(),
                supply: HeapSourceMapSupplySchema,
            }),
            z.null(),
        ),
    },
    { frozenSchema: wireSchema },
);
