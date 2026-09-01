import assert from "node:assert/strict";
import { spawn } from "node:child_process";
import { randomUUID } from "node:crypto";
import { mkdir, rm, writeFile } from "node:fs/promises";
import { join, resolve } from "node:path";
import { fileURLToPath } from "node:url";
import { run } from "./live-test-harness.mjs";

const executableSuffix = process.platform === "win32" ? ".exe" : "";
const cli = resolve(`target/debug/jsdbg${executableSuffix}`);
const service = resolve(`target/debug/jsdbg-service${executableSuffix}`);
const fixtureProgram = resolve("tests/playwright/process-tree-browser-fixture.mjs");
const artifactsDirectory = resolve("artifacts");
const processTreeTranscriptPath = join(artifactsDirectory, "process-tree-browser-cli.md");
const vscodeTranscriptPath = join(artifactsDirectory, "vscode-iframe-cli.md");
const markdownFence = "```";

export async function generateTargetTreeTranscripts() {
	assert.equal(process.platform, "win32", "process-tree discovery is currently Windows-only");
	await buildBinaries();
	await generateProcessTreeBrowserTranscript();
	await generateVscodeIframeTranscript();
}

export async function generateProcessTreeBrowserTranscript(options = {}) {
	assert.equal(process.platform, "win32", "process-tree discovery is currently Windows-only");
	if (options.build === true) {
		await buildBinaries();
	}

	const root = resolve(".test-tmp", `process-tree-browser-${randomUUID()}`);
	const profileDirectory = join(root, "chrome-profile");
	const stateFile = join(root, "service.json");
	await mkdir(profileDirectory, { recursive: true });

	const fixture = spawn(process.execPath, [fixtureProgram, profileDirectory], {
		cwd: process.cwd(),
		env: { ...process.env },
		stdio: ["pipe", "pipe", "pipe"],
	});
	const environment = {
		JSDBG_SERVICE_EXE: service,
		JSDBG_SERVICE_STATE: stateFile,
	};
	let serviceStarted = false;

	try {
		const ready = await waitForReady(fixture);
		const transcript = new Transcript(
			processTreeTranscriptPath,
			"Composing a browser root discovered inside a process tree",
			"This is a generated live Windows E2E run. A Node root process owns a headless Chrome child with a DevTools port. `jsdbg` attaches once to the process tree, discovers the browser endpoint as a child capability, recursively publishes its page and OOPIF, and debugs the page through that composed route.",
			options.write !== false,
		);

		await transcript.runCli(
			"Create a context for the composed process tree.",
			["context", "create", "--context", ":process-tree-demo", "Process tree browser demo", "--set"],
			environment,
		);
		serviceStarted = true;
		const connected = await transcript.runCli(
			"Attach one connection to the Node process tree root. The browser endpoint is not configured separately.",
			[
				"connection",
				"add",
				"--process-tree",
				String(fixture.pid),
				"--connection",
				"tree",
				"--connect",
			],
			environment,
		);
		assert.match(connected, /browser\s+Chrome\//);
		assert.match(connected, /iframe\s+http:\/\/localhost:/);

		const targets = await waitForComposedTargets(environment, ready.url);
		const browser = targets.find((target) => target.targetType === "browser");
		const page = targets.find(
			(target) => target.targetType === "page" && target.url === ready.url,
		);
		const iframe = targets.find((target) => target.targetType === "iframe");
		assert.ok(browser, "the composed target inventory did not contain a browser");
		assert.ok(page, "the composed target inventory did not contain the fixture page");
		assert.ok(iframe, "the composed target inventory did not contain the OOPIF");

		const targetTree = await transcript.runCli(
			"List the heterogeneous target inventory contributed by the one process-tree connection.",
			["target", "list"],
			environment,
		);
		assert.match(targetTree, /└─ tree\/\$node-root:tree/);
		assert.doesNotMatch(targetTree, /parent=/);
		await transcript.runCli(
			"Inspect the canonical resource graph, including process, browser-control, and debug capabilities.",
			["target", "graph"],
			environment,
		);
		await transcript.runCli(
			"Select only the recursively discovered cross-origin iframe.",
			["target", "list", "--type", "iframe"],
			environment,
		);
		await transcript.runCli(
			"Attach directly to the printed OOPIF target and make it the selected debug scope.",
			["target", "attach", "--target", iframe.targetId, "--set"],
			environment,
		);
		await transcript.runCli(
			"Evaluate inside the cross-origin iframe through the composed process-tree → browser → page → OOPIF route.",
			[
				"target",
				"eval",
				'document.querySelector("#oopif-marker").textContent',
			],
			environment,
		);
		await transcript.runCli(
			"Disconnect the process tree; all contributed browser and descendant resources disappear together.",
			["connection", "disconnect", "--connection", "tree"],
			environment,
		);
		await transcript.runCli("Stop the debugger service.", ["service", "stop"], environment);
		serviceStarted = false;
		await transcript.finish(
			"The browser root, page, and OOPIF were discovered without adding a separate CDP connection. The printed OOPIF ID was directly actionable, and evaluation reached it through capabilities routed by the same process-tree graph.",
		);
	} finally {
		if (serviceStarted) {
			await run(cli, ["service", "stop"], environment);
		}
		fixture.stdin.end();
		await waitForExit(fixture);
		await rm(root, { recursive: true, force: true });
	}
}

export async function generateVscodeIframeTranscript(options = {}) {
	assert.equal(process.platform, "win32", "VS Code process-tree discovery is currently Windows-only");
	if (options.build === true) {
		await buildBinaries();
	}

	const tree = await selectCurrentVscodeTree();
	const root = resolve(".test-tmp", `vscode-iframe-${randomUUID()}`);
	const stateFile = join(root, "service.json");
	await mkdir(root, { recursive: true });

	const environment = {
		JSDBG_SERVICE_EXE: service,
		JSDBG_SERVICE_STATE: stateFile,
	};
	const transcript = new Transcript(
		vscodeTranscriptPath,
		"Discovering and debugging an iframe inside a live VS Code process tree",
		"This transcript is generated from the VS Code instance that launched the generator. `process list --vscode` is an OS/window inventory: its renderer row exposes an actionable process target but cannot contain CDP iframe targets. Those descendants appear after the process-tree connection enables renderer-local target discovery.",
		options.write !== false,
	);
	let serviceStarted = false;

	try {
		await transcript.runCli(
			"Show the current VS Code window's process inventory. Renderer rows have no iframe children because this command does not establish a CDP target-discovery connection.",
			["process", "list", "--vscode", "--no-cmd-line", "--no-trim", "--filter", "renderer"],
			environment,
		);
		await transcript.runCli(
			"Create an isolated context for live VS Code discovery.",
			["context", "create", "--context", ":vscode-iframe-demo", "VS Code iframe demo", "--set"],
			environment,
		);
		serviceStarted = true;
		await transcript.runCli(
			"Attach one process-tree connection to the VS Code main process. This enables event-driven WebContents discovery and renderer-local CDP target discovery.",
			[
				"connection",
				"add",
				"--process-tree",
				String(tree.rootProcessId),
				"--connection",
				"vscode",
				"--connect",
			],
			environment,
			{ timeoutMs: 60_000 },
		);

		const targets = await waitForVscodeIframe(environment);
		const iframe = selectVscodeIframe(targets);
		const renderer = findRendererAncestor(targets, iframe);
		assert.ok(renderer, `iframe '${iframe.targetId}' has no renderer ancestor`);

		const targetTree = await transcript.runCli(
			"Render the connected VS Code target inventory as a spanning tree. The selected WebContents renderer now owns the recursively discovered iframe.",
			["target", "list"],
			environment,
			{ timeoutMs: 60_000 },
		);
		assert.ok(
			targetTree.includes(`vscode/${renderer.targetId}`),
			`target tree did not contain renderer '${renderer.targetId}'`,
		);
		assert.ok(
			targetTree.includes(`vscode/${iframe.targetId}`),
			`target tree did not contain iframe '${iframe.targetId}'`,
		);
		assert.doesNotMatch(targetTree, /parent=/);

		await transcript.runCli(
			"List the iframe targets alone so the generated transcript records the exact actionable ID.",
			["target", "list", "--type", "iframe"],
			environment,
		);
		await transcript.runCli(
			"Attach directly to the discovered VS Code iframe.",
			["target", "attach", "--target", iframe.targetId, "--set"],
			environment,
		);
		await transcript.runCli(
			"Evaluate inside the VS Code iframe and report its extension identity and any same-process inner frame.",
			[
				"target",
				"eval",
				'JSON.stringify({ extensionId: new URL(location.href).searchParams.get("extensionId"), title: document.title, innerFrame: document.querySelector("iframe")?.getAttribute("id") ?? null, innerFrameAccessible: !!document.querySelector("iframe")?.contentDocument })',
			],
			environment,
		);
		await transcript.runCli(
			"Disconnect the live VS Code process tree.",
			["connection", "disconnect", "--connection", "vscode"],
			environment,
		);
		await transcript.runCli("Stop the isolated debugger service.", ["service", "stop"], environment);
		serviceStarted = false;
		await transcript.finish(
			`The OS process inventory exposed the renderer process, while the connected target tree exposed WebContents target ${renderer.targetId} and its iframe descendant ${iframe.targetId}. The iframe ID was directly attachable and evaluable.`,
		);
	} finally {
		if (serviceStarted) {
			await run(cli, ["service", "stop"], environment);
		}
		await rm(root, { recursive: true, force: true });
	}
}

class Transcript {
	constructor(path, title, introduction, write) {
		this.path = path;
		this.write = write;
		this.startedAt = performance.now();
		this.previousStepFinishedAt = undefined;
		this.stepNumber = 0;
		this.sections = [
			`# ${title}\n\n${introduction}\n\n_Generated by \`npm run generate:target-tree-transcripts\`; do not edit this file manually._\n`,
		];
	}

	async runCli(explanation, arguments_, environment, options = {}) {
		const startedAt = performance.now();
		const gapMs = this.previousStepFinishedAt === undefined
			? 0
			: startedAt - this.previousStepFinishedAt;
		const result = await run(cli, arguments_, environment, {
			timeoutMs: options.timeoutMs ?? 45_000,
		});
		const finishedAt = performance.now();
		this.previousStepFinishedAt = finishedAt;
		this.stepNumber += 1;
		const output = stableOutput(result.output);
		const content =
			`\n## Step ${this.stepNumber} — ${explanation}\n\n` +
			`_Timing: ${formatDuration(gapMs)} passed since the previous step; the command took ${formatDuration(result.durationMs)}; ${formatDuration(finishedAt - this.startedAt)} elapsed since the transcript started._\n\n` +
			`${markdownFence}console\n$ ${formatCommand("jsdbg", arguments_)}\n${markdownFence}\n\n` +
			`${markdownFence}text\n${output.endsWith("\n") ? output : `${output}\n`}${markdownFence}\n`;
		this.sections.push(content);
		process.stdout.write(content);
		assert.equal(result.code, 0, `${arguments_.join(" ")}\n${result.output}`);
		return result.output;
	}

	async finish(summary) {
		const content = `\n${summary}\n`;
		this.sections.push(content);
		process.stdout.write(content);
		if (!this.write) {
			return;
		}
		await mkdir(artifactsDirectory, { recursive: true });
		await writeFile(this.path, this.sections.join(""));
	}
}

async function buildBinaries() {
	if (process.env.JSDBG_SKIP_BUILD === "1") {
		return;
	}
	const result = await run("cargo", ["build", "--bins"], {});
	assert.equal(result.code, 0, result.output);
}

async function selectCurrentVscodeTree() {
	const result = await run(
		cli,
		["--json", "process", "list", "--vscode", "--no-cmd-line"],
		{},
		{ timeoutMs: 45_000 },
	);
	assert.equal(result.code, 0, result.output);
	const trees = JSON.parse(result.output);
	assert.ok(trees.length > 0, "no running VS Code process trees were found");

	const explicitRoot = Number(process.env.JSDBG_VSCODE_ROOT_PID);
	if (Number.isSafeInteger(explicitRoot) && explicitRoot > 0) {
		const explicitTree = trees.find((tree) => tree.rootProcessId === explicitRoot);
		assert.ok(explicitTree, `VS Code process tree ${explicitRoot} was not found`);
		return explicitTree;
	}

	return trees.find((tree) =>
		tree.processes.some((candidate) => candidate.processId === process.pid)
	) ?? trees.find((tree) =>
		tree.processes.some((candidate) =>
			candidate.agentSessions?.some((session) =>
				session.workingDirectories?.some((directory) =>
					decodeURIComponent(directory).toLowerCase().includes(process.cwd().toLowerCase())
				)
			)
		)
	) ?? trees.toSorted((left, right) => right.processes.length - left.processes.length)[0];
}

async function waitForReady(child) {
	let stdout = "";
	let stderr = "";
	child.stdout.setEncoding("utf8");
	child.stderr.setEncoding("utf8");
	child.stdout.on("data", (chunk) => {
		stdout += chunk;
	});
	child.stderr.on("data", (chunk) => {
		stderr += chunk;
	});
	const ready = await pollUntil(() => {
		const line = completeLines(stdout).find(Boolean);
		return line === undefined ? undefined : JSON.parse(line);
	}, (value) => value?.kind === "ready", 30_000);
	assert.ok(ready, `process-tree browser fixture did not become ready:\n${stderr}`);
	return ready;
}

async function waitForComposedTargets(environment, pageUrl) {
	return pollTargets(
		environment,
		":process-tree-demo",
		(targets) => {
			const types = targets
				.filter(
					(target) =>
						target.targetType === "browser" ||
						(target.targetType === "page" && target.url === pageUrl) ||
						target.targetType === "iframe",
				)
				.map((target) => target.targetType)
				.toSorted();
			return types.join(",") === "browser,iframe,page";
		},
	);
}

async function waitForVscodeIframe(environment) {
	return pollTargets(
		environment,
		":vscode-iframe-demo",
		(targets) => targets.some((target) => target.targetType === "iframe"),
		60_000,
	);
}

async function pollTargets(environment, context, predicate, timeoutMs = 30_000) {
	const targets = await pollUntil(async () => {
		const result = await run(
			cli,
			["--json", "target", "list", "--context", context],
			environment,
			{ timeoutMs: 45_000 },
		);
		if (result.code !== 0) {
			return undefined;
		}
		return JSON.parse(result.output).targets;
	}, predicate, timeoutMs);
	assert.ok(targets, `target inventory did not satisfy the expected condition within ${timeoutMs} ms`);
	return targets;
}

function selectVscodeIframe(targets) {
	const iframes = targets.filter((target) => target.targetType === "iframe");
	return iframes.find((target) =>
		target.url.includes("extensionId=vscode.markdown-language-features")
	) ?? iframes.find((target) => target.url.startsWith("vscode-webview://")) ?? iframes[0];
}

function findRendererAncestor(targets, target) {
	const byId = new Map(targets.map((candidate) => [candidate.targetId, candidate]));
	let current = target;
	while (current.parentTargetId !== undefined && current.parentTargetId !== null) {
		current = byId.get(current.parentTargetId);
		if (current === undefined) {
			return undefined;
		}
		if (current.subtype === "electron-renderer" || current.targetId.startsWith("renderer-")) {
			return current;
		}
	}
	return undefined;
}

async function pollUntil(operation, predicate, timeoutMs) {
	const deadline = performance.now() + timeoutMs;
	do {
		const value = await operation();
		if (value !== undefined && predicate(value)) {
			return value;
		}
		await new Promise((resolvePromise) => setTimeout(resolvePromise, 100));
	} while (performance.now() < deadline);
	return undefined;
}

function stableOutput(output) {
	return output
		.replaceAll(process.cwd(), "<worktree>")
		.replaceAll(process.cwd().replaceAll("\\", "/"), "<worktree>");
}

function formatDuration(milliseconds) {
	return `${(milliseconds / 1_000).toFixed(3)} s`;
}

function formatCommand(command, arguments_) {
	return [command, ...arguments_].map(shellQuote).join(" ");
}

function shellQuote(value) {
	return /^[A-Za-z0-9_./:=?&$-]+$/.test(value)
		? value
		: `'${value.replaceAll("'", `'\\''`)}'`;
}

function waitForExit(child) {
	if (child.exitCode !== null) {
		return Promise.resolve();
	}
	return new Promise((resolvePromise) => child.once("exit", resolvePromise));
}

function completeLines(value) {
	const lines = value.split(/\r?\n/);
	return value.endsWith("\n") ? lines : lines.slice(0, -1);
}

if (process.argv[1] !== undefined && resolve(process.argv[1]) === fileURLToPath(import.meta.url)) {
	generateTargetTreeTranscripts().catch((error) => {
		console.error(error?.stack ?? error);
		process.exitCode = 1;
	});
}
