import { createServer } from "node:http";
import { mkdtemp, rm } from "node:fs/promises";
import { tmpdir } from "node:os";
import { join, resolve } from "node:path";
import { transform } from "esbuild";
import { chromium, expect, test } from "@playwright/test";
import { allocatePort, readCdpEndpoint, run } from "./live-test-harness.mjs";

test("stored heap mappings retain two same-process frames after restart", async () => {
	test.setTimeout(300_000);
	const built = await run("cargo", ["build", "--bins"], {});
	expect(built.code, built.output).toBe(0);
	const resources = new Map([
		["/", '<title>Heap provenance</title><iframe src="/a"></iframe><iframe src="/b"></iframe>'],
	]);
	for (const frame of ["a", "b"]) {
		const name = `Original${frame.toUpperCase()}`;
		const compiled = await transform(
			`class ${name} { constructor() { this.label = "${frame}"; } }
globalThis.heapFixture = [new ${name}(), new ${name}()];`,
			{ minify: true, format: "iife", sourcemap: "external", sourcefile: `frame-${frame}.ts` },
		);
		resources.set(`/${frame}`, `<script src="/${frame}.js"></script>`);
		resources.set(`/${frame}.js`, `${compiled.code}\n//# sourceMappingURL=/${frame}.js.map`);
		resources.set(`/${frame}.js.map`, compiled.map);
	}
	const mapRequests = [];
	const server = createServer((request, response) => {
		const content = resources.get(request.url);
		if (content === undefined) {
			response.writeHead(404);
			response.end();
			return;
		}
		if (request.url.endsWith(".map")) {
			mapRequests.push(request.url);
		}
		response.setHeader("Content-Type", request.url.endsWith(".js")
			? "application/javascript"
			: request.url.endsWith(".map") ? "application/json" : "text/html");
		response.end(content);
	});
	await new Promise((resolve, reject) => {
		server.once("error", reject);
		server.listen(0, "127.0.0.1", resolve);
	});
	const directory = await mkdtemp(join(tmpdir(), "jsdbg-heap-frames-"));
	const environment = {
		JSDBG_SERVICE_EXE: resolve(`target/debug/jsdbg-service${process.platform === "win32" ? ".exe" : ""}`),
		JSDBG_SERVICE_STATE: join(directory, "service.json"),
	};
	const cli = resolve(`target/debug/jsdbg${process.platform === "win32" ? ".exe" : ""}`);
	const contextId = `:heap-frames-${process.pid}`;
	const json = async (...arguments_) => {
		const result = await run(cli, ["--json", ...arguments_], environment, { timeoutMs: 60_000 });
		expect(result.code, result.output).toBe(0);
		return JSON.parse(result.output);
	};
	let browser;
	let serviceStarted = false;
	try {
		const port = await allocatePort();
		const channel = process.env.PLAYWRIGHT_CHANNEL ?? "chrome";
		browser = await chromium.launchPersistentContext(join(directory, "chrome"), {
			...(channel === "bundled" ? {} : { channel }),
			headless: true,
			args: [`--remote-debugging-port=${port}`, "--remote-allow-origins=*"],
		});
		const address = server.address();
		expect(typeof address).toBe("object");
		const origin = `http://127.0.0.1:${address.port}`;
		const page = browser.pages()[0];
		await page.goto(origin);
		await expect.poll(() => page.frames().filter((frame) => frame.parentFrame()).length).toBe(2);
		for (const frame of page.frames().filter((frame) => frame.parentFrame())) {
			await expect.poll(() => frame.evaluate(() => globalThis.heapFixture?.length)).toBe(2);
		}
		const cdp = await browser.newCDPSession(page);
		const { frameTree } = await cdp.send("Page.getFrameTree");
		const frames = new Map(frameTree.childFrames.map(({ frame }) => [frame.url, frame.id]));
		const endpoint = await readCdpEndpoint(port);
		serviceStarted = true;
		await json("context", "create", "--context", contextId);
		const connected = await json(
			"connection", "add", endpoint, "--connection", "browser", "--context", contextId, "--connect",
		);
		const target = connected.targetForest.find(({ target }) => target.url === `${origin}/`).target;
		await json("target", "show", "--target", target.targetId, "--context", contextId);
		const captured = await run(
			cli,
			["--json", "heap", "capture", "--id", "frames", "--target", target.targetId, "--context", contextId],
			environment,
			{ timeoutMs: 60_000 },
		);
		expect(captured.code, captured.output).toBe(0);
		await json("connection", "disconnect", "--connection", "browser", "--context", contextId);
		await json("service", "stop");
		serviceStarted = false;
		await browser.close();
		browser = undefined;
		const requestsAtCapture = mapRequests.length;
		expect(mapRequests).toEqual(expect.arrayContaining(["/a.js.map", "/b.js.map"]));

		serviceStarted = true;
		const stored = await json("heap", "classes", "frames", "--context", contextId);
		const identities = [];
		for (const frame of ["a", "b"]) {
			const group = stored.classes.find((group) => group.sourceUrl === `${origin}/frame-${frame}.ts`);
			expect(group, JSON.stringify(stored.analysis)).toBeDefined();
			expect(group.name).toContain(`Original${frame.toUpperCase()}`);
			expect(group.instanceCount).toBe(2);
			expect(group.provenance.frameId).toBe(frames.get(`${origin}/${frame}`));
			expect(group.provenance.executionContextId).toBeDefined();
			identities.push(group.provenance.executionContextId);
			const mapping = stored.analysis.scriptMappings.find((mapping) => mapping.scriptId === group.scriptId);
			expect(mapping.status).toBe("mapped");
		}
		expect(new Set(identities).size).toBe(2);
		expect(mapRequests.length).toBe(requestsAtCapture);
	} finally {
		try {
			if (serviceStarted) {
				await json("service", "stop");
			}
		} finally {
			try {
				await browser?.close();
			} finally {
				await new Promise((resolve, reject) => server.close((error) => error ? reject(error) : resolve()));
				await rm(directory, { recursive: true, force: true });
			}
		}
	}
});
