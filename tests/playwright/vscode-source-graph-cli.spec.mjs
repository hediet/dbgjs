import { appendFile, mkdir, mkdtemp, rm, writeFile } from "node:fs/promises";
import { tmpdir } from "node:os";
import { join, resolve } from "node:path";
import { chromium, expect, test } from "@playwright/test";
import {
	allocatePort,
	findPageTargetId,
	readCdpEndpoint,
	run,
} from "./live-test-harness.mjs";

const executableSuffix = process.platform === "win32" ? ".exe" : "";
const cli = resolve(`target/release/jsdbg${executableSuffix}`);
const service = resolve(`target/release/jsdbg-service${executableSuffix}`);
const transcriptPath = resolve("artifacts/vscode-source-map-graph.md");
const commandTimeoutMs = 300_000;
let stepNumber = 0;
let totalDurationMs = 0;

test("renders the context-wide vscode.dev source-map graph", async () => {
	test.setTimeout(600_000);
	stepNumber = 0;
	totalDurationMs = 0;
	await mkdir(resolve("artifacts"), { recursive: true });
	await writeFile(
		transcriptPath,
		"# How are live vscode.dev sources connected?\n\n" +
			"This golden scenario opens vscode.dev, captures live V8 coverage to resolve its source maps, and renders the debugger-context-wide compacted source graph.\n",
	);
	const build = await run(
		"cargo",
		["build", "--release", "--bins"],
		{},
		{ timeoutMs: commandTimeoutMs },
	);
	expect(build.code, build.output).toBe(0);
	await recordCompleted(
		"Build the release binaries.",
		["cargo", "build", "--release", "--bins"],
		build,
	);

	const setup = await recordOperation("Launch the instrumented browser.", async () => {
		const stateDirectory = await mkdtemp(
			join(tmpdir(), "jsdbg-vscode-source-graph-"),
		);
		const userDataDirectory = await mkdtemp(
			join(tmpdir(), "cdp-vscode-source-graph-"),
		);
		const debuggingPort = await allocatePort();
		const browser = await chromium.launchPersistentContext(userDataDirectory, {
			headless: true,
			ignoreHTTPSErrors: true,
			args: [
				`--remote-debugging-port=${debuggingPort}`,
				"--remote-allow-origins=*",
				"--no-first-run",
				"--no-default-browser-check",
			],
		});
		return { browser, debuggingPort, stateDirectory, userDataDirectory };
	});
	const { browser, debuggingPort, stateDirectory, userDataDirectory } = setup;
	const environment = {
		JSDBG_SERVICE_EXE: service,
		JSDBG_SERVICE_STATE: join(stateDirectory, "service.json"),
		JSDBG_SOURCE_MAP_CACHE: join(tmpdir(), "jsdbg-vscode-source-map-cache"),
	};
	try {
		const endpoint = await recordOperation("Load vscode.dev.", async () => {
			const page = browser.pages()[0] ?? (await browser.newPage());
			await page.goto("https://vscode.dev/", {
				waitUntil: "domcontentloaded",
				timeout: 120_000,
			});
			await expect(page.locator(".monaco-workbench")).toBeVisible({
				timeout: 120_000,
			});
			return readCdpEndpoint(debuggingPort);
		});
		await runCli(
			"Create and select a debugger context.",
			["context", "create", "--context", "vscode-source-graph", "--set"],
			environment,
		);
		await runCli(
			"Attach jsdbg to the Playwright-launched browser and its vscode.dev page.",
			[
				"connection",
				"add",
				endpoint,
				"--context",
				"vscode-source-graph",
				"--connection",
				"browser",
				"--connect",
			],
			environment,
		);
		const pageTarget = await recordOperation(
			"Find the vscode.dev page target.",
			() =>
				findPageTargetId(
					cli,
					"vscode-source-graph",
					"browser",
					"https://vscode.dev/",
					environment,
				),
		);
		await runCli(
			"Select the vscode.dev page instead of its extension-host worker.",
			["set", "target", "--target", pageTarget],
			environment,
		);
		await retryCli(
			"Wait for the vscode.dev workbench.",
			["target", "click", ".monaco-workbench"],
			environment,
			60_000,
		);
		await runCli(
			"Start precise coverage so loaded runtime scripts are observed.",
			["coverage", "start"],
			environment,
		);
		await recordOperation("Collect coverage for 2 seconds.", () =>
			new Promise((resolve_) => setTimeout(resolve_, 2_000)),
		);
		await runCli(
			"Freeze the observed vscode.dev execution.",
			["coverage", "stop"],
			environment,
		);
		const coverage = await runCli(
			"Resolve the captured runtime ranges through their source maps.",
			["coverage", "show", ".", "--max-lines", "20"],
			environment,
		);
		expect(coverage).toMatch(/\.tsx?\s+\d+ HL, \d+ RL/);
		const graph = await runCli(
			"Render the complete shared compacted source graph.",
			["source", "graph"],
			environment,
		);
		expect(graph).not.toContain("No sources are currently observed.");
		expect(graph).toContain("source map");
		expect(graph).toContain("workbench.web.main.internal.js");
		expect(graph).toMatch(/\[\d+ mappings, fan-out\]/);
		expect(graph).toMatch(
			/https:\/\/main\.vscode-cdn\.net\/sourcemaps\/[^/]+\/src\/vs\/\*\s+\[\d+ sources\]/,
		);
		expect(graph).toMatch(/\n├─ source @microsoft\/1ds-core-js\//);
		expect(graph).toMatch(/\n├─ source anonymous\//);
		expect(graph).not.toMatch(/\n[├└]─ source https?:\/\//);
		expect(graph).not.toMatch(/\n[├└]─ source source:\/\/runtime\//);
		expect(graph).not.toContain("~up");
		expect(graph.trimEnd().split("\n").length).toBeGreaterThan(2);
		const loadedTree = await runCli(
			"Render the loaded runtime source tree.",
			["source", "tree", "loaded", "--max-lines", "30", "--no-trim"],
			environment,
		);
		expect(loadedTree).not.toContain("No loaded sources are currently observed.");
		expect(loadedTree).toContain("https://main.vscode-cdn.net/");
		expect(loadedTree).toContain("workbench.web.main.internal.js");
		const resolvedTree = await runCli(
			"Render the terminal sources reached through all known projections.",
			["source", "tree", "resolved", "--max-lines", "30", "--no-trim"],
			environment,
		);
		expect(resolvedTree).not.toContain(
			"No resolved sources are currently observed.",
		);
		expect(resolvedTree).toContain("https://main.vscode-cdn.net/");
		expect(resolvedTree).toMatch(/sourcemaps\/[^/]+\/src/);
	} finally {
		await recordOperation("Stop services and remove temporary state.", async () => {
			await run(cli, ["service", "stop"], environment, { timeoutMs: 10_000 });
			await browser.close();
			await rm(stateDirectory, { recursive: true, force: true });
			await rm(userDataDirectory, { recursive: true, force: true });
		});
		await appendFile(
			transcriptPath,
			`\n## Total recorded time\n\n${formatDuration(totalDurationMs)}\n`,
		);
	}
});

async function retryCli(explanation, arguments_, environment, timeoutMs) {
	const startedAt = performance.now();
	const deadline = Date.now() + timeoutMs;
	let last;
	while (Date.now() < deadline) {
		last = await run(cli, arguments_, environment, {
			timeoutMs: Math.min(commandTimeoutMs, Math.max(5_000, deadline - Date.now())),
		});
		if (last.code === 0) {
			last.durationMs = performance.now() - startedAt;
			return recordCompleted(explanation, ["jsdbg", ...arguments_], last);
		}
		await new Promise((resolve_) => setTimeout(resolve_, 250));
	}
	throw new Error(`${arguments_.join(" ")}\n${last?.output}`);
}

async function runCli(explanation, arguments_, environment) {
	const result = await run(cli, arguments_, environment, {
		timeoutMs: commandTimeoutMs,
	});
	expect(result.code, `${arguments_.join(" ")}\n${result.output}`).toBe(0);
	return recordCompleted(explanation, ["jsdbg", ...arguments_], result);
}

async function recordCompleted(explanation, arguments_, result) {
	return recordStep(
		explanation,
		result.durationMs,
		`\`\`\`console\n$ ${arguments_.map(quoteArgument).join(" ")}\n\`\`\`\n\n\`\`\`text\n${
			result.output.endsWith("\n") ? result.output : `${result.output}\n`
		}\`\`\`\n`,
	).then(() => result.output);
}

async function recordOperation(explanation, operation) {
	const startedAt = performance.now();
	const value = await operation();
	await recordStep(explanation, performance.now() - startedAt);
	return value;
}

async function recordStep(explanation, durationMs, body = "") {
	stepNumber += 1;
	const roundedDurationMs = Math.round(durationMs);
	totalDurationMs += roundedDurationMs;
	const text = `\n## Step ${stepNumber} — ${explanation}\n\nTime: ${formatDuration(roundedDurationMs)}\n\n${body}`;
	process.stdout.write(text);
	await appendFile(transcriptPath, text);
}

function formatDuration(durationMs) {
	return `${durationMs} ms (${(durationMs / 1000).toFixed(3)} s)`;
}

function quoteArgument(argument) {
	return /^[A-Za-z0-9_./:@%+=,#-]+$/.test(argument)
		? argument
		: JSON.stringify(argument);
}
