import { randomUUID } from "node:crypto";
import { createServer } from "node:http";
import { appendFile, mkdir, rm, writeFile } from "node:fs/promises";
import { join, resolve } from "node:path";
import { chromium, expect, test } from "@playwright/test";
import {
	allocatePort,
	readCdpEndpoint,
	run,
} from "./live-test-harness.mjs";

const executableSuffix = process.platform === "win32" ? ".exe" : "";
const cli = resolve(`target/debug/jsdbg${executableSuffix}`);
const service = resolve(`target/debug/jsdbg-service${executableSuffix}`);
const transcriptPath = resolve("artifacts/context-relay-cli-transcript.md");
let stepNumber = 0;

test("one jsdbg context relays two browser connections with two targets each", async () => {
	test.setTimeout(300_000);
	const build = await run("cargo", ["build", "--bins"], {});
	expect(build.code, build.output).toBe(0);

	const root = resolve(".test-tmp", `context-relay-${randomUUID()}`);
	await mkdir(root, { recursive: true });
	await mkdir(resolve("artifacts"), { recursive: true });
	const sourceState = join(root, "source-service.json");
	const consumerState = join(root, "consumer-service.json");
	const sourceEnvironment = serviceEnvironment(sourceState);
	const consumerEnvironment = serviceEnvironment(consumerState);
	const fixture = await startFixture();
	const browsers = [];
	let sourceStarted = false;
	let consumerStarted = false;

	await writeFile(
		transcriptPath,
		"# Chaining two `jsdbg` contexts over CDP stdio\n\n" +
			"This live E2E scenario starts two independent Chromium connections with two pages each. A source `jsdbg` context owns both connections. A second `jsdbg` imports the entire source context through `jsdbg context relay --stdio`, discovers all four targets and their sources, installs target-scoped breakpoints, and observes each breakpoint being hit.\n",
	);

	try {
		for (const name of ["alpha", "beta"]) {
			const launched = await launchBrowser(root, name, fixture.origin);
			browsers.push(launched);
		}

		await runCli(
			"Create the source context that owns both real browser connections.",
			["context", "create", "--context", ":source", "Source browsers", "--set"],
			sourceEnvironment,
		);
		sourceStarted = true;
		for (const browser of browsers) {
			await runCli(
				`Connect the source context to browser ${browser.name}.`,
				[
					"connection",
					"add",
					browser.endpoint,
					"--connection",
					browser.name,
					"--connect",
				],
				sourceEnvironment,
			);
		}
		const sourceTargets = await waitForPageTargets(sourceEnvironment, 4);
		expect(sourceTargets).toHaveLength(4);
		await runCli(
			"Confirm that the source context sees two page targets from each connection.",
			["target", "list", "--type", "page"],
			sourceEnvironment,
		);

		await runCli(
			"Create a separate consumer context. It has no direct browser connection.",
			["context", "create", "--context", ":consumer", "Relay consumer", "--set"],
			consumerEnvironment,
		);
		consumerStarted = true;
		await runCli(
			"Import the complete source context as one browser-style CDP connection. The nested relay speaks MCP-style newline-delimited JSON over stdio.",
			[
				"connection",
				"add",
				"--stdio",
				"--connection",
				"upstream",
				"--topology",
				"browser",
				"--env",
				`JSDBG_SERVICE_STATE=${sourceState}`,
				"--env",
				`JSDBG_SERVICE_EXE=${service}`,
				"--connect",
				"--",
				cli,
				"context",
				"relay",
				"--stdio",
				"--context",
				":source",
			],
			consumerEnvironment,
		);

		const blocked = await runCliFailure(
			"While relaying, the source instance gives up local debugger control for the context.",
			[
				"target",
				"attach",
				"--connection",
				sourceTargets[0].connectionId,
				"--target",
				sourceTargets[0].targetId,
			],
			sourceEnvironment,
		);
		expect(blocked).toContain("exclusively owned by an active relay");

		const importedTargets = await waitForPageTargets(consumerEnvironment, 4);
		expect(importedTargets).toHaveLength(4);
		await runCli(
			"The consumer discovers all four pages from both upstream connections through one virtual Target domain.",
			["target", "list", "--type", "page"],
			consumerEnvironment,
		);

		const cases = importedTargets
			.map((entry) => {
				const match = /Relay (alpha|beta) ([12])/.exec(entry.title);
				expect(match, entry.title).not.toBeNull();
				const label = `${match[1]}-${match[2]}`;
				return {
					entry,
					label,
					sourceUrl: `${fixture.origin}/script-${label}.js`,
					marker: `BREAK_${label.toUpperCase().replace("-", "_")}`,
				};
			})
			.sort((left, right) => left.label.localeCompare(right.label));

		const sourceInventory = await runCli(
			"Explore the relayed source inventory before choosing breakpoint locations.",
			["source", "list", "--path", "script-"],
			consumerEnvironment,
		);
		expect(sourceInventory).not.toContain("runtime:unresolved");
		expect(sourceInventory.match(/\[runtime:loaded\]/g)).toHaveLength(4);

		for (const item of cases) {
			item.breakpointLine = 2;
			const breakpointOutput = await runCli(
				`Install a breakpoint on the marker line of only the ${item.label} target.`,
				[
					"breakpoint",
					"configure",
					`break-${item.label}`,
					item.sourceUrl,
					String(item.breakpointLine),
					"1",
					"--target",
					item.entry.targetId,
				],
				consumerEnvironment,
			);
			expect(breakpointOutput).toContain(`Source: ${item.sourceUrl}`);
			expect(breakpointOutput).toContain(item.marker);
			expect(breakpointOutput).toContain("CDP confirmed");
			expect(breakpointOutput).toContain("via identity");
			expect(breakpointOutput).not.toContain("?formatted");
		}

		for (const item of cases) {
			const scope = [
				"--connection",
				item.entry.connectionId,
				"--target",
				item.entry.targetId,
			];
			await runCli(
				`Invoke ${item.label}'s function asynchronously so its breakpoint can pause through the relay.`,
				["target", "eval", "setTimeout(globalThis.runRelayedJob, 100)", ...scope],
				consumerEnvironment,
			);
			const paused = await runCli(
				`Observe the ${item.label} breakpoint hit, including its relayed source frame.`,
				[
					"target",
					"wait",
					"paused",
					"0",
					"30000",
					...scope,
				],
				consumerEnvironment,
			);
			expect(paused).toContain(item.marker);
			expect(paused).toContain(item.sourceUrl);
			await runCli(
				`Resume ${item.label} before triggering the next target.`,
				["target", "resume", ...scope],
				consumerEnvironment,
			);
		}

		await runCli(
			"Disconnect the imported connection. Closing its stdio ends the source context relay.",
			["connection", "disconnect", "--connection", "upstream"],
			consumerEnvironment,
		);
		await expect
			.poll(async () => {
				const result = await run(cli, [
					"--json",
					"target",
					"show",
					"--context",
					":source",
					"--connection",
					sourceTargets[0].connectionId,
					"--target",
					sourceTargets[0].targetId,
				], sourceEnvironment);
				return result.code;
			}, { timeout: 30_000 })
			.toBe(0);
		await runCli(
			"After the relay ends, the source jsdbg instance can debug its context again.",
			[
				"target",
				"show",
				"--connection",
				sourceTargets[0].connectionId,
				"--target",
				sourceTargets[0].targetId,
			],
			sourceEnvironment,
		);

		await runCli("Stop the consumer service.", ["service", "stop"], consumerEnvironment);
		consumerStarted = false;
		await runCli("Stop the source service.", ["service", "stop"], sourceEnvironment);
		sourceStarted = false;
		await emitTranscript(`\n---\n\n_Transcript saved to \`${transcriptPath}\`._\n`);
	} finally {
		if (consumerStarted) {
			await run(cli, ["service", "stop"], consumerEnvironment);
		}
		if (sourceStarted) {
			await run(cli, ["service", "stop"], sourceEnvironment);
		}
		for (const browser of browsers) {
			await browser.close();
		}
		await fixture.close();
		await rm(root, { recursive: true, force: true });
	}
});

