import { spawn, spawnSync } from "node:child_process";
import { cp, mkdir, mkdtemp, readFile, rm, symlink } from "node:fs/promises";
import { tmpdir } from "node:os";
import { dirname, join, resolve } from "node:path";
import { fileURLToPath, pathToFileURL } from "node:url";
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
const workspacePath = join(temporaryDirectory, "workspace");
await cp(join(extensionRoot, "test", "workspace"), workspacePath, { recursive: true });
await mkdir(join(workspacePath, "node_modules"), { recursive: true });
await symlink(
	join(repositoryRoot, "node_modules", "playwright"),
	join(workspacePath, "node_modules", "playwright"),
	"dir",
);
const { chromium } = await import(
	pathToFileURL(join(repositoryRoot, "node_modules", "playwright", "index.mjs")).href
);
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
			workspacePath,
			`--user-data-dir=${join(temporaryDirectory, "user-data")}`,
			`--extensions-dir=${join(temporaryDirectory, "extensions")}`,
			"--disable-workspace-trust",
		],
		extensionTestsEnv: {
			JSDBG_SERVICE_STATE: stateFile,
			JSDBG_TEST_CHROME: chromium.executablePath(),
		},
	});
} catch (error) {
	process.stderr.write(`jsdbg-service stderr:\n${serviceError}\n`);
	throw error;
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
