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
const cli = resolve(`target/debug/jsdbg${executableSuffix}`);
const service = resolve(`target/debug/jsdbg-service${executableSuffix}`);
const transcriptPath = resolve("artifacts/playwright-page-cli.md");

test("selected page runs bounded Playwright programs through jsdbg", async () => {
	test.setTimeout(300_000);
	const build = await run("cargo", ["build", "--bins"], {});
	expect(build.code, build.output).toBe(0);

	const scratchRoot = resolve(".test-tmp");
	const stateDirectory = join(scratchRoot, `playwright-page-${randomUUID()}`);
	await mkdir(stateDirectory, { recursive: true });
	await mkdir(resolve("artifacts"), { recursive: true });
	await writeFile(
		transcriptPath,
		"# Playwright against a selected `jsdbg` page\n\nThis is a real Chromium/Playwright E2E run. The daemon control plane uses authenticated local IPC; Playwright receives a one-shot, capability-URL WebSocket bound only to loopback.\n",
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
				"--no-first-run",
				"--no-default-browser-check",
			],
		},
	);
	const page = browserContext.pages()[0] ?? (await browserContext.newPage());
	const unrelatedPage = await browserContext.newPage();
	const stateFile = join(stateDirectory, "service.json");
	const environment = {
		JSDBG_SERVICE_EXE: service,
		JSDBG_SERVICE_STATE: stateFile,
	};

	try {
		await unrelatedPage.setContent(
			"<title>unrelated owner page</title><p id='unrelated'>untouched</p>",
		);
		await page.goto(fixture.origin);
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
				"page",
				"playwright",
				"--eval",
				"await page.mouse.wheel(0, 800)",
				...scope,
			],
			environment,
		);
		expect(wheel).toBe("");
		const inline = await runCli(
			[
				"page",
				"playwright",
				"--eval",
				"return { title: await page.title(), scrollY: await page.evaluate(() => scrollY) };",
				...scope,
			],
			environment,
		);
		expect(JSON.parse(inline)).toEqual({ title: "jsdbg Playwright E2E", scrollY: 800 });

		const stdinProgram =
			'return { url: page.url(), marker: await page.locator("#marker").textContent() };';
		const fromStdin = await runCli(
			["page", "playwright", "-", ...scope],
			environment,
			stdinProgram,
		);
		expect(JSON.parse(fromStdin)).toEqual({
			url: `${fixture.origin}/`,
			marker: "selected page",
		});
		expect(await page.title()).toBe("jsdbg Playwright E2E");
		const rejected = JSON.parse(
			await runCli(
				[
					"page",
					"playwright",
					"--eval",
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
			"page",
			"playwright",
			"--eval",
			`await page.evaluate(() => {
				document.title = "proxy command pending";
				return new Promise(() => {});
			})`,
			...scope,
		], environment, { timeoutMs: 15_000 });
		await expect.poll(() => page.title()).toBe("proxy command pending");
		await page.close();
		const destroyed = await destruction;
		await appendTranscriptCommand(
			[
				"page",
				"playwright",
				"--eval",
				"<pending page.evaluate>",
				...scope,
			],
			destroyed.output,
		);
		expect(destroyed.code, destroyed.output).not.toBe(0);
		expect(destroyed.timedOut, destroyed.output).toBe(false);
		expect(destroyed.durationMs, destroyed.output).toBeLessThan(10_000);
		expect(await unrelatedPage.title()).toBe("unrelated owner page");
		await appendFile(
			transcriptPath,
			"\nBrowser-wide operations were rejected, the unrelated page remained untouched, and destroying the selected page cancelled its pending proxy command promptly. The original Playwright owner remained connected.\n",
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
	const command = `jsdbg ${args.map(shellQuote).join(" ")}`;
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
	const server = createServer((_request, response) => {
		response.writeHead(200, { "content-type": "text/html; charset=utf-8" });
		response.end(`<!doctype html>
<title>jsdbg Playwright E2E</title>
<style>body { margin: 0 } main { height: 3000px; padding: 16px }</style>
<main><strong id="marker">selected page</strong></main>`);
	});
	await new Promise((resolveReady, reject) => {
		server.once("error", reject);
		server.listen(0, "127.0.0.1", resolveReady);
	});
	const address = server.address();
	if (typeof address !== "object" || address === null) {
		throw new Error("fixture server did not bind to TCP");
	}
	return {
		origin: `http://127.0.0.1:${address.port}`,
		close: () =>
			new Promise((resolveClose, reject) =>
				server.close((error) => (error ? reject(error) : resolveClose())),
			),
	};
}
