import { createServer } from "node:http";
import {
	appendFile,
	mkdir,
	mkdtemp,
	readFile,
	rm,
	writeFile,
} from "node:fs/promises";
import { tmpdir } from "node:os";
import { join, resolve } from "node:path";
import { expect, test } from "@playwright/test";
import { run } from "./live-test-harness.mjs";

const fixtureDirectory = resolve("tests/fixtures/typescript-browser");
const executableSuffix = process.platform === "win32" ? ".exe" : "";
const cli = resolve(`target/debug/jsdbg${executableSuffix}`);
const service = resolve(`target/debug/jsdbg-service${executableSuffix}`);
const transcriptPath = resolve("test-results/typescript-cli-transcript.md");
let stepNumber = 0;

test("CLI pauses at an authored TypeScript breakpoint through HubRPC", async () => {
	test.setTimeout(600_000);
	const build = await run("cargo", ["build", "--bins"], {});
	expect(build.code, build.output).toBe(0);
	const compile = await run(
		resolve(`node_modules/.bin/tsc${process.platform === "win32" ? ".cmd" : ""}`),
		["-p", join(fixtureDirectory, "tsconfig.json")],
		{},
	);
	expect(compile.code, compile.output).toBe(0);
	await mkdir(resolve("test-results"), { recursive: true });
	await writeFile(
		transcriptPath,
		"# Debugging authored TypeScript with `jsdbg`\n\nThis transcript exercises the CLI, authenticated HubRPC service, reducer-driven debugger engine, Chromium CDP, and source maps.\n",
	);

	const stateDirectory = await mkdtemp(join(tmpdir(), "jsdbg-typescript-e2e-"));
	const stateFile = join(stateDirectory, "service.json");
	const fixtureServer = await startFixtureServer();
	const environment = {
		JSDBG_SERVICE_EXE: service,
		JSDBG_SERVICE_STATE: stateFile,
	};
	let serviceStarted = false;

	try {
		const authoredSource = "../src/app.ts";

		await runCli(
			"We start with a durable debugging context. It will retain our connection and breakpoint intent across disconnects.",
			["context", "create", "typescript-e2e"],
			environment,
		);
		serviceStarted = true;
		const connected = await runCli(
			"We describe how to launch the debuggee by giving jsdbg a pasteable page URL. The Playwright provider starts bundled Chromium, opens the page, discovers CDP, and connects immediately.",
			[
				"connection",
				"add",
				"typescript-e2e",
				"browser",
				"--playwright",
				fixtureServer.origin,
				"--connect",
			],
			environment,
		);
		expect(connected).toContain("page  page");
		await runCli(
			"We set a durable breakpoint where checkout computes its subtotal. The unique selector `page` keeps the command readable.",
			[
				"breakpoint",
				"set",
				"typescript-e2e",
				"checkout",
				authoredSource,
				"7",
				"3",
			],
			environment,
		);
		const attached = await runCli(
			"Targets are auto-attached on connect. We inspect the uniquely selected page and see that the authored breakpoint is already installed.",
			[
				"target",
				"show",
				"typescript-e2e",
				"browser",
				"page",
			],
			environment,
		);
		expect(attached).toContain(
			"checkout  ../src/app.ts:7:3  [installed; 1 binding]",
		);
		await runCli(
			"We add a logpoint inside applyDiscount. It records useful runtime data without stopping execution.",
			[
				"target",
				"logpoint",
				"typescript-e2e",
				"browser",
				"page",
				"discount",
				authoredSource,
				"2",
				"3",
				"({ total, rate })",
			],
			environment,
		);

		const pausedCommand = runCli(
			"We wait for checkout to pause, then trigger the purchase in the browser.",
			[
				"target",
				"wait",
				"typescript-e2e",
				"browser",
				"page",
				"paused",
				"0",
				"30000",
			],
			environment,
		);
		await emitTranscript(
			"\n> The wait is active. We now trigger checkout in Chromium.\n\n",
		);
		fixtureServer.trigger();
		const paused = await pausedCommand;
		expect(paused).toContain("[paused at epoch 1]");
		expect(paused).toContain("#0 checkout");
		expect(paused).toContain("Source: ../src/app.ts");
		expect(paused).toMatch(/>\s+7 \|/);

		await runCli(
			"We evaluate the current item list in the selected frame.",
			["target", "evaluate", "typescript-e2e", "browser", "page", "items"],
			environment,
		);
		await runCli(
			"We add a watch-style inspection for the number of items.",
			["target", "watch", "typescript-e2e", "browser", "page", "items.length"],
			environment,
		);

		await runCli(
			"We step over the subtotal calculation. The current pause epoch is inferred automatically.",
			[
				"target",
				"step",
				"typescript-e2e",
				"browser",
				"page",
				"over",
			],
			environment,
		);
		const afterOver = await runCli(
			"We wait for the next pause and verify that execution advanced to the discount call.",
			[
				"target",
				"wait",
				"typescript-e2e",
				"browser",
				"page",
				"paused",
				"1",
				"30000",
			],
			environment,
		);
		expect(afterOver).toMatch(/>\s+8 \|/);
		await runCli(
			"Now that the reduction completed, we inspect the subtotal.",
			["target", "evaluate", "typescript-e2e", "browser", "page", "subtotal"],
			environment,
		);

		await runCli(
			"We step into applyDiscount.",
			["target", "step", "typescript-e2e", "browser", "page", "into"],
			environment,
		);
		const insideDiscount = await runCli(
			"We wait for the callee pause; the preferred location is authored TypeScript.",
			["target", "wait", "typescript-e2e", "browser", "page", "paused", "2", "30000"],
			environment,
		);
		expect(insideDiscount).toMatch(/>\s+2 \|/);
		await runCli(
			"We inspect the discount rate while inside the helper.",
			["target", "evaluate", "typescript-e2e", "browser", "page", "rate"],
			environment,
		);
		await runCli(
			"We step out to checkout.",
			["target", "step", "typescript-e2e", "browser", "page", "out"],
			environment,
		);
		await runCli(
			"We wait for checkout to regain control.",
			["target", "wait", "typescript-e2e", "browser", "page", "paused", "3", "30000"],
			environment,
		);
		await runCli(
			"We resume without spelling an epoch; jsdbg safely uses the current pause.",
			["target", "resume", "typescript-e2e", "browser", "page"],
			environment,
		);
		await runCli(
			"We confirm that execution is running again.",
			["target", "wait", "typescript-e2e", "browser", "page", "running"],
			environment,
		);
		await fixtureServer.waitForResult("45");

		const disconnected = await runCli(
			"After proving the calculation completed, we disconnect cleanly. Runtime facts should disappear while durable intent remains.",
			["connection", "disconnect", "typescript-e2e", "browser"],
			environment,
		);
		expect(disconnected).toContain("browser  [disconnected; generation 1]");
		const reconnected = await runCli(
			"We reconnect to exercise generation safety. The provider launches a fresh browser, so both the connection generation and page target identity change.",
			["connection", "connect", "typescript-e2e", "browser"],
			environment,
		);
		expect(reconnected).toContain("generation 2]");
		const reattached = await runCli(
			"The fresh page was auto-attached. We select it by type and verify that durable breakpoint intent was reinstalled.",
			[
				"target",
				"show",
				"typescript-e2e",
				"browser",
				"page",
			],
			environment,
		);
		expect(reattached).toContain("Generation: 2");
		expect(reattached).toContain(
			"checkout  ../src/app.ts:7:3  [installed; 1 binding]",
		);
		await runCli(
			"The reconnect check is complete; we close the live CDP connection before stopping the service.",
			["connection", "disconnect", "typescript-e2e", "browser"],
			environment,
		);
		await runCli(
			"Finally, we stop the shared debugger service and let it clean up its authenticated local endpoint.",
			["service", "stop"],
			environment,
		);
		await emitTranscript(`\n---\n\n_Transcript saved to \`${transcriptPath}\`._\n`);
		serviceStarted = false;
	} finally {
		if (serviceStarted) {
			await run(cli, ["service", "stop"], environment);
		}
		await fixtureServer.close();
		await rm(stateDirectory, { recursive: true, force: true });
		await rm(join(fixtureDirectory, "dist"), { recursive: true, force: true });
	}
});

