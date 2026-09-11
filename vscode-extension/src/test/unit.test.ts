import assert from "node:assert/strict";
import { randomUUID } from "node:crypto";
import { chmod, mkdir, mkdtemp, rm, writeFile } from "node:fs/promises";
import { createServer, type Socket } from "node:net";
import { tmpdir } from "node:os";
import test from "node:test";
import { join, resolve } from "node:path";
import { pathToFileURL } from "node:url";
import { parseObservationResult, parseTargetDebuggerSnapshot } from "../apiTypes.js";
import { DaemonClient, defaultServiceStateFile, parseEndpointFile } from "../daemonClient.js";
import { resolveDaemonExecutable } from "../daemonProcess.js";
import { findInstalledChrome, parseLaunch, resolveLaunch } from "../launchConfig.js";
import {
	breakpointId,
	findTargetNode,
	normalizeContextPath,
	targetKey,
	targetReference,
} from "../model.js";

test("daemon state uses the dbgjs namespace on every platform", () => {
	assert.equal(defaultServiceStateFile({
		DBGJS_SERVICE_STATE: "custom-service.json",
		LOCALAPPDATA: "local",
	}), "custom-service.json");
	assert.equal(defaultServiceStateFile({ LOCALAPPDATA: "local" }),
		join("local", "hediet", "dbgjs", "service.json"));
	assert.equal(defaultServiceStateFile({ XDG_RUNTIME_DIR: "runtime" }),
		join("runtime", "hediet-dbgjs", "service.json"));
	assert.equal(defaultServiceStateFile({ HOME: "home" }),
		join("home", ".cache", "hediet", "dbgjs", "service.json"));
	assert.equal(defaultServiceStateFile({}),
		join(tmpdir(), `hediet-dbgjs-${process.pid}`, "service.json"));
});

test("workspace context identity uses lexical lowercase absolute paths", () => {
	assert.equal(normalizeContextPath("/Work/Shop/../Store"), "/work/store");
	assert.equal(normalizeContextPath("D:\\Src\\Shop\\..\\Store"), "d:\\src\\store");
	assert.equal(
		normalizeContextPath("\\\\Server\\Share\\Shop\\..\\Store"),
		"\\\\server\\share\\store",
	);
	assert.equal(normalizeContextPath("\\\\Server\\Share"), "\\\\server\\share");
	assert.throws(() => normalizeContextPath("D:relative"), /Drive-relative/);
});

test("empty context observations represent idle long-poll timeouts", () => {
	assert.deepEqual(parseObservationResult({ kind: "items", items: [] }), {});
});

test("unit enum states parse from their daemon string representation", () => {
	const observation = parseObservationResult({
		kind: "historyGap",
		current: {
			agentInstanceId: "agent",
			id: "workspace",
			displayName: "Workspace",
			revision: 2,
			connections: [],
			targetForest: [],
			breakpoints: [{
				id: "breakpoint",
				sourcePath: "file:///workspace/app.js",
				line: 1,
				column: 1,
				status: "pending",
				enabled: true,
			}, {
				id: "bound-breakpoint",
				sourcePath: "file:///workspace/app.js",
				line: 2,
				column: 1,
				status: { bound: { application_count: 1 } },
				enabled: true,
			}],
		},
	});
	assert.equal(observation.snapshot?.breakpoints[0]?.status.kind, "pending");
	assert.deepEqual(observation.snapshot?.breakpoints[1]?.status, {
		kind: "bound",
		application_count: 1,
	});
});

	test("context snapshots preserve target forest edges and generations", () => {
		const target = (targetId: string) => ({
			targetId,
			targetType: "node",
			title: targetId,
			url: "",
			attached: true,
			parentId: null,
			openerId: null,
			browserContextId: null,
			subtype: null,
		});
		const snapshot = parseObservationResult({
			kind: "historyGap",
			requestedRevision: 0,
			oldestAvailableRevision: 1,
			current: {
				agentInstanceId: "agent",
				id: "workspace",
				displayName: "Workspace",
				revision: 2,
				connections: [],
				targetForest: [{
					connectionId: "node",
					connectionGeneration: 3,
					target: target("parent"),
					parentTargetId: null,
				}, {
					connectionId: "node",
					connectionGeneration: 3,
					target: target("child"),
					parentTargetId: "parent",
				}],
				breakpoints: [],
			},
		}).snapshot;

		assert.ok(snapshot);
		const parent = snapshot.targetForest[0];
		const child = snapshot.targetForest[1];
		assert.ok(parent);
		assert.ok(child);
		assert.equal(
			targetKey(targetReference("workspace", child)),
			"workspace\u0000node\u00003\u0000child",
		);
		assert.equal(
			findTargetNode(snapshot.targetForest, targetReference("workspace", child)),
			child,
		);
		assert.equal(findTargetNode(snapshot.targetForest, {
			...targetReference("workspace", child),
			connectionGeneration: 4,
		}), undefined);
});

