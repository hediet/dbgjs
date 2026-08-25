import { spawn } from "node:child_process";
import { mkdtemp, rm, writeFile } from "node:fs/promises";
import { createServer } from "node:net";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { createInterface } from "node:readline";
import { randomBytes } from "node:crypto";

const bootloaderSource = String.raw`
const inspector = require("node:inspector");
const net = require("node:net");
const { randomBytes } = require("node:crypto");
if (!inspector.url()) {
	inspector.open(0, "127.0.0.1", false);
}
const endpoint = inspector.url();
const port = Number(process.env.JSDBG_NODE_DISCOVERY_PORT);
const token = process.env.JSDBG_NODE_DISCOVERY_TOKEN;
if (endpoint && Number.isInteger(port) && token) {
	const socket = net.createConnection({ host: "127.0.0.1", port }, () => {
		socket.write(JSON.stringify({
			token,
			instanceId: randomBytes(8).toString("hex"),
			pid: process.pid,
			parentPid: process.ppid,
			endpoint,
			title: process.argv[1] || process.title || "Node.js",
		}) + "\n");
	});
	socket.on("error", () => {});
	socket.unref();
}
`;

main().catch(reportError);

async function main() {
	const executable = required("JSDBG_NODE_EXECUTABLE");
	const program = required("JSDBG_NODE_PROGRAM");
	const cwd = required("JSDBG_NODE_CWD");
	const runtimeArgs = parseJson("JSDBG_NODE_RUNTIME_ARGS", []);
	const programArgs = parseJson("JSDBG_NODE_ARGS", []);
	const environment = parseJson("JSDBG_NODE_ENV", {});
	const discovery = await createDiscoveryServer();
	const bootloaderDirectory = await mkdtemp(join(tmpdir(), "jsdbg-node-loader-"));
	const bootloader = join(bootloaderDirectory, "discover.cjs");
	await writeFile(bootloader, bootloaderSource, { mode: 0o600 });
	const child = spawn(executable, [
		...runtimeArgs,
		"--inspect-brk=127.0.0.1:0",
		program,
		...programArgs,
	], {
		cwd,
		env: {
			...process.env,
			...environment,
			NODE_OPTIONS: appendNodeOption(
				environment.NODE_OPTIONS ?? process.env.NODE_OPTIONS,
				`--require=${JSON.stringify(bootloader)}`,
			),
			JSDBG_NODE_DISCOVERY_PORT: String(discovery.port),
			JSDBG_NODE_DISCOVERY_TOKEN: discovery.token,
		},
		stdio: ["ignore", "pipe", "pipe"],
	});
	child.stdout.pipe(process.stderr);
	try {
		const endpoint = await debuggerEndpoint(child);
		discovery.setRootPid(child.pid);
		process.stdout.write(`${JSON.stringify({ endpoint })}\n`);
		discovery.startPublishing();
		await Promise.race([
			onceExit(child),
			new Promise((resolve) => {
				process.stdin.once("end", resolve);
				process.once("SIGINT", resolve);
				process.once("SIGTERM", resolve);
				process.stdin.resume();
			}),
		]);
	} finally {
		if (child.exitCode === null) {
			child.kill("SIGTERM");
			await Promise.race([onceExit(child), delay(5_000)]);
		}
		if (child.exitCode === null) {
			child.kill("SIGKILL");
		}
		await discovery.terminateProcesses();
		await discovery.close();
		await rm(bootloaderDirectory, { recursive: true, force: true });
	}
}

