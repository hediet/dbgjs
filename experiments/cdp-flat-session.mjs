import { spawn } from "node:child_process";
import { mkdtemp, rm } from "node:fs/promises";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { isDeepStrictEqual } from "node:util";
import {
	CdpConnection,
	CdpRecording,
	RecordingPeer,
	StrictReplayPeer,
} from "./cdp-recording.mjs";

async function main() {
	const chrome = await launchChrome();
	const recordingDirectory =
		process.env.CDP_RECORDING_DIR ??
		(await mkdtemp(join(tmpdir(), "cdp-recording-")));
	const keepRecording = process.env.CDP_RECORDING_DIR !== undefined;
	try {
		const livePeer = await WebSocketPeer.connect(chrome.browserWebSocketUrl);
		const recording = new CdpRecording({
			containsSensitiveData: true,
			scenario: "flat-session",
		});
		const recordingPeer = new RecordingPeer(livePeer, recording);
		const liveResult = await runScenario(new CdpConnection(recordingPeer), 0);

		await recording.save(recordingDirectory);
		const loadedRecording = await CdpRecording.load(recordingDirectory);
		const replayPeer = new StrictReplayPeer(loadedRecording.materialize());
		const replayResult = await runScenario(new CdpConnection(replayPeer), 10_000);
		replayPeer.assertComplete();

		if (!isDeepStrictEqual(replayResult, liveResult)) {
			throw new Error("Replay produced a different semantic result");
		}

		console.log(
			JSON.stringify(
				{
					...liveResult,
					recordingId: loadedRecording.recordingId,
					recordedFrames: loadedRecording.frames.length,
					recordedBlobs: loadedRecording.blobCount,
					recordingDirectory: keepRecording
						? recordingDirectory
						: "<temporary>",
					replayRequestIdOffset: 10_000,
				},
				null,
				2,
			),
		);
	} finally {
		await chrome.dispose();
		if (!keepRecording) {
			await rm(recordingDirectory, { recursive: true, force: true });
		}
	}
}

async function runScenario(cdp, requestIdOffset) {
	const id = (value) => value + requestIdOffset;
	const { targetId } = await cdp.call(
		undefined,
		id(1),
		"Target.createTarget",
		{ url: "about:blank" },
	);
	const { sessionId } = await cdp.call(
		undefined,
		id(2),
		"Target.attachToTarget",
		{ targetId, flatten: true },
	);

	const eventPromise = cdp.waitForEvent(
		sessionId,
		"Runtime.executionContextCreated",
	);

	// This intentionally reuses the same numeric id on the root and child session.
	// If CDP scopes ids by flattened session, both calls remain independently routable.
	const [browserVersion, runtimeEnabled] = await Promise.all([
		cdp.call(undefined, id(777), "Browser.getVersion", {}),
		cdp.call(sessionId, id(777), "Runtime.enable", {}),
	]);
	const contextCreated = await eventPromise;

	const evaluation = await cdp.call(
		sessionId,
		id(778),
		"Runtime.evaluate",
		{
			expression: "({ answer: 6 * 7 })",
			returnByValue: true,
		},
	);

	if (!browserVersion.product.startsWith("Chrome/")) {
		throw new Error(`Unexpected browser product: ${browserVersion.product}`);
	}
	if (Object.keys(runtimeEnabled).length !== 0) {
		throw new Error("Runtime.enable unexpectedly returned a non-empty result");
	}
	if (contextCreated.params.context.auxData?.type !== "default") {
		throw new Error("Did not observe the child session's default execution context");
	}
	if (evaluation.result.value?.answer !== 42) {
		throw new Error("Session-scoped Runtime.evaluate returned the wrong value");
	}

	await cdp.call(undefined, id(3), "Browser.close", {});
	cdp.close();

	return {
		product: browserVersion.product,
		protocolVersion: browserVersion.protocolVersion,
		sessionId,
		reusedRequestId: 777,
		sessionEvent: contextCreated.method,
		evaluation: evaluation.result.value,
	};
}

async function launchChrome() {
	const chromePath =
		process.env.CHROME_PATH ??
		"C:\\Program Files\\Google\\Chrome\\Application\\chrome.exe";
	const userDataDir = await mkdtemp(join(tmpdir(), "cdp-flat-session-"));
	const chromeProcess = spawn(
		chromePath,
		[
			"--headless=new",
			"--remote-debugging-port=0",
			`--user-data-dir=${userDataDir}`,
			"--no-first-run",
			"--no-default-browser-check",
			"about:blank",
		],
		{ stdio: ["ignore", "ignore", "pipe"] },
	);

	try {
		const browserWebSocketUrl = await readBrowserWebSocketUrl(chromeProcess);
		return {
			browserWebSocketUrl,
			async dispose() {
				if (chromeProcess.exitCode === null) {
					chromeProcess.kill();
				}
				if (chromeProcess.exitCode === null) {
					await new Promise((resolve) =>
						chromeProcess.once("exit", resolve),
					);
				}
				await rm(userDataDir, { recursive: true, force: true });
			},
		};
	} catch (error) {
		if (chromeProcess.exitCode === null) {
			chromeProcess.kill();
		}
		await rm(userDataDir, { recursive: true, force: true });
		throw error;
	}
}

async function readBrowserWebSocketUrl(chromeProcess) {
	let stderr = "";
	for await (const chunk of chromeProcess.stderr) {
		stderr += chunk.toString();
		const match = stderr.match(/DevTools listening on (ws:\/\/[^\s]+)/);
		if (match) {
			return match[1];
		}
	}
	throw new Error(`Chrome exited before exposing CDP:\n${stderr}`);
}

class WebSocketPeer {
	static async connect(url) {
		const socket = new WebSocket(url);
		await new Promise((resolve, reject) => {
			socket.addEventListener("open", resolve, { once: true });
			socket.addEventListener(
				"error",
				() => reject(new Error(`Could not connect to ${url}`)),
				{ once: true },
			);
		});
		return new WebSocketPeer(socket);
	}

	#listener;

	constructor(socket) {
		this.socket = socket;
		socket.addEventListener("message", ({ data }) => {
			this.#listener?.(JSON.parse(data), data);
		});
	}

	onMessage(listener) {
		this.#listener = listener;
	}

	send(message) {
		this.socket.send(JSON.stringify(message));
	}

	close() {
		this.socket.close();
	}
}

await main();
