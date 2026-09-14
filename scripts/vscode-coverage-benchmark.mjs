import assert from "node:assert/strict";
import { spawn } from "node:child_process";
import { createHash } from "node:crypto";
import { once } from "node:events";
import { access, appendFile, copyFile, mkdir, readFile, realpath, writeFile } from "node:fs/promises";
import { arch, cpus, platform, totalmem } from "node:os";
import { dirname, isAbsolute, join, relative, resolve } from "node:path";
import { fileURLToPath, pathToFileURL } from "node:url";
import { parseArgs } from "node:util";
import { allocatePort, findPageTargetId, readCdpEndpoint, run } from "../tests/playwright/live-test-harness.mjs";

export async function main(args = process.argv.slice(2)) {
	const { values } = parseArgs({
		args,
		options: {
			code: { type: "string" },
			output: { type: "string" },
			profile: { type: "string", default: "debug" },
			"skip-build": { type: "boolean", default: false },
			"timeout-ms": { type: "string", default: "300000" },
		},
	});
	assert(values.output, "--output <new-directory> is required");
	assert(["debug", "release"].includes(values.profile), "--profile must be debug or release");
	const timeoutMs = Number(values["timeout-ms"]);
	assert(Number.isSafeInteger(timeoutMs) && timeoutMs > 0, "--timeout-ms must be a positive integer");
	const root = fileURLToPath(new URL("..", import.meta.url));
	process.chdir(root);
	const output = resolve(values.output);
	await mkdir(dirname(output), { recursive: true });
	await mkdir(output);
	const runtime = join(output, "runtime");
	const logs = join(output, "logs");
	await mkdir(runtime);
	await mkdir(logs);
	const code = await findInstalledCode(values.code ?? process.env.DBGJS_CODE_EXECUTABLE);
	const timings = [];
	let step = 0;
	const environment = isolatedEnvironment(process.env);
	const command = async (label, executable, arguments_, extraEnvironment = environment) => {
		const number = ++step;
		console.log(`[${number}] ${label}`);
		const result = await run(executable, arguments_, extraEnvironment, { timeoutMs });
		const outputFile = join("logs", `${String(number).padStart(2, "0")}.txt`);
		await writeFile(join(output, outputFile), result.output);
		const entry = { label, executable, arguments: arguments_, durationMs: result.durationMs, exitCode: result.code, timedOut: result.timedOut, outputFile };
		timings.push(entry);
		await appendFile(join(output, "commands.jsonl"), `${JSON.stringify(entry)}\n`);
		console.log(`    ${(result.durationMs / 1000).toFixed(3)}s, exit ${result.code}`);
		assert.equal(result.code, 0, `${label} failed; see ${join(output, outputFile)}\n${result.output}`);
		return result.output;
	};
	const metadata = JSON.parse(await command("Cargo metadata", "cargo", ["metadata", "--no-deps", "--format-version", "1"]));
	const binaryDirectory = join(metadata.target_directory, values.profile);
	const suffix = process.platform === "win32" ? ".exe" : "";
	const cli = join(binaryDirectory, `dbgjs${suffix}`);
	const service = join(binaryDirectory, `dbgjs-service${suffix}`);
	const replay = join(binaryDirectory, "examples", `vscode_breadcrumbs${suffix}`);
	if (!values["skip-build"]) {
		await command("Build benchmark and dbgjs", "cargo", [
			"build", "--profile", values.profile === "debug" ? "dev" : "release",
			"--bin", "dbgjs", "--bin", "dbgjs-service", "--example", "vscode_breadcrumbs", "-j", "1",
		]);
	}
	await Promise.all([cli, service, replay].map((path) => access(path)));
	const revision = (await command("Record dbgjs revision", "git", ["rev-parse", "HEAD"])).trim();
	const workingTreeStatus = await command("Record worktree state", "git", ["status", "--porcelain"]);
	const compiler = (await command("Record Rust version", "rustc", ["--version"])).trim();
	const fixture = join(runtime, "fixture");
	await mkdir(fixture);
	const original = 'export function greet(name: string): string {\n    return `Hello, ${name}!`;\n}\n';
	const modified = 'export function greet(name: string, enthusiastic = false): string {\n    const displayName = name.trim() || "World";\n    const punctuation = enthusiastic ? "!" : ".";\n    return `Hello, ${displayName}${punctuation}`;\n}\n';
	await writeFile(join(fixture, "greeting.ts"), original);
	await writeFile(join(fixture, "greeting.test.ts"), 'import { greet } from "./greeting";\nconsole.log(greet("World"));\n');
	await command("Initialize disposable Git fixture", "git", ["-C", fixture, "init", "-b", "benchmark"]);
	await command("Stage fixture baseline", "git", ["-C", fixture, "add", "greeting.ts", "greeting.test.ts"]);
	await command("Commit fixture baseline", "git", [
		"-C", fixture, "-c", "user.name=dbgjs benchmark", "-c", "user.email=benchmark@example.invalid",
		"-c", "commit.gpgsign=false", "commit", "-m", "Initialize benchmark fixture",
	]);
	await writeFile(join(fixture, "greeting.ts"), modified);
	await writeFile(join(fixture, "greeting.test.ts"), 'import { greet } from "./greeting";\nconsole.log(greet("Ada", true));\n');
	const userData = join(runtime, "user-data");
	await mkdir(join(userData, "User"), { recursive: true });
	await writeFile(join(userData, "User", "settings.json"), JSON.stringify({
		"workbench.startupEditor": "none",
		"workbench.secondarySideBar.defaultVisibility": "hidden",
		"security.workspace.trust.enabled": false,
		"telemetry.telemetryLevel": "off",
		"update.mode": "none",
		"extensions.autoUpdate": false,
		"git.autofetch": false,
	}));
	const port = await allocatePort();
	const launchArguments = [
		"--new-window", "--locale=en", `--user-data-dir=${userData}`,
		`--extensions-dir=${join(runtime, "extensions")}`,
		`--shared-data-dir=${join(runtime, "shared")}`,
		`--agent-plugins-dir=${join(runtime, "agent-plugins")}`,
		`--agents-user-data-dir=${join(runtime, "agents-data")}`,
		`--agents-extensions-dir=${join(runtime, "agents-extensions")}`,
		`--remote-debugging-port=${port}`, "--disable-workspace-trust", fixture,
	];
	const child = spawn(code, launchArguments, { env: environment, stdio: ["ignore", "pipe", "pipe"] });
	const appOutput = [];
	child.stdout.on("data", (chunk) => appOutput.push(chunk));
	child.stderr.on("data", (chunk) => appOutput.push(chunk));
	const serviceEnvironment = {
		...environment,
		DBGJS_SERVICE_EXE: service,
		DBGJS_SERVICE_STATE: join(runtime, "service.json"),
		DBGJS_SOURCE_MAP_CACHE: join(runtime, "source-map-cache"),
	};
	const context = ":vscode-coverage-benchmark";
	let serviceStarted = false;
	let pageScope;
	const failures = [];
	try {
		await once(child, "spawn");
		const endpoint = await readCdpEndpoint(port);
		assert.equal(child.exitCode, null, "VS Code exited before attachment");
		serviceStarted = true;
		await command("Create isolated debugger context", cli, ["context", "create", context], serviceEnvironment);
		await command("Attach to installed VS Code", cli, [
			"connection", "add", endpoint, "--context", context, "--connection", "code", "--connect",
		], serviceEnvironment);
		const target = await findPageTargetId(cli, context, "code", "vscode-file:", serviceEnvironment);
		const scope = ["--context", context, "--connection", "code", "--target", target];
		pageScope = scope;
		const dbgjs = (label, arguments_) => command(label, cli, arguments_, serviceEnvironment);
		const playwright = (label, program) => dbgjs(label, ["playwright", program, ...scope]);
		await playwright("Dismiss first-run welcome and open Source Control", `
			await page.addLocatorHandler(
				page.getByRole("dialog", { name: "Welcome to Visual Studio Code", exact: true }),
				async dialog => { await dialog.getByRole("button", { name: "Close", exact: true }).click(); }
			);
			await page.getByRole("tab", { name: /^Source Control/ }).click();
			return await page.locator("body").ariaSnapshot();
		`);
		await playwright("Open multi-diff editor", `
			const changes = page.getByRole("treeitem", { name: "Changes", exact: true });
			await changes.waitFor();
			await changes.hover();
			await page.getByRole("button", { name: "Open Changes", exact: true }).click();
		`);
		await playwright("Wait for both rendered diffs", `
			await page.getByRole("tab", { name: /Git: Changes \\(2 files\\)/ }).waitFor();
			await page.getByRole("button", { name: "Revert Block", exact: true }).nth(1).waitFor();
		`);
		await dbgjs("Start coverage", ["coverage", "start", ...scope]);
		await playwright("Click the last Revert Block arrow", `
			await page.getByRole("button", { name: "Revert Block", exact: true }).last().click();
		`);
		const unprojected = JSON.parse(await dbgjs("Capture without projection", [
			"--json", "coverage", "capture", "--id", "unprojected", ...scope,
		]));
		const projected = JSON.parse(await dbgjs("Capture with mapping and breadcrumbs", [
			"--json", "coverage", "capture", ...scope,
		]));
		const { source, lookups, revert } = extractWorkload(projected);
		await dbgjs("Explain mapped source provenance", ["source", "explain", source.generatedUrl, "--context", context]);
		await dbgjs("Read mapped TypeScript", [
			"source", "show", revert.authoredLocation.sourceUrl,
			"--line", String(revert.authoredLocation.line), "--context-lines", "8", "--context", context,
		]);
		await dbgjs("Resolve mapped TypeScript position", [
			"source", "map", revert.authoredLocation.sourceUrl, String(revert.authoredLocation.line),
			String(revert.authoredLocation.column), "--context", context,
		]);
		const bundle = await installedBundlePath(source.generatedUrl, code);
		const bundleBytes = await readFile(bundle);
		const appDirectory = resolve(dirname(bundle), "..", "..", "..");
		const product = JSON.parse(await readFile(join(appDirectory, "product.json"), "utf8"));
		const packageInfo = JSON.parse(await readFile(join(appDirectory, "package.json"), "utf8"));
		assert.equal(typeof product.commit, "string", "Installed VS Code must identify its build commit");
		const workload = {
			version: 1, sourceFile: "workbench.desktop.main.js",
			sourceSha256: sha256(bundleBytes), vscodeCommit: product.commit, lookups,
		};
		await copyFile(bundle, join(output, workload.sourceFile));
		await writeFile(join(output, "workload.json"), `${JSON.stringify(workload, null, 2)}\n`);
		await writeFile(join(output, "capture-report.json"), `${JSON.stringify({
			version: 1, vscode: { executable: code, version: packageInfo.version, commit: product.commit, sourceUrl: source.generatedUrl },
			dbgjs: { revision, workingTreeStatus, profile: values.profile, compiler, binarySha256: sha256(await readFile(cli)), serviceSha256: sha256(await readFile(service)), replaySha256: sha256(await readFile(replay)) },
			machine: { platform: platform(), architecture: arch(), cpu: cpus()[0]?.model, logicalCpus: cpus().length, memoryBytes: totalmem() },
			functions: source.functions.length, lookupCount: lookups.length,
			unprojectedFunctions: unprojected.sources.flatMap((entry) => entry.functions).length,
			revert: { name: revert.name, authoredLocation: revert.authoredLocation, ranges: revert.ranges },
			launchArguments,
		}, null, 2)}\n`);
		await dbgjs("Stop coverage", ["coverage", "stop", ...scope]);
	} catch (error) {
		failures.push(error);
		if (pageScope) {
			try {
				await command("Capture failure UI diagnostics", cli, [
					"playwright", 'return await page.locator("body").ariaSnapshot();', ...pageScope,
				], serviceEnvironment);
			} catch (diagnosticError) {
				failures.push(diagnosticError);
			}
		}
	} finally {
		if (serviceStarted) {
			try {
				await command("Stop owned debugger service", cli, ["service", "stop"], serviceEnvironment);
			} catch (error) {
				failures.push(error);
			}
		}
		try {
			await stopOwnedCode(child, environment);
		} catch (error) {
			failures.push(error);
		}
		await writeFile(join(logs, "vscode.txt"), Buffer.concat(appOutput));
	}
	if (failures.length) throw new AggregateError(failures, "VS Code workload capture failed");
	const baseline = await command("Replay frozen workload with VS Code stopped", replay, [join(output, "workload.json")]);
	JSON.parse(baseline);
	await writeFile(join(output, "replay-baseline.json"), baseline);
	await writeFile(join(output, "timings.json"), `${JSON.stringify(timings, null, 2)}\n`);
	console.log(`Baseline ready: ${output}`);
}

