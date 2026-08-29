import {
	appendFile,
	mkdir,
	mkdtemp,
	readFile,
	rm,
	writeFile,
} from "node:fs/promises";
import { tmpdir } from "node:os";
import { basename, join, resolve } from "node:path";
import { build } from "esbuild";
import { expect, test } from "@playwright/test";
import { run } from "./live-test-harness.mjs";

const executableSuffix = process.platform === "win32" ? ".exe" : "";
const cli = resolve(`target/debug/jsdbg${executableSuffix}`);
const service = resolve(`target/debug/jsdbg-service${executableSuffix}`);
const transcriptPath = resolve("artifacts/minified-node-cli-transcript.md");
let stepNumber = 0;

test("CLI debugs a bundled minified Node.js source without a source map", async () => {
	test.setTimeout(300_000);
	const buildBins = await run("cargo", ["build", "--bins"], {});
	expect(buildBins.code, buildBins.output).toBe(0);

	const fixtureDirectory = await mkdtemp(join(tmpdir(), "jsdbg-minified-node-"));
	const stateDirectory = resolve(
		"artifacts",
		`.jsdbg-minified-node-e2e-${process.pid}`,
	);
	await mkdir(stateDirectory, { recursive: true });
	await mkdir(resolve("artifacts"), { recursive: true });
	const stateFile = join(stateDirectory, "service.json");
	const entryPath = join(fixtureDirectory, "invoice.js");
	const bundlePath = join(fixtureDirectory, "invoice.bundle.min.cjs");
	const environment = {
		JSDBG_SERVICE_EXE: service,
		JSDBG_SERVICE_STATE: stateFile,
	};
	let serviceStarted = false;

	await writeFile(
		entryPath,
		`function calculateInvoice(order) {
  const subtotal = order.items.reduce((sum, item) => sum + item.price * item.quantity, 0);
  const tax = Math.round(subtotal * 0.2);
  if (!order.customerId) {
    throw new Error("Missing customer id");
  }
  return { subtotal, tax, total: subtotal + tax };
}

globalThis.__debugOrder = {
  customerId: "",
  items: [{ price: 25, quantity: 2 }, { price: 10, quantity: 1 }]
};
globalThis.__runInvoice = () => calculateInvoice(globalThis.__debugOrder);
setInterval(() => {}, 1000);
`,
	);
	await build({
		entryPoints: [entryPath],
		bundle: true,
		minify: true,
		platform: "node",
		format: "cjs",
		sourcemap: false,
		outfile: bundlePath,
	});
	const bundledSource = await readFile(bundlePath, "utf8");
	expect(bundledSource.split(/\r?\n/)).toHaveLength(2);
	expect(bundledSource).not.toContain("sourceMappingURL");

	await writeFile(
		transcriptPath,
		"# Debugging a minified Node.js bundle with `jsdbg`\n\n" +
			"This scenario bundles a small application with esbuild using full minification and no source map, starts Node.js under the Inspector, and debugs the derived formatted projection while the runtime executes the original one-line bundle.\n\n" +
			"```text\n" +
			"bundle: true\n" +
			"minify: true\n" +
			"sourcemap: false\n" +
			"```\n",
	);

	try {
		await runCli(
			"Create and select a durable context for the Node.js investigation.",
			["context", "create", "--context", "minified-node"],
			environment,
		);
		serviceStarted = true;
		await runCli(
			"Select the context so the remaining commands need no repeated context flag.",
			["set", "context", "--context", "minified-node"],
			environment,
		);
		await runCli(
			"Launch the bundle as a Node.js connection. jsdbg starts it under the Inspector and attaches before releasing startup.",
			[
				"connection",
				"add",
				"--node",
				bundlePath,
				"--connection",
				"runtime",
				"--cwd",
				fixtureDirectory,
				"--runtime-executable",
				process.execPath,
				"--connect",
			],
			environment,
		);
		await runCli(
			"Claim the launch-owned direct debugger session before Node.js leaves its startup wait.",
			["target", "attach", "--connection", "runtime", "--force"],
			environment,
		);

		await runCli(
			"With one unambiguous Node target, enable deterministic automatic formatting for minified sources in this context.",
			["source", "formatting", "set", "auto"],
			environment,
		);
		await runCli(
			"Place a temporary runtime breakpoint to hydrate the lazily loaded bundle source.",
			["breakpoint", "set", "hydrate-bundle", bundlePath, "1"],
			environment,
		);
		await runCli(
			"Release the launch wait. The main bundle is now parsed with breakpoint intent already present.",
			["target", "release"],
			environment,
		);
		await runCli(
			"Wait until source hydration and physical breakpoint installation have completed.",
			[
				"target",
				"wait",
				"breakpoint-installed",
				"hydrate-bundle",
				"30000",
			],
			environment,
		);
		await runCli(
			"Remove the temporary breakpoint now that the source graph contains the bundle.",
			["breakpoint", "delete", "hydrate-bundle"],
			environment,
		);

		await runCli(
			"List the loaded bundle source. There is no authored source-map entry to fall back to.",
			["source", "list", "--path", basename(bundlePath)],
			environment,
		);
		const sources = await runJsonSilent(
			["source", "list", "--path", basename(bundlePath)],
			environment,
		);
		const runtimeSource = sources.find(
			(source) =>
				!source.path.endsWith("?formatted") &&
				source.path.endsWith(basename(bundlePath)),
		);
		expect(runtimeSource, JSON.stringify(sources, null, 2)).toBeDefined();
		const sourceUrl = runtimeSource.path;

		const original = await runCli(
			"Inspect the exact runtime text: the entire application is a one-line minified bundle.",
			["source", "show", sourceUrl, "--view", "original"],
			environment,
		);
		expect(original).toContain("Missing customer id");
		expect(original.trim().split(/\r?\n/)).toHaveLength(1);

		const formatted = await runCli(
			"Ask for the formatted projection. OXC parses the bundle and emits readable source without changing runtime bytes.",
			["source", "show", sourceUrl, "--view", "formatted"],
			environment,
		);
		expect(formatted).toContain("Missing customer id");
		expect(formatted.trim().split(/\r?\n/).length).toBeGreaterThan(5);
		const breakpointLine =
			formatted
				.split(/\r?\n/)
				.findIndex((line) => line.includes("Missing customer id")) + 1;
		expect(breakpointLine).toBeGreaterThan(0);
		const formattedUrl = `${sourceUrl}?formatted`;

		const breakpoint = await runCli(
			"Set a breakpoint on the readable formatted line. jsdbg maps it back to a physical offset in the one-line runtime bundle.",
			[
				"breakpoint",
				"set",
				"missing-customer",
				formattedUrl,
				String(breakpointLine),
			],
			environment,
		);
		expect(breakpoint).toContain("missing-customer");
		expect(breakpoint).toContain("installed");
		expect(breakpoint).toContain("via format");

		await runCli(
			"Schedule the failing application operation after the breakpoint has been installed.",
			["target", "eval", "setTimeout(globalThis.__runInvoice, 250)"],
			environment,
		);
		const paused = await runCli(
			"Wait for the formatted breakpoint and render the mapped stack and source excerpt.",
			["target", "wait", "paused", "1", "30000"],
			environment,
		);
		expect(paused).toContain("Frames:");
		expect(paused).toContain("Missing customer id");
		expect(paused).toContain("?formatted");

		const evaluated = await runCli(
			"Evaluate application state while paused, using the same target and frame selection.",
			["target", "eval", "globalThis.__debugOrder"],
			environment,
		);
		expect(evaluated).toContain("customerId");
		expect(evaluated).toContain("items");

		await runCli(
			"Disconnect after the investigation; the debuggee can now terminate independently.",
			["connection", "disconnect", "--connection", "runtime"],
			environment,
		);
		await runCli(
			"Stop the shared service.",
			["service", "stop"],
			environment,
		);
		serviceStarted = false;
		await emitTranscript(`\n---\n\n_Transcript saved to \`${transcriptPath}\`._\n`);
	} finally {
		if (serviceStarted) {
			await run(cli, ["service", "stop"], environment);
		}
		await rm(stateDirectory, { recursive: true, force: true });
		await rm(fixtureDirectory, { recursive: true, force: true });
	}
});

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

async function runJsonSilent(arguments_, environment) {
	const result = await run(cli, ["--json", ...arguments_], environment);
	expect(result.code, `${arguments_.join(" ")}\n${result.output}`).toBe(0);
	return JSON.parse(result.output);
}

async function emitTranscript(text) {
	process.stdout.write(text);
	await appendFile(transcriptPath, text);
}

function formatCommand(command, arguments_) {
	return [command, ...arguments_].map(quoteArgument).join(" ");
}

function quoteArgument(argument) {
	return /^[A-Za-z0-9_./:@%+=,?~-]+$/.test(argument)
		? argument
		: JSON.stringify(argument);
}
