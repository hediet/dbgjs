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

const executableSuffix = process.platform === "win32" ? ".exe" : "";
const cli = resolve(`target/release/dbgjs${executableSuffix}`);
const service = resolve(`target/release/dbgjs-service${executableSuffix}`);
const transcriptPath = resolve("artifacts/vscode-extension-host-coverage.md");
const commandTimeoutMs = 120_000;
let stepNumber = 0;

test("captures coverage from a running VS Code extension host", async () => {
	test.setTimeout(180_000);
	const startedAt = Date.now();
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
		"# What executes in a running VS Code extension host?\n\n" +
			"This golden scenario discovers an already-running VS Code forest, selects an attachable extension host, activates its Node inspector, captures precise coverage, and renders the immutable source-mapped result.\n",
	);

	const stateDirectory = await mkdtemp(join(tmpdir(), "dbgjs-extension-host-coverage-"));
	const stateFile = join(stateDirectory, "service.json");
	const sourceMapCache = join(tmpdir(), "dbgjs-vscode-source-map-cache");
	await mkdir(sourceMapCache, { recursive: true });
	const environment = {
		DBGJS_SERVICE_EXE: service,
		DBGJS_SERVICE_STATE: stateFile,
		DBGJS_SOURCE_MAP_CACHE: sourceMapCache,
	};

	try {
		const forests = await runJson(
			"Discover attachable targets without starting or mutating the debugger service.",
			["process", "list", "--vscode", "--no-cmd-line"],
			environment,
			summarizeProcessForests,
		);
		const extensionHosts = forests
			.flatMap((forest) => forest.processes)
			.filter(
				(process_) =>
					process_.role === "extension-host" && process_.attachable,
			);
		expect(extensionHosts.length).toBeGreaterThan(0);
		const extensionHost =
			extensionHosts.find((process_) =>
				process_.windowTitle?.toLowerCase().includes("dbgjs"),
			) ?? extensionHosts[0];
		const processId = String(extensionHost.processId);
		const connectionId = `process-${processId}`;

		await runCli(
			"Create a durable context for the selected extension host.",
			["context", "create", "--context", "extension-host-coverage", "--set"],
			environment,
		);
		await runCli(
			"Activate the selected process inspector and attach its Node root target.",
			[
				"process",
				"attach",
				processId,
				"--context",
				"extension-host-coverage",
				"--set",
			],
			environment,
		);
		const identity = await runCli(
			"Evaluate runtime identity inside the live extension host.",
			[
				"target",
				"eval",
				"['pid=' + process.pid, 'node=' + process.versions.node, 'execPath=' + process.execPath, 'loadedModules=' + process.moduleLoadList.length].join('; ')",
			],
			environment,
		);
		expect(identity).toContain(`pid=${processId}`);

		await runCli(
			"Start precise function and block coverage.",
			["coverage", "start"],
			environment,
		);
		await runCli(
			"Exercise ordinary Node path logic inside the running extension host.",
			[
				"target",
				"eval",
				"process.getBuiltinModule('path').join('extension', 'host', 'coverage')",
			],
			environment,
		);
		await emitTranscript(
			"\n> Keep coverage active for one second so normal extension-host timers and message handling can run.\n\n",
		);
		await new Promise((resolve_) => setTimeout(resolve_, 1_000));
		await runCli(
			"Stop coverage and preserve the immutable capture.",
			["coverage", "stop"],
			environment,
		);
		const report = await runCli(
			"Render a bounded source-mapped coverage tree.",
			["coverage", "show", ".", "--max-lines", "80", "--no-cache"],
			environment,
		);
		expect(report).toMatch(/^\d+ RL \(run lines\), \d+ HL \(hit lines\)$/m);
		expect(report.trimEnd().split("\n").length).toBeLessThanOrEqual(80);

		const coverage = await runJson(
			"Read the same immutable coverage capture as structured data.",
			["coverage", "show", "."],
			environment,
			(value) => {
				const ranges = value.sources.flatMap((source) =>
					source.functions.flatMap((fn) => fn.ranges),
				);
				return {
					sourceCount: value.sources.length,
					functionCount: value.sources.reduce(
						(total, source) => total + source.functions.length,
						0,
					),
					hitRangeCount: ranges.filter((range) => range.count > 0).length,
					authoredRangeCount: ranges.filter(
						(range) => range.authoredStart != null,
					).length,
				};
			},
		);
		const ranges = coverage.sources.flatMap((source) =>
			source.functions.flatMap((fn) => fn.ranges),
		);
		expect(ranges.some((range) => range.count > 0)).toBe(true);

		await emitTranscript(
			`\n## Finding\n\nThe selected extension host was PID **${processId}** for **${extensionHost.windowTitle ?? "an unlabelled window"}**. The coverage capture contains **${coverage.sources.length}** scripts and **${ranges.filter((range) => range.count > 0).length}** hit ranges, proving that dbgjs attached to and observed a live extension-host runtime.\n`,
		);
		await runCli(
			"Disconnect without terminating the existing VS Code extension host.",
			[
				"connection",
				"disconnect",
				"extension-host-coverage",
				connectionId,
			],
			environment,
		);
		await runCli("Stop the debugger service.", ["service", "stop"], environment);
		await emitTranscript(
			`\n---\n\n_Total duration: ${((Date.now() - startedAt) / 1000).toFixed(1)}s._\n`,
		);
	} finally {
		await run(cli, ["service", "stop"], environment, { timeoutMs: 10_000 });
		await terminatePersistedService(stateFile);
		await rm(stateDirectory, { recursive: true, force: true });
	}
});