export function isolatedEnvironment(environment) {
	return Object.fromEntries(Object.entries(environment).map(([key, value]) => [
		key, /^GIT_CONFIG_(COUNT|KEY_\d+|VALUE_\d+)$/i.test(key) || ["ELECTRON_RUN_AS_NODE", "VSCODE_PORTABLE"].includes(key.toUpperCase()) ? undefined : value,
	]));
}

export function extractWorkload(snapshot) {
	const sources = snapshot.sources.filter((source) => source.generatedUrl.endsWith("/workbench.desktop.main.js"));
	assert.equal(sources.length, 1, "Expected one installed workbench bundle");
	const source = sources[0];
	const revert = source.functions.find((fn) => fn.name === "revertRangeMappings" && fn.ranges.some((range) => range.count > 0));
	assert(revert?.authoredLocation?.sourceUrl.endsWith("/diffEditorWidget.ts"), "Revert must execute and map to TypeScript");
	const lookups = source.functions
		.filter((fn) => !fn.authoredLocation && fn.generatedLocation)
		.map((fn) => {
			const { line, column } = fn.generatedLocation;
			assert(Number.isSafeInteger(line) && line > 0 && Number.isSafeInteger(column) && column > 0, "Expected positive generated coordinates");
			return { line, column };
		});
	assert(lookups.length > 0, "No generated-fallback lookups captured; this build did not reproduce the workload");
	return { source, lookups, revert };
}

