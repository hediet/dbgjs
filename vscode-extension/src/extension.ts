import * as vscode from "vscode";
import type { ContextSnapshot } from "./apiTypes.js";
import { JsdbgDebugAdapter } from "./dapAdapter.js";
import { DebugSessionReconciler } from "./debugSessionReconciler.js";
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
	const output = vscode.window.createOutputChannel("jsdbg");
	const log = (message: string): void => {
		const line = `[${new Date().toISOString()}] ${message}`;
		output.appendLine(line);
		if (process.env.JSDBG_TEST_LOG_STDOUT === "1") {
			console.log(`[jsdbg] ${line}`);
		}
	};
	context.subscriptions.push(output);
	log("Activating jsdbg extension");
	const controller = new WorkspaceContextController(context, log);
	const overlays = new EditorOverlayTracker();
	const targetTree = new TargetTreeProvider(controller);
	const sessionReconciler = new DebugSessionReconciler(controller, log);

	context.subscriptions.push(
		controller,
		overlays,
		targetTree,
		sessionReconciler,
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
			createDebugAdapterDescriptor: (session) =>
				new vscode.DebugAdapterInlineImplementation(
					new JsdbgDebugAdapter(controller, session, sessionReconciler, log),
				),
		}),
	);

	log("Registered debug adapter descriptor factory for 'jsdbg'");
	void vscode.window.showInformationMessage(
		"jsdbg extension activated; debug adapter 'jsdbg' is registered.",
		"Show Log",
	).then((selection) => {
		if (selection === "Show Log") {
			output.show(true);
		}
	});

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