async function runCli(explanation, arguments_, environment) {
	const result = await runCliResult(explanation, arguments_, environment);
	expect(result.code, `${arguments_.join(" ")}\n${result.output}`).toBe(0);
	return result.output;
}

async function runCliResult(explanation, arguments_, environment) {
	stepNumber += 1;
	await emitTranscript(
		`\n## Step ${stepNumber} — ${explanation}\n\n\`\`\`console\n$ ${formatCommand("jsdbg", arguments_)}\n\`\`\`\n\n`,
	);
	const result = await run(cli, arguments_, environment);
	const output = result.output.endsWith("\n") ? result.output : `${result.output}\n`;
	await emitTranscript(`\`\`\`text\n${output}\`\`\`\n`);
	return result;
}

async function emitTranscript(text) {
	process.stdout.write(text);
	await appendFile(transcriptPath, text);
}

function formatCommand(command, arguments_) {
	return [command, ...arguments_].map(quoteArgument).join(" ");
}

function quoteArgument(argument) {
	return /^[A-Za-z0-9_./:@%+=,-]+$/.test(argument)
		? argument
		: JSON.stringify(argument);
}

async function startFixtureServer() {
	let triggerRequested = false;
	let observedResult;
	let resolveResult;
	const resultObserved = new Promise((resolve_) => {
		resolveResult = resolve_;
	});
	const server = createServer(async (request, response) => {
		try {
			if (request.url?.startsWith("/result?")) {
				observedResult = new URL(request.url, "http://localhost").searchParams.get(
					"value",
				);
				resolveResult(observedResult);
				response.statusCode = 204;
				response.end();
				return;
			}
			switch (request.url) {
				case "/":
					response.setHeader("content-type", "text/html; charset=utf-8");
					response.end(
						'<!doctype html><button id="run">Run fixture</button><script src="/dist/app.js"></script><script>setInterval(async () => { if ((await fetch("/trigger")).ok) document.querySelector("#run").click(); }, 25)</script>',
					);
					break;
				case "/trigger":
					response.statusCode = triggerRequested ? 200 : 404;
					triggerRequested = false;
					response.end();
					break;
				case "/dist/app.js":
					response.setHeader("content-type", "text/javascript; charset=utf-8");
					response.end(await readFile(join(fixtureDirectory, "dist/app.js")));
					break;
				case "/dist/app.js.map":
					response.setHeader("content-type", "application/json; charset=utf-8");
					response.end(await readFile(join(fixtureDirectory, "dist/app.js.map")));
					break;
				default:
					response.statusCode = 404;
					response.end("not found");
			}
		} catch (error) {
			response.statusCode = 500;
			response.end(String(error));
		}
	});
	await new Promise((resolve_, reject) => {
		server.once("error", reject);
		server.listen(0, "127.0.0.1", resolve_);
	});
	const address = server.address();
	if (typeof address !== "object" || address === null) {
		throw new Error("fixture server did not bind a TCP address");
	}
	return {
		origin: `http://127.0.0.1:${address.port}`,
		trigger: () => {
			triggerRequested = true;
		},
		waitForResult: async (expected) => {
			if (observedResult !== undefined) {
				expect(observedResult).toBe(expected);
				return;
			}
			await expect(resultObserved).resolves.toBe(expected);
		},
		close: () =>
			new Promise((resolve_, reject) =>
				server.close((error) => (error ? reject(error) : resolve_())),
			),
	};
}
