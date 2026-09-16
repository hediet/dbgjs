import { randomUUID } from "node:crypto";
import { createServer } from "node:http";
import { mkdir, rm, writeFile, appendFile } from "node:fs/promises";
import { resolve, join } from "node:path";
import { chromium, expect, test } from "@playwright/test";
import {
	allocatePort,
	findPageTargetId,
	readCdpEndpoint,
	run,
} from "./live-test-harness.mjs";

const executableSuffix = process.platform === "win32" ? ".exe" : "";
const binDirectory = process.env.DBGJS_TEST_BIN_DIR ?? "target/debug";
const cli = resolve(binDirectory, `dbgjs${executableSuffix}`);
const service = resolve(binDirectory, `dbgjs-service${executableSuffix}`);
const transcriptPath = resolve("artifacts/playwright-page-cli.md");

test("selected page runs bounded Playwright programs through dbgjs", async () => {
	test.setTimeout(300_000);
	if (!process.env.DBGJS_TEST_BIN_DIR) {
		const build = await run("cargo", ["build", "--bins"], {});
		expect(build.code, build.output).toBe(0);
	}

	const scratchRoot = resolve(".test-tmp");
	const stateDirectory = join(scratchRoot, `playwright-page-${randomUUID()}`);
	await mkdir(stateDirectory, { recursive: true });
	await mkdir(resolve("artifacts"), { recursive: true });
	await writeFile(
		transcriptPath,
		"# Playwright against a selected `dbgjs` page\n\nThis is a real Chromium/Playwright E2E run. The daemon control plane uses authenticated local IPC; Playwright receives a one-shot, capability-URL WebSocket bound only to loopback.\n",
	);
	const fixture = await startFixture();
	const debuggingPort = await allocatePort();
	const channel = process.env.PLAYWRIGHT_CHANNEL ?? "bundled";
	const browserContext = await chromium.launchPersistentContext(
		join(stateDirectory, "chromium"),
		{
			...(channel === "bundled" ? {} : { channel }),
			headless: true,
			args: [
				`--remote-debugging-port=${debuggingPort}`,
				"--remote-allow-origins=*",
				"--site-per-process",
				"--no-first-run",
				"--no-default-browser-check",
			],
		},
	);
	const page = browserContext.pages()[0] ?? (await browserContext.newPage());
	const unrelatedPage = await browserContext.newPage();
	const stateFile = join(stateDirectory, "service.json");
	const environment = {
		DBGJS_SERVICE_EXE: service,
		DBGJS_SERVICE_STATE: stateFile,
	};

	try {
		await unrelatedPage.setContent(
			"<title>unrelated owner page</title><p id='unrelated'>untouched</p>",
		);
		await page.goto(fixture.origin);
		await expect(
			page.frameLocator("#oopif").locator("#oopif-marker"),
		).toHaveText("cross-origin iframe");
		const ownerBrowser = browserContext.browser();
		expect(ownerBrowser).not.toBeNull();
		const ownerCdp = await ownerBrowser.newBrowserCDPSession();
		const targetInfos = await ownerCdp.send("Target.getTargets");
		expect(
			targetInfos.targetInfos.some(
				(target) =>
					target.type === "iframe" && target.url.startsWith(fixture.oopifOrigin),
			),
		).toBe(true);
		await ownerCdp.detach();
		const endpoint = await readCdpEndpoint(debuggingPort);
		await runCli(["context", "create", "--context", ":playwright-e2e"], environment);
		await runCli(
			[
				"connection",
				"add",
				endpoint,
				"--context",
				":playwright-e2e",
				"--connection",
				"browser",
				"--connect",
			],
			environment,
		);
		const targetId = await findPageTargetId(
			cli,
			":playwright-e2e",
			"browser",
			fixture.origin,
			environment,
		);
		const scope = [
			"--context",
			":playwright-e2e",
			"--connection",
			"browser",
			"--target",
			targetId,
		];

		const wheel = await runCli(
			[
				"playwright",
				"await page.mouse.wheel(0, 800)",
				...scope,
			],
			environment,
		);
		expect(wheel).toBe("");
		const inline = await runCli(
			[
				"playwright",
				"return { title: await page.title(), scrollY: await page.evaluate(() => scrollY) };",
				...scope,
			],
			environment,
		);
		expect(JSON.parse(inline)).toEqual({ title: "dbgjs Playwright E2E", scrollY: 800 });

		const stdinProgram =
			'return { url: page.url(), marker: await page.locator("#marker").textContent() };';
		const fromStdin = await runCli(
			["playwright", "-", ...scope],
			environment,
			stdinProgram,
		);
		expect(JSON.parse(fromStdin)).toEqual({
			url: `${fixture.origin}/`,
			marker: "selected page",
		});
		expect(await page.title()).toBe("dbgjs Playwright E2E");
		const oopif = await runCli(
			[
				"playwright",
				'return { text: await page.frameLocator("#oopif").locator("#oopif-marker").textContent() };',
				...scope,
			],
			environment,
		);
		expect(JSON.parse(oopif)).toEqual({ text: "cross-origin iframe" });
		const fileChooser = await runCli(
			[
				"playwright",
				`const [chooser] = await Promise.all([
					page.waitForEvent("filechooser"),
					page.locator("#file").click(),
				]);
				return { multiple: chooser.isMultiple(), title: await page.title() };`,
				...scope,
			],
			environment,
		);
		expect(JSON.parse(fileChooser)).toEqual({
			multiple: false,
			title: "dbgjs Playwright E2E",
		});
		const opaqueHeader = await runCli(
			[
				"playwright",
				`await page.setExtraHTTPHeaders({ targetId: "opaque-header-value" });
				await page.goto(${JSON.stringify(`${fixture.origin}/opaque-header`)});
				return {
					header: await page.locator("#target-id-header").textContent(),
					oopif: await page.frameLocator("#oopif").locator("#oopif-marker").textContent(),
				};`,
				...scope,
			],
			environment,
		);
		expect(JSON.parse(opaqueHeader)).toEqual({
			header: "opaque-header-value",
			oopif: "cross-origin iframe",
		});
		const detachedSession = await runCli(
			[
				"playwright",
				`const session = await page.context().newCDPSession(page);
				const evaluation = await session.send("Runtime.evaluate", {
					expression: "document.title",
					returnByValue: true,
				});
				await session.detach();
				return {
					auxiliarySessionTitle: evaluation.result.value,
					pageTitleAfterDetach: await page.title(),
				};`,
				...scope,
			],
			environment,
		);
		expect(JSON.parse(detachedSession)).toEqual({
			auxiliarySessionTitle: "dbgjs Playwright E2E",
			pageTitleAfterDetach: "dbgjs Playwright E2E",
		});
		const rejected = JSON.parse(
			await runCli(
				[
					"playwright",
					`async function rejection(operation) {
						try { await operation(); return null; } catch (error) { return error.message; }
					}
					return {
						newPage: await rejection(() => page.context().newPage()),
						newContext: await rejection(() => page.context().browser().newContext()),
						clearCookies: await rejection(() => page.context().clearCookies()),
					};`,
					...scope,
				],
				environment,
			),
		);
		for (const error of Object.values(rejected)) {
			expect(error).toContain("outside the selected page allowlist");
		}
		expect(await unrelatedPage.title()).toBe("unrelated owner page");
		expect(await unrelatedPage.locator("#unrelated").textContent()).toBe("untouched");

		const destruction = run(cli, [
			"playwright",
			`await page.evaluate(() => {
				document.title = "proxy command pending";
				return new Promise(() => {});
			})`,
			...scope,
		], environment, { timeoutMs: 45_000 });
		await expect.poll(() => page.title(), { timeout: 30_000 }).toBe("proxy command pending");
		const closingAt = performance.now();
		await page.close();
		const destroyed = await destruction;
		await appendTranscriptCommand(
			[
				"playwright",
				"<pending page.evaluate>",
				...scope,
			],
			destroyed.output,
		);
		expect(destroyed.code, destroyed.output).not.toBe(0);
		expect(destroyed.timedOut, destroyed.output).toBe(false);
		expect(performance.now() - closingAt, destroyed.output).toBeLessThan(10_000);
		expect(await unrelatedPage.title()).toBe("unrelated owner page");
		await appendFile(
			transcriptPath,
			"\nA verified cross-origin iframe, file chooser interception, and an opaque `targetId` HTTP header worked through the selected page. Detaching an auxiliary CDP session left the page proxy usable. Browser-wide operations were rejected, the unrelated page remained untouched, and destroying the selected page cancelled its pending proxy command promptly. The original Playwright owner remained connected.\n",
		);
	} finally {
		await run(cli, ["service", "stop"], environment);
		await browserContext.close();
		await fixture.close();
		await rm(stateDirectory, { recursive: true, force: true });
	}
});

