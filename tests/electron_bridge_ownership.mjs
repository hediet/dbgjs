import { EventEmitter } from "node:events";
import { readFile } from "node:fs/promises";
import net from "node:net";

class FakeDebugger extends EventEmitter {
	constructor(attached = true) {
		super();
		this.attached = attached;
		this.attachCount = 0;
		this.detachCount = 0;
		this.failNextCommand = undefined;
		this.runIfWaitingCount = 0;
		this.pendingWaitForDebugger = undefined;
	}

	isAttached() {
		return this.attached;
	}

	attach() {
		if (this.attached) {
			throw new Error("already attached");
		}
		this.attached = true;
		this.attachCount++;
	}

	detach() {
		if (!this.attached) {
			return;
		}
		this.attached = false;
		this.detachCount++;
		this.pendingWaitForDebugger?.resolve({});
		this.pendingWaitForDebugger = undefined;
		this.emit("detach", {}, "detached");
	}

	async sendCommand(method) {
		if (method === this.failNextCommand) {
			this.failNextCommand = undefined;
			throw new Error(`deterministic ${method} initialization failure`);
		}
		// Chromium only answers `Page.waitForDebugger` once the renderer is resumed, so the fake
		// keeps the promise pending exactly like a genuinely blocked renderer would.
		if (method === "Page.waitForDebugger") {
			return new Promise((resolve, reject) => {
				this.pendingWaitForDebugger = { resolve, reject };
			});
		}
		if (method === "Runtime.runIfWaitingForDebugger") {
			this.runIfWaitingCount++;
			this.pendingWaitForDebugger?.resolve({});
			this.pendingWaitForDebugger = undefined;
			return {};
		}
		return {};
	}
}

class FakeWebContents extends EventEmitter {
	constructor(id, processId, title, url, attached = false) {
		super();
		this.id = id;
		this.debugger = new FakeDebugger(attached);
		this.processId = processId;
		this.title = title;
		this.url = url;
		this.destroyed = false;
	}

	isDestroyed() {
		return this.destroyed;
	}

	getOSProcessId() {
		return this.processId;
	}

	getType() {
		return "window";
	}

	getTitle() {
		return this.title;
	}

	getURL() {
		return this.url;
	}
}

const contents = new FakeWebContents(7, 4242, "deterministic renderer", "file:///renderer.html", true);
const fakeDebugger = contents.debugger;
const allContents = [contents];
const app = new EventEmitter();
const createWebContents = (id, processId, title) => {
	const created = new FakeWebContents(id, processId, title, `file:///${title}.html`);
	allContents.push(created);
	app.emit("web-contents-created", {}, created);
	return created;
};
globalThis.require = (name) => {
	if (name === "electron") {
		return {
			app,
			webContents: {
				fromId: (id) => allContents.find((candidate) => candidate.id === id),
				getAllWebContents: () => allContents,
			},
		};
	}
	if (name === "node:net") {
		return net;
	}
	throw new Error(`unexpected module ${name}`);
};

const source = await readFile(
	new URL("../src/providers/electron_renderer_bridge.js", import.meta.url),
	"utf8",
);
const install = (0, eval)(`(${source})`);
const token = "deterministic-token";
const bridge = await install(token);
const { port } = bridge.endpoint();

const connect = (role, force = false, webContentsId = contents.id) => new Promise((resolve, reject) => {
	const socket = net.connect({ host: "127.0.0.1", port });
	let buffer = "";
	socket.setEncoding("utf8");
	socket.on("error", reject);
	socket.on("data", (chunk) => {
		buffer += chunk;
		const newline = buffer.indexOf("\n");
		if (newline < 0) {
			return;
		}
		const frame = JSON.parse(buffer.slice(0, newline));
		buffer = buffer.slice(newline + 1);
		resolve({ socket, frame });
	});
	socket.on("connect", () => {
		socket.write(`${JSON.stringify({
			token,
			role,
			webContentsId: role === "renderer" ? webContentsId : undefined,
			force,
		})}\n`);
	});
});

const closeRenderer = ({ socket }) => new Promise((resolve, reject) => {
	let buffer = "";
	socket.removeAllListeners("data");
	socket.on("error", reject);
	socket.on("data", (chunk) => {
		buffer += chunk;
		const newline = buffer.indexOf("\n");
		if (newline < 0) {
			return;
		}
		const frame = JSON.parse(buffer.slice(0, newline));
		if (frame.kind === "closed") {
			resolve();
		}
	});
	socket.write(`${JSON.stringify({ kind: "close" })}\n`);
});

const sendCommand = ({ socket }, id, method) => new Promise((resolve, reject) => {
	let buffer = "";
	socket.removeAllListeners("data");
	socket.on("error", reject);
	socket.on("data", (chunk) => {
		buffer += chunk;
		const newline = buffer.indexOf("\n");
		if (newline < 0) {
			return;
		}
		const frame = JSON.parse(buffer.slice(0, newline));
		if (frame.kind === "cdp" && frame.envelope?.id === id) {
			resolve(frame.envelope);
		}
	});
	socket.write(`${JSON.stringify({
		kind: "cdp",
		envelope: { id, method, params: {} },
	})}\n`);
});

