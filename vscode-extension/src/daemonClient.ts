import type { JsonValue } from "@vscode/hubrpc";
import { homedir, tmpdir } from "node:os";
import { dirname, join } from "node:path";
import { mkdir, readFile } from "node:fs/promises";
import {
	type BreakpointSpec,
	type ConnectionConfiguration,
	type ContextSnapshot,
	type ContextSummary,
	type EvaluationSnapshot,
	type SourceContentSnapshot,
	type SourceSnapshotInfo,
	type TargetDebuggerSnapshot,
	type VariableSnapshot,
	parseContextSnapshot,
	parseContextSummaries,
	parseEvaluation,
	parseObservationResult,
	parseSourceContent,
	parseSourceInfos,
	parseTargetDebuggerSnapshot,
	parseVariables,
} from "./apiTypes.js";
import { connectLegacyHub, type LegacyHubConnection } from "./legacyHubTransport.js";

const debuggerInterface = "dev.hediet.cdp-debugger";

export class DaemonClient {
	private constructor(
		private readonly hub: LegacyHubConnection,
		public readonly stateFile: string,
	) {}

	public static async connect(
		stateFile: string,
		log?: (message: string) => void,
	): Promise<DaemonClient> {
		const hub = await connectDaemonHub(stateFile, "commands", log);
		return new DaemonClient(hub, stateFile);
	}

	public onClose(listener: () => void): { dispose(): void } {
		return this.hub.onClose(listener);
	}

	public close(): void {
		this.hub.close();
	}

	public async listContexts(cwd?: string): Promise<readonly ContextSummary[]> {
		return parseContextSummaries(await this.call("list_contexts", {
			cwd: cwd ?? null,
		}));
	}

	public async putContext(
		contextId: string,
		kind: "path" | "named",
		displayName: string,
	): Promise<ContextSnapshot> {
		return parseContextSnapshot(await this.call("put_context", {
			contextId,
			kind,
			displayName,
		}));
	}

	public async getContext(contextId: string): Promise<ContextSnapshot> {
		return parseContextSnapshot(await this.call("get_context", { contextId }));
	}

	public async putConnection(
		contextId: string,
		connectionId: string,
		configuration: ConnectionConfiguration,
	): Promise<ContextSnapshot> {
		return parseContextSnapshot(await this.call("put_connection", {
			contextId,
			connectionId,
			configuration: connectionConfigurationJson(configuration),
		}));
	}

	public async connectConnection(
		contextId: string,
		connectionId: string,
	): Promise<ContextSnapshot> {
		return parseContextSnapshot(await this.call("connect_connection", {
			contextId,
			connectionId,
		}));
	}

	public async disconnectConnection(
		contextId: string,
		connectionId: string,
	): Promise<ContextSnapshot> {
		return parseContextSnapshot(await this.call("disconnect_connection", {
			contextId,
			connectionId,
		}));
	}

	public async deleteConnection(
		contextId: string,
		connectionId: string,
		requestId: string,
	): Promise<ContextSnapshot> {
		return parseContextSnapshot(await this.call("delete_connection", {
			contextId,
			connectionId,
			options: { requestId },
		}));
	}

	public async observeContext(
		contextId: string,
		revision: number | undefined,
		timeoutMs: number,
	): Promise<ContextSnapshot | undefined> {
		const cursor = revision === undefined
			? { kind: "current" }
			: { kind: "after", revision };
		return parseObservationResult(await this.call("observe_context", {
			contextId,
			cursor,
			timeoutMs,
		})).snapshot;
	}

	public async getTarget(
		contextId: string,
		connectionId: string,
		targetId: string,
	): Promise<TargetDebuggerSnapshot> {
		return parseTargetDebuggerSnapshot(await this.call("get_target", {
			contextId,
			connectionId,
			targetId,
		}));
	}

	public async attachTarget(
		contextId: string,
		connectionId: string,
		targetId: string,
	): Promise<TargetDebuggerSnapshot> {
		return parseTargetDebuggerSnapshot(await this.call("attach_target", {
			contextId,
			connectionId,
			targetId,
		}));
	}