test("target snapshots preserve ordered console messages", () => {
	const snapshot = parseTargetDebuggerSnapshot({
		contextId: "workspace",
		connectionId: "node",
		targetId: "process",
		connectionGeneration: 1,
		revision: 2,
		phase: { kind: "running" },
		scripts: [],
		breakpoints: [],
		logs: [
			{ index: 1, values: ["hello", "42"] },
			{ index: 2, values: ["done"] },
		],
		pause: null,
	});
	assert.deepEqual(snapshot.logs, [
		{ index: 1, values: ["hello", "42"] },
		{ index: 2, values: ["done"] },
	]);
});

test("Node.js launch configurations preserve runtime and program arguments", () => {
	assert.deepEqual(parseLaunch({
		runtime: "node",
		program: "/workspace/server.js",
		args: ["--port", "3000"],
		runtimeArgs: ["--enable-source-maps"],
		env: { NODE_ENV: "test", OMITTED: null },
	}), {
		runtime: "node",
		program: "/workspace/server.js",
		args: ["--port", "3000"],
		runtimeArgs: ["--enable-source-maps"],
		env: { NODE_ENV: "test", OMITTED: null },
	});
});

test("Chrome launch configurations normalize absolute paths to file URLs", async () => {
	const page = resolve("workspace", "index.html");
	const launch = await resolveLaunch({
		runtime: "chrome",
		url: page,
		executablePath: process.execPath,
	}, "session");
	assert.equal(
		launch.configuration?.kind === "chrome" ? launch.configuration.url : undefined,
		pathToFileURL(page).toString(),
	);
});

test("DAP breakpoint IDs are stable daemon identifiers", () => {
	const first = breakpointId("d:\\workspace\\app.js", 3, 1);
	const second = breakpointId("d:\\workspace\\app.js", 3, 1);
	assert.equal(first, second);
	assert.match(first, /^[A-Za-z0-9_.-]+$/);
	assert.notEqual(first, breakpointId("d:\\workspace\\app.js", 4, 1));
});

test("installed Chrome discovery searches PATH", async () => {
	const directory = await mkdtemp(join(process.cwd(), ".dbgjs-chrome-test-"));
	const executable = join(directory, "google-chrome");
	try {
		await writeFile(executable, "#!/bin/sh\nexit 0\n");
		await chmod(executable, 0o755);
		assert.equal(
			await findInstalledChrome("linux", { PATH: directory }),
			executable,
		);
	} finally {
		await rm(directory, { recursive: true, force: true });
	}
});

test("daemon discovery prefers the extension's bundled executable", async () => {
	const directory = await mkdtemp(join(process.cwd(), ".dbgjs-daemon-test-"));
	const executable = join(
		directory,
		"bin",
		process.platform === "win32" ? "dbgjs-service.exe" : "dbgjs-service",
	);
	try {
		await mkdir(join(directory, "bin"));
		await writeFile(executable, "");
		assert.equal(
			await resolveDaemonExecutable({
				extensionPath: directory,
				environment: {},
			}),
			executable,
		);
	} finally {
		await rm(directory, { recursive: true, force: true });
	}
});

