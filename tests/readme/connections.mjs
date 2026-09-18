import assert from "node:assert/strict";
import { spawn } from "node:child_process";
import { once } from "node:events";
import { mkdtemp, readFile, rm, writeFile } from "node:fs/promises";
import { createServer } from "node:http";
import { tmpdir } from "node:os";
import { join, resolve } from "node:path";
import { setTimeout as delay } from "node:timers/promises";
import { fileURLToPath, pathToFileURL } from "node:url";
import { parseArgs } from "node:util";
import { findChromeExecutable, run } from "../playwright/live-test-harness.mjs";
import { createRecorder } from "./recorder.mjs";

// Standalone smoke: node tests\readme\connections.mjs --bin-dir target\debug --output artifacts\readme-connections
export async function recordConnections({ cli, service, output }) {
	const directory = await mkdtemp(join(tmpdir(), "dbgjs-readme-connections-"));
	const environment = {
		DBGJS_SERVICE_EXE: service,
		DBGJS_SERVICE_STATE: join(directory, "service.json"),
		DBGJS_SOURCE_MAP_CACHE: join(directory, "source-maps"),
	};
	const replacements = [[directory, "CONNECTIONS_DIR"]];
	let server;
	let node;
	let nodeClosed;
	let nodeOutput = "";
	let serviceUsed = false;
	let failure;
	let recording;
	try {
		const recorder = await createRecorder({
			cli, environment, output, name: "connections", replacements,
		});
		const { command, json, steps } = recorder;
		const html = await readFile(new URL("./website.html", import.meta.url));
		server = createServer((request, response) => {
			if (request.url === "/") {
				response.writeHead(200, { "content-type": "text/html; charset=utf-8" });
				response.end(html);
			} else {
				response.writeHead(request.url === "/favicon.ico" ? 204 : 404);
				response.end();
			}
		});
		server.listen(0, "127.0.0.1");
		await once(server, "listening");
		const url = `http://127.0.0.1:${server.address().port}/`;
		replacements.push([url, "WEBSITE_URL"]);
		assert.equal(await (await fetch(url)).text(), html.toString());

		serviceUsed = true;
		await command("web-context", [
			"context", "create", ":readme-web", "Local website", "--set",
		], "exact", (text) => assert.match(text, /readme-web/));
		await command("web-playwright-connect", [
			"connection", "add", "--playwright", url, "--connection", "browser", "--connect", "--set",
		], "live", async (text) => {
			assert.match(text, /Playwright bundled Chromium/);
			assert.ok(text.includes(url), "The launched page must be in the connection inventory.");
			await assertBrowser(json, "browser", "playwright", url, replacements, "PLAYWRIGHT_TARGET");
		}, { maxOutputLines: 14 });
		await command("web-playwright-input", ["playwright", [
			'await page.getByLabel("Name", { exact: true }).fill("Ada");',
			'await page.getByLabel("Name", { exact: true }).press("Enter");',
			'await page.getByRole("status").filter({ hasText: "Hello, Ada!" }).waitFor();',
			'return { greeting: await page.getByRole("status").textContent(), ...await page.evaluate(() => websiteState) };',
		].join("\n")], "exact", (text) => assert.deepEqual(JSON.parse(text), {
			greeting: "Hello, Ada!", inputEvents: 1, submissions: 1, name: "Ada",
		}));
		await command("web-playwright-disconnect", [
			"connection", "disconnect", "--connection", "browser",
		], "live", async (text) => {
			assert.match(text, /disconnected/i);
			await assertDisconnected(json, "browser");
		});

		const chrome = await findChromeExecutable();
		replacements.push([chrome, "CHROME_EXE"]);
		await command("web-chrome-connect", [
			"connection", "add", "--chrome", url, "--connection", "chrome",
			"--executable", chrome, "--user-data-dir", join(directory, "chrome-profile"), "--connect", "--set",
		], "live", async (text) => {
			assert.match(text, /chrome/);
			assert.ok(text.includes(url), "Installed Chrome must load the local website.");
			await assertBrowser(json, "chrome", "chrome", url, replacements, "CHROME_TARGET");
		}, { maxOutputLines: 14 });
		await command("web-chrome-eval", [
			"target", "eval", 'document.querySelector("h1").textContent + " / " + document.querySelector("#result").textContent',
		], "exact", (text) => assert.equal(text.trim(), "dbgjs local website / Waiting for input"));
		await command("web-chrome-disconnect", [
			"connection", "disconnect", "--connection", "chrome",
		], "live", async (text) => {
			assert.match(text, /disconnected/i);
			await assertDisconnected(json, "chrome");
		});

		const nodeEnvironment = { ...process.env };
		delete nodeEnvironment.NODE_OPTIONS;
		// An ephemeral inspector avoids competing with unrelated applications on port 9229.
		node = spawn(process.execPath, [
			"--inspect=127.0.0.1:0", fileURLToPath(new URL("./node-target.mjs", import.meta.url)),
		], {
			env: nodeEnvironment, stdio: ["ignore", "pipe", "pipe", "ipc"],
		});
		nodeClosed = once(node, "close");
		node.stdout.on("data", (chunk) => { nodeOutput += chunk; });
		node.stderr.on("data", (chunk) => { nodeOutput += chunk; });
		const [ready] = await once(node, "message", { signal: AbortSignal.timeout(15_000) });
		assert.deepEqual(ready, { type: "ready", pid: node.pid });
		const before = await probeNode(node);
		replacements.push([String(node.pid), "NODE_PID"]);

		await command("node-context", [
			"context", "create", ":readme-node", "Existing Node application", "--set",
		], "exact", (text) => assert.match(text, /readme-node/));
		let nodeTargetId;
		await command("node-attach", [
			"connection", "add", "--process", String(node.pid), "--connection", "app", "--connect",
		], "live", async (text) => {
			assert.match(text, /connected/);
			const context = await json(["context", "show"]);
			const connection = context.connections.find((candidate) => candidate.id === "app");
			assert.deepEqual(connection.configuration, { kind: "process", processId: node.pid });
			assert.equal(connection.status.kind, "connected");
			assert.equal(connection.targets.length, 1);
			assert.equal(connection.targets[0].targetType, "node");
			assert.equal(connection.targets[0].url, `process:${node.pid}`);
			assert.ok(connection.targets[0].attached);
			nodeTargetId = connection.targets[0].targetId;
		}, { maxOutputLines: 14 });
		await command("node-target-attach", [
			"target", "attach", "--connection", "app",
		], "live", async (text) => {
			assert.match(text, /app/);
			const target = await json(["target", "show", "--connection", "app"]);
			assert.equal(target.target.targetId, nodeTargetId);
		}, { maxOutputLines: 14 });
		const nodeScope = ["--connection", "app"];
		const pid = await json(["target", "eval", "process.pid", ...nodeScope]);
		assert.equal(pid.preview.kind, "number");
		assert.equal(pid.preview.preview, String(node.pid), "Attach must inspect the already-running fixture process.");
		await command("node-eval", [
			"target", "eval", "readmeApp.orders.reduce((total, order) => total + order.total, 0)", ...nodeScope,
		], "exact", (text) => assert.equal(text.trim(), "42"));
		const beforeDisconnect = await probeNode(node);
		await command("node-disconnect", ["connection", "disconnect", "--connection", "app"], "live", async (text) => {
			assert.match(text, /disconnected/i);
			await assertDisconnected(json, "app");
		});
		assert.equal(node.exitCode, null, `Disconnect killed the application:\n${nodeOutput}`);
		await delay(100);
		const after = await probeNode(node);
		assert.equal(after.pid, before.pid);
		assert.ok(after.ticks > beforeDisconnect.ticks, "The application must keep doing work after disconnect.");
		await writeFile(join(output, "connections-liveness.json"), JSON.stringify({
			beforeAttach: before, beforeDisconnect, afterDisconnect: after,
		}, null, 2) + "\n");
		assert.equal(new Set(steps.map((step) => step.id)).size, steps.length);
		recording = { steps };
	} catch (error) {
		failure = error;
	} finally {
		const errors = failure ? [failure] : [];
		async function cleanup(operation) {
			try { await operation(); } catch (error) { errors.push(error); }
		}
		await cleanup(async () => {
			if (serviceUsed) {
				const stopped = await run(cli, ["service", "stop"], environment, { timeoutMs: 20_000 });
				assert.equal(stopped.code, 0, stopped.output);
			}
		});
		await cleanup(async () => {
			if (node?.pid) {
				if (node.exitCode === null && node.signalCode === null) {
					assert.ok(node.kill(), "Could not stop the owned Node fixture.");
				}
				await nodeClosed;
			}
			await writeFile(join(output, "connections-node.log"), nodeOutput);
		});
		await cleanup(async () => {
			if (server?.listening) {
				await new Promise((resolveClose, reject) =>
					server.close((error) => error ? reject(error) : resolveClose()));
			}
		});
		await cleanup(() => rm(directory, { recursive: true, force: true, maxRetries: 10, retryDelay: 200 }));
		if (errors.length) throw new AggregateError(errors, "Connection recording or cleanup failed");
	}
	return recording;
}

