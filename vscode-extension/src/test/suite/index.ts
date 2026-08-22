import assert from "node:assert/strict";
import { execFile } from "node:child_process";
import { promisify } from "node:util";
import * as vscode from "vscode";
import type { JsdbgExtensionApi } from "../../extension.js";

const execFileAsync = promisify(execFile);

export async function run(): Promise<void> {
	const extension = vscode.extensions.getExtension<JsdbgExtensionApi>("hediet.jsdbg-vscode");
	assert.ok(extension, "jsdbg prototype extension is installed");
	const api = await extension.activate();
	await api.ready;

	const snapshot = api.getSnapshot();
	assert.ok(snapshot, "workspace context was created");
	assert.equal(snapshot.id, api.contextId);
	assert.match(api.contextId, /^vscode-[0-9a-f]{16}$/);

	await vscode.commands.executeCommand("jsdbg.refreshTargets");
	const cli = process.env.JSDBG_TEST_CLI;
	assert.ok(cli, "integration test received the jsdbg CLI path");
	await execFileAsync(cli, [
		"connection",
		"add",
		api.contextId,
		"browser",
		"--playwright",
		vscode.Uri.joinPath(
			vscode.workspace.workspaceFolders?.[0]?.uri
				?? fail("integration workspace folder is missing"),
			"index.html",
		).toString(),
		"--connect",
	]);
	const targetId = await waitForTarget(api);
	await execFileAsync(cli, [
		"target",
		"attach",
		api.contextId,
		"browser",
		targetId,
	]);
	await waitForAttachedTarget(api, targetId);

	const started = waitForDebugSession();
	const launched = await vscode.debug.startDebugging(undefined, {
		type: "jsdbg",
		request: "launch",
		name: "jsdbg extension integration",
	});
	assert.equal(launched, true);
	const session = await started;
	assert.equal(session.type, "jsdbg");
	const threads = await session.customRequest("threads") as unknown;
	assertThreadResponse(threads);
	await vscode.debug.stopDebugging(session);
}

async function waitForTarget(api: JsdbgExtensionApi): Promise<string> {
	const deadline = Date.now() + 30_000;
	while (Date.now() < deadline) {
		await api.refresh();
		const target = api.getSnapshot()?.connections
			.find((connection) => connection.id === "browser")
			?.targets.find((candidate) => candidate.targetType === "page");
		if (target !== undefined) {
			return target.targetId;
		}
		await delay(100);
	}
	throw new Error("Timed out waiting for Playwright page target");
}

async function waitForAttachedTarget(
	api: JsdbgExtensionApi,
	targetId: string,
): Promise<void> {
	const deadline = Date.now() + 30_000;
	while (Date.now() < deadline) {
		await api.refresh();
		const target = api.getSnapshot()?.connections
			.find((connection) => connection.id === "browser")
			?.targets.find((candidate) => candidate.targetId === targetId);
		if (target?.attached === true) {
			return;
		}
		await delay(100);
	}
	throw new Error("Timed out waiting for attached Playwright target");
}

function waitForDebugSession(): Promise<vscode.DebugSession> {
	return new Promise((resolve, reject) => {
		const timeout = setTimeout(() => {
			subscription.dispose();
			reject(new Error("Timed out waiting for jsdbg debug session"));
		}, 10_000);
		const subscription = vscode.debug.onDidStartDebugSession((session) => {
			if (session.type === "jsdbg") {
				clearTimeout(timeout);
				subscription.dispose();
				resolve(session);
			}
		});
	});
}

function assertThreadResponse(value: unknown): void {
	assert.ok(typeof value === "object" && value !== null);
	assert.ok("threads" in value && Array.isArray(value.threads));
	assert.ok(value.threads.length > 0, "attached target is exposed as a DAP thread");
}

function delay(milliseconds: number): Promise<void> {
	return new Promise((resolve) => setTimeout(resolve, milliseconds));
}

function fail(message: string): never {
	throw new Error(message);
}
