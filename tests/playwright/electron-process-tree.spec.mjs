import assert from "node:assert/strict";
import { spawn } from "node:child_process";
import { randomUUID } from "node:crypto";
import { access, mkdir, rm } from "node:fs/promises";
import { createServer } from "node:http";
import { join, resolve } from "node:path";
import electron from "electron";
import { expect, test } from "@playwright/test";
import { run } from "./live-test-harness.mjs";

const suffix = process.platform === "win32" ? ".exe" : "";
const binaries = process.env.DBGJS_TEST_BIN_DIR ?? "target/debug";
const cli = resolve(binaries, `dbgjs${suffix}`);
const service = resolve(binaries, `dbgjs-service${suffix}`);
const fixtureProgram = resolve("tests/playwright/fixtures/electron-webview.cjs");
let lastRelayInventory = [];

test("isolated Electron process tree projects an OOPIF webview and its inner document", async () => {
	test.setTimeout(300_000);
	if (!process.env.DBGJS_TEST_BIN_DIR) {
		const build = await run("cargo", ["build", "--locked", "--bin", "dbgjs", "--bin", "dbgjs-service"], {});
		expect(build.code, build.output).toBe(0);
	}
	const root = resolve(".test-tmp", `electron-webview-${randomUUID()}`);
	let fixture;
	let child;
	let serviceChild;
	let env;
	let output = "";
	let serviceOutput = "";
	let inventory = [];
	let connectionSummary = [];
	let frameTree;
	let nativeInventory = [];
	try {
		await mkdir(join(root, "profile"), { recursive: true });
		fixture = await startPages();
		env = {
			DBGJS_SERVICE_EXE: service,
			DBGJS_SERVICE_STATE: join(root, "service.json"),
		};
		serviceChild = spawn(service, ["--state-file", env.DBGJS_SERVICE_STATE], {
			env: { ...process.env, ...env },
			stdio: ["ignore", "pipe", "pipe"],
		});
		serviceChild.stdout.on("data", (chunk) => { serviceOutput += chunk; });
		serviceChild.stderr.on("data", (chunk) => { serviceOutput += chunk; });
		await expect.poll(async () => {
			try { await access(env.DBGJS_SERVICE_STATE); return true; } catch { return false; }
		}, { timeout: 15_000 }).toBe(true);
		const electronEnvironment = { ...process.env };
		delete electronEnvironment.ELECTRON_RUN_AS_NODE;
		child = spawn(electron, [
			"--inspect=0", "--no-sandbox", "--disable-dev-shm-usage",
			fixtureProgram, fixture.rootUrl, join(root, "profile"),
		], {
			cwd: process.cwd(),
			env: electronEnvironment,
			stdio: ["pipe", "pipe", "pipe"],
		});
		child.stdout.on("data", (chunk) => { output += chunk; });
		child.stderr.on("data", (chunk) => { output += chunk; });
		child.stdin.on("error", () => {});
		await Promise.race([
			expect.poll(() => output.includes('"kind":"loaded"'), {
				timeout: 20_000,
				message: "Electron fixture did not load its host",
			}).toBe(true),
			new Promise((_, reject) => {
				child.once("error", (error) => reject(new Error(`Electron launch failed: ${error.message}`)));
				child.once("exit", (code, signal) => reject(new Error(
					`Electron exited before fixture load (code ${code}, signal ${signal}); output: ${output.slice(-2200)}`,
				)));
			}),
		]);
		const loaded = output.split("\n").map((line) => {
			try { return JSON.parse(line); } catch { return undefined; }
		}).find((entry) => entry?.kind === "loaded");
		const nativeTargetId = loaded.targetInfos.find((entry) =>
			entry.url.startsWith(fixture.webviewUrl))?.targetId;
		assert.ok(nativeTargetId, "Electron fixture must expose a native OOPIF target");
		assert.equal(loaded.targetInfos.find((entry) => entry.targetId === nativeTargetId)?.parentId,
			loaded.rootTargetId, "the OOPIF must be a child of the selected renderer");
		await command(["context", "create", "--context", ":isolated-electron-webview"], env);
		await command([
			"connection", "add", "--process-tree", String(child.pid),
			"--context", ":isolated-electron-webview", "--connection", "tree", "--connect",
		], env);
		const afterConnection = JSON.parse(await command([
			"--json", "context", "show", "--context", ":isolated-electron-webview",
		], env));
		connectionSummary = afterConnection.connections.map((entry) => ({
			id: entry.id, state: entry.status, targetCount: entry.targets?.length,
		}));
		assert.ok(connectionSummary.some((entry) => entry.id === "tree" && entry.state?.kind === "connected"),
			`process-tree connection did not remain connected: ${JSON.stringify(connectionSummary)}`);
		inventory = afterConnection.connections.find((entry) => entry.id === "tree")?.targets ?? [];
		const renderer = inventory.find((entry) =>
			entry.url === fixture.rootUrl && entry.targetType === "page");
		assert.ok(renderer, "fixture Electron renderer must be discovered through the process tree");
		const scope = [
			"--context", ":isolated-electron-webview",
			"--connection", "tree", "--target", renderer.targetId,
		];
		await command(["--json", "target", "attach", ...scope], env);
		await refreshTargets(env);
		assert.ok(!inventory.some((entry) => entry.targetType === "browser"),
			"fixture must exercise the Electron renderer bridge, not a browser CDP fallback");
		let target;
		let sibling;
		await expect.poll(async () => {
			inventory = await readTargets(env);
			connectionSummary = [{ id: "tree", targetCount: inventory.length }];
			target = inventory.find((entry) => entry.url?.startsWith(fixture.webviewUrl));
			sibling = inventory.find((entry) => entry.url?.startsWith(fixture.siblingUrl));
			return target?.targetId && sibling?.targetId;
		}, { timeout: 20_000 }).toBeTruthy();
		assert.equal(target.targetType, "iframe", "cross-origin webview must be a real OOPIF");
		assert.ok(target.targetId.endsWith(`/target/${nativeTargetId}`),
			"process-tree child identity must correspond to the native OOPIF");
		assert.notEqual(target.targetId, sibling.targetId);
		const tree = JSON.parse(await command(["--json", "target", "cdp", "Page.getFrameTree", ...scope], env));
		frameTree = tree.frameTree;
		const result = JSON.parse(await command([
			"--json", "playwright", `
				const session = await page.context().newCDPSession(page);
				const { targetInfo } = await session.send("Target.getTargetInfo");
				await session.detach();
				let initialFrames;
				for (let attempt = 0; attempt < 40; attempt++) {
					initialFrames = page.frames().map((frame) => ({
						url: frame.url(), parent: frame.parentFrame()?.url(),
					}));
					if (initialFrames.some((frame) => frame.url.endsWith("/inner") &&
						frame.parent?.includes("/webview"))) break;
					await new Promise((resolveDelay) => setTimeout(resolveDelay, 100));
				}
				if (!initialFrames.some((frame) => frame.url.endsWith("/inner") &&
					frame.parent?.includes("/webview"))) {
					throw new Error("OOPIF inner frame missing before input: " +
						JSON.stringify(initialFrames));
				}
				const frames = page.frames();
				const before = await Promise.all(frames.map(async (frame) => ({
					url: frame.url(), marker: await Promise.race([
						frame.evaluate(() =>
							document.querySelector("[data-marker]")?.getAttribute("data-marker")),
						new Promise((_, rejectEval) => setTimeout(() =>
							rejectEval(new Error("frame evaluation stalled: " + JSON.stringify({
								frame: frame.url(), all: frames.map((candidate) => candidate.url()),
							}))), 3_000)),
					]),
				})));
				const inner = frames.find((frame) => frame.url().endsWith("/inner") &&
					frame.parentFrame()?.url().includes("/webview"));
				if (!inner) throw new Error("same-origin inner document is missing: " +
					JSON.stringify(frames.map((frame) => ({
						url: frame.url(), parent: frame.parentFrame()?.url(),
					}))));
				const innerDocument = await inner.evaluate(() => ({
					input: Boolean(document.querySelector("#editor")),
					marker: document.querySelector("[data-marker]")?.getAttribute("data-marker"),
					body: document.body?.innerHTML,
				}));
				if (!innerDocument.input) throw new Error("inner input missing: " + JSON.stringify(innerDocument));
				await page.frameLocator("#primary").frameLocator("iframe")
					.locator("#editor").fill("edited in the isolated webview", { timeout: 5_000 });
				return {
					targetId: targetInfo.targetId,
					frames: before,
					edited: await inner.locator("#editor").inputValue(),
				};
			`, ...scope,
		], env, 35_000));
		expect(result.targetId).toBe(tree.frameTree.frame.id);
		expect(result.frames.map((frame) => frame.marker).sort()).toEqual(
			["host", "inner", "inner", "sibling", "webview"],
		);
		expect(result.edited).toBe("edited in the isolated webview");

		const attach = JSON.parse(await command([
			"--json", "target", "cdp", "Target.attachToTarget",
			"--params", JSON.stringify({ targetId: nativeTargetId, flatten: true }),
			...scope,
		], env));
		assert.ok(attach.sessionId, "native child attachment must return a raw session ID");
		const raw = ["--session-id", attach.sessionId, ...scope];
		for (const expression of ["document.title", "document.querySelector('[data-marker]').dataset.marker"]) {
			const evaluated = JSON.parse(await command([
				"--json", "target", "cdp", "Runtime.evaluate",
				"--params", JSON.stringify({ expression, returnByValue: true }), ...raw,
			], env));
			assert.equal(evaluated.result.value, expression === "document.title" ? "Webview" : "webview");
		}
		await command([
			"--json", "target", "cdp", "Target.detachFromTarget",
			"--params", JSON.stringify({ sessionId: attach.sessionId }),
			...scope,
		], env);
		const detached = await run(cli, [
			"--json", "target", "cdp", "Runtime.evaluate",
			"--params", '{"expression":"1+1","returnByValue":true}', ...raw,
		], env, { timeoutMs: 5_000 });
		assert.equal(detached.timedOut, false, detached.output);
		assert.notEqual(detached.code, 0, "detached raw session must fail promptly");
		assert.match(detached.output, /session|detach|stale|unknown/i);

		const childScope = [
			"--context", ":isolated-electron-webview",
			"--connection", "tree", "--target", target.targetId,
		];
		const discovered = await run(cli, ["target", "eval", "document.title", ...childScope], env, {
			timeoutMs: 5_000,
		});
		assert.equal(discovered.timedOut, false, discovered.output);
		assert.notEqual(discovered.code, 0,
			"native discovery must not masquerade as an explicitly managed debugger");
		assert.match(discovered.output, /discovered.*not attached|not attached.*debugging/i);
		assert.match(discovered.output, /target attach/i);
		assert.doesNotMatch(discovered.output, /--force/);
		const attachment = JSON.parse(await command([
			"--json", "target", "attach", ...childScope,
		], env));
		assert.equal(attachment.target.targetId, target.targetId);
		assert.match(await command(["target", "eval", "document.title", ...childScope], env),
			/Webview/);
		child.stdin.write('{"kind":"remove"}\n');
		await expect.poll(() => output.includes('"kind":"remove"'), { timeout: 10_000 }).toBe(true);
		nativeInventory = JSON.parse(await command([
			"--json", "target", "cdp", "Target.getTargets", ...scope,
		], env)).targetInfos?.filter(({ type }) => type === "iframe");
		await expect.poll(async () => {
			const targets = await readTargets(env);
			return !targets.some((entry) => entry.targetId === target.targetId) &&
				targets.some((entry) => entry.targetId === sibling.targetId);
		}, { timeout: 15_000 }).toBe(true);
		const stale = await run(cli, ["target", "eval", "document.title", ...childScope], env,
			{ timeoutMs: 5_000 });
		assert.equal(stale.timedOut, false, stale.output);
		assert.notEqual(stale.code, 0, "dead managed child must not look attached");
		assert.match(stale.output, /target|attach|stale|disconnect|not found/i);
		const replacementUrl = `${fixture.webviewUrl}?generation=2`;
		child.stdin.write(`${JSON.stringify({ kind: "replace", url: replacementUrl })}\n`);
		await expect.poll(() => output.includes('"kind":"replace"'), { timeout: 10_000 }).toBe(true);
		let replacement;
		await expect.poll(async () => {
			replacement = (await readTargets(env))
				.find((entry) => entry.url === replacementUrl);
			return replacement?.targetId;
		}, { timeout: 15_000 }).toBeTruthy();
		assert.notEqual(replacement.targetId, target.targetId, "replacement must not reuse stale identity");
		await command([
			"--json", "target", "attach", "--context", ":isolated-electron-webview",
			"--connection", "tree", "--target", replacement.targetId,
		], env);
		const recovered = JSON.parse(await command([
			"--json", "playwright", `return {
				host: await page.title(),
				sibling: await page.frameLocator("#sibling").locator("[data-marker]").first().getAttribute("data-marker"),
				replacement: await page.frameLocator("#primary").locator("[data-marker]").first().getAttribute("data-marker"),
			};`, ...scope,
		], env, 35_000));
		assert.deepEqual(recovered, {
			host: "Fixture host",
			sibling: "sibling",
			replacement: "webview",
		});
		const stalled = await run(cli, [
			"--json", "playwright", "await new Promise(() => {})", ...scope,
		], env, { timeoutMs: 35_000 });
		assert.equal(stalled.timedOut, false, "execution must end at its own deadline");
		assert.notEqual(stalled.code, 0, "unresolved program must fail");
		assert.match(stalled.output, /executing.*deadline/i);
		assert.doesNotMatch(stalled.output, /wss?:\/\/127\.0\.0\.1/);
		await command(["--json", "target", "show", ...scope], env);
	} catch (error) {
		throw new Error(redactEndpoints(`${error.message}\nTargets: ${JSON.stringify(inventory.map(
			(entry) => ({ targetId: entry.targetId, targetType: entry.targetType, url: entry.url }),
		))}\nConnections: ${JSON.stringify(connectionSummary)}\nRelay inventory: ${JSON.stringify(lastRelayInventory)}\nNative inventory: ${JSON.stringify(nativeInventory)}\nFrame tree: ${JSON.stringify(frameTree)}\nHTTP requests: ${JSON.stringify(fixture?.requests ?? [])}\nService: ${serviceOutput.slice(-12000)}\nElectron exited ${child?.exitCode}; output: ${output.slice(-2200)}`));
	} finally {
		try {
			if (env) await run(cli, ["service", "stop"], env, { timeoutMs: 8_000 });
		} finally {
			try {
				if (serviceChild?.exitCode === null && serviceChild?.signalCode === null) {
					serviceChild.kill("SIGTERM");
				}
				if (child) {
					child.stdin.end();
					for (const signal of [undefined, "SIGTERM", "SIGKILL"]) {
						if (child.exitCode !== null || child.signalCode !== null) break;
						if (signal) child.kill(signal);
						await Promise.race([
							new Promise((resolveExit) => child.once("exit", resolveExit)),
							new Promise((resolveTimeout) => setTimeout(resolveTimeout, 3_000)),
						]);
					}
				}
			} finally {
				try {
					if (fixture) await fixture.close();
				} finally {
					await rm(root, { recursive: true, force: true, maxRetries: 10, retryDelay: 200 });
				}
			}
		}
	}
});

