import { EventEmitter } from "node:events";
import assert from "node:assert/strict";
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
const windows = [{ id: 1, webContents: contents, isDestroyed: () => false }];
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
			BrowserWindow: {
				fromWebContents: (contents) => windows.find(
					(window) => window.webContents === contents || contents.ownerWindow === window,
				),
			},
			webContents: {
				fromId: (id) => allContents.find((candidate) => candidate.id === id),
				fromDevToolsTargetId: (id) => allContents.find((candidate) => `native-${candidate.id}` === id),
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
const tokenA = "deterministic-token-a";
const tokenB = "deterministic-token-b";
const tokenC = "deterministic-token-c";
const [bridge, parallelBridge, thirdBridge] = await Promise.all([
	install(tokenA),
	install(tokenB),
	install(tokenC),
]);
const { port } = bridge.endpoint();
assert.equal(parallelBridge.endpoint().port, port);
assert.equal(thirdBridge.endpoint().port, port);
assert.deepEqual(bridge.resolveBrowserTargets(["native-7", "missing"]), { "native-7": 7 });
assert.deepEqual(parallelBridge.resolveBrowserTargets(["native-7"]), { "native-7": 7 });
assert.equal(fakeDebugger.attachCount, 0, "target identity mapping must not attach the Electron debugger");
assert.equal(fakeDebugger.detachCount, 0, "target identity mapping must not evict an existing debugger");
assert.equal(bridge.list()[0].primaryWindowId, 1);
const shared = new FakeWebContents(8, contents.processId, "shared renderer", "file:///shared.html");
shared.ownerWindow = windows[0];
allContents.push(shared);
assert.equal(bridge.list().find((target) => target.webContentsId === shared.id).primaryWindowId, undefined);
windows.push({ id: 2, webContents: shared, isDestroyed: () => false });
shared.ownerWindow = windows[1];
assert.equal(bridge.list().find((target) => target.webContentsId === shared.id).primaryWindowId, 2);
windows[1].isDestroyed = () => true;
assert.equal(bridge.list().find((target) => target.webContentsId === shared.id).primaryWindowId, undefined);
allContents.pop();

const connect = (
	clientToken,
	role,
	force = false,
	webContentsId = contents.id,
	targetPort = port,
) => new Promise((resolve, reject) => {
	const socket = net.connect({ host: "127.0.0.1", port: targetPort });
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
			token: clientToken,
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

const waitForRendererClosed = ({ socket }) => new Promise((resolve, reject) => {
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
			resolve(frame);
		}
	});
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

const createControl = async (clientToken, targetPort = port) => {
	const control = await connect(clientToken, "control", false, contents.id, targetPort);
	const frames = [];
	const waiters = [];
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
			const waiter = waiters.findIndex((candidate) => candidate.predicate(frame));
			if (waiter >= 0) {
				const [{ resolve, timer }] = waiters.splice(waiter, 1);
				clearTimeout(timer);
				resolve(frame);
			} else {
				frames.push(frame);
			}
		}
	});
	const waitForFrame = (predicate) => new Promise((resolve, reject) => {
		const buffered = frames.findIndex(predicate);
		if (buffered >= 0) {
			resolve(frames.splice(buffered, 1)[0]);
			return;
		}
		const timer = setTimeout(() => {
			reject(new Error("timed out waiting for a control frame"));
		}, 5000);
		waiters.push({ predicate, resolve, timer });
	});
	let frameId = 0;
	const send = async (frame) => {
		const id = ++frameId;
		control.socket.write(`${JSON.stringify({ ...frame, id })}\n`);
		await waitForFrame((candidate) => candidate.kind === "ack" && candidate.id === id);
	};
	return { ...control, frames, waitForFrame, send };
};

const control = await createControl(tokenA);
const secondControl = await createControl(tokenB);
const thirdControl = await createControl(tokenC);
const waitForControlFrame = control.waitForFrame;
const sendControl = control.send;

const normal = await connect(tokenA, "renderer");
console.log(`normal: ${normal.frame.ready ? "created" : "ownership-conflict"}`);
console.log(`connection-after-conflict: targets=${bridge.list().length}`);

const forced = await connect(tokenA, "renderer", true);
console.log(`force: ${forced.frame.ready ? "created" : "failed"} outcome=${forced.frame.stolen ? "stolen" : "created"}`);
console.log(`force-lifecycle: detach=${fakeDebugger.detachCount} attach=${fakeDebugger.attachCount}`);
const denied = await connect(tokenB, "renderer");
assert.equal(denied.frame.ready, false, "same renderer remains exclusive across logical clients");
const forcedClosed = waitForRendererClosed(forced);
const transferred = await connect(tokenB, "renderer", true);
assert.equal(transferred.frame.ready, true);
assert.equal(transferred.frame.stolen, true);
assert.match((await forcedClosed).reason, /ownership was stolen/);
await closeRenderer(transferred);
console.log(`released: attached=${fakeDebugger.isAttached()}`);

const raced = await Promise.all([
	connect(tokenA, "renderer"),
	connect(tokenB, "renderer"),
]);
const created = raced.filter(({ frame }) => frame.ready);
const conflicts = raced.filter(({ frame }) => !frame.ready);
console.log(`race: created=${created.length} ownership-conflicts=${conflicts.length}`);
await closeRenderer(created[0]);

const reattached = await connect(tokenC, "renderer");
console.log(`reattach-after-release: ${reattached.frame.ready ? "created" : "failed"}`);
await closeRenderer(reattached);

