import type { DebugProtocol } from "@vscode/debugprotocol";
import { createHash } from "node:crypto";
import * as vscode from "vscode";
import type {
	BreakpointSnapshot,
	ContextSnapshot,
	FrameSnapshot,
	SourceSnapshotInfo,
	TargetDebuggerSnapshot,
	TargetNodeSnapshot,
	TargetSnapshot,
	VariableSnapshot,
} from "./apiTypes.js";
import type { DebugSessionReconciler } from "./debugSessionReconciler.js";
import {
	resolveLaunch,
	targetDebugConfiguration,
} from "./launchConfig.js";
import {
	breakpointId,
	findTargetNode,
	targetKey,
	targetReference,
	type TargetReference,
} from "./model.js";
import { SourceRegistry } from "./sourceRegistry.js";
import type { WorkspaceContextController } from "./workspaceContext.js";

interface TargetBinding {
	readonly connectionId: string;
	readonly connectionGeneration: number;
	readonly target: TargetSnapshot;
}

interface FrameBinding {
	readonly target: TargetBinding;
	readonly frame: FrameSnapshot;
	readonly pauseEpoch: number;
}

type VariablesBinding =
	| {
		readonly kind: "scope";
		readonly frame: FrameBinding;
		readonly scopeIndex: number;
	}
	| {
		readonly kind: "object";
		readonly target: TargetBinding;
		readonly pauseEpoch: number | undefined;
		readonly objectId: string;
	};

export class JsdbgDebugAdapter implements vscode.DebugAdapter, vscode.Disposable {
	private static readonly threadId = 1;
	private sequence = 1;
	private nextFrameId = 1;
	private nextVariablesReference = 1;
	private supportsInvalidatedEvent = false;
	private supportsStartDebuggingRequest = false;
	private readonly messageEmitter = new vscode.EventEmitter<DebugProtocol.ProtocolMessage>();
	private readonly frameBindings = new Map<number, FrameBinding>();
	private readonly variablesBindings = new Map<number, VariablesBinding>();
	private readonly breakpointIdsBySource = new Map<string, Set<string>>();
	private readonly pendingRequests = new Map<
		number,
		{ resolve(): void; reject(error: Error): void }
	>();
	private readonly declaredChildren = new Set<string>();
	private readonly sourceRegistry: SourceRegistry;
	private readonly subscriptions: vscode.Disposable[];
	private targetBinding: TargetBinding | undefined;
	private targetObserver: { cancelled: boolean } | undefined;
	private sourcePaths = new Set<string>();
	private disposed = false;
	private configurationDone = false;
	private childCapabilityWarningShown = false;
	private ownedConnectionId: string | undefined;
	private launchTask: Promise<void> | undefined;
	private cleanupTask: Promise<void> | undefined;
	private sourceRefreshTask = Promise.resolve();

	public readonly onDidSendMessage = this.messageEmitter.event;

	public constructor(
		private readonly controller: WorkspaceContextController,
		private readonly debugSession: vscode.DebugSession,
		private readonly reconciler: DebugSessionReconciler,
		private readonly log: (message: string) => void,
	) {
		reconciler.registerAdapter(debugSession.id);
		this.sourceRegistry = new SourceRegistry(controller);
		this.subscriptions = [
			controller.onDidChangeSnapshot((snapshot) => {
				if (snapshot !== undefined) {
					this.acceptSnapshot(snapshot);
				}
			}),
			reconciler.onDidChangeBindings(() => {
				if (this.configurationDone) {
					void this.reconcileChildren();
				}
			}),
		];
		if (controller.snapshot !== undefined) {
			this.acceptSnapshot(controller.snapshot);
		}
	}

	public handleMessage(message: DebugProtocol.ProtocolMessage): void {
		if (message.type === "response") {
			this.acceptResponse(message as DebugProtocol.Response);
			return;
		}
		if (message.type !== "request") {
			return;
		}
		void this.dispatch(message as DebugProtocol.Request).catch((error: unknown) => {
			this.sendError(message as DebugProtocol.Request, error);
		});
	}

