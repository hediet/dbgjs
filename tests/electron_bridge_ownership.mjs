import { EventEmitter } from "node:events";
import { readFile } from "node:fs/promises";
import net from "node:net";

class FakeDebugger extends EventEmitter {
	constructor() {
		super();
		this.attached = true;
		this.attachCount = 0;
		this.detachCount = 0;
		this.failNextCommand = undefined;
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
		this.emit("detach", {}, "detached");
	}

	async sendCommand(method) {
		if (method === this.failNextCommand) {
			this.failNextCommand = undefined;
			throw new Error(`deterministic ${method} initialization failure`);
		}
		return {};
	}
}

const fakeDebugger = new FakeDebugger();
const contents = {
	id: 7,
	debugger: fakeDebugger,
	isDestroyed: () => false,
	getOSProcessId: () => 4242,
	getType: () => "window",
	getTitle: () => "deterministic renderer",
	getURL: () => "file:///renderer.html",
};
globalThis.require = (name) => {
	if (name === "electron") {
		return {
			webContents: {
				fromId: (id) => id === contents.id ? contents : undefined,
				getAllWebContents: () => [contents],
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

const connect = (role, force = false) => new Promise((resolve, reject) => {
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
			webContentsId: role === "renderer" ? contents.id : undefined,
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

control.socket.write(`${JSON.stringify({ kind: "dispose" })}\n`);
await new Promise((resolve) => control.socket.once("close", resolve));
