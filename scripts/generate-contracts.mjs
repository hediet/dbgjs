import { existsSync, readFileSync } from "node:fs";
import { spawnSync } from "node:child_process";
import { createRequire } from "node:module";
import { dirname, resolve } from "node:path";
import { fileURLToPath } from "node:url";
import { deepStrictEqual } from "node:assert";

const repositoryRoot = resolve(dirname(fileURLToPath(import.meta.url)), "..");
const check = process.argv.slice(2).includes("--check");
const unexpected = process.argv.slice(2).filter((argument) => argument !== "--check");
if (unexpected.length > 0) {
	throw new Error(`Unexpected arguments: ${unexpected.join(" ")}`);
}

run("cargo", [
	"run",
	"--locked",
	"--quiet",
	"--bin",
	"export_contracts",
	"--",
	...(check ? ["--check"] : []),
]);

const require = createRequire(import.meta.url);
const packagePath = require.resolve("@hediet/linkrpc-cli/package.json");
const { bin } = JSON.parse(readFileSync(packagePath, "utf8"));
const cliEntry = typeof bin === "string" ? bin : bin?.linkrpc;
if (typeof cliEntry !== "string") {
	throw new Error("The installed @hediet/linkrpc-cli package does not declare a linkrpc executable");
}
const linkrpc = resolve(dirname(packagePath), cliEntry);
if (!existsSync(linkrpc)) {
	throw new Error(
		`The installed LinkRPC CLI is missing at ${linkrpc}. Run npm ci; for a local development override, build the linked CLI first.`,
	);
}

const interfaces = [
	["dev.dbgjs.cdp-debugger", "ServiceApi"],
	["dev.dbgjs.context", "ContextApi"],
	["dev.dbgjs.source", "SourceApi"],
	["dev.dbgjs.capture", "CaptureApi"],
	["dev.dbgjs.target-debugger", "TargetDebuggerApi"],
	["dev.dbgjs.cdp-access", "CdpAccessApi"],
	["dev.dbgjs.relay", "RelayApi"],
	["dev.dbgjs.browser-automation", "BrowserAutomationApi"],
	["dev.dbgjs.coverage", "CoverageApi"],
	["dev.dbgjs.cpu-profiler", "CpuProfilerApi"],
	["dev.dbgjs.heap-profiler", "HeapProfilerApi"],
];

const bundle = JSON.parse(readFileSync(resolve(repositoryRoot, "schemas/dbgjs.interfaces.json"), "utf8"));
deepStrictEqual(
	interfaces.map(([id]) => id).sort(),
	bundle.services.flatMap(service => service.interfaces.map(reference => reference.interfaceId)).sort(),
	"TypeScript generation must cover every advertised daemon interface exactly once",
);

for (const [id, name] of interfaces) {
	const filename = name[0].toLowerCase() + name.slice(1);
	run(process.execPath, [
		linkrpc,
		"codegen",
		"--input",
		"schemas/dbgjs.interfaces.json",
		"--interface",
		id,
		"--name",
		name,
		"--output",
		`vscode-extension/src/generated/${filename}.ts`,
		"--preserve-wire-schema",
		...(check ? ["--check"] : []),
	]);
}

function run(command, args) {
	const result = spawnSync(command, args, {
		cwd: repositoryRoot,
		stdio: "inherit",
	});
	if (result.error) {
		throw result.error;
	}
	if (result.status !== 0) {
		process.exit(result.status ?? 1);
	}
}
