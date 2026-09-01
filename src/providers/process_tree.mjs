	import { execFile as execFileCallback } from "node:child_process";
import { randomUUID } from "node:crypto";
import { readFile, rm } from "node:fs/promises";
import { basename, join } from "node:path";
import { tmpdir } from "node:os";
import { promisify } from "node:util";

const execFile = promisify(execFileCallback);
const rootPid = Number(required("JSDBG_PROCESS_ROOT_PID"));
const processMode = process.env.JSDBG_PROCESS_MODE || "tree";
const pollIntervalMs = 2_000;
const knownEndpoints = new Map();
const announcedTargets = new Map();
let activationQueue = Promise.resolve();
let scanQueue = Promise.resolve();
let running = true;
let discoveryEnabled = false;
let wake = () => {};

if (process.platform !== "win32") {
	throw new Error("existing process-tree discovery is currently implemented only on Windows");
}
if (typeof WebSocket !== "function") {
	throw new Error("existing process-tree discovery requires a Node.js runtime with WebSocket support");
}
if (!Number.isSafeInteger(rootPid) || rootPid <= 0) {
	throw new Error(`invalid root process ID: ${rootPid}`);
}

main().catch(reportError);

async function main() {
	const [processes, listeners] = await Promise.all([listProcesses(), listListeners()]);
	const root = processes.find((candidate) => candidate.pid === rootPid);
	if (!root) {
		throw new Error(`root process ${rootPid} does not exist`);
	}
	const rootEndpoint = await ensureRootEndpoint(rootPid, listeners);
	process.stdout.write(`${JSON.stringify({ endpoint: rootEndpoint })}\n`);

	readControlCommands();
	process.once("SIGINT", stop);
	process.once("SIGTERM", stop);

	if (processMode === "single") {
		while (running) {
			await idle();
		}
		return;
	}
	while (running) {
		if (!discoveryEnabled) {
			await idle();
			continue;
		}
		await scan();
		await idle(pollIntervalMs);
	}
}

/// Descendant scanning is demand driven: the debugger service enables it while a CDP client
/// requires target discovery and asks for one-shot scans otherwise.
function readControlCommands() {
	let buffer = "";
	process.stdin.setEncoding("utf8");
	process.stdin.on("data", (chunk) => {
		buffer += chunk;
		for (;;) {
			const newline = buffer.indexOf("\n");
			if (newline < 0) {
				break;
			}
			const line = buffer.slice(0, newline).trim();
			buffer = buffer.slice(newline + 1);
			if (line) {
				handleCommand(line);
			}
		}
	});
	process.stdin.once("end", stop);
	process.stdin.resume();
}

function handleCommand(line) {
	let command;
	try {
		command = JSON.parse(line);
	} catch (error) {
		console.error(`ignored invalid process-tree command: ${error?.message ?? error}`);
		return;
	}
	if (command?.command === "setDiscovery") {
		discoveryEnabled = command.enabled === true;
		wake();
		return;
	}
	if (command?.command === "scan") {
		void scan().then(() => {
			process.stdout.write(`${JSON.stringify({ kind: "scanComplete", id: command.id ?? null })}\n`);
		});
		return;
	}
	console.error(`ignored unknown process-tree command: ${line}`);
}

function scan() {
	scanQueue = scanQueue.then(async () => {
		if (processMode === "single") {
			return;
		}
		try {
			await reconcile();
		} catch (error) {
			console.error(`process-tree discovery failed: ${error?.stack ?? error}`);
		}
	});
	return scanQueue;
}

function stop() {
	running = false;
	wake();
}

function idle(milliseconds) {
	return new Promise((resolve) => {
		const timer = milliseconds === undefined ? undefined : setTimeout(finish, milliseconds);
		wake = finish;
		function finish() {
			wake = () => {};
			if (timer) {
				clearTimeout(timer);
			}
			resolve();
		}
		if (!running) {
			finish();
		}
	});
}

