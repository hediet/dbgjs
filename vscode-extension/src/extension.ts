import * as vscode from "vscode";
import type { ContextSnapshot } from "./apiTypes.js";
import { DbgjsDebugAdapter } from "./dapAdapter.js";
import { DebugSessionReconciler } from "./debugSessionReconciler.js";
import {
	EditorOverlayTracker,
	type EditorSourceSnapshot,
} from "./editorOverlay.js";
import { TargetTreeProvider } from "./targetTree.js";
import { WorkspaceContextController } from "./workspaceContext.js";

export interface DbgjsExtensionApi {
	readonly contextId: string;
	readonly ready: Promise<void>;
	getSnapshot(): ContextSnapshot | undefined;
	getEditorSnapshots(): readonly EditorSourceSnapshot[];
	refresh(): Promise<void>;
}

export function activate(context: vscode.ExtensionContext): DbgjsExtensionApi {
	const output = vscode.window.createOutputChannel("dbgjs");
	const log = (message: string): void => {
		const line = `[${new Date().toISOString()}] ${message}`;
		output.appendLine(line);
		if (process.env.DBGJS_TEST_LOG_STDOUT === "1") {
			console.log(`[dbgjs] ${line}`);
		}
	};
	context.subscriptions.push(output);
	log("Activating dbgjs extension");
	const controller = new WorkspaceContextController(context, log);
	const overlays = new EditorOverlayTracker();
	const targetTree = new TargetTreeProvider(controller);
	const sessionReconciler = new DebugSessionReconciler(controller, log);

	context.subscriptions.push(
		controller,
		overlays,
		targetTree,
		sessionReconciler,
		vscode.window.registerTreeDataProvider("dbgjs.targets", targetTree),
		vscode.commands.registerCommand("dbgjs.refreshTargets", async () => {
			try {
				await controller.refresh();
			} catch (error) {
				await vscode.window.showErrorMessage(
					`Failed to refresh dbgjs targets: ${errorMessage(error)}`,
				);
			}
		}),
		vscode.commands.registerCommand("dbgjs.copyContextId", async () => {
			await vscode.env.clipboard.writeText(controller.contextId);
		}),
		vscode.debug.registerDebugAdapterDescriptorFactory("dbgjs", {
			createDebugAdapterDescriptor: (session) =>
				new vscode.DebugAdapterInlineImplementation(
					new DbgjsDebugAdapter(controller, session, sessionReconciler, log),
				),
		}),
	);

	log("Registered debug adapter descriptor factory for 'dbgjs'");
	void vscode.window.showInformationMessage(
		"dbgjs extension activated; debug adapter 'dbgjs' is registered.",
		"Show Log",
	).then((selection) => {
		if (selection === "Show Log") {
			output.show(true);
		}
	});

	void controller.ready.catch((error: unknown) => {
		void vscode.window.showWarningMessage(
			`dbgjs daemon is unavailable: ${errorMessage(error)}`,
			"Retry",
		).then((selection) => {
			if (selection === "Retry") {
				void vscode.commands.executeCommand("dbgjs.refreshTargets");
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