function redactEndpoints(text) {
	return text.replace(/wss?:\/\/[^\s"\\]+/g, "<debugger-endpoint>");
}

async function readTargets(env) {
	await refreshTargets(env);
	const result = await command([
		"--json", "target", "list", "--context", ":isolated-electron-webview",
	], env);
	return JSON.parse(result).targets;
}

async function refreshTargets(env) {
	const relay = spawn(cli, [
		"context", "relay", "--stdio", "--context", ":isolated-electron-webview",
	], { cwd: process.cwd(), env: { ...process.env, ...env }, stdio: ["pipe", "pipe", "pipe"] });
	let buffered = "";
	let stderr = "";
	const pending = new Map();
	relay.stdout.on("data", (chunk) => {
		buffered += chunk;
		for (;;) {
			const end = buffered.indexOf("\n");
			if (end < 0) break;
			const line = buffered.slice(0, end);
			buffered = buffered.slice(end + 1);
			let message;
			try { message = JSON.parse(line); } catch { continue; }
			pending.get(message.id)?.(message);
			pending.delete(message.id);
		}
	});
	relay.stderr.on("data", (chunk) => { stderr += chunk; });
	relay.stdin.on("error", () => {});
	let nextId = 0;
	const request = async (method, params) => {
		const id = ++nextId;
		const response = new Promise((resolveReply, rejectReply) => {
			pending.set(id, resolveReply);
			relay.once("error", rejectReply);
			relay.once("exit", (code) => rejectReply(new Error(
				`context relay exited ${code} during ${method}: ${redactEndpoints(stderr)}`,
			)));
		});
		relay.stdin.write(`${JSON.stringify({ id, method, params })}\n`);
		const result = await Promise.race([
			response,
			new Promise((_, reject) => setTimeout(
				() => reject(new Error(`context relay ${method} exceeded 10 seconds`)), 10_000)),
		]);
		assert.ifError(result.error);
		return result.result;
	};
	try {
		await request("Target.setDiscoverTargets", { discover: true });
		await new Promise((resolveDelay) => setTimeout(resolveDelay, 650));
		const result = await request("Target.getTargets", {});
		assert.ok(result?.targetInfos, "context relay did not return target inventory");
		lastRelayInventory = result.targetInfos.map(({ targetId, url }) => ({ targetId, url }));
	} finally {
		relay.stdin.end();
		await Promise.race([
			new Promise((resolveExit) => relay.once("exit", resolveExit)),
			new Promise((resolveTimeout) => setTimeout(resolveTimeout, 2_000)),
		]);
		if (relay.exitCode === null && relay.signalCode === null) relay.kill();
	}
}

async function command(args, env, timeoutMs = 15_000) {
	const result = await run(cli, args, env, { timeoutMs });
	assert.equal(result.timedOut, false, `dbgjs ${args.join(" ")}: ${result.output}`);
	assert.equal(result.code, 0, `dbgjs ${args.join(" ")}: ${result.output}`);
	return result.output.trim();
}

async function startPages() {
	const requests = [];
	const webview = createServer((request, response) => {
		requests.push(request.url);
		response.writeHead(200, { "content-type": "text/html; charset=utf-8" });
		response.end(request.url === "/inner"
			? '<title>Inner</title><span data-marker="inner"></span><input id="editor">'
			: request.url === "/sibling"
				? '<title>Sibling</title><span data-marker="sibling"></span><iframe src="/inner"></iframe>'
				: '<title>Webview</title><span data-marker="webview"></span><iframe src="/inner"></iframe>');
	});
	await listen(webview, "127.0.0.1");
	try {
		const webviewUrl = `http://localhost:${webview.address().port}/webview`;
		const siblingUrl = `http://localhost:${webview.address().port}/sibling`;
		const root = createServer((_request, response) => {
			response.writeHead(200, { "content-type": "text/html; charset=utf-8" });
			response.end(`<title>Fixture host</title><span data-marker="host"></span>
				<iframe id="primary" src="${webviewUrl}"></iframe>
				<iframe id="sibling" src="${siblingUrl}"></iframe>`);
		});
		await listen(root, "127.0.0.1");
		return {
			rootUrl: `http://127.0.0.1:${root.address().port}/`,
			webviewUrl,
			siblingUrl,
			requests,
			close: () => Promise.all([close(root), close(webview)]),
		};
	} catch (error) {
		await close(webview);
		throw error;
	}
}

function listen(server, host) {
	return new Promise((resolveListen, reject) => {
		server.once("error", reject);
		server.listen(0, host, resolveListen);
	});
}

function close(server) {
	return new Promise((resolveClose, reject) => server.close((error) =>
		error ? reject(error) : resolveClose()));
}
