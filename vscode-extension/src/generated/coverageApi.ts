import { InterfaceDefinition, notificationType, requestType, type LinkRpcInterfaceSchema } from "@hediet/linkrpc";
import { z } from "zod";

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

const wireSchema: LinkRpcInterfaceSchema = JSON.parse("{\"id\":\"dev.dbgjs.coverage\",\"hash\":\"1c94c363b999af19\",\"methods\":{\"finish_coverage\":{\"params\":{\"type\":\"object\",\"required\":[\"connectionId\",\"contextId\",\"targetId\"],\"properties\":{\"captureId\":{\"type\":[\"string\",\"null\"]},\"connectionId\":{\"type\":\"string\"},\"contextId\":{\"type\":\"string\"},\"targetId\":{\"type\":\"string\"}},\"additionalProperties\":false},\"result\":{\"type\":\"boolean\"}},\"get_coverage\":{\"params\":{\"type\":\"object\",\"required\":[\"captureId\",\"connectionId\",\"contextId\",\"noCache\",\"targetId\"],\"properties\":{\"captureId\":{\"type\":\"string\"},\"connectionId\":{\"type\":\"string\"},\"contextId\":{\"type\":\"string\"},\"noCache\":{\"type\":\"boolean\"},\"sourcePath\":{\"type\":[\"string\",\"null\"]},\"targetId\":{\"type\":\"string\"}},\"additionalProperties\":false},\"result\":{\"$ref\":\"#/components/schemas/CoverageSnapshot\"}},\"start_coverage\":{\"params\":{\"type\":\"object\",\"required\":[\"connectionId\",\"contextId\",\"targetId\"],\"properties\":{\"connectionId\":{\"type\":\"string\"},\"contextId\":{\"type\":\"string\"},\"targetId\":{\"type\":\"string\"}},\"additionalProperties\":false},\"result\":{\"type\":\"boolean\"}},\"stop_coverage\":{\"params\":{\"type\":\"object\",\"required\":[\"connectionId\",\"contextId\",\"targetId\"],\"properties\":{\"captureId\":{\"type\":[\"string\",\"null\"]},\"connectionId\":{\"type\":\"string\"},\"contextId\":{\"type\":\"string\"},\"targetId\":{\"type\":\"string\"}},\"additionalProperties\":false},\"result\":{\"$ref\":\"#/components/schemas/CoverageSnapshot\"}},\"take_coverage\":{\"params\":{\"type\":\"object\",\"required\":[\"connectionId\",\"contextId\",\"targetId\"],\"properties\":{\"captureId\":{\"type\":[\"string\",\"null\"]},\"connectionId\":{\"type\":\"string\"},\"contextId\":{\"type\":\"string\"},\"raw\":{\"type\":[\"boolean\",\"null\"]},\"targetId\":{\"type\":\"string\"}},\"additionalProperties\":false},\"result\":{\"$ref\":\"#/components/schemas/CoverageSnapshot\"}}},\"components\":{\"schemas\":{\"CoverageAnalysisSnapshot\":{\"type\":\"object\",\"required\":[\"durationMicros\",\"sourceMapCacheBypasses\",\"sourceMapCacheHits\",\"sourceMapCacheMisses\"],\"properties\":{\"durationMicros\":{\"type\":\"integer\"},\"sourceMapCacheBypasses\":{\"type\":\"integer\"},\"sourceMapCacheHits\":{\"type\":\"integer\"},\"sourceMapCacheMisses\":{\"type\":\"integer\"}},\"additionalProperties\":false},\"CoverageFunctionSnapshot\":{\"type\":\"object\",\"required\":[\"blockCoverage\",\"name\",\"ranges\",\"rootEndOffset\",\"rootStartOffset\"],\"properties\":{\"authoredLocation\":{\"anyOf\":[{\"$ref\":\"#/components/schemas/SourceLocation\"},{\"type\":\"null\"}]},\"blockCoverage\":{\"type\":\"boolean\"},\"breadcrumb\":{\"type\":[\"string\",\"null\"]},\"effectiveRanges\":{\"type\":\"array\",\"items\":{\"$ref\":\"#/components/schemas/CoverageRangeSnapshot\"}},\"generatedLocation\":{\"anyOf\":[{\"$ref\":\"#/components/schemas/SourceLocation\"},{\"type\":\"null\"}]},\"name\":{\"type\":\"string\"},\"ranges\":{\"type\":\"array\",\"items\":{\"$ref\":\"#/components/schemas/CoverageRangeSnapshot\"}},\"rootEndOffset\":{\"type\":\"integer\"},\"rootStartOffset\":{\"type\":\"integer\"}},\"additionalProperties\":false},\"CoverageRangeSnapshot\":{\"type\":\"object\",\"required\":[\"count\",\"endOffset\",\"startOffset\"],\"properties\":{\"authoredEnd\":{\"anyOf\":[{\"$ref\":\"#/components/schemas/SourceLocation\"},{\"type\":\"null\"}]},\"authoredStart\":{\"anyOf\":[{\"$ref\":\"#/components/schemas/SourceLocation\"},{\"type\":\"null\"}]},\"count\":{\"type\":\"integer\"},\"endOffset\":{\"type\":\"integer\"},\"startOffset\":{\"type\":\"integer\"}},\"additionalProperties\":false},\"CoverageSnapshot\":{\"type\":\"object\",\"required\":[\"sources\",\"timestampMicros\"],\"properties\":{\"analysis\":{\"anyOf\":[{\"$ref\":\"#/components/schemas/CoverageAnalysisSnapshot\"},{\"type\":\"null\"}]},\"captureId\":{\"type\":[\"string\",\"null\"]},\"sources\":{\"type\":\"array\",\"items\":{\"$ref\":\"#/components/schemas/CoverageSourceSnapshot\"}},\"timestampMicros\":{\"type\":\"integer\"}},\"additionalProperties\":false},\"CoverageSourceSnapshot\":{\"type\":\"object\",\"required\":[\"functions\",\"generatedUrl\",\"scriptId\"],\"properties\":{\"associatedAuthoredSource\":{\"type\":[\"string\",\"null\"]},\"functions\":{\"type\":\"array\",\"items\":{\"$ref\":\"#/components/schemas/CoverageFunctionSnapshot\"}},\"generatedUrl\":{\"type\":\"string\"},\"scriptId\":{\"type\":\"string\"}},\"additionalProperties\":false},\"SourceLocation\":{\"type\":\"object\",\"required\":[\"column\",\"line\",\"sourceUrl\"],\"properties\":{\"column\":{\"type\":\"integer\"},\"line\":{\"type\":\"integer\"},\"sourceUrl\":{\"type\":\"string\"}},\"additionalProperties\":false}}}}");

export const CoverageApi = new InterfaceDefinition(
    {
        id: "dev.dbgjs.coverage",
        hash: "1c94c363b999af19",
    },
    {
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
        start_coverage: requestType(
            z.object({
                connectionId: z.string(),
                contextId: z.string(),
                targetId: z.string(),
            }),
            z.boolean(),
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
    },
    { frozenSchema: wireSchema },
);
