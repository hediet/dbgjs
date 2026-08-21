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
const cli = resolve(`target/release/jsdbg${executableSuffix}`);
const service = resolve(`target/release/jsdbg-service${executableSuffix}`);
const transcriptPath = resolve("artifacts/vscode-typing-coverage.md");
const expectedDurationMs = 120_000;
const hardTimeoutMs = Math.ceil(expectedDurationMs * 1.2);
const commandTimeoutMs = 90_000;
let stepNumber = 0;

test("reports vscode.dev code executed by typing one character", async () => {
	test.setTimeout(hardTimeoutMs);
	const startedAt = Date.now();
	const build = await run("cargo", ["build", "--release", "--bins"], {}, { timeoutMs: commandTimeoutMs });
	expect(build.code, build.output).toBe(0);
	await mkdir(resolve("artifacts"), { recursive: true });
	await writeFile(
		transcriptPath,
		`# Code executed by typing one character in \`vscode.dev\`\n\nThis is a real jsdbg/HubRPC/Playwright/CDP precise-coverage run.\n\n- Expected duration: ${expectedDurationMs / 1000}s\n- Hard timeout (+20%): ${hardTimeoutMs / 1000}s\n`,
	);

	const stateDirectory = await mkdtemp(join(tmpdir(), "jsdbg-vscode-coverage-"));
	const stateFile = join(stateDirectory, "service.json");
	const sourceMapCache = join(tmpdir(), "jsdbg-vscode-source-map-cache");
	await mkdir(sourceMapCache, { recursive: true });
	const environment = {
		JSDBG_SERVICE_EXE: service,
		JSDBG_SERVICE_STATE: stateFile,
		JSDBG_SOURCE_MAP_CACHE: sourceMapCache,
	};
	try {
		await runCli(
			"Create and select a workspace for this investigation.",
			["context", "create", "vscode-typing"],
			environment,
		);
		await runCli(
			"Select it as the CLI-local workspace.",
			["set", "workspace", "vscode-typing"],
			environment,
		);
		await runCli(
			"Launch bundled Chromium, open vscode.dev, connect CDP, and auto-attach the page.",
			[
				"connection",
				"add",
				"vscode-typing",
				"browser",
				"--playwright",
				"https://vscode.dev/",
				"--ignore-https-errors",
				"--connect",
			],
			environment,
		);
		await runCli(
			"Select the unique page target.",
			["set", "target", "page"],
			environment,
		);
		await retryCli(
			"Wait for the workbench DOM and focus it with a real CDP click.",
			["target", "click", ".monaco-workbench"],
			environment,
			60_000,
		);
		await runCli(
			"Create an untitled editor using a real Ctrl+N key chord.",
			["target", "key", "ctrl+n"],
			environment,
		);
		await retryCli(
			"Focus the Monaco editor through its DOM box.",
			["target", "click", ".monaco-editor"],
			environment,
			30_000,
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
			"Type exactly one character through CDP Input.insertText.",
			["target", "type", "x"],
			environment,
		);
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
		expect(report).toMatch(/\.tsx?\s+\d+ hit LoC/);
		expect(report).toMatch(/… \[\d+ items, \d+ files, \d+ hit LoC\]/);
		expect(report).not.toContain("additional files omitted");
		expect(report).not.toContain("additional hit ranges omitted");
		expect(report).toContain("snippet/browser/snippetParser.ts");
		expect(report).not.toContain("Scanner.next");
		expect(report.split("\n").length).toBeLessThan(240);
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
		const candidate = selectTypingCandidate(delta);
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
		expect(drilldown).toContain("CursorsController.type");
		await runCli(
			`Install a coverage-guided logpoint in \`${candidate.function.breadcrumb ?? candidate.function.name}\`.`,
			[
				"target",
				"logpoint",
				"vscode-typing",
				"browser",
				"page",
				"typing-path",
				sourcePath,
				String(candidate.range.authoredStart.line),
				String(candidate.range.authoredStart.column),
				'"coverage-guided"',
			],
			environment,
		);
		await runJsonSilent(["log"], environment);
		await runCli(
			"Type a second character to revisit the measured path.",
			["target", "type", "y"],
			environment,
		);
		await new Promise((resolve_) => setTimeout(resolve_, 250));
		const targetAfterLogpoint = await runCli(
			"Inspect the target after the second character.",
			["target", "show"],
			environment,
		);
		expect(targetAfterLogpoint).toContain("typing-path");
		const newLogs = await runCli(
			"Read only console entries newer than the CLI-local log cursor.",
			["log"],
			environment,
		);
		expect(newLogs).toContain("typing-path");
		expect(newLogs).toContain("coverage-guided");
		await runCli(
			"Create a burst of console messages to demonstrate bounded log paging.",
			[
				"target",
				"eval",
				'void Array.from({ length: 25 }, (_, i) => console.log("burst", i))',
			],
			environment,
		);
		const boundedLogs = await runCli(
			"Read the latest 20 log entries; older unseen entries are summarized.",
			["log", "--limit", "20"],
			environment,
		);
		expect(boundedLogs).toContain("[...skipped 5 entries...]");
		const editorAfterSecondCharacter = await runCli(
			"Verify the editor now contains both typed characters.",
			[
				"target",
				"eval",
				'document.querySelector(".monaco-editor.focused .view-lines")?.textContent ?? document.querySelector(".view-lines")?.textContent',
			],
			environment,
		);
		expect(editorAfterSecondCharacter).toContain("xy");
		const editorText = await runCli(
			"Read the focused editor through CDP and verify the inserted character is present.",
			[
				"target",
				"eval",
				'document.querySelector(".monaco-editor.focused .view-lines")?.textContent ?? document.querySelector(".view-lines")?.textContent',
			],
			environment,
		);
		expect(editorText).toContain("x");
		await runCli(
			"Disconnect and terminate the launched browser.",
			["connection", "disconnect", "vscode-typing", "browser"],
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
	return "jsdbg";
}

function quoteArgument(argument) {
	return /^[A-Za-z0-9_./:@%+=,#-]+$/.test(argument)
		? argument
		: JSON.stringify(argument);
}

function selectTypingCandidate(snapshot) {
	const candidates = snapshot.sources.flatMap((source) =>
		source.functions.flatMap((fn) =>
			fn.ranges
				.filter((range) => range.count > 0 && range.authoredStart != null)
				.map((range) => ({ source, function: fn, range })),
		),
	);
	const score = ({ function: fn, range }) => {
		const path = range.authoredStart.sourceUrl.toLowerCase();
		const name = `${fn.breadcrumb ?? ""} ${fn.name}`.toLowerCase();
		let value = 0;
		if (path.includes("/cursor/") || path.endsWith("/cursor.ts")) value += 100;
		if (path.includes("/editor/")) value += 40;
		if (path.includes("vieweventhandler")) value += 30;
		if (name.includes("type")) value += 80;
		if (name.includes("cursor")) value += 60;
		if (name.includes("change")) value += 20;
		return value;
	};
	return candidates.sort((left, right) => score(right) - score(left))[0];
}
