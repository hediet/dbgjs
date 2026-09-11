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
import {
	findChromeExecutable,
	run,
} from "./live-test-harness.mjs";

const executableSuffix = process.platform === "win32" ? ".exe" : "";
const cli = resolve(`target/release/dbgjs${executableSuffix}`);
const service = resolve(`target/release/dbgjs-service${executableSuffix}`);
const transcriptPath = resolve("artifacts/vscode-editor-profile.md");
const exportedProfilePath = resolve("artifacts/vscode-editor.cpuprofile");
const commandTimeoutMs = 180_000;
let stepNumber = 0;

test("profiles vscode.dev editor creation and text insertion", async () => {
	test.setTimeout(240_000);
	const startedAt = Date.now();
	const build = await run(
		"cargo",
		["build", "--release", "--bins"],
		{},
		{ timeoutMs: commandTimeoutMs },
	);
	expect(build.code, build.output).toBe(0);
	const chromeExecutable = await findChromeExecutable();
	await mkdir(resolve("artifacts"), { recursive: true });
	await writeFile(
		transcriptPath,
		"# Where does vscode.dev spend CPU time while creating and populating an editor?\n\n" +
			"This scenario records a real V8 sampling CPU profile through dbgjs, projects generated frames through source maps, renders bounded hotspots, and exports the original DevTools-compatible profile.\n",
	);

	const stateDirectory = await mkdtemp(join(tmpdir(), "dbgjs-vscode-profile-"));
	const stateFile = join(stateDirectory, "service.json");
	const sourceMapCache = join(tmpdir(), "dbgjs-vscode-source-map-cache");
	await mkdir(sourceMapCache, { recursive: true });
	const environment = {
		DBGJS_SERVICE_EXE: service,
		DBGJS_SERVICE_STATE: stateFile,
		DBGJS_SOURCE_MAP_CACHE: sourceMapCache,
	};

	try {
		await runCli(
			"Create the isolated performance investigation workspace.",
			["context", "create", "--context", "vscode-profile", "--set"],
			environment,
		);
		await runCli(
			"Launch installed Chrome and attach to vscode.dev.",
			[
				"connection",
				"add",
				"--chrome",
				"https://vscode.dev/",
				"--context",
				"vscode-profile",
				"--connection",
				"browser",
				"--executable",
				chromeExecutable,
				"--connect",
				"--set",
			],
			environment,
		);
		await retryCli(
			"Wait for the workbench and focus it.",
			["target", "click", ".monaco-workbench"],
			environment,
			60_000,
		);
		await runCli(
			"Start target-scoped CPU sampling at a one millisecond interval.",
			["profile", "start", "--sampling-interval", "1ms"],
			environment,
		);
		await createUntitledEditor(
			"Create an editor through VS Code's Ctrl+K N command chord while profiling.",
			environment,
		);
		await retryCli(
			"Focus the new editor.",
			["target", "click", ".monaco-editor"],
			environment,
			30_000,
		);
		await runCli(
			"Insert a substantial JavaScript document to exercise the editor model and view.",
			["target", "type", editorWorkload()],
			environment,
		);
		const stopped = await runCli(
			"Stop sampling and preserve the immutable profile.",
			["profile", "stop", "--id", "editor-input"],
			environment,
		);
		expect(stopped).toContain("Captured editor-input");

		const hotspots = await runCli(
			"Render the hottest source-mapped functions by self time.",
			[
				"profile",
				"show",
				"editor-input",
				"--view",
				"functions",
				"--sort",
				"self",
				"--max-lines",
				"60",
			],
			environment,
		);
		expect(hotspots).toContain("CPU profile editor-input");
		expect(hotspots).toContain("Self");
		expect(hotspots).toContain("→ callback");
		expect(hotspots.trimEnd().split("\n").length).toBeLessThanOrEqual(60);

		const profile = await runJson(
			"Read the same immutable profile as structured data.",
			["profile", "show", "editor-input"],
			environment,
		);
		expect(profile.samplingIntervalMicros).toBe(1_000);
		expect(profile.samples.length).toBeGreaterThan(0);
		expect(profile.samples.length).toBe(profile.timeDeltasMicros.length);
		expect(profile.functions.length).toBeGreaterThan(0);
		expect(
			profile.functions.some((fn) => fn.authoredLocation?.sourceUrl),
		).toBe(true);

		await runCli(
			"Export the raw sample stream in DevTools CPU-profile format.",
			[
				"profile",
				"export",
				"editor-input",
				"--output",
				exportedProfilePath,
			],
			environment,
		);
		const exported = JSON.parse(await readFile(exportedProfilePath, "utf8"));
		expect(exported.nodes.length).toBe(profile.nodes.length);
		expect(exported.samples).toEqual(profile.samples);
		expect(exported.timeDeltas).toEqual(profile.timeDeltasMicros);

		await runCli(
			"Disconnect and terminate the launched browser.",
			[
				"connection",
				"disconnect",
				"--context",
				"vscode-profile",
				"--connection",
				"browser",
			],
			environment,
		);
		await runCli("Stop the debugger service.", ["service", "stop"], environment);
		await emitTranscript(
			`\n---\n\n_Total duration: ${((Date.now() - startedAt) / 1000).toFixed(1)}s._\n`,
		);
	} finally {
		const stopped = await run(cli, ["service", "stop"], environment, {
			timeoutMs: 10_000,
		});
		if (stopped.code !== 0) {
			await terminatePersistedService(stateFile);
		}
		await rm(stateDirectory, { recursive: true, force: true });
	}
});

