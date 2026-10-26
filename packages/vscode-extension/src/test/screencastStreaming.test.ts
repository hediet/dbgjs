import assert from "node:assert/strict";
import { execFile } from "node:child_process";
import { existsSync } from "node:fs";
import { mkdtemp, readFile, rm, writeFile } from "node:fs/promises";
import { tmpdir } from "node:os";
import { join, resolve } from "node:path";
import test from "node:test";
import { pathToFileURL } from "node:url";
import { promisify } from "node:util";
import { parseEndpointFile } from "../daemonClient.js";
import { connectDaemon, type DaemonConnection } from "../daemonTransport.js";
import { DbgServiceClient } from "../dbgServiceClient.js";
import type { ScreencastApi } from "../generated/interfaces.js";
import { unwrapRpcResult } from "../rpcResult.js";

const execute = promisify(execFile);

type Message = typeof ScreencastApi.members.screencast._serverStream;

/** Animates continuously (so frames keep coming) and records the input it receives. */
const page = `<!doctype html>
<body style="margin:0;height:100vh;background:#123">
<script>
globalThis.received = [];
for (const type of ["mousedown", "mouseup", "wheel", "keydown"]) {
	addEventListener(type, event => received.push(type === "keydown" ? "keydown:" + event.key : type));
}
let hue = 0;
setInterval(() => { document.body.style.background = "hsl(" + (hue = (hue + 7) % 360) + ",50%,30%)"; }, 30);
</script>`;

function browserExecutable(): string | undefined {
	return [
		process.env.DBGJS_TEST_BROWSER,
		"C:/Program Files (x86)/Microsoft/Edge/Application/msedge.exe",
		"C:/Program Files/Google/Chrome/Application/chrome.exe",
		"/usr/bin/google-chrome",
		"/usr/bin/chromium",
	].find((candidate): candidate is string => candidate !== undefined && existsSync(candidate));
}

test("generated TS client drives a duplex screencast through the Rust daemon", { timeout: 180_000 }, async t => {
	const executable = browserExecutable();
	if (!executable) {
		t.skip("no Chromium-based browser found; set DBGJS_TEST_BROWSER");
		return;
	}
	const root = await mkdtemp(join(tmpdir(), "dbgjs-screencast-streaming-"));
	const repository = resolve("../..");
	const targetDirectory = process.env.CARGO_TARGET_DIR ?? join(repository, "target");
	const binarySuffix = process.platform === "win32" ? ".exe" : "";
	const cli = join(targetDirectory, "debug", `dbgjs${binarySuffix}`);
	const state = join(root, "service.json");
	const environment = {
		...process.env,
		DBGJS_SERVICE_STATE: state,
		DBGJS_SERVICE_EXE: join(targetDirectory, "debug", `dbgjs-service${binarySuffix}`),
	};
	const command = (args: string[]) => execute(cli, args, { env: environment, timeout: 60_000 });
	const links: DaemonConnection[] = [];
	let started = false;
	try {
		await command(["context", "list"]);
		started = true;
		const endpoint = parseEndpointFile(JSON.parse(await readFile(state, "utf8")));
		const link = await connectDaemon(endpoint.address, endpoint.token, () => {});
		links.push(link);
		const client = new DbgServiceClient(link.connection);
		const pagePath = join(root, "page.html");
		await writeFile(pagePath, page);
		const connectionRef = { contextId: "screencast-streaming", connectionId: "browser" };
		await client.contexts.put_context({ contextId: connectionRef.contextId, kind: "named", displayName: null });
		await client.contexts.put_connection({
			connectionRef,
			configuration: {
				kind: "chrome", url: pathToFileURL(pagePath).href, executable, headless: true,
				userDataDir: join(root, "profile"), args: ["--window-size=640,480"],
			},
		});
		await client.contexts.connect_connection({ connectionRef });
		let pageTarget;
		for (let attempt = 0; attempt < 100 && !pageTarget; attempt++) {
			const context = await client.contexts.get_context({ contextId: connectionRef.contextId });
			pageTarget = context.connections
				.flatMap(connection => connection.targets)
				.find(target => target.targetType === "page");
			if (!pageTarget) await new Promise(resolve => setTimeout(resolve, 100));
		}
		assert.ok(pageTarget, "the browser exposes its page target");
		const targetRef = { connection: connectionRef, targetId: pageTarget.targetId };

		const frames: Extract<Message, { kind: "frame" }>[] = [];
		let enoughFrames!: () => void;
		const framesArrived = new Promise<void>(resolve => { enoughFrames = resolve; });
		// The page is never attached before the call: the screencast attaches it for its own duration.
		const call = client.screencast.screencast(
			{ targetRef, options: { format: "jpeg", quality: 50, size: { maxWidth: 320, maxHeight: 240 } } },
			{
				onMessage: message => {
					if (message.kind !== "frame") return;
					frames.push(message);
					// Frames only continue when the client's acknowledgements reach the service.
					void call.send({ kind: "frameAck", frameId: message.frameId });
					if (frames.length === 5) enoughFrames();
				},
			},
		);
		const outcome = call.then(() => null, (error: unknown) => error);
		await Promise.race([
			framesArrived,
			outcome.then(error => { throw new Error(`screencast ended before 5 frames: ${String(error)}`); }),
		]);
		const { deviceWidth, deviceHeight } = frames.at(-1)!.metadata;
		const center = { x: deviceWidth / 2, y: deviceHeight / 2 };
		await call.send({ kind: "mouse", type: "mousePressed", ...center, button: "left", buttons: 1, clickCount: 1 });
		await call.send({ kind: "mouse", type: "mouseReleased", ...center, button: "left", buttons: 0, clickCount: 1 });
		await call.send({ kind: "mouse", type: "mouseWheel", ...center, deltaX: 0, deltaY: 120 });
		await call.send({ kind: "key", type: "keyDown", key: "a", code: "KeyA", text: "a", windowsVirtualKeyCode: 65 });
		await call.send({ kind: "key", type: "keyUp", key: "a", code: "KeyA", windowsVirtualKeyCode: 65 });

		let received: unknown;
		for (let attempt = 0; attempt < 50; attempt++) {
			const evaluation = unwrapRpcResult(await client.targets.evaluate_target({
				targetRef, pauseEpoch: null, frameIndex: 0, expression: "JSON.stringify(globalThis.received)",
			}));
			received = JSON.parse(String(evaluation.value));
			if (Array.isArray(received) && received.length >= 4) break;
			await new Promise(resolve => setTimeout(resolve, 100));
		}
		assert.deepEqual(received, ["mousedown", "mouseup", "wheel", "keydown:a"]);

		await call.cancel("test finished");
		assert.ok(await outcome, "cancellation ends the call");
	} finally {
		for (const link of links) link.close();
		if (started) await command(["service", "stop"]);
		await rm(root, { recursive: true, force: true });
	}
});
