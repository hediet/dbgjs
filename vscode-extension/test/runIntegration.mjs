import { spawn, spawnSync } from "node:child_process";
import { mkdtemp, readFile, rm } from "node:fs/promises";
import { tmpdir } from "node:os";
import { dirname, join, resolve } from "node:path";
import { fileURLToPath } from "node:url";
import { runTests } from "@vscode/test-electron";

const extensionRoot = resolve(dirname(fileURLToPath(import.meta.url)), "..");
const repositoryRoot = resolve(extensionRoot, "..");
const build = spawnSync("cargo", ["build", "--bins"], {
	cwd: repositoryRoot,
	stdio: "inherit",
});
if (build.status !== 0) {
	throw new Error(`cargo build failed with status ${build.status}`);
}

const temporaryDirectory = await mkdtemp(join(tmpdir(), "jsdbg-vscode-test-"));
const stateFile = join(temporaryDirectory, "service.json");
const executable = join(
	repositoryRoot,
	"target",
	"debug",
	process.platform === "win32" ? "jsdbg-service.exe" : "jsdbg-service",
);
const service = spawn(executable, ["--state-file", stateFile], {
	stdio: ["ignore", "pipe", "pipe"],
});
let serviceError = "";
service.stderr.setEncoding("utf8");
service.stderr.on("data", (chunk) => {
	serviceError += chunk;
});

try {
	await waitForEndpoint(stateFile, service);
	await runTests({
		extensionDevelopmentPath: extensionRoot,
		extensionTestsPath: join(extensionRoot, "dist", "test", "suite", "index.cjs"),
		launchArgs: [
			join(extensionRoot, "test", "workspace"),
			`--user-data-dir=${join(temporaryDirectory, "user-data")}`,
			`--extensions-dir=${join(temporaryDirectory, "extensions")}`,
			"--disable-workspace-trust",
		],
		extensionTestsEnv: {
			JSDBG_SERVICE_STATE: stateFile,
			JSDBG_TEST_CLI: join(
				repositoryRoot,
				"target",
				"debug",
				process.platform === "win32" ? "jsdbg.exe" : "jsdbg",
			),
		},
	});
} finally {
	service.kill();
	await rm(temporaryDirectory, { recursive: true, force: true });
}

async function waitForEndpoint(path, child) {
	const deadline = Date.now() + 10_000;
	while (Date.now() < deadline) {
		if (child.exitCode !== null) {
			throw new Error(`jsdbg-service exited early: ${serviceError}`);
		}
		try {
			await readFile(path);
			return;
		} catch (error) {
			if (error?.code !== "ENOENT") {
				throw error;
			}
		}
		await new Promise((resolvePromise) => setTimeout(resolvePromise, 50));
	}
	throw new Error(`Timed out waiting for jsdbg-service endpoint: ${serviceError}`);
}