	public dispose(): void {
		this.disposed = true;
		this.cancelTargetObservers();
		for (const pending of this.pendingRequests.values()) {
			pending.reject(new Error("jsdbg debug adapter was disposed"));
		}
		this.pendingRequests.clear();
		for (const subscription of this.subscriptions) {
			subscription.dispose();
		}
		this.messageEmitter.dispose();
	}

	private cancelTargetObservers(): void {
		if (this.targetObserver !== undefined) {
			this.targetObserver.cancelled = true;
			this.targetObserver = undefined;
		}
	}

	private async dispatch(request: DebugProtocol.Request): Promise<void> {
		switch (request.command) {
			case "initialize": {
				const initialize = request as DebugProtocol.InitializeRequest;
				this.supportsInvalidatedEvent =
					initialize.arguments.supportsInvalidatedEvent === true;
				this.supportsStartDebuggingRequest =
					initialize.arguments.supportsStartDebuggingRequest === true;
				this.log(
					`[dap:${this.debugSession.id}] initialize `
					+ `supportsStartDebuggingRequest=${this.supportsStartDebuggingRequest}`,
				);
				this.sendResponse(request, {
					supportsConfigurationDoneRequest: true,
					supportsEvaluateForHovers: true,
					supportsLoadedSourcesRequest: true,
					supportsTerminateRequest: true,
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
				await this.scheduleSourceRefresh();
				return;
			case "configurationDone":
				await this.releaseWaitingTarget();
				this.sendResponse(request);
				this.configurationDone = true;
				void this.reconcileChildren();
				return;
			case "threads":
				this.handleThreads(request);
				return;
			case "stackTrace":
				await this.handleStackTrace(request as DebugProtocol.StackTraceRequest);
				return;
			case "scopes":
				await this.handleScopes(request as DebugProtocol.ScopesRequest);
				return;
			case "variables":
				await this.handleVariables(request as DebugProtocol.VariablesRequest);
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
				this.log(`[dap:${this.debugSession.id}] detaching VS Code session`);
				this.sendResponse(request);
				this.sendEvent("terminated");
				return;
			case "terminate":
				this.log(`[dap:${this.debugSession.id}] terminating owned runtime`);
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

	private scheduleSourceRefresh(): Promise<void> {
		const task = this.sourceRefreshTask.then(() => this.refreshSources());
		this.sourceRefreshTask = task.catch(() => undefined);
		return task;
	}

	private async configureLaunch(request: DebugProtocol.LaunchRequest): Promise<void> {
		const launch = await resolveLaunch(request.arguments, this.debugSession.id);
		if (this.disposed) {
			throw new Error("jsdbg debug session was disposed during launch");
		}
		if (launch.target !== undefined) {
			await this.bindTarget(launch.target);
			return;
		}
		if (launch.configuration === undefined) {
			const root = await this.waitForRootTarget();
			await this.bindTarget(targetReference(this.controller.contextId, root));
			return;
		}
		const connectionId = launch.connectionId;
		if (connectionId === undefined) {
			throw new Error("jsdbg runtime launch did not produce a connection ID");
		}
		this.ownedConnectionId = connectionId;
		let snapshot = await this.controller.client.putConnection(
			this.controller.contextId,
			connectionId,
			launch.configuration,
		);
		this.controller.adoptSnapshot(snapshot);
		if (this.disposed) {
			throw new Error("jsdbg debug session was disposed during launch");
		}
		snapshot = await this.controller.client.connectConnection(
			this.controller.contextId,
			connectionId,
		);
		this.controller.adoptSnapshot(snapshot);
		if (this.disposed) {
			throw new Error("jsdbg debug session was disposed during launch");
		}
		const connection = snapshot.connections.find(
			(candidate) => candidate.id === connectionId,
		);
		if (connection?.status.kind === "failed") {
			throw new Error(
				typeof connection.status.message === "string"
					? connection.status.message
					: `jsdbg connection '${connectionId}' failed`,
			);
		}
		const root = await this.waitForRootTarget(connectionId);
		await this.bindTarget(targetReference(this.controller.contextId, root));
	}

	private async waitForRootTarget(connectionId?: string): Promise<TargetNodeSnapshot> {
		const deadline = Date.now() + 30_000;
		while (Date.now() < deadline) {
			await this.controller.refresh();
			const root = this.controller.snapshot?.targetForest.find(
				(candidate) =>
					candidate.parentTargetId === undefined
					&& (connectionId === undefined || candidate.connectionId === connectionId),
			);
			if (root !== undefined) {
				return root;
			}
			if (connectionId === undefined) {
				await new Promise((resolve) => setTimeout(resolve, 50));
				continue;
			}
			const connection = this.controller.snapshot?.connections.find(
				(candidate) => candidate.id === connectionId,
			);
			if (connection?.status.kind === "failed") {
				throw new Error(
					typeof connection.status.message === "string"
						? connection.status.message
						: `jsdbg connection '${connectionId}' failed`,
				);
			}
			await new Promise((resolve) => setTimeout(resolve, 50));
		}
		throw new Error(connectionId === undefined
			? "Timed out waiting for a jsdbg root target"
			: `Timed out waiting for jsdbg connection '${connectionId}' to expose a root target`);
	}

	private async bindTarget(reference: TargetReference): Promise<void> {
		if (reference.contextId !== this.controller.contextId) {
			throw new Error(
				`jsdbg target belongs to context '${reference.contextId}', `
				+ `not workspace context '${this.controller.contextId}'`,
			);
		}
		await this.controller.refresh();
		const node = findTargetNode(
			this.controller.snapshot?.targetForest ?? [],
			reference,
		);
		if (node === undefined) {
			throw new Error(
				`jsdbg target '${reference.targetId}' generation ${reference.connectionGeneration} does not exist`,
			);
		}
		await this.controller.client.attachTarget(
			this.controller.contextId,
			reference.connectionId,
			reference.targetId,
		);
		this.targetBinding = {
			connectionId: reference.connectionId,
			connectionGeneration: reference.connectionGeneration,
			target: node.target,
		};
		this.reconciler.bindSession(this.debugSession.id, reference);
		this.log(
			`[dap:${this.debugSession.id}] bound target `
			+ `${reference.connectionId}/${reference.connectionGeneration}/${reference.targetId}`,
		);
		this.sendEvent("thread", {
			reason: "started",
			threadId: JsdbgDebugAdapter.threadId,
		});
		const observer = { cancelled: false };
		this.targetObserver = observer;
		void this.observeTarget(
			JsdbgDebugAdapter.threadId,
			this.targetBinding,
			observer,
		).catch((error: unknown) => {
			if (!observer.cancelled && !this.disposed) {
				this.reportAdapterError(error);
			}
		});
	}

	private async releaseWaitingTarget(): Promise<void> {
		const binding = this.requireBoundTarget();
		await this.controller.client.releaseTarget(
			this.controller.contextId,
			binding.connectionId,
			binding.target.targetId,
		);
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
		this.cancelTargetObservers();
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
		const binding = this.targetBinding;
		const threads = binding === undefined
			? []
			: [{
				id: JsdbgDebugAdapter.threadId,
				name: binding.target.title
					|| binding.target.url
					|| `${binding.target.targetType} ${binding.target.targetId}`,
			}];
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

	private async handleScopes(request: DebugProtocol.ScopesRequest): Promise<void> {
		const frame = this.frameBindings.get(request.arguments.frameId);
		if (frame === undefined) {
			throw new Error(`Unknown or stale jsdbg frame ${request.arguments.frameId}`);
		}
		const scopes = frame.frame.scopes.map((scope) => ({
			name: scope.name ?? scopeName(scope.kind),
			presentationHint: scope.kind === "global" ? "globals" as const : "locals" as const,
			variablesReference: this.bindVariables({
				kind: "scope",
				frame,
				scopeIndex: scope.index,
			}),
			expensive: scope.kind === "global",
		}));
		this.sendResponse(request, { scopes } satisfies DebugProtocol.ScopesResponse["body"]);
	}

	private async handleVariables(request: DebugProtocol.VariablesRequest): Promise<void> {
		const binding = this.variablesBindings.get(request.arguments.variablesReference);
		if (binding === undefined) {
			throw new Error(
				`Unknown or stale jsdbg variables reference ${request.arguments.variablesReference}`,
			);
		}
		const variables = binding.kind === "scope"
			? await this.controller.client.getScopeVariables(
				this.controller.contextId,
				binding.frame.target.connectionId,
				binding.frame.target.target.targetId,
				binding.frame.pauseEpoch,
				binding.frame.frame.index,
				binding.scopeIndex,
			)
			: await this.controller.client.getObjectProperties(
				this.controller.contextId,
				binding.target.connectionId,
				binding.target.target.targetId,
				binding.pauseEpoch,
				binding.objectId,
			);
		this.sendResponse(request, {
			variables: variables.map((variable) => this.toDapVariable(
				binding.kind === "scope" ? binding.frame.target : binding.target,
				binding.kind === "scope" ? binding.frame.pauseEpoch : binding.pauseEpoch,
				variable,
			)),
		} satisfies DebugProtocol.VariablesResponse["body"]);
	}

	private async handleSource(request: DebugProtocol.SourceRequest): Promise<void> {
		const result = await this.sourceRegistry.content(request.arguments.sourceReference);
		this.sendResponse(request, result satisfies DebugProtocol.SourceResponse["body"]);
	}

	private async handleLoadedSources(request: DebugProtocol.Request): Promise<void> {
		const sources = this.filterTargetSources(
			await this.controller.client.listSources(this.controller.contextId),
		);
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
		this.sendResponse(request, { allThreadsContinued: true });
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
		const target = frame?.target ?? this.requireBoundTarget();
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
			type: evaluation.kind,
			variablesReference: evaluation.objectId === undefined
				? 0
				: this.bindVariables({
					kind: "object",
					target,
					pauseEpoch: frame?.pauseEpoch,
					objectId: evaluation.objectId,
				}),
		} satisfies DebugProtocol.EvaluateResponse["body"]);
	}

	private bindVariables(binding: VariablesBinding): number {
		const reference = this.nextVariablesReference++;
		this.variablesBindings.set(reference, binding);
		return reference;
	}

	private toDapVariable(
		target: TargetBinding,
		pauseEpoch: number | undefined,
		variable: VariableSnapshot,
	): DebugProtocol.Variable {
		return {
			name: variable.name,
			value: variable.description
				?? variable.unserializableValue
				?? formatEvaluationValue(variable.value),
			type: variable.kind,
			variablesReference: variable.objectId === undefined
				? 0
				: this.bindVariables({
					kind: "object",
					target,
					pauseEpoch,
					objectId: variable.objectId,
				}),
		};
	}

	private acceptSnapshot(snapshot: ContextSnapshot): void {
		const binding = this.targetBinding;
		if (binding !== undefined) {
			const reference = {
				contextId: this.controller.contextId,
				connectionId: binding.connectionId,
				connectionGeneration: binding.connectionGeneration,
				targetId: binding.target.targetId,
			};
			const node = findTargetNode(snapshot.targetForest, reference);
			if (node === undefined) {
				this.targetBinding = undefined;
				this.cancelTargetObservers();
				this.reconciler.targetEnded(this.debugSession.id);
				this.sendEvent("thread", {
					reason: "exited",
					threadId: JsdbgDebugAdapter.threadId,
				});
				this.sendEvent("terminated");
				return;
			}
			if (!this.reconciler.isSessionPlacementCurrent(this.debugSession.id, node)) {
				this.cancelTargetObservers();
				this.reconciler.targetEnded(this.debugSession.id);
				this.sendEvent("terminated");
				return;
			}
			this.targetBinding = {
				connectionId: node.connectionId,
				connectionGeneration: node.connectionGeneration,
				target: node.target,
			};
			if (this.configurationDone) {
				void this.reconcileChildren(snapshot.targetForest);
			}
		}
		if (this.supportsInvalidatedEvent) {
			this.sendEvent("invalidated", { areas: ["threads", "stacks"] });
		}
		void this.scheduleSourceRefresh().catch((error: unknown) => {
			if (!this.disposed) {
				this.reportAdapterError(error);
			}
		});
	}

	private async reconcileChildren(
		forest = this.controller.snapshot?.targetForest ?? [],
	): Promise<void> {
		const binding = this.targetBinding;
		if (binding === undefined || !this.configurationDone) {
			return;
		}
		const node = findTargetNode(
			forest,
			{
				contextId: this.controller.contextId,
				connectionId: binding.connectionId,
				connectionGeneration: binding.connectionGeneration,
				targetId: binding.target.targetId,
			},
		);
		if (node === undefined) {
			return;
		}
		const children = forest.filter((candidate) =>
			candidate.connectionId === node.connectionId
			&& candidate.connectionGeneration === node.connectionGeneration
			&& candidate.parentTargetId === node.target.targetId
		);
		this.log(
			`[dap:${this.debugSession.id}] reconciling ${children.length} direct child target(s)`,
		);
		const childKeys = new Set(children.map((child) =>
			targetKey(targetReference(this.controller.contextId, child))
		));
		for (const declared of this.declaredChildren) {
			if (!childKeys.has(declared)) {
				this.declaredChildren.delete(declared);
			}
		}
		if (children.length > 0 && !this.supportsStartDebuggingRequest) {
			if (!this.childCapabilityWarningShown) {
				this.childCapabilityWarningShown = true;
				this.reportAdapterError(
					new Error("VS Code did not advertise support for child debug sessions"),
				);
			}
			return;
		}

		for (const child of children) {
			const childReference = targetReference(this.controller.contextId, child);
			const key = targetKey(childReference);
			if (this.declaredChildren.has(key)) {
				continue;
			}
			if (this.reconciler.hasSessionForTarget(childReference, this.debugSession.id)) {
				continue;
			}
			this.declaredChildren.add(key);
			const configuration = targetDebugConfiguration(this.controller.contextId, child);
			const { type: _type, request: childRequest, ...adapterConfiguration } = configuration;
			this.log(
				`[dap:${this.debugSession.id}] requesting child session for ${child.target.targetId}`,
			);
			void this.sendRequest("startDebugging", {
				request: childRequest,
				configuration: adapterConfiguration,
			}).catch((error: unknown) => {
				this.declaredChildren.delete(key);
				if (!this.disposed) {
					this.reportAdapterError(error);
				}
			});
		}
	}

	private async observeTarget(
		threadId: number,
		binding: TargetBinding,
		observer: { cancelled: boolean },
	): Promise<void> {
		let lastRevision = 0;
		let lastPauseEpoch = 0;
		let lastLogIndex = 0;
		let paused = false;
		while (!observer.cancelled && !this.disposed) {
			const snapshot = await this.controller.client.observeTarget(
				this.controller.contextId,
				binding.connectionId,
				binding.target.targetId,
				lastRevision,
				30_000,
			);
			if (observer.cancelled || this.disposed) {
				return;
			}
			if (snapshot === undefined) {
				continue;
			}
			lastRevision = snapshot.revision;
			for (const log of snapshot.logs) {
				if (log.index > lastLogIndex) {
					this.sendEvent("output", {
						category: "console",
						output: `${log.values.join(" ")}\n`,
					});
					lastLogIndex = log.index;
				}
			}
			await this.scheduleSourceRefresh();
			if (snapshot.phase.kind === "paused"
				&& snapshot.pause !== undefined
				&& (!paused || snapshot.pause.epoch > lastPauseEpoch)) {
				this.clearInspectionBindings();
				lastPauseEpoch = snapshot.pause.epoch;
				paused = true;
				this.sendEvent("stopped", {
					reason: stoppedReason(snapshot.pause.reason),
					description: stoppedDescription(snapshot.pause.reason),
					threadId,
					allThreadsStopped: true,
				});
			} else if (paused && snapshot.phase.kind === "running") {
				this.clearInspectionBindings();
				paused = false;
				this.sendEvent("continued", {
					threadId,
					allThreadsContinued: true,
				});
			}

		}
	}

	private clearInspectionBindings(): void {
		this.frameBindings.clear();
		this.variablesBindings.clear();
	}

	private async refreshSources(): Promise<void> {
		const infos = this.filterTargetSources(
			await this.controller.client.listSources(this.controller.contextId),
		);
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

	private filterTargetSources(
		sources: readonly SourceSnapshotInfo[],
	): readonly SourceSnapshotInfo[] {
		const binding = this.targetBinding;
		if (binding === undefined) {
			return [];
		}
		return sources.filter((source) =>
			source.connectionId === undefined
			|| (
				source.connectionId === binding.connectionId
				&& source.targetId === binding.target.targetId
			)
		);
	}

	private requireTarget(threadId: number): TargetBinding {
		if (threadId !== JsdbgDebugAdapter.threadId) {
			throw new Error(`Unknown jsdbg thread ${threadId}`);
		}
		return this.requireBoundTarget();
	}

	private requireBoundTarget(): TargetBinding {
		if (this.targetBinding === undefined) {
			throw new Error("No jsdbg target is bound to this debug session");
		}
		return this.targetBinding;
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

	private sendRequest(command: string, args: object): Promise<void> {
		const seq = this.sequence++;
		const request: DebugProtocol.Request = {
			seq,
			type: "request",
			command,
			arguments: args,
		};
		const response = new Promise<void>((resolve, reject) => {
			this.pendingRequests.set(seq, { resolve, reject });
		});
		this.messageEmitter.fire(request);
		return response;
	}

	private acceptResponse(response: DebugProtocol.Response): void {
		const pending = this.pendingRequests.get(response.request_seq);
		if (pending === undefined) {
			return;
		}
		this.pendingRequests.delete(response.request_seq);
		this.log(
			`[dap:${this.debugSession.id}] reverse request '${response.command}' `
			+ `${response.success ? "succeeded" : "failed"}`,
		);
		if (response.success) {
			pending.resolve();
		} else {
			pending.reject(new Error(
				response.message ?? `DAP request '${response.command}' failed`,
			));
		}
	}

	private reportAdapterError(error: unknown): void {
		const message = error instanceof Error ? error.message : String(error);
		this.sendEvent("output", {
			category: "stderr",
			output: `jsdbg adapter: ${message}\n`,
		});
	}
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

function scopeName(kind: string): string {
	switch (kind) {
		case "local":
			return "Local";
		case "closure":
			return "Closure";
		case "catch":
			return "Catch";
		case "block":
			return "Block";
		case "script":
			return "Script";
		case "with":
			return "With";
		case "global":
			return "Global";
		default:
			return kind;
	}
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

function stoppedDescription(reason: string): string {
	const normalized = reason.toLowerCase();
	if (normalized === "ambiguous" || normalized === "other") {
		return "Paused";
	}
	if (normalized === "break on start") {
		return "Paused on entry";
	}
	if (normalized.includes("breakpoint")) {
		return "Paused on breakpoint";
	}
	if (normalized.includes("exception")) {
		return "Paused on exception";
	}
	if (normalized.includes("step")) {
		return "Paused after step";
	}
	return `Paused: ${reason}`;
}
