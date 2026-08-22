import type { DebugProtocol } from "@vscode/debugprotocol";
import { createHash } from "node:crypto";
import * as vscode from "vscode";
import type {
	BreakpointSnapshot,
	ContextSnapshot,
	FrameSnapshot,
	TargetDebuggerSnapshot,
	TargetSnapshot,
} from "./apiTypes.js";
import { targetKey } from "./model.js";
import { resolveLaunch } from "./launchConfig.js";
import { SourceRegistry } from "./sourceRegistry.js";
import type { WorkspaceContextController } from "./workspaceContext.js";

interface TargetBinding {
	readonly connectionId: string;
	readonly target: TargetSnapshot;
}

interface FrameBinding {
	readonly target: TargetBinding;
	readonly frame: FrameSnapshot;
	readonly pauseEpoch: number;
}

export class JsdbgDebugAdapter implements vscode.DebugAdapter, vscode.Disposable {
	private sequence = 1;
	private nextThreadId = 1;
	private nextFrameId = 1;
	private supportsInvalidatedEvent = false;
	private readonly messageEmitter = new vscode.EventEmitter<DebugProtocol.ProtocolMessage>();
	private readonly targetKeyToThread = new Map<string, number>();
	private readonly threadToTarget = new Map<number, TargetBinding>();
	private readonly frameBindings = new Map<number, FrameBinding>();
	private readonly breakpointIdsBySource = new Map<string, Set<string>>();
	private readonly targetObservers = new Map<string, { cancelled: boolean }>();
	private readonly sourceRegistry: SourceRegistry;
	private readonly subscriptions: vscode.Disposable[];
	private sourcePaths = new Set<string>();
	private disposed = false;
	private ownedConnectionId: string | undefined;
	private launchTask: Promise<void> | undefined;
	private cleanupTask: Promise<void> | undefined;

	public readonly onDidSendMessage = this.messageEmitter.event;

	public constructor(
		private readonly controller: WorkspaceContextController,
		private readonly debugSessionId: string,
	) {
		this.sourceRegistry = new SourceRegistry(controller);
		this.subscriptions = [
			controller.onDidChangeSnapshot((snapshot) => {
				if (snapshot !== undefined) {
					this.acceptSnapshot(snapshot);
				}
			}),
		];
		if (controller.snapshot !== undefined) {
			this.acceptSnapshot(controller.snapshot);
		}
	}

	public handleMessage(message: DebugProtocol.ProtocolMessage): void {
		if (message.type !== "request") {
			return;
		}
		void this.dispatch(message as DebugProtocol.Request).catch((error: unknown) => {
			this.sendError(message as DebugProtocol.Request, error);
		});
	}

	public dispose(): void {
		this.disposed = true;
		void this.cleanupLaunch(0).catch(() => undefined);
		for (const observer of this.targetObservers.values()) {
			observer.cancelled = true;
		}
		this.targetObservers.clear();
		for (const subscription of this.subscriptions) {
			subscription.dispose();
		}
		this.messageEmitter.dispose();
	}

	private async dispatch(request: DebugProtocol.Request): Promise<void> {
		switch (request.command) {
			case "initialize": {
				const initialize = request as DebugProtocol.InitializeRequest;
				this.supportsInvalidatedEvent =
					initialize.arguments.supportsInvalidatedEvent === true;
				this.sendResponse(request, {
					supportsConfigurationDoneRequest: true,
					supportsEvaluateForHovers: true,
					supportsLoadedSourcesRequest: true,
					supportsSetVariable: false,
				} satisfies DebugProtocol.Capabilities);
				return;
			}
			case "launch":
			case "attach":
				await this.controller.ensureReady();
				try {
					await this.startLaunch(request as DebugProtocol.LaunchRequest);
				} catch (error) {
					await this.cleanupLaunch(request.seq);
					throw error;
				}
				this.sendResponse(request);
				this.sendEvent("initialized");
				await this.refreshSources();
				return;
			case "configurationDone":
				this.sendResponse(request);
				return;
			case "threads":
				this.handleThreads(request);
				return;
			case "stackTrace":
				await this.handleStackTrace(request as DebugProtocol.StackTraceRequest);
				return;
			case "scopes":
				this.sendResponse(request, { scopes: [] } satisfies DebugProtocol.ScopesResponse["body"]);
				return;
			case "variables":
				this.sendResponse(request, { variables: [] } satisfies DebugProtocol.VariablesResponse["body"]);
				return;
			case "source":
				await this.handleSource(request as DebugProtocol.SourceRequest);
				return;
			case "loadedSources":
				await this.handleLoadedSources(request);
				return;
			case "setBreakpoints":
				await this.handleSetBreakpoints(request as DebugProtocol.SetBreakpointsRequest);
				return;
			case "continue":
				await this.handleContinue(request as DebugProtocol.ContinueRequest);
				return;
			case "next":
				await this.handleStep(request as DebugProtocol.NextRequest, "over");
				return;
			case "stepIn":
				await this.handleStep(request as DebugProtocol.StepInRequest, "into");
				return;
			case "stepOut":
				await this.handleStep(request as DebugProtocol.StepOutRequest, "out");
				return;
			case "evaluate":
				await this.handleEvaluate(request as DebugProtocol.EvaluateRequest);
				return;
			case "disconnect":
			case "terminate":
				await this.cleanupLaunch(request.seq);
				this.sendResponse(request);
				this.sendEvent("terminated");
				return;
			default:
				this.sendError(request, new Error(`DAP request '${request.command}' is not implemented`));
		}
	}

