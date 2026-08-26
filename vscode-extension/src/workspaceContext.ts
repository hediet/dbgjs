import * as vscode from "vscode";
import type { ContextSnapshot } from "./apiTypes.js";
import { DaemonClient, defaultServiceStateFile } from "./daemonClient.js";
import { ensureDaemonProcess } from "./daemonProcess.js";
import { normalizeContextPath } from "./model.js";

const contextIdStateKey = "jsdbg.workspaceContextId";

export class WorkspaceContextController implements vscode.Disposable {
	private clientValue: DaemonClient | undefined;
	private snapshotValue: ContextSnapshot | undefined;
	private errorValue: Error | undefined;
	private disposed = false;
	private observing = false;
	private connectionAttempt: Promise<void> | undefined;
	private readonly snapshotEmitter = new vscode.EventEmitter<ContextSnapshot | undefined>();
	private readonly errorEmitter = new vscode.EventEmitter<Error | undefined>();

	public readonly onDidChangeSnapshot = this.snapshotEmitter.event;
	public readonly onDidChangeError = this.errorEmitter.event;
	public readonly contextId: string;
	public readonly displayName: string;

	public constructor(
		private readonly extensionContext: vscode.ExtensionContext,
		private readonly log: (message: string) => void,
	) {
		this.contextId = singleFolderContextId();
		this.displayName = workspaceDisplayName();
		void extensionContext.workspaceState.update(contextIdStateKey, undefined);
		void this.ensureReady();
	}

	public get ready(): Promise<void> {
		return this.ensureReady();
	}

	public get client(): DaemonClient {
		if (this.clientValue === undefined) {
			throw this.errorValue ?? new Error("jsdbg daemon is not connected");
		}
		return this.clientValue;
	}

	public get snapshot(): ContextSnapshot | undefined {
		return this.snapshotValue;
	}

	public get error(): Error | undefined {
		return this.errorValue;
	}

	public async refresh(): Promise<void> {
		if (this.clientValue === undefined) {
			await this.ensureReady();
			return;
		}
		this.adoptSnapshot(await this.clientValue.getContext(this.contextId));
		if (!this.observing) {
			void this.observe();
		}
	}

	public ensureReady(): Promise<void> {
		if (this.clientValue !== undefined) {
			return Promise.resolve();
		}
		if (this.connectionAttempt === undefined) {
			const attempt = this.connectAndInitialize();
			this.connectionAttempt = attempt;
			void attempt.finally(() => {
				if (this.connectionAttempt === attempt) {
					this.connectionAttempt = undefined;
				}
			}).catch(() => undefined);
		}
		return this.connectionAttempt;
	}

	public adoptSnapshot(snapshot: ContextSnapshot): void {
		if (snapshot.id !== this.contextId) {
			throw new Error(`Received context '${snapshot.id}' for '${this.contextId}'`);
		}
		if (this.snapshotValue?.agentInstanceId === snapshot.agentInstanceId
			&& snapshot.revision < this.snapshotValue.revision) {
			this.log(
				`Ignoring stale context snapshot revision ${snapshot.revision}; `
				+ `current revision is ${this.snapshotValue.revision}`,
			);
			return;
		}
		this.snapshotValue = snapshot;
		this.errorValue = undefined;
		this.snapshotEmitter.fire(snapshot);
		this.errorEmitter.fire(undefined);
	}

	public dispose(): void {
		this.disposed = true;
		this.clientValue?.close();
		this.snapshotEmitter.dispose();
		this.errorEmitter.dispose();
	}

	private async connectAndInitialize(): Promise<void> {
		try {
			this.clientValue?.close();
			const configured = vscode.workspace
				.getConfiguration("jsdbg")
				.get<string>("serviceStatePath");
			const configuredExecutable = vscode.workspace
				.getConfiguration("jsdbg")
				.get<string>("serviceExecutable");
			const stateFile = configured?.trim() || defaultServiceStateFile();
			await ensureDaemonProcess({
				extensionPath: this.extensionContext.extensionPath,
				stateFile,
				log: this.log,
				...(configuredExecutable === undefined ? {} : { configuredExecutable }),
			});
			const client = await DaemonClient.connect(stateFile, this.log);
			this.clientValue = client;
			client.onClose(() => {
				if (!this.disposed) {
					this.clientValue = undefined;
					this.setError(new Error("jsdbg daemon connection closed"));
				}
			});
			this.adoptSnapshot(await client.putContext(this.contextId, "path", this.displayName));
			void this.observe();
		} catch (error) {
			this.clientValue = undefined;
			this.setError(asError(error));
			throw error;
		}
	}

	private async observe(): Promise<void> {
		if (this.observing) {
			return;
		}
		this.observing = true;
		try {
			while (!this.disposed && this.clientValue !== undefined) {
				const client = this.clientValue;
				const snapshot = await client.observeContext(
					this.contextId,
					this.snapshotValue?.revision,
					30_000,
				);
				if (client !== this.clientValue) {
					return;
				}
				if (snapshot !== undefined) {
					this.adoptSnapshot(snapshot);
				}
			}
		} catch (error) {
			if (!this.disposed) {
				const observationError = asError(error);
				this.log(`Context observation failed: ${observationError.message}`);
				this.setError(observationError);
			}
		} finally {
			this.observing = false;
		}
	}

	private setError(error: Error): void {
		this.errorValue = error;
		this.errorEmitter.fire(error);
		this.snapshotEmitter.fire(this.snapshotValue);
	}
}

function singleFolderContextId(): string {
	const folders = vscode.workspace.workspaceFolders;
	if (folders?.length !== 1) {
		throw new Error("jsdbg requires exactly one workspace folder; multi-root and untitled workspace context selection is not yet defined");
	}
	return normalizeContextPath(folders[0]!.uri.fsPath);
}

function workspaceDisplayName(): string {
	if (vscode.workspace.name !== undefined) {
		return vscode.workspace.name;
	}
	const folder = vscode.workspace.workspaceFolders?.[0];
	return folder?.name ?? "Untitled VS Code Workspace";
}

function asError(value: unknown): Error {
	return value instanceof Error ? value : new Error(String(value));
}
