import { homedir, tmpdir } from "node:os";
import { dirname, join } from "node:path";
import { mkdir, readFile } from "node:fs/promises";
import {
	type BreakpointSpec,
	type ConnectionConfiguration,
	type ContextSnapshot,
	type ContextSummary,
	type ContextKind,
	type ObservationCursor,
	type EvaluationSnapshot,
	type SourceContentSnapshot,
	type SourceSnapshotInfo,
	type TargetDebuggerSnapshot,
	type TargetWaitPredicate,
	type StepKind,
	type VariableSnapshot,
	observationSnapshot,
} from "./apiTypes.js";
import { connectDaemon, type DaemonConnection } from "./daemonTransport.js";
import { DbgServiceClient } from "./dbgServiceClient.js";

export class DaemonClient {
	private constructor(
		private readonly hub: DaemonConnection,
		private readonly _service: DbgServiceClient,
		public readonly stateFile: string,
	) {}

	public static async connect(
		stateFile: string,
		log?: (message: string) => void,
	): Promise<DaemonClient> {
		const hub = await connectDaemonHub(stateFile, "commands", log);
		return new DaemonClient(hub, new DbgServiceClient(hub.connection), stateFile);
	}

	public onClose(listener: () => void): { dispose(): void } {
		return this.hub.onClose(listener);
	}

	public close(): void {
		this.hub.close();
	}

	public async listContexts(cwd?: string): Promise<readonly ContextSummary[]> {
		return this._service.contexts.list_contexts({
			cwd: cwd ?? null,
		});
	}

	public async putContext(
		contextId: string,
		kind: ContextKind,
		displayName: string,
	): Promise<ContextSnapshot> {
		return this._service.contexts.put_context({
			contextId,
			kind,
			displayName,
		});
	}

	public async getContext(contextId: string): Promise<ContextSnapshot> {
		return this._service.contexts.get_context({ contextId });
	}

	public async putConnection(
		contextId: string,
		connectionId: string,
		configuration: ConnectionConfiguration,
	): Promise<ContextSnapshot> {
		return this._service.contexts.put_connection({
			connectionRef: { contextId, connectionId },
			configuration,
		});
	}

	public async connectConnection(
		contextId: string,
		connectionId: string,
	): Promise<ContextSnapshot> {
		return this._service.contexts.connect_connection({
			connectionRef: { contextId, connectionId },
		});
	}

	public async disconnectConnection(
		contextId: string,
		connectionId: string,
	): Promise<ContextSnapshot> {
		return this._service.contexts.disconnect_connection({
			connectionRef: { contextId, connectionId },
		});
	}

	public async deleteConnection(
		contextId: string,
		connectionId: string,
		requestId: string,
	): Promise<ContextSnapshot> {
		return this._service.contexts.delete_connection({
			connectionRef: { contextId, connectionId },
			options: { expectedRevision: null, requestId },
		});
	}

	public async observeContext(
		contextId: string,
		revision: number | undefined,
		timeoutMs: number,
	): Promise<ContextSnapshot | undefined> {
		const cursor: ObservationCursor = revision === undefined
			? { kind: "current" }
			: { kind: "after", revision };
		return observationSnapshot(await this._service.contexts.observe_context({
			contextId,
			cursor,
			timeoutMs,
		}));
	}

	public async getTarget(
		contextId: string,
		connectionId: string,
		targetId: string,
	): Promise<TargetDebuggerSnapshot> {
		return this._service.targets.get_target({
			targetRef: { connection: { contextId, connectionId }, targetId },
		});
	}

	public async attachTarget(
		contextId: string,
		connectionId: string,
		targetId: string,
		expectedConnectionGeneration: number,
	): Promise<TargetDebuggerSnapshot> {
		const result = await this._service.targets.attach_target({
			targetRef: { connection: { contextId, connectionId }, targetId },
			options: {
				force: false,
				expectedConnectionGeneration,
			},
		});
		return result.target;
	}

	public async waitTarget(
		contextId: string,
		connectionId: string,
		targetId: string,
		predicate: TargetWaitPredicate,
		timeoutMs: number,
	): Promise<TargetDebuggerSnapshot> {
		return this._service.targets.wait_target({
			targetRef: { connection: { contextId, connectionId }, targetId },
			predicate,
			timeoutMs,
		});
	}

	public async observeTarget(
		contextId: string,
		connectionId: string,
		targetId: string,
		afterRevision: number,
		timeoutMs: number,
	): Promise<TargetDebuggerSnapshot | undefined> {
		const result = await this._service.targets.observe_target({
			targetRef: { connection: { contextId, connectionId }, targetId },
			afterRevision,
			timeoutMs,
		});
		return result ?? undefined;
	}

	public async releaseTarget(
		contextId: string,
		connectionId: string,
		targetId: string,
	): Promise<TargetDebuggerSnapshot> {
		return this._service.targets.release_target({
			targetRef: { connection: { contextId, connectionId }, targetId },
		});
	}

	public async resumeTarget(
		contextId: string,
		connectionId: string,
		targetId: string,
		pauseEpoch: number,
	): Promise<TargetDebuggerSnapshot> {
		return this._service.targets.resume_target({
			targetRef: { connection: { contextId, connectionId }, targetId },
			pauseEpoch,
		});
	}