function editorWorkload() {
	return Array.from(
		{ length: 500 },
		(_, index) =>
			`export function value${index}(input) { return input * ${index + 1}; }`,
	).join("\n");
}

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
	const startedAt = Date.now();
	const result = await run(cli, arguments_, environment, {
		timeoutMs: commandTimeoutMs,
	});
	expect(result.code, `${arguments_.join(" ")}\n${result.output}`).toBe(0);
	return recordCompleted(
		`${explanation} (${((Date.now() - startedAt) / 1000).toFixed(1)}s)`,
		arguments_,
		result,
	);
}

async function createUntitledEditor(explanation, environment) {
	await runCli(
		explanation,
		["target", "key", "ctrl+k,n"],
		environment,
	);
}

async function runJson(explanation, arguments_, environment) {
	const result = await run(cli, ["--json", ...arguments_], environment, {
		timeoutMs: commandTimeoutMs,
	});
	expect(result.code, `${arguments_.join(" ")}\n${result.output}`).toBe(0);
	const parsed = JSON.parse(result.output);
	await recordCompleted(explanation, ["--json", ...arguments_], {
		output: JSON.stringify(
			{
				captureId: parsed.captureId,
				samplingIntervalMicros: parsed.samplingIntervalMicros,
				nodeCount: parsed.nodes.length,
				sampleCount: parsed.samples.length,
				functionCount: parsed.functions.length,
				analysis: parsed.analysis,
			},
			null,
			2,
		),
	});
	return parsed;
}

async function recordCompleted(explanation, arguments_, result) {
	stepNumber += 1;
	const command = ["dbgjs", ...arguments_].map(quoteArgument).join(" ");
	const output = result.output.endsWith("\n")
		? result.output
		: `${result.output}\n`;
	const text = `\n## Step ${stepNumber} - ${explanation}\n\n\`\`\`console\n$ ${command}\n\`\`\`\n\n\`\`\`text\n${output}\`\`\`\n`;
	await emitTranscript(text);
	return result.output;
}

async function emitTranscript(text) {
	process.stdout.write(text);
	await appendFile(transcriptPath, text);
}

function quoteArgument(argument) {
	if (argument.length > 160) {
		return `"<${argument.length} characters>"`;
	}
	return /^[A-Za-z0-9_./:@%+=,#|-]+$/.test(argument)
		? argument
		: JSON.stringify(argument);
}

async function terminatePersistedService(stateFile) {
	let processId;
	try {
		processId = JSON.parse(await readFile(stateFile, "utf8")).processId;
	} catch {
		return;
	}
	if (!Number.isInteger(processId)) return;
	if (process.platform === "win32") {
		await run(
			"taskkill",
			["/PID", String(processId), "/T", "/F"],
			{},
			{ timeoutMs: 10_000 },
		);
		return;
	}
	try {
		process.kill(processId, "SIGKILL");
	} catch {
		// The process may already have exited.
	}
}
