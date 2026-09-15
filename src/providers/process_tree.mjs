	import { execFile as execFileCallback } from "node:child_process";
import { randomUUID } from "node:crypto";
import { readFile, readdir, readlink, rm } from "node:fs/promises";
import { basename, join } from "node:path";
import { tmpdir } from "node:os";
import { promisify } from "node:util";

const execFile = promisify(execFileCallback);
const rootPid = Number(required("DBGJS_PROCESS_ROOT_PID"));
const processMode = process.env.DBGJS_PROCESS_MODE || "tree";
const pollIntervalMs = 2_000;
const knownEndpoints = new Map();
const announcedTargets = new Map();
const expandedProcessIds = new Set([rootPid]);
let activationQueue = Promise.resolve();
let scanQueue = Promise.resolve();
let running = true;
let discoveryEnabled = false;
let wake = () => {};

if (!["win32", "linux", "darwin"].includes(process.platform)) {
	throw new Error(`existing process-tree discovery is unsupported on ${process.platform}`);
}
if (typeof WebSocket !== "function") {
	throw new Error("existing process-tree discovery requires a Node.js runtime with WebSocket support");
}
if (!Number.isSafeInteger(rootPid) || rootPid <= 0) {
	throw new Error(`invalid root process ID: ${rootPid}`);
}

main().catch(reportError);

async function main() {
	const processes = await listProcesses();
	const listeners = await listListeners(processes);
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
	if (command?.command === "activate") {
		void activateTarget(command);
		return;
	}
	if (command?.command === "setProcessDiscovery") {
		const processId = Number(command.processId);
		if (Number.isSafeInteger(processId) && processId > 0 && processId !== rootPid) {
			if (command.enabled === true) {
				expandedProcessIds.add(processId);
			} else {
				expandedProcessIds.delete(processId);
			}
			void scan();
		}
		return;
	}
	console.error(`ignored unknown process-tree command: ${line}`);
}

