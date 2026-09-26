import * as vscode from "vscode";

export interface LengthEdit {
	readonly oldStartUtf16: number;
	readonly oldLengthUtf16: number;
	readonly newLengthUtf16: number;
}

export interface EditorSourceSnapshot {
	readonly uri: string;
	readonly version: number;
	readonly dirty: boolean;
	readonly content: string;
	readonly previousVersion?: number;
	readonly lengthEdits: readonly LengthEdit[];
}

export class EditorOverlayTracker implements vscode.Disposable {
	private readonly snapshots = new Map<string, EditorSourceSnapshot>();
	private readonly subscriptions: vscode.Disposable[];
	private readonly changeEmitter = new vscode.EventEmitter<EditorSourceSnapshot>();

	public readonly onDidChange = this.changeEmitter.event;

	public constructor() {
		for (const document of vscode.workspace.textDocuments) {
			this.record(document, []);
		}
		this.subscriptions = [
			vscode.workspace.onDidOpenTextDocument((document) => this.record(document, [])),
			vscode.workspace.onDidChangeTextDocument((event) => {
				const previous = this.snapshots.get(event.document.uri.toString());
				this.record(
					event.document,
					event.contentChanges.map((change) => ({
						oldStartUtf16: change.rangeOffset,
						oldLengthUtf16: change.rangeLength,
						newLengthUtf16: change.text.length,
					})),
					previous?.version,
				);
			}),
			vscode.workspace.onDidSaveTextDocument((document) => this.record(document, [])),
			vscode.workspace.onDidCloseTextDocument((document) => {
				this.snapshots.delete(document.uri.toString());
			}),
		];
	}

	public get(uri: vscode.Uri): EditorSourceSnapshot | undefined {
		return this.snapshots.get(uri.toString());
	}

	public all(): readonly EditorSourceSnapshot[] {
		return [...this.snapshots.values()];
	}

	public dispose(): void {
		for (const subscription of this.subscriptions) {
			subscription.dispose();
		}
		this.changeEmitter.dispose();
	}

	private record(
		document: vscode.TextDocument,
		lengthEdits: readonly LengthEdit[],
		previousVersion?: number,
	): void {
		const snapshot: EditorSourceSnapshot = {
			uri: document.uri.toString(),
			version: document.version,
			dirty: document.isDirty,
			content: document.getText(),
			...(previousVersion === undefined ? {} : { previousVersion }),
			lengthEdits,
		};
		this.snapshots.set(snapshot.uri, snapshot);
		this.changeEmitter.fire(snapshot);
	}
}