async function reconcile(initialProcesses, initialListeners) {
	const [processes, listeners] = initialProcesses && initialListeners
		? [initialProcesses, initialListeners]
		: await Promise.all([listProcesses(), listListeners()]);
	const descendants = processDescendants(processes, rootPid);
	const nodeProcesses = new Map(
		descendants
			.filter((candidate) => candidate.pid !== rootPid && isNodeProcess(candidate))
			.map((candidate) => [candidate.pid, candidate]),
	);
	const desired = new Map();
	const discoveredProcesses = new Map(
		[...nodeProcesses.values()].map((candidate) => {
			const targetId = `process-${candidate.pid}-${processInstanceId(candidate)}`;
			return [candidate.pid, { candidate, targetId }];
		}),
	);

	for (const candidate of nodeProcesses.values()) {
		const { targetId } = discoveredProcesses.get(candidate.pid);
		const target = {
			kind: "nodeTarget",
			targetId,
			parentTargetId: nearestAttachableParent(candidate, processes, discoveredProcesses),
			targetType: "node",
			title: processTitle(candidate),
			url: `process:${candidate.pid}`,
			processId: candidate.pid,
		};
		const knownEndpoint = knownEndpoints.get(candidate.pid);
		if (knownEndpoint) {
			target.endpoint = knownEndpoint;
		}
		desired.set(targetId, target);
		publishTarget(target);
	}

	const chromiumTargets = discoverChromiumTargets(
		descendants,
		processes,
		discoveredProcesses,
		listeners,
	);
	await Promise.all([...nodeProcesses.values()].map(async (candidate) => {
		const { targetId } = discoveredProcesses.get(candidate.pid);
		try {
			const endpoint = await ensureInspector(candidate.pid, listeners);
			const target = { ...desired.get(targetId), endpoint };
			desired.set(targetId, target);
			publishTarget(target);
		} catch (error) {
			console.error(`could not activate inspector for process ${candidate.pid}: ${error?.message ?? error}`);
		}
	}));

	for (const target of await chromiumTargets) {
		desired.set(target.targetId, target);
		publishTarget(target);
	}

	for (const targetId of [...announcedTargets.keys()]) {
		if (!desired.has(targetId)) {
			announcedTargets.delete(targetId);
			process.stdout.write(`${JSON.stringify({ kind: "nodeTargetRemoved", targetId })}\n`);
		}
	}
}

function publishTarget(target) {
	const serialized = JSON.stringify(target);
	if (announcedTargets.get(target.targetId) !== serialized) {
		announcedTargets.set(target.targetId, serialized);
		process.stdout.write(`${serialized}\n`);
	}
}

async function ensureInspector(pid, knownListeners) {
	const cached = knownEndpoints.get(pid);
	if (cached && await isInspectorEndpoint(cached)) {
		return cached;
	}

	const listeners = knownListeners ?? await listListeners();
	const existing = await inspectorForPid(pid, listeners);
	if (existing) {
		knownEndpoints.set(pid, existing);
		return existing;
	}
	const activation = activationQueue.then(() => activateInspector(pid));
	activationQueue = activation.catch(() => undefined);
	return activation;
}

async function activateInspector(pid) {
	const existing = await inspectorForPid(pid, await listListeners());
	if (existing) {
		knownEndpoints.set(pid, existing);
		return existing;
	}
	await execFile(process.execPath, ["-e", `process._debugProcess(${pid})`], {
		windowsHide: true,
		timeout: 10_000,
	});
	const activatedEndpoint = await poll(async () => {
		return inspectorForPid(pid, await listListeners());
	}, 5_000);
	if (!activatedEndpoint) {
		const defaultOwner = (await listListeners())
			.find((listener) => listener.port === 9229)?.pid;
		const suffix = defaultOwner && defaultOwner !== pid
			? `; port 9229 is owned by process ${defaultOwner}`
			: "";
		throw new Error(`the process did not open an inspector${suffix}`);
	}
	if (Number(new URL(activatedEndpoint).port) !== 9229) {
		knownEndpoints.set(pid, activatedEndpoint);
		return activatedEndpoint;
	}

	const marker = join(tmpdir(), `jsdbg-inspector-${pid}-${randomUUID()}.json`);
	const token = randomUUID();
	const expression = `(() => {
		const fs = process.getBuiltinModule("fs");
		const inspector = process.getBuiltinModule("inspector");
		const result = { pid: process.pid };
		setTimeout(() => {
			try {
				inspector.close();
				inspector.open(0, "127.0.0.1", false);
				fs.writeFileSync(${JSON.stringify(marker)}, JSON.stringify({
					token: ${JSON.stringify(token)},
					pid: process.pid,
					endpoint: inspector.url()
				}));
			} catch (error) {
				fs.writeFileSync(${JSON.stringify(marker)}, JSON.stringify({
					token: ${JSON.stringify(token)},
					pid: process.pid,
					error: String(error?.stack || error)
				}));
			}
		}, 500);
		return result;
	})()`;
	const identity = await evaluate(activatedEndpoint, expression);
	if (identity?.pid !== pid) {
		throw new Error(`port 9229 belongs to process ${identity?.pid ?? "unknown"}, not ${pid}`);
	}

	const rotated = await poll(async () => {
		try {
			const value = JSON.parse(await readFile(marker, "utf8"));
			return value.token === token ? value : undefined;
		} catch {
			return undefined;
		}
	}, 10_000);
	await rm(marker, { force: true });
	if (!rotated) {
		throw new Error("timed out while moving the inspector to an ephemeral port");
	}
	if (rotated.error) {
		throw new Error(rotated.error);
	}
	if (rotated.pid !== pid || typeof rotated.endpoint !== "string") {
		throw new Error("the inspector returned an invalid rotated endpoint");
	}
	knownEndpoints.set(pid, rotated.endpoint);
	return rotated.endpoint;
}

