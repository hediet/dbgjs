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
const vscodeIframeScreenshotPath = join(artifactsDirectory, "vscode-iframe.png");
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
			"This is a generated live Windows E2E run. A Node root process owns a headless Chrome child with a DevTools port. Passive root recognition finds the browser process; `process list --full` then uses that debugger route to nest the browser page and OOPIF beneath the canonical OS process rather than printing a duplicate browser-endpoint node.",
			options.write !== false,
		);

		const fullProcessTree = await transcript.runCli(
			"Recognize the browser root and actively expand it with the same process-tree target discovery used by connections.",
			[
				"process",
				"list",
				"--root",
				"browser",
				"--full",
				"--no-cmd-line",
				"--no-trim",
				"--filter",
				`p:${ready.browserPid}`,
			],
			environment,
			{ timeoutMs: 60_000 },
		);
		assert.match(fullProcessTree, /browser-main/);
		assert.match(fullProcessTree, /Process tree browser demo/);
		assert.match(fullProcessTree, /\[iframe\]/);
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
	const rendererProcess = selectCurrentRendererProcess(tree);
	const connectionId = `process-tree-${tree.rootProcessId}`;
	const root = resolve(".test-tmp", `vscode-iframe-${randomUUID()}`);
	const stateFile = join(root, "service.json");
	await mkdir(root, { recursive: true });

	const environment = {
		JSDBG_SERVICE_EXE: service,
		JSDBG_SERVICE_STATE: stateFile,
	};
	const transcript = new Transcript(
		vscodeTranscriptPath,
		"Selectively debugging an iframe inside a live VS Code process tree",
		"This transcript is generated from the VS Code instance that launched the generator. Connecting the process tree discovers resources and routes without creating a debugger-engine attachment for every target. Provider-internal supervision correlates Electron WebContents with native CDP identities and contributes related OOPIFs and workers; only the explicitly selected iframe contributes loaded scripts.",
		options.write !== false,
	);
	let serviceStarted = false;

	try {
		const processTrees = await transcript.runCli(
			"List every recognized VS Code process tree without starting the debugger service or attaching any target.",
			[
				"process",
				"list",
				"--root",
				"vscode",
				"--no-cmd-line",
			],
			environment,
			{ timeoutMs: 60_000 },
		);
		assert.match(processTrees, new RegExp(`VS Code process tree ${tree.rootProcessId}\\b`));
		assert.match(processTrees, new RegExp(`p:${rendererProcess.processId}\\s+renderer`));
		await transcript.runCli(
			"Create an isolated context for live VS Code discovery.",
			["context", "create", "--context", ":vscode-iframe-demo", "VS Code iframe demo", "--set"],
			environment,
		);
		serviceStarted = true;
		await transcript.runCli(
			"Connect the selected VS Code process tree. This discovers its targets but does not create debugger-engine attachments for them.",
			[
				"connection",
				"add",
				"--process-tree",
				String(tree.rootProcessId),
				"--connection",
				connectionId,
				"--connect",
			],
			environment,
			{ timeoutMs: 60_000 },
		);

		const targets = await waitForVscodeIframe(environment, rendererProcess.windowTitle);
		const renderer = targets.find(
			(target) =>
				target.subtype === "electron-renderer" &&
				target.title === rendererProcess.windowTitle,
		);
		assert.ok(renderer, "the selected VS Code window did not contribute an Electron renderer");
		const iframe = selectVscodeIframe(targets, renderer.targetId);
		assert.ok(iframe, `renderer '${renderer.targetId}' has no iframe descendant`);
		const main = targets.find(
			(target) =>
				target.targetType === "node" &&
				target.url === `process:${tree.rootProcessId}`,
		);
		assert.ok(main, "the selected VS Code tree did not contribute its main process target");

		const beforeAttach = await transcript.runCli(
			"Show that discovery alone contributed no loaded scripts because no debugger target is attached.",
			["source", "tree", "loaded", "--all"],
			environment,
		);
		assert.match(beforeAttach, /No loaded sources are currently observed/);
		const targetTree = await transcript.runCli(
			"Render the connected VS Code target inventory as a spanning tree. WebContents contain only their related iframe and worker targets.",
			["target", "list"],
			environment,
			{ timeoutMs: 60_000 },
		);
		assert.ok(
			targetTree.includes(`${connectionId}/$node-root:${connectionId}`) &&
				targetTree.includes(`./${renderer.targetId}`),
			`target tree did not contain renderer '${renderer.targetId}'`,
		);
		const iframeRelativeId = iframe.targetId.startsWith(`${renderer.targetId}/`)
			? iframe.targetId.slice(renderer.targetId.length + 1)
			: iframe.targetId;
		assert.ok(
			targetTree.includes(`./${iframeRelativeId}`),
			`target tree did not contain iframe '${iframe.targetId}'`,
		);
		assert.doesNotMatch(targetTree, /parent=/);

		await transcript.runCli(
			"List the iframe targets alone so the generated transcript records the exact actionable ID.",
			["target", "list", "--type", "iframe"],
			environment,
		);
		await transcript.runCli(
			"Attach the debugger engine only to the discovered VS Code iframe and select it.",
			["target", "attach", "--target", iframe.targetId, "--set", "--force"],
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
			"Capture the selected iframe through its ordinary page screenshot capability.",
			["screenshot", "capture", "--output", vscodeIframeScreenshotPath],
			environment,
			{ timeoutMs: 60_000 },
		);
		const loadedSources = await waitForLoadedSources(environment);
		assert.doesNotMatch(loadedSources, /No loaded sources are currently observed/);
		await transcript.runCli(
			"Dump the loaded-source tree contributed by the one explicitly attached iframe.",
			["source", "tree", "loaded", "--max-lines", "40", "--no-trim"],
			environment,
		);
		await transcript.runCli(
			"Release the iframe before checking the renderer's own generated scripts and source maps.",
			["target", "release", "--target", iframe.targetId],
			environment,
		);
		await transcript.runCli(
			"Attach the renderer WebContents without attaching any of the other discovered targets.",
			["target", "attach", "--target", renderer.targetId, "--set", "--force"],
			environment,
		);
		await transcript.runCli(
			"Start precise coverage to materialize the renderer's source-map projections.",
			["coverage", "start"],
			environment,
		);
		await transcript.wait("Let the live renderer collect precise coverage.", 1_000);
		const rendererCoverage = await transcript.runCli(
			"Capture and resolve the renderer coverage through loadable source maps.",
			["coverage", "capture", "--max-lines", "25", "--no-trim"],
			environment,
			{ timeoutMs: 120_000 },
		);
		assert.match(rendererCoverage, /\.tsx?\b/);
		await transcript.runCli(
			"Release the renderer before attaching the VS Code main process.",
			["target", "release", "--target", renderer.targetId],
			environment,
		);
		await transcript.runCli(
			"Attach only the VS Code main process target.",
			["target", "attach", "--target", main.targetId, "--set", "--force"],
			environment,
			{ timeoutMs: 60_000 },
		);
		await transcript.runCli(
			"Start precise coverage to materialize the main process source-map projections.",
			["coverage", "start"],
			environment,
		);
		await transcript.wait("Let the live VS Code main process collect precise coverage.", 1_000);
		const mainCoverage = await transcript.runCli(
			"Capture and resolve the main process coverage through loadable source maps.",
			["coverage", "capture", "--max-lines", "25", "--no-trim"],
			environment,
			{ timeoutMs: 120_000 },
		);
		assert.match(mainCoverage, /\.tsx?\b/);
		await transcript.runCli(
			"Render the terminal source tree reached through the verified renderer and main-process maps.",
			["source", "tree", "resolved", "--max-lines", "40", "--no-trim"],
			environment,
		);
		await transcript.runCli(
			"Disconnect the live VS Code process tree.",
			["connection", "disconnect", "--connection", connectionId],
			environment,
		);
		await transcript.runCli("Stop the isolated debugger service.", ["service", "stop"], environment);
		serviceStarted = false;
		await transcript.finish(
			`The passive forest exposed renderer process ${rendererProcess.processId}. Connecting the tree exposed WebContents ${renderer.targetId} and iframe ${iframe.targetId} while the loaded-source tree remained empty. Only after explicitly attaching ${iframe.targetId} did loaded sources appear, proving that process-tree discovery and debugger attachment are separate.`,
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

	async wait(explanation, durationMs) {
		const startedAt = performance.now();
		const gapMs = this.previousStepFinishedAt === undefined
			? 0
			: startedAt - this.previousStepFinishedAt;
		await new Promise((resolvePromise) => setTimeout(resolvePromise, durationMs));
		const finishedAt = performance.now();
		this.previousStepFinishedAt = finishedAt;
		this.stepNumber += 1;
		const content =
			`\n## Step ${this.stepNumber} — ${explanation}\n\n` +
			`_Timing: ${formatDuration(gapMs)} passed since the previous step; the wait took ${formatDuration(finishedAt - startedAt)}; ${formatDuration(finishedAt - this.startedAt)} elapsed since the transcript started._\n`;
		this.sections.push(content);
		process.stdout.write(content);
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
		["--json", "process", "list", "--root", "vscode", "--no-cmd-line"],
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

function selectCurrentRendererProcess(tree) {
	const processes = new Map(
		tree.processes.map((candidate) => [candidate.processId, candidate]),
	);
	let current = processes.get(process.pid);
	let windowId;
	while (current !== undefined) {
		if (Number.isSafeInteger(current.windowId)) {
			windowId = current.windowId;
			break;
		}
		current = processes.get(current.parentProcessId);
	}
	const workspaceName = process.cwd().split(/[\\/]/).at(-1)?.toLowerCase();
	return tree.processes.find(
		(candidate) =>
			candidate.role === "renderer" &&
			windowId !== undefined &&
			candidate.windowId === windowId,
	) ?? tree.processes.find(
		(candidate) =>
			candidate.role === "renderer" &&
			workspaceName !== undefined &&
			candidate.windowTitle?.toLowerCase().includes(workspaceName),
	) ?? tree.processes.find((candidate) => candidate.role === "renderer") ??
		assert.fail(`VS Code process tree ${tree.rootProcessId} has no renderer process`);
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

async function waitForVscodeIframe(environment, windowTitle) {
	return pollTargets(
		environment,
		":vscode-iframe-demo",
		(targets) => {
			const renderer = targets.find(
				(target) =>
					target.subtype === "electron-renderer" &&
					target.title === windowTitle,
			);
			return renderer !== undefined && targets.some(
				(target) =>
					target.targetType === "iframe" &&
					findRendererAncestor(targets, target)?.targetId === renderer.targetId,
			);
		},
		60_000,
	);
}

async function waitForLoadedSources(environment) {
	const output = await pollUntil(async () => {
		const result = await run(
			cli,
			["source", "tree", "loaded", "--all", "--context", ":vscode-iframe-demo"],
			environment,
			{ timeoutMs: 45_000 },
		);
		return result.code === 0 ? result.output : undefined;
	}, (value) => !value.includes("No loaded sources are currently observed"), 30_000);
	assert.ok(output, "the attached iframe did not contribute loaded sources");
	return output;
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

function selectVscodeIframe(targets, rendererTargetId) {
	const iframes = targets.filter(
		(target) =>
			target.targetType === "iframe" &&
			findRendererAncestor(targets, target)?.targetId === rendererTargetId,
	);
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
