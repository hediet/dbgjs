import { spawn } from "node:child_process";
import { access } from "node:fs/promises";
import { join, resolve } from "node:path";

const serviceStartupTimeoutMs = 15_000;

export interface DaemonProcessOptions {
	readonly extensionPath: string;
	readonly stateFile: string;
	readonly configuredExecutable?: string;
	readonly environment?: NodeJS.ProcessEnv;
	readonly log?: (message: string) => void;
}

export async function ensureDaemonProcess(options: DaemonProcessOptions): Promise<void> {
	const executable = await resolveDaemonExecutable(options);
	options.log?.(`Starting daemon check: ${executable} --ensure --state-file ${options.stateFile}`);
	const child = spawn(executable, ["--ensure", "--state-file", options.stateFile], {
		env: options.environment ?? process.env,
		stdio: ["ignore", "ignore", "pipe"],
		windowsHide: true,
	});
	child.stderr.setEncoding("utf8");
	let stderr = "";
	child.stderr.on("data", (chunk: string) => {
		stderr += chunk;
		for (const line of chunk.trimEnd().split(/\r?\n/)) {
			options.log?.(`daemon stderr: ${line}`);
		}
	});

	const exitCode = await waitForExit(child, serviceStartupTimeoutMs);
	options.log?.(`Daemon check exited with code ${exitCode ?? "null"}`);
	if (exitCode !== 0) {
		const detail = stderr.trim();
		throw new Error(
			`Failed to start dbgjs-service (exit code ${exitCode})${detail ? `: ${detail}` : ""}`,
		);
	}
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
		child.once("exit", (code) => {
			clearTimeout(timer);
			resolvePromise(code);
		});
	});
}
