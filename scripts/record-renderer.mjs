import { spawn } from "node:child_process";
import { constants } from "node:fs";
import { copyFile, mkdir, mkdtemp, rm, stat } from "node:fs/promises";
import { dirname, join, resolve } from "node:path";
import { performance } from "node:perf_hooks";
import { createInterface } from "node:readline";
import { setTimeout as delay } from "node:timers/promises";
import { pathToFileURL } from "node:url";
import { parseArgs } from "node:util";

const frameRate = 25;
const requestTimeoutMs = 5000;

export async function withTargetRelay({ dbgjs, scope = [], env = {} }, action) {
	const child = spawn(dbgjs, ["target", "relay", "--stdio", ...scope], {
		env: { ...process.env, ...env },
		stdio: ["pipe", "pipe", "pipe"],
	});
	const pending = new Map();
	const listeners = new Set();
	const failure = Promise.withResolvers();
	let nextId = 0;
	let closing = false;
	let error;
	let stderr = "";
	const fail = (cause) => {
		error ??= cause;
		failure.reject(error);
		for (const request of pending.values()) {
			clearTimeout(request.timer);
			request.reject(error);
		}
		pending.clear();
	};
	const closed = new Promise((resolveClosed) => {
		child.once("close", (code, signal) => {
			resolveClosed({ code, signal });
			if (!closing) fail(new Error(`dbgjs relay exited (${code ?? signal}): ${stderr}`));
		});
	});
	child.on("error", fail);
	child.stdin.on("error", fail);
	child.stderr.on("data", (chunk) => { stderr = (stderr + chunk).slice(-8192); });
	const lines = createInterface({ input: child.stdout });
	lines.on("line", (line) => {
		try {
			const message = JSON.parse(line);
			if (message.id !== undefined) {
				const request = pending.get(message.id);
				if (!request) return;
				pending.delete(message.id);
				clearTimeout(request.timer);
				if (message.error) {
					request.reject(new Error(`${request.method}: ${JSON.stringify(message.error)}`));
				} else {
					request.resolve(message.result);
				}
			} else {
				for (const listener of listeners) listener(message);
			}
		} catch (cause) { fail(cause); }
	});
	const relay = {
		call(method, params = {}) {
			if (error) return Promise.reject(error);
			const id = ++nextId;
			return new Promise((resolveRequest, reject) => {
				const timer = setTimeout(() => {
					pending.delete(id);
					reject(new Error(`${method} timed out after ${requestTimeoutMs}ms`));
				}, requestTimeoutMs);
				pending.set(id, { method, resolve: resolveRequest, reject, timer });
				child.stdin.write(JSON.stringify({ id, method, params }) + "\n");
			});
		},
		onEvent(listener) {
			listeners.add(listener);
			return () => listeners.delete(listener);
		},
		fail,
	};
	try {
		return await Promise.race([Promise.resolve().then(() => action(relay)), failure.promise]);
	} catch (cause) {
		error ??= cause;
		throw cause;
	} finally {
		const primaryError = error;
		closing = true;
		child.stdin.end();
		const result = await Promise.race([closed, delay(2000, undefined, { ref: false }).then(() => undefined)]);
		if (!result) {
			child.kill();
			await closed;
		}
		lines.close();
		fail(new Error("dbgjs relay closed"));
		if (!primaryError && !result) throw new Error("dbgjs relay cleanup exceeded 2 seconds");
		if (!primaryError && result.code !== 0 && result.code !== null) {
			throw new Error(`dbgjs relay exited (${result.code}): ${stderr}`);
		}
	}
}

export function validateRecordingOptions({ output, ffmpeg, durationMs = 10_000 }) {
	if (!output?.endsWith(".webm")) throw new Error("--output must end in .webm");
	if (!ffmpeg) throw new Error("--ffmpeg must point to the installed FFmpeg executable");
	if (!Number.isInteger(durationMs) || durationMs < 40 || durationMs > 300_000) {
		throw new Error("--duration-ms must be an integer from 40 to 300000");
	}
}

