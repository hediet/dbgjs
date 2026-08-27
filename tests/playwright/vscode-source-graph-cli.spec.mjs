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

test("renders the context-wide vscode.dev source-map graph", async () => {
	test.setTimeout(600_000);
	const build = await run(
		"cargo",
		["build", "--release", "--bins"],
		{},
		{ timeoutMs: commandTimeoutMs },
	);
	expect(build.code, build.output).toBe(0);
	await mkdir(resolve("artifacts"), { recursive: true });
	await writeFile(
		transcriptPath,
		"# How are live vscode.dev sources connected?\n\n" +
			"This golden scenario opens vscode.dev, captures live V8 coverage to resolve its source maps, and renders the debugger-context-wide compacted source graph.\n",
	);

	const stateDirectory = await mkdtemp(join(tmpdir(), "jsdbg-vscode-source-graph-"));
	const userDataDirectory = await mkdtemp(join(tmpdir(), "cdp-vscode-source-graph-"));
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
	const environment = {
		JSDBG_SERVICE_EXE: service,
		JSDBG_SERVICE_STATE: join(stateDirectory, "service.json"),
		JSDBG_SOURCE_MAP_CACHE: join(tmpdir(), "jsdbg-vscode-source-map-cache"),
	};
	try {
		const page = browser.pages()[0] ?? (await browser.newPage());
		await page.goto("https://vscode.dev/", {
			waitUntil: "domcontentloaded",
			timeout: 120_000,
		});
		await expect(page.locator(".monaco-workbench")).toBeVisible({
			timeout: 120_000,
		});
		const endpoint = await readCdpEndpoint(debuggingPort);
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
		const pageTarget = await findPageTargetId(
			cli,
			"vscode-source-graph",
			"browser",
			"https://vscode.dev/",
			environment,
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
		await new Promise((resolve_) => setTimeout(resolve_, 2_000));
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
		expect(graph).not.toContain("~up");
		expect(graph.trimEnd().split("\n").length).toBeGreaterThan(2);
	} finally {
		await run(cli, ["service", "stop"], environment, { timeoutMs: 10_000 });
		await browser.close();
		await rm(stateDirectory, { recursive: true, force: true });
		await rm(userDataDirectory, { recursive: true, force: true });
	}
});

async function retryCli(explanation, arguments_, environment, timeoutMs) {
	const deadline = Date.now() + timeoutMs;
	let last;
	while (Date.now() < deadline) {
		last = await run(cli, arguments_, environment, {
			timeoutMs: Math.min(commandTimeoutMs, Math.max(5_000, deadline - Date.now())),
		});
		if (last.code === 0) {
			return recordCompleted(explanation, arguments_, last);
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
	return recordCompleted(explanation, arguments_, result);
}

async function recordCompleted(explanation, arguments_, result) {
	stepNumber += 1;
	const command = ["jsdbg", ...arguments_].map(quoteArgument).join(" ");
	const output = result.output.endsWith("\n") ? result.output : `${result.output}\n`;
	const text = `\n## Step ${stepNumber} — ${explanation}\n\n\`\`\`console\n$ ${command}\n\`\`\`\n\n\`\`\`text\n${output}\`\`\`\n`;
	process.stdout.write(text);
	await appendFile(transcriptPath, text);
	return result.output;
}

function quoteArgument(argument) {
	return /^[A-Za-z0-9_./:@%+=,#-]+$/.test(argument)
		? argument
		: JSON.stringify(argument);
}
