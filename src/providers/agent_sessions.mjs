import { execFile as execFileCallback } from "node:child_process";
import { join } from "node:path";
import { promisify } from "node:util";

const execFile = promisify(execFileCallback);
const agentHostPid = Number(required("JSDBG_AGENT_HOST_PID"));
const copilotPids = JSON.parse(required("JSDBG_COPILOT_PIDS"));

async function main() {
	const endpoint = await inspectorForPid(agentHostPid);
	if (!endpoint) {
		throw new Error(`agent host ${agentHostPid} has no active Node inspector`);
	}
	const cdp = await CdpClient.connect(endpoint);
	try {
		await cdp.call("Runtime.enable");
		const constructor = await copilotClientConstructor(cdp, copilotPids[0]);
		const prototype = property(
			await properties(cdp, requiredObjectId(constructor, "CopilotClient")),
			"prototype",
		);
		const clients = await cdp.call("Runtime.queryObjects", {
			prototypeObjectId: requiredObjectId(prototype, "CopilotClient.prototype"),
			objectGroup: "jsdbg-agent-sessions",
		});
		const clientsId = requiredObjectId(clients.objects, "CopilotClient instances");
		const mapPrototype = await evaluate(cdp, "Map.prototype");
		const maps = await cdp.call("Runtime.queryObjects", {
			prototypeObjectId: requiredObjectId(mapPrototype, "Map.prototype"),
			objectGroup: "jsdbg-agent-sessions",
		});
		const mapsId = requiredObjectId(maps.objects, "Map instances");

		const result = [];
		for (const processId of copilotPids) {
			const sessions = await callByValue(cdp, clientsId, function (pid) {
				for (const client of this) {
					if (client?.cliProcess?.pid !== pid) {
						continue;
					}
					return [...client.sessions.entries()].map(([internalId, session]) => ({
						internalId,
						disconnected: typeof session?.disconnected === "boolean"
							? session.disconnected
							: undefined,
					}));
				}
				return [];
			}, [processId]);
			const enriched = await callByValue(cdp, mapsId, function (items) {
				return items.map((item) => {
					let chatUri;
					for (const map of this) {
						if (!map.has(item.internalId)) {
							continue;
						}
						const candidate = map.get(item.internalId);
						if (candidate?.chatSession?._ownerSessionUri) {
							chatUri = String(candidate.chatSession._ownerSessionUri);
							break;
						}
					}
					let chat;
					if (chatUri) {
						for (const map of this) {
							if (!map.has(chatUri)) {
								continue;
							}
							const candidate = map.get(chatUri);
							if (candidate
								&& typeof candidate === "object"
								&& (typeof candidate.title === "string"
									|| Array.isArray(candidate.workingDirectories))) {
								chat = candidate;
								break;
							}
						}
					}
					return {
						...item,
						chatUri,
						title: typeof chat?.title === "string" ? chat.title : undefined,
						workingDirectories: Array.isArray(chat?.workingDirectories)
							? chat.workingDirectories.filter((value) => typeof value === "string")
							: [],
					};
				});
			}, [sessions]);
			result.push({ processId, sessions: enriched });
		}
		process.stdout.write(`${JSON.stringify(result)}\n`);
	} finally {
		await cdp.call("Runtime.releaseObjectGroup", {
			objectGroup: "jsdbg-agent-sessions",
		}).catch(() => undefined);
		cdp.close();
	}
}

async function copilotClientConstructor(cdp, processId) {
	const listener = await evaluate(
		cdp,
		`process._getActiveHandles().find(handle => handle && handle.pid === ${JSON.stringify(processId)})?._events?.error`,
	);
	const listenerId = requiredObjectId(listener, "Copilot child error listener");
	const listenerProperties = await properties(cdp, listenerId);
	const scopes = listenerProperties.internalProperties
		?.find((candidate) => candidate.name === "[[Scopes]]")?.value;
	const scopesId = requiredObjectId(scopes, "listener scopes");
	const scopeEntries = (await properties(cdp, scopesId)).result
		.filter((candidate) => /^\d+$/.test(candidate.name));
	for (const scopeEntry of scopeEntries) {
		const scopeId = scopeEntry.value?.objectId;
		if (!scopeId) {
			continue;
		}
		const constructor = property(await properties(cdp, scopeId), "CopilotClient");
		if (constructor?.objectId) {
			return constructor;
		}
	}
	throw new Error("the CopilotClient constructor is not captured by the child listener");
}