async function createDiscoveryServer() {
	const token = randomBytes(32).toString("hex");
	const sockets = new Set();
	const announcements = new Map();
	const pidToTarget = new Map();
	const queued = [];
	let rootPid;
	let publishing = false;
	const server = createServer((socket) => {
		if (sockets.size >= 128) {
			socket.destroy();
			return;
		}
		sockets.add(socket);
		let buffer = "";
		let targetId;
		let authenticated = false;
		const authenticationTimeout = setTimeout(() => socket.destroy(), 5_000);
		authenticationTimeout.unref();
		socket.setEncoding("utf8");
		socket.on("data", (chunk) => {
			if (authenticated) {
				return;
			}
			buffer += chunk;
			if (buffer.length > 64 * 1024) {
				socket.destroy();
				return;
			}
			for (;;) {
				const newline = buffer.indexOf("\n");
				if (newline < 0) {
					break;
				}
				const line = buffer.slice(0, newline);
				buffer = buffer.slice(newline + 1);
				try {
					const announcement = JSON.parse(line);
					if (announcement.token !== token
						|| !Number.isInteger(announcement.pid)
						|| typeof announcement.endpoint !== "string"
						|| typeof announcement.instanceId !== "string") {
						socket.destroy();
						return;
					}
					authenticated = true;
					clearTimeout(authenticationTimeout);
					if (announcement.pid === rootPid) {
						return;
					}
					targetId = `node-${announcement.pid}-${announcement.instanceId}`;
					const target = {
						kind: "nodeTarget",
						targetId,
						parentTargetId: pidToTarget.get(announcement.parentPid) ?? "$node-root",
						title: announcement.title || `Node.js ${announcement.pid}`,
						url: announcement.endpoint,
						endpoint: announcement.endpoint,
					};
					announcements.set(targetId, { ...announcement, target });
					pidToTarget.set(announcement.pid, targetId);
					publish(target);
					for (const entry of announcements.values()) {
						if (entry.parentPid === announcement.pid) {
							entry.target = {
								...entry.target,
								parentTargetId: targetId,
							};
							publish(entry.target);
						}
					}
				} catch {
					socket.destroy();
					return;
				}
			}
		});
		socket.on("close", () => {
			clearTimeout(authenticationTimeout);
			sockets.delete(socket);
			if (targetId) {
				const entry = announcements.get(targetId);
				if (entry) {
					announcements.delete(targetId);
					pidToTarget.delete(entry.pid);
					for (const childEntry of announcements.values()) {
						if (childEntry.target.parentTargetId === targetId) {
							childEntry.target = {
								...childEntry.target,
								parentTargetId: entry.target.parentTargetId,
							};
							publish(childEntry.target);
						}
					}
				}
				publish({ kind: "nodeTargetRemoved", targetId });
			}
		});
	});
	await new Promise((resolve, reject) => {
		server.once("error", reject);
		server.listen(0, "127.0.0.1", resolve);
	});
	const address = server.address();
	if (typeof address !== "object" || address === null) {
		throw new Error("could not allocate Node.js discovery port");
	}
	return {
		port: address.port,
		token,
		setRootPid(value) {
			rootPid = value;
			for (const [targetId, entry] of announcements) {
				if (entry.pid === value) {
					announcements.delete(targetId);
					pidToTarget.delete(value);
					for (let index = queued.length - 1; index >= 0; index -= 1) {
						if (queued[index].targetId === targetId) {
							queued.splice(index, 1);
						}
					}
				}
			}
		},
		startPublishing() {
			publishing = true;
			for (const event of queued.splice(0)) {
				writeEvent(event);
			}
		},
		async close() {
			for (const socket of sockets) {
				socket.destroy();
			}
			await new Promise((resolve) => server.close(resolve));
		},
		async terminateProcesses() {
			const processIds = [...announcements.values()].map((entry) => entry.pid);
			for (const processId of processIds) {
				try {
					process.kill(processId, "SIGTERM");
				} catch {}
			}
			await delay(250);
			for (const processId of processIds) {
				try {
					process.kill(processId, "SIGKILL");
				} catch {}
			}
		},
	};

	function publish(event) {
		if (publishing) {
			writeEvent(event);
		} else {
			queued.push(event);
		}
	}
}

function writeEvent(event) {
	process.stdout.write(`${JSON.stringify(event)}\n`);
}

function appendNodeOption(current, option) {
	return current ? `${current} ${option}` : option;
}

function debuggerEndpoint(child) {
	return new Promise((resolve, reject) => {
		const lines = createInterface({ input: child.stderr });
		let settled = false;
		const timeout = setTimeout(() => {
			settled = true;
			reject(new Error("Node.js inspector startup timed out"));
		}, 30_000);
		lines.on("line", (line) => {
			process.stderr.write(`${line}\n`);
			const match = /Debugger listening on (ws:\/\/\S+)/.exec(line);
			if (!settled && match?.[1]) {
				settled = true;
				clearTimeout(timeout);
				resolve(match[1]);
			}
		});
		child.once("exit", (code) => {
			if (!settled) {
				settled = true;
				clearTimeout(timeout);
				reject(new Error(`Node.js exited during startup with code ${code}`));
			}
		});
	});
}

function onceExit(child) {
	return new Promise((resolve) => child.once("exit", resolve));
}

function required(name) {
	const value = process.env[name];
	if (!value) {
		throw new Error(`${name} is required`);
	}
	return value;
}

function parseJson(name, fallback) {
	const value = process.env[name];
	return value ? JSON.parse(value) : fallback;
}

function delay(milliseconds) {
	return new Promise((resolve) => setTimeout(resolve, milliseconds));
}

function reportError(error) {
	process.stdout.write(`${JSON.stringify({
		error: error instanceof Error ? error.stack : String(error),
	})}\n`);
	process.exitCode = 1;
}