	public async waitTarget(
		contextId: string,
		connectionId: string,
		targetId: string,
		predicate:
			| { readonly kind: "running"; }
			| { readonly kind: "paused"; readonly afterEpoch: number; },
		timeoutMs: number,
	): Promise<TargetDebuggerSnapshot> {
		return parseTargetDebuggerSnapshot(await this.call("wait_target", {
			contextId,
			connectionId,
			targetId,
			predicate,
			timeoutMs,
		}));
	}

	public async observeTarget(
		contextId: string,
		connectionId: string,
		targetId: string,
		afterRevision: number,
		timeoutMs: number,
	): Promise<TargetDebuggerSnapshot | undefined> {
		const result = await this.call("observe_target", {
			contextId,
			connectionId,
			targetId,
			afterRevision,
			timeoutMs,
		});
		return result === null ? undefined : parseTargetDebuggerSnapshot(result);
	}

	public async releaseTarget(
		contextId: string,
		connectionId: string,
		targetId: string,
	): Promise<TargetDebuggerSnapshot> {
		return parseTargetDebuggerSnapshot(await this.call("release_target", {
			contextId,
			connectionId,
			targetId,
		}));
	}

	public async resumeTarget(
		contextId: string,
		connectionId: string,
		targetId: string,
		pauseEpoch: number,
	): Promise<TargetDebuggerSnapshot> {
		return parseTargetDebuggerSnapshot(await this.call("resume_target", {
			contextId,
			connectionId,
			targetId,
			pauseEpoch,
		}));
	}

	public async stepTarget(
		contextId: string,
		connectionId: string,
		targetId: string,
		pauseEpoch: number,
		kind: "into" | "over" | "out",
	): Promise<TargetDebuggerSnapshot> {
		return parseTargetDebuggerSnapshot(await this.call("step_target", {
			contextId,
			connectionId,
			targetId,
			pauseEpoch,
			kind,
		}));
	}

	public async evaluateTarget(
		contextId: string,
		connectionId: string,
		targetId: string,
		pauseEpoch: number | undefined,
		frameIndex: number,
		expression: string,
	): Promise<EvaluationSnapshot> {
		return parseEvaluation(await this.call("evaluate_target", {
			contextId,
			connectionId,
			targetId,
			pauseEpoch: pauseEpoch ?? null,
			frameIndex,
			expression,
		}));
	}

	public async getScopeVariables(
		contextId: string,
		connectionId: string,
		targetId: string,
		pauseEpoch: number,
		frameIndex: number,
		scopeIndex: number,
	): Promise<readonly VariableSnapshot[]> {
		return parseVariables(await this.call("get_scope_variables", {
			contextId,
			connectionId,
			targetId,
			pauseEpoch,
			frameIndex,
			scopeIndex,
		}));
	}

	public async getObjectProperties(
		contextId: string,
		connectionId: string,
		targetId: string,
		pauseEpoch: number | undefined,
		objectId: string,
	): Promise<readonly VariableSnapshot[]> {
		return parseVariables(await this.call("get_object_properties", {
			contextId,
			connectionId,
			targetId,
			pauseEpoch: pauseEpoch ?? null,
			objectId,
		}));
	}

	public async putBreakpoint(
		contextId: string,
		breakpointId: string,
		specification: BreakpointSpec,
		requestId: string,
	): Promise<ContextSnapshot> {
		return parseContextSnapshot(await this.call("put_breakpoint_spec", {
			contextId,
			breakpointId,
			specification: {
				sourcePath: specification.sourcePath,
				line: specification.line,
				column: specification.column,
				enabled: specification.enabled,
				...(specification.condition === undefined
					? {}
					: { condition: specification.condition }),
				...(specification.targetSelector === undefined
					? {}
					: { targetSelector: specification.targetSelector }),
			},
			options: { requestId },
		}));
	}