function serviceEnvironment(stateFile) {
	return {
		JSDBG_SERVICE_EXE: service,
		JSDBG_SERVICE_STATE: stateFile,
	};
}

async function launchBrowser(_root, name, origin) {
	const port = await allocatePort();
	const channel = process.env.PLAYWRIGHT_CHANNEL ?? "bundled";
	const browserServer = await chromium.launchServer({
		...(channel === "bundled" ? {} : { channel }),
		headless: true,
		args: [
			`--remote-debugging-port=${port}`,
			"--remote-allow-origins=*",
			"--no-first-run",
			"--no-default-browser-check",
		],
	});
	const endpoint = await readCdpEndpoint(port);
	const originUrl = new URL(endpoint);
	const debugOrigin = `http://${originUrl.host}`;
	const initialTargets = await (await fetch(`${debugOrigin}/json/list`)).json();
	await Promise.all(
		initialTargets
			.filter((target) => target.type === "page")
			.map((target) => fetch(`${debugOrigin}/json/close/${target.id}`)),
	);
	for (const index of [1, 2]) {
		const url = `${origin}/page-${name}-${index}`;
		const response = await fetch(`${debugOrigin}/json/new?${encodeURIComponent(url)}`, {
			method: "PUT",
		});
		expect(response.ok, await response.text()).toBe(true);
	}
	await expect.poll(async () => {
		const targets = await (await fetch(`${debugOrigin}/json/list`)).json();
		return targets
			.filter((target) => target.type === "page")
			.map((target) => target.title)
			.sort();
	}).toEqual([`Relay ${name} 1`, `Relay ${name} 2`]);
	return {
		name,
		endpoint,
		close: () => browserServer.close(),
	};
}