const control = await connect("control");
const controlFrames = [];
const controlWaiters = [];
{
	let buffer = "";
	control.socket.removeAllListeners("data");
	control.socket.on("data", (chunk) => {
		buffer += chunk;
		for (;;) {
			const newline = buffer.indexOf("\n");
			if (newline < 0) {
				break;
			}
			const frame = JSON.parse(buffer.slice(0, newline));
			buffer = buffer.slice(newline + 1);
			const waiter = controlWaiters.findIndex((candidate) => candidate.predicate(frame));
			if (waiter >= 0) {
				const [{ resolve, timer }] = controlWaiters.splice(waiter, 1);
				clearTimeout(timer);
				resolve(frame);
			} else {
				controlFrames.push(frame);
			}
		}
	});
}
const waitForControlFrame = (predicate) => new Promise((resolve, reject) => {
	const buffered = controlFrames.findIndex(predicate);
	if (buffered >= 0) {
		resolve(controlFrames.splice(buffered, 1)[0]);
		return;
	}
	const timer = setTimeout(() => {
		reject(new Error("timed out waiting for a control frame"));
	}, 5000);
	controlWaiters.push({ predicate, resolve, timer });
});
let controlFrameId = 0;
const sendControl = async (frame) => {
	const id = ++controlFrameId;
	control.socket.write(`${JSON.stringify({ ...frame, id })}\n`);
	await waitForControlFrame((candidate) => candidate.kind === "ack" && candidate.id === id);
};

const normal = await connect("renderer");
console.log(`normal: ${normal.frame.ready ? "created" : "ownership-conflict"}`);
console.log(`connection-after-conflict: targets=${bridge.list().length}`);

const forced = await connect("renderer", true);
console.log(`force: ${forced.frame.ready ? "created" : "failed"} outcome=${forced.frame.stolen ? "stolen" : "created"}`);
console.log(`force-lifecycle: detach=${fakeDebugger.detachCount} attach=${fakeDebugger.attachCount}`);
await closeRenderer(forced);
console.log(`released: attached=${fakeDebugger.isAttached()}`);

const raced = await Promise.all([connect("renderer"), connect("renderer")]);
const created = raced.filter(({ frame }) => frame.ready);
const conflicts = raced.filter(({ frame }) => !frame.ready);
console.log(`race: created=${created.length} ownership-conflicts=${conflicts.length}`);
await closeRenderer(created[0]);

const reattached = await connect("renderer");
console.log(`reattach-after-release: ${reattached.frame.ready ? "created" : "failed"}`);
await closeRenderer(reattached);

fakeDebugger.failNextCommand = "Debugger.enable";
const failedInitialization = await connect("renderer");
const failedResponse = await sendCommand(failedInitialization, 1, "Debugger.enable");
console.log(`initialization: ${failedResponse.error ? "failed" : "unexpected-success"}`);
await closeRenderer(failedInitialization);
console.log(`initialization-cleanup: attached=${fakeDebugger.isAttached()}`);
const retry = await connect("renderer");
console.log(`retry-after-initialization-failure: ${retry.frame.ready ? "created" : "failed"}`);
await closeRenderer(retry);

// Renderer discovery is pushed by Electron's own lifecycle events, never polled, and stays silent
// until a client asks for it.
createWebContents(9, 4243, "quiet");
console.log(`discovery-while-disabled: buffered=${controlFrames.length}`);
await sendControl({ kind: "setDiscovery", enabled: true });
const discovered = createWebContents(11, 4244, "discovered");
const discoveredFrame = await waitForControlFrame(
	(frame) => frame.kind === "targetCreated" && frame.target?.webContentsId === discovered.id,
);
console.log(
	`discovery: ${discoveredFrame.kind} waiting=${discoveredFrame.target.waitingForDebugger} attached=${discoveredFrame.target.attached}`,
);
discovered.emit("did-navigate");
const changedFrame = await waitForControlFrame(
	(frame) => frame.kind === "targetInfoChanged" && frame.target?.webContentsId === discovered.id,
);
console.log(`discovery-update: ${changedFrame.kind}`);

// A renderer created while wait-for-debugger is armed is genuinely paused before its first script.
await sendControl({ kind: "setWaitForDebuggerOnStart", enabled: true });
const blocked = createWebContents(13, 4245, "blocked");
const blockedFrame = await waitForControlFrame(
	(frame) => frame.kind === "targetCreated" && frame.target?.webContentsId === blocked.id,
);
console.log(
	`wait-for-debugger: waiting=${blockedFrame.target.waitingForDebugger} resumed=${blocked.debugger.runIfWaitingCount}`,
);
const blockedClient = await connect("renderer", false, blocked.id);
console.log(
	`wait-for-debugger-attach: ${blockedClient.frame.ready ? "created" : "failed"} extra-attach=${blocked.debugger.attachCount}`,
);
await closeRenderer(blockedClient);
console.log(
	`wait-for-debugger-resume: resumed=${blocked.debugger.runIfWaitingCount} attached=${blocked.debugger.isAttached()}`,
);

// Disarming the flag must resume renderers nobody attached to instead of leaving them frozen.
const abandoned = createWebContents(15, 4246, "abandoned");
await waitForControlFrame(
	(frame) => frame.kind === "targetCreated" && frame.target?.webContentsId === abandoned.id,
);
await sendControl({ kind: "setWaitForDebuggerOnStart", enabled: false });
await waitForControlFrame(
	(frame) => frame.kind === "targetInfoChanged"
		&& frame.target?.webContentsId === abandoned.id
		&& frame.target.waitingForDebugger === false,
);
console.log(
	`disarm-resumes-blocked: resumed=${abandoned.debugger.runIfWaitingCount} attached=${abandoned.debugger.isAttached()}`,
);

control.socket.write(`${JSON.stringify({ kind: "dispose" })}\n`);
await new Promise((resolve) => control.socket.once("close", resolve));
