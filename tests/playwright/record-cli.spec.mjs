import assert from "node:assert/strict";
import { spawn } from "node:child_process";
import { randomUUID } from "node:crypto";
import { mkdir, readFile, readdir, rm, writeFile } from "node:fs/promises";
import { createServer } from "node:http";
import { homedir } from "node:os";
import { join, resolve } from "node:path";
import { chromium, expect, test } from "@playwright/test";
import { allocatePort, findPageTargetId, readCdpEndpoint, run } from "./live-test-harness.mjs";

const binaries = process.env.DBGJS_TEST_BIN_DIR ?? "target/debug";
const suffix = process.platform === "win32" ? ".exe" : "";
const cli = resolve(binaries, `dbgjs${suffix}`);
const service = resolve(binaries, `dbgjs-service${suffix}`);

test("native recorder streams timestamped MKV and uses only explicit external encoders", async () => {
	test.setTimeout(180_000);
	if (!process.env.DBGJS_TEST_BIN_DIR) {
		const build = await run("cargo", ["build", "--locked", "--bin", "dbgjs", "--bin", "dbgjs-service"], {});
		expect(build.code, build.output).toBe(0);
	}
	const root = resolve(".test-tmp", `record-${randomUUID()}`);
	await mkdir(root, { recursive: true });
	const env = {
		DBGJS_SERVICE_EXE: service,
		DBGJS_SERVICE_STATE: join(root, "service.json"),
		DBGJS_PLAYWRIGHT_PACKAGE: join(root, "intentionally-unavailable.mjs"),
	};
	let video;
	const http = createServer((request, response) => {
		if (request.url.startsWith("/capture.")) {
			const type = request.url.endsWith(".mkv") ? "video/x-matroska"
				: request.url.endsWith(".webm") ? "video/webm" : "video/mp4";
			response.writeHead(200, { "Content-Type": type, "Content-Length": video.length });
			response.end(video);
		} else {
			response.end("<title>Native recorder fixture</title><canvas width=320 height=240></canvas><script>let n=0; setInterval(()=>{const c=document.querySelector('canvas').getContext('2d'); c.fillStyle=++n%2?'coral':'lightblue';c.fillRect(0,0,320,240)},100)</script>");
		}
	});
	await new Promise((done) => http.listen(0, "127.0.0.1", done));
	const origin = `http://127.0.0.1:${http.address().port}`;
	const port = await allocatePort();
	const browser = await chromium.launchPersistentContext(join(root, "profile"), {
		headless: true, args: [`--remote-debugging-port=${port}`],
	});
	const page = browser.pages()[0];
	const command = async (args) => {
		const result = await run(cli, args, env);
		assert.equal(result.code, 0, result.output);
		return result;
	};
	const play = async (path, bytes) => {
		video = bytes;
		const playback = await browser.newPage();
		try {
			await playback.goto(origin);
			return await playback.evaluate(async (path) => {
				const video = document.createElement("video");
				video.muted = true;
				document.body.append(video);
				const outcome = new Promise((done) => {
					video.onloadeddata = () => done({
						decoded: true, width: video.videoWidth, height: video.videoHeight, duration: video.duration,
					});
					video.onerror = () => done({ decoded: false, error: video.error.code, message: video.error.message });
					setTimeout(() => done({ timeout: true }), 10_000);
				});
				video.src = path;
				void video.play().catch(() => {});
				return outcome;
			}, path);
		} finally {
			await playback.close();
		}
	};
	try {
		await page.goto(origin);
		await command(["context", "create", "--context", ":record-test"]);
		await command(["connection", "add", await readCdpEndpoint(port),
			"--context", ":record-test", "--connection", "browser", "--connect"]);
		const target = await findPageTargetId(cli, ":record-test", "browser", origin, env);
		const scope = ["--context", ":record-test", "--connection", "browser", "--target", target];
		const mkv = join(root, "capture.mkv");
		await command(["target", "record", "--output", mkv, "--duration-ms", "700", ...scope]);
		const nativeBytes = await readFile(mkv);
		assert.equal(nativeBytes.subarray(0, 4).toString("hex"), "1a45dfa3");
		assert.ok(nativeBytes.includes(Buffer.from("V_MJPEG")));

		const decoded = await play("/capture.mkv", nativeBytes);
		console.log("Chromium MJPEG/Matroska playback:", JSON.stringify(decoded));
		expect(decoded).toEqual({
			decoded: false, error: 4,
			message: "PipelineStatus::DEMUXER_ERROR_NO_SUPPORTED_STREAMS: FFmpegDemuxer: no supported streams",
		});

		const capture = async (extra) => {
			const child = spawn(cli, ["target", "record", ...extra, "--duration-ms", "300", ...scope], {
				env: { ...process.env, ...env }, stdio: ["ignore", "pipe", "pipe"],
			});
			const chunks = [];
			let stderr = "";
			child.stdout.on("data", (chunk) => chunks.push(chunk));
			child.stderr.on("data", (chunk) => { stderr += chunk; });
			const code = await new Promise((done, reject) => {
				child.once("error", reject);
				child.once("close", done);
			});
			assert.equal(code, 0, stderr);
			return Buffer.concat(chunks);
		};
		const streamed = await capture(["--output", "-", "--format", "mkv"]);
		assert.equal(streamed.subarray(0, 4).toString("hex"), "1a45dfa3");
		assert.ok(streamed.includes(Buffer.from("V_MJPEG")));
		const manifest = JSON.parse(await readFile(resolve("node_modules/playwright-core/browsers.json"), "utf8"));
		const revision = manifest.browsers.find((entry) => entry.name === "ffmpeg").revision;
		const cache = process.env.PLAYWRIGHT_BROWSERS_PATH === "0" ? resolve("node_modules/playwright-core/.local-browsers")
			: process.env.PLAYWRIGHT_BROWSERS_PATH ?? (process.platform === "win32" ? join(process.env.LOCALAPPDATA, "ms-playwright")
				: process.platform === "darwin" ? join(homedir(), "Library", "Caches", "ms-playwright")
					: join(process.env.XDG_CACHE_HOME ?? join(homedir(), ".cache"), "ms-playwright"));
		const encoder = process.env.DBGJS_TEST_FFMPEG ?? join(cache, `ffmpeg-${revision}`,
			process.platform === "win32" ? "ffmpeg-win64.exe" : process.platform === "darwin" ? "ffmpeg-mac" : "ffmpeg-linux");
		const webm = join(root, "capture.webm");
		await command(["target", "record", "--output", webm, "--duration-ms", "700", "--ffmpeg", encoder, ...scope]);
		const webmBytes = await readFile(webm);
		assert.equal(webmBytes.subarray(0, 4).toString("hex"), "1a45dfa3");
		const webmPlayback = await play("/capture.webm", webmBytes);
		expect(webmPlayback).toMatchObject({ decoded: true, width: 1280, height: 720 });
		expect(webmPlayback.duration).toBeGreaterThanOrEqual(0.65);
		expect(webmPlayback.duration).toBeLessThan(1.5);
		await command(["target", "record", "--output", join(root, "override.webm"), "--duration-ms", "100",
			"--ffmpeg", encoder, "--ffmpeg-args", '["-c:v","vp8","-b:v","400k","-threads","1"]', ...scope]);
		const png = join(root, "decoded.png");
		const decode = await run(encoder, ["-hide_banner", "-loglevel", "error", "-i", mkv, "-frames:v", "1", png], {});
		assert.equal(decode.code, 0, decode.output);
		assert.equal((await readFile(png)).subarray(1, 4).toString(), "PNG");
		if (process.env.DBGJS_TEST_FFMPEG) {
			const mp4 = join(root, "capture.mp4");
			await command(["target", "record", "--output", mp4, "--duration-ms", "700", "--ffmpeg", encoder, ...scope]);
			const fragmented = await capture(["--output", "-", "--format", "mp4", "--ffmpeg", encoder]);
			assert.equal(fragmented.subarray(4, 8).toString(), "ftyp");
			assert.ok(fragmented.includes(Buffer.from("moof")));
			const fragmentedPath = join(root, "stream.mp4");
			await writeFile(fragmentedPath, fragmented);
			const mp4Playback = await play("/capture.mp4", await readFile(mp4));
			expect(mp4Playback).toMatchObject({ decoded: true, width: 1280, height: 720 });
			expect(mp4Playback.duration).toBeGreaterThanOrEqual(0.65);
			expect(mp4Playback.duration).toBeLessThan(1.5);
			for (const path of [mkv, webm, mp4, fragmentedPath]) {
				const decoded = await run(encoder, ["-hide_banner", "-loglevel", "error", "-i", path, "-f", "null", "-"], {});
				assert.equal(decoded.code, 0, decoded.output);
			}
			const probe = join(resolve(encoder, ".."), `ffprobe${suffix}`);
			for (const path of [mkv, webm, mp4]) {
				const result = await run(probe, ["-v", "error", "-show_entries",
					"stream=codec_name,width,height,start_time,time_base,avg_frame_rate:frame=pts_time,duration_time:format=duration", "-of", "json", path], {});
				assert.equal(result.code, 0, result.output);
				const data = JSON.parse(result.output);
				assert.equal(data.streams[0].width, 1280);
				assert.equal(data.streams[0].height, 720);
				assert.equal(data.streams[0].codec_name, path.endsWith(".mkv") ? "mjpeg" : path.endsWith(".webm") ? "vp8" : "h264");
				assert.ok(Number(data.frames.at(-1).pts_time) >= 0.65, result.output);
				if (!path.endsWith(".mkv")) {
					assert.ok(Number(data.format.duration) >= 0.65 && Number(data.format.duration) < 1.5, result.output);
				}
			}
		}
		await page.evaluate(() => {
			const canvas = document.querySelector("canvas");
			canvas.width = 1280;
			canvas.height = 720;
			const context = canvas.getContext("2d");
			const image = context.createImageData(1280, 720);
			for (let i = 0; i < image.data.length; i += 4) {
				image.data[i] = Math.random() * 256;
				image.data[i + 1] = Math.random() * 256;
				image.data[i + 2] = Math.random() * 256;
				image.data[i + 3] = 255;
			}
			context.putImageData(image, 0, 0);
		});
		const stalled = spawn(cli, ["target", "record", "--output", "-", "--format", "mkv",
			"--duration-ms", "2000", ...scope], {
			env: { ...process.env, ...env }, stdio: ["ignore", "pipe", "pipe"],
		});
		let stalledError = "";
		stalled.stderr.on("data", (bytes) => { stalledError += bytes; });
		let watchdog;
		try {
			const code = await new Promise((done, reject) => {
				watchdog = setTimeout(() => reject(new Error("stalled stdout prevented recorder exit")), 20_000);
				stalled.once("error", reject);
				stalled.once("exit", done);
			});
			assert.notEqual(code, 0, stalledError);
			assert.match(stalledError, /recording output blocked for 10 seconds/);
		} finally {
			clearTimeout(watchdog);
			stalled.stdout.destroy();
			if (stalled.exitCode === null) stalled.kill();
		}
		await page.evaluate(() => {
			document.querySelector("canvas").width = 320;
			document.querySelector("canvas").height = 240;
		});
		const before = await readdir(root);
		for (const extra of [
			["--output", mkv],
			["--output", join(root, "bad.webm"), "--ffmpeg", join(root, "missing-ffmpeg")],
			["--output", join(root, "bad.webm"), "--ffmpeg", encoder, "--ffmpeg-args", '["-c:v","nonexistent-codec"]'],
			["--output", join(root, "bad.mkv"), "--format", "webm"],
		]) {
			const result = await run(cli, ["target", "record", ...extra, "--duration-ms", "100", ...scope], env);
			assert.notEqual(result.code, 0, result.output);
		}
		assert.deepEqual(await readdir(root), before);
		assert.ok((await readFile(mkv)).equals(nativeBytes), "existing recording must not change");
		await command(["target", "cdp", "Runtime.evaluate", "--params", '{"expression":"6 * 7"}', ...scope]);
	} finally {
		await run(cli, ["service", "stop"], env);
		await browser.close();
		await new Promise((done) => http.close(done));
		await rm(root, { recursive: true, force: true });
	}
});
