import assert from "node:assert/strict";
import { spawn } from "node:child_process";
import { once } from "node:events";
import { mkdir, mkdtemp, readFile, rm, writeFile } from "node:fs/promises";
import { createServer } from "node:http";
import { join, resolve } from "node:path";
import { setTimeout as delay } from "node:timers/promises";
import { fileURLToPath, pathToFileURL } from "node:url";
import { parseArgs } from "node:util";
import { findChromeExecutable, run } from "../playwright/live-test-harness.mjs";
import { createRecorder } from "./recorder.mjs";

// Standalone smoke: node tests\readme\connections.mjs --bin-dir target\debug --output artifacts\readme-connections
export async function recordConnections({ cli, service, output }) {
	await mkdir(output, { recursive: true });
	const directory = await mkdtemp(join(output, "connections-state-"));
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
	let pendingRequest;
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
		await command("web-chrome-context", [
			"context", "create", ":readme-chrome", "Installed Chrome", "--set",
		], "exact", (text) => assert.match(text, /readme-chrome/));
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
		const fixture = new URL("./express-server.mjs", import.meta.url);
		const fixturePath = fileURLToPath(fixture);
		replacements.push(
			[fixture.href, "EXPRESS_FIXTURE"],
			[fixturePath, "EXPRESS_FIXTURE"],
			[fixture.href.replace("file:///", "file:/"), "EXPRESS_FIXTURE"],
		);
		// An ephemeral inspector avoids competing with unrelated applications on port 9229.
		node = spawn(process.execPath, [
			"--inspect=127.0.0.1:0", fixturePath,
		], {
			env: nodeEnvironment, stdio: ["ignore", "pipe", "pipe", "ipc"],
		});
		nodeClosed = once(node, "close");
		node.stdout.on("data", (chunk) => { nodeOutput += chunk; });
		node.stderr.on("data", (chunk) => { nodeOutput += chunk; });
		const [ready] = await once(node, "message", { signal: AbortSignal.timeout(15_000) });
		assert.equal(ready.type, "ready");
		assert.equal(ready.pid, node.pid);
		assert.ok(Number.isInteger(ready.port) && ready.port > 0);
		const appUrl = `http://127.0.0.1:${ready.port}`;
		replacements.push([appUrl, "EXPRESS_URL"]);
		assert.deepEqual(await (await fetch(`${appUrl}/health`)).json(), { status: "ready" });
		const before = await probeNode(node);
		replacements.push([String(node.pid), "NODE_PID"]);

		await command("node-context", [
			"context", "create", ":readme-node", "Existing Node application", "--set",
		], "exact", (text) => assert.match(text, /readme-node/));
		let nodeConnection;
		await command("node-attach", [
			"process", "attach", String(node.pid), "--set",
		], "live", async (text) => {
			assert.match(text, /Target /);
			const context = await json(["context", "show"]);
			assert.equal(context.connections.length, 1, "PID attach must not connect to the enclosing IDE.");
			const connection = context.connections[0];
			assert.deepEqual(connection.configuration, { kind: "process", processId: node.pid });
			assert.equal(connection.status.kind, "connected");
			assert.equal(connection.targets.length, 1);
			assert.equal(connection.targets[0].targetType, "node");
			assert.equal(connection.targets[0].url, `process:${node.pid}`);
			assert.ok(connection.targets[0].attached);
			nodeConnection = connection.id;
			const target = await json(["target", "show"]);
			assert.equal(target.target.targetId, connection.targets[0].targetId);
		}, { maxOutputLines: 14 });
		const pid = await json(["target", "eval", "process.pid"]);
		assert.equal(pid.preview.kind, "number");
		assert.equal(pid.preview.preview, String(node.pid), "Attach must inspect the already-running fixture process.");
		const curlOptions = {
			executable: process.platform === "win32" ? "curl.exe" : "curl",
			displayExecutable: process.platform === "win32" ? "curl.exe" : "curl",
		};
		const curlArgs = (quantity) => [
			"--silent", "--show-error", "--fail", "--max-time", "60",
			`${appUrl}/quote/notebook?quantity=${quantity}`,
		];
		await command("node-coverage-start", ["coverage", "start"], "exact",
			(text) => assert.match(text, /coverage/i));
		await command("node-coverage-request", curlArgs(3), "exact",
			(text) => assert.deepEqual(JSON.parse(text), {
				product: "Notebook", quantity: 3, subtotal: 36, discount: 3.6, total: 32.4,
			}), curlOptions);
		await command("node-coverage-stop", ["coverage", "stop", "--id", "quote"], "live",
			(text) => assert.match(text, /quote/));
		await command("node-coverage-show", [
			"coverage", "show", "quote", "--path-prefix", fixture.href, "--max-lines", "12",
		], "live", (text) => {
			assert.match(text, /express-server\.mjs/);
			assert.match(text, /[1-9]\d* HL \(hit lines\), [1-9]\d* RL \(run lines\)/);
			assert.ok(text.trimEnd().split(/\r?\n/).length <= 12);
		});
		const source = await readFile(fixture, "utf8");
		const handlerLine = source.split(/\r?\n/).findIndex((line) => line.includes("const subtotal =")) + 1;
		assert.ok(handlerLine > 0);
		await command("node-breakpoint-set", [
			"breakpoint", "set", "quote-handler", fixture.href, String(handlerLine),
		], "live", (text) => assert.match(text, /\[bound; 1 application\(s\)\]/), { maxOutputLines: 12 });
		const beforePause = await json(["target", "show"]);
		pendingRequest = command("node-breakpoint-request", curlArgs(2), "exact",
			(text) => assert.deepEqual(JSON.parse(text), {
				product: "Notebook", quantity: 2, subtotal: 24, discount: 0, total: 24,
			}), curlOptions);
		pendingRequest.catch(() => {});
		await command("node-breakpoint-wait", [
			"target", "wait", "paused", String(beforePause.pause?.epoch ?? 0), "30000",
		], "live", (text) => {
			assert.match(text, /quote/);
			assert.match(text, /express-server\.mjs/);
		}, { maxOutputLines: 15 });
		await command("node-breakpoint-eval", [
			"target", "eval", 'product.name + " x " + quantity',
		], "exact", (text) => assert.equal(text.trim(), "Notebook x 2"));
		await command("node-breakpoint-resume", ["target", "resume"], "live",
			(text) => assert.match(text, /running/));
		await pendingRequest;
		pendingRequest = undefined;
		await command("node-breakpoint-delete", ["breakpoint", "delete", "quote-handler"], "live",
			(text) => assert.match(text, /readme-node/), { maxOutputLines: 8 });
		const beforeDisconnect = await probeNode(node);
		assert.equal(beforeDisconnect.requests, 2);
		await command("node-disconnect", ["connection", "disconnect", "--connection", nodeConnection], "live", async (text) => {
			assert.match(text, /disconnected/i);
			await assertDisconnected(json, nodeConnection);
		});
		assert.equal(node.exitCode, null, `Disconnect killed the application:\n${nodeOutput}`);
		await delay(100);
		const after = await probeNode(node);
		assert.equal(after.pid, before.pid);
		assert.ok(after.ticks > beforeDisconnect.ticks, "The application must keep doing work after disconnect.");
		assert.deepEqual(await (await fetch(`${appUrl}/quote/pencil?quantity=1`)).json(), {
			product: "Pencil", quantity: 1, subtotal: 2, discount: 0, total: 2,
		}, "The server must still handle requests after debugger disconnect.");
		await writeFile(join(output, "connections-liveness.json"), JSON.stringify({
			beforeAttach: before, beforeDisconnect, afterDisconnect: after,
		}, null, 2) + "\n");
		assert.equal(new Set(steps.map((step) => step.id)).size, steps.length);
		recording = { steps, replacements };
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
		await cleanup(async () => { await pendingRequest; });
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
	assert.equal(context.connections.length, 1, "Each browser example must have its own context.");
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