test("daemon endpoint parsing accepts the Rust named-pipe shape", () => {
	assert.deepEqual(
		parseEndpointFile({
			process_id: 42,
			transport: {
				kind: "namedPipe",
				pipe_name: "\\\\.\\pipe\\dbgjs-test",
			},
			token: "test-token",
		}),
		{
			address: "\\\\.\\pipe\\dbgjs-test",
			token: "test-token",
		},
	);
});

test("long polls do not block command RPCs", async () => {
	const directory = await mkdtemp(join(tmpdir(), "dbgjs-rpc-test-"));
	const stateFile = join(directory, "service.json");
	const endpoint = process.platform === "win32"
		? `\\\\.\\pipe\\dbgjs-test-${randomUUID()}`
		: join(directory, "service.sock");
	const sockets = new Set<Socket>();
	let observationReceivedResolve!: () => void;
	const observationReceived = new Promise<void>((resolve) => {
		observationReceivedResolve = resolve;
	});
	let releaseObservation!: () => void;
	const observationRelease = new Promise<void>((resolve) => {
		releaseObservation = resolve;
	});
	const server = createServer((socket) => {
		sockets.add(socket);
		socket.setEncoding("utf8");
		socket.on("close", () => sockets.delete(socket));
		let buffer = "";
		let authenticated = false;
		socket.on("data", (chunk: string) => {
			buffer += chunk;
			for (;;) {
				const newline = buffer.indexOf("\n");
				if (newline < 0) {
					break;
				}
				const line = buffer.slice(0, newline).trim();
				buffer = buffer.slice(newline + 1);
				if (!line) {
					continue;
				}
				const message = JSON.parse(line) as {
					hello?: number;
					token?: string;
					id?: number;
					method?: string;
				};
				if (!authenticated) {
					assert.deepEqual(message, { hello: 1, token: "test-token" });
					authenticated = true;
					continue;
				}
				assert.equal(typeof message.id, "number");
				if (message.method?.endsWith("::observe_context")) {
					observationReceivedResolve();
					void observationRelease.then(() => {
						sendResult(socket, message.id!, { kind: "items", items: [] });
					});
				} else if (message.method?.endsWith("::list_sources")) {
					sendResult(socket, message.id!, []);
				} else {
					sendError(socket, message.id!, `Unexpected method ${message.method}`);
				}
			}
		});
	});
	await new Promise<void>((resolve, reject) => {
		server.once("error", reject);
		server.listen(endpoint, resolve);
	});
	await writeFile(stateFile, JSON.stringify({
		process_id: process.pid,
		transport: { kind: "test", path: endpoint },
		token: "test-token",
	}));

	const client = await DaemonClient.connect(stateFile);
	try {
		const observation = client.observeContext("workspace", 1, 30_000);
		await observationReceived;
		assert.deepEqual(
			await withTimeout(client.listSources("workspace"), 1_000),
			[],
		);
		releaseObservation();
		assert.equal(await observation, undefined);
	} finally {
		client.close();
		for (const socket of sockets) {
			socket.destroy();
		}
		await new Promise<void>((resolve) => server.close(() => resolve()));
		await rm(directory, { recursive: true, force: true });
	}
});

function target(
	targetId: string,
	targetType: string,
	relations: { parentId?: string; openerId?: string } = {},
) {
	return {
		targetId,
		targetType,
		title: targetId,
		url: `https://example.test/${targetId}`,
		attached: true,
		...relations,
	};
}

function sendResult(socket: Socket, id: number, result: unknown): void {
	socket.write(`${JSON.stringify({ jsonrpc: "2.0", id, result })}\n`);
}

function sendError(socket: Socket, id: number, message: string): void {
	socket.write(`${JSON.stringify({
		jsonrpc: "2.0",
		id,
		error: { code: -32601, message },
	})}\n`);
}

async function withTimeout<T>(promise: Promise<T>, timeoutMs: number): Promise<T> {
	let timer: NodeJS.Timeout | undefined;
	try {
		return await Promise.race([
			promise,
			new Promise<never>((_, reject) => {
				timer = setTimeout(
					() => reject(new Error(`Timed out after ${timeoutMs} ms`)),
					timeoutMs,
				);
			}),
		]);
	} finally {
		if (timer !== undefined) {
			clearTimeout(timer);
		}
	}
}
