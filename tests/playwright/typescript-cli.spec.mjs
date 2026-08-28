import { createServer } from "node:http";
import {
	appendFile,
	mkdir,
	readFile,
	rm,
	writeFile,
} from "node:fs/promises";
import { join, resolve } from "node:path";
import { expect, test } from "@playwright/test";
import {
	findChromeExecutable,
	findPageTargetId,
	run,
} from "./live-test-harness.mjs";

const fixtureDirectory = resolve("tests/fixtures/typescript-browser");
const executableSuffix = process.platform === "win32" ? ".exe" : "";
const cli = resolve(`target/debug/jsdbg${executableSuffix}`);
const service = resolve(`target/debug/jsdbg-service${executableSuffix}`);
const transcriptPath = resolve("artifacts/typescript-cli-transcript.md");
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
	const chromeExecutable = await findChromeExecutable();
	await mkdir(resolve("artifacts"), { recursive: true });
	await writeFile(
		transcriptPath,
		"# Debugging authored TypeScript with `jsdbg`\n\nThis transcript exercises the CLI, authenticated HubRPC service, reducer-driven debugger engine, installed Chrome, CDP, and source maps.\n",
	);

	const stateDirectory = resolve(
		"artifacts",
		`.jsdbg-typescript-e2e-${process.pid}`,
	);
	await mkdir(stateDirectory, { recursive: true });
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
			["context", "create", "--context", "typescript-e2e"],
			environment,
		);
		serviceStarted = true;
		await runCli(
			"We select this context once so subsequent debugger commands can stay focused on the investigation.",
			["set", "context", "--context", "typescript-e2e"],
			environment,
		);
		const connected = await runCli(
			"We describe how to launch the debuggee by giving jsdbg a pasteable page URL. The native Chrome provider starts installed Chrome, opens the page, discovers CDP, and connects immediately.",
			[
				"connection",
				"add",
				"--chrome",
				fixtureServer.origin,
				"--context",
				"typescript-e2e",
				"--connection",
				"browser",
				"--executable",
				chromeExecutable,
				"--user-data-dir",
				"artifacts/typescript-cli-chrome-profile",
				"--connect",
			],
			environment,
		);
		expect(connected).toMatch(/^\s+page\s+.*http:\/\/127\.0\.0\.1:/m);
		const pageTarget = await findPageTargetId(
			cli,
			"typescript-e2e",
			"browser",
			fixtureServer.origin,
			environment,
		);
		await runCli(
			"We select the exact fixture page target so later browser-created targets cannot make the selection ambiguous.",
			["set", "target", "--target", pageTarget],
			environment,
		);
		const missing = await runCli(
			"We request a source that is not present. Breakpoint output explains the pending assessment directly, including the scripts that were checked.",
			[
				"breakpoint",
				"set",
				"missing",
				"missing.ts",
				"1",
				"--context",
				"typescript-e2e",
			],
			environment,
		);
		expect(missing).toContain("missing  missing.ts:1:1  [source not found]");
		expect(missing).toContain("none matching 'missing.ts'");
		expect(missing).toContain("Breakpoint missing: source not found");

		const ambiguous = await runCli(
			"The short source name matches two authored candidates. The same breakpoint command reports ambiguity and prints bounded candidates, without requiring a source command.",
			[
				"breakpoint",
				"set",
				"ambiguous",
				"app.ts",
				"8",
				"--column",
				"3",
				"--context",
				"typescript-e2e",
			],
			environment,
		);
		expect(ambiguous).toContain(
			"ambiguous  app.ts:8:3  [ambiguous source; 2 candidates]",
		);
		expect(ambiguous).toContain("Breakpoint ambiguous: ambiguous source");
		expect(ambiguous).toContain("../src/app.ts");
		expect(ambiguous).toContain("../candidate/app.ts");

		const breakpoint = await runCli(
			"We qualify the intended candidate. Breakpoint output now includes the physical application and source-map projection provenance.",
			[
				"breakpoint",
				"set",
				"checkout",
				authoredSource,
				"8",
				"--column",
				"3",
				"--context",
				"typescript-e2e",
			],
			environment,
		);
		expect(breakpoint).toContain(
			"checkout  ../src/app.ts:8:3  [installed; 1 binding]",
		);
		expect(breakpoint).toContain(
			"Breakpoint checkout: ../src/app.ts — CheckoutService.checkout",
		);
		expect(breakpoint).toMatch(/script .* v1 -> .* via source map/);
		const breakpointSnapshot = await runJsonSilent(
			["context", "show", "--context", "typescript-e2e"],
			environment,
		);
		const missingSnapshot = breakpointSnapshot.breakpoints.find(
			(candidate) => candidate.id === "missing",
		);
		expect(missingSnapshot.pendingReason.kind).toBe("sourceNotFound");
		expect(missingSnapshot.targets[0].assessments[0]).toMatchObject({
			connectionId: "browser",
			connectionGeneration: 1,
			scriptVersion: 1,
		});
		const ambiguousSnapshot = breakpointSnapshot.breakpoints.find(
			(candidate) => candidate.id === "ambiguous",
		);
		expect(ambiguousSnapshot.pendingReason.kind).toBe("ambiguousSource");
		expect(ambiguousSnapshot.pendingReason.candidates).toHaveLength(2);
		const checkoutSnapshot = breakpointSnapshot.breakpoints.find(
			(candidate) => candidate.id === "checkout",
		);
		expect(checkoutSnapshot.applications[0]).toMatchObject({
			connectionId: "browser",
			connectionGeneration: 1,
			scriptVersion: 1,
		});
		expect(checkoutSnapshot.applications[0].mapping.projection.length).toBeGreaterThan(
			0,
		);
		const logpoints = await runCli(
			"We install two logpoints in one batch and inspect both authored source contexts.",
			[
				"target",
				"logpoints",
				"discount",
				authoredSource,
				"3",
				"3",
				"({ total, rate })",
				"checkout-entry",
				authoredSource,
				"10",
				"3",
				"finalTotal",
			],
			environment,
		);
		expect(logpoints).toContain(
			"Breakpoint log:discount: ../src/app.ts — CheckoutService.applyDiscount",
		);
		expect(logpoints).toContain(
			"Breakpoint log:checkout-entry: ../src/app.ts — CheckoutService.checkout",
		);
		await runCli(
			"We start precise function and block coverage before the interaction.",
			["coverage", "start"],
			environment,
		);

		const pausedCommand = runCli(
			"We wait for checkout to pause, then trigger the purchase in the browser.",
			[
				"target",
				"wait",
				"paused",
				"0",
				"30000",
				"--context",
				"typescript-e2e",
				"--connection",
				"browser",
				"--target",
				pageTarget,
			],
			environment,
		);
		await emitTranscript(
			"\n> The wait is active. We now trigger checkout in Chromium.\n\n",
		);
		await runCli(
			"We perform a real CDP click: jsdbg resolves the button's DOM box and dispatches mouse press/release input events.",
			["target", "click", "#run"],
			environment,
		);
		const paused = await pausedCommand;
		expect(paused).toContain("[paused at epoch 1]");
		expect(paused).toContain("#0 CheckoutService.checkout");
		expect(paused).toContain("Source: ../src/app.ts");
		expect(paused).toContain("CheckoutService.checkout");
		expect(paused).toMatch(/>\s+8 \|/);

		await runCli(
			"We evaluate the current item list in the selected frame.",
			["target", "eval", "items"],
			environment,
		);
		await runCli(
			"We add a watch for the item count. It will be reevaluated and shown after every later pause.",
			["target", "watch", "items.length"],
			environment,
		);
		await runCli(
			"We capture the recording so far without stopping it; later coverage must accumulate on top of this immutable snapshot.",
			["coverage", "capture", "--id", "background"],
			environment,
		);
		const firstCoverageJson = await runJsonSilent(["coverage", "capture"], environment);
		const firstSource = firstCoverageJson.sources.find(
			(source) => source.functions.some((fn) => fn.name === "checkout"),
		);
		expect(firstSource).toBeDefined();
		expect(firstSource.functions.some((fn) => fn.name === "checkout")).toBe(true);
		expect(
			firstSource.functions.flatMap((fn) => fn.ranges).some((range) => range.count === 0),
		).toBe(true);
		expect(
			firstSource.functions.every((fn) =>
				fn.ranges.every(
					(range) =>
						range.startOffset >= 0 &&
						range.endOffset > range.startOffset &&
						range.count >= 0,
				),
			),
		).toBe(true);

		const afterOver = await runCli(
			"We step over the subtotal calculation. Scope and pause epoch are inferred, and the command waits briefly for the next pause.",
			["target", "step", "over"],
			environment,
		);
		expect(afterOver).toMatch(/>\s+9 \|/);
		expect(afterOver).toContain("items.length: 3");
		await runCli(
			"Now that the reduction completed, we inspect the subtotal.",
			["target", "eval", "subtotal"],
			environment,
		);

		const insideDiscount = await runCli(
			"We step into applyDiscount and receive the callee pause directly.",
			["target", "step", "into"],
			environment,
		);
		expect(insideDiscount).toContain("CheckoutService.applyDiscount");
		expect(insideDiscount).toMatch(/>\s+3 \|/);
		expect(insideDiscount).toContain("items.length: unavailable in this frame");
		const discountLog = await runCli(
			"Read the new discount logpoint event through the cursor-based log stream.",
			["log"],
			environment,
		);
		expect(discountLog).toContain('discount {"total":50,"rate":0.1}');
		await runCli(
			"We inspect the discount rate while inside the helper.",
			["target", "eval", "rate"],
			environment,
		);
		const afterOut = await runCli(
			"We step out and receive checkout's next authored pause directly.",
			["target", "step", "out"],
			environment,
		);
		expect(afterOut).toMatch(/>\s+10 \|/);
		expect(afterOut).toContain("items.length: 3");
		await runCli(
			"We resume without spelling an epoch; jsdbg safely uses the current pause.",
			["target", "resume"],
			environment,
		);
		await runCli(
			"We confirm that execution is running again.",
			["target", "show"],
			environment,
		);
		await fixtureServer.waitForResult("45");
		const finalCoverageJson = await runJsonSilent(["coverage", "capture"], environment);
		const finalSource = finalCoverageJson.sources.find(
			(source) => source.functions.some((fn) => fn.name === "checkout"),
		);
		expect(finalSource).toBeDefined();
		const functionNames = finalSource.functions.map((fn) => fn.name);
		expect(functionNames).toContain("checkout");
		expect(functionNames).toContain("applyDiscount");
		const functionIdentities = finalSource.functions.map(
			(fn) =>
				`${fn.name}:${fn.blockCoverage}:${fn.ranges[0].startOffset}:${fn.ranges[0].endOffset}`,
		);
		expect(new Set(functionIdentities).size).toBe(functionIdentities.length);
		expect(
			finalSource.functions
				.find((fn) => fn.name === "checkout")
				.ranges.some((range) => range.count > 0),
		).toBe(true);
		expect(
			finalSource.functions
				.find((fn) => fn.name === "applyDiscount")
				.ranges.some((range) => range.count > 0),
		).toBe(true);
		await runCli(
			"We stop precise coverage and freeze the background-excluded capture.",
			["coverage", "stop", "--exclude", "background"],
			environment,
		);
		const coverage = await runCli(
			"We render the stored source-mapped coverage capture.",
			["coverage", "show", "."],
			environment,
		);
		expect(coverage).toContain("app.ts");
		expect(coverage).toContain("CheckoutService");

		const disconnected = await runCli(
			"After proving the calculation completed, we disconnect cleanly. Runtime facts should disappear while durable intent remains.",
			[
				"connection",
				"disconnect",
				"--context",
				"typescript-e2e",
				"--connection",
				"browser",
			],
			environment,
		);
		expect(disconnected).toContain("browser  [disconnected; generation 1]");
		const reconnected = await runCli(
			"We reconnect to exercise generation safety. The provider launches a fresh browser, so both the connection generation and page target identity change.",
			[
				"connection",
				"connect",
				"--context",
				"typescript-e2e",
				"--connection",
				"browser",
			],
			environment,
		);
		expect(reconnected).toContain("generation 2]");
		const reconnectedPageTarget = await findPageTargetId(
			cli,
			"typescript-e2e",
			"browser",
			fixtureServer.origin,
			environment,
		);
		const reattached = await runCli(
			"The fresh page was auto-attached. We select its new exact identity and verify that durable breakpoint intent was reinstalled.",
			[
				"target",
				"wait",
				"breakpoint-installed",
				"checkout",
				"--context",
				"typescript-e2e",
				"--connection",
				"browser",
				"--target",
				reconnectedPageTarget,
			],
			environment,
		);
		expect(reattached).toContain("gen 2");
		expect(reattached).toContain(
			"checkout  ../src/app.ts:8:3  [installed; 1 binding]",
		);
		await runCli(
			"The reconnect check is complete; we close the live CDP connection before stopping the service.",
			[
				"connection",
				"disconnect",
				"--context",
				"typescript-e2e",
				"--connection",
				"browser",
			],
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
		await rm(resolve("artifacts/typescript-cli-chrome-profile"), {
			recursive: true,
			force: true,
			maxRetries: 5,
			retryDelay: 100,
		});
		await rm(join(fixtureDirectory, "dist"), { recursive: true, force: true });
	}
});