async function startFixture() {
	const server = createServer((request, response) => {
		const page = /^\/page-(alpha|beta)-([12])$/.exec(request.url ?? "");
		if (page) {
			const label = `${page[1]}-${page[2]}`;
			response.writeHead(200, { "content-type": "text/html" });
			response.end(
				`<!doctype html><title>Relay ${page[1]} ${page[2]}</title>` +
					`<script src="/script-${label}.js"></script><p>${label}</p>`,
			);
			return;
		}
		const script = /^\/script-(alpha|beta)-([12])\.js$/.exec(request.url ?? "");
		if (script) {
			const label = `${script[1]}-${script[2]}`;
			const marker = `BREAK_${label.toUpperCase().replace("-", "_")}`;
			response.writeHead(200, { "content-type": "text/javascript" });
			response.end(
				`globalThis.runRelayedJob = function runRelayedJob() {\n` +
					`  const marker = "${marker}";\n` +
					`  globalThis.lastRelayedMarker = marker;\n` +
					`  return marker;\n` +
					`};\n`,
			);
			return;
		}
		response.writeHead(404);
		response.end("not found");
	});
	await new Promise((resolvePromise, reject) => {
		server.once("error", reject);
		server.listen(0, "127.0.0.1", resolvePromise);
	});
	const address = server.address();
	if (!address || typeof address === "string") {
		throw new Error("fixture did not bind a TCP port");
	}
	return {
		origin: `http://127.0.0.1:${address.port}`,
		close: () => new Promise((resolvePromise) => server.close(resolvePromise)),
	};
}

async function waitForPageTargets(environment, count) {
	let targets = [];
	await expect
		.poll(async () => {
			const result = await run(cli, ["--json", "target", "list", "--type", "page"], environment);
			if (result.code !== 0) {
				return -1;
			}
			targets = JSON.parse(result.output).targets;
			return targets.length;
		})
		.toBe(count);
	return targets;
}

async function runCli(explanation, arguments_, environment) {
	stepNumber += 1;
	await emitTranscript(
		`\n## Step ${stepNumber} — ${explanation}\n\n\`\`\`console\n$ ${formatCommand("jsdbg", arguments_)}\n\`\`\`\n\n`,
	);
	const result = await run(cli, arguments_, environment);
	const output = result.output.endsWith("\n") ? result.output : `${result.output}\n`;
	await emitTranscript(`\`\`\`text\n${output}\`\`\`\n`);
	expect(result.code, `${arguments_.join(" ")}\n${result.output}`).toBe(0);
	return result.output;
}

async function runCliFailure(explanation, arguments_, environment) {
	stepNumber += 1;
	await emitTranscript(
		`\n## Step ${stepNumber} — ${explanation}\n\n\`\`\`console\n$ ${formatCommand("jsdbg", arguments_)}\n\`\`\`\n\n`,
	);
	const result = await run(cli, arguments_, environment);
	const output = result.output.endsWith("\n") ? result.output : `${result.output}\n`;
	await emitTranscript(`\`\`\`text\n${output}\`\`\`\n`);
	expect(result.code, result.output).not.toBe(0);
	return result.output;
}

async function emitTranscript(content) {
	await appendFile(transcriptPath, content);
	process.stdout.write(content);
}

function formatCommand(command, arguments_) {
	return [command, ...arguments_].map(shellQuote).join(" ");
}

function shellQuote(value) {
	return /^[A-Za-z0-9_./:=?&-]+$/.test(value)
		? value
		: `'${value.replaceAll("'", `'\\''`)}'`;
}
