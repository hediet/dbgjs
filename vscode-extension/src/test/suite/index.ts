import assert from "node:assert/strict";
import * as vscode from "vscode";
import type { TargetSnapshot } from "../../apiTypes.js";
import type { JsdbgExtensionApi } from "../../extension.js";

export async function run(): Promise<void> {
	const extension = vscode.extensions.getExtension<JsdbgExtensionApi>("hediet.jsdbg-vscode");
	assert.ok(extension, "jsdbg prototype extension is installed");
	await waitForActivation(extension);
	const api = extension.exports;
	assert.ok(api, "automatically activated jsdbg extension exported its API");
	await api.ready;

	const snapshot = api.getSnapshot();
	assert.ok(snapshot, "workspace context was created");
	assert.equal(snapshot.id, api.contextId);
	assert.match(api.contextId, /^vscode-[0-9a-f]{16}$/);

	const nodeSource = vscode.Uri.joinPath(workspaceFolder(), "node-app.js");
	const nodeBreakpoint = new vscode.SourceBreakpoint(
		new vscode.Location(nodeSource, new vscode.Position(13, 0)),
	);
	vscode.debug.addBreakpoints([nodeBreakpoint]);
	let nodeTargets: readonly TargetSnapshot[];
	try {
		nodeTargets = await launchAndAssert(
			api,
			{
				type: "jsdbg",
				request: "launch",
				name: "jsdbg Node.js integration",
				runtime: "node",
				program: "${workspaceFolder}/node-app.js",
			},
			1,
			async (session, startedSessions) => {
				await waitForStoppedLine(session, nodeSource, 14);
				await assertLoadedSource(session, nodeSource);
				await assertSingleThread(session, api);
				await assertVariablesAndWatch(session);
				const childSession = await waitForChildSession(
					session,
					startedSessions,
					api,
				);
				await waitForThreadCount(childSession, 1, api);
				await assertSingleThread(childSession, api);
				assert.equal(childSession.configuration.runtime, "target");
				assert.notEqual(
					childSession.configuration.targetId,
					session.configuration.targetId,
				);
				const evaluation = await session.customRequest("evaluate", {
					expression: "process.pid",
					context: "repl",
				}) as { result?: unknown };
				assert.match(String(evaluation.result), /^\d+$/);
			},
		);
	} finally {
		vscode.debug.removeBreakpoints([nodeBreakpoint]);
	}
	if (process.env.JSDBG_TEST_NODE_ONLY === "1") {
		return;
	}
	const childTarget = nodeTargets.find(
		(target) => target.subtype === "child-process",
	);
	assert.ok(childTarget, "spawned Node.js process is modeled as a target");
	assert.equal(childTarget.parentId, "$node-root");
	assert.equal(childTarget.attached, true);
	await launchAndAssert(api, {
		type: "jsdbg",
		request: "launch",
		name: "jsdbg compiled TypeScript integration",
		runtime: "node",
		program: "${workspaceFolder}/dist/tsc-app.js",
	});
	await launchAndAssert(api, {
		type: "jsdbg",
		request: "launch",
		name: "jsdbg tsx TypeScript integration",
		runtime: "node",
		runtimeArgs: ["--import", "tsx"],
		program: "${workspaceFolder}/tsx-app.ts",
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

async function assertVariablesAndWatch(session: vscode.DebugSession): Promise<void> {
	const threads = await session.customRequest("threads") as {
		threads: readonly { id: number }[];
	};
	assert.equal(threads.threads.length, 1);
	const thread = threads.threads[0];
	assert.ok(thread);
	const stack = await session.customRequest("stackTrace", {
		threadId: thread.id,
	}) as { stackFrames: readonly { id: number }[] };
	const frame = stack.stackFrames[0];
	assert.ok(frame, "paused target exposes a stack frame");
	const scopes = await session.customRequest("scopes", {
		frameId: frame.id,
	}) as {
		scopes: readonly { name: string; variablesReference: number }[];
	};
	assert.ok(scopes.scopes.length > 0, "paused frame exposes scopes");
	const local = scopes.scopes.find((scope) => scope.name === "Local");
	assert.ok(local, "paused frame exposes its local scope");
	const locals = await session.customRequest("variables", {
		variablesReference: local.variablesReference,
	}) as {
		variables: readonly { name: string; value: string; variablesReference: number }[];
	};
	const state = locals.variables.find((variable) => variable.name === "state");
	assert.ok(state, "local scope includes state");
	assert.ok(state.variablesReference > 0, "local object is expandable");

	const evaluation = await session.customRequest("evaluate", {
		expression: "state",
		frameId: frame.id,
		context: "watch",
	}) as { variablesReference: number };
	assert.ok(evaluation.variablesReference > 0, "Watch object is expandable");
	const stateProperties = await session.customRequest("variables", {
		variablesReference: evaluation.variablesReference,
	}) as {
		variables: readonly { name: string; value: string; variablesReference: number }[];
	};
	assert.equal(
		stateProperties.variables.find((variable) => variable.name === "counter")?.value,
		"1",
	);
	const nested = stateProperties.variables.find((variable) => variable.name === "nested");
	assert.ok(nested && nested.variablesReference > 0, "nested Watch property is expandable");
	const nestedProperties = await session.customRequest("variables", {
		variablesReference: nested.variablesReference,
	}) as {
		variables: readonly { name: string; value: string }[];
	};
	assert.equal(
		nestedProperties.variables.find((variable) => variable.name === "label")?.value,
		"watch",
	);
}

async function waitForActivation(
	extension: vscode.Extension<JsdbgExtensionApi>,
): Promise<void> {
	const deadline = Date.now() + 10_000;
	while (!extension.isActive && Date.now() < deadline) {
		await delay(50);
	}
	assert.equal(
		extension.isActive,
		true,
		"jsdbg extension activated through its manifest activation events",
	);
}

async function launchAndAssert(
	api: JsdbgExtensionApi,
	configuration: vscode.DebugConfiguration,
	minimumThreads = 1,
	verify?: (
		session: vscode.DebugSession,
		startedSessions: readonly vscode.DebugSession[],
	) => Promise<void>,
): Promise<readonly TargetSnapshot[]> {
	const before = new Set(api.getSnapshot()?.connections.map((connection) => connection.id));
	const startedSessions: vscode.DebugSession[] = [];
	const sessionSubscription = vscode.debug.onDidStartDebugSession((session) => {
		if (session.type === "jsdbg") {
			startedSessions.push(session);
		}
	});
	const started = waitForDebugSession(configuration.name);
	let session: vscode.DebugSession | undefined;
	try {
		const launched = await vscode.debug.startDebugging(undefined, configuration);
		assert.equal(launched, true);
		session = await started;
		await waitForThreadCount(session, minimumThreads, api);
		await verify?.(session, startedSessions);
		return api.getSnapshot()?.connections
			.filter((connection) => !before.has(connection.id))
			.flatMap((connection) => connection.targets) ?? [];
	} finally {
		sessionSubscription.dispose();
		if (session !== undefined) {
			await vscode.debug.stopDebugging(session);
		}
		await waitForConnectionCleanup(api, before);
	}
}

async function waitForChildSession(
	parent: vscode.DebugSession,
	startedSessions: readonly vscode.DebugSession[],
	api: JsdbgExtensionApi,
): Promise<vscode.DebugSession> {
	const deadline = Date.now() + 30_000;
	while (Date.now() < deadline) {
		const child = startedSessions.find(
			(session) => session.parentSession?.id === parent.id,
		);
		if (child !== undefined) {
			return child;
		}
		await delay(50);
	}
	throw new Error(
		`Timed out waiting for a child session of ${parent.name}; sessions=${
			JSON.stringify(startedSessions.map((session) => ({
				id: session.id,
				name: session.name,
				parentId: session.parentSession?.id,
				configuration: session.configuration,
			})))
		}; forest=${JSON.stringify(api.getSnapshot()?.targetForest)}`,
	);
}

async function assertSingleThread(
	session: vscode.DebugSession,
	api: JsdbgExtensionApi,
): Promise<void> {
	const response = await session.customRequest("threads") as unknown;
	assertThreadResponse(response, 1, api);
	assert.equal(
		threadCount(response),
		1,
		`${session.name} must represent exactly one jsdbg target`,
	);
}

async function waitForStoppedLine(
	session: vscode.DebugSession,
	source: vscode.Uri,
	line: number,
): Promise<void> {
	const deadline = Date.now() + 30_000;
	while (Date.now() < deadline) {
		const threads = await session.customRequest("threads") as {
			threads?: Array<{ id?: unknown }>;
		};
		for (const thread of threads.threads ?? []) {
			if (typeof thread.id !== "number") {
				continue;
			}
			const trace = await session.customRequest("stackTrace", {
				threadId: thread.id,
				startFrame: 0,
				levels: 1,
			}) as {
				stackFrames?: Array<{ line?: unknown; source?: { path?: unknown } }>;
			};
			const frame = trace.stackFrames?.[0];
			if (frame?.line === line && samePath(frame.source?.path, source.fsPath)) {
				return;
			}
		}
		await delay(50);
	}
	throw new Error(`Timed out waiting for ${source.fsPath}:${line}`);
}

async function assertLoadedSource(
	session: vscode.DebugSession,
	source: vscode.Uri,
): Promise<void> {
	const response = await session.customRequest("loadedSources") as {
		sources?: Array<{ path?: unknown }>;
	};
	assert.ok(
		response.sources?.some((candidate) => samePath(candidate.path, source.fsPath)),
		`Loaded Scripts does not contain ${source.fsPath}: ${JSON.stringify(response.sources)}`,
	);
}

function samePath(left: unknown, right: string): boolean {
	return typeof left === "string"
		&& left.replaceAll("\\", "/").toLowerCase() === right.replaceAll("\\", "/").toLowerCase();
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