export async function recordRenderer({
	dbgjs, scope, env, ffmpeg, output, durationMs = 10_000, onReady,
}) {
	validateRecordingOptions({ output, ffmpeg, durationMs });
	output = resolve(output);
	await mkdir(dirname(output), { recursive: true });
	const scratch = await mkdtemp(join(dirname(output), ".dbgjs-recording-"));
	const temporaryVideo = join(scratch, "recording.webm");
	let encoder;
	let encoderClosed;
	let encoderError;
	let encoderStderr = "";
	try {
		const result = await withTargetRelay({ dbgjs, scope, env }, async (relay) => {
			encoder = spawn(ffmpeg, [
				"-hide_banner", "-loglevel", "error", "-f", "image2pipe",
				"-framerate", String(frameRate), "-c:v", "mjpeg", "-i", "pipe:0",
				"-an", "-c:v", "vp8", "-deadline", "realtime", "-speed", "8",
				"-b:v", "1M", "-threads", "1", temporaryVideo,
			], { stdio: ["pipe", "ignore", "pipe"] });
			const encoderFail = (cause) => { encoderError ??= cause; relay.fail(cause); };
			encoder.on("error", encoderFail);
			encoder.stdin.on("error", encoderFail);
			encoder.stderr.on("data", (chunk) => {
				encoderStderr = (encoderStderr + chunk).slice(-8192);
			});
			encoderClosed = new Promise((resolveClosed) => {
				encoder.once("close", (code, signal) => {
					if (code !== 0) encoderFail(new Error(
						`FFmpeg exited (${code ?? signal}): ${encoderStderr}`,
					));
					resolveClosed(code);
				});
			});
			let latestFrame;
			let capturedFrames = 0;
			const firstFrame = Promise.withResolvers();
			const removeListener = relay.onEvent((event) => {
				if (event.method !== "Page.screencastFrame") return;
				latestFrame = Buffer.from(event.params.data, "base64");
				if (!latestFrame.length) throw new Error("Empty CDP screencast frame");
				capturedFrames++;
				firstFrame.resolve();
				relay.call("Page.screencastFrameAck", {
					sessionId: event.params.sessionId,
				}).catch(relay.fail);
			});
			let started = false;
			const abort = new AbortController();
			try {
				await relay.call("Page.enable");
				await relay.call("Page.bringToFront");
				await relay.call("Page.startScreencast", {
					format: "jpeg", quality: 85, everyNthFrame: 1,
				});
				started = true;
				await relay.call("Page.captureScreenshot", { format: "jpeg" });
				const totalFrames = Math.ceil(durationMs * frameRate / 1000);
				const encode = async () => {
					await Promise.race([
						firstFrame.promise,
						delay(requestTimeoutMs, undefined, { signal: abort.signal }).then(() => {
							throw new Error("No screencast frame arrived within 5 seconds; keep the window visible");
						}),
					]);
					const start = performance.now();
					for (let index = 0; index < totalFrames; index++) {
						if (abort.signal.aborted) throw abort.signal.reason;
						await new Promise((resolveWrite, reject) => {
							encoder.stdin.write(latestFrame, (cause) => cause ? reject(cause) : resolveWrite());
						});
						await delay(Math.max(0, start + (index + 1) * 1000 / frameRate - performance.now()),
							undefined, { signal: abort.signal });
					}
				};
				await Promise.all([encode(), onReady?.(relay)]);
				return { output, capturedFrames, encodedFrames: totalFrames, durationMs };
			} finally {
				abort.abort(new Error("Recording finished"));
				try {
					if (started) await relay.call("Page.stopScreencast");
				} finally { removeListener(); }
			}
		});
		encoder.stdin.end();
		const exitCode = await Promise.race([
			encoderClosed,
			delay(10_000, undefined, { ref: false }).then(() => {
				throw new Error("FFmpeg finalization exceeded 10 seconds");
			}),
		]);
		if (encoderError || exitCode !== 0) throw encoderError ?? new Error("FFmpeg failed");
		const bytes = (await stat(temporaryVideo)).size;
		if (!bytes) throw new Error("Empty video output");
		await copyFile(temporaryVideo, output, constants.COPYFILE_EXCL);
		return { ...result, bytes };
	} finally {
		if (encoder && encoder.exitCode === null && encoder.signalCode === null) {
			encoder.kill();
			await encoderClosed;
		}
		await rm(scratch, { recursive: true });
	}
}

if (process.argv[1] && pathToFileURL(resolve(process.argv[1])).href === import.meta.url) {
	try {
		const { values } = parseArgs({ options: {
			dbgjs: { type: "string", default: "dbgjs" },
			ffmpeg: { type: "string" },
			output: { type: "string" },
			"duration-ms": { type: "string", default: "10000" },
			context: { type: "string" },
			connection: { type: "string" },
			target: { type: "string" },
		} });
		const scope = ["context", "connection", "target"].flatMap((name) =>
			values[name] === undefined ? [] : [`--${name}`, values[name]]);
		console.log(JSON.stringify(await recordRenderer({
			dbgjs: values.dbgjs, ffmpeg: values.ffmpeg, output: values.output,
			durationMs: Number(values["duration-ms"]), scope,
		})));
	} catch (error) {
		console.error(error);
		process.exitCode = 1;
	}
}