	private async startLaunch(request: DebugProtocol.LaunchRequest): Promise<void> {
		if (this.launchTask !== undefined) {
			throw new Error("A jsdbg runtime launch is already in progress");
		}
		const task = this.configureLaunch(request);
		this.launchTask = task;
		try {
			await task;
		} finally {
			if (this.launchTask === task) {
				this.launchTask = undefined;
			}
		}
	}

	private async configureLaunch(request: DebugProtocol.LaunchRequest): Promise<void> {
		const launch = await resolveLaunch(request.arguments, this.debugSessionId);
		if (this.disposed) {
			throw new Error("jsdbg debug session was disposed during launch");
		}
		if (launch.configuration === undefined) {
			return;
		}
		this.ownedConnectionId = launch.connectionId;
		let snapshot = await this.controller.client.putConnection(
			this.controller.contextId,
			launch.connectionId,
			launch.configuration,
		);
		this.controller.adoptSnapshot(snapshot);
		if (this.disposed) {
			throw new Error("jsdbg debug session was disposed during launch");
		}
		snapshot = await this.controller.client.connectConnection(
			this.controller.contextId,
			launch.connectionId,
		);
		this.controller.adoptSnapshot(snapshot);
		if (this.disposed) {
			throw new Error("jsdbg debug session was disposed during launch");
		}
		const connection = snapshot.connections.find(
			(candidate) => candidate.id === launch.connectionId,
		);
		if (connection?.status.kind === "failed") {
			throw new Error(
				typeof connection.status.message === "string"
					? connection.status.message
					: `jsdbg connection '${launch.connectionId}' failed`,
			);
		}
		await this.waitForAttachedTarget(launch.connectionId);
	}

	private async waitForAttachedTarget(connectionId: string): Promise<void> {
		const deadline = Date.now() + 30_000;
		while (Date.now() < deadline) {
			await this.controller.refresh();
			const connection = this.controller.snapshot?.connections.find(
				(candidate) => candidate.id === connectionId,
			);
			if (connection?.targets.some((target) => target.attached)) {
				return;
			}
			if (connection?.status.kind === "failed") {
				throw new Error(
					typeof connection.status.message === "string"
						? connection.status.message
						: `jsdbg connection '${connectionId}' failed`,
				);
			}
			await new Promise((resolve) => setTimeout(resolve, 50));
		}
		throw new Error(`Timed out waiting for jsdbg connection '${connectionId}' to attach a target`);
	}

	private async cleanupLaunch(requestSequence: number): Promise<void> {
		if (this.cleanupTask !== undefined) {
			return this.cleanupTask;
		}
		const task = this.runCleanup(requestSequence);
		this.cleanupTask = task;
		try {
			await task;
		} finally {
			if (this.cleanupTask === task) {
				this.cleanupTask = undefined;
			}
		}
	}

	private async runCleanup(requestSequence: number): Promise<void> {
		await this.launchTask?.catch(() => undefined);
		const connectionId = this.ownedConnectionId;
		this.ownedConnectionId = undefined;
		if (connectionId !== undefined) {
			const disconnected = await this.controller.client.disconnectConnection(
				this.controller.contextId,
				connectionId,
			);
			this.controller.adoptSnapshot(disconnected);
			const deleted = await this.controller.client.deleteConnection(
				this.controller.contextId,
				connectionId,
				`dap:${requestSequence}:delete-connection:${connectionId}`,
			);
			this.controller.adoptSnapshot(deleted);
		}
	}