async function activateTarget(command) {
	const id = command.id ?? null;
	const targetId = typeof command.targetId === "string" ? command.targetId : "";
	const pid = Number(command.processId);
	if (!targetId || !Number.isSafeInteger(pid) || pid <= 0) {
		process.stdout.write(`${JSON.stringify({
			kind: "activationComplete",
			id,
			targetId,
			error: "invalid target activation request",
		})}\n`);
		return;
	}
	try {
		const endpoint = await ensureInspector(pid);
		process.stdout.write(`${JSON.stringify({
			kind: "activationComplete",
			id,
			targetId,
			endpoint,
		})}\n`);
	} catch (error) {
		process.stdout.write(`${JSON.stringify({
			kind: "activationComplete",
			id,
			targetId,
			error: error?.message ?? String(error),
		})}\n`);
	}
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
	const processes = initialProcesses ?? await listProcesses();
	const listeners = initialListeners ?? await listListeners(processes);
	const liveProcessIds = new Set(processes.map((candidate) => candidate.pid));
	for (const processId of expandedProcessIds) {
		if (processId !== rootPid && !liveProcessIds.has(processId)) {
			expandedProcessIds.delete(processId);
		}
	}
	const descendants = processDescendants(processes, rootPid);
	const allNodeProcesses = descendants.filter(
		(candidate) => candidate.pid !== rootPid && isNodeProcess(candidate),
	);
	const nodeProcessIds = new Set(allNodeProcesses.map((candidate) => candidate.pid));
	const allDiscoveredProcesses = new Map(
		allNodeProcesses.map((candidate) => {
			const targetId = `process-${candidate.pid}-${processInstanceId(candidate)}`;
			return [candidate.pid, { candidate, targetId }];
		}),
	);
	const allChromiumTargets = await discoverChromiumTargets(
		descendants,
		processes,
		allDiscoveredProcesses,
		listeners,
	);
	const attachableProcessIds = new Set([
		...nodeProcessIds,
		...allChromiumTargets.map((target) => target.processId),
	]);
	const processesInScope = descendants.filter((candidate) =>
		isInsideExpandedProcessFrontier(candidate, processes, attachableProcessIds),
	);
	const processIdsInScope = new Set(processesInScope.map((candidate) => candidate.pid));
	const nodeProcesses = new Map(
		processesInScope
			.filter((candidate) => nodeProcessIds.has(candidate.pid))
			.map((candidate) => [candidate.pid, candidate]),
	);
	const desired = new Map();
	const discoveredProcesses = new Map(
		[...nodeProcesses.keys()].map((pid) => [pid, allDiscoveredProcesses.get(pid)]),
	);
	const chromiumTargets = allChromiumTargets.filter((target) =>
		processIdsInScope.has(target.processId),
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
		desired.set(targetId, target);
	}

	await Promise.all([...nodeProcesses.values()].map(async (candidate) => {
		const { targetId } = discoveredProcesses.get(candidate.pid);
		const cached = knownEndpoints.get(candidate.pid);
		const endpoint = cached && await isInspectorEndpoint(cached)
			? cached
			: await inspectorForPid(candidate.pid, listeners);
		if (endpoint) {
			knownEndpoints.set(candidate.pid, endpoint);
			const target = { ...desired.get(targetId), endpoint };
			desired.set(targetId, target);
			publishTarget(target);
		} else {
			knownEndpoints.delete(candidate.pid);
		}
	}));
	for (const target of desired.values()) {
		publishTarget(target);
	}

	for (const target of chromiumTargets) {
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

function isInsideExpandedProcessFrontier(candidate, processes, attachableProcessIds) {
	let parentPid = candidate.parentPid;
	while (parentPid && parentPid !== rootPid) {
		if (expandedProcessIds.has(parentPid)) {
			return true;
		}
		if (attachableProcessIds.has(parentPid)) {
			return false;
		}
		parentPid = processes.find((process) => process.pid === parentPid)?.parentPid;
	}
	return expandedProcessIds.has(rootPid);
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
	if (process.platform !== "win32") {
		const candidate = (await listProcesses()).find((candidate) => candidate.pid === pid);
		if (!candidate || !["node", "nodejs"].includes(candidate.name.toLowerCase())) {
			throw new Error(
				`process ${pid} has no inspector; launch VS Code with --inspect-extensions=PORT `
				+ "for extension hosts and --inspect=PORT for process-tree roots. "
				+ "Automatic Unix activation is supported only for standalone Node.js processes",
			);
		}
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

	const marker = join(tmpdir(), `dbgjs-inspector-${pid}-${randomUUID()}.json`);
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
	if (process.platform !== "win32") {
		const options = {
			env: { ...process.env, LC_ALL: "C" },
			maxBuffer: 16 * 1024 * 1024,
			timeout: 10_000,
		};
		const [commands, names] = await Promise.all([
			execFile("ps", ["-axww", "-o", "pid=,ppid=,lstart=,args="], options),
			execFile("ps", ["-axww", "-o", "pid=,comm="], options),
		]);
		return parseUnixProcesses(commands.stdout, names.stdout);
	}
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

function parseUnixProcesses(commands, names) {
	const executableNames = new Map(names.split(/\r?\n/).flatMap((line) => {
		const match = /^\s*(\d+)\s+(.+?)\s*$/.exec(line);
		return match ? [[Number(match[1]), match[2].split("/").at(-1)]] : [];
	}));
	return commands.split(/\r?\n/).flatMap((line) => {
		const match = /^\s*(\d+)\s+(\d+)\s+(\w{3}\s+\w{3}\s+\d+\s+\d\d:\d\d:\d\d\s+\d{4})\s+(.*)$/.exec(line);
		if (!match) {
			return [];
		}
		const started = Date.parse(match[3]);
		if (!Number.isFinite(started)) {
			throw new Error(`invalid process start time: ${match[3]}`);
		}
		const pid = Number(match[1]);
		return [{
			pid,
			parentPid: Number(match[2]),
			name: executableNames.get(pid) ?? "",
			commandLine: match[4],
			creationDate: String(Math.floor(started / 1000)).padStart(20, "0"),
		}];
	});
}

async function listListeners(processes) {
	if (process.platform === "linux") {
		return listLinuxListeners(processes ?? await listProcesses());
	}
	if (process.platform === "darwin") {
		try {
			const { stdout } = await execFile("/usr/sbin/lsof", [
				"-nP", "-iTCP", "-sTCP:LISTEN", "-Fpn",
			], { maxBuffer: 16 * 1024 * 1024, timeout: 10_000 });
			return parseLsofListeners(stdout);
		} catch (error) {
			if (error.code === 1 && !error.stdout && !error.stderr) {
				return [];
			}
			throw error;
		}
	}
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

function parseLsofListeners(stdout) {
	let pid;
	const listeners = new Map();
	for (const line of stdout.split(/\r?\n/)) {
		if (/^p\d+$/.test(line)) {
			pid = Number(line.slice(1));
		} else if (pid && line.startsWith("n")) {
			const match = /:(\d+)$/.exec(line);
			if (match) {
				const port = Number(match[1]);
				listeners.set(`${pid}:${port}`, { pid, port });
			}
		}
	}
	return [...listeners.values()];
}

function parseLinuxTcpListeners(stdout) {
	const sockets = new Map();
	for (const line of stdout.trim().split(/\r?\n/).slice(1)) {
		const fields = line.trim().split(/\s+/);
		if (fields[3] === "0A" && fields[9]) {
			const port = Number.parseInt(fields[1].split(":")[1], 16);
			if (port > 0) {
				sockets.set(fields[9], port);
			}
		}
	}
	return sockets;
}

async function listLinuxListeners(processes) {
	const tables = await Promise.all(["tcp", "tcp6"].map(async (name) => {
		try {
			return parseLinuxTcpListeners(await readFile(`/proc/net/${name}`, "utf8"));
		} catch (error) {
			if (error.code === "ENOENT" && name === "tcp6") {
				return new Map();
			}
			throw error;
		}
	}));
	const sockets = new Map(tables.flatMap((table) => [...table]));
	const listeners = new Map();
	await Promise.all(processDescendants(processes, rootPid).map(async ({ pid }) => {
		try {
			const directory = `/proc/${pid}/fd`;
			await Promise.all((await readdir(directory)).map(async (fd) => {
				try {
					const target = await readlink(`${directory}/${fd}`);
					const inode = /^socket:\[(\d+)\]$/.exec(target)?.[1];
					const port = sockets.get(inode);
					if (port) {
						listeners.set(`${pid}:${port}`, { pid, port });
					}
				} catch (error) {
					if (!["ENOENT", "EACCES", "EPERM"].includes(error.code)) {
						throw error;
					}
				}
			}));
		} catch (error) {
			if (!["ENOENT", "EACCES", "EPERM"].includes(error.code)) {
				throw error;
			}
		}
	}));
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
		&& (["node.exe", "node", "nodejs"].includes(executable)
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
