import { spawn } from "node:child_process";
import { createInterface } from "node:readline";

main().catch(reportError);

async function main() {
	const executable = required("JSDBG_NODE_EXECUTABLE");
	const program = required("JSDBG_NODE_PROGRAM");
	const cwd = required("JSDBG_NODE_CWD");
	const runtimeArgs = parseJson("JSDBG_NODE_RUNTIME_ARGS", []);
	const programArgs = parseJson("JSDBG_NODE_ARGS", []);
	const environment = parseJson("JSDBG_NODE_ENV", {});
	const child = spawn(executable, [
		...runtimeArgs,
		"--inspect-brk=127.0.0.1:0",
		program,
		...programArgs,
	], {
		cwd,
		env: { ...process.env, ...environment },
		stdio: ["ignore", "pipe", "pipe"],
	});
	child.stdout.pipe(process.stderr);
	try {
		const endpoint = await debuggerEndpoint(child);
		process.stdout.write(`${JSON.stringify({ endpoint })}\n`);
		await Promise.race([
			onceExit(child),
			new Promise((resolve) => {
				process.stdin.once("end", resolve);
				process.once("SIGINT", resolve);
				process.once("SIGTERM", resolve);
				process.stdin.resume();
			}),
		]);
	} finally {
		if (child.exitCode === null) {
			child.kill("SIGTERM");
			await Promise.race([onceExit(child), delay(5_000)]);
		}
		if (child.exitCode === null) {
			child.kill("SIGKILL");
		}
	}
}

function debuggerEndpoint(child) {
	return new Promise((resolve, reject) => {
		const lines = createInterface({ input: child.stderr });
		let settled = false;
		const timeout = setTimeout(() => {
			settled = true;
			reject(new Error("Node.js inspector startup timed out"));
		}, 30_000);
		lines.on("line", (line) => {
			process.stderr.write(`${line}\n`);
			const match = /Debugger listening on (ws:\/\/\S+)/.exec(line);
			if (!settled && match?.[1]) {
				settled = true;
				clearTimeout(timeout);
				resolve(match[1]);
			}
		});
		child.once("exit", (code) => {
			if (!settled) {
				settled = true;
				clearTimeout(timeout);
				reject(new Error(`Node.js exited during startup with code ${code}`));
			}
		});
	});
}

function onceExit(child) {
	return new Promise((resolve) => child.once("exit", resolve));
}

function required(name) {
	const value = process.env[name];
	if (!value) {
		throw new Error(`${name} is required`);
	}
	return value;
}

function parseJson(name, fallback) {
	const value = process.env[name];
	return value ? JSON.parse(value) : fallback;
}

function delay(milliseconds) {
	return new Promise((resolve) => setTimeout(resolve, milliseconds));
}

function reportError(error) {
	process.stdout.write(`${JSON.stringify({
		error: error instanceof Error ? error.stack : String(error),
	})}\n`);
	process.exitCode = 1;
}