	private handleThreads(request: DebugProtocol.Request): void {
		const threads = [...this.threadToTarget.entries()].map(([id, binding]) => ({
			id,
			name: binding.target.title
				|| binding.target.url
				|| `${binding.target.targetType} ${binding.target.targetId}`,
		}));
		this.sendResponse(request, { threads } satisfies DebugProtocol.ThreadsResponse["body"]);
	}

	private async handleStackTrace(request: DebugProtocol.StackTraceRequest): Promise<void> {
		const binding = this.requireTarget(request.arguments.threadId);
		const target = await this.controller.client.getTarget(
			this.controller.contextId,
			binding.connectionId,
			binding.target.targetId,
		);
		const pause = target.pause;
		if (pause === undefined) {
			this.sendResponse(request, { stackFrames: [], totalFrames: 0 });
			return;
		}
		const start = request.arguments.startFrame ?? 0;
		const levels = request.arguments.levels ?? pause.frames.length;
		const frames = pause.frames.slice(start, start + levels).map((frame) => {
			const frameId = this.nextFrameId++;
			this.frameBindings.set(frameId, {
				target: binding,
				frame,
				pauseEpoch: pause.epoch,
			});
			const location = frame.projected.kind === "resolved"
				? frame.projected.location
				: frame.raw;
			return {
				id: frameId,
				name: frame.functionName || "(anonymous)",
				source: this.sourceRegistry.sourceForLocation(location),
				line: Math.max(1, location.line),
				column: Math.max(1, location.column),
			};
		});
		this.sendResponse(request, {
			stackFrames: frames,
			totalFrames: pause.frames.length,
		} satisfies DebugProtocol.StackTraceResponse["body"]);
	}

	private async handleSource(request: DebugProtocol.SourceRequest): Promise<void> {
		const result = await this.sourceRegistry.content(request.arguments.sourceReference);
		this.sendResponse(request, result satisfies DebugProtocol.SourceResponse["body"]);
	}

	private async handleLoadedSources(request: DebugProtocol.Request): Promise<void> {
		const sources = await this.controller.client.listSources(this.controller.contextId);
		this.sendResponse(request, {
			sources: sources.map((source) => this.sourceRegistry.sourceForInfo(source)),
		} satisfies DebugProtocol.LoadedSourcesResponse["body"]);
	}

	private async handleSetBreakpoints(
		request: DebugProtocol.SetBreakpointsRequest,
	): Promise<void> {
		const sourcePath = this.sourceRegistry.sourcePath(request.arguments.source);
		if (sourcePath === undefined) {
			throw new Error("setBreakpoints requires a source path or sourceReference");
		}
		const prior = this.breakpointIdsBySource.get(sourcePath) ?? new Set<string>();
		const next = new Set<string>();
		const responses: DebugProtocol.Breakpoint[] = [];

		for (const requested of request.arguments.breakpoints ?? []) {
			const id = breakpointId(sourcePath, requested.line, requested.column ?? 1);
			next.add(id);
			const snapshot = await this.controller.client.putBreakpoint(
				this.controller.contextId,
				id,
				{
					sourcePath,
					line: requested.line,
					column: requested.column ?? 1,
					enabled: true,
					...(requested.condition === undefined ? {} : { condition: requested.condition }),
				},
				`dap:${request.seq}:${id}`,
			);
			this.controller.adoptSnapshot(snapshot);
			responses.push(toDapBreakpoint(
				snapshot.breakpoints.find((breakpoint) => breakpoint.id === id),
				requested,
			));
		}
		for (const id of prior) {
			if (!next.has(id)) {
				const snapshot = await this.controller.client.deleteBreakpoint(
					this.controller.contextId,
					id,
					`dap:${request.seq}:delete:${id}`,
				);
				this.controller.adoptSnapshot(snapshot);
			}
		}
		this.breakpointIdsBySource.set(sourcePath, next);
		this.sendResponse(request, { breakpoints: responses });
	}

	private async handleContinue(request: DebugProtocol.ContinueRequest): Promise<void> {
		const binding = this.requireTarget(request.arguments.threadId);
		const target = await this.getPausedTarget(binding);
		await this.controller.client.resumeTarget(
			this.controller.contextId,
			binding.connectionId,
			binding.target.targetId,
			target.pause?.epoch ?? requirePauseEpoch(target),
		);
		this.sendResponse(request, { allThreadsContinued: false });
	}