export async function installedBundlePath(sourceUrl, executable) {
	const url = new URL(sourceUrl);
	assert(["file:", "vscode-file:"].includes(url.protocol), "Expected a local installed bundle");
	assert(["", "vscode-app"].includes(url.hostname), "Expected a local bundle URL");
	const bundle = await realpath(fileURLToPath(new URL(`file://${url.pathname}`)));
	const installation = await realpath(process.platform === "darwin" ? resolve(dirname(executable), "..") : dirname(executable));
	const path = relative(installation, bundle);
	assert(!path.startsWith("..") && !isAbsolute(path), "Bundle must belong to the selected VS Code installation");
	return bundle;
}

async function findInstalledCode(explicit) {
	const candidates = explicit ? [resolve(explicit)] : process.platform === "win32"
		? [process.env.LOCALAPPDATA, process.env.PROGRAMFILES].filter(Boolean).flatMap((directory) => [
			join(directory, "Programs", "Microsoft VS Code", "Code.exe"),
			join(directory, "Microsoft VS Code", "Code.exe"),
		])
		: process.platform === "darwin"
			? ["/Applications/Visual Studio Code.app/Contents/MacOS/Electron"]
			: ["/usr/share/code/code", "/usr/lib/code/code"];
	for (const candidate of candidates) {
		try {
			await access(candidate);
			return await realpath(candidate);
		} catch (error) {
			if (error.code !== "ENOENT") throw error;
		}
	}
	throw new Error(`Installed VS Code not found. Pass --code <native-executable>.\nChecked: ${candidates.join(", ")}`);
}

async function stopOwnedCode(child, environment) {
	if (child.pid === undefined || child.exitCode !== null || child.signalCode !== null) return;
	const exited = once(child, "exit");
	if (process.platform === "win32") {
		const result = await run("taskkill", ["/PID", String(child.pid), "/T", "/F"], environment, { timeoutMs: 15_000 });
		assert.equal(result.code, 0, `Could not stop owned VS Code process ${child.pid}: ${result.output}`);
	} else {
		child.kill("SIGTERM");
	}
	await Promise.race([
		exited,
		new Promise((_, reject) => {
			const timer = setTimeout(() => reject(new Error(`Owned VS Code process ${child.pid} did not exit`)), 15_000);
			timer.unref();
		}),
	]);
}

function sha256(bytes) {
	return createHash("sha256").update(bytes).digest("hex");
}

if (process.argv[1] && import.meta.url === pathToFileURL(resolve(process.argv[1])).href) {
	await main();
}