fakeDebugger.failNextCommand = "Debugger.enable";
const failedInitialization = await connect(tokenA, "renderer");
const failedResponse = await sendCommand(failedInitialization, 1, "Debugger.enable");
console.log(`initialization: ${failedResponse.error ? "failed" : "unexpected-success"}`);
await closeRenderer(failedInitialization);
console.log(`initialization-cleanup: attached=${fakeDebugger.isAttached()}`);
const retry = await connect(tokenA, "renderer");
console.log(`retry-after-initialization-failure: ${retry.frame.ready ? "created" : "failed"}`);
await closeRenderer(retry);

const rendererA = createWebContents(17, 4250, "client-a");
const rendererB = createWebContents(19, 4251, "client-b");
const rendererC = createWebContents(21, 4252, "client-c");
const ownedA = await connect(tokenA, "renderer", false, rendererA.id);
const ownedB = await connect(tokenB, "renderer", false, rendererB.id);
const ownedC = await connect(tokenC, "renderer", false, rendererC.id);
const ownedCClosed = waitForRendererClosed(ownedC);
await thirdBridge.dispose();
assert.match((await ownedCClosed).reason, /client disposed/);
assert.equal((await sendCommand(ownedA, 101, "Runtime.enable")).id, 101);
assert.equal((await sendCommand(ownedB, 102, "Runtime.enable")).id, 102);
await closeRenderer(ownedA);
await closeRenderer(ownedB);
assert.equal(bridge.endpoint().port, port, "disposing one client preserves the shared server");
assert.equal(parallelBridge.endpoint().port, port);

// Renderer discovery is pushed by Electron's own lifecycle events, never polled, and stays silent
// until a client asks for it.
createWebContents(9, 4243, "quiet");
console.log(`discovery-while-disabled: buffered=${control.frames.length}`);
await sendControl({ kind: "setDiscovery", enabled: true });
await secondControl.send({ kind: "setDiscovery", enabled: true });
const discovered = createWebContents(11, 4244, "discovered");
const [discoveredFrame, secondDiscoveredFrame] = await Promise.all([
	waitForControlFrame(
		(frame) => frame.kind === "targetCreated" && frame.target?.webContentsId === discovered.id,
	),
	secondControl.waitForFrame(
		(frame) => frame.kind === "targetCreated" && frame.target?.webContentsId === discovered.id,
	),
]);
assert.equal(secondDiscoveredFrame.target.webContentsId, discovered.id);
console.log(
	`discovery: ${discoveredFrame.kind} waiting=${discoveredFrame.target.waitingForDebugger} attached=${discoveredFrame.target.attached}`,
);
discovered.emit("did-navigate");
const changedFrame = await waitForControlFrame(
	(frame) => frame.kind === "targetInfoChanged" && frame.target?.webContentsId === discovered.id,
);
console.log(`discovery-update: ${changedFrame.kind}`);
const discoveredWindow = { id: 3, webContents: discovered, isDestroyed: () => false };
windows.push(discoveredWindow);
app.emit("browser-window-created", {}, discoveredWindow);
const ownershipFrame = await waitForControlFrame(
	(frame) => frame.kind === "targetInfoChanged"
		&& frame.target?.webContentsId === discovered.id
		&& frame.target.primaryWindowId === discoveredWindow.id,
);
assert.equal(ownershipFrame.target.primaryWindowId, 3);

// A renderer created while wait-for-debugger is armed is genuinely paused before its first script.
await sendControl({ kind: "setWaitForDebuggerOnStart", enabled: true });
await secondControl.send({ kind: "setWaitForDebuggerOnStart", enabled: true });
const blocked = createWebContents(13, 4245, "blocked");
const blockedFrame = await waitForControlFrame(
	(frame) => frame.kind === "targetCreated" && frame.target?.webContentsId === blocked.id,
);
console.log(
	`wait-for-debugger: waiting=${blockedFrame.target.waitingForDebugger} resumed=${blocked.debugger.runIfWaitingCount}`,
);
const blockedClient = await connect(tokenB, "renderer", false, blocked.id);
console.log(
	`wait-for-debugger-attach: ${blockedClient.frame.ready ? "created" : "failed"} extra-attach=${blocked.debugger.attachCount}`,
);
await closeRenderer(blockedClient);
console.log(
	`wait-for-debugger-resume: resumed=${blocked.debugger.runIfWaitingCount} attached=${blocked.debugger.isAttached()}`,
);

// Disarming the flag must resume renderers nobody attached to instead of leaving them frozen.
await sendControl({ kind: "setWaitForDebuggerOnStart", enabled: false });
const abandoned = createWebContents(15, 4246, "abandoned");
await waitForControlFrame(
	(frame) => frame.kind === "targetCreated" && frame.target?.webContentsId === abandoned.id,
);
assert.equal(abandoned.debugger.runIfWaitingCount, 0, "another client's demand keeps startup blocked");
await secondControl.send({ kind: "setWaitForDebuggerOnStart", enabled: false });
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
assert.equal(parallelBridge.endpoint().port, port, "disposing one control lease preserves peers");
assert.deepEqual(parallelBridge.resolveBrowserTargets(["native-7"]), { "native-7": 7 });
const stillDiscovered = createWebContents(23, 4253, "still-discovered");
await secondControl.waitForFrame(
	(frame) => frame.kind === "targetCreated" && frame.target?.webContentsId === stillDiscovered.id,
);
secondControl.socket.write(`${JSON.stringify({ kind: "dispose" })}\n`);
await new Promise((resolve) => secondControl.socket.once("close", resolve));

const replacementToken = "deterministic-token-replacement";
const replacement = await install(replacementToken);
const replacementPort = replacement.endpoint().port;
assert.ok(replacementPort > 0);
const replacementControl = await createControl(replacementToken, replacementPort);
assert.equal(replacement.list().some((target) => target.webContentsId === contents.id), true);
replacementControl.socket.write(`${JSON.stringify({ kind: "dispose" })}\n`);
await new Promise((resolve) => replacementControl.socket.once("close", resolve));