function extensionHostSummary(process_) {
	return {
		processId: process_.processId,
		attachable: process_.attachable,
		windowId: process_.windowId,
		windowTitle: process_.windowTitle,
	};
}

function summarizeProcessForests(forests) {
	const processes = forests.flatMap((forest) => forest.processes);
	const roleCounts = Object.fromEntries(
		Object.entries(
			processes.reduce((counts, process_) => {
				counts[process_.role] = (counts[process_.role] ?? 0) + 1;
				return counts;
			}, {}),
		).sort(([left], [right]) => left.localeCompare(right)),
	);
	return {
		forestCount: forests.length,
		processCount: processes.length,
		attachableTargetCount: processes.filter((process_) => process_.attachable).length,
		roleCounts,
		renderers: processes
			.filter((process_) => process_.role === "renderer")
			.map((process_) => ({
				processId: process_.processId,
				windowId: process_.windowId,
				windowTitle: process_.windowTitle,
			})),
		extensionHosts: processes
			.filter((process_) => process_.role === "extension-host")
			.map(extensionHostSummary),
		copilotRunners: processes
			.filter((process_) => process_.role === "copilot")
			.map((process_) => ({
				processId: process_.processId,
				sessions: process_.agentSessions.map((session) => ({
					internalId: session.internalId,
					title: session.title,
					disconnected: session.disconnected,
				})),
			})),
	};
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
		result.output,
	);
}

async function runJson(
	explanation,
	arguments_,
	environment,
	summarize = (value) => value,
) {
	const startedAt = Date.now();
	const result = await run(cli, ["--json", ...arguments_], environment, {
		timeoutMs: commandTimeoutMs,
	});
	expect(result.code, `${arguments_.join(" ")}\n${result.output}`).toBe(0);
	const parsed = JSON.parse(result.output);
	await recordCompleted(
		`${explanation} (${((Date.now() - startedAt) / 1000).toFixed(1)}s)`,
		["--json", ...arguments_],
		`Summary of the full JSON response:\n${JSON.stringify(summarize(parsed), null, 2)}\n`,
	);
	return parsed;
}

async function recordCompleted(explanation, arguments_, output) {
	stepNumber += 1;
	const command = ["dbgjs", ...arguments_].map(quoteArgument).join(" ");
	const text = `\n## Step ${stepNumber} - ${explanation}\n\n\`\`\`console\n$ ${command}\n\`\`\`\n\n\`\`\`text\n${output}\`\`\`\n`;
	await emitTranscript(text);
	return output;
}

async function emitTranscript(text) {
	process.stdout.write(text);
	await appendFile(transcriptPath, text);
}

function quoteArgument(argument) {
	return /^[A-Za-z0-9_./:@%+=,#|$-]+$/.test(argument)
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
	try {
		process.kill(processId, "SIGKILL");
	} catch {
		// The process may already have exited.
	}
}
