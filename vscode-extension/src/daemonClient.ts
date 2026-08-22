import type { JsonValue } from "@vscode/hubrpc";
import { homedir, tmpdir } from "node:os";
import { dirname, join } from "node:path";
import { mkdir, readFile } from "node:fs/promises";
import {
	type BreakpointSpec,
	type ContextSnapshot,
	type ContextSummary,
	type EvaluationSnapshot,
	type SourceContentSnapshot,
	type SourceSnapshotInfo,
	type TargetDebuggerSnapshot,
	parseContextSnapshot,
	parseContextSummaries,
	parseEvaluation,
	parseObservationResult,
	parseSourceContent,
	parseSourceInfos,
	parseTargetDebuggerSnapshot,
} from "./apiTypes.js";
import { connectLegacyHub, type LegacyHubConnection } from "./legacyHubTransport.js";

const debuggerInterface = "dev.hediet.cdp-debugger";

export class DaemonClient {
	private constructor(
		private readonly hub: LegacyHubConnection,
		public readonly stateFile: string,
	) {}

	public static async connect(stateFile: string): Promise<DaemonClient> {
		const endpoint = parseEndpointFile(
			JSON.parse(await readFile(stateFile, "utf8")) as unknown,
		);
		const hub = await connectLegacyHub(endpoint.address, endpoint.token);
		return new DaemonClient(hub, stateFile);
	}

	public onClose(listener: () => void): { dispose(): void } {
		return this.hub.onClose(listener);
	}

	public close(): void {
		this.hub.close();
	}

	public async listContexts(): Promise<readonly ContextSummary[]> {
		return parseContextSummaries(await this.call("list_contexts", {}));
	}

	public async putContext(contextId: string, displayName: string): Promise<ContextSnapshot> {
		return parseContextSnapshot(await this.call("put_context", {
			contextId,
			displayName,
		}));
	}

	public async getContext(contextId: string): Promise<ContextSnapshot> {
		return parseContextSnapshot(await this.call("get_context", { contextId }));
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

function parseEndpointFile(value: unknown): EndpointFile {
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
	const address = transportRecord.path ?? transportRecord.pipeName;
	if (typeof address !== "string") {
		throw new Error("Unsupported jsdbg service transport");
	}
	return { address, token };
}
