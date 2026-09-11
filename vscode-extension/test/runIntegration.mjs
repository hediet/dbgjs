import { spawnSync } from "node:child_process";
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

const temporaryDirectory = await mkdtemp(join(tmpdir(), "dbgjs-vscode-test-"));
const stateFile = join(temporaryDirectory, "service.json");
const workspacePath = join(temporaryDirectory, "workspace");
await cp(join(extensionRoot, "test", "workspace"), workspacePath, { recursive: true });
const npmCommand = process.platform === "win32"
	? {
			executable: process.env.ComSpec ?? "cmd.exe",
			args: ["/d", "/s", "/c", "npm ci --ignore-scripts"],
		}
	: {
			executable: "npm",
			args: ["ci", "--ignore-scripts"],
		};
const install = spawnSync(npmCommand.executable, npmCommand.args, {
	cwd: workspacePath,
	stdio: "inherit",
});
if (install.status !== 0) {
	throw new Error(
		`fixture npm install failed with status ${install.status}: ${
			install.error?.message ?? "unknown error"
		}`,
	);
}
const buildCommand = process.platform === "win32"
	? {
			executable: process.env.ComSpec ?? "cmd.exe",
			args: ["/d", "/s", "/c", "npm run build:tsc"],
		}
	: {
			executable: "npm",
			args: ["run", "build:tsc"],
		};
const fixtureBuild = spawnSync(buildCommand.executable, buildCommand.args, {
	cwd: workspacePath,
	stdio: "inherit",
});
if (fixtureBuild.status !== 0) {
	throw new Error(
		`fixture TypeScript build failed with status ${fixtureBuild.status}: ${
			fixtureBuild.error?.message ?? "unknown error"
		}`,
	);
}
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
	process.platform === "win32" ? "dbgjs-service.exe" : "dbgjs-service",
);
try {
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
			DBGJS_SERVICE_STATE: stateFile,
			DBGJS_SERVICE_EXE: executable,
			DBGJS_TEST_CHROME: chromium.executablePath(),
			DBGJS_TEST_NODE_ONLY: process.env.DBGJS_TEST_NODE_ONLY ?? "",
			DBGJS_TEST_LOG_STDOUT: process.env.DBGJS_TEST_LOG_STDOUT ?? "",
		},
	});
	const endpoint = JSON.parse(await readFile(stateFile, "utf8"));
	if (!Number.isInteger(endpoint.processId)) {
		throw new Error("dbgjs-service endpoint does not contain a process ID");
	}
	try {
		process.kill(endpoint.processId, 0);
	} catch (error) {
		throw new Error(
			`dbgjs-service process ${endpoint.processId} did not survive the VS Code window`,
			{ cause: error },
		);
	}
} finally {
	spawnSync(
		join(
			repositoryRoot,
			"target",
			"debug",
			process.platform === "win32" ? "dbgjs.exe" : "dbgjs",
		),
		["service", "stop"],
		{
			env: { ...process.env, DBGJS_SERVICE_STATE: stateFile },
			stdio: "ignore",
		},
	);
	await rm(temporaryDirectory, {
		recursive: true,
		force: true,
		maxRetries: 5,
		retryDelay: 200,
	});
}
