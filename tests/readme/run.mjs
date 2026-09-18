import assert from "node:assert/strict";
import { spawn } from "node:child_process";
import { once } from "node:events";
import { mkdir, mkdtemp, readFile, rm, writeFile } from "node:fs/promises";
import { tmpdir } from "node:os";
import { dirname, join, resolve } from "node:path";
import { setTimeout as delay } from "node:timers/promises";
import { parseArgs } from "node:util";
import { downloadAndUnzipVSCode } from "@vscode/test-electron";
import { run } from "../playwright/live-test-harness.mjs";
import { resolveCodeExecutable } from "../vscode-discovery/launch.mjs";
import { checkGeneratedDocs, compareRecordings, saveReadme } from "./transcript.mjs";
import { createRecorder } from "./recorder.mjs";
import { recordConnections } from "./connections.mjs";

const { values } = parseArgs({ options: {
	update: { type: "boolean", default: false },
	"bin-dir": { type: "string", default: "target/debug" },
	output: { type: "string", default: "artifacts/readme" },
} });
assert.equal(process.platform, "win32", "The recorded desktop walkthrough currently runs on Windows.");
const expected = values.update ? undefined : await checkGeneratedDocs();
const vscodeVersion = "1.137.0";
const output = resolve(values.output);
await mkdir(output, { recursive: true });
const code = await resolveCodeExecutable(await downloadAndUnzipVSCode({
	version: vscodeVersion, cachePath: resolve("artifacts/vscode-download"),
}));
const directory = await mkdtemp(join(tmpdir(), "dbgjs-readme-"));
const cli = resolve(values["bin-dir"], "dbgjs.exe");
const service = resolve(values["bin-dir"], "dbgjs-service.exe");
const profile = join(directory, "profile");
const workspace = join(directory, "readme-demo");
const environment = {
	DBGJS_SERVICE_EXE: service,
	DBGJS_SERVICE_STATE: join(directory, "service.json"),
	DBGJS_SOURCE_MAP_CACHE: resolve("artifacts/readme-source-maps"),
};
const replacements = [
	[directory, "DEMO_DIR"], [output, "ARTIFACTS"],
	[`\\\\?\\${output}`, "ARTIFACTS"],
	[dirname(code), "VSCODE_INSTALL"],
	[dirname(code).toLowerCase(), "VSCODE_INSTALL"],
	[dirname(code).toLowerCase().replaceAll("\\", "/"), "VSCODE_INSTALL"],
	[dirname(code).toLowerCase().replaceAll("\\", "\\\\"), "VSCODE_INSTALL"],
];
const { steps, command, json } = await createRecorder({
	cli, environment, output, name: "vscode", replacements,
});
const recording = { vscodeVersion, steps };
let codeProcess;
let codeExited;
let codeOutput = "";
let serviceUsed = false;
try {
	await mkdir(workspace);
	await mkdir(join(profile, "User"), { recursive: true });
	await writeFile(join(profile, "User", "settings.json"), JSON.stringify({
		"workbench.startupEditor": "none",
		"security.workspace.trust.enabled": false,
		"telemetry.telemetryLevel": "off",
		"update.mode": "none",
		"extensions.autoUpdate": false,
		"extensions.autoCheckUpdates": false,
		"editor.accessibilitySupport": "off",
		"editor.minimap.enabled": false,
	}));
	const codeEnvironment = { ...process.env };
	for (const key of Object.keys(codeEnvironment)) {
		if (/^(DBGJS_|VSCODE_|ELECTRON_RUN_AS_NODE$|NODE_OPTIONS$)/.test(key)) delete codeEnvironment[key];
	}
	codeProcess = spawn(code, [
		"--new-window", "--locale=en", "--skip-welcome", "--skip-release-notes",
		"--disable-workspace-trust", "--disable-updates", "--disable-gpu",
		"--disable-extensions", "--remote-debugging-port=0",
		`--user-data-dir=${profile}`, `--extensions-dir=${join(directory, "extensions")}`,
		workspace,
	], { env: codeEnvironment, stdio: ["ignore", "pipe", "pipe"] });
	codeProcess.stdout.on("data", (chunk) => { codeOutput += chunk; });
	codeProcess.stderr.on("data", (chunk) => { codeOutput += chunk; });
	codeExited = new Promise((resolveExit) => codeProcess.once("exit", resolveExit));
	await once(codeProcess, "spawn");
	replacements.push([String(codeProcess.pid), "VSCODE_PID"]);
	let tree;
	let renderer;
	await poll("discovering the isolated renderer", async () => {
		const trees = await json(["process", "list", "--root", "vscode", "--no-cmd-line"]);
		tree = trees.find((candidate) => candidate.rootProcessId === codeProcess.pid);
		renderer = tree?.processes.find((candidate) =>
			candidate.role === "renderer" && candidate.windowTitle?.includes("readme-demo"));
		return Boolean(renderer);
	});
	let processNumber = 0;
	for (const process of tree.processes) {
		if (process.processId !== codeProcess.pid) {
			replacements.push([String(process.processId),
				process.processId === renderer.processId ? "RENDERER_PID" : `PROCESS_${++processNumber}`]);
		}
	}
	await command("discover", [
		"process", "list", "--root", "vscode", "--no-cmd-line", "--filter", `p:${codeProcess.pid}`,
	], "live", (text) => {
		assert.match(text, /renderer/);
		assert.match(text, /readme-demo/);
		for (const [, pid] of text.matchAll(/\bp:(\d+)\b/g)) {
			if (!replacements.some(([value]) => value === pid)) {
				replacements.push([pid, `PROCESS_${++processNumber}`]);
			}
		}
	});
	serviceUsed = true;
	await command("context", ["context", "create", ":readme", "VS Code typing", "--set"], "exact",
		(text) => assert.match(text, /readme/));
	await command("attach", ["process", "attach", String(renderer.processId), "--set"], "live",
		(text) => {
			const attached = text.match(/^Target (renderer-\d+)/m);
			assert.ok(attached, "PID attachment must select the isolated renderer.");
			replacements.push([attached[1], "RENDERER_TARGET"]);
		});
	const target = await json(["target", "show"]);
	assert.match(target.target.targetId, /^renderer-\d+$/);
	await command("evaluate", ["target", "eval", "document.title"], "exact",
		(text) => assert.match(text, /readme-demo - Visual Studio Code/));
	await command("playwright", [
		"playwright", 'await page.locator(".monaco-workbench").click();\nawait page.keyboard.press("Control+n");\nawait page.locator(".editor-group-container .monaco-editor").waitFor();\nreturn await page.title();',
	], "exact", (text) => assert.match(JSON.parse(text), /Untitled-1.*readme-demo/));
	await command("source", [
		"source", "grep", "private _doApplyEdits", "--path", "textModel.ts", "--max-results", "1", "--context-lines", "2",
	], "excerpt", (text) => {
		assert.match(text, /textModel\.ts/);
		assert.match(text, /_doApplyEdits/);
		assert.doesNotMatch(text, /0 source\(s\) searched/);
		const prefix = text.match(/^(https:.*\/src\/vs\/)/m);
		assert.ok(prefix, "Source grep must return the authored VS Code source URL.");
		replacements.push([prefix[1], "VSCODE_SOURCE/"]);
		replacements.push([prefix[1].replace("https://", "https:/"), "VSCODE_SOURCE/"]);
	}, { maxOutputLines: 7 });
	const search = await json([
		"source", "grep", "private _doApplyEdits", "--path", "textModel.ts", "--max-results", "1", "--context-lines", "2",
	]);
	await command("coverage-start", ["coverage", "start"], "exact",
		(text) => assert.match(text, /Coverage recording started/));
	await command("coverage-baseline", ["coverage", "capture", "--id", "background"], "exact",
		(text) => assert.match(text, /Captured background/));
	await command("coverage-type", ["target", "type", "hello from dbgjs"], "exact",
		(text) => assert.match(text, /Typed/));
	await command("coverage-stop", ["coverage", "stop", "--id", "typing", "--exclude", "background"], "exact",
		(text) => assert.match(text, /Captured typing/));
	await command("coverage-show", [
		"coverage", "show", "typing", "--path-prefix", "src/vs/editor/common/model", "--max-lines", "16",
	], "live", (text) => {
		assert.match(text, /\d+ RL \(run lines\), \d+ HL \(hit lines\)/);
		assert.match(text, /textModel\.ts/);
		assert.ok(text.trimEnd().split(/\r?\n/).length <= 16);
	});
	const coverage = await json(["coverage", "show", "typing"]);
	assert.ok(coverage.sources.flatMap((source) => source.functions)
		.flatMap((fn) => fn.ranges).some((range) => range.count > 0 && range.authoredStart),
		"Typing must produce executed source-mapped coverage, not just a successful command.");
	const hit = search.matches.find((match) => match.text.includes("private _doApplyEdits"));
	assert.ok(hit, "The source search must discover the real method definition.");
	const sourceUrl = hit.path.slice(hit.path.indexOf("src/vs/"));
	const firstStatement = hit.afterContext.findIndex((line) => line.includes("const oldLineCount"));
	assert.ok(firstStatement >= 0, "The source excerpt must expose the first executable statement.");
	const bodyLine = hit.line + firstStatement + 1;
	await command("breakpoint-set", [
		"breakpoint", "set", "edit", sourceUrl, String(bodyLine),
	], "live", (text) => assert.match(text, /\[bound; 1 application\(s\)\]/), { maxOutputLines: 12 });
	const beforePause = await json(["target", "show"]);
	await command("breakpoint-type", ["target", "type", "!"], "exact",
		(text) => assert.match(text, /Typed/));
	await command("breakpoint-wait", [
		"target", "wait", "paused", String(beforePause.pause?.epoch ?? 0), "30000",
	], "live", (text) => {
		assert.match(text, /_doApplyEdits/);
		assert.match(text, /textModel\.ts/);
	}, { maxOutputLines: 15 });
	await command("breakpoint-eval", ["target", "eval", "this.getValue()"], "exact",
		(text) => assert.equal(text.trim(), "hello from dbgjs"));
	await command("resume", ["target", "resume"], "live",
		(text) => assert.match(text, /running/));
	await command("breakpoint-delete", ["breakpoint", "delete", "edit"], "live",
		(text) => assert.match(text, /Context readme/), { maxOutputLines: 8 });
	await command("profile-start", ["profile", "start", "--sampling-interval", "1ms"], "exact",
		(text) => assert.match(text, /1000us/));
	await command("profile-type", [
		"playwright", 'await page.keyboard.type(" profiling the editor".repeat(20));',
	], "exact", (text) => assert.equal(text.trim(), ""));
	await command("profile-stop", ["profile", "stop", "--id", "typing-cpu"], "exact",
		(text) => assert.match(text, /Captured typing-cpu/));
	await command("profile-show", [
		"profile", "show", "typing-cpu", "--view", "functions", "--sort", "self", "--path", "src/vs/editor", "--max-lines", "8",
	], "live", (text) => {
		assert.match(text, /Self\s+Total\s+Samples\s+Function/);
		assert.match(text, /[1-9]\d* samples/);
		assert.match(text, /src\/vs\/editor\/.*\.ts:/);
		assert.ok(text.trimEnd().split(/\r?\n/).length <= 8);
	});
	const profileData = await json(["profile", "show", "typing-cpu"]);
	assert.ok(profileData.samples.length > 0);
	assert.ok(profileData.functions.some((fn) => fn.authoredLocation?.sourceUrl),
		"The CPU profile must contain mapped authored frames.");
	await command("screenshot", [
		"screenshot", "capture", "--output", join(output, "editor.png"),
	], "live", (text) => assert.match(text, /Captured \d+x\d+ screenshot/));
	const png = await readFile(join(output, "editor.png"));
	assert.equal(png.subarray(0, 8).toString("hex"), "89504e470d0a1a0a");
	await command("raw-cdp", [
		"target", "cdp", "Runtime.evaluate", "--params", '{"expression":"document.title","returnByValue":true}',
	], "exact", (text) => assert.match(JSON.parse(text).result.value, /Untitled-1.*readme-demo/));
	await command("heap-capture", ["heap", "capture", "--id", "editor"], "live",
		(text) => assert.match(text, /Captured editor/));
	let bufferReference;
	await command("heap-classes", [
		"heap", "classes", "editor", "--filter", "^PieceTreeTextBuffer$", "--instances", "--max-lines", "16",
	], "live", (text) => {
		assert.match(text, /PieceTreeTextBuffer@\d+\s+id \d+/);
		assert.match(text, /pieceTreeTextBuffer\.ts/);
		assert.ok(text.trimEnd().split(/\r?\n/).length <= 16);
		const instance = text.match(/PieceTreeTextBuffer@\d+\s+id (\d+)/);
		bufferReference = `editor#${instance[1]}`;
		replacements.push([bufferReference, "BUFFER"]);
	}, { foldMappingDiagnostics: true });
	await command("heap-refs", [
		"heap", "refs", bufferReference, "--incoming", "--limit", "4",
	], "live", (text) => {
		assert.ok(text.includes(bufferReference));
		assert.match(text, /property/);
	});
	await command("disconnect", [
		"connection", "disconnect", "--connection", target.target.connectionId,
	], "live", (text) => assert.match(text, /disconnected/));
	await command("offline-coverage", [
		"coverage", "show", "typing", "--path-prefix", "src/vs/editor/common/model", "--max-lines", "8",
	], "live", (text) => assert.match(text, /src\/vs\/editor\/common\/model/));
	const offline = await json(["coverage", "show", "typing"]);
	assert.deepEqual(offline.sources, coverage.sources, "Disconnecting must preserve the coverage evidence.");
} finally {
	await writeFile(join(output, "recording.json"), JSON.stringify(recording, null, 2) + "\n");
	await writeFile(join(output, "vscode.log"), codeOutput);
	try {
		if (serviceUsed) {
			const stopped = await run(cli, ["service", "stop"], environment, { timeoutMs: 20_000 });
			assert.equal(stopped.code, 0, stopped.output);
		}
	} finally {
		try {
			if (codeProcess?.pid && codeProcess.exitCode === null && codeProcess.signalCode === null) {
				const killed = await run("taskkill", ["/PID", String(codeProcess.pid), "/T", "/F"], {}, { timeoutMs: 20_000 });
				assert.equal(killed.code, 0, killed.output);
				await codeExited;
			}
		} finally {
			await rm(directory, { recursive: true, force: true, maxRetries: 30, retryDelay: 200 });
		}
	}
}

recording.steps.push(...(await recordConnections({ cli, service, output })).steps);
await writeFile(join(output, "recording.json"), JSON.stringify(recording, null, 2) + "\n");
if (values.update) {
	await saveReadme(recording);
	console.log("Generated README and walkthroughs from successful live CLI recordings.");
} else {
	compareRecordings(recording, expected);
	console.log("The README examples and walkthroughs still work.");
}

async function poll(label, operation) {
	const deadline = Date.now() + 90_000;
	do {
		assert.equal(codeProcess.exitCode, null, `VS Code exited while ${label}:\n${codeOutput}`);
		if (await operation()) return;
		await delay(300);
	} while (Date.now() < deadline);
	throw new Error(`Timed out ${label}. See ${output} for diagnostics.`);
}
