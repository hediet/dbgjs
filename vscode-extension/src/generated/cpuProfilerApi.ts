import { InterfaceDefinition, notificationType, requestType, type LinkRpcInterfaceSchema } from "@hediet/linkrpc";
import { z } from "zod";

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

const SourceLocationSchema = z.object({
    column: z.int(),
    line: z.int(),
    sourceUrl: z.string(),
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

const wireSchema: LinkRpcInterfaceSchema = JSON.parse("{\"id\":\"dev.dbgjs.cpu-profiler\",\"hash\":\"62d94241079ad690\",\"methods\":{\"get_cpu_profile\":{\"params\":{\"type\":\"object\",\"required\":[\"captureId\",\"connectionId\",\"contextId\",\"noCache\",\"project\",\"targetId\"],\"properties\":{\"captureId\":{\"type\":\"string\"},\"connectionId\":{\"type\":\"string\"},\"contextId\":{\"type\":\"string\"},\"noCache\":{\"type\":\"boolean\"},\"project\":{\"type\":\"boolean\"},\"sourcePath\":{\"type\":[\"string\",\"null\"]},\"targetId\":{\"type\":\"string\"}},\"additionalProperties\":false},\"result\":{\"$ref\":\"#/components/schemas/CpuProfileSnapshot\"}},\"start_cpu_profile\":{\"params\":{\"type\":\"object\",\"required\":[\"connectionId\",\"contextId\",\"targetId\"],\"properties\":{\"connectionId\":{\"type\":\"string\"},\"contextId\":{\"type\":\"string\"},\"samplingIntervalMicros\":{\"type\":[\"integer\",\"null\"],\"format\":\"uint64\"},\"targetId\":{\"type\":\"string\"}},\"additionalProperties\":false},\"result\":{\"type\":\"boolean\"}},\"stop_cpu_profile\":{\"params\":{\"type\":\"object\",\"required\":[\"connectionId\",\"contextId\",\"targetId\"],\"properties\":{\"captureId\":{\"type\":[\"string\",\"null\"]},\"connectionId\":{\"type\":\"string\"},\"contextId\":{\"type\":\"string\"},\"targetId\":{\"type\":\"string\"}},\"additionalProperties\":false},\"result\":{\"$ref\":\"#/components/schemas/CpuProfileSnapshot\"}}},\"components\":{\"schemas\":{\"CpuProfileAnalysisSnapshot\":{\"type\":\"object\",\"required\":[\"durationMicros\",\"sourceMapCacheBypasses\",\"sourceMapCacheHits\",\"sourceMapCacheMisses\"],\"properties\":{\"durationMicros\":{\"type\":\"integer\"},\"sourceMapCacheBypasses\":{\"type\":\"integer\"},\"sourceMapCacheHits\":{\"type\":\"integer\"},\"sourceMapCacheMisses\":{\"type\":\"integer\"}},\"additionalProperties\":false},\"CpuProfileCallFrameSnapshot\":{\"type\":\"object\",\"required\":[\"columnNumber\",\"functionName\",\"lineNumber\",\"scriptId\",\"url\"],\"properties\":{\"columnNumber\":{\"type\":\"integer\"},\"functionName\":{\"type\":\"string\"},\"lineNumber\":{\"type\":\"integer\"},\"scriptId\":{\"type\":\"string\"},\"url\":{\"type\":\"string\"}},\"additionalProperties\":false},\"CpuProfileFunctionSnapshot\":{\"type\":\"object\",\"required\":[\"generatedLocation\",\"name\",\"sampleCount\",\"selfTimeMicros\",\"totalTimeMicros\"],\"properties\":{\"authoredLocation\":{\"anyOf\":[{\"$ref\":\"#/components/schemas/SourceLocation\"},{\"type\":\"null\"}]},\"breadcrumb\":{\"type\":[\"string\",\"null\"]},\"generatedLocation\":{\"$ref\":\"#/components/schemas/SourceLocation\"},\"name\":{\"type\":\"string\"},\"sampleCount\":{\"type\":\"integer\"},\"selfTimeMicros\":{\"type\":\"integer\"},\"totalTimeMicros\":{\"type\":\"integer\"}},\"additionalProperties\":false},\"CpuProfileNodeSnapshot\":{\"type\":\"object\",\"required\":[\"callFrame\",\"children\",\"id\",\"positionTicks\",\"sampleCount\",\"selfTimeMicros\",\"totalTimeMicros\"],\"properties\":{\"authoredLocation\":{\"anyOf\":[{\"$ref\":\"#/components/schemas/SourceLocation\"},{\"type\":\"null\"}]},\"breadcrumb\":{\"type\":[\"string\",\"null\"]},\"callFrame\":{\"$ref\":\"#/components/schemas/CpuProfileCallFrameSnapshot\"},\"children\":{\"type\":\"array\",\"items\":{\"type\":\"integer\"}},\"deoptReason\":{\"type\":[\"string\",\"null\"]},\"hitCount\":{\"type\":[\"integer\",\"null\"],\"format\":\"int64\"},\"id\":{\"type\":\"integer\"},\"positionTicks\":{\"type\":\"array\",\"items\":{\"$ref\":\"#/components/schemas/CpuProfilePositionTickSnapshot\"}},\"sampleCount\":{\"type\":\"integer\"},\"selfTimeMicros\":{\"type\":\"integer\"},\"totalTimeMicros\":{\"type\":\"integer\"}},\"additionalProperties\":false},\"CpuProfilePositionTickSnapshot\":{\"type\":\"object\",\"required\":[\"line\",\"ticks\"],\"properties\":{\"line\":{\"type\":\"integer\"},\"ticks\":{\"type\":\"integer\"}},\"additionalProperties\":false},\"CpuProfileSnapshot\":{\"type\":\"object\",\"required\":[\"captureId\",\"endTimeMicros\",\"nodes\",\"samples\",\"startTimeMicros\",\"timeDeltasMicros\"],\"properties\":{\"analysis\":{\"anyOf\":[{\"$ref\":\"#/components/schemas/CpuProfileAnalysisSnapshot\"},{\"type\":\"null\"}]},\"captureId\":{\"type\":\"string\"},\"endTimeMicros\":{\"type\":\"number\"},\"functions\":{\"type\":\"array\",\"items\":{\"$ref\":\"#/components/schemas/CpuProfileFunctionSnapshot\"}},\"nodes\":{\"type\":\"array\",\"items\":{\"$ref\":\"#/components/schemas/CpuProfileNodeSnapshot\"}},\"samples\":{\"type\":\"array\",\"items\":{\"type\":\"integer\"}},\"samplingIntervalMicros\":{\"type\":[\"integer\",\"null\"],\"format\":\"uint64\"},\"startTimeMicros\":{\"type\":\"number\"},\"timeDeltasMicros\":{\"description\":\"Raw CDP timestamp differences in sample order, which need not be chronological.\",\"type\":\"array\",\"items\":{\"type\":\"integer\"}}},\"additionalProperties\":false},\"SourceLocation\":{\"type\":\"object\",\"required\":[\"column\",\"line\",\"sourceUrl\"],\"properties\":{\"column\":{\"type\":\"integer\"},\"line\":{\"type\":\"integer\"},\"sourceUrl\":{\"type\":\"string\"}},\"additionalProperties\":false}}}}");

export const CpuProfilerApi = new InterfaceDefinition(
    {
        id: "dev.dbgjs.cpu-profiler",
        hash: "62d94241079ad690",
    },
    {
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
    },
    { frozenSchema: wireSchema },
);
