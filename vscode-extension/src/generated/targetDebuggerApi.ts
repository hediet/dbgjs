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

const SourceLocationSchema = z.object({
    column: z.int(),
    line: z.int(),
    sourceUrl: z.string(),
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

const PauseSnapshotSchema = z.object({
    epoch: z.int(),
    frames: z.array(FrameSnapshotSchema),
    reason: z.string(),
    source: z.union([
        SourceExcerptSchema,
        z.null(),
    ]).optional(),
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

const wireSchema: LinkRpcInterfaceSchema = JSON.parse("{\"id\":\"dev.dbgjs.target-debugger\",\"hash\":\"a36724cd17fb6c51\",\"methods\":{\"attach_target\":{\"params\":{\"type\":\"object\",\"required\":[\"connectionId\",\"contextId\",\"options\",\"targetId\"],\"properties\":{\"connectionId\":{\"type\":\"string\"},\"contextId\":{\"type\":\"string\"},\"options\":{\"$ref\":\"#/components/schemas/TargetAttachOptions\"},\"targetId\":{\"type\":\"string\"}},\"additionalProperties\":false},\"result\":{\"$ref\":\"#/components/schemas/TargetAttachmentResult\"}},\"detach_target\":{\"params\":{\"type\":\"object\",\"required\":[\"connectionId\",\"contextId\",\"targetId\"],\"properties\":{\"connectionId\":{\"type\":\"string\"},\"contextId\":{\"type\":\"string\"},\"expectedConnectionGeneration\":{\"type\":[\"integer\",\"null\"],\"format\":\"uint64\"},\"targetId\":{\"type\":\"string\"}},\"additionalProperties\":false},\"result\":{\"$ref\":\"#/components/schemas/ContextSnapshot\"}},\"evaluate_target\":{\"params\":{\"type\":\"object\",\"required\":[\"connectionId\",\"contextId\",\"expression\",\"frameIndex\",\"targetId\"],\"properties\":{\"connectionId\":{\"type\":\"string\"},\"contextId\":{\"type\":\"string\"},\"expression\":{\"type\":\"string\"},\"frameIndex\":{\"type\":\"integer\"},\"pauseEpoch\":{\"type\":[\"integer\",\"null\"],\"format\":\"uint64\"},\"targetId\":{\"type\":\"string\"}},\"additionalProperties\":false},\"result\":{\"$ref\":\"#/components/schemas/EvaluationSnapshot\"}},\"get_logs\":{\"params\":{\"type\":\"object\",\"required\":[\"connectionId\",\"contextId\",\"targetId\"],\"properties\":{\"connectionId\":{\"type\":\"string\"},\"contextId\":{\"type\":\"string\"},\"targetId\":{\"type\":\"string\"}},\"additionalProperties\":false},\"result\":{\"$ref\":\"#/components/schemas/TargetLogSnapshot\"}},\"get_object_properties\":{\"params\":{\"type\":\"object\",\"required\":[\"connectionId\",\"contextId\",\"objectId\",\"targetId\"],\"properties\":{\"connectionId\":{\"type\":\"string\"},\"contextId\":{\"type\":\"string\"},\"objectId\":{\"type\":\"string\"},\"pauseEpoch\":{\"type\":[\"integer\",\"null\"],\"format\":\"uint64\"},\"targetId\":{\"type\":\"string\"}},\"additionalProperties\":false},\"result\":{\"type\":\"array\",\"items\":{\"$ref\":\"#/components/schemas/VariableSnapshot\"}}},\"get_scope_variables\":{\"params\":{\"type\":\"object\",\"required\":[\"connectionId\",\"contextId\",\"frameIndex\",\"pauseEpoch\",\"scopeIndex\",\"targetId\"],\"properties\":{\"connectionId\":{\"type\":\"string\"},\"contextId\":{\"type\":\"string\"},\"frameIndex\":{\"type\":\"integer\"},\"pauseEpoch\":{\"type\":\"integer\"},\"scopeIndex\":{\"type\":\"integer\"},\"targetId\":{\"type\":\"string\"}},\"additionalProperties\":false},\"result\":{\"type\":\"array\",\"items\":{\"$ref\":\"#/components/schemas/VariableSnapshot\"}}},\"get_target\":{\"params\":{\"type\":\"object\",\"required\":[\"connectionId\",\"contextId\",\"targetId\"],\"properties\":{\"connectionId\":{\"type\":\"string\"},\"contextId\":{\"type\":\"string\"},\"targetId\":{\"type\":\"string\"}},\"additionalProperties\":false},\"result\":{\"$ref\":\"#/components/schemas/TargetDebuggerSnapshot\"}},\"inspect_value\":{\"params\":{\"type\":\"object\",\"required\":[\"connectionId\",\"contextId\",\"options\",\"selector\",\"targetId\"],\"properties\":{\"connectionId\":{\"type\":\"string\"},\"contextId\":{\"type\":\"string\"},\"options\":{\"$ref\":\"#/components/schemas/ValueInspectionOptions\"},\"pauseEpoch\":{\"type\":[\"integer\",\"null\"],\"format\":\"uint64\"},\"selector\":{\"$ref\":\"#/components/schemas/ValueSelector\"},\"targetId\":{\"type\":\"string\"}},\"additionalProperties\":false},\"result\":{\"$ref\":\"#/components/schemas/ValueSnapshot\"}},\"observe_target\":{\"params\":{\"type\":\"object\",\"required\":[\"afterRevision\",\"connectionId\",\"contextId\",\"targetId\",\"timeoutMs\"],\"properties\":{\"afterRevision\":{\"type\":\"integer\"},\"connectionId\":{\"type\":\"string\"},\"contextId\":{\"type\":\"string\"},\"targetId\":{\"type\":\"string\"},\"timeoutMs\":{\"type\":\"integer\"}},\"additionalProperties\":false},\"result\":{\"anyOf\":[{\"$ref\":\"#/components/schemas/TargetDebuggerSnapshot\"},{\"type\":\"null\"}]}},\"release_target\":{\"params\":{\"type\":\"object\",\"required\":[\"connectionId\",\"contextId\",\"targetId\"],\"properties\":{\"connectionId\":{\"type\":\"string\"},\"contextId\":{\"type\":\"string\"},\"targetId\":{\"type\":\"string\"}},\"additionalProperties\":false},\"result\":{\"$ref\":\"#/components/schemas/TargetDebuggerSnapshot\"}},\"resolve_target\":{\"params\":{\"type\":\"object\",\"required\":[\"contextId\",\"selector\"],\"properties\":{\"contextId\":{\"type\":\"string\"},\"selector\":{\"type\":\"string\"}},\"additionalProperties\":false},\"result\":{\"$ref\":\"#/components/schemas/CanonicalTargetSnapshot\"}},\"resume_target\":{\"params\":{\"type\":\"object\",\"required\":[\"connectionId\",\"contextId\",\"pauseEpoch\",\"targetId\"],\"properties\":{\"connectionId\":{\"type\":\"string\"},\"contextId\":{\"type\":\"string\"},\"pauseEpoch\":{\"type\":\"integer\"},\"targetId\":{\"type\":\"string\"}},\"additionalProperties\":false},\"result\":{\"$ref\":\"#/components/schemas/TargetDebuggerSnapshot\"}},\"set_logpoint\":{\"params\":{\"type\":\"object\",\"required\":[\"column\",\"connectionId\",\"contextId\",\"expression\",\"line\",\"logpointId\",\"sourceUrl\",\"targetId\"],\"properties\":{\"column\":{\"type\":\"integer\"},\"connectionId\":{\"type\":\"string\"},\"contextId\":{\"type\":\"string\"},\"expression\":{\"type\":\"string\"},\"line\":{\"type\":\"integer\"},\"logpointId\":{\"type\":\"string\"},\"sourceUrl\":{\"type\":\"string\"},\"targetId\":{\"type\":\"string\"}},\"additionalProperties\":false},\"result\":{\"$ref\":\"#/components/schemas/TargetDebuggerSnapshot\"}},\"set_logpoints\":{\"params\":{\"type\":\"object\",\"required\":[\"connectionId\",\"contextId\",\"logpoints\",\"targetId\"],\"properties\":{\"connectionId\":{\"type\":\"string\"},\"contextId\":{\"type\":\"string\"},\"logpoints\":{\"type\":\"array\",\"items\":{\"$ref\":\"#/components/schemas/LogpointSpec\"}},\"targetId\":{\"type\":\"string\"}},\"additionalProperties\":false},\"result\":{\"$ref\":\"#/components/schemas/TargetDebuggerSnapshot\"}},\"step_target\":{\"params\":{\"type\":\"object\",\"required\":[\"connectionId\",\"contextId\",\"kind\",\"pauseEpoch\",\"targetId\"],\"properties\":{\"connectionId\":{\"type\":\"string\"},\"contextId\":{\"type\":\"string\"},\"kind\":{\"$ref\":\"#/components/schemas/StepKind\"},\"pauseEpoch\":{\"type\":\"integer\"},\"targetId\":{\"type\":\"string\"}},\"additionalProperties\":false},\"result\":{\"$ref\":\"#/components/schemas/TargetDebuggerSnapshot\"}},\"wait_target\":{\"params\":{\"type\":\"object\",\"required\":[\"connectionId\",\"contextId\",\"predicate\",\"targetId\",\"timeoutMs\"],\"properties\":{\"connectionId\":{\"type\":\"string\"},\"contextId\":{\"type\":\"string\"},\"predicate\":{\"$ref\":\"#/components/schemas/TargetWaitPredicate\"},\"targetId\":{\"type\":\"string\"},\"timeoutMs\":{\"type\":\"integer\"}},\"additionalProperties\":false},\"result\":{\"$ref\":\"#/components/schemas/TargetDebuggerSnapshot\"}}},\"components\":{\"schemas\":{\"BreakpointApplicationSnapshot\":{\"type\":\"object\",\"required\":[\"connectionGeneration\",\"connectionId\",\"generatedColumn\",\"generatedLine\",\"scriptId\",\"scriptUrl\",\"scriptVersion\",\"status\",\"targetId\"],\"properties\":{\"connectionGeneration\":{\"type\":\"integer\"},\"connectionId\":{\"type\":\"string\"},\"generatedColumn\":{\"type\":\"integer\"},\"generatedLine\":{\"type\":\"integer\"},\"mapping\":{\"anyOf\":[{\"$ref\":\"#/components/schemas/BreakpointMappingSnapshot\"},{\"type\":\"null\"}]},\"scriptId\":{\"type\":\"string\"},\"scriptUrl\":{\"type\":\"string\"},\"scriptVersion\":{\"type\":\"integer\"},\"status\":{\"$ref\":\"#/components/schemas/BreakpointApplicationStatus\"},\"targetId\":{\"type\":\"string\"}},\"additionalProperties\":false},\"BreakpointApplicationStatus\":{\"oneOf\":[{\"type\":\"object\",\"required\":[\"kind\"],\"properties\":{\"kind\":{\"type\":\"string\",\"const\":\"installing\"}},\"additionalProperties\":false},{\"type\":\"object\",\"required\":[\"backend_id\",\"kind\"],\"properties\":{\"backend_id\":{\"type\":\"string\"},\"kind\":{\"type\":\"string\",\"const\":\"installed\"}},\"additionalProperties\":false},{\"type\":\"object\",\"required\":[\"kind\"],\"properties\":{\"kind\":{\"type\":\"string\",\"const\":\"removing\"}},\"additionalProperties\":false},{\"type\":\"object\",\"required\":[\"kind\",\"message\"],\"properties\":{\"kind\":{\"type\":\"string\",\"const\":\"failed\"},\"message\":{\"type\":\"string\"}},\"additionalProperties\":false}],\"discriminator\":{\"propertyName\":\"kind\"}},\"BreakpointMappingSnapshot\":{\"type\":\"object\",\"required\":[\"generatedColumn\",\"generatedLine\",\"generatedUrl\",\"projection\",\"quality\",\"requestedColumn\",\"requestedLine\",\"sourceUrl\"],\"properties\":{\"generatedColumn\":{\"type\":\"integer\"},\"generatedLine\":{\"type\":\"integer\"},\"generatedUrl\":{\"type\":\"string\"},\"projection\":{\"type\":\"array\",\"items\":{\"type\":\"string\"}},\"quality\":{\"type\":\"string\"},\"requestedColumn\":{\"type\":\"integer\"},\"requestedLine\":{\"type\":\"integer\"},\"sourceUrl\":{\"type\":\"string\"}},\"additionalProperties\":false},\"BreakpointPendingReason\":{\"oneOf\":[{\"type\":\"object\",\"required\":[\"kind\"],\"properties\":{\"kind\":{\"type\":\"string\",\"const\":\"waitingForTarget\"}},\"additionalProperties\":false},{\"type\":\"object\",\"required\":[\"kind\"],\"properties\":{\"kind\":{\"type\":\"string\",\"const\":\"waitingForScript\"}},\"additionalProperties\":false},{\"type\":\"object\",\"required\":[\"diagnostics\",\"kind\"],\"properties\":{\"diagnostics\":{\"type\":\"array\",\"items\":{\"type\":\"string\"}},\"kind\":{\"type\":\"string\",\"const\":\"sourceNotFound\"}},\"additionalProperties\":false},{\"type\":\"object\",\"required\":[\"candidates\",\"kind\",\"omitted_candidate_count\"],\"properties\":{\"candidates\":{\"type\":\"array\",\"items\":{\"$ref\":\"#/components/schemas/BreakpointSourceCandidateSnapshot\"}},\"kind\":{\"type\":\"string\",\"const\":\"ambiguousSource\"},\"omitted_candidate_count\":{\"type\":\"integer\"}},\"additionalProperties\":false},{\"type\":\"object\",\"required\":[\"diagnostics\",\"kind\"],\"properties\":{\"diagnostics\":{\"type\":\"array\",\"items\":{\"type\":\"string\"}},\"kind\":{\"type\":\"string\",\"const\":\"unmapped\"}},\"additionalProperties\":false},{\"type\":\"object\",\"required\":[\"kind\"],\"properties\":{\"kind\":{\"type\":\"string\",\"const\":\"applicable\"}},\"additionalProperties\":false},{\"type\":\"object\",\"required\":[\"kind\"],\"properties\":{\"kind\":{\"type\":\"string\",\"const\":\"installing\"}},\"additionalProperties\":false},{\"type\":\"object\",\"required\":[\"kind\",\"message\"],\"properties\":{\"kind\":{\"type\":\"string\",\"const\":\"failed\"},\"message\":{\"type\":\"string\"}},\"additionalProperties\":false}],\"discriminator\":{\"propertyName\":\"kind\"}},\"BreakpointScriptAssessmentSnapshot\":{\"type\":\"object\",\"required\":[\"connectionGeneration\",\"connectionId\",\"scriptId\",\"scriptUrl\",\"scriptVersion\",\"status\",\"targetId\"],\"properties\":{\"connectionGeneration\":{\"type\":\"integer\"},\"connectionId\":{\"type\":\"string\"},\"scriptId\":{\"type\":\"string\"},\"scriptUrl\":{\"type\":\"string\"},\"scriptVersion\":{\"type\":\"integer\"},\"status\":{\"$ref\":\"#/components/schemas/BreakpointScriptAssessmentStatus\"},\"targetId\":{\"type\":\"string\"}},\"additionalProperties\":false},\"BreakpointScriptAssessmentStatus\":{\"oneOf\":[{\"type\":\"object\",\"required\":[\"kind\"],\"properties\":{\"kind\":{\"type\":\"string\",\"const\":\"waitingForScript\"}},\"additionalProperties\":false},{\"type\":\"object\",\"required\":[\"diagnostics\",\"kind\"],\"properties\":{\"diagnostics\":{\"type\":\"array\",\"items\":{\"type\":\"string\"}},\"kind\":{\"type\":\"string\",\"const\":\"sourceNotFound\"}},\"additionalProperties\":false},{\"type\":\"object\",\"required\":[\"candidates\",\"kind\",\"omitted_candidate_count\"],\"properties\":{\"candidates\":{\"type\":\"array\",\"items\":{\"$ref\":\"#/components/schemas/BreakpointSourceCandidateSnapshot\"}},\"kind\":{\"type\":\"string\",\"const\":\"ambiguousSource\"},\"omitted_candidate_count\":{\"type\":\"integer\"}},\"additionalProperties\":false},{\"type\":\"object\",\"required\":[\"candidate\",\"kind\"],\"properties\":{\"candidate\":{\"$ref\":\"#/components/schemas/BreakpointSourceCandidateSnapshot\"},\"kind\":{\"type\":\"string\",\"const\":\"mapping\"}},\"additionalProperties\":false},{\"type\":\"object\",\"required\":[\"candidate\",\"diagnostics\",\"kind\"],\"properties\":{\"candidate\":{\"$ref\":\"#/components/schemas/BreakpointSourceCandidateSnapshot\"},\"diagnostics\":{\"type\":\"array\",\"items\":{\"type\":\"string\"}},\"kind\":{\"type\":\"string\",\"const\":\"unmapped\"}},\"additionalProperties\":false},{\"type\":\"object\",\"required\":[\"candidate\",\"kind\",\"mappings\"],\"properties\":{\"candidate\":{\"$ref\":\"#/components/schemas/BreakpointSourceCandidateSnapshot\"},\"kind\":{\"type\":\"string\",\"const\":\"applicable\"},\"mappings\":{\"type\":\"array\",\"items\":{\"$ref\":\"#/components/schemas/BreakpointMappingSnapshot\"}}},\"additionalProperties\":false},{\"type\":\"object\",\"required\":[\"kind\",\"message\"],\"properties\":{\"kind\":{\"type\":\"string\",\"const\":\"failed\"},\"message\":{\"type\":\"string\"}},\"additionalProperties\":false}],\"discriminator\":{\"propertyName\":\"kind\"}},\"BreakpointSnapshot\":{\"type\":\"object\",\"required\":[\"column\",\"enabled\",\"id\",\"line\",\"sourcePath\",\"status\"],\"properties\":{\"applications\":{\"type\":\"array\",\"items\":{\"$ref\":\"#/components/schemas/BreakpointApplicationSnapshot\"}},\"column\":{\"type\":\"integer\"},\"condition\":{\"type\":[\"string\",\"null\"]},\"enabled\":{\"type\":\"boolean\"},\"id\":{\"type\":\"string\"},\"line\":{\"type\":\"integer\"},\"pendingReason\":{\"anyOf\":[{\"$ref\":\"#/components/schemas/BreakpointPendingReason\"},{\"type\":\"null\"}]},\"sourcePath\":{\"type\":\"string\"},\"status\":{\"$ref\":\"#/components/schemas/BreakpointStatus\"},\"targetSelector\":{\"type\":[\"string\",\"null\"]},\"targets\":{\"type\":\"array\",\"items\":{\"$ref\":\"#/components/schemas/TargetBreakpointSnapshot\"}}},\"additionalProperties\":false},\"BreakpointSourceCandidateSnapshot\":{\"type\":\"object\",\"required\":[\"contentHash\",\"provenance\",\"sourceUrl\"],\"properties\":{\"contentHash\":{\"type\":\"string\"},\"provenance\":{\"type\":\"string\"},\"sourceUrl\":{\"type\":\"string\"}},\"additionalProperties\":false},\"BreakpointStatus\":{\"oneOf\":[{\"type\":\"string\",\"enum\":[\"unconfirmed\",\"disabled\",\"pending\"]},{\"type\":\"object\",\"required\":[\"partiallyBound\"],\"properties\":{\"partiallyBound\":{\"type\":\"object\",\"required\":[\"application_count\"],\"properties\":{\"application_count\":{\"type\":\"integer\"}},\"additionalProperties\":false}},\"additionalProperties\":false},{\"type\":\"object\",\"required\":[\"bound\"],\"properties\":{\"bound\":{\"type\":\"object\",\"required\":[\"application_count\"],\"properties\":{\"application_count\":{\"type\":\"integer\"}},\"additionalProperties\":false}},\"additionalProperties\":false},{\"type\":\"object\",\"required\":[\"failed\"],\"properties\":{\"failed\":{\"type\":\"object\",\"required\":[\"message\"],\"properties\":{\"message\":{\"type\":\"string\"}},\"additionalProperties\":false}},\"additionalProperties\":false}]},\"CanonicalTargetSnapshot\":{\"type\":\"object\",\"required\":[\"connectionGeneration\",\"connectionId\",\"contextId\",\"resourceId\",\"target\",\"targetId\"],\"properties\":{\"connectionGeneration\":{\"type\":\"integer\"},\"connectionId\":{\"type\":\"string\"},\"contextId\":{\"type\":\"string\"},\"resourceId\":{\"type\":\"string\"},\"target\":{\"$ref\":\"#/components/schemas/TargetSnapshot\"},\"targetId\":{\"type\":\"string\"}},\"additionalProperties\":false},\"CdpStdioTopology\":{\"type\":\"string\",\"enum\":[\"browser\",\"target\"]},\"ConnectionConfiguration\":{\"oneOf\":[{\"type\":\"object\",\"required\":[\"endpoint\",\"kind\"],\"properties\":{\"endpoint\":{\"type\":\"string\"},\"kind\":{\"type\":\"string\",\"const\":\"directCdp\"}},\"additionalProperties\":false},{\"type\":\"object\",\"required\":[\"endpoint\",\"kind\"],\"properties\":{\"endpoint\":{\"type\":\"string\"},\"kind\":{\"type\":\"string\",\"const\":\"nodeInspector\"}},\"additionalProperties\":false},{\"type\":\"object\",\"required\":[\"kind\",\"processId\"],\"properties\":{\"kind\":{\"type\":\"string\",\"const\":\"process\"},\"processId\":{\"type\":\"integer\"}},\"additionalProperties\":false},{\"type\":\"object\",\"required\":[\"kind\",\"rootPid\"],\"properties\":{\"kind\":{\"type\":\"string\",\"const\":\"processTree\"},\"rootPid\":{\"type\":\"integer\"}},\"additionalProperties\":false},{\"description\":\"Uses the process tree rooted at `root_pid` as the access path while exposing only `target_id` and its descendants as this connection's public target scope.\",\"type\":\"object\",\"required\":[\"kind\",\"rootPid\",\"targetId\"],\"properties\":{\"kind\":{\"type\":\"string\",\"const\":\"scopedProcessTree\"},\"rootPid\":{\"type\":\"integer\"},\"targetId\":{\"type\":\"string\"}},\"additionalProperties\":false},{\"type\":\"object\",\"required\":[\"channel\",\"headless\",\"kind\",\"url\"],\"properties\":{\"channel\":{\"$ref\":\"#/components/schemas/PlaywrightChannel\"},\"headless\":{\"type\":\"boolean\"},\"ignoreHttpsErrors\":{\"type\":\"boolean\"},\"kind\":{\"type\":\"string\",\"const\":\"playwright\"},\"playwrightPackage\":{\"type\":[\"string\",\"null\"]},\"url\":{\"type\":\"string\"}},\"additionalProperties\":false},{\"type\":\"object\",\"required\":[\"args\",\"executable\",\"headless\",\"kind\",\"url\"],\"properties\":{\"args\":{\"type\":\"array\",\"items\":{\"type\":\"string\"}},\"executable\":{\"type\":\"string\"},\"headless\":{\"type\":\"boolean\"},\"kind\":{\"type\":\"string\",\"const\":\"chrome\"},\"url\":{\"type\":\"string\"},\"userDataDir\":{\"type\":[\"string\",\"null\"]}},\"additionalProperties\":false},{\"type\":\"object\",\"required\":[\"args\",\"cwd\",\"env\",\"kind\",\"program\",\"runtimeExecutable\"],\"properties\":{\"args\":{\"type\":\"array\",\"items\":{\"type\":\"string\"}},\"cwd\":{\"type\":\"string\"},\"env\":{\"type\":\"object\",\"additionalProperties\":{\"type\":\"string\"},\"properties\":{}},\"kind\":{\"type\":\"string\",\"const\":\"node\"},\"program\":{\"type\":\"string\"},\"runtimeArgs\":{\"type\":\"array\",\"items\":{\"type\":\"string\"}},\"runtimeExecutable\":{\"type\":\"string\"}},\"additionalProperties\":false},{\"type\":\"object\",\"required\":[\"args\",\"command\",\"cwd\",\"env\",\"kind\"],\"properties\":{\"args\":{\"type\":\"array\",\"items\":{\"type\":\"string\"}},\"command\":{\"type\":\"string\"},\"cwd\":{\"type\":\"string\"},\"env\":{\"type\":\"object\",\"additionalProperties\":{\"type\":\"string\"},\"properties\":{}},\"kind\":{\"type\":\"string\",\"const\":\"stdio\"},\"topology\":{\"$ref\":\"#/components/schemas/CdpStdioTopology\"}},\"additionalProperties\":false}],\"discriminator\":{\"propertyName\":\"kind\"}},\"ConnectionSnapshot\":{\"type\":\"object\",\"required\":[\"configuration\",\"generation\",\"id\",\"status\",\"targets\"],\"properties\":{\"configuration\":{\"$ref\":\"#/components/schemas/ConnectionConfiguration\"},\"generation\":{\"type\":\"integer\"},\"id\":{\"type\":\"string\"},\"status\":{\"$ref\":\"#/components/schemas/ConnectionStatus\"},\"targets\":{\"type\":\"array\",\"items\":{\"$ref\":\"#/components/schemas/TargetSnapshot\"}}},\"additionalProperties\":false},\"ConnectionStatus\":{\"oneOf\":[{\"type\":\"object\",\"required\":[\"kind\"],\"properties\":{\"kind\":{\"type\":\"string\",\"const\":\"disconnected\"}},\"additionalProperties\":false},{\"type\":\"object\",\"required\":[\"kind\"],\"properties\":{\"kind\":{\"type\":\"string\",\"const\":\"connecting\"}},\"additionalProperties\":false},{\"type\":\"object\",\"required\":[\"kind\"],\"properties\":{\"kind\":{\"type\":\"string\",\"const\":\"disconnecting\"}},\"additionalProperties\":false},{\"type\":\"object\",\"required\":[\"kind\",\"product\",\"protocolVersion\"],\"properties\":{\"kind\":{\"type\":\"string\",\"const\":\"connected\"},\"product\":{\"type\":\"string\"},\"protocolVersion\":{\"type\":\"string\"}},\"additionalProperties\":false},{\"type\":\"object\",\"required\":[\"kind\",\"message\"],\"properties\":{\"kind\":{\"type\":\"string\",\"const\":\"failed\"},\"message\":{\"type\":\"string\"}},\"additionalProperties\":false}],\"discriminator\":{\"propertyName\":\"kind\"}},\"ConsoleMessageSnapshot\":{\"type\":\"object\",\"required\":[\"index\",\"values\"],\"properties\":{\"index\":{\"type\":\"integer\"},\"params\":true,\"values\":{\"type\":\"array\",\"items\":{\"type\":\"string\"}}},\"additionalProperties\":false},\"ContextSnapshot\":{\"type\":\"object\",\"required\":[\"agentInstanceId\",\"breakpoints\",\"connections\",\"displayName\",\"id\",\"revision\",\"targetForest\"],\"properties\":{\"agentInstanceId\":{\"type\":\"string\"},\"breakpoints\":{\"type\":\"array\",\"items\":{\"$ref\":\"#/components/schemas/BreakpointSnapshot\"}},\"connections\":{\"type\":\"array\",\"items\":{\"$ref\":\"#/components/schemas/ConnectionSnapshot\"}},\"displayName\":{\"type\":\"string\"},\"id\":{\"type\":\"string\"},\"resourceRevision\":{\"type\":\"integer\"},\"revision\":{\"type\":\"integer\"},\"sourceFormatting\":{\"$ref\":\"#/components/schemas/SourceFormattingSettings\"},\"targetForest\":{\"type\":\"array\",\"items\":{\"$ref\":\"#/components/schemas/TargetNodeSnapshot\"}}},\"additionalProperties\":false},\"EvaluationSnapshot\":{\"type\":\"object\",\"required\":[\"expression\",\"kind\",\"preview\"],\"properties\":{\"description\":{\"type\":[\"string\",\"null\"]},\"expression\":{\"type\":\"string\"},\"kind\":{\"type\":\"string\"},\"objectId\":{\"type\":[\"string\",\"null\"]},\"preview\":{\"$ref\":\"#/components/schemas/ValuePreviewSnapshot\"},\"unserializableValue\":{\"type\":[\"string\",\"null\"]},\"value\":true},\"additionalProperties\":false},\"FrameProjectionSnapshot\":{\"oneOf\":[{\"type\":\"object\",\"required\":[\"kind\"],\"properties\":{\"kind\":{\"type\":\"string\",\"const\":\"raw\"}},\"additionalProperties\":false},{\"type\":\"object\",\"required\":[\"kind\"],\"properties\":{\"kind\":{\"type\":\"string\",\"const\":\"pending\"}},\"additionalProperties\":false},{\"type\":\"object\",\"required\":[\"kind\",\"location\"],\"properties\":{\"kind\":{\"type\":\"string\",\"const\":\"resolved\"},\"location\":{\"$ref\":\"#/components/schemas/SourceLocation\"}},\"additionalProperties\":false},{\"type\":\"object\",\"required\":[\"kind\",\"message\"],\"properties\":{\"kind\":{\"type\":\"string\",\"const\":\"failed\"},\"message\":{\"type\":\"string\"}},\"additionalProperties\":false}],\"discriminator\":{\"propertyName\":\"kind\"}},\"FrameSnapshot\":{\"type\":\"object\",\"required\":[\"functionName\",\"index\",\"projected\",\"raw\",\"scopes\"],\"properties\":{\"breadcrumb\":{\"type\":[\"string\",\"null\"]},\"functionName\":{\"type\":\"string\"},\"index\":{\"type\":\"integer\"},\"projected\":{\"$ref\":\"#/components/schemas/FrameProjectionSnapshot\"},\"raw\":{\"$ref\":\"#/components/schemas/SourceLocation\"},\"scopes\":{\"type\":\"array\",\"items\":{\"$ref\":\"#/components/schemas/ScopeSnapshot\"}}},\"additionalProperties\":false},\"LogCaptureSnapshot\":{\"type\":\"object\",\"required\":[\"collectedEvents\",\"status\"],\"properties\":{\"captureId\":{\"type\":[\"string\",\"null\"]},\"collectedEvents\":{\"type\":\"array\",\"items\":{\"type\":\"string\"}},\"droppedCount\":{\"type\":[\"integer\",\"null\"],\"format\":\"uint64\"},\"evictedCount\":{\"type\":[\"integer\",\"null\"],\"format\":\"uint64\"},\"sessionId\":{\"type\":[\"string\",\"null\"]},\"startedAtUnixMs\":{\"type\":[\"integer\",\"null\"],\"format\":\"uint64\"},\"status\":{\"$ref\":\"#/components/schemas/LogCaptureStatus\"}},\"additionalProperties\":false},\"LogCaptureStatus\":{\"type\":\"string\",\"enum\":[\"active\",\"inactive\",\"stopped\",\"unknown\"]},\"LogpointSpec\":{\"type\":\"object\",\"required\":[\"column\",\"expression\",\"id\",\"line\",\"sourceUrl\"],\"properties\":{\"column\":{\"type\":\"integer\"},\"expression\":{\"type\":\"string\"},\"id\":{\"type\":\"string\"},\"line\":{\"type\":\"integer\"},\"sourceUrl\":{\"type\":\"string\"}},\"additionalProperties\":false},\"ObjectLocationSnapshot\":{\"type\":\"object\",\"required\":[\"kind\",\"origin\",\"position\",\"scriptId\"],\"properties\":{\"kind\":{\"type\":\"string\"},\"origin\":{\"type\":\"string\"},\"position\":{\"$ref\":\"#/components/schemas/ResolvedSourcePosition\"},\"scriptId\":{\"type\":\"string\"}},\"additionalProperties\":false},\"ObjectSourceSnapshot\":{\"type\":\"object\",\"required\":[\"diagnostics\",\"locations\"],\"properties\":{\"diagnostics\":{\"type\":\"array\",\"items\":{\"type\":\"string\"}},\"locations\":{\"type\":\"array\",\"items\":{\"$ref\":\"#/components/schemas/ObjectLocationSnapshot\"}}},\"additionalProperties\":false},\"PauseSnapshot\":{\"type\":\"object\",\"required\":[\"epoch\",\"frames\",\"reason\"],\"properties\":{\"epoch\":{\"type\":\"integer\"},\"frames\":{\"type\":\"array\",\"items\":{\"$ref\":\"#/components/schemas/FrameSnapshot\"}},\"reason\":{\"type\":\"string\"},\"source\":{\"anyOf\":[{\"$ref\":\"#/components/schemas/SourceExcerpt\"},{\"type\":\"null\"}]}},\"additionalProperties\":false},\"PlaywrightChannel\":{\"type\":\"string\",\"enum\":[\"bundled\",\"chrome\",\"chromeBeta\",\"chromeDev\",\"chromeCanary\",\"msedge\",\"msedgeBeta\",\"msedgeDev\",\"msedgeCanary\"]},\"PromiseClassification\":{\"type\":\"string\",\"const\":\"indeterminate\"},\"PromiseOrigin\":{\"type\":\"string\",\"enum\":[\"live\",\"heapSnapshot\"]},\"PromiseSnapshot\":{\"type\":\"object\",\"required\":[\"classification\",\"origin\",\"state\"],\"properties\":{\"classification\":{\"$ref\":\"#/components/schemas/PromiseClassification\"},\"origin\":{\"$ref\":\"#/components/schemas/PromiseOrigin\"},\"reference\":{\"type\":[\"string\",\"null\"]},\"retained\":{\"type\":[\"boolean\",\"null\"]},\"settlement\":{\"anyOf\":[{\"$ref\":\"#/components/schemas/ValuePreviewSnapshot\"},{\"type\":\"null\"}]},\"state\":{\"$ref\":\"#/components/schemas/PromiseState\"}},\"additionalProperties\":false},\"PromiseState\":{\"type\":\"string\",\"enum\":[\"pending\",\"fulfilled\",\"rejected\",\"unknown\"]},\"ResolvedSourcePosition\":{\"description\":\"Best available source coordinates. Both locations use 1-based lines and UTF-16 columns; URLs are never shortened for display.\",\"type\":\"object\",\"required\":[\"generated\",\"mapping\",\"resolved\"],\"properties\":{\"breadcrumb\":{\"type\":[\"string\",\"null\"]},\"diagnostic\":{\"type\":[\"string\",\"null\"]},\"generated\":{\"$ref\":\"#/components/schemas/SourceLocation\"},\"mapping\":{\"type\":\"string\"},\"resolved\":{\"$ref\":\"#/components/schemas/SourceLocation\"}},\"additionalProperties\":false},\"ScopeSnapshot\":{\"type\":\"object\",\"required\":[\"index\",\"kind\"],\"properties\":{\"index\":{\"type\":\"integer\"},\"kind\":{\"type\":\"string\"},\"name\":{\"type\":[\"string\",\"null\"]}},\"additionalProperties\":false},\"SourceExcerpt\":{\"type\":\"object\",\"required\":[\"currentLine\",\"highlightLength\",\"highlightStart\",\"lines\",\"sourceUrl\"],\"properties\":{\"breadcrumb\":{\"type\":[\"string\",\"null\"]},\"currentLine\":{\"type\":\"integer\"},\"highlightLength\":{\"type\":\"integer\"},\"highlightStart\":{\"type\":\"integer\"},\"lines\":{\"type\":\"array\",\"items\":{\"$ref\":\"#/components/schemas/SourceExcerptLine\"}},\"sourceUrl\":{\"type\":\"string\"}},\"additionalProperties\":false},\"SourceExcerptLine\":{\"type\":\"object\",\"required\":[\"line\",\"text\"],\"properties\":{\"line\":{\"type\":\"integer\"},\"text\":{\"type\":\"string\"}},\"additionalProperties\":false},\"SourceFormattingMode\":{\"type\":\"string\",\"enum\":[\"off\",\"auto\",\"on\"]},\"SourceFormattingRule\":{\"type\":\"object\",\"required\":[\"id\",\"mode\"],\"properties\":{\"id\":{\"type\":\"string\"},\"mode\":{\"$ref\":\"#/components/schemas/SourceFormattingMode\"},\"targetPattern\":{\"type\":[\"string\",\"null\"]},\"urlPattern\":{\"type\":[\"string\",\"null\"]}},\"additionalProperties\":false},\"SourceFormattingSettings\":{\"type\":\"object\",\"required\":[\"defaultMode\",\"rules\"],\"properties\":{\"defaultMode\":{\"$ref\":\"#/components/schemas/SourceFormattingMode\"},\"rules\":{\"type\":\"array\",\"items\":{\"$ref\":\"#/components/schemas/SourceFormattingRule\"}}},\"additionalProperties\":false},\"SourceLocation\":{\"type\":\"object\",\"required\":[\"column\",\"line\",\"sourceUrl\"],\"properties\":{\"column\":{\"type\":\"integer\"},\"line\":{\"type\":\"integer\"},\"sourceUrl\":{\"type\":\"string\"}},\"additionalProperties\":false},\"StepKind\":{\"type\":\"string\",\"enum\":[\"into\",\"over\",\"out\"]},\"TargetAttachOptions\":{\"type\":\"object\",\"properties\":{\"expectedConnectionGeneration\":{\"type\":[\"integer\",\"null\"],\"format\":\"uint64\"},\"force\":{\"type\":\"boolean\"}},\"additionalProperties\":false},\"TargetAttachmentOutcome\":{\"type\":\"string\",\"enum\":[\"created\",\"stolen\"]},\"TargetAttachmentResult\":{\"type\":\"object\",\"required\":[\"outcome\",\"target\"],\"properties\":{\"outcome\":{\"$ref\":\"#/components/schemas/TargetAttachmentOutcome\"},\"target\":{\"$ref\":\"#/components/schemas/TargetDebuggerSnapshot\"}},\"additionalProperties\":false},\"TargetAttachmentState\":{\"type\":\"string\",\"enum\":[\"detached\",\"external\",\"debugger\"]},\"TargetBreakpointSnapshot\":{\"type\":\"object\",\"required\":[\"column\",\"id\",\"line\",\"sourceUrl\",\"status\"],\"properties\":{\"applications\":{\"type\":\"array\",\"items\":{\"$ref\":\"#/components/schemas/BreakpointApplicationSnapshot\"}},\"assessments\":{\"type\":\"array\",\"items\":{\"$ref\":\"#/components/schemas/BreakpointScriptAssessmentSnapshot\"}},\"column\":{\"type\":\"integer\"},\"id\":{\"type\":\"string\"},\"line\":{\"type\":\"integer\"},\"source\":{\"anyOf\":[{\"$ref\":\"#/components/schemas/SourceExcerpt\"},{\"type\":\"null\"}]},\"sourceUrl\":{\"type\":\"string\"},\"status\":{\"$ref\":\"#/components/schemas/TargetBreakpointStatus\"}},\"additionalProperties\":false},\"TargetBreakpointStatus\":{\"oneOf\":[{\"type\":\"object\",\"required\":[\"kind\"],\"properties\":{\"kind\":{\"type\":\"string\",\"const\":\"waitingForScript\"}},\"additionalProperties\":false},{\"type\":\"object\",\"required\":[\"diagnostics\",\"kind\"],\"properties\":{\"diagnostics\":{\"type\":\"array\",\"items\":{\"type\":\"string\"}},\"kind\":{\"type\":\"string\",\"const\":\"sourceNotFound\"}},\"additionalProperties\":false},{\"type\":\"object\",\"required\":[\"candidates\",\"kind\",\"omitted_candidate_count\"],\"properties\":{\"candidates\":{\"type\":\"array\",\"items\":{\"$ref\":\"#/components/schemas/BreakpointSourceCandidateSnapshot\"}},\"kind\":{\"type\":\"string\",\"const\":\"ambiguousSource\"},\"omitted_candidate_count\":{\"type\":\"integer\"}},\"additionalProperties\":false},{\"type\":\"object\",\"required\":[\"diagnostics\",\"kind\"],\"properties\":{\"diagnostics\":{\"type\":\"array\",\"items\":{\"type\":\"string\"}},\"kind\":{\"type\":\"string\",\"const\":\"unmapped\"}},\"additionalProperties\":false},{\"type\":\"object\",\"required\":[\"kind\",\"mapping_count\"],\"properties\":{\"kind\":{\"type\":\"string\",\"const\":\"applicable\"},\"mapping_count\":{\"type\":\"integer\"}},\"additionalProperties\":false},{\"type\":\"object\",\"required\":[\"application_count\",\"kind\"],\"properties\":{\"application_count\":{\"type\":\"integer\"},\"kind\":{\"type\":\"string\",\"const\":\"installing\"}},\"additionalProperties\":false},{\"type\":\"object\",\"required\":[\"binding_count\",\"kind\"],\"properties\":{\"binding_count\":{\"type\":\"integer\"},\"kind\":{\"type\":\"string\",\"const\":\"installed\"}},\"additionalProperties\":false},{\"type\":\"object\",\"required\":[\"kind\",\"message\"],\"properties\":{\"kind\":{\"type\":\"string\",\"const\":\"failed\"},\"message\":{\"type\":\"string\"}},\"additionalProperties\":false}],\"discriminator\":{\"propertyName\":\"kind\"}},\"TargetDebuggerPhase\":{\"oneOf\":[{\"type\":\"object\",\"required\":[\"kind\"],\"properties\":{\"kind\":{\"type\":\"string\",\"const\":\"running\"}},\"additionalProperties\":false},{\"type\":\"object\",\"required\":[\"epoch\",\"kind\"],\"properties\":{\"epoch\":{\"type\":\"integer\"},\"kind\":{\"type\":\"string\",\"const\":\"paused\"}},\"additionalProperties\":false},{\"type\":\"object\",\"required\":[\"epoch\",\"kind\"],\"properties\":{\"epoch\":{\"type\":\"integer\"},\"kind\":{\"type\":\"string\",\"const\":\"resuming\"}},\"additionalProperties\":false},{\"type\":\"object\",\"required\":[\"kind\",\"message\"],\"properties\":{\"kind\":{\"type\":\"string\",\"const\":\"failed\"},\"message\":{\"type\":\"string\"}},\"additionalProperties\":false}],\"discriminator\":{\"propertyName\":\"kind\"}},\"TargetDebuggerSnapshot\":{\"type\":\"object\",\"required\":[\"breakpoints\",\"connectionGeneration\",\"connectionId\",\"contextId\",\"logs\",\"phase\",\"revision\",\"scripts\",\"targetId\"],\"properties\":{\"breakpoints\":{\"type\":\"array\",\"items\":{\"$ref\":\"#/components/schemas/TargetBreakpointSnapshot\"}},\"connectionGeneration\":{\"type\":\"integer\"},\"connectionId\":{\"type\":\"string\"},\"contextId\":{\"type\":\"string\"},\"logCapture\":{\"$ref\":\"#/components/schemas/LogCaptureSnapshot\"},\"logs\":{\"type\":\"array\",\"items\":{\"$ref\":\"#/components/schemas/ConsoleMessageSnapshot\"}},\"pause\":{\"anyOf\":[{\"$ref\":\"#/components/schemas/PauseSnapshot\"},{\"type\":\"null\"}]},\"phase\":{\"$ref\":\"#/components/schemas/TargetDebuggerPhase\"},\"revision\":{\"type\":\"integer\"},\"scripts\":{\"type\":\"array\",\"items\":{\"$ref\":\"#/components/schemas/TargetScriptSnapshot\"}},\"targetId\":{\"type\":\"string\"}},\"additionalProperties\":false},\"TargetLogSnapshot\":{\"type\":\"object\",\"required\":[\"capture\",\"connectionGeneration\",\"connectionId\",\"contextId\",\"messages\",\"targetId\"],\"properties\":{\"capture\":{\"$ref\":\"#/components/schemas/LogCaptureSnapshot\"},\"connectionGeneration\":{\"type\":\"integer\"},\"connectionId\":{\"type\":\"string\"},\"contextId\":{\"type\":\"string\"},\"messages\":{\"type\":\"array\",\"items\":{\"$ref\":\"#/components/schemas/ConsoleMessageSnapshot\"}},\"targetId\":{\"type\":\"string\"}},\"additionalProperties\":false},\"TargetNodeSnapshot\":{\"type\":\"object\",\"required\":[\"attachment\",\"connectionGeneration\",\"connectionId\",\"target\"],\"properties\":{\"attachment\":{\"$ref\":\"#/components/schemas/TargetAttachmentState\"},\"connectionGeneration\":{\"type\":\"integer\"},\"connectionId\":{\"type\":\"string\"},\"parentTargetId\":{\"type\":[\"string\",\"null\"]},\"target\":{\"$ref\":\"#/components/schemas/TargetSnapshot\"}},\"additionalProperties\":false},\"TargetScriptSnapshot\":{\"type\":\"object\",\"required\":[\"status\",\"url\"],\"properties\":{\"sourceMapUrl\":{\"type\":[\"string\",\"null\"]},\"status\":{\"$ref\":\"#/components/schemas/TargetScriptStatus\"},\"url\":{\"type\":\"string\"}},\"additionalProperties\":false},\"TargetScriptStatus\":{\"oneOf\":[{\"type\":\"object\",\"required\":[\"kind\"],\"properties\":{\"kind\":{\"type\":\"string\",\"const\":\"unresolved\"}},\"additionalProperties\":false},{\"type\":\"object\",\"required\":[\"kind\"],\"properties\":{\"kind\":{\"type\":\"string\",\"const\":\"pending\"}},\"additionalProperties\":false},{\"type\":\"object\",\"required\":[\"authored_sources\",\"kind\"],\"properties\":{\"authored_sources\":{\"type\":\"array\",\"items\":{\"type\":\"string\"}},\"kind\":{\"type\":\"string\",\"const\":\"resolved\"}},\"additionalProperties\":false},{\"type\":\"object\",\"required\":[\"kind\",\"message\"],\"properties\":{\"kind\":{\"type\":\"string\",\"const\":\"failed\"},\"message\":{\"type\":\"string\"}},\"additionalProperties\":false}],\"discriminator\":{\"propertyName\":\"kind\"}},\"TargetSnapshot\":{\"type\":\"object\",\"required\":[\"attached\",\"targetId\",\"targetType\",\"title\",\"url\"],\"properties\":{\"attached\":{\"type\":\"boolean\"},\"browserContextId\":{\"type\":[\"string\",\"null\"]},\"openerId\":{\"type\":[\"string\",\"null\"]},\"parentId\":{\"type\":[\"string\",\"null\"]},\"subtype\":{\"type\":[\"string\",\"null\"]},\"targetId\":{\"type\":\"string\"},\"targetType\":{\"type\":\"string\"},\"title\":{\"type\":\"string\"},\"url\":{\"type\":\"string\"}},\"additionalProperties\":false},\"TargetWaitPredicate\":{\"oneOf\":[{\"type\":\"object\",\"required\":[\"afterRevision\",\"kind\"],\"properties\":{\"afterRevision\":{\"type\":\"integer\"},\"kind\":{\"type\":\"string\",\"const\":\"changed\"}},\"additionalProperties\":false},{\"type\":\"object\",\"required\":[\"kind\"],\"properties\":{\"kind\":{\"type\":\"string\",\"const\":\"running\"}},\"additionalProperties\":false},{\"type\":\"object\",\"required\":[\"breakpointId\",\"kind\"],\"properties\":{\"breakpointId\":{\"type\":\"string\"},\"kind\":{\"type\":\"string\",\"const\":\"breakpointInstalled\"}},\"additionalProperties\":false},{\"type\":\"object\",\"required\":[\"afterEpoch\",\"kind\"],\"properties\":{\"afterEpoch\":{\"type\":\"integer\"},\"kind\":{\"type\":\"string\",\"const\":\"paused\"}},\"additionalProperties\":false}],\"discriminator\":{\"propertyName\":\"kind\"}},\"ValueInspectionOptions\":{\"type\":\"object\",\"required\":[\"maxPreviewLength\",\"maxProperties\",\"retainReferences\"],\"properties\":{\"maxPreviewLength\":{\"type\":\"integer\"},\"maxProperties\":{\"type\":\"integer\"},\"retainReferences\":{\"type\":\"boolean\"}},\"additionalProperties\":false},\"ValuePreviewSnapshot\":{\"type\":\"object\",\"required\":[\"kind\",\"truncated\"],\"properties\":{\"kind\":{\"type\":\"string\"},\"preview\":{\"type\":[\"string\",\"null\"]},\"reference\":{\"type\":[\"string\",\"null\"]},\"source\":{\"$ref\":\"#/components/schemas/ObjectSourceSnapshot\"},\"truncated\":{\"type\":\"boolean\"}},\"additionalProperties\":false},\"ValuePropertySnapshot\":{\"type\":\"object\",\"required\":[\"name\",\"value\"],\"properties\":{\"name\":{\"type\":\"string\"},\"value\":{\"$ref\":\"#/components/schemas/ValuePreviewSnapshot\"}},\"additionalProperties\":false},\"ValueSelector\":{\"oneOf\":[{\"type\":\"object\",\"required\":[\"allowSideEffects\",\"expression\",\"kind\"],\"properties\":{\"allowSideEffects\":{\"type\":\"boolean\"},\"expression\":{\"type\":\"string\"},\"kind\":{\"type\":\"string\",\"const\":\"expression\"}},\"additionalProperties\":false},{\"type\":\"object\",\"required\":[\"kind\",\"object_id\"],\"properties\":{\"kind\":{\"type\":\"string\",\"const\":\"remoteObject\"},\"object_id\":{\"type\":\"string\"}},\"additionalProperties\":false}],\"discriminator\":{\"propertyName\":\"kind\"}},\"ValueSnapshot\":{\"type\":\"object\",\"required\":[\"preview\",\"properties\",\"selector\"],\"properties\":{\"className\":{\"type\":[\"string\",\"null\"]},\"omittedPropertyCount\":{\"type\":\"integer\"},\"preview\":{\"$ref\":\"#/components/schemas/ValuePreviewSnapshot\"},\"promise\":{\"anyOf\":[{\"$ref\":\"#/components/schemas/PromiseSnapshot\"},{\"type\":\"null\"}]},\"properties\":{\"type\":\"array\",\"items\":{\"$ref\":\"#/components/schemas/ValuePropertySnapshot\"}},\"propertiesTruncated\":{\"type\":\"boolean\"},\"selector\":{\"$ref\":\"#/components/schemas/ValueSelector\"},\"subtype\":{\"type\":[\"string\",\"null\"]}},\"additionalProperties\":false},\"VariableSnapshot\":{\"type\":\"object\",\"required\":[\"kind\",\"name\",\"preview\"],\"properties\":{\"description\":{\"type\":[\"string\",\"null\"]},\"kind\":{\"type\":\"string\"},\"name\":{\"type\":\"string\"},\"objectId\":{\"type\":[\"string\",\"null\"]},\"preview\":{\"$ref\":\"#/components/schemas/ValuePreviewSnapshot\"},\"unserializableValue\":{\"type\":[\"string\",\"null\"]},\"value\":true},\"additionalProperties\":false}}}}");

export const TargetDebuggerApi = new InterfaceDefinition(
    {
        id: "dev.dbgjs.target-debugger",
        hash: "a36724cd17fb6c51",
    },
    {
        attach_target: requestType(
            z.object({
                connectionId: z.string(),
                contextId: z.string(),
                options: TargetAttachOptionsSchema,
                targetId: z.string(),
            }),
            TargetAttachmentResultSchema,
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
        get_target: requestType(
            z.object({
                connectionId: z.string(),
                contextId: z.string(),
                targetId: z.string(),
            }),
            TargetDebuggerSnapshotSchema,
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
        release_target: requestType(
            z.object({
                connectionId: z.string(),
                contextId: z.string(),
                targetId: z.string(),
            }),
            TargetDebuggerSnapshotSchema,
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
