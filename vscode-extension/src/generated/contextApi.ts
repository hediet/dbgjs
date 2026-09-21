import { InterfaceDefinition, notificationType, requestType, type LinkRpcInterfaceSchema } from "@hediet/linkrpc";
import { z } from "zod";

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

const CdpStdioTopologySchema = z.enum(["browser", "target"]);

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

const ConnectionSnapshotSchema = z.object({
    configuration: ConnectionConfigurationSchema,
    generation: z.int(),
    id: z.string(),
    status: ConnectionStatusSchema,
    targets: z.array(TargetSnapshotSchema),
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

const wireSchema: LinkRpcInterfaceSchema = JSON.parse("{\"id\":\"dev.dbgjs.context\",\"hash\":\"84dc47d63168f609\",\"methods\":{\"connect_connection\":{\"params\":{\"type\":\"object\",\"required\":[\"connectionId\",\"contextId\"],\"properties\":{\"connectionId\":{\"type\":\"string\"},\"contextId\":{\"type\":\"string\"}},\"additionalProperties\":false},\"result\":{\"$ref\":\"#/components/schemas/ContextSnapshot\"}},\"delete_breakpoint\":{\"params\":{\"type\":\"object\",\"required\":[\"breakpointId\",\"contextId\",\"options\"],\"properties\":{\"breakpointId\":{\"type\":\"string\"},\"contextId\":{\"type\":\"string\"},\"options\":{\"$ref\":\"#/components/schemas/MutationOptions\"}},\"additionalProperties\":false},\"result\":{\"$ref\":\"#/components/schemas/ContextSnapshot\"}},\"delete_connection\":{\"params\":{\"type\":\"object\",\"required\":[\"connectionId\",\"contextId\",\"options\"],\"properties\":{\"connectionId\":{\"type\":\"string\"},\"contextId\":{\"type\":\"string\"},\"options\":{\"$ref\":\"#/components/schemas/MutationOptions\"}},\"additionalProperties\":false},\"result\":{\"$ref\":\"#/components/schemas/ContextSnapshot\"}},\"delete_context\":{\"params\":{\"type\":\"object\",\"required\":[\"contextId\",\"options\"],\"properties\":{\"contextId\":{\"type\":\"string\"},\"options\":{\"$ref\":\"#/components/schemas/MutationOptions\"}},\"additionalProperties\":false},\"result\":{\"type\":\"boolean\"}},\"disconnect_connection\":{\"params\":{\"type\":\"object\",\"required\":[\"connectionId\",\"contextId\"],\"properties\":{\"connectionId\":{\"type\":\"string\"},\"contextId\":{\"type\":\"string\"}},\"additionalProperties\":false},\"result\":{\"$ref\":\"#/components/schemas/ContextSnapshot\"}},\"get_context\":{\"params\":{\"type\":\"object\",\"required\":[\"contextId\"],\"properties\":{\"contextId\":{\"type\":\"string\"}},\"additionalProperties\":false},\"result\":{\"$ref\":\"#/components/schemas/ContextSnapshot\"}},\"get_resource_graph\":{\"params\":{\"type\":\"object\",\"required\":[\"contextId\"],\"properties\":{\"contextId\":{\"type\":\"string\"}},\"additionalProperties\":false},\"result\":{\"$ref\":\"#/components/schemas/ResourceGraphSnapshot\"}},\"list_contexts\":{\"params\":{\"type\":\"object\",\"properties\":{\"cwd\":{\"type\":[\"string\",\"null\"]}},\"additionalProperties\":false},\"result\":{\"type\":\"array\",\"items\":{\"$ref\":\"#/components/schemas/ContextSummary\"}}},\"observe_context\":{\"params\":{\"type\":\"object\",\"required\":[\"contextId\",\"cursor\",\"timeoutMs\"],\"properties\":{\"contextId\":{\"type\":\"string\"},\"cursor\":{\"$ref\":\"#/components/schemas/ObservationCursor\"},\"timeoutMs\":{\"type\":\"integer\"}},\"additionalProperties\":false},\"result\":{\"$ref\":\"#/components/schemas/ObservationResult\"}},\"put_breakpoint\":{\"params\":{\"type\":\"object\",\"required\":[\"breakpointId\",\"column\",\"contextId\",\"line\",\"sourcePath\"],\"properties\":{\"breakpointId\":{\"type\":\"string\"},\"column\":{\"type\":\"integer\"},\"contextId\":{\"type\":\"string\"},\"line\":{\"type\":\"integer\"},\"sourcePath\":{\"type\":\"string\"}},\"additionalProperties\":false},\"result\":{\"$ref\":\"#/components/schemas/ContextSnapshot\"}},\"put_breakpoint_spec\":{\"params\":{\"type\":\"object\",\"required\":[\"breakpointId\",\"contextId\",\"options\",\"specification\"],\"properties\":{\"breakpointId\":{\"type\":\"string\"},\"contextId\":{\"type\":\"string\"},\"options\":{\"$ref\":\"#/components/schemas/MutationOptions\"},\"specification\":{\"$ref\":\"#/components/schemas/BreakpointSpec\"}},\"additionalProperties\":false},\"result\":{\"$ref\":\"#/components/schemas/ContextSnapshot\"}},\"put_connection\":{\"params\":{\"type\":\"object\",\"required\":[\"configuration\",\"connectionId\",\"contextId\"],\"properties\":{\"configuration\":{\"$ref\":\"#/components/schemas/ConnectionConfiguration\"},\"connectionId\":{\"type\":\"string\"},\"contextId\":{\"type\":\"string\"}},\"additionalProperties\":false},\"result\":{\"$ref\":\"#/components/schemas/ContextSnapshot\"}},\"put_context\":{\"params\":{\"type\":\"object\",\"required\":[\"contextId\",\"kind\"],\"properties\":{\"contextId\":{\"type\":\"string\"},\"displayName\":{\"type\":[\"string\",\"null\"]},\"kind\":{\"$ref\":\"#/components/schemas/ContextKind\"}},\"additionalProperties\":false},\"result\":{\"$ref\":\"#/components/schemas/ContextSnapshot\"}},\"set_pause_future_children\":{\"params\":{\"type\":\"object\",\"required\":[\"connectionId\",\"contextId\",\"enabled\"],\"properties\":{\"connectionId\":{\"type\":\"string\"},\"contextId\":{\"type\":\"string\"},\"enabled\":{\"type\":\"boolean\"}},\"additionalProperties\":false},\"result\":{\"type\":\"boolean\"}}},\"components\":{\"schemas\":{\"BreakpointApplicationSnapshot\":{\"type\":\"object\",\"required\":[\"connectionGeneration\",\"connectionId\",\"generatedColumn\",\"generatedLine\",\"scriptId\",\"scriptUrl\",\"scriptVersion\",\"status\",\"targetId\"],\"properties\":{\"connectionGeneration\":{\"type\":\"integer\"},\"connectionId\":{\"type\":\"string\"},\"generatedColumn\":{\"type\":\"integer\"},\"generatedLine\":{\"type\":\"integer\"},\"mapping\":{\"anyOf\":[{\"$ref\":\"#/components/schemas/BreakpointMappingSnapshot\"},{\"type\":\"null\"}]},\"scriptId\":{\"type\":\"string\"},\"scriptUrl\":{\"type\":\"string\"},\"scriptVersion\":{\"type\":\"integer\"},\"status\":{\"$ref\":\"#/components/schemas/BreakpointApplicationStatus\"},\"targetId\":{\"type\":\"string\"}},\"additionalProperties\":false},\"BreakpointApplicationStatus\":{\"oneOf\":[{\"type\":\"object\",\"required\":[\"kind\"],\"properties\":{\"kind\":{\"type\":\"string\",\"const\":\"installing\"}},\"additionalProperties\":false},{\"type\":\"object\",\"required\":[\"backend_id\",\"kind\"],\"properties\":{\"backend_id\":{\"type\":\"string\"},\"kind\":{\"type\":\"string\",\"const\":\"installed\"}},\"additionalProperties\":false},{\"type\":\"object\",\"required\":[\"kind\"],\"properties\":{\"kind\":{\"type\":\"string\",\"const\":\"removing\"}},\"additionalProperties\":false},{\"type\":\"object\",\"required\":[\"kind\",\"message\"],\"properties\":{\"kind\":{\"type\":\"string\",\"const\":\"failed\"},\"message\":{\"type\":\"string\"}},\"additionalProperties\":false}],\"discriminator\":{\"propertyName\":\"kind\"}},\"BreakpointMappingSnapshot\":{\"type\":\"object\",\"required\":[\"generatedColumn\",\"generatedLine\",\"generatedUrl\",\"projection\",\"quality\",\"requestedColumn\",\"requestedLine\",\"sourceUrl\"],\"properties\":{\"generatedColumn\":{\"type\":\"integer\"},\"generatedLine\":{\"type\":\"integer\"},\"generatedUrl\":{\"type\":\"string\"},\"projection\":{\"type\":\"array\",\"items\":{\"type\":\"string\"}},\"quality\":{\"type\":\"string\"},\"requestedColumn\":{\"type\":\"integer\"},\"requestedLine\":{\"type\":\"integer\"},\"sourceUrl\":{\"type\":\"string\"}},\"additionalProperties\":false},\"BreakpointPendingReason\":{\"oneOf\":[{\"type\":\"object\",\"required\":[\"kind\"],\"properties\":{\"kind\":{\"type\":\"string\",\"const\":\"waitingForTarget\"}},\"additionalProperties\":false},{\"type\":\"object\",\"required\":[\"kind\"],\"properties\":{\"kind\":{\"type\":\"string\",\"const\":\"waitingForScript\"}},\"additionalProperties\":false},{\"type\":\"object\",\"required\":[\"diagnostics\",\"kind\"],\"properties\":{\"diagnostics\":{\"type\":\"array\",\"items\":{\"type\":\"string\"}},\"kind\":{\"type\":\"string\",\"const\":\"sourceNotFound\"}},\"additionalProperties\":false},{\"type\":\"object\",\"required\":[\"candidates\",\"kind\",\"omitted_candidate_count\"],\"properties\":{\"candidates\":{\"type\":\"array\",\"items\":{\"$ref\":\"#/components/schemas/BreakpointSourceCandidateSnapshot\"}},\"kind\":{\"type\":\"string\",\"const\":\"ambiguousSource\"},\"omitted_candidate_count\":{\"type\":\"integer\"}},\"additionalProperties\":false},{\"type\":\"object\",\"required\":[\"diagnostics\",\"kind\"],\"properties\":{\"diagnostics\":{\"type\":\"array\",\"items\":{\"type\":\"string\"}},\"kind\":{\"type\":\"string\",\"const\":\"unmapped\"}},\"additionalProperties\":false},{\"type\":\"object\",\"required\":[\"kind\"],\"properties\":{\"kind\":{\"type\":\"string\",\"const\":\"applicable\"}},\"additionalProperties\":false},{\"type\":\"object\",\"required\":[\"kind\"],\"properties\":{\"kind\":{\"type\":\"string\",\"const\":\"installing\"}},\"additionalProperties\":false},{\"type\":\"object\",\"required\":[\"kind\",\"message\"],\"properties\":{\"kind\":{\"type\":\"string\",\"const\":\"failed\"},\"message\":{\"type\":\"string\"}},\"additionalProperties\":false}],\"discriminator\":{\"propertyName\":\"kind\"}},\"BreakpointScriptAssessmentSnapshot\":{\"type\":\"object\",\"required\":[\"connectionGeneration\",\"connectionId\",\"scriptId\",\"scriptUrl\",\"scriptVersion\",\"status\",\"targetId\"],\"properties\":{\"connectionGeneration\":{\"type\":\"integer\"},\"connectionId\":{\"type\":\"string\"},\"scriptId\":{\"type\":\"string\"},\"scriptUrl\":{\"type\":\"string\"},\"scriptVersion\":{\"type\":\"integer\"},\"status\":{\"$ref\":\"#/components/schemas/BreakpointScriptAssessmentStatus\"},\"targetId\":{\"type\":\"string\"}},\"additionalProperties\":false},\"BreakpointScriptAssessmentStatus\":{\"oneOf\":[{\"type\":\"object\",\"required\":[\"kind\"],\"properties\":{\"kind\":{\"type\":\"string\",\"const\":\"waitingForScript\"}},\"additionalProperties\":false},{\"type\":\"object\",\"required\":[\"diagnostics\",\"kind\"],\"properties\":{\"diagnostics\":{\"type\":\"array\",\"items\":{\"type\":\"string\"}},\"kind\":{\"type\":\"string\",\"const\":\"sourceNotFound\"}},\"additionalProperties\":false},{\"type\":\"object\",\"required\":[\"candidates\",\"kind\",\"omitted_candidate_count\"],\"properties\":{\"candidates\":{\"type\":\"array\",\"items\":{\"$ref\":\"#/components/schemas/BreakpointSourceCandidateSnapshot\"}},\"kind\":{\"type\":\"string\",\"const\":\"ambiguousSource\"},\"omitted_candidate_count\":{\"type\":\"integer\"}},\"additionalProperties\":false},{\"type\":\"object\",\"required\":[\"candidate\",\"kind\"],\"properties\":{\"candidate\":{\"$ref\":\"#/components/schemas/BreakpointSourceCandidateSnapshot\"},\"kind\":{\"type\":\"string\",\"const\":\"mapping\"}},\"additionalProperties\":false},{\"type\":\"object\",\"required\":[\"candidate\",\"diagnostics\",\"kind\"],\"properties\":{\"candidate\":{\"$ref\":\"#/components/schemas/BreakpointSourceCandidateSnapshot\"},\"diagnostics\":{\"type\":\"array\",\"items\":{\"type\":\"string\"}},\"kind\":{\"type\":\"string\",\"const\":\"unmapped\"}},\"additionalProperties\":false},{\"type\":\"object\",\"required\":[\"candidate\",\"kind\",\"mappings\"],\"properties\":{\"candidate\":{\"$ref\":\"#/components/schemas/BreakpointSourceCandidateSnapshot\"},\"kind\":{\"type\":\"string\",\"const\":\"applicable\"},\"mappings\":{\"type\":\"array\",\"items\":{\"$ref\":\"#/components/schemas/BreakpointMappingSnapshot\"}}},\"additionalProperties\":false},{\"type\":\"object\",\"required\":[\"kind\",\"message\"],\"properties\":{\"kind\":{\"type\":\"string\",\"const\":\"failed\"},\"message\":{\"type\":\"string\"}},\"additionalProperties\":false}],\"discriminator\":{\"propertyName\":\"kind\"}},\"BreakpointSnapshot\":{\"type\":\"object\",\"required\":[\"column\",\"enabled\",\"id\",\"line\",\"sourcePath\",\"status\"],\"properties\":{\"applications\":{\"type\":\"array\",\"items\":{\"$ref\":\"#/components/schemas/BreakpointApplicationSnapshot\"}},\"column\":{\"type\":\"integer\"},\"condition\":{\"type\":[\"string\",\"null\"]},\"enabled\":{\"type\":\"boolean\"},\"id\":{\"type\":\"string\"},\"line\":{\"type\":\"integer\"},\"pendingReason\":{\"anyOf\":[{\"$ref\":\"#/components/schemas/BreakpointPendingReason\"},{\"type\":\"null\"}]},\"sourcePath\":{\"type\":\"string\"},\"status\":{\"$ref\":\"#/components/schemas/BreakpointStatus\"},\"targetSelector\":{\"type\":[\"string\",\"null\"]},\"targets\":{\"type\":\"array\",\"items\":{\"$ref\":\"#/components/schemas/TargetBreakpointSnapshot\"}}},\"additionalProperties\":false},\"BreakpointSourceCandidateSnapshot\":{\"type\":\"object\",\"required\":[\"contentHash\",\"provenance\",\"sourceUrl\"],\"properties\":{\"contentHash\":{\"type\":\"string\"},\"provenance\":{\"type\":\"string\"},\"sourceUrl\":{\"type\":\"string\"}},\"additionalProperties\":false},\"BreakpointSpec\":{\"type\":\"object\",\"required\":[\"column\",\"enabled\",\"line\",\"sourcePath\"],\"properties\":{\"column\":{\"type\":\"integer\"},\"condition\":{\"type\":[\"string\",\"null\"]},\"enabled\":{\"type\":\"boolean\"},\"line\":{\"type\":\"integer\"},\"sourcePath\":{\"type\":\"string\"},\"targetSelector\":{\"type\":[\"string\",\"null\"]}},\"additionalProperties\":false},\"BreakpointStatus\":{\"oneOf\":[{\"type\":\"string\",\"enum\":[\"unconfirmed\",\"disabled\",\"pending\"]},{\"type\":\"object\",\"required\":[\"partiallyBound\"],\"properties\":{\"partiallyBound\":{\"type\":\"object\",\"required\":[\"application_count\"],\"properties\":{\"application_count\":{\"type\":\"integer\"}},\"additionalProperties\":false}},\"additionalProperties\":false},{\"type\":\"object\",\"required\":[\"bound\"],\"properties\":{\"bound\":{\"type\":\"object\",\"required\":[\"application_count\"],\"properties\":{\"application_count\":{\"type\":\"integer\"}},\"additionalProperties\":false}},\"additionalProperties\":false},{\"type\":\"object\",\"required\":[\"failed\"],\"properties\":{\"failed\":{\"type\":\"object\",\"required\":[\"message\"],\"properties\":{\"message\":{\"type\":\"string\"}},\"additionalProperties\":false}},\"additionalProperties\":false}]},\"CdpStdioTopology\":{\"type\":\"string\",\"enum\":[\"browser\",\"target\"]},\"ConnectionConfiguration\":{\"oneOf\":[{\"type\":\"object\",\"required\":[\"endpoint\",\"kind\"],\"properties\":{\"endpoint\":{\"type\":\"string\"},\"kind\":{\"type\":\"string\",\"const\":\"directCdp\"}},\"additionalProperties\":false},{\"type\":\"object\",\"required\":[\"endpoint\",\"kind\"],\"properties\":{\"endpoint\":{\"type\":\"string\"},\"kind\":{\"type\":\"string\",\"const\":\"nodeInspector\"}},\"additionalProperties\":false},{\"type\":\"object\",\"required\":[\"kind\",\"processId\"],\"properties\":{\"kind\":{\"type\":\"string\",\"const\":\"process\"},\"processId\":{\"type\":\"integer\"}},\"additionalProperties\":false},{\"type\":\"object\",\"required\":[\"kind\",\"rootPid\"],\"properties\":{\"kind\":{\"type\":\"string\",\"const\":\"processTree\"},\"rootPid\":{\"type\":\"integer\"}},\"additionalProperties\":false},{\"description\":\"Uses the process tree rooted at `root_pid` as the access path while exposing only `target_id` and its descendants as this connection's public target scope.\",\"type\":\"object\",\"required\":[\"kind\",\"rootPid\",\"targetId\"],\"properties\":{\"kind\":{\"type\":\"string\",\"const\":\"scopedProcessTree\"},\"rootPid\":{\"type\":\"integer\"},\"targetId\":{\"type\":\"string\"}},\"additionalProperties\":false},{\"type\":\"object\",\"required\":[\"channel\",\"headless\",\"kind\",\"url\"],\"properties\":{\"channel\":{\"$ref\":\"#/components/schemas/PlaywrightChannel\"},\"headless\":{\"type\":\"boolean\"},\"ignoreHttpsErrors\":{\"type\":\"boolean\"},\"kind\":{\"type\":\"string\",\"const\":\"playwright\"},\"playwrightPackage\":{\"type\":[\"string\",\"null\"]},\"url\":{\"type\":\"string\"}},\"additionalProperties\":false},{\"type\":\"object\",\"required\":[\"args\",\"executable\",\"headless\",\"kind\",\"url\"],\"properties\":{\"args\":{\"type\":\"array\",\"items\":{\"type\":\"string\"}},\"executable\":{\"type\":\"string\"},\"headless\":{\"type\":\"boolean\"},\"kind\":{\"type\":\"string\",\"const\":\"chrome\"},\"url\":{\"type\":\"string\"},\"userDataDir\":{\"type\":[\"string\",\"null\"]}},\"additionalProperties\":false},{\"type\":\"object\",\"required\":[\"args\",\"cwd\",\"env\",\"kind\",\"program\",\"runtimeExecutable\"],\"properties\":{\"args\":{\"type\":\"array\",\"items\":{\"type\":\"string\"}},\"cwd\":{\"type\":\"string\"},\"env\":{\"type\":\"object\",\"additionalProperties\":{\"type\":\"string\"},\"properties\":{}},\"kind\":{\"type\":\"string\",\"const\":\"node\"},\"program\":{\"type\":\"string\"},\"runtimeArgs\":{\"type\":\"array\",\"items\":{\"type\":\"string\"}},\"runtimeExecutable\":{\"type\":\"string\"}},\"additionalProperties\":false},{\"type\":\"object\",\"required\":[\"args\",\"command\",\"cwd\",\"env\",\"kind\"],\"properties\":{\"args\":{\"type\":\"array\",\"items\":{\"type\":\"string\"}},\"command\":{\"type\":\"string\"},\"cwd\":{\"type\":\"string\"},\"env\":{\"type\":\"object\",\"additionalProperties\":{\"type\":\"string\"},\"properties\":{}},\"kind\":{\"type\":\"string\",\"const\":\"stdio\"},\"topology\":{\"$ref\":\"#/components/schemas/CdpStdioTopology\"}},\"additionalProperties\":false}],\"discriminator\":{\"propertyName\":\"kind\"}},\"ConnectionSnapshot\":{\"type\":\"object\",\"required\":[\"configuration\",\"generation\",\"id\",\"status\",\"targets\"],\"properties\":{\"configuration\":{\"$ref\":\"#/components/schemas/ConnectionConfiguration\"},\"generation\":{\"type\":\"integer\"},\"id\":{\"type\":\"string\"},\"status\":{\"$ref\":\"#/components/schemas/ConnectionStatus\"},\"targets\":{\"type\":\"array\",\"items\":{\"$ref\":\"#/components/schemas/TargetSnapshot\"}}},\"additionalProperties\":false},\"ConnectionStatus\":{\"oneOf\":[{\"type\":\"object\",\"required\":[\"kind\"],\"properties\":{\"kind\":{\"type\":\"string\",\"const\":\"disconnected\"}},\"additionalProperties\":false},{\"type\":\"object\",\"required\":[\"kind\"],\"properties\":{\"kind\":{\"type\":\"string\",\"const\":\"connecting\"}},\"additionalProperties\":false},{\"type\":\"object\",\"required\":[\"kind\"],\"properties\":{\"kind\":{\"type\":\"string\",\"const\":\"disconnecting\"}},\"additionalProperties\":false},{\"type\":\"object\",\"required\":[\"kind\",\"product\",\"protocolVersion\"],\"properties\":{\"kind\":{\"type\":\"string\",\"const\":\"connected\"},\"product\":{\"type\":\"string\"},\"protocolVersion\":{\"type\":\"string\"}},\"additionalProperties\":false},{\"type\":\"object\",\"required\":[\"kind\",\"message\"],\"properties\":{\"kind\":{\"type\":\"string\",\"const\":\"failed\"},\"message\":{\"type\":\"string\"}},\"additionalProperties\":false}],\"discriminator\":{\"propertyName\":\"kind\"}},\"ContextEventSnapshot\":{\"type\":\"object\",\"required\":[\"kind\",\"revision\"],\"properties\":{\"kind\":{\"type\":\"string\"},\"revision\":{\"type\":\"integer\"},\"subjectId\":{\"type\":[\"string\",\"null\"]}},\"additionalProperties\":false},\"ContextKind\":{\"type\":\"string\",\"enum\":[\"path\",\"named\"]},\"ContextObservation\":{\"type\":\"object\",\"required\":[\"events\",\"snapshot\"],\"properties\":{\"events\":{\"type\":\"array\",\"items\":{\"$ref\":\"#/components/schemas/ContextEventSnapshot\"}},\"snapshot\":{\"$ref\":\"#/components/schemas/ContextSnapshot\"}},\"additionalProperties\":false},\"ContextSnapshot\":{\"type\":\"object\",\"required\":[\"agentInstanceId\",\"breakpoints\",\"connections\",\"displayName\",\"id\",\"revision\",\"targetForest\"],\"properties\":{\"agentInstanceId\":{\"type\":\"string\"},\"breakpoints\":{\"type\":\"array\",\"items\":{\"$ref\":\"#/components/schemas/BreakpointSnapshot\"}},\"connections\":{\"type\":\"array\",\"items\":{\"$ref\":\"#/components/schemas/ConnectionSnapshot\"}},\"displayName\":{\"type\":\"string\"},\"id\":{\"type\":\"string\"},\"resourceRevision\":{\"type\":\"integer\"},\"revision\":{\"type\":\"integer\"},\"sourceFormatting\":{\"$ref\":\"#/components/schemas/SourceFormattingSettings\"},\"targetForest\":{\"type\":\"array\",\"items\":{\"$ref\":\"#/components/schemas/TargetNodeSnapshot\"}}},\"additionalProperties\":false},\"ContextSummary\":{\"type\":\"object\",\"required\":[\"agentInstanceId\",\"breakpointCount\",\"connectionCount\",\"displayName\",\"id\",\"kind\",\"revision\"],\"properties\":{\"agentInstanceId\":{\"type\":\"string\"},\"breakpointCount\":{\"type\":\"integer\"},\"connectionCount\":{\"type\":\"integer\"},\"displayName\":{\"type\":\"string\"},\"id\":{\"type\":\"string\"},\"kind\":{\"$ref\":\"#/components/schemas/ContextKind\"},\"pathAncestor\":{\"type\":[\"boolean\",\"null\"]},\"pathDistance\":{\"type\":[\"integer\",\"null\"],\"format\":\"uint32\"},\"revision\":{\"type\":\"integer\"}},\"additionalProperties\":false},\"MutationOptions\":{\"type\":\"object\",\"properties\":{\"expectedRevision\":{\"type\":[\"integer\",\"null\"],\"format\":\"uint64\"},\"requestId\":{\"type\":[\"string\",\"null\"]}},\"additionalProperties\":false},\"ObservationCursor\":{\"oneOf\":[{\"type\":\"object\",\"required\":[\"kind\"],\"properties\":{\"kind\":{\"type\":\"string\",\"const\":\"current\"}},\"additionalProperties\":false},{\"type\":\"object\",\"required\":[\"kind\",\"revision\"],\"properties\":{\"kind\":{\"type\":\"string\",\"const\":\"after\"},\"revision\":{\"type\":\"integer\"}},\"additionalProperties\":false}],\"discriminator\":{\"propertyName\":\"kind\"}},\"ObservationResult\":{\"oneOf\":[{\"type\":\"object\",\"required\":[\"items\",\"kind\"],\"properties\":{\"items\":{\"type\":\"array\",\"items\":{\"$ref\":\"#/components/schemas/ContextObservation\"}},\"kind\":{\"type\":\"string\",\"const\":\"items\"}},\"additionalProperties\":false},{\"type\":\"object\",\"required\":[\"current\",\"kind\",\"oldest_available_revision\",\"requested_revision\"],\"properties\":{\"current\":{\"$ref\":\"#/components/schemas/ContextSnapshot\"},\"kind\":{\"type\":\"string\",\"const\":\"historyGap\"},\"oldest_available_revision\":{\"type\":\"integer\"},\"requested_revision\":{\"type\":\"integer\"}},\"additionalProperties\":false}],\"discriminator\":{\"propertyName\":\"kind\"}},\"PlaywrightChannel\":{\"type\":\"string\",\"enum\":[\"bundled\",\"chrome\",\"chromeBeta\",\"chromeDev\",\"chromeCanary\",\"msedge\",\"msedgeBeta\",\"msedgeDev\",\"msedgeCanary\"]},\"ResourceCapabilitySnapshot\":{\"type\":\"object\",\"required\":[\"detail\",\"handle\",\"kind\",\"source\",\"title\"],\"properties\":{\"detail\":{\"type\":\"object\",\"additionalProperties\":true,\"properties\":{}},\"handle\":{\"type\":\"integer\"},\"kind\":{\"type\":\"string\"},\"source\":{\"type\":\"string\"},\"title\":{\"type\":\"string\"}},\"additionalProperties\":false},\"ResourceFrontierSnapshot\":{\"type\":\"object\",\"required\":[\"relation\",\"state\"],\"properties\":{\"relation\":{\"type\":\"string\"},\"state\":true},\"additionalProperties\":false},\"ResourceGraphSnapshot\":{\"type\":\"object\",\"required\":[\"relations\",\"resources\",\"revision\"],\"properties\":{\"relations\":{\"type\":\"array\",\"items\":{\"$ref\":\"#/components/schemas/ResourceRelationSnapshot\"}},\"resources\":{\"type\":\"array\",\"items\":{\"$ref\":\"#/components/schemas/ResourceSnapshot\"}},\"revision\":{\"type\":\"integer\"}},\"additionalProperties\":false},\"ResourceRelationSnapshot\":{\"type\":\"object\",\"required\":[\"contributors\",\"from\",\"kind\",\"to\"],\"properties\":{\"contributors\":{\"type\":\"array\",\"items\":{\"type\":\"string\"}},\"from\":{\"type\":\"string\"},\"kind\":{\"type\":\"string\"},\"to\":{\"type\":\"string\"}},\"additionalProperties\":false},\"ResourceSnapshot\":{\"type\":\"object\",\"required\":[\"attributes\",\"capabilities\",\"contributors\",\"frontiers\",\"id\",\"kinds\"],\"properties\":{\"attributes\":{\"type\":\"object\",\"additionalProperties\":true,\"properties\":{}},\"capabilities\":{\"type\":\"array\",\"items\":{\"$ref\":\"#/components/schemas/ResourceCapabilitySnapshot\"}},\"contributors\":{\"type\":\"array\",\"items\":{\"type\":\"string\"}},\"frontiers\":{\"type\":\"array\",\"items\":{\"$ref\":\"#/components/schemas/ResourceFrontierSnapshot\"}},\"id\":{\"type\":\"string\"},\"kinds\":{\"type\":\"array\",\"items\":{\"type\":\"string\"}},\"label\":{\"type\":[\"string\",\"null\"]}},\"additionalProperties\":false},\"SourceExcerpt\":{\"type\":\"object\",\"required\":[\"currentLine\",\"highlightLength\",\"highlightStart\",\"lines\",\"sourceUrl\"],\"properties\":{\"breadcrumb\":{\"type\":[\"string\",\"null\"]},\"currentLine\":{\"type\":\"integer\"},\"highlightLength\":{\"type\":\"integer\"},\"highlightStart\":{\"type\":\"integer\"},\"lines\":{\"type\":\"array\",\"items\":{\"$ref\":\"#/components/schemas/SourceExcerptLine\"}},\"sourceUrl\":{\"type\":\"string\"}},\"additionalProperties\":false},\"SourceExcerptLine\":{\"type\":\"object\",\"required\":[\"line\",\"text\"],\"properties\":{\"line\":{\"type\":\"integer\"},\"text\":{\"type\":\"string\"}},\"additionalProperties\":false},\"SourceFormattingMode\":{\"type\":\"string\",\"enum\":[\"off\",\"auto\",\"on\"]},\"SourceFormattingRule\":{\"type\":\"object\",\"required\":[\"id\",\"mode\"],\"properties\":{\"id\":{\"type\":\"string\"},\"mode\":{\"$ref\":\"#/components/schemas/SourceFormattingMode\"},\"targetPattern\":{\"type\":[\"string\",\"null\"]},\"urlPattern\":{\"type\":[\"string\",\"null\"]}},\"additionalProperties\":false},\"SourceFormattingSettings\":{\"type\":\"object\",\"required\":[\"defaultMode\",\"rules\"],\"properties\":{\"defaultMode\":{\"$ref\":\"#/components/schemas/SourceFormattingMode\"},\"rules\":{\"type\":\"array\",\"items\":{\"$ref\":\"#/components/schemas/SourceFormattingRule\"}}},\"additionalProperties\":false},\"TargetAttachmentState\":{\"type\":\"string\",\"enum\":[\"detached\",\"external\",\"debugger\"]},\"TargetBreakpointSnapshot\":{\"type\":\"object\",\"required\":[\"column\",\"id\",\"line\",\"sourceUrl\",\"status\"],\"properties\":{\"applications\":{\"type\":\"array\",\"items\":{\"$ref\":\"#/components/schemas/BreakpointApplicationSnapshot\"}},\"assessments\":{\"type\":\"array\",\"items\":{\"$ref\":\"#/components/schemas/BreakpointScriptAssessmentSnapshot\"}},\"column\":{\"type\":\"integer\"},\"id\":{\"type\":\"string\"},\"line\":{\"type\":\"integer\"},\"source\":{\"anyOf\":[{\"$ref\":\"#/components/schemas/SourceExcerpt\"},{\"type\":\"null\"}]},\"sourceUrl\":{\"type\":\"string\"},\"status\":{\"$ref\":\"#/components/schemas/TargetBreakpointStatus\"}},\"additionalProperties\":false},\"TargetBreakpointStatus\":{\"oneOf\":[{\"type\":\"object\",\"required\":[\"kind\"],\"properties\":{\"kind\":{\"type\":\"string\",\"const\":\"waitingForScript\"}},\"additionalProperties\":false},{\"type\":\"object\",\"required\":[\"diagnostics\",\"kind\"],\"properties\":{\"diagnostics\":{\"type\":\"array\",\"items\":{\"type\":\"string\"}},\"kind\":{\"type\":\"string\",\"const\":\"sourceNotFound\"}},\"additionalProperties\":false},{\"type\":\"object\",\"required\":[\"candidates\",\"kind\",\"omitted_candidate_count\"],\"properties\":{\"candidates\":{\"type\":\"array\",\"items\":{\"$ref\":\"#/components/schemas/BreakpointSourceCandidateSnapshot\"}},\"kind\":{\"type\":\"string\",\"const\":\"ambiguousSource\"},\"omitted_candidate_count\":{\"type\":\"integer\"}},\"additionalProperties\":false},{\"type\":\"object\",\"required\":[\"diagnostics\",\"kind\"],\"properties\":{\"diagnostics\":{\"type\":\"array\",\"items\":{\"type\":\"string\"}},\"kind\":{\"type\":\"string\",\"const\":\"unmapped\"}},\"additionalProperties\":false},{\"type\":\"object\",\"required\":[\"kind\",\"mapping_count\"],\"properties\":{\"kind\":{\"type\":\"string\",\"const\":\"applicable\"},\"mapping_count\":{\"type\":\"integer\"}},\"additionalProperties\":false},{\"type\":\"object\",\"required\":[\"application_count\",\"kind\"],\"properties\":{\"application_count\":{\"type\":\"integer\"},\"kind\":{\"type\":\"string\",\"const\":\"installing\"}},\"additionalProperties\":false},{\"type\":\"object\",\"required\":[\"binding_count\",\"kind\"],\"properties\":{\"binding_count\":{\"type\":\"integer\"},\"kind\":{\"type\":\"string\",\"const\":\"installed\"}},\"additionalProperties\":false},{\"type\":\"object\",\"required\":[\"kind\",\"message\"],\"properties\":{\"kind\":{\"type\":\"string\",\"const\":\"failed\"},\"message\":{\"type\":\"string\"}},\"additionalProperties\":false}],\"discriminator\":{\"propertyName\":\"kind\"}},\"TargetNodeSnapshot\":{\"type\":\"object\",\"required\":[\"attachment\",\"connectionGeneration\",\"connectionId\",\"target\"],\"properties\":{\"attachment\":{\"$ref\":\"#/components/schemas/TargetAttachmentState\"},\"connectionGeneration\":{\"type\":\"integer\"},\"connectionId\":{\"type\":\"string\"},\"parentTargetId\":{\"type\":[\"string\",\"null\"]},\"target\":{\"$ref\":\"#/components/schemas/TargetSnapshot\"}},\"additionalProperties\":false},\"TargetSnapshot\":{\"type\":\"object\",\"required\":[\"attached\",\"targetId\",\"targetType\",\"title\",\"url\"],\"properties\":{\"attached\":{\"type\":\"boolean\"},\"browserContextId\":{\"type\":[\"string\",\"null\"]},\"openerId\":{\"type\":[\"string\",\"null\"]},\"parentId\":{\"type\":[\"string\",\"null\"]},\"subtype\":{\"type\":[\"string\",\"null\"]},\"targetId\":{\"type\":\"string\"},\"targetType\":{\"type\":\"string\"},\"title\":{\"type\":\"string\"},\"url\":{\"type\":\"string\"}},\"additionalProperties\":false}}}}");

export const ContextApi = new InterfaceDefinition(
    {
        id: "dev.dbgjs.context",
        hash: "84dc47d63168f609",
    },
    {
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
        disconnect_connection: requestType(
            z.object({
                connectionId: z.string(),
                contextId: z.string(),
            }),
            ContextSnapshotSchema,
        ),
        get_context: requestType(
            z.object({
                contextId: z.string(),
            }),
            ContextSnapshotSchema,
        ),
        get_resource_graph: requestType(
            z.object({
                contextId: z.string(),
            }),
            ResourceGraphSnapshotSchema,
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
        observe_context: requestType(
            z.object({
                contextId: z.string(),
                cursor: ObservationCursorSchema,
                timeoutMs: z.int(),
            }),
            ObservationResultSchema,
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
        set_pause_future_children: requestType(
            z.object({
                connectionId: z.string(),
                contextId: z.string(),
                enabled: z.boolean(),
            }),
            z.boolean(),
        ),
    },
    { frozenSchema: wireSchema },
);
