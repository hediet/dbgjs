import assert from "node:assert/strict";
import * as vscode from "vscode";
import type { TargetSnapshot } from "../../apiTypes.js";
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

	const nodeTargets = await launchAndAssert(
		api,
		{
			type: "jsdbg",
			request: "launch",
			name: "jsdbg Node.js integration",
			runtime: "node",
			program: "${workspaceFolder}/node-app.js",
		},
		2,
	);
	const childTarget = nodeTargets.find(
		(target) => target.subtype === "child-process",
	);
	assert.ok(childTarget, "spawned Node.js process is modeled as a target");
	assert.equal(childTarget.parentId, "$node-root");
	assert.equal(childTarget.attached, true);
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
	minimumThreads = 1,
): Promise<readonly TargetSnapshot[]> {
	const before = new Set(api.getSnapshot()?.connections.map((connection) => connection.id));
	const started = waitForDebugSession(configuration.name);
	const launched = await vscode.debug.startDebugging(undefined, configuration);
	assert.equal(launched, true);
	const session = await started;
	await waitForThreadCount(session, minimumThreads, api);
	const targets = api.getSnapshot()?.connections
		.filter((connection) => !before.has(connection.id))
		.flatMap((connection) => connection.targets) ?? [];
	await session.customRequest("disconnect", { terminateDebuggee: true });
	await vscode.debug.stopDebugging(session);
	await waitForConnectionCleanup(api, before);
	return targets;
}

async function waitForThreadCount(
	session: vscode.DebugSession,
	minimumThreads: number,
	api: JsdbgExtensionApi,
): Promise<void> {
	const deadline = Date.now() + 30_000;
	let last: unknown;
	while (Date.now() < deadline) {
		last = await session.customRequest("threads") as unknown;
		if (threadCount(last) >= minimumThreads) {
			return;
		}
		await delay(50);
	}
	assertThreadResponse(last, minimumThreads, api);
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

function assertThreadResponse(
	value: unknown,
	minimumThreads = 1,
	api?: JsdbgExtensionApi,
): void {
	assert.ok(typeof value === "object" && value !== null);
	assert.ok("threads" in value && Array.isArray(value.threads));
	assert.ok(
		value.threads.length >= minimumThreads,
		`expected at least ${minimumThreads} attached DAP threads; targets: ${
			JSON.stringify(api?.getSnapshot()?.connections.flatMap((connection) =>
				connection.targets.map((target) => ({
					id: target.targetId,
					parent: target.parentId,
					attached: target.attached,
					subtype: target.subtype,
				}))) ?? [])
		}`,
	);
}

function threadCount(value: unknown): number {
	if (typeof value !== "object"
		|| value === null
		|| !("threads" in value)
		|| !Array.isArray(value.threads)) {
		return 0;
	}
	return value.threads.length;
}

function delay(milliseconds: number): Promise<void> {
	return new Promise((resolve) => setTimeout(resolve, milliseconds));
}

function workspaceFolder(): vscode.Uri {
	const folder = vscode.workspace.workspaceFolders?.[0];
	assert.ok(folder, "integration workspace folder is present");
	return folder.uri;
}
