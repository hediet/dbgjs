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
	findPageTargetId,
	run,
} from "./live-test-harness.mjs";

const executableSuffix = process.platform === "win32" ? ".exe" : "";
const cli = resolve(`target/release/dbgjs${executableSuffix}`);
const service = resolve(`target/release/dbgjs-service${executableSuffix}`);
const transcriptPath = resolve("artifacts/vscode-typing-coverage.md");
const expectedDurationMs = 120_000;
const hardTimeoutMs = Math.ceil(expectedDurationMs * 1.2);
const commandTimeoutMs = 90_000;
let stepNumber = 0;

test("traces deferred auto-whitespace cleanup in vscode.dev", async () => {
	test.setTimeout(hardTimeoutMs);
	const startedAt = Date.now();
	const build = await run("cargo", ["build", "--release", "--bins"], {}, { timeoutMs: commandTimeoutMs });
	expect(build.code, build.output).toBe(0);
	const chromeExecutable = await findChromeExecutable();
	await mkdir(resolve("artifacts"), { recursive: true });
	await writeFile(
		transcriptPath,
		`# Why does vscode.dev remove auto-indented whitespace later?\n\nThis is a real dbgjs/HubRPC/Chrome/CDP coverage, breakpoint, and stepping investigation.\n\n- Expected duration: ${expectedDurationMs / 1000}s\n- Hard timeout (+20%): ${hardTimeoutMs / 1000}s\n`,
	);

	const stateDirectory = await mkdtemp(join(tmpdir(), "dbgjs-vscode-coverage-"));
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
			"Create and select a workspace for this investigation.",
			["context", "create", "--context", "vscode-typing", "--set"],
			environment,
		);
		await runCli(
			"Launch installed Chrome, open vscode.dev, connect CDP, and auto-attach the page.",
			[
				"connection",
				"add",
				"--chrome",
				"https://vscode.dev/",
				"--context",
				"vscode-typing",
				"--connection",
				"browser",
				"--executable",
				chromeExecutable,
				"--connect",
				"--set",
			],
			environment,
		);
		const pageTarget = await findPageTargetId(
			cli,
			"vscode-typing",
			"browser",
			"https://vscode.dev/",
			environment,
		);
		await retryCli(
			"Wait for the workbench DOM and focus it with a real CDP click.",
			["target", "click", ".monaco-workbench"],
			environment,
			60_000,
		);
		await createUntitledEditor(
			"Create an untitled editor through VS Code's command chord.",
			environment,
		);
		await retryCli(
			"Focus the Monaco editor through its DOM box.",
			["target", "click", ".monaco-editor"],
			environment,
			30_000,
		);
		await setTypeScriptMode(environment);
		await runCli(
			"Seed the editor with two spaces followed by `foo`.",
			["target", "type", "  foo"],
			environment,
		);
		await runCli(
			"Start precise function and block coverage.",
			["coverage", "start"],
			environment,
		);
		await emitTranscript(
			"\n> Let one short background interval elapse, then capture it so recurring startup/timer work can be excluded from the typing sample.\n\n",
		);
		await new Promise((resolve_) => setTimeout(resolve_, 5000));
		await runCli(
			"Capture the background baseline without stopping the recording.",
			["coverage", "capture", "--id", "background"],
			environment,
		);
		await runCli(
			"Press Enter as a real key event so VS Code inserts transient auto whitespace.",
			["target", "key", "enter"],
			environment,
		);
		const sampledAutoIndent = await readEditorLines(
			"Observe the auto-indented empty second line.",
			environment,
		);
		expect(sampledAutoIndent).toMatch(/"lines":\s*\[\s*"  foo",\s*"  "\s*\]/);
		await runCli(
			"Press ArrowUp to abandon the auto-indented line.",
			["target", "key", "arrowup"],
			environment,
		);
		await new Promise((resolve_) => setTimeout(resolve_, 250));
		const sampledAfterCleanup = await readEditorLines(
			"Observe that ArrowUp moved the cursor but did not mutate the buffer yet.",
			environment,
		);
		expect(sampledAfterCleanup).toMatch(/"lines":\s*\[\s*"  foo",\s*"  "\s*\]/);
		expect(sampledAfterCleanup).toContain("top: 0px");
		await new Promise((resolve_) => setTimeout(resolve_, 250));
		const stoppedCoverage = await runCli(
			"Stop coverage and freeze the background-excluded immutable capture.",
			["coverage", "stop", "--exclude", "background"],
			environment,
		);
		expect(stoppedCoverage).toContain("Captured .");
		const report = await runCli(
			"Analyze the stored capture and render its source-mapped symbol tree.",
			["coverage", "show", "."],
			environment,
			90_000,
		);
		expect(report).toMatch(/^\d+ RL \(run lines\), \d+ HL \(hit lines\)$/m);
		expect(report).toMatch(/\.tsx?\s+\d+ HL, \d+ RL/);
		expect(report).not.toContain("additional files omitted");
		expect(report).not.toContain("additional hit ranges omitted");
		expect(report).toMatch(/\[\d+ children pruned\]/);
		expect(report).not.toMatch(/\n[ │]+└─ … \[all \d+ children pruned/);
		expect(report.trimEnd().split("\n").length).toBeLessThanOrEqual(300);
		const bounded = await runTextSilent(
			["coverage", "show", ".", "--max-lines", "40"],
			environment,
		);
		expect(bounded.trimEnd().split("\n").length).toBeLessThanOrEqual(40);
		const delta = await runJsonSilent(
			["coverage", "show", "."],
			environment,
		);
		const authoredRanges = delta.sources
			.flatMap((source) => source.functions)
			.flatMap((fn) => fn.ranges)
			.filter((range) => range.authoredStart != null);
		expect(authoredRanges.length).toBeGreaterThan(0);
		expect(authoredRanges.some((range) => range.count === 1)).toBe(true);
		const candidate = selectIndentCleanupCandidate(delta);
		expect(candidate).toBeDefined();
		const sourcePath = candidate.range.authoredStart.sourceUrl;
		const pathPrefix = sourcePath
			.replace(/^(\.\.\/)+/, "")
			.split("/")
			.slice(0, -1)
			.join("/");
		const drilldown = await runCli(
			`Drill into the measured typing path \`${pathPrefix}\`.`,
			["coverage", "show", ".", "--path", pathPrefix],
			environment,
		);
		expect(drilldown).not.toContain("additional hit ranges omitted");
		expect(drilldown).toMatch(/TextModel|PieceTreeTextBuffer|Cursor/);
		const exhaustive = await runTextSilent(
			["coverage", "show", ".", "--path", pathPrefix, "--all"],
			environment,
		);
		expect(exhaustive).toContain(
			(candidate.function.breadcrumb ?? candidate.function.name)
				.split(".")
				.at(-1),
		);
		expect(exhaustive.trimEnd().split("\n").length).toBeGreaterThanOrEqual(
			drilldown.trimEnd().split("\n").length,
		);
		const noCache = await runTextSilent(
			["coverage", "show", ".", "--max-lines", "5", "--no-cache"],
			environment,
		);
		expect(noCache.trimEnd().split("\n").length).toBeLessThanOrEqual(5);
		const sourceGrep = await runCli(
			"Search the debugger's resolved authored sources for both ends of the deferred auto-whitespace handoff.",
			[
				"source",
				"grep",
				"newTrimAutoWhitespaceCandidates|trimAutoWhitespaceLineNumbers",
				"--regex",
				"--path",
				"src/vs/editor/common/model",
				"--max-results",
				"20",
				"--context-lines",
				"2",
			],
			environment,
		);
		expect(sourceGrep).toContain("pieceTreeTextBuffer.ts");
		expect(sourceGrep).toContain("textModel.ts");
		expect(sourceGrep).toContain("newTrimAutoWhitespaceCandidates");
		expect(sourceGrep).toContain("trimAutoWhitespaceLineNumbers");
		const sourceMatches = await runJsonSilent(
			[
				"source",
				"grep",
				"newTrimAutoWhitespaceCandidates|trimAutoWhitespaceLineNumbers",
				"--regex",
				"--path",
				"src/vs/editor/common/model",
				"--max-results",
				"20",
			],
			environment,
		);
		const pieceTreeMatch = sourceMatches.matches.find((match) =>
			match.path.endsWith("/pieceTreeTextBuffer.ts")
				&& match.text.includes("newTrimAutoWhitespaceCandidates.push"),
		);
		expect(pieceTreeMatch).toBeDefined();
		const sourceExcerpt = await runCli(
			"Show the exact PieceTree code that records future trim candidates.",
			[
				"source",
				"show",
				pieceTreeMatch.path,
				"--line",
				String(pieceTreeMatch.line),
				"--context-lines",
				"12",
			],
			environment,
		);
		expect(sourceExcerpt).toContain("isAutoWhitespaceEdit");
		expect(sourceExcerpt).toContain("newTrimAutoWhitespaceCandidates.push");
		const sourceGraph = await runCli(
			"Explain how the authored TextModel source is connected to the runtime workbench bundle.",
			["source", "explain", sourcePath],
			environment,
		);
		expect(sourceGraph).toContain("authored source");
		expect(sourceGraph).toContain("source map");
		expect(sourceGraph).toContain("workbench.web.main.internal.js");
		const reverseMapping = await runCli(
			"Map the authored cleanup location back to its generated runtime endpoint.",
			[
				"source",
				"map",
				sourcePath,
				String(candidate.range.authoredStart.line + 1),
				"3",
			],
			environment,
		);
		expect(reverseMapping).toContain("authored-to-generated");
		expect(reverseMapping).toContain("workbench.web.main.internal.js");
		await createUntitledEditor(
			"Create a fresh editor to repeat the exact interaction under a normal breakpoint.",
			environment,
		);
		await retryCli(
			"Focus the fresh editor.",
			["target", "click", ".monaco-editor"],
			environment,
			30_000,
		);
		await setTypeScriptMode(environment);
		await runCli(
			"Recreate the original line.",
			["target", "type", "  foo"],
			environment,
		);
		await runCli(
			"Press Enter and leave the cursor on the auto-indented empty line.",
			["target", "key", "enter"],
			environment,
		);
		const beforeBreakpoint = await readEditorLines(
			"Confirm the two transient spaces exist before cursor movement.",
			environment,
		);
		expect(beforeBreakpoint).toMatch(/"lines":\s*\[\s*"  foo",\s*"  "\s*\]/);
		const breakpoint = await runCli(
			`Install a normal authored breakpoint in \`${candidate.function.breadcrumb ?? candidate.function.name}\`.`,
			[
				"breakpoint",
				"set",
				"indent-cleanup",
				sourcePath,
				String(candidate.range.authoredStart.line + 1),
				"--column",
				"3",
			],
			environment,
		);
		expect(breakpoint).toContain("indent-cleanup");
		expect(breakpoint).toContain("Breakpoint indent-cleanup:");
		expect(breakpoint).toMatch(
			new RegExp(
				`>\\s+${candidate.range.authoredStart.line + 1}\\s+\\|`,
			),
		);
		const pausedCommand = runCli(
			"Wait for the normal breakpoint in the first edit transaction after ArrowUp.",
			[
				"target",
				"wait",
				"paused",
				"0",
				"30000",
				"--context",
				"vscode-typing",
				"--connection",
				"browser",
				"--target",
				pageTarget,
			],
			environment,
		);
		await emitTranscript(
			"\n> The pause wait is active. ArrowUp moves the cursor but does not edit the model. Typing on the destination line starts the next edit transaction, which consumes the saved auto-whitespace candidate.\n\n",
		);
		await runCli(
			"Dispatch ArrowUp and leave the auto-whitespace candidate pending.",
			["target", "key", "arrowup"],
			environment,
		);
		const editCommand = runCli(
			"Type one character to start the edit transaction that trims the abandoned blank line.",
			["target", "type", "x"],
			environment,
		);
		const paused = await pausedCommand;
		expect(paused).toContain("[paused at epoch 1]");
		expect(paused).toContain(
			candidate.function.breadcrumb ?? candidate.function.name,
		);
		const firstStep = await runCli(
			"Step over the first authored statement in the cleanup path.",
			["target", "step", "over"],
			environment,
		);
		expect(firstStep).toContain("[paused at epoch 2]");
		const secondStep = await runCli(
			"Step over once more to follow validation or edit construction.",
			["target", "step", "over"],
			environment,
		);
		expect(secondStep).toContain("[paused at epoch 3]");
		await runCli(
			"Resume so the cursor movement and whitespace deletion complete.",
			["target", "resume"],
			environment,
		);
		await editCommand;
		await new Promise((resolve_) => setTimeout(resolve_, 250));
		const afterBreakpoint = await readEditorLines(
			"Verify the next edit consumed the candidate and removed the transient spaces.",
			environment,
		);
		expect(afterBreakpoint).toMatch(/"lines":\s*\[[^\]]+,\s*""\s*\]/);
		await emitTranscript(
			"\n## What removes the spaces?\n\nEnter marks its indentation edit as auto whitespace. PieceTreeTextBuffer records the resulting blank line as a future trim candidate. ArrowUp only moves the cursor; the buffer still contains the two spaces. The next edit calls TextModel._pushEditOperations, which checks the saved candidates, appends a deletion for a still-whitespace-only line when it does not conflict with the incoming edit, clears the candidate list, and applies the combined operation. The breakpoint and authored steps above distinguish cursor movement from the deferred cleanup transaction.\n\n",
		);
		await runCli(
			"Disconnect and terminate the launched browser.",
			[
				"connection",
				"disconnect",
				"--context",
				"vscode-typing",
				"--connection",
				"browser",
			],
			environment,
		);
		await runCli("Stop the debugger service.", ["service", "stop"], environment);
		await emitTranscript(`\n---\n\n_Transcript saved to \`${transcriptPath}\`._\n`);
		await emitTranscript(
			`\n_Total duration: ${((Date.now() - startedAt) / 1000).toFixed(1)}s._\n`,
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

	async function terminatePersistedService(stateFile) {
		let processId;
		try {
			processId = JSON.parse(await readFile(stateFile, "utf8")).processId;
		} catch {
			return;
		}
		if (!Number.isInteger(processId)) return;
		if (process.platform === "win32") {
			await run("taskkill", ["/PID", String(processId), "/T", "/F"], {}, {
				timeoutMs: 10_000,
			});
			return;
		}
		const descendants = await posixDescendants(processId);
		for (const pid of [...descendants.reverse(), processId]) {
			try {
				process.kill(pid, "SIGKILL");
			} catch {
				// The process may already have exited.
			}
		}
	}

	async function posixDescendants(processId) {
		let children = [];
		try {
			const text = await readFile(
				`/proc/${processId}/task/${processId}/children`,
				"utf8",
			);
			children = text
				.trim()
				.split(/\s+/)
				.filter(Boolean)
				.map(Number);
		} catch {
			const listing = await run("ps", ["-axo", "pid=,ppid="], {}, {
				timeoutMs: 5_000,
			});
			if (listing.code !== 0) return [];
			const byParent = new Map();
			for (const line of listing.output.split(/\r?\n/)) {
				const [pid, parent] = line.trim().split(/\s+/).map(Number);
				if (!Number.isInteger(pid) || !Number.isInteger(parent)) continue;
				const siblings = byParent.get(parent) ?? [];
				siblings.push(pid);
				byParent.set(parent, siblings);
			}
			return collectDescendants(processId, byParent);
		}
		const descendants = [];
		for (const child of children) {
			descendants.push(child, ...(await posixDescendants(child)));
		}
		return descendants;
	}

	function collectDescendants(processId, byParent) {
		const result = [];
		for (const child of byParent.get(processId) ?? []) {
			result.push(child, ...collectDescendants(child, byParent));
		}
		return result;
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

async function runCli(
	explanation,
	arguments_,
	environment,
	timeoutMs = commandTimeoutMs,
) {
	const stepStartedAt = Date.now();
	const result = await run(cli, arguments_, environment, { timeoutMs });
	expect(result.code, `${arguments_.join(" ")}\n${result.output}`).toBe(0);
	return recordCompleted(
		`${explanation} (${((Date.now() - stepStartedAt) / 1000).toFixed(1)}s)`,
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

async function runJsonSilent(
	arguments_,
	environment,
	timeoutMs = commandTimeoutMs,
) {
	const result = await run(cli, ["--json", ...arguments_], environment, {
		timeoutMs,
	});
	expect(result.code, `${arguments_.join(" ")}\n${result.output}`).toBe(0);
	return JSON.parse(result.output);
}

async function runTextSilent(
	arguments_,
	environment,
	timeoutMs = commandTimeoutMs,
) {
	const result = await run(cli, arguments_, environment, { timeoutMs });
	expect(result.code, `${arguments_.join(" ")}\n${result.output}`).toBe(0);
	return result.output;
}

async function readEditorLines(explanation, environment) {
	const output = await runCli(
		explanation,
		[
			"target",
			"eval",
			'JSON.stringify({ lines: Array.from(document.querySelectorAll(".monaco-editor.focused .view-lines .view-line"), line => line.textContent?.replace(/\\u00a0/g, " ")), cursor: document.querySelector(".monaco-editor.focused .cursor")?.getAttribute("style") })',
		],
		environment,
	);
	return JSON.parse(output);
}

async function setTypeScriptMode(environment) {
	await retryCli(
		"Open Change Language Mode through its status-bar entry.",
		["target", "click", '[id="status.editor.mode"]'],
		environment,
		30_000,
	);
	await new Promise((resolve_) => setTimeout(resolve_, 100));
	await runCli(
		"Filter the language picker to TypeScript.",
		["target", "type", "TypeScript"],
		environment,
	);
	await runCli(
		"Accept TypeScript in the language picker.",
		["target", "key", "accept"],
		environment,
	);
	await new Promise((resolve_) => setTimeout(resolve_, 100));
	await retryCli(
		"Return focus to the TypeScript editor.",
		["target", "click", ".monaco-editor"],
		environment,
		30_000,
	);
}

async function recordCompleted(explanation, arguments_, result) {
	stepNumber += 1;
	const command = [cliName(), ...arguments_].map(quoteArgument).join(" ");
	const output = result.output.endsWith("\n") ? result.output : `${result.output}\n`;
	const text = `\n## Step ${stepNumber} — ${explanation}\n\n\`\`\`console\n$ ${command}\n\`\`\`\n\n\`\`\`text\n${output}\`\`\`\n`;
	await emitTranscript(text);
	return result.output;
}

async function emitTranscript(text) {
	process.stdout.write(text);
	await appendFile(transcriptPath, text);
}

function cliName() {
	return "dbgjs";
}

function quoteArgument(argument) {
	return /^[A-Za-z0-9_./:@%+=,#-]+$/.test(argument)
		? argument
		: JSON.stringify(argument);
}

function selectIndentCleanupCandidate(snapshot) {
	const candidates = snapshot.sources.flatMap((source) =>
		source.functions.flatMap((fn) =>
			fn.ranges
				.filter((range) => range.count > 0 && range.authoredStart != null)
				.map((range) => ({ source, function: fn, range })),
		),
	);
	const score = ({ function: fn, range }) => {
		const path = range.authoredStart.sourceUrl.toLowerCase();
		const breadcrumb = fn.breadcrumb ?? fn.name;
		const name = `${breadcrumb} ${fn.name}`.toLowerCase();
		let value = 0;
		if (breadcrumb === "TextModel._pushEditOperations") value += 10_000;
		if (name.includes("_pusheditoperations")) value += 2_000;
		if (path.endsWith("/model/textmodel.ts")) value += 1_000;
		if (name.includes("applyedits")) value += 800;
		if (path.includes("piecetreetextbuffer")) value += 500;
		if (name.includes("move") || name.includes("cursor")) value += 200;
		if (path.includes("/cursor/")) value += 100;
		return value;
	};
	return candidates.sort((left, right) => score(right) - score(left))[0];
}
