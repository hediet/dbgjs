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
		const targetId = pageTargetId(connected);
		await runCli(
			"We record the authored breakpoint as durable intent. The source identity comes directly from the source map, and the location is one-based.",
			[
				"breakpoint",
				"set",
				"typescript-e2e",
				"calculate",
				authoredSource,
				"2",
				"3",
			],
			environment,
		);
		const attached = await runCli(
			"We attach jsdbg to the fixture's page target. Unlike Chromium's inventory flag, this creates jsdbg's own flattened debugger session, reducer actor, and script discovery pipeline.",
			[
				"target",
				"attach",
				"typescript-e2e",
				"browser",
				targetId,
			],
			environment,
		);
		expect(attached).toContain(
			"calculate  ../src/app.ts:2:3  [installed; 1 binding]",
		);

		const pausedCommand = runCli(
			"Attach returned with the breakpoint installed, so we begin an observable wait for the first pause epoch before triggering application code.",
			[
				"target",
				"wait",
				"typescript-e2e",
				"browser",
				targetId,
				"paused",
				"0",
				"30000",
			],
			environment,
		);
		await emitTranscript(
			"\n> The wait is active. We now click the fixture button in Chromium; execution should stop inside `calculate()` before the handler returns.\n\n",
		);
		fixtureServer.trigger();
		const paused = await pausedCommand;
		expect(paused).toContain("[paused at epoch 1]");
		expect(paused).toContain("#0 calculate");
		expect(paused).toMatch(/Authored: \.\.\/src\/app\.ts:2:[1-9][0-9]*/);
		const pauseEpoch = paused.match(/Pause: epoch ([0-9]+)/)?.[1];
		expect(pauseEpoch).toBeDefined();

		await runCli(
			"The authored frame is correct, so we resume using the exact pause epoch. Supplying the epoch prevents a stale command from resuming a newer pause.",
			[
				"target",
				"resume",
				"typescript-e2e",
				"browser",
				targetId,
				pauseEpoch,
			],
			environment,
		);
		await runCli(
			"We wait until the reducer observes Chromium's resumed event; command acceptance alone is not proof that execution continued.",
			[
				"target",
				"wait",
				"typescript-e2e",
				"browser",
				targetId,
				"running",
			],
			environment,
		);
		await fixtureServer.waitForResult("41");

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
		const reconnectedTargetId = pageTargetId(reconnected);
		expect(reconnectedTargetId).not.toBe(targetId);
		const reattached = await runCli(
			"We attach the same logical target under generation 2. No session handle from generation 1 may be reused.",
			[
				"target",
				"attach",
				"typescript-e2e",
				"browser",
				reconnectedTargetId,
			],
			environment,
		);
		expect(reattached).toContain("Generation: 2");
		expect(reattached).toContain(
			"calculate  ../src/app.ts:2:3  [installed; 1 binding]",
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

function pageTargetId(output) {
	const targetId = output.match(/^\s+([A-F0-9]{32})\s+page\s+/m)?.[1];
	expect(targetId, output).toBeDefined();
	return targetId;
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