	private async handleStep(
		request: DebugProtocol.NextRequest | DebugProtocol.StepInRequest | DebugProtocol.StepOutRequest,
		kind: "into" | "over" | "out",
	): Promise<void> {
		const binding = this.requireTarget(request.arguments.threadId);
		const target = await this.getPausedTarget(binding);
		await this.controller.client.stepTarget(
			this.controller.contextId,
			binding.connectionId,
			binding.target.targetId,
			target.pause?.epoch ?? requirePauseEpoch(target),
			kind,
		);
		this.sendResponse(request);
	}

	private async handleEvaluate(request: DebugProtocol.EvaluateRequest): Promise<void> {
		const frame = request.arguments.frameId === undefined
			? undefined
			: this.frameBindings.get(request.arguments.frameId);
		const target = frame?.target ?? this.threadToTarget.values().next().value as TargetBinding | undefined;
		if (target === undefined) {
			throw new Error("No attached jsdbg target is available for evaluation");
		}
		const evaluation = await this.controller.client.evaluateTarget(
			this.controller.contextId,
			target.connectionId,
			target.target.targetId,
			frame?.pauseEpoch,
			frame?.frame.index ?? 0,
			request.arguments.expression,
		);
		this.sendResponse(request, {
			result: evaluation.description
				?? evaluation.unserializableValue
				?? formatEvaluationValue(evaluation.value),
			variablesReference: 0,
		} satisfies DebugProtocol.EvaluateResponse["body"]);
	}

	private acceptSnapshot(snapshot: ContextSnapshot): void {
		const previousKeys = new Set(this.targetKeyToThread.keys());
		const currentKeys = new Set<string>();
		for (const connection of snapshot.connections) {
			for (const target of connection.targets.filter((candidate) => candidate.attached)) {
				const key = targetKey(connection.id, target.targetId);
				currentKeys.add(key);
				let thread = this.targetKeyToThread.get(key);
				if (thread === undefined) {
					thread = this.nextThreadId++;
					this.targetKeyToThread.set(key, thread);
					this.sendEvent("thread", { reason: "started", threadId: thread });
					const observer = { cancelled: false };
					this.targetObservers.set(key, observer);
					void this.observeTarget(thread, {
						connectionId: connection.id,
						target,
					}, observer).catch((error: unknown) => {
						if (!observer.cancelled && !this.disposed) {
							this.reportAdapterError(error);
						}
					});
				}
				this.threadToTarget.set(thread, {
					connectionId: connection.id,
					target,
				});
			}
		}
		for (const key of previousKeys) {
			if (!currentKeys.has(key)) {
				const thread = this.targetKeyToThread.get(key);
				if (thread !== undefined) {
					this.sendEvent("thread", { reason: "exited", threadId: thread });
					this.threadToTarget.delete(thread);
				}
				const observer = this.targetObservers.get(key);
				if (observer !== undefined) {
					observer.cancelled = true;
					this.targetObservers.delete(key);
				}
				this.targetKeyToThread.delete(key);
			}
		}
		if (this.supportsInvalidatedEvent) {
			this.sendEvent("invalidated", { areas: ["threads", "stacks"] });
		}
		void this.refreshSources().catch((error: unknown) => {
			if (!this.disposed) {
				this.reportAdapterError(error);
			}
		});
	}

	private async observeTarget(
		threadId: number,
		binding: TargetBinding,
		observer: { cancelled: boolean },
	): Promise<void> {
		let lastPauseEpoch = 0;
		let paused = false;
		while (!observer.cancelled && !this.disposed) {
			const snapshot = paused
				? await this.controller.client.waitTarget(
					this.controller.contextId,
					binding.connectionId,
					binding.target.targetId,
					{ kind: "running" },
					30_000,
				)
				: await this.controller.client.waitTarget(
					this.controller.contextId,
					binding.connectionId,
					binding.target.targetId,
					{ kind: "paused", afterEpoch: lastPauseEpoch },
					30_000,
				);
			if (observer.cancelled || this.disposed) {
				return;
			}
			if (!paused && snapshot.phase.kind === "paused" && snapshot.pause !== undefined) {
				lastPauseEpoch = snapshot.pause.epoch;
				paused = true;
				this.sendEvent("stopped", {
					reason: stoppedReason(snapshot.pause.reason),
					description: snapshot.pause.reason,
					threadId,
					allThreadsStopped: false,
				});
			} else if (paused && snapshot.phase.kind === "running") {
				paused = false;
				this.sendEvent("continued", {
					threadId,
					allThreadsContinued: false,
				});
			}
		}
	}

