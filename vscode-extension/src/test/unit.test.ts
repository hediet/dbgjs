import assert from "node:assert/strict";
import { randomUUID } from "node:crypto";
import { chmod, mkdir, mkdtemp, rm, writeFile } from "node:fs/promises";
import { createServer, type Socket } from "node:net";
import { tmpdir } from "node:os";
import test from "node:test";
import { join, resolve } from "node:path";
import { pathToFileURL } from "node:url";
import { parse } from "zod/v4/core";
import {
	breakpointStatusKind,
	observationSnapshot,
} from "../apiTypes.js";
import { DaemonClient, defaultServiceStateFile, parseEndpointFile } from "../daemonClient.js";
import { resolveDaemonExecutable } from "../daemonProcess.js";
import { connectDaemon } from "../daemonTransport.js";
import { DbgServiceClient } from "../dbgServiceClient.js";
import { ContextApi } from "../generated/contextApi.js";
import { TargetDebuggerApi } from "../generated/targetDebuggerApi.js";
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
		join("local", "dbgjs", "service.json"));
	assert.equal(defaultServiceStateFile({ XDG_RUNTIME_DIR: "runtime" }),
		join("runtime", "dbgjs", "service.json"));
	assert.equal(defaultServiceStateFile({ HOME: "home" }),
		join("home", ".cache", "dbgjs", "service.json"));
	assert.equal(defaultServiceStateFile({}),
		join(tmpdir(), `dbgjs-${process.pid}`, "service.json"));
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
	const result = parse(ContextApi.members.observe_context.resultSchema, {
		kind: "items",
		items: [],
	});
	assert.equal(observationSnapshot(result), undefined);
});

