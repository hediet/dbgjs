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

const ServiceInfoSchema = z.object({
    agentInstanceId: z.string(),
    processId: z.int(),
});

const wireSchema: LinkRpcInterfaceSchema = JSON.parse("{\"id\":\"dev.dbgjs.cdp-debugger\",\"hash\":\"95367afe5f9ccd83\",\"methods\":{\"discover_vscode_process_trees\":{\"params\":{\"type\":\"object\",\"properties\":{},\"additionalProperties\":false},\"result\":{\"type\":\"array\",\"items\":{\"$ref\":\"#/components/schemas/ProcessTreeSnapshot\"}}},\"get_process_projection\":{\"params\":{\"type\":\"object\",\"required\":[\"contextId\",\"expandedRootProcessIds\"],\"properties\":{\"contextId\":{\"type\":\"string\"},\"expandedRootProcessIds\":{\"type\":\"array\",\"items\":{\"type\":\"integer\"}}},\"additionalProperties\":false},\"result\":{\"type\":\"array\",\"items\":{\"$ref\":\"#/components/schemas/ProcessTreeSnapshot\"}},\"description\":\"Returns a process-oriented resource projection. Runtime target discovery is performed only\\nfor the roots named in `expanded_root_process_ids`.\"},\"service_info\":{\"params\":{\"type\":\"object\",\"properties\":{},\"additionalProperties\":false},\"result\":{\"$ref\":\"#/components/schemas/ServiceInfo\"}},\"shutdown\":{\"params\":{\"type\":\"object\",\"properties\":{},\"additionalProperties\":false},\"result\":{\"type\":\"boolean\"}}},\"components\":{\"schemas\":{\"AgentSessionSnapshot\":{\"type\":\"object\",\"required\":[\"internalId\",\"workingDirectories\"],\"properties\":{\"chatUri\":{\"type\":[\"string\",\"null\"]},\"disconnected\":{\"type\":[\"boolean\",\"null\"]},\"internalId\":{\"type\":\"string\"},\"title\":{\"type\":[\"string\",\"null\"]},\"workingDirectories\":{\"type\":\"array\",\"items\":{\"type\":\"string\"}}},\"additionalProperties\":false},\"ProcessRole\":{\"type\":\"string\",\"enum\":[\"vscode-main\",\"electron-main\",\"browser-main\",\"renderer\",\"extension-host\",\"node-utility\",\"node\",\"type-script-server\",\"type-script-installer\",\"language-server\",\"pty-host\",\"file-watcher\",\"agent-host\",\"copilot\",\"claude\",\"codex\",\"agent\",\"gpu\",\"network-service\",\"audio-service\",\"crashpad\",\"utility\",\"other\"]},\"ProcessRootKind\":{\"type\":\"string\",\"enum\":[\"vscode\",\"node\",\"electron\",\"browser\"]},\"ProcessSnapshot\":{\"type\":\"object\",\"required\":[\"commandLine\",\"creationDate\",\"name\",\"processId\",\"role\"],\"properties\":{\"agentSessions\":{\"type\":\"array\",\"items\":{\"$ref\":\"#/components/schemas/AgentSessionSnapshot\"}},\"attachable\":{\"type\":\"boolean\"},\"commandLine\":{\"type\":\"string\"},\"cpuPercent\":{\"type\":[\"integer\",\"null\"],\"format\":\"uint32\"},\"creationDate\":{\"type\":\"string\"},\"debugTargetId\":{\"type\":[\"string\",\"null\"]},\"displayName\":{\"type\":[\"string\",\"null\"]},\"memoryBytes\":{\"type\":[\"integer\",\"null\"],\"format\":\"uint64\"},\"name\":{\"type\":\"string\"},\"parentProcessId\":{\"type\":[\"integer\",\"null\"],\"format\":\"uint32\"},\"processId\":{\"type\":\"integer\"},\"role\":{\"$ref\":\"#/components/schemas/ProcessRole\"},\"windowId\":{\"type\":[\"integer\",\"null\"],\"format\":\"uint32\"},\"windowTitle\":{\"type\":[\"string\",\"null\"]}},\"additionalProperties\":false},\"ProcessTargetSnapshot\":{\"type\":\"object\",\"required\":[\"attached\",\"targetId\",\"targetType\",\"title\",\"url\"],\"properties\":{\"attached\":{\"type\":\"boolean\"},\"browserContextId\":{\"type\":[\"string\",\"null\"]},\"openerId\":{\"type\":[\"string\",\"null\"]},\"parentId\":{\"type\":[\"string\",\"null\"]},\"processId\":{\"type\":[\"integer\",\"null\"],\"format\":\"uint32\"},\"subtype\":{\"type\":[\"string\",\"null\"]},\"targetId\":{\"type\":\"string\"},\"targetType\":{\"type\":\"string\"},\"title\":{\"type\":\"string\"},\"url\":{\"type\":\"string\"}},\"additionalProperties\":false},\"ProcessTreeSnapshot\":{\"type\":\"object\",\"required\":[\"processes\",\"rootProcessId\",\"runtimeMetadataAvailable\"],\"properties\":{\"processes\":{\"type\":\"array\",\"items\":{\"$ref\":\"#/components/schemas/ProcessSnapshot\"}},\"rootKind\":{\"$ref\":\"#/components/schemas/ProcessRootKind\"},\"rootProcessId\":{\"type\":\"integer\"},\"runtimeMetadataAvailable\":{\"type\":\"boolean\"},\"targetDiscoveryError\":{\"type\":[\"string\",\"null\"]},\"targets\":{\"type\":\"array\",\"items\":{\"$ref\":\"#/components/schemas/ProcessTargetSnapshot\"}},\"targetsObserved\":{\"description\":\"Whether runtime children were queried for this root. An empty `targets` collection is only authoritative when this is true.\",\"type\":\"boolean\"}},\"additionalProperties\":false},\"ServiceInfo\":{\"type\":\"object\",\"required\":[\"agentInstanceId\",\"processId\"],\"properties\":{\"agentInstanceId\":{\"type\":\"string\"},\"processId\":{\"type\":\"integer\"}},\"additionalProperties\":false}}}}");

export const ServiceApi = new InterfaceDefinition(
    {
        id: "dev.dbgjs.cdp-debugger",
        hash: "95367afe5f9ccd83",
    },
    {
        discover_vscode_process_trees: requestType(
            z.object({}),
            z.array(ProcessTreeSnapshotSchema),
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
        service_info: requestType(
            z.object({}),
            ServiceInfoSchema,
        ),
        shutdown: requestType(
            z.object({}),
            z.boolean(),
        ),
    },
    { frozenSchema: wireSchema },
);