async function assertBrowser(json, id, kind, url, replacements, name) {
	const context = await json(["context", "show"]);
	const connection = context.connections.find((candidate) => candidate.id === id);
	assert.equal(connection.configuration.kind, kind);
	assert.equal(connection.status.kind, "connected");
	const page = connection.targets.find((target) => target.targetType === "page" && target.url === url);
	assert.ok(page?.attached, "The fixture page must be attached.");
	replacements.push([page.targetId, name]);
	const selected = await json(["target", "show"]);
	assert.equal(selected.target.targetId, page.targetId, "--set must select the fixture page.");
}

async function assertDisconnected(json, id) {
	const context = await json(["context", "show"]);
	assert.equal(context.connections.find((connection) => connection.id === id)?.status.kind, "disconnected");
}

async function probeNode(node) {
	const message = once(node, "message", { signal: AbortSignal.timeout(10_000) });
	node.send("probe");
	const [result] = await message;
	assert.equal(result.type, "probe");
	assert.equal(result.pid, node.pid);
	return result;
}

if (process.argv[1] && import.meta.url === pathToFileURL(resolve(process.argv[1])).href) {
	const { values } = parseArgs({ options: {
		"bin-dir": { type: "string", default: join("target", "debug") },
		output: { type: "string", default: join("artifacts", "readme-connections") },
	} });
	const suffix = process.platform === "win32" ? ".exe" : "";
	const { steps } = await recordConnections({
		cli: resolve(values["bin-dir"], `dbgjs${suffix}`),
		service: resolve(values["bin-dir"], `dbgjs-service${suffix}`),
		output: resolve(values.output),
	});
	console.log(`Recorded and validated ${steps.length} connection commands.`);
}