	public async stepTarget(
		contextId: string,
		connectionId: string,
		targetId: string,
		pauseEpoch: number,
		kind: StepKind,
	): Promise<TargetDebuggerSnapshot> {
		return this._service.targets.step_target({
			targetRef: { connection: { contextId, connectionId }, targetId },
			pauseEpoch,
			kind,
		});
	}

	public async evaluateTarget(
		contextId: string,
		connectionId: string,
		targetId: string,
		pauseEpoch: number | undefined,
		frameIndex: number,
		expression: string,
	): Promise<EvaluationSnapshot> {
		return this._service.targets.evaluate_target({
			targetRef: { connection: { contextId, connectionId }, targetId },
			pauseEpoch: pauseEpoch ?? null,
			frameIndex,
			expression,
		});
	}

	public async getScopeVariables(
		contextId: string,
		connectionId: string,
		targetId: string,
		pauseEpoch: number,
		frameIndex: number,
		scopeIndex: number,
	): Promise<readonly VariableSnapshot[]> {
		return this._service.targets.get_scope_variables({
			targetRef: { connection: { contextId, connectionId }, targetId },
			pauseEpoch,
			frameIndex,
			scopeIndex,
		});
	}

	public async getObjectProperties(
		contextId: string,
		connectionId: string,
		targetId: string,
		pauseEpoch: number | undefined,
		objectId: string,
	): Promise<readonly VariableSnapshot[]> {
		return this._service.targets.get_object_properties({
			targetRef: { connection: { contextId, connectionId }, targetId },
			pauseEpoch: pauseEpoch ?? null,
			objectId,
		});
	}

	public async putBreakpoint(
		contextId: string,
		breakpointId: string,
		specification: BreakpointSpec,
		requestId: string,
	): Promise<ContextSnapshot> {
		return this._service.contexts.put_breakpoint_spec({
			contextId,
			breakpointId,
			specification,
			options: { expectedRevision: null, requestId },
		});
	}

	public async deleteBreakpoint(
		contextId: string,
		breakpointId: string,
		requestId: string,
	): Promise<ContextSnapshot> {
		return this._service.contexts.delete_breakpoint({
			contextId,
			breakpointId,
			options: { expectedRevision: null, requestId },
		});
	}

	public async listSources(contextId: string): Promise<readonly SourceSnapshotInfo[]> {
		return this._service.sources.list_sources({
			contextId,
			path: null,
		});
	}

	public async showSource(contextId: string, path: string): Promise<SourceContentSnapshot> {
		return this._service.sources.show_source({
			contextId,
			path,
			options: {
				line: null,
				contextLines: 0,
				view: "policy",
			},
		});
	}
}

export function defaultServiceStateFile(environment: NodeJS.ProcessEnv = process.env): string {
	if (environment.DBGJS_SERVICE_STATE !== undefined) {
		return environment.DBGJS_SERVICE_STATE;
	}
	if (environment.LOCALAPPDATA !== undefined) {
		return join(environment.LOCALAPPDATA, "dbgjs", "service.json");
	}
	if (environment.XDG_RUNTIME_DIR !== undefined) {
		return join(environment.XDG_RUNTIME_DIR, "dbgjs", "service.json");
	}
	if (environment.HOME !== undefined) {
		return join(environment.HOME, ".cache", "dbgjs", "service.json");
	}
	return join(tmpdir(), `dbgjs-${process.pid}`, "service.json");
}

export async function ensureStateDirectory(stateFile: string): Promise<void> {
	await mkdir(dirname(stateFile), { recursive: true, mode: 0o700 });
}

interface EndpointFile {
	readonly address: string;
	readonly token: string;
}

async function connectDaemonHub(
	stateFile: string,
	purpose: string,
	log?: (message: string) => void,
): Promise<DaemonConnection> {
	const scopedLog = log === undefined
		? undefined
		: (message: string): void => log(`[${purpose}] ${message}`);
	scopedLog?.(`Reading daemon endpoint from ${stateFile}`);
	const endpoint = parseEndpointFile(
		JSON.parse(await readFile(stateFile, "utf8")) as unknown,
	);
	return connectDaemon(endpoint.address, endpoint.token, scopedLog);
}

export function parseEndpointFile(value: unknown): EndpointFile {
	if (typeof value !== "object" || value === null || Array.isArray(value)) {
		throw new Error("Invalid dbgjs service endpoint file");
	}
	const endpoint = value as Record<string, unknown>;
	const token = endpoint.token;
	const transport = endpoint.transport;
	if (typeof token !== "string"
		|| typeof transport !== "object"
		|| transport === null
		|| Array.isArray(transport)) {
		throw new Error("Invalid dbgjs service endpoint");
	}
	const transportRecord = transport as Record<string, unknown>;
	const address = transportRecord.path
		?? transportRecord.pipeName
		?? transportRecord.pipe_name;
	if (typeof address !== "string") {
		throw new Error("Unsupported dbgjs service transport");
	}
	return { address, token };
}