async function discoverChromiumTargets(processesInScope, allProcesses, discoveredProcesses, listeners) {
	const discoveries = await Promise.all(
		processesInScope.flatMap((process) =>
			listeners.filter((listener) => listener.pid === process.pid).map(async (listener) => {
				const version = await fetchJson(`http://127.0.0.1:${listener.port}/json/version`);
				if (typeof version?.webSocketDebuggerUrl !== "string"
					|| !version.webSocketDebuggerUrl.includes("/devtools/browser/")) {
					return [];
				}
				return [{
					kind: "nodeTarget",
					targetId: `cdp-browser-${listener.pid}-${listener.port}`,
					parentTargetId: nearestAttachableParent(
						process,
						allProcesses,
						discoveredProcesses,
					),
					targetType: "browser",
					title: typeof version.Browser === "string" ? version.Browser : `Browser ${listener.pid}`,
					url: `http://127.0.0.1:${listener.port}`,
					endpoint: version.webSocketDebuggerUrl,
					processId: listener.pid,
				}];
			})),
	);
	return discoveries.flat();
}

async function ensureRootEndpoint(pid, listeners) {
	const inspector = await inspectorForPid(pid, listeners);
	if (inspector) {
		knownEndpoints.set(pid, inspector);
		return inspector;
	}
	const browser = await browserForPid(pid, listeners);
	if (browser) {
		knownEndpoints.set(pid, browser);
		return browser;
	}
	return ensureInspector(pid, listeners);
}

async function browserForPid(pid, listeners) {
	const endpoints = await Promise.all(
		listeners.filter((candidate) => candidate.pid === pid).map(async (listener) => {
			const version = await fetchJson(`http://127.0.0.1:${listener.port}/json/version`);
			return typeof version?.webSocketDebuggerUrl === "string"
				&& version.webSocketDebuggerUrl.includes("/devtools/browser/")
				? version.webSocketDebuggerUrl
				: undefined;
		}),
	);
	return endpoints.find(Boolean);
}

async function inspectorForPid(pid, listeners) {
	const endpoints = await Promise.all(
		listeners.filter((candidate) => candidate.pid === pid).map(async (listener) => {
			const targets = await fetchJson(`http://127.0.0.1:${listener.port}/json/list`);
			const target = Array.isArray(targets)
				? targets.find((candidate) => candidate?.type === "node")
				: undefined;
			return typeof target?.webSocketDebuggerUrl === "string"
				? target.webSocketDebuggerUrl
				: undefined;
		}),
	);
	return endpoints.find(Boolean);
}

async function isInspectorEndpoint(endpoint) {
	try {
		const url = new URL(endpoint);
		const targets = await fetchJson(`${url.protocol === "wss:" ? "https:" : "http:"}//${url.host}/json/list`);
		return Array.isArray(targets)
			&& targets.some((candidate) => candidate?.webSocketDebuggerUrl === endpoint);
	} catch {
		return false;
	}
}

async function evaluate(endpoint, expression) {
	return new Promise((resolve, reject) => {
		const socket = new WebSocket(endpoint);
		const timer = setTimeout(() => {
			socket.close();
			reject(new Error("CDP evaluation timed out"));
		}, 5_000);
		socket.onopen = () => {
			socket.send(JSON.stringify({
				id: 1,
				method: "Runtime.evaluate",
				params: { expression, returnByValue: true },
			}));
		};
		socket.onmessage = (event) => {
			const message = JSON.parse(event.data);
			if (message.id !== 1) {
				return;
			}
			clearTimeout(timer);
			socket.close();
			if (message.error) {
				reject(new Error(message.error.message));
			} else if (message.result?.exceptionDetails) {
				reject(new Error(message.result.exceptionDetails.text || "CDP evaluation failed"));
			} else {
				resolve(message.result?.result?.value);
			}
		};
		socket.onerror = () => {
			clearTimeout(timer);
			reject(new Error("CDP WebSocket failed"));
		};
	});
}

