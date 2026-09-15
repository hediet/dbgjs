import assert from "node:assert/strict";
import { spawn } from "node:child_process";
import { once } from "node:events";
import { access, appendFile, copyFile, mkdir, mkdtemp, readFile, rm, writeFile } from "node:fs/promises";
import { tmpdir } from "node:os";
import { join, resolve } from "node:path";
import { setTimeout as delay } from "node:timers/promises";
import { fileURLToPath } from "node:url";
import { parseArgs } from "node:util";
import { downloadAndUnzipVSCode } from "@vscode/test-electron";
import { transform } from "esbuild";
import { allocatePort, run } from "../playwright/live-test-harness.mjs";
import { breakpointResult, discoveryResult, pauseResult } from "./transcript.mjs";

const vscodeVersion = "1.137.0";
const authoredUrl = "dbgjs-fixture:///fixture.ts";
const fixtureSource = await readFile(new URL("./fixture.ts", import.meta.url), "utf8");
const { values } = parseArgs({ options: {
	"bin-dir": { type: "string", default: "target/release" },
	output: { type: "string", default: "artifacts/vscode-discovery" },
	update: { type: "boolean", default: false },
} });
const output = resolve(values.output);
await mkdir(output, { recursive: true });
await writeFile(join(output, "commands.jsonl"), "");
await writeFile(join(output, "actual.json"), "[]\n");
const code = await downloadAndUnzipVSCode({
	version: vscodeVersion,
	cachePath: resolve("artifacts/vscode-download"),
});
const directory = await mkdtemp(join(tmpdir(), "dbgjs-vscode-e2e-"));
const suffix = process.platform === "win32" ? ".exe" : "";
const cli = join(directory, `dbgjs${suffix}`);
const service = join(directory, `dbgjs-service${suffix}`);
const transcript = [];
const environment = { ...process.env };
for (const key of Object.keys(environment)) {
	if (/^(DBGJS_|VSCODE_|ELECTRON_RUN_AS_NODE$|NODE_OPTIONS$)/.test(key)) delete environment[key];
}
Object.assign(environment, {
	DBGJS_SERVICE_EXE: service,
	DBGJS_SERVICE_STATE: join(directory, "service.json"),
	DBGJS_SOURCE_MAP_CACHE: join(directory, "maps"),
	DBGJS_NODE: process.execPath,
});
let codeProcess;
let codeExited;
let serviceUsed = false;
let codeOutput = "";
try {
	await copyFile(resolve(values["bin-dir"], `dbgjs${suffix}`), cli);
	await copyFile(resolve(values["bin-dir"], `dbgjs-service${suffix}`), service);
	const extension = join(directory, "extension");
	const userData = join(directory, "profile");
	const workspace = join(directory, "workspace");
	await mkdir(extension);
	await mkdir(workspace);
	await mkdir(join(userData, "User"), { recursive: true });
	await writeFile(join(userData, "User", "settings.json"), JSON.stringify({
		"workbench.startupEditor": "none",
		"security.workspace.trust.enabled": false,
		"telemetry.telemetryLevel": "off",
		"update.mode": "none",
		"extensions.autoUpdate": false,
		"extensions.autoCheckUpdates": false,
	}));
	const compiled = await transform(fixtureSource, {
		loader: "ts", format: "cjs", target: "es2022",
		sourcemap: "external", sourcefile: authoredUrl,
	});
	await writeFile(join(extension, "extension.cjs"), compiled.code + "\n//# sourceMappingURL=extension.cjs.map\n");
	await writeFile(join(extension, "extension.cjs.map"), compiled.map);
	await writeFile(join(extension, "package.json"), JSON.stringify({
		name: "dbgjs-discovery-fixture", publisher: "dbgjs-test", version: "0.0.0",
		engines: { vscode: "^1.95.0" }, main: "./extension.cjs",
		activationEvents: ["*"], extensionKind: ["workspace"],
	}));
	const readyPath = join(directory, "ready.json");
	const inspectorPort = await allocatePort();
	codeProcess = spawn(code, [
		"--new-window", "--locale=en", "--skip-welcome", "--skip-release-notes",
		"--disable-workspace-trust", "--disable-updates", "--disable-gpu",
		"--no-sandbox", "--disable-dev-shm-usage",
		`--user-data-dir=${userData}`,
		`--extensions-dir=${join(directory, "extensions")}`,
		`--extensionDevelopmentPath=${extension}`,
		`--inspect-extensions=${inspectorPort}`,
		workspace,
	], {
		env: { ...environment, DBGJS_VSCODE_FIXTURE_READY: readyPath },
		stdio: ["ignore", "pipe", "pipe"],
		detached: process.platform !== "win32",
	});
	codeProcess.stdout.on("data", (chunk) => { codeOutput += chunk; });
	codeProcess.stderr.on("data", (chunk) => { codeOutput += chunk; });
	codeExited = new Promise((resolve) => codeProcess.once("exit", resolve));
	await once(codeProcess, "spawn");
	await poll("fixture extension activation", async () => {
		try {
			await access(readyPath);
			return true;
		} catch (error) {
			if (error.code !== "ENOENT") throw error;
			return false;
		}
	});
	const ready = JSON.parse(await readFile(readyPath, "utf8"));
	assert.ok(Number.isSafeInteger(ready.pid) && ready.pid > 0);
	let tree;
	let extensionHost;
	await poll("VS Code process discovery", async () => {
		const forests = await command(["process", "list", "--root", "vscode", "--no-cmd-line"]);
		tree = forests.find((forest) => forest.rootProcessId === codeProcess.pid);
		extensionHost = tree?.processes.find((process) => process.role === "extension-host");
		return Boolean(extensionHost);
	});
	assert.equal(extensionHost.processId, ready.pid, "Discovery must select the fixture's real extension host.");
	transcript.push({ command: "process list --root vscode", result: discoveryResult(tree, extensionHost) });

	serviceUsed = true;
	await command(["context", "create", "--context", ":vscode-discovery-e2e", "--set"]);
	await command(["process", "attach", String(extensionHost.processId), "--context", ":vscode-discovery-e2e", "--set"]);
	const identity = await command(["target", "eval", "process.pid"]);
	assert.equal(identity.preview.kind, "number");
	assert.equal(identity.preview.preview, String(ready.pid), "Process attachment must reach the discovered extension host.");
	const evaluation = await command(["target", "eval", "globalThis.__dbgjsVscodeFixture.compute(20)"]);
	transcript.push({ command: "target eval compute(20)", result: evaluation.preview });
	assert.equal(evaluation.preview.preview, "41");

	const line = fixtureSource.split("\n").findIndex((text) => text.includes("return doubled + 1")) + 1;
	assert.ok(line > 0);
	await command(["breakpoint", "set", "fixture", authoredUrl, String(line), "--column", "2"]);
	await command(["target", "wait", "breakpoint-installed", "fixture", "30000"]);
	const context = await command(["context", "show"]);
	const breakpoint = context.breakpoints.find((candidate) => candidate.id === "fixture");
	assert.ok(breakpoint);
	transcript.push({ command: "breakpoint set fixture", result: breakpointResult(breakpoint) });
	const source = await command(["source", "show", authoredUrl]);
	assert.equal(source.content.replaceAll("\r\n", "\n"), fixtureSource.replaceAll("\r\n", "\n"));
	transcript.push({ command: "source show fixture.ts", result: { path: source.path, content: source.content.replaceAll("\r\n", "\n") } });
	const before = await command(["target", "show"]);
	await command(["target", "eval", "(() => { setTimeout(() => { globalThis.__dbgjsVscodeFixture.result = globalThis.__dbgjsVscodeFixture.compute(20); }, 250); return 'scheduled'; })()"]);
	const paused = await command(["target", "wait", "paused", String(before.pause?.epoch ?? 0), "30000"]);
	transcript.push({ command: "target wait paused", result: pauseResult(paused) });
	assert.equal(paused.pause.frames[0].projected.location.sourceUrl, authoredUrl);
	assert.equal(paused.pause.frames[0].projected.location.line, line);
	const local = await command(["target", "eval", "doubled"]);
	assert.equal(local.preview.preview, "40");
	transcript.push({ command: "target eval doubled", result: local.preview });
	await command(["target", "resume"]);
	let completed;
	await poll("fixture result after resume", async () => {
		completed = await command(["target", "eval", "globalThis.__dbgjsVscodeFixture.result"]);
		return completed.preview.preview === "41";
	});
	transcript.push({ command: "target eval result", result: completed.preview });
	const actual = JSON.stringify(transcript, null, 2) + "\n";
	await writeFile(join(output, "actual.json"), actual);
	const goldenPath = fileURLToPath(new URL("./expected.json", import.meta.url));
	if (values.update) {
		await writeFile(goldenPath, actual);
	} else {
		assert.deepEqual(transcript, JSON.parse(await readFile(goldenPath, "utf8")),
			"VS Code discovery/source-map transcript changed; inspect actual.json before explicitly updating the golden.");
	}
	console.log(`VS Code ${vscodeVersion} discovery, authored breakpoint, mapped pause, and evaluation passed.`);
} finally {
	await writeFile(join(output, "actual.json"), JSON.stringify(transcript, null, 2) + "\n");
	await writeFile(join(output, "vscode.log"), codeOutput);
	try {
		if (serviceUsed) {
			const stopped = await run(cli, ["service", "stop"], environment, { timeoutMs: 15_000 });
			assert.equal(stopped.code, 0, stopped.output);
		}
	} finally {
		try {
			if (codeProcess?.pid && codeProcess.exitCode === null && codeProcess.signalCode === null) {
				if (process.platform === "win32") {
					const killed = await run("taskkill", ["/PID", String(codeProcess.pid), "/T", "/F"], {}, { timeoutMs: 15_000 });
					assert.equal(killed.code, 0, killed.output);
				} else {
					process.kill(-codeProcess.pid, "SIGKILL");
				}
				await codeExited;
			}
		} finally {
			await rm(directory, { recursive: true, force: true, maxRetries: 30, retryDelay: 200 });
		}
	}
}

async function command(args) {
	const result = await run(cli, ["--json", ...args], environment, { timeoutMs: 60_000 });
	await appendFile(join(output, "commands.jsonl"), JSON.stringify({ args, ...result }) + "\n");
	assert.equal(result.code, 0, `${args.join(" ")}:\n${result.output}`);
	return JSON.parse(result.stdout);
}

async function poll(label, operation) {
	const deadline = Date.now() + 60_000;
	do {
		assert.equal(codeProcess.exitCode, null, `VS Code exited while waiting for ${label}:\n${codeOutput}`);
		if (await operation()) return;
		await delay(200);
	} while (Date.now() < deadline);
	throw new Error(`Timed out waiting for ${label}. See ${output} for diagnostics.\n${codeOutput}`);
}
