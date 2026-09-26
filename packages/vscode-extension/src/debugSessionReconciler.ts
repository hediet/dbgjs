import * as vscode from "vscode";
import type { TargetNodeSnapshot } from "./apiTypes.js";
import {
	targetDebugConfiguration,
	targetReferenceFromConfiguration,
} from "./launchConfig.js";
import {
	targetKey,
	targetReference,
	type TargetReference,
} from "./model.js";
import type { WorkspaceContextController } from "./workspaceContext.js";

export class DebugSessionReconciler implements vscode.Disposable {
	private readonly bindingsEmitter = new vscode.EventEmitter<void>();
	private readonly bindings = new Map<string, TargetReference>();
	private readonly confirmedBindings = new Set<string>();
	private readonly sessions = new Map<string, vscode.DebugSession>();
	private readonly unboundSessions = new Set<string>();
	private readonly expectedTerminations = new Set<string>();
	private readonly suppressedRoots = new Set<string>();
	private readonly pendingRoots = new Set<string>();
	private readonly subscriptions: vscode.Disposable[];
	private reconcileTask = Promise.resolve();
	private disposed = false;
	public readonly onDidChangeBindings = this.bindingsEmitter.event;

	public constructor(
		private readonly controller: WorkspaceContextController,
		private readonly log: (message: string) => void,
	) {
		if (vscode.debug.activeDebugSession !== undefined) {
			this.sessions.set(
				vscode.debug.activeDebugSession.id,
				vscode.debug.activeDebugSession,
			);
		}
		this.subscriptions = [
			controller.onDidChangeSnapshot(() => this.scheduleReconcile()),
			vscode.debug.onDidStartDebugSession((session) => {
				this.sessions.set(session.id, session);
				if (session.type === "dbgjs") {
					const target = targetReferenceFromConfiguration(session.configuration);
					if (target !== undefined) {
						this.bindings.set(session.id, target);
						this.bindingsEmitter.fire();
					}
				}
			}),
			vscode.debug.onDidTerminateDebugSession((session) => {
				this.sessions.delete(session.id);
				if (session.type === "dbgjs") {
					this.sessionTerminated(session);
				}
			}),
		];
		this.scheduleReconcile();
	}

	public registerAdapter(sessionId: string): void {
		if (!this.bindings.has(sessionId)) {
			this.unboundSessions.add(sessionId);
		}
	}

	public bindSession(sessionId: string, target: TargetReference): void {
		this.unboundSessions.delete(sessionId);
		this.bindings.set(sessionId, target);
		this.confirmedBindings.add(sessionId);
		this.pendingRoots.delete(targetKey(target));
		this.bindingsEmitter.fire();
		this.scheduleReconcile();
	}

	public targetEnded(sessionId: string): void {
		this.expectedTerminations.add(sessionId);
	}

	public hasSessionForTarget(
		target: TargetReference,
		exceptSessionId?: string,
	): boolean {
		const key = targetKey(target);
		return [...this.bindings].some(([sessionId, binding]) =>
			sessionId !== exceptSessionId && targetKey(binding) === key
		);
	}

	public isSessionPlacementCurrent(
		sessionId: string,
		node: TargetNodeSnapshot,
	): boolean {
		const session = this.sessions.get(sessionId);
		if (session === undefined) {
			return true;
		}
		const expectedParentId = node.parentTargetId;
		const actualParent = session.parentSession === undefined
			? undefined
			: this.bindings.get(session.parentSession.id);
		if (expectedParentId === undefined) {
			return actualParent === undefined;
		}
		return actualParent !== undefined
			&& actualParent.contextId === this.controller.contextId
			&& actualParent.connectionId === node.connectionId
			&& actualParent.connectionGeneration === node.connectionGeneration
			&& actualParent.targetId === expectedParentId;
	}

	public dispose(): void {
		this.disposed = true;
		for (const subscription of this.subscriptions) {
			subscription.dispose();
		}
		this.bindingsEmitter.dispose();
	}

	private sessionTerminated(session: vscode.DebugSession): void {
		const target = this.bindings.get(session.id);
		this.bindings.delete(session.id);
		const confirmed = this.confirmedBindings.delete(session.id);
		this.unboundSessions.delete(session.id);
		const expected = this.expectedTerminations.delete(session.id);
		this.bindingsEmitter.fire();
		if (target !== undefined
			&& confirmed
			&& session.parentSession === undefined
			&& !expected
			&& this.isCurrentRoot(target)) {
			this.suppressedRoots.add(targetKey(target));
			this.log(`Suppressing automatic restart of manually stopped target ${target.targetId}`);
		}
		this.scheduleReconcile();
	}

	private isCurrentRoot(target: TargetReference): boolean {
		return (this.controller.snapshot?.targetForest ?? [])
			.some((root) =>
				root.parentTargetId === null
				&& targetKey(targetReference(this.controller.contextId, root))
					=== targetKey(target)
			);
	}

	private scheduleReconcile(): void {
		this.reconcileTask = this.reconcileTask
			.then(() => this.reconcileRoots())
			.catch((error: unknown) => {
				this.log(`Failed to reconcile root debug sessions: ${errorMessage(error)}`);
			});
	}

	private async reconcileRoots(): Promise<void> {
		if (this.disposed || this.unboundSessions.size > 0) {
			return;
		}
		const snapshot = this.controller.snapshot;
		if (snapshot === undefined) {
			return;
		}
		const roots = snapshot.targetForest.filter(
			(node) => node.parentTargetId === null,
		);
		const rootKeys = new Set(
			roots.map((root) =>
				targetKey(targetReference(this.controller.contextId, root))
			),
		);
		for (const suppressed of this.suppressedRoots) {
			if (!rootKeys.has(suppressed)) {
				this.suppressedRoots.delete(suppressed);
			}
		}
		const active = new Set(
			[...this.bindings.values()].map(targetKey),
		);
		for (const session of this.sessions.values()) {
			const target = targetReferenceFromConfiguration(session.configuration);
			if (session.type === "dbgjs" && target !== undefined) {
				active.add(targetKey(target));
			}
		}

		for (const root of roots) {
			const target = targetReference(this.controller.contextId, root);
			const key = targetKey(target);
			if (active.has(key)
				|| this.pendingRoots.has(key)
				|| this.suppressedRoots.has(key)) {
				continue;
			}
			this.pendingRoots.add(key);
			this.log(`Starting VS Code debug session for root target ${target.targetId}`);
			let started: boolean;
			try {
				started = await vscode.debug.startDebugging(
					workspaceFolderFor(root),
					targetDebugConfiguration(this.controller.contextId, root),
					{
						suppressSaveBeforeStart: true,
						suppressDebugView: true,
					},
				);
			} catch (error) {
				this.pendingRoots.delete(key);
				throw error;
			}
			if (!started) {
				this.pendingRoots.delete(key);
				this.log(`VS Code declined root debug session for target ${target.targetId}`);
			}
		}
	}
}

function workspaceFolderFor(_node: TargetNodeSnapshot): vscode.WorkspaceFolder | undefined {
	return vscode.workspace.workspaceFolders?.[0];
}

function errorMessage(error: unknown): string {
	return error instanceof Error ? error.message : String(error);
}