async function runCli(args, environment, input) {
	const result = await run(cli, args, environment, { input, timeoutMs: 45_000 });
	await appendTranscriptCommand(args, result.output, input);
	expect(result.code, result.output).toBe(0);
	return result.output.trim();
}

async function appendTranscriptCommand(args, output, input) {
	const command = `dbgjs ${args.map(shellQuote).join(" ")}`;
	const stableOutput = output.replaceAll(process.cwd(), "<worktree>");
	await appendFile(
		transcriptPath,
		`\n\`\`\`console\n$ ${command}${input === undefined ? "" : ` <<'JS'\n${input}\nJS`}\n${stableOutput.trim()}\n\`\`\`\n`,
	);
}

function shellQuote(value) {
	return /^[A-Za-z0-9_./:@-]+$/.test(value) ? value : JSON.stringify(value);
}

async function startFixture() {
	const oopifServer = createServer((_request, response) => {
		response.writeHead(200, { "content-type": "text/html; charset=utf-8" });
		response.end('<strong id="oopif-marker">cross-origin iframe</strong>');
	});
	await listen(oopifServer, "localhost");
	const oopifAddress = oopifServer.address();
	if (typeof oopifAddress !== "object" || oopifAddress === null) {
		throw new Error("OOPIF fixture server did not bind to TCP");
	}
	const oopifOrigin = `http://localhost:${oopifAddress.port}`;

	const server = createServer((request, response) => {
		response.writeHead(200, { "content-type": "text/html; charset=utf-8" });
		response.end(`<!doctype html>
<title>dbgjs Playwright E2E</title>
<style>body { margin: 0 } main { height: 3000px; padding: 16px }</style>
<main>
	<strong id="marker">selected page</strong>
	<input id="file" type="file">
	<span id="target-id-header">${request.headers.targetid ?? ""}</span>
	<iframe id="oopif" src="${oopifOrigin}/child"></iframe>
</main>`);
	});
	await listen(server, "127.0.0.1");
	const address = server.address();
	if (typeof address !== "object" || address === null) {
		throw new Error("fixture server did not bind to TCP");
	}
	return {
		origin: `http://127.0.0.1:${address.port}`,
		oopifOrigin,
		close: () => Promise.all([closeServer(server), closeServer(oopifServer)]),
	};
}

function listen(server, host) {
	return new Promise((resolveReady, reject) => {
		server.once("error", reject);
		server.listen(0, host, resolveReady);
	});
}

function closeServer(server) {
	return new Promise((resolveClose, reject) =>
		server.close((error) => (error ? reject(error) : resolveClose())),
	);
}
