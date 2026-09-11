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
const transcriptPath = resolve("artifacts/vscode-heap-classes.md");
const expectedDurationMs = 120_000;
const hardTimeoutMs = 180_000;
const commandTimeoutMs = 120_000;
const heapMarker = "dbgjs-vscode-heap-golden-marker";
let stepNumber = 0;

test("compares vscode.dev editor buffers with infrastructure objects", async () => {
	test.setTimeout(hardTimeoutMs);
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
		"# What occupies the live vscode.dev JavaScript heap?\n\n" +
			"This scenario compares editor text-buffer objects with the event, lifecycle, and collection infrastructure around them. It uses a real V8 heap snapshot, source-maps constructor locations back to authored TypeScript classes, then queries the same immutable indexed heap graph for strings, reverse references, retainer paths, dominators, and arbitrary node aggregates.\n\n" +
			`- Expected duration: ${expectedDurationMs / 1000}s\n- Hard timeout: ${hardTimeoutMs / 1000}s\n`,
	);

	const stateDirectory = await mkdtemp(join(tmpdir(), "dbgjs-vscode-heap-"));
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
			"Create the isolated heap investigation workspace.",
			["context", "create", "--context", "vscode-heap", "--set"],
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
				"vscode-heap",
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
			"Create an editor so its text model and PieceTree buffer are live.",
			[
				"playwright",
				"await page.keyboard.press('Control+K'); await page.keyboard.press('n')",
			],
			environment,
		);
		await retryCli(
			"Focus the editor.",
			["target", "click", ".monaco-editor"],
			environment,
			30_000,
		);
		await runCli(
			"Populate the editor with enough text to exercise its buffer.",
			["target", "type", "  foo\n  bar\n  baz"],
			environment,
		);
		await runCli(
			"Create one unmistakable retained object for graph navigation.",
			[
				"target",
				"eval",
				`globalThis.__dbgjsHeapGolden = { title: ${JSON.stringify(heapMarker)}, payload: { kind: "golden", values: [1, 2, 3] } }; ${JSON.stringify(heapMarker)}`,
			],
			environment,
		);
		await runCli(
			"Capture the managed V8 heap snapshot.",
			["heap", "capture"],
			environment,
		);

		const infrastructureTree = await runCli(
			"Inspect the event, lifecycle, observable, and collection infrastructure.",
			[
				"heap",
				"classes",
				"--filter",
				"Emitter|Disposable|LinkedList|Observable",
				"--sort-by-instances",
				"--max-lines",
				"80",
			],
			environment,
		);
		expect(infrastructureTree).toMatch(/FunctionDisposable|DisposableStore/);
		expect(infrastructureTree.trimEnd().split("\n").length).toBeLessThanOrEqual(80);
		const pieceTree = await runCli(
			"Drill into the editor's PieceTree buffer and show representative object IDs.",
			[
				"heap",
				"classes",
				"--filter",
				"PieceTree",
				"--instances",
				"--max-lines",
				"60",
			],
			environment,
		);
		expect(pieceTree).toContain("PieceTreeTextBuffer");
		expect(pieceTree).not.toContain(".constructor");
		expect(pieceTree).toMatch(/PieceTreeTextBuffer@\d+\s+id \d+/);
		expect(pieceTree.trimEnd().split("\n").length).toBeLessThanOrEqual(60);

		const markerStrings = await runCli(
			"Search arbitrary heap string nodes for the retained marker.",
			["heap", "strings", "--grep", heapMarker, "--limit", "8"],
			environment,
		);
		expect(markerStrings).toContain(heapMarker);
		const markerSelection = await runJson(
			"Read the matching string nodes and stable capture-scoped references.",
			["heap", "strings", "--grep", heapMarker, "--limit", "8"],
			environment,
			summarizeHeapSelection,
		);
		const markerNode = await findStringReferencedByProperty(
			markerSelection,
			"title",
			environment,
		);
		expect(markerNode).toBeDefined();
		const markerReference = markerNode.reference;
		const reverseReferences = await runCli(
			"Follow the reverse property edge from the marker string to its retaining object.",
			["heap", "refs", markerReference, "--incoming", "--limit", "12"],
			environment,
		);
		expect(reverseReferences).toContain('"title"');
		const retainerPath = await runCli(
			"Find a readable retaining path from the marker back to the synthetic heap root.",
			["heap", "retainer-path", markerReference],
			environment,
		);
		expect(retainerPath).toContain("Heap path");
		const dominators = await runCli(
			"Compute the marker's immediate-dominator chain and retained sizes.",
			["heap", "dominators", markerReference],
			environment,
		);
		expect(dominators).toContain("Dominator chain");
		const aggregate = await runCli(
			"Aggregate every heap node kind, including non-class objects and strings.",
			["heap", "aggregate", ".", "--by", "type", "--limit", "12"],
			environment,
		);
		expect(aggregate).toContain("Heap aggregate");
		expect(aggregate).toContain("aggregate groups omitted");

		const snapshot = await runJson(
			"Load the cached compact constructor index as structured data.",
			["heap", "classes", "."],
			environment,
			summarizeClassSnapshot,
		);
		expect(snapshot.classes.length).toBeGreaterThan(100);
		expect(snapshot.totalInstances).toBeGreaterThan(1_000);
		expect(snapshot.analysis.usedCachedGroups).toBe(true);
		const pieceTreeClasses = snapshot.classes.filter((class_) =>
			/PieceTree/.test(class_.name),
		);
		const infrastructure = snapshot.classes.filter((class_) =>
			/(Emitter|Disposable|LinkedList|Observable)/.test(class_.name),
		);
		expect(
			pieceTreeClasses.some((class_) => class_.name === "PieceTreeTextBuffer"),
		).toBe(true);
		expect(infrastructure.length).toBeGreaterThan(0);

		const topAuthored = snapshot.classes
			.filter((class_) => /\.tsx?$/.test(class_.sourceUrl))
			.sort((left, right) => right.instanceCount - left.instanceCount)
			.slice(0, 12);
		const infrastructureInstances = sumInstances(infrastructure);
		const pieceTreeInstances = sumInstances(pieceTreeClasses);
		expect(infrastructureInstances).toBeGreaterThan(pieceTreeInstances);
		await emitTranscript(renderFindings({
			snapshot,
			topAuthored,
			infrastructure,
			infrastructureInstances,
			pieceTree: pieceTreeClasses,
			pieceTreeInstances,
		}));

		await runCli(
			"Disconnect and terminate the launched browser.",
			[
				"connection",
				"disconnect",
				"--context",
				"vscode-heap",
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

function sumInstances(classes) {
	return classes.reduce((total, class_) => total + class_.instanceCount, 0);
}

function renderFindings({
	snapshot,
	topAuthored,
	infrastructure,
	infrastructureInstances,
	pieceTree,
	pieceTreeInstances,
}) {
	const table = topAuthored
		.map(
			(class_) =>
				`| ${class_.name} | ${class_.instanceCount} | ${class_.shallowSize} | ${class_.sourceUrl} |`,
		)
		.join("\n");
	const ratio =
		pieceTreeInstances === 0
			? "n/a"
			: (infrastructureInstances / pieceTreeInstances).toFixed(1);
	return `\n## Findings\n\n` +
		`The snapshot contains **${snapshot.totalInstances}** source-located object instances across **${snapshot.classes.length}** classes. ` +
		`The focused infrastructure families account for **${infrastructureInstances}** instances in ${infrastructure.length} classes, while PieceTree accounts for **${pieceTreeInstances}** instances in ${pieceTree.length} classes. ` +
		`In this run, the selected infrastructure-to-buffer instance ratio is **${ratio}:1**. The editor buffer is structurally important, but general event, lifecycle, observable, and collection plumbing is much more numerous.\n\n` +
		`### Most numerous authored classes\n\n` +
		`| Class | Instances | Shallow bytes | Authored source |\n` +
		`| --- | ---: | ---: | --- |\n${table}\n\n` +
		`The analysis parsed the snapshot in ${(snapshot.analysis.parseDurationMicros / 1_000_000).toFixed(3)}s and projected constructors in ${(snapshot.analysis.projectionDurationMicros / 1_000_000).toFixed(3)}s.\n`;
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
		{
			output: `Summary of the full JSON response:\n${JSON.stringify(summarize(parsed), null, 2)}\n`,
		},
	);
	return parsed;
}

async function runJsonSilent(arguments_, environment) {
	const result = await run(cli, ["--json", ...arguments_], environment, {
		timeoutMs: commandTimeoutMs,
	});
	expect(result.code, `${arguments_.join(" ")}\n${result.output}`).toBe(0);
	return JSON.parse(result.output);
}

function summarizeClassSnapshot(snapshot) {
	return {
		captureId: snapshot.captureId,
		totalInstances: snapshot.totalInstances,
		totalShallowSize: snapshot.totalShallowSize,
		classCount: snapshot.classes.length,
		analysis: snapshot.analysis,
	};
}

function summarizeHeapSelection(selection) {
	return {
		captureId: selection.captureId,
		totalNodes: selection.totalNodes,
		totalEdges: selection.totalEdges,
		graphParseDurationMicros: selection.graphParseDurationMicros,
		usedCachedGraph: selection.usedCachedGraph,
		nodes: selection.nodes.map((node) => ({
			reference: node.reference,
			nodeType: node.nodeType,
			stringValue: node.stringValue,
			shallowSize: node.shallowSize,
			incomingReferenceCount: node.incomingReferenceCount,
		})),
	};
}

async function findStringReferencedByProperty(selection, property, environment) {
	for (const node of selection.nodes) {
		const references = await runJsonSilent(
			["heap", "refs", node.reference, "--incoming", "--limit", "100"],
			environment,
		);
		if (references.references.some((reference) => reference.name === property)) {
			return node;
		}
	}
	return undefined;
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
