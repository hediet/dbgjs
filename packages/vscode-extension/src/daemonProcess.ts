import { spawn } from "node:child_process";
import { access } from "node:fs/promises";
import { join, resolve } from "node:path";
import { defaultServiceDirectory } from "./daemonClient.js";
import { serviceContractFingerprint } from "./generated/interfaces.js";

const serviceStartupTimeoutMs = 15_000;

export interface DaemonProcessOptions {
	readonly extensionPath: string;
	readonly stateFile: string;
	readonly configuredExecutable?: string;
	readonly environment?: NodeJS.ProcessEnv;
	readonly log?: (message: string) => void;
}

export async function ensureDaemonProcess(
	options: DaemonProcessOptions,
	allowParallelService = false,
): Promise<void> {
	await runDaemonCommand(options, [
		"--ensure",
		...(allowParallelService ? [] : ["--refuse-incompatible"]),
	]);
}

export async function listDaemonServices(options: DaemonProcessOptions): Promise<string> {
	return runDaemonCommand(options, ["--list"]);
}

async function runDaemonCommand(options: DaemonProcessOptions, command: readonly string[]): Promise<string> {
	const executable = await resolveDaemonExecutable(options);
	const args = [
		...command, "--state-file", options.stateFile,
		"--expected-contract", serviceContractFingerprint,
		"--registry-directory", defaultServiceDirectory(options.environment),
	];
	options.log?.(`Running service command: ${executable} ${args.join(" ")}`);
	const child = spawn(executable, args, {
		env: options.environment ?? process.env,
		stdio: ["ignore", "pipe", "pipe"],
		windowsHide: true,
	});
	child.stderr.setEncoding("utf8");
	child.stdout.setEncoding("utf8");
	let stderr = "";
	let stdout = "";
	let outputExceeded = false;
	child.stdout.on("data", (chunk: string) => {
		if (stdout.length + chunk.length > 1024 * 1024) {
			outputExceeded = true;
			child.kill();
		} else {
			stdout += chunk;
		}
	});
	child.stderr.on("data", (chunk: string) => {
		if (stderr.length + chunk.length > 64 * 1024) {
			outputExceeded = true;
			child.kill();
			return;
		}
		stderr += chunk;
		for (const line of chunk.trimEnd().split(/\r?\n/)) {
			options.log?.(`daemon stderr: ${line}`);
		}
	});

	const exitCode = await waitForExit(child, serviceStartupTimeoutMs);
	if (outputExceeded) {
		throw new Error("dbgjs-service discovery output exceeded its limit");
	}
	options.log?.(`Daemon check exited with code ${exitCode ?? "null"}`);
	if (exitCode !== 0) {
		const detail = stderr.trim();
		throw new Error(
			`dbgjs-service command failed (exit code ${exitCode})${detail ? `: ${detail}` : ""}`,
		);
	}
	return stdout;
}

export async function resolveDaemonExecutable(
	options: Pick<DaemonProcessOptions, "extensionPath" | "configuredExecutable" | "environment">,
): Promise<string> {
	const environment = options.environment ?? process.env;
	const explicit = options.configuredExecutable?.trim() || environment.DBGJS_SERVICE_EXE?.trim();
	if (explicit) {
		const executable = resolve(explicit);
		await requireExecutable(executable);
		return executable;
	}

	const name = process.platform === "win32" ? "dbgjs-service.exe" : "dbgjs-service";
	const candidates = [
		join(options.extensionPath, "bin", name),
		resolve(options.extensionPath, "..", "target", "debug", name),
		resolve(options.extensionPath, "..", "target", "release", name),
	];
	for (const candidate of candidates) {
		try {
			await access(candidate);
			return candidate;
		} catch {
			// Continue through the known packaged and development locations.
		}
	}
	throw new Error(
		`Could not find dbgjs-service. Build it with 'cargo build --bin dbgjs-service' `
			+ `or configure 'dbgjs.serviceExecutable'. Searched: ${candidates.join(", ")}`,
	);
}

async function requireExecutable(path: string): Promise<void> {
	try {
		await access(path);
	} catch (error) {
		throw new Error(`Configured dbgjs-service executable does not exist: ${path}`, {
			cause: error,
		});
	}
}

function waitForExit(
	child: ReturnType<typeof spawn>,
	timeoutMs: number,
): Promise<number | null> {
	return new Promise((resolvePromise, reject) => {
		const timer = setTimeout(() => {
			child.kill();
			reject(new Error(`Timed out after ${timeoutMs} ms while starting dbgjs-service`));
		}, timeoutMs);
		child.once("error", (error) => {
			clearTimeout(timer);
			reject(error);
		});
		child.once("close", (code) => {
			clearTimeout(timer);
			resolvePromise(code);
		});
	});
}