async function runCli(explanation, arguments_, environment) {
	const result = await runCliResult(explanation, arguments_, environment);
	expect(result.code, `${arguments_.join(" ")}\n${result.output}`).toBe(0);
	return result.output;
}

async function runJsonSilent(arguments_, environment) {
	const result = await run(cli, ["--json", ...arguments_], environment);
	expect(result.code, `${arguments_.join(" ")}\n${result.output}`).toBe(0);
	return JSON.parse(result.output);
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
						'<!doctype html><button id="run">Run fixture</button><script src="/dist/app.js"></script>',
					);
					break;
				case "/dist/app.js":
					response.setHeader("content-type", "text/javascript; charset=utf-8");
					response.end(await readFile(join(fixtureDirectory, "dist/app.js")));
					break;
				case "/dist/app.js.map":
					response.setHeader("content-type", "application/json; charset=utf-8");
					response.end(
						await sourceMapWithAmbiguousCandidate(
							join(fixtureDirectory, "dist/app.js.map"),
						),
					);
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

async function sourceMapWithAmbiguousCandidate(path) {
	const sourceMap = JSON.parse(await readFile(path, "utf8"));
	sourceMap.sources.push("../candidate/app.ts");
	sourceMap.sourcesContent.push(
		"export function unrelatedCandidate(): string { return 'candidate'; }\n",
	);
	return JSON.stringify(sourceMap);
}
