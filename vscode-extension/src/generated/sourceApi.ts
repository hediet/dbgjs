import { InterfaceDefinition, notificationType, requestType, type LinkRpcInterfaceSchema } from "@hediet/linkrpc";
import { z } from "zod";

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

const wireSchema: LinkRpcInterfaceSchema = JSON.parse("{\"id\":\"dev.dbgjs.source\",\"hash\":\"7f364625a9686e48\",\"methods\":{\"add_source_formatting_rule\":{\"params\":{\"type\":\"object\",\"required\":[\"contextId\",\"mode\"],\"properties\":{\"contextId\":{\"type\":\"string\"},\"mode\":{\"$ref\":\"#/components/schemas/SourceFormattingMode\"},\"targetPattern\":{\"type\":[\"string\",\"null\"]},\"urlPattern\":{\"type\":[\"string\",\"null\"]}},\"additionalProperties\":false},\"result\":{\"$ref\":\"#/components/schemas/SourceFormattingSettings\"}},\"delete_source_formatting_rule\":{\"params\":{\"type\":\"object\",\"required\":[\"contextId\",\"ruleId\"],\"properties\":{\"contextId\":{\"type\":\"string\"},\"ruleId\":{\"type\":\"string\"}},\"additionalProperties\":false},\"result\":{\"$ref\":\"#/components/schemas/SourceFormattingSettings\"}},\"evict_source_caches\":{\"params\":{\"type\":\"object\",\"required\":[\"contextId\"],\"properties\":{\"contextId\":{\"type\":\"string\"}},\"additionalProperties\":false},\"result\":{\"type\":\"integer\"}},\"explain_source\":{\"params\":{\"type\":\"object\",\"required\":[\"contextId\",\"path\"],\"properties\":{\"contextId\":{\"type\":\"string\"},\"path\":{\"type\":\"string\"}},\"additionalProperties\":false},\"result\":{\"type\":\"array\",\"items\":{\"$ref\":\"#/components/schemas/SourceGraphViewSnapshot\"}}},\"export_sources\":{\"params\":{\"type\":\"object\",\"required\":[\"contextId\",\"destination\"],\"properties\":{\"contextId\":{\"type\":\"string\"},\"destination\":{\"type\":\"string\"}},\"additionalProperties\":false},\"result\":{\"type\":\"array\",\"items\":{\"type\":\"string\"}}},\"grep_sources\":{\"params\":{\"type\":\"object\",\"required\":[\"contextId\",\"options\"],\"properties\":{\"contextId\":{\"type\":\"string\"},\"options\":{\"$ref\":\"#/components/schemas/SourceSearchOptions\"}},\"additionalProperties\":false},\"result\":{\"$ref\":\"#/components/schemas/SourceSearchSnapshot\"}},\"list_sources\":{\"params\":{\"type\":\"object\",\"required\":[\"contextId\"],\"properties\":{\"contextId\":{\"type\":\"string\"},\"path\":{\"type\":[\"string\",\"null\"]}},\"additionalProperties\":false},\"result\":{\"type\":\"array\",\"items\":{\"$ref\":\"#/components/schemas/SourceSnapshotInfo\"}}},\"map_source\":{\"params\":{\"type\":\"object\",\"required\":[\"column\",\"contextId\",\"line\",\"path\"],\"properties\":{\"column\":{\"type\":\"integer\"},\"contextId\":{\"type\":\"string\"},\"line\":{\"type\":\"integer\"},\"path\":{\"type\":\"string\"}},\"additionalProperties\":false},\"result\":{\"type\":\"array\",\"items\":{\"$ref\":\"#/components/schemas/SourceMappingSnapshot\"}}},\"resolve_sources\":{\"params\":{\"type\":\"object\",\"required\":[\"contextId\",\"source\"],\"properties\":{\"contextId\":{\"type\":\"string\"},\"source\":{\"type\":\"string\"}},\"additionalProperties\":false},\"result\":{\"$ref\":\"#/components/schemas/UncompactedSourceGraphSnapshot\"}},\"set_source_formatting\":{\"params\":{\"type\":\"object\",\"required\":[\"contextId\",\"mode\"],\"properties\":{\"contextId\":{\"type\":\"string\"},\"mode\":{\"$ref\":\"#/components/schemas/SourceFormattingMode\"}},\"additionalProperties\":false},\"result\":{\"$ref\":\"#/components/schemas/SourceFormattingSettings\"}},\"show_source\":{\"params\":{\"type\":\"object\",\"required\":[\"contextId\",\"options\",\"path\"],\"properties\":{\"contextId\":{\"type\":\"string\"},\"options\":{\"$ref\":\"#/components/schemas/SourceDisplayOptions\"},\"path\":{\"type\":\"string\"}},\"additionalProperties\":false},\"result\":{\"$ref\":\"#/components/schemas/SourceContentSnapshot\"}},\"show_source_graph\":{\"params\":{\"type\":\"object\",\"required\":[\"contextId\"],\"properties\":{\"contextId\":{\"type\":\"string\"}},\"additionalProperties\":false},\"result\":{\"$ref\":\"#/components/schemas/CompactedSourceGraphSnapshot\"}},\"show_source_tree\":{\"params\":{\"type\":\"object\",\"required\":[\"contextId\",\"kind\"],\"properties\":{\"contextId\":{\"type\":\"string\"},\"kind\":{\"$ref\":\"#/components/schemas/SourceTreeKind\"}},\"additionalProperties\":false},\"result\":{\"$ref\":\"#/components/schemas/SourceTreeSnapshot\"}},\"show_uncompacted_source_graph\":{\"params\":{\"type\":\"object\",\"required\":[\"contextId\"],\"properties\":{\"contextId\":{\"type\":\"string\"}},\"additionalProperties\":false},\"result\":{\"$ref\":\"#/components/schemas/UncompactedSourceGraphSnapshot\"}}},\"components\":{\"schemas\":{\"CompactedSourceEdgeSnapshot\":{\"type\":\"object\",\"required\":[\"basis\",\"derived\",\"kind\",\"mappingCount\"],\"properties\":{\"basis\":{\"type\":\"integer\"},\"derived\":{\"type\":\"integer\"},\"fanOut\":{\"type\":\"boolean\"},\"kind\":{\"type\":\"string\"},\"mappingCount\":{\"type\":\"integer\"},\"suffixRewrite\":{\"anyOf\":[{\"$ref\":\"#/components/schemas/SourceSuffixRewriteSnapshot\"},{\"type\":\"null\"}]}},\"additionalProperties\":false},\"CompactedSourceGraphSnapshot\":{\"type\":\"object\",\"required\":[\"edges\",\"nodes\",\"roots\"],\"properties\":{\"edges\":{\"type\":\"array\",\"items\":{\"$ref\":\"#/components/schemas/CompactedSourceEdgeSnapshot\"}},\"nodes\":{\"type\":\"array\",\"items\":{\"$ref\":\"#/components/schemas/CompactedSourceNodeSnapshot\"}},\"roots\":{\"type\":\"array\",\"items\":{\"type\":\"integer\"}}},\"additionalProperties\":false},\"CompactedSourceNodeSnapshot\":{\"type\":\"object\",\"required\":[\"id\",\"prefix\",\"runtimeInternal\",\"sourceCount\"],\"properties\":{\"id\":{\"type\":\"integer\"},\"listedSourcePaths\":{\"type\":\"array\",\"items\":{\"type\":\"string\"}},\"prefix\":{\"type\":\"string\"},\"runtimeInternal\":{\"type\":\"boolean\"},\"snapshotCount\":{\"type\":\"integer\"},\"sourceCount\":{\"type\":\"integer\"}},\"additionalProperties\":false},\"SourceContentSnapshot\":{\"type\":\"object\",\"required\":[\"content\",\"endLine\",\"path\",\"startLine\",\"totalLines\"],\"properties\":{\"content\":{\"type\":\"string\"},\"endLine\":{\"type\":\"integer\"},\"path\":{\"type\":\"string\"},\"startLine\":{\"type\":\"integer\"},\"totalLines\":{\"type\":\"integer\"}},\"additionalProperties\":false},\"SourceDisplayOptions\":{\"type\":\"object\",\"required\":[\"contextLines\"],\"properties\":{\"contextLines\":{\"type\":\"integer\"},\"line\":{\"type\":[\"integer\",\"null\"],\"format\":\"uint32\"},\"view\":{\"$ref\":\"#/components/schemas/SourceViewPreference\"}},\"additionalProperties\":false},\"SourceFormattingMode\":{\"type\":\"string\",\"enum\":[\"off\",\"auto\",\"on\"]},\"SourceFormattingRule\":{\"type\":\"object\",\"required\":[\"id\",\"mode\"],\"properties\":{\"id\":{\"type\":\"string\"},\"mode\":{\"$ref\":\"#/components/schemas/SourceFormattingMode\"},\"targetPattern\":{\"type\":[\"string\",\"null\"]},\"urlPattern\":{\"type\":[\"string\",\"null\"]}},\"additionalProperties\":false},\"SourceFormattingSettings\":{\"type\":\"object\",\"required\":[\"defaultMode\",\"rules\"],\"properties\":{\"defaultMode\":{\"$ref\":\"#/components/schemas/SourceFormattingMode\"},\"rules\":{\"type\":\"array\",\"items\":{\"$ref\":\"#/components/schemas/SourceFormattingRule\"}}},\"additionalProperties\":false},\"SourceGraphViewSnapshot\":{\"type\":\"object\",\"required\":[\"alternativeProvenance\",\"connectionId\",\"diagnostics\",\"generatedUrl\",\"kind\",\"primaryProvenance\",\"projectionPaths\",\"resolvedSourceCount\",\"role\",\"sourcePath\",\"targetId\"],\"properties\":{\"alternativeProvenance\":{\"type\":\"array\",\"items\":{\"type\":\"string\"}},\"connectionId\":{\"type\":\"string\"},\"diagnostics\":{\"type\":\"array\",\"items\":{\"type\":\"string\"}},\"generatedUrl\":{\"type\":\"string\"},\"kind\":{\"type\":\"string\"},\"primaryProvenance\":{\"type\":\"string\"},\"projectionPaths\":{\"type\":\"array\",\"items\":{\"$ref\":\"#/components/schemas/SourceProjectionPathSnapshot\"}},\"resolvedSourceCount\":{\"type\":\"integer\"},\"role\":{\"type\":\"string\"},\"sourcePath\":{\"type\":\"string\"},\"targetId\":{\"type\":\"string\"}},\"additionalProperties\":false},\"SourceMappingSnapshot\":{\"type\":\"object\",\"required\":[\"column\",\"connectionId\",\"direction\",\"line\",\"quality\",\"sourceUrl\",\"targetId\"],\"properties\":{\"column\":{\"type\":\"integer\"},\"connectionId\":{\"type\":\"string\"},\"direction\":{\"type\":\"string\"},\"line\":{\"type\":\"integer\"},\"quality\":{\"type\":\"string\"},\"sourceUrl\":{\"type\":\"string\"},\"targetId\":{\"type\":\"string\"}},\"additionalProperties\":false},\"SourceMatchSnapshot\":{\"type\":\"object\",\"required\":[\"afterContext\",\"beforeContext\",\"column\",\"contentHash\",\"kind\",\"line\",\"matchLength\",\"path\",\"provenance\",\"text\"],\"properties\":{\"afterContext\":{\"type\":\"array\",\"items\":{\"type\":\"string\"}},\"beforeContext\":{\"type\":\"array\",\"items\":{\"type\":\"string\"}},\"column\":{\"type\":\"integer\"},\"connectionId\":{\"type\":[\"string\",\"null\"]},\"contentHash\":{\"type\":\"string\"},\"kind\":{\"type\":\"string\"},\"line\":{\"type\":\"integer\"},\"matchLength\":{\"type\":\"integer\"},\"path\":{\"type\":\"string\"},\"provenance\":{\"type\":\"string\"},\"targetId\":{\"type\":[\"string\",\"null\"]},\"text\":{\"type\":\"string\"}},\"additionalProperties\":false},\"SourceProjectionPathSnapshot\":{\"type\":\"object\",\"required\":[\"generatedUrl\",\"steps\"],\"properties\":{\"generatedUrl\":{\"type\":\"string\"},\"steps\":{\"type\":\"array\",\"items\":{\"type\":\"string\"}}},\"additionalProperties\":false},\"SourceSearchOptions\":{\"type\":\"object\",\"required\":[\"caseSensitive\",\"contextLines\",\"maxResults\",\"pattern\",\"regex\"],\"properties\":{\"caseSensitive\":{\"type\":\"boolean\"},\"contextLines\":{\"type\":\"integer\"},\"maxResults\":{\"type\":\"integer\"},\"path\":{\"type\":[\"string\",\"null\"]},\"pattern\":{\"type\":\"string\"},\"regex\":{\"type\":\"boolean\"},\"timeoutMs\":{\"type\":[\"integer\",\"null\"],\"format\":\"uint64\"},\"view\":{\"$ref\":\"#/components/schemas/SourceViewPreference\"}},\"additionalProperties\":false},\"SourceSearchSkip\":{\"type\":\"object\",\"required\":[\"kind\",\"path\",\"reason\"],\"properties\":{\"connectionId\":{\"type\":[\"string\",\"null\"]},\"kind\":{\"type\":\"string\"},\"path\":{\"type\":\"string\"},\"reason\":{\"type\":\"string\"},\"targetId\":{\"type\":[\"string\",\"null\"]}},\"additionalProperties\":false},\"SourceSearchSnapshot\":{\"type\":\"object\",\"required\":[\"matches\",\"omittedMatches\",\"searchedContents\",\"searchedSources\",\"skippedSources\"],\"properties\":{\"matches\":{\"type\":\"array\",\"items\":{\"$ref\":\"#/components/schemas/SourceMatchSnapshot\"}},\"omittedMatches\":{\"type\":\"integer\"},\"searchedContents\":{\"type\":\"integer\"},\"searchedSources\":{\"type\":\"integer\"},\"skipped\":{\"type\":\"array\",\"items\":{\"$ref\":\"#/components/schemas/SourceSearchSkip\"}},\"skippedSources\":{\"type\":\"integer\"}},\"additionalProperties\":false},\"SourceSnapshotInfo\":{\"type\":\"object\",\"required\":[\"kind\",\"path\",\"status\"],\"properties\":{\"connectionId\":{\"type\":[\"string\",\"null\"]},\"kind\":{\"type\":\"string\"},\"path\":{\"type\":\"string\"},\"sourceMapUrl\":{\"type\":[\"string\",\"null\"]},\"status\":{\"type\":\"string\"},\"targetId\":{\"type\":[\"string\",\"null\"]}},\"additionalProperties\":false},\"SourceSuffixRewriteSnapshot\":{\"type\":\"object\",\"required\":[\"from\",\"to\"],\"properties\":{\"from\":{\"type\":\"string\"},\"to\":{\"type\":\"string\"}},\"additionalProperties\":false},\"SourceTreeKind\":{\"type\":\"string\",\"enum\":[\"loaded\",\"sourceMapped\",\"formatted\",\"resolved\"]},\"SourceTreeSnapshot\":{\"type\":\"object\",\"required\":[\"kind\",\"sources\"],\"properties\":{\"kind\":{\"$ref\":\"#/components/schemas/SourceTreeKind\"},\"sources\":{\"type\":\"array\",\"items\":{\"$ref\":\"#/components/schemas/UncompactedSourceNodeSnapshot\"}}},\"additionalProperties\":false},\"SourceViewPreference\":{\"type\":\"string\",\"enum\":[\"policy\",\"original\",\"formatted\"]},\"UncompactedProjectionSnapshot\":{\"oneOf\":[{\"type\":\"object\",\"required\":[\"contentHash\",\"kind\"],\"properties\":{\"contentHash\":{\"type\":\"string\"},\"kind\":{\"type\":\"string\",\"const\":\"identityEqualContent\"}},\"additionalProperties\":false},{\"type\":\"object\",\"required\":[\"kind\",\"provider\"],\"properties\":{\"kind\":{\"type\":\"string\",\"const\":\"identityDeclaredByProvider\"},\"provider\":{\"type\":\"string\"}},\"additionalProperties\":false},{\"type\":\"object\",\"required\":[\"kind\",\"mapHash\",\"sourceIndex\"],\"properties\":{\"kind\":{\"type\":\"string\",\"const\":\"sourceMap\"},\"mapHash\":{\"type\":\"string\"},\"sourceIndex\":{\"type\":\"integer\"}},\"additionalProperties\":false},{\"type\":\"object\",\"required\":[\"formatter\",\"kind\"],\"properties\":{\"formatter\":{\"type\":\"string\"},\"kind\":{\"type\":\"string\",\"const\":\"format\"}},\"additionalProperties\":false},{\"type\":\"object\",\"required\":[\"edit\",\"kind\"],\"properties\":{\"edit\":{\"type\":\"string\"},\"kind\":{\"type\":\"string\",\"const\":\"edit\"}},\"additionalProperties\":false},{\"type\":\"object\",\"required\":[\"columnDelta\",\"kind\",\"lineDelta\"],\"properties\":{\"columnDelta\":{\"type\":\"integer\"},\"kind\":{\"type\":\"string\",\"const\":\"offset\"},\"lineDelta\":{\"type\":\"integer\"}},\"additionalProperties\":false}],\"discriminator\":{\"propertyName\":\"kind\"}},\"UncompactedSourceEdgeSnapshot\":{\"type\":\"object\",\"required\":[\"basis\",\"derived\",\"id\",\"projection\"],\"properties\":{\"basis\":{\"type\":\"integer\"},\"derived\":{\"type\":\"integer\"},\"id\":{\"type\":\"integer\"},\"projection\":{\"$ref\":\"#/components/schemas/UncompactedProjectionSnapshot\"}},\"additionalProperties\":false},\"UncompactedSourceGraphSnapshot\":{\"type\":\"object\",\"required\":[\"edges\",\"nodes\",\"roots\"],\"properties\":{\"edges\":{\"type\":\"array\",\"items\":{\"$ref\":\"#/components/schemas/UncompactedSourceEdgeSnapshot\"}},\"nodes\":{\"type\":\"array\",\"items\":{\"$ref\":\"#/components/schemas/UncompactedSourceNodeSnapshot\"}},\"roots\":{\"type\":\"array\",\"items\":{\"type\":\"integer\"}}},\"additionalProperties\":false},\"UncompactedSourceNodeSnapshot\":{\"type\":\"object\",\"required\":[\"id\",\"revision\",\"uri\"],\"properties\":{\"id\":{\"type\":\"integer\"},\"revision\":{\"$ref\":\"#/components/schemas/UncompactedSourceRevisionSnapshot\"},\"uri\":{\"type\":\"string\"}},\"additionalProperties\":false},\"UncompactedSourceRevisionSnapshot\":{\"oneOf\":[{\"type\":\"object\",\"required\":[\"hash\",\"kind\"],\"properties\":{\"hash\":{\"type\":\"string\"},\"kind\":{\"type\":\"string\",\"const\":\"content\"}},\"additionalProperties\":false},{\"type\":\"object\",\"required\":[\"kind\",\"namespace\",\"value\"],\"properties\":{\"kind\":{\"type\":\"string\",\"const\":\"version\"},\"namespace\":{\"type\":\"string\"},\"value\":{\"type\":\"string\"}},\"additionalProperties\":false}],\"discriminator\":{\"propertyName\":\"kind\"}}}}}");

export const SourceApi = new InterfaceDefinition(
    {
        id: "dev.dbgjs.source",
        hash: "7f364625a9686e48",
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
        delete_source_formatting_rule: requestType(
            z.object({
                contextId: z.string(),
                ruleId: z.string(),
            }),
            SourceFormattingSettingsSchema,
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
        grep_sources: requestType(
            z.object({
                contextId: z.string(),
                options: SourceSearchOptionsSchema,
            }),
            SourceSearchSnapshotSchema,
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
        resolve_sources: requestType(
            z.object({
                contextId: z.string(),
                source: z.string(),
            }),
            UncompactedSourceGraphSnapshotSchema,
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
    },
    { frozenSchema: wireSchema },
);