async function properties(cdp, objectId) {
	return cdp.call("Runtime.getProperties", {
		objectId,
		ownProperties: false,
	}).catch((error) => {
		throw new Error(`Runtime.getProperties(${objectId}): ${error.message}`);
	});
}

function property(response, name) {
	return response.result.find((candidate) => candidate.name === name)?.value;
}

async function evaluate(cdp, expression) {
	const response = await cdp.call("Runtime.evaluate", {
		expression,
		returnByValue: false,
	});
	if (response.exceptionDetails) {
		throw new Error(response.exceptionDetails.text || "agent-host evaluation failed");
	}
	return response.result;
}

async function callByValue(cdp, objectId, declaration, arguments_) {
	const response = await cdp.call("Runtime.callFunctionOn", {
		objectId,
		functionDeclaration: String(declaration),
		arguments: arguments_.map((value) => ({ value })),
		returnByValue: true,
	});
	if (response.exceptionDetails) {
		throw new Error(response.exceptionDetails.text || "agent-host function call failed");
	}
	return response.result.value;
}

function requiredObjectId(remoteObject, description) {
	if (typeof remoteObject?.objectId !== "string") {
		throw new Error(`${description} did not produce a remote object`);
	}
	return remoteObject.objectId;
}

async function inspectorForPid(pid) {
	const executable = join(process.env.SystemRoot || "C:\\Windows", "System32", "netstat.exe");
	const { stdout } = await execFile(executable, ["-ano", "-p", "tcp"], {
		windowsHide: true,
		maxBuffer: 16 * 1024 * 1024,
		timeout: 10_000,
	});
	const ports = [];
	for (const line of stdout.split(/\r?\n/)) {
		const match = /^\s*TCP\s+\S+:(\d+)\s+\S+\s+LISTENING\s+(\d+)\s*$/i.exec(line);
		if (match && Number(match[2]) === pid) {
			ports.push(Number(match[1]));
		}
	}
	const endpoints = await Promise.all(ports.map(async (port) => {
		const targets = await fetchJson(`http://127.0.0.1:${port}/json/list`);
		const target = Array.isArray(targets)
			? targets.find((candidate) => candidate?.type === "node")
			: undefined;
		return typeof target?.webSocketDebuggerUrl === "string"
			? target.webSocketDebuggerUrl
			: undefined;
	}));
	return endpoints.find(Boolean);
}

async function fetchJson(url) {
	try {
		const response = await fetch(url, { signal: AbortSignal.timeout(1_000) });
		return response.ok ? response.json() : undefined;
	} catch {
		return undefined;
	}
}

function required(name) {
	const value = process.env[name];
	if (!value) {
		throw new Error(`missing ${name}`);
	}
	return value;
}

class CdpClient {
	static connect(endpoint) {
		return new Promise((resolve, reject) => {
			const socket = new WebSocket(endpoint);
			socket.addEventListener("open", () => resolve(new CdpClient(socket)), { once: true });
			socket.addEventListener("error", () => reject(new Error(`failed to connect to ${endpoint}`)), { once: true });
		});
	}

	constructor(socket) {
		this.socket = socket;
		this.nextId = 1;
		this.pending = new Map();
		socket.addEventListener("message", (event) => {
			const message = JSON.parse(event.data);
			const pending = this.pending.get(message.id);
			if (!pending) {
				return;
			}
			this.pending.delete(message.id);
			if (message.error) {
				pending.reject(new Error(`${pending.method}: ${message.error.message}`));
			} else {
				pending.resolve(message.result);
			}
		});
	}

	call(method, params = {}) {
		const id = this.nextId++;
		return new Promise((resolve, reject) => {
			this.pending.set(id, { method, resolve, reject });
			this.socket.send(JSON.stringify({ id, method, params }));
		});
	}

	close() {
		this.socket.close();
	}
}

try {
	await main();
} catch (error) {
	console.error(error?.stack || String(error));
	process.exitCode = 1;
}