	private async refreshSources(): Promise<void> {
		const infos = await this.controller.client.listSources(this.controller.contextId);
		const next = new Set(infos.map((source) => source.path));
		for (const source of infos) {
			if (!this.sourcePaths.has(source.path)) {
				this.sendEvent("loadedSource", {
					reason: "new",
					source: this.sourceRegistry.sourceForInfo(source),
				});
			}
		}
		for (const path of this.sourcePaths) {
			if (!next.has(path)) {
				this.sendEvent("loadedSource", {
					reason: "removed",
					source: this.sourceRegistry.sourceForInfo({
						path,
						kind: "unknown",
						status: "removed",
					}),
				});
			}
		}
		this.sourcePaths = next;
	}

	private requireTarget(threadId: number): TargetBinding {
		const target = this.threadToTarget.get(threadId);
		if (target === undefined) {
			throw new Error(`Unknown jsdbg thread ${threadId}`);
		}
		return target;
	}

	private getPausedTarget(binding: TargetBinding): Promise<TargetDebuggerSnapshot> {
		return this.controller.client.getTarget(
			this.controller.contextId,
			binding.connectionId,
			binding.target.targetId,
		);
	}

	private sendResponse(request: DebugProtocol.Request, body?: object): void {
		const response: DebugProtocol.Response = {
			seq: this.sequence++,
			type: "response",
			request_seq: request.seq,
			command: request.command,
			success: true,
			...(body === undefined ? {} : { body }),
		};
		this.messageEmitter.fire(response);
	}

	private sendError(request: DebugProtocol.Request, error: unknown): void {
		const message = error instanceof Error ? error.message : String(error);
		const response: DebugProtocol.Response = {
			seq: this.sequence++,
			type: "response",
			request_seq: request.seq,
			command: request.command,
			success: false,
			message,
		};
		this.messageEmitter.fire(response);
	}

	private sendEvent(event: string, body?: object): void {
		const debugEvent: DebugProtocol.Event = {
			seq: this.sequence++,
			type: "event",
			event,
			...(body === undefined ? {} : { body }),
		};
		this.messageEmitter.fire(debugEvent);
	}

	private reportAdapterError(error: unknown): void {
		const message = error instanceof Error ? error.message : String(error);
		this.sendEvent("output", {
			category: "stderr",
			output: `jsdbg adapter: ${message}\n`,
		});
	}
}

function breakpointId(path: string, line: number, column: number): string {
	const digest = createHash("sha256")
		.update(`${path}\0${line}\0${column}`)
		.digest("hex")
		.slice(0, 20);
	return `vscode:${digest}`;
}

function toDapBreakpoint(
	breakpoint: BreakpointSnapshot | undefined,
	requested: DebugProtocol.SourceBreakpoint,
): DebugProtocol.Breakpoint {
	if (breakpoint === undefined) {
		return {
			verified: false,
			line: requested.line,
			...(requested.column === undefined ? {} : { column: requested.column }),
			message: "jsdbg did not return breakpoint state",
		};
	}
	const verified = breakpoint.status.kind === "bound"
		|| breakpoint.status.kind === "partiallyBound";
	return {
		id: numericBreakpointId(breakpoint.id),
		verified,
		line: breakpoint.line,
		column: breakpoint.column,
		...(!verified ? { message: breakpointStatusMessage(breakpoint) } : {}),
	};
}

function numericBreakpointId(id: string): number {
	return Number.parseInt(createHash("sha256").update(id).digest("hex").slice(0, 7), 16);
}

function breakpointStatusMessage(breakpoint: BreakpointSnapshot): string {
	if (breakpoint.status.kind === "failed" && typeof breakpoint.status.message === "string") {
		return breakpoint.status.message;
	}
	return `jsdbg breakpoint is ${breakpoint.status.kind}`;
}

function requirePauseEpoch(target: TargetDebuggerSnapshot): number {
	const epoch = target.phase.epoch;
	if (target.phase.kind !== "paused" || typeof epoch !== "number") {
		throw new Error(`Target '${target.targetId}' is not paused`);
	}
	return epoch;
}

function formatEvaluationValue(value: unknown): string {
	if (typeof value === "string") {
		return value;
	}
	if (value === undefined) {
		return "undefined";
	}
	return JSON.stringify(value);
}

function stoppedReason(reason: string): string {
	const normalized = reason.toLowerCase();
	if (normalized.includes("breakpoint")) {
		return "breakpoint";
	}
	if (normalized.includes("exception")) {
		return "exception";
	}
	if (normalized.includes("step")) {
		return "step";
	}
	return "pause";
}