async function listProcesses() {
	const value = await powershell(`
		@(Get-CimInstance Win32_Process | Select-Object ProcessId,ParentProcessId,Name,CommandLine,CreationDate) |
			ConvertTo-Json -Compress
	`);
	return asArray(value).map((candidate) => ({
		pid: Number(candidate.ProcessId),
		parentPid: Number(candidate.ParentProcessId),
		name: String(candidate.Name || ""),
		commandLine: String(candidate.CommandLine || ""),
		creationDate: String(candidate.CreationDate || ""),
	}));
}

async function listListeners() {
	const executable = join(process.env.SystemRoot || "C:\\Windows", "System32", "netstat.exe");
	const { stdout } = await execFile(executable, ["-ano", "-p", "tcp"], {
		windowsHide: true,
		maxBuffer: 16 * 1024 * 1024,
		timeout: 10_000,
	});
	const listeners = new Map();
	for (const line of stdout.split(/\r?\n/)) {
		const match = /^\s*TCP\s+\S+:(\d+)\s+\S+\s+LISTENING\s+(\d+)\s*$/i.exec(line);
		if (!match) {
			continue;
		}
		const port = Number(match[1]);
		const pid = Number(match[2]);
		listeners.set(`${pid}:${port}`, { pid, port });
	}
	return [...listeners.values()];
}

async function powershell(script) {
	const { stdout } = await execFile("powershell.exe", [
		"-NoProfile",
		"-NonInteractive",
		"-Command",
		`$ErrorActionPreference = "Stop"; ${script}`,
	], {
		windowsHide: true,
		maxBuffer: 16 * 1024 * 1024,
		timeout: 30_000,
	});
	const text = stdout.trim();
	return text ? JSON.parse(text) : [];
}

function processDescendants(processes, pid) {
	const children = new Map();
	for (const candidate of processes) {
		const entries = children.get(candidate.parentPid) ?? [];
		entries.push(candidate);
		children.set(candidate.parentPid, entries);
	}
	const result = [];
	const queue = [pid];
	const seen = new Set();
	while (queue.length) {
		const current = queue.shift();
		if (seen.has(current)) {
			continue;
		}
		seen.add(current);
		const process = processes.find((candidate) => candidate.pid === current);
		if (process) {
			result.push(process);
		}
		for (const child of children.get(current) ?? []) {
			queue.push(child.pid);
		}
	}
	return result;
}

function nearestAttachableParent(candidate, processes, discoveredProcesses) {
	let parentPid = candidate.parentPid;
	while (parentPid && parentPid !== rootPid) {
		const parent = discoveredProcesses.get(parentPid);
		if (parent) {
			return parent.targetId;
		}
		parentPid = processes.find((process) => process.pid === parentPid)?.parentPid;
	}
	return "$node-root";
}

function processInstanceId(candidate) {
	const value = candidate.creationDate.replace(/[^A-Za-z0-9_.-]/g, "");
	return value || "unknown";
}

function isNodeProcess(candidate) {
	const executable = candidate.name.toLowerCase();
	const command = candidate.commandLine.toLowerCase();
	return !command.includes("process._debugprocess(")
		&& (executable === "node.exe"
		|| command.includes("node.mojom.nodeservice")
		|| command.includes("--node-ipc")
		|| command.includes("bootstrap-fork")
		|| command.includes("tsserver.js")
		|| command.includes("typingsinstaller.js"));
}

function processTitle(candidate) {
	const quoted = [...candidate.commandLine.matchAll(/"([^"]+\.(?:js|cjs|mjs))"/gi)];
	const script = quoted.at(-1)?.[1];
	return `${script ? basename(script) : candidate.name} (${candidate.pid})`;
}

async function fetchJson(url) {
	try {
		const response = await fetch(url, { signal: AbortSignal.timeout(300) });
		return response.ok ? await response.json() : undefined;
	} catch {
		return undefined;
	}
}

async function poll(operation, timeoutMs) {
	const deadline = Date.now() + timeoutMs;
	do {
		const value = await operation();
		if (value !== undefined) {
			return value;
		}
		await delay(100);
	} while (Date.now() < deadline);
	return undefined;
}

function asArray(value) {
	return Array.isArray(value) ? value : value ? [value] : [];
}

function delay(milliseconds) {
	return new Promise((resolve) => setTimeout(resolve, milliseconds));
}

function required(name) {
	const value = process.env[name];
	if (!value) {
		throw new Error(`missing ${name}`);
	}
	return value;
}

function reportError(error) {
	process.stdout.write(`${JSON.stringify({ error: String(error?.stack || error) })}\n`);
	process.exitCode = 1;
}