	public async deleteBreakpoint(
		contextId: string,
		breakpointId: string,
		requestId: string,
	): Promise<ContextSnapshot> {
		return parseContextSnapshot(await this.call("delete_breakpoint", {
			contextId,
			breakpointId,
			options: { requestId },
		}));
	}

	public async listSources(contextId: string): Promise<readonly SourceSnapshotInfo[]> {
		return parseSourceInfos(await this.call("list_sources", {
			contextId,
			path: null,
		}));
	}

	public async showSource(contextId: string, path: string): Promise<SourceContentSnapshot> {
		return parseSourceContent(await this.call("show_source", { contextId, path }));
	}

	private async call(member: string, params: JsonValue): Promise<JsonValue> {
		return this.hub.connection.channel.sendRequest(
			`${debuggerInterface}::${member}`,
			params,
		);
	}
}

function connectionConfigurationJson(
	configuration: ConnectionConfiguration,
): Record<string, JsonValue> {
	switch (configuration.kind) {
		case "directCdp":
		case "nodeInspector":
		case "process":
			return configuration;
		case "processTree":
			return configuration;
		case "playwright":
			return {
				kind: configuration.kind,
				url: configuration.url,
				playwrightPackage: configuration.playwrightPackage ?? null,
				channel: configuration.channel,
				headless: configuration.headless,
				ignoreHttpsErrors: configuration.ignoreHttpsErrors,
			};
		case "chrome":
			return {
				kind: configuration.kind,
				url: configuration.url,
				executable: configuration.executable,
				headless: configuration.headless,
				userDataDir: configuration.userDataDir ?? null,
				args: [...configuration.args],
			};
		case "node":
			return {
				kind: configuration.kind,
				program: configuration.program,
				args: [...configuration.args],
				cwd: configuration.cwd,
				runtimeExecutable: configuration.runtimeExecutable,
				runtimeArgs: [...configuration.runtimeArgs],
				env: { ...configuration.env },
			};
	}
}

export function defaultServiceStateFile(environment: NodeJS.ProcessEnv = process.env): string {
	if (environment.JSDBG_SERVICE_STATE !== undefined) {
		return environment.JSDBG_SERVICE_STATE;
	}
	if (environment.LOCALAPPDATA !== undefined) {
		return join(environment.LOCALAPPDATA, "hediet", "cdp-client", "service.json");
	}
	if (environment.XDG_RUNTIME_DIR !== undefined) {
		return join(environment.XDG_RUNTIME_DIR, "hediet-cdp-client", "service.json");
	}
	if (environment.HOME !== undefined) {
		return join(environment.HOME, ".cache", "hediet", "cdp-client", "service.json");
	}
	return join(tmpdir(), `hediet-cdp-client-${process.pid}`, "service.json");
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
): Promise<LegacyHubConnection> {
	const scopedLog = log === undefined
		? undefined
		: (message: string): void => log(`[${purpose}] ${message}`);
	scopedLog?.(`Reading daemon endpoint from ${stateFile}`);
	const endpoint = parseEndpointFile(
		JSON.parse(await readFile(stateFile, "utf8")) as unknown,
	);
	return connectLegacyHub(endpoint.address, endpoint.token, scopedLog);
}

export function parseEndpointFile(value: unknown): EndpointFile {
	if (typeof value !== "object" || value === null || Array.isArray(value)) {
		throw new Error("Invalid jsdbg service endpoint file");
	}
	const endpoint = value as Record<string, unknown>;
	const token = endpoint.token;
	const transport = endpoint.transport;
	if (typeof token !== "string"
		|| typeof transport !== "object"
		|| transport === null
		|| Array.isArray(transport)) {
		throw new Error("Invalid jsdbg service endpoint");
	}
	const transportRecord = transport as Record<string, unknown>;
	const address = transportRecord.path
		?? transportRecord.pipeName
		?? transportRecord.pipe_name;
	if (typeof address !== "string") {
		throw new Error("Unsupported jsdbg service transport");
	}
	return { address, token };
}