test("generated schemas validate Rust enum wire representations", () => {
	const observation = parse(ContextApi.members.observe_context.resultSchema, {
		kind: "historyGap",
		requested_revision: 0,
		oldest_available_revision: 1,
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
				condition: null,
				targetSelector: null,
			}, {
				id: "bound-breakpoint",
				sourcePath: "file:///workspace/app.js",
				line: 2,
				column: 1,
				status: { bound: { application_count: 1 } },
				enabled: true,
				condition: null,
				targetSelector: null,
			}],
		},
	});
	const snapshot = observationSnapshot(observation);
	assert.equal(
		snapshot?.breakpoints[0] === undefined
			? undefined
			: breakpointStatusKind(snapshot.breakpoints[0].status),
		"pending",
	);
	assert.deepEqual(snapshot?.breakpoints[1]?.status, {
		bound: { application_count: 1 },
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
		const snapshot = observationSnapshot(
			parse(ContextApi.members.observe_context.resultSchema, {
			kind: "historyGap",
			requested_revision: 0,
			oldest_available_revision: 1,
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
					attachment: "detached",
				}, {
					connectionId: "node",
					connectionGeneration: 3,
					target: target("child"),
					parentTargetId: "parent",
					attachment: "detached",
				}],
				breakpoints: [],
			},
		}));

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
	const snapshot = parse(TargetDebuggerApi.members.get_target.resultSchema, {
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

test("debug service facets route through one authenticated connection", async () => {
	const directory = await mkdtemp(join(tmpdir(), "dbgjs-facets-"));
	const endpoint = testEndpoint(directory);
	const sockets = new Set<Socket>();
	const requests: { method: string; params: unknown }[] = [];
	let connections = 0;
	let preambles = 0;
	const server = createServer((socket) => {
		connections++;
		sockets.add(socket);
		socket.setEncoding("utf8");
		socket.on("close", () => sockets.delete(socket));
		let buffer = "";
		socket.on("data", (chunk: string) => {
			buffer += chunk;
			for (;;) {
				const newline = buffer.indexOf("\n");
				if (newline < 0) {
					return;
				}
				const message = JSON.parse(buffer.slice(0, newline));
				buffer = buffer.slice(newline + 1);
				if ("hello" in message) {
					assert.deepEqual(message, { hello: 1, token: "facet-token" });
					preambles++;
					continue;
				}
				requests.push({ method: message.method, params: message.params });
				sendError(socket, message.id, `Routed ${message.method}`);
			}
		});
	});
	await listen(server, endpoint);
	const daemon = await connectDaemon(endpoint, "facet-token");
	try {
		const client = new DbgServiceClient(daemon.connection);
		const scope = { contextId: "workspace", connectionId: "node", targetId: "process" };
		const coverage = { ...scope, captureId: "capture", noCache: false, sourcePath: null };
		const cpu = { ...coverage, project: false };
		const cdp = { ...scope, method: "Runtime.enable", params: {}, validate: true };
		const cases = [
			["dev.dbgjs.cdp-debugger::service_info", {}, () => client.service.service_info({})],
			["dev.dbgjs.context::list_contexts", { cwd: null }, () => client.contexts.list_contexts({ cwd: null })],
			["dev.dbgjs.source::list_sources", { contextId: scope.contextId, path: null },
				() => client.sources.list_sources({ contextId: scope.contextId, path: null })],
			["dev.dbgjs.capture::list_captures", { contextId: scope.contextId },
				() => client.captures.list_captures({ contextId: scope.contextId })],
			["dev.dbgjs.target-debugger::get_target", scope, () => client.targets.get_target(scope)],
			["dev.dbgjs.cdp-access::raw_cdp_request", cdp, () => client.cdp.raw_cdp_request(cdp)],
			["dev.dbgjs.relay::close_relay", { relayId: "relay" }, () => client.relay.close_relay({ relayId: "relay" })],
			["dev.dbgjs.browser-automation::type_target", { ...scope, text: "hello" },
				() => client.browser.type_target({ ...scope, text: "hello" })],
			["dev.dbgjs.coverage::get_coverage", coverage, () => client.coverage.get_coverage(coverage)],
			["dev.dbgjs.cpu-profiler::get_cpu_profile", cpu, () => client.cpu.get_cpu_profile(cpu)],
			["dev.dbgjs.heap-profiler::get_heap_snapshot_progress", scope,
				() => client.heap.get_heap_snapshot_progress(scope)],
		] as const;
		for (const [method, , call] of cases) {
			await assert.rejects(async () => await call(), { message: `Routed ${method}` });
		}
		assert.deepEqual(requests, cases.map(([method, params]) => ({ method, params })));
		assert.equal(connections, 1);
		assert.equal(preambles, 1);
	} finally {
		daemon.close();
		await closeServer(server, sockets);
		await rm(directory, { recursive: true, force: true });
	}
});

test("daemon transport preserves the preamble and handles fragmented and coalesced responses", async () => {
	const directory = await mkdtemp(join(tmpdir(), "dbgjs-transport-test-"));
	const endpoint = testEndpoint(directory);
	const sockets = new Set<Socket>();
	const requests: unknown[] = [];
	const logs: string[] = [];
	const server = createServer((socket) => {
		sockets.add(socket);
		socket.setEncoding("utf8");
		socket.on("close", () => sockets.delete(socket));
		let buffer = "";
		socket.on("data", (chunk: string) => {
			buffer += chunk;
			for (;;) {
				const newline = buffer.indexOf("\n");
				if (newline < 0) {
					return;
				}
				const line = buffer.slice(0, newline);
				buffer = buffer.slice(newline + 1);
				requests.push(JSON.parse(line));
				if (requests.length === 3) {
					const first = requests[1] as { id: number };
					const second = requests[2] as { id: number };
					const response = [
						JSON.stringify({ jsonrpc: "2.0", id: first.id, result: { value: 1 } }),
						"malformed-json-is-skipped",
						JSON.stringify({ jsonrpc: "2.0", id: second.id, result: { value: 2 } }),
						"",
					].join("\n");
					socket.write(response.slice(0, 7));
					setTimeout(() => socket.write(response.slice(7)), 5);
				}
			}
		});
	});
	await listen(server, endpoint);

	const daemon = await connectDaemon(endpoint, "secret-token", (message) => logs.push(message));
	try {
		assert.deepEqual(
			await Promise.all([
				daemon.connection.channel.sendRequest(
					"dev.dbgjs.cdp-debugger::first",
					{ contextId: "workspace", cursor: 1 },
				),
				daemon.connection.channel.sendRequest(
					"dev.dbgjs.cdp-debugger::second",
					{ targetId: "target" },
				),
			]),
			[{ value: 1 }, { value: 2 }],
		);
		assert.deepEqual(requests, [
			{ hello: 1, token: "secret-token" },
			{
				jsonrpc: "2.0",
				id: 1,
				method: "dev.dbgjs.cdp-debugger::first",
				params: { contextId: "workspace", cursor: 1 },
			},
			{
				jsonrpc: "2.0",
				id: 2,
				method: "dev.dbgjs.cdp-debugger::second",
				params: { targetId: "target" },
			},
		]);
		assert.equal(logs.some((message) => message.includes("secret-token")), false);
		assert.equal(
			logs.some((message) => message.includes("dev.dbgjs.cdp-debugger::first")),
			true,
		);
	} finally {
		daemon.close();
		await closeServer(server, sockets);
		await rm(directory, { recursive: true, force: true });
	}
});

test("daemon transport rejects pending requests when the peer disconnects", async () => {
	const directory = await mkdtemp(join(tmpdir(), "dbgjs-disconnect-test-"));
	const endpoint = testEndpoint(directory);
	const sockets = new Set<Socket>();
	const server = createServer((socket) => {
		sockets.add(socket);
		socket.setEncoding("utf8");
		socket.on("close", () => sockets.delete(socket));
		let buffer = "";
		let lines = 0;
		socket.on("data", (chunk: string) => {
			buffer += chunk;
			for (;;) {
				const newline = buffer.indexOf("\n");
				if (newline < 0) {
					return;
				}
				buffer = buffer.slice(newline + 1);
				lines++;
				if (lines === 2) {
					socket.destroy();
					return;
				}
			}
		});
	});
	await listen(server, endpoint);

	const daemon = await connectDaemon(endpoint, "test-token");
	let closes = 0;
	const closed = new Promise<void>((resolve) => {
		daemon.onClose(() => {
			closes++;
			resolve();
		});
	});
	try {
		await assert.rejects(
			daemon.connection.channel.sendRequest("dev.dbgjs.cdp-debugger::pending", {}),
			/Connection closed/,
		);
		await closed;
		assert.equal(closes, 1);
		let lateCloses = 0;
		daemon.onClose(() => lateCloses++);
		await Promise.resolve();
		assert.equal(lateCloses, 1);
	} finally {
		daemon.close();
		await closeServer(server, sockets);
		await rm(directory, { recursive: true, force: true });
	}
});

test("closing the daemon transport destroys its owned socket exactly once", async () => {
	const directory = await mkdtemp(join(tmpdir(), "dbgjs-close-test-"));
	const endpoint = testEndpoint(directory);
	const sockets = new Set<Socket>();
	let peerClosedResolve!: () => void;
	const peerClosed = new Promise<void>((resolve) => {
		peerClosedResolve = resolve;
	});
	let peerConnectedResolve!: () => void;
	const peerConnected = new Promise<void>((resolve) => {
		peerConnectedResolve = resolve;
	});
	const server = createServer((socket) => {
		sockets.add(socket);
		peerConnectedResolve();
		socket.resume();
		socket.on("close", () => {
			sockets.delete(socket);
			peerClosedResolve();
		});
	});
	await listen(server, endpoint);

	const daemon = await connectDaemon(endpoint, "test-token");
	await peerConnected;
	let closes = 0;
	daemon.onClose(() => closes++);
	daemon.close();
	daemon.close();
	try {
		await withTimeout(peerClosed, 1_000);
		assert.equal(closes, 1);
	} finally {
		await closeServer(server, sockets);
		await rm(directory, { recursive: true, force: true });
	}
});

test("daemon transport reports a close that races setup to late subscribers", async () => {
	const directory = await mkdtemp(join(tmpdir(), "dbgjs-setup-close-test-"));
	const endpoint = testEndpoint(directory);
	const sockets = new Set<Socket>();
	const server = createServer((socket) => {
		sockets.add(socket);
		socket.on("close", () => sockets.delete(socket));
		socket.destroy();
	});
	await listen(server, endpoint);

	const daemon = await connectDaemon(endpoint, "test-token");
	try {
		let closes = 0;
		const closed = new Promise<void>((resolve) => {
			daemon.onClose(() => {
				closes++;
				resolve();
			});
		});
		await withTimeout(closed, 1_000);
		assert.equal(closes, 1);
		await assert.rejects(
			daemon.connection.channel.sendRequest("dev.dbgjs.cdp-debugger::closed", {}),
			/Connection closed/,
		);
		let lateCloses = 0;
		const lateClosed = new Promise<void>((resolve) => {
			daemon.onClose(() => {
				lateCloses++;
				resolve();
			});
		});
		await lateClosed;
		assert.equal(lateCloses, 1);
	} finally {
		daemon.close();
		await closeServer(server, sockets);
		await rm(directory, { recursive: true, force: true });
	}
});

test("daemon transport rejects an initial connection failure", async () => {
	const directory = await mkdtemp(join(tmpdir(), "dbgjs-connect-failure-test-"));
	const endpoint = testEndpoint(directory);
	try {
		await assert.rejects(connectDaemon(endpoint, "test-token"));
	} finally {
		await rm(directory, { recursive: true, force: true });
	}
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
				if (message.method === "dev.dbgjs.context::observe_context") {
					observationReceivedResolve();
					void observationRelease.then(() => {
						sendResult(socket, message.id!, { kind: "items", items: [] });
					});
				} else if (message.method === "dev.dbgjs.source::list_sources") {
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

function testEndpoint(directory: string): string {
	return process.platform === "win32"
		? `\\\\.\\pipe\\dbgjs-test-${randomUUID()}`
		: join(directory, "s");
}

async function listen(server: ReturnType<typeof createServer>, endpoint: string): Promise<void> {
	await new Promise<void>((resolve, reject) => {
		server.once("error", reject);
		server.listen(endpoint, resolve);
	});
}

async function closeServer(server: ReturnType<typeof createServer>, sockets: Set<Socket>): Promise<void> {
	for (const socket of sockets) {
		socket.destroy();
	}
	await new Promise<void>((resolve) => server.close(() => resolve()));
}
