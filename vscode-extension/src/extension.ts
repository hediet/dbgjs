import * as vscode from "vscode";
import type { ContextSnapshot } from "./apiTypes.js";
import { JsdbgDebugAdapter } from "./dapAdapter.js";
import {
	EditorOverlayTracker,
	type EditorSourceSnapshot,
} from "./editorOverlay.js";
import { TargetTreeProvider } from "./targetTree.js";
import { WorkspaceContextController } from "./workspaceContext.js";

export interface JsdbgExtensionApi {
	readonly contextId: string;
	readonly ready: Promise<void>;
	getSnapshot(): ContextSnapshot | undefined;
	getEditorSnapshots(): readonly EditorSourceSnapshot[];
	refresh(): Promise<void>;
}

export function activate(context: vscode.ExtensionContext): JsdbgExtensionApi {
	const controller = new WorkspaceContextController(context);
	const overlays = new EditorOverlayTracker();
	const targetTree = new TargetTreeProvider(controller);

	context.subscriptions.push(
		controller,
		overlays,
		targetTree,
		vscode.window.registerTreeDataProvider("jsdbg.targets", targetTree),
		vscode.commands.registerCommand("jsdbg.refreshTargets", async () => {
			try {
				await controller.refresh();
			} catch (error) {
				await vscode.window.showErrorMessage(
					`Failed to refresh jsdbg targets: ${errorMessage(error)}`,
				);
			}
		}),
		vscode.commands.registerCommand("jsdbg.copyContextId", async () => {
			await vscode.env.clipboard.writeText(controller.contextId);
		}),
		vscode.debug.registerDebugAdapterDescriptorFactory("jsdbg", {
			createDebugAdapterDescriptor: () =>
				new vscode.DebugAdapterInlineImplementation(
					new JsdbgDebugAdapter(controller),
				),
		}),
	);

	void controller.ready.catch((error: unknown) => {
		void vscode.window.showWarningMessage(
			`jsdbg daemon is unavailable: ${errorMessage(error)}`,
			"Retry",
		).then((selection) => {
			if (selection === "Retry") {
				void vscode.commands.executeCommand("jsdbg.refreshTargets");
			}
		});
	});

	return {
		contextId: controller.contextId,
		ready: controller.ready,
		getSnapshot: () => controller.snapshot,
		getEditorSnapshots: () => overlays.all(),
		refresh: () => controller.refresh(),
	};
}

export function deactivate(): void {}

function errorMessage(error: unknown): string {
	return error instanceof Error ? error.message : String(error);
}
