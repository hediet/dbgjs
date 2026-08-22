import assert from "node:assert/strict";
import * as vscode from "vscode";
import type { JsdbgExtensionApi } from "../../extension.js";

export async function run(): Promise<void> {
	const extension = vscode.extensions.getExtension<JsdbgExtensionApi>("hediet.jsdbg-vscode");
	assert.ok(extension, "jsdbg prototype extension is installed");
	const api = await extension.activate();
	await api.ready;

	const snapshot = api.getSnapshot();
	assert.ok(snapshot, "workspace context was created");
	assert.equal(snapshot.id, api.contextId);
	assert.match(api.contextId, /^vscode-[0-9a-f]{16}$/);

	await launchAndAssert(api, {
		type: "jsdbg",
		request: "launch",
		name: "jsdbg Node.js integration",
		runtime: "node",
		program: "${workspaceFolder}/node-app.js",
	});
	await launchAndAssert(api, {
		type: "jsdbg",
		request: "launch",
		name: "jsdbg Playwright integration",
		runtime: "playwright",
		url: vscode.Uri.joinPath(workspaceFolder(), "index.html").toString(),
		headless: true,
	});
	const chrome = process.env.JSDBG_TEST_CHROME;
	assert.ok(chrome, "integration test received a Chrome executable path");
	await launchAndAssert(api, {
		type: "jsdbg",
		request: "launch",
		name: "jsdbg Chrome integration",
		runtime: "chrome",
		url: vscode.Uri.joinPath(workspaceFolder(), "index.html").toString(),
		executablePath: chrome,
		headless: true,
		args: ["--no-sandbox"],
	});
}

async function launchAndAssert(
	api: JsdbgExtensionApi,
	configuration: vscode.DebugConfiguration,
): Promise<void> {
	const before = new Set(api.getSnapshot()?.connections.map((connection) => connection.id));
	const started = waitForDebugSession(configuration.name);
	const launched = await vscode.debug.startDebugging(undefined, configuration);
	assert.equal(launched, true);
	const session = await started;
	const threads = await session.customRequest("threads") as unknown;
	assertThreadResponse(threads);
	await session.customRequest("disconnect", { terminateDebuggee: true });
	await vscode.debug.stopDebugging(session);
	await waitForConnectionCleanup(api, before);
}

function waitForDebugSession(name: string): Promise<vscode.DebugSession> {
	return new Promise((resolve, reject) => {
		const timeout = setTimeout(() => {
			subscription.dispose();
			reject(new Error("Timed out waiting for jsdbg debug session"));
		}, 60_000);
		const subscription = vscode.debug.onDidStartDebugSession((session) => {
			if (session.type === "jsdbg" && session.name === name) {
				clearTimeout(timeout);
				subscription.dispose();
				resolve(session);
			}
		});
	});
}

async function waitForConnectionCleanup(
	api: JsdbgExtensionApi,
	expected: ReadonlySet<string | undefined>,
): Promise<void> {
	const deadline = Date.now() + 15_000;
	while (Date.now() < deadline) {
		await api.refresh();
		const current = api.getSnapshot()?.connections.map((connection) => connection.id) ?? [];
		if (current.every((connection) => expected.has(connection))) {
			return;
		}
		await delay(50);
	}
	throw new Error(
		`Timed out waiting for launched jsdbg connection cleanup; remaining: ${
			(api.getSnapshot()?.connections.map((connection) =>
				`${connection.id}:${connection.status.kind}`) ?? []).join(", ")
		}`,
	);
}

function assertThreadResponse(value: unknown): void {
	assert.ok(typeof value === "object" && value !== null);
	assert.ok("threads" in value && Array.isArray(value.threads));
	assert.ok(value.threads.length > 0, "attached target is exposed as a DAP thread");
}

function delay(milliseconds: number): Promise<void> {
	return new Promise((resolve) => setTimeout(resolve, milliseconds));
}

function workspaceFolder(): vscode.Uri {
	const folder = vscode.workspace.workspaceFolders?.[0];
	assert.ok(folder, "integration workspace folder is present");
	return folder.uri;
}
