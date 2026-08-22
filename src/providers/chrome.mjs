import { spawn } from "node:child_process";
import { mkdtemp, rm } from "node:fs/promises";
import { createServer } from "node:net";
import { tmpdir } from "node:os";
import { join } from "node:path";

main().catch(reportError);

async function main() {
	const executable = required("JSDBG_CHROME_EXECUTABLE");
	const url = required("JSDBG_PROVIDER_URL");
	const headless = required("JSDBG_PROVIDER_MODE") === "headless";
	const configuredDataDir = process.env.JSDBG_CHROME_USER_DATA_DIR;
	const userDataDir = configuredDataDir || await mkdtemp(join(tmpdir(), "jsdbg-chrome-"));
	const removeDataDir = !configuredDataDir;
	const port = await allocatePort();
	const extraArgs = parseJson("JSDBG_CHROME_ARGS", []);
	const child = spawn(executable, [
		`--remote-debugging-port=${port}`,
		"--remote-debugging-address=127.0.0.1",
		"--no-first-run",
		"--no-default-browser-check",
		`--user-data-dir=${userDataDir}`,
		...(headless ? ["--headless=new", "--disable-gpu"] : []),
		...extraArgs,
		url,
	], { stdio: "ignore" });
	try {
		const endpoint = await readCdpEndpoint(port, child);
		process.stdout.write(`${JSON.stringify({ endpoint })}\n`);
		await waitForShutdown(child);
	} finally {
		if (child.exitCode === null) {
			child.kill("SIGTERM");
			await Promise.race([onceExit(child), delay(5_000)]);
		}
		if (child.exitCode === null) {
			child.kill("SIGKILL");
		}
		if (removeDataDir) {
			await rm(userDataDir, { recursive: true, force: true });
		}
	}
}

async function allocatePort() {
	const server = createServer();
	await new Promise((resolve, reject) => {
		server.once("error", reject);
		server.listen(0, "127.0.0.1", resolve);
	});
	const address = server.address();
	if (typeof address !== "object" || address === null) {
		throw new Error("could not allocate a Chrome debugging port");
	}
	await new Promise((resolve, reject) =>
		server.close((error) => error ? reject(error) : resolve()),
	);
	return address.port;
}

async function readCdpEndpoint(port, child) {
	const endpoint = `http://127.0.0.1:${port}/json/version`;
	const deadline = Date.now() + 30_000;
	while (Date.now() < deadline) {
		if (child.exitCode !== null) {
			throw new Error(`Chrome exited during startup with code ${child.exitCode}`);
		}
		try {
			const response = await fetch(endpoint);
			const version = response.ok ? await response.json() : undefined;
			if (typeof version?.webSocketDebuggerUrl === "string") {
				return version.webSocketDebuggerUrl;
			}
		} catch {}
		await delay(50);
	}
	throw new Error(`Chrome did not expose CDP at ${endpoint}`);
}

function waitForShutdown(child) {
	return Promise.race([
		onceExit(child),
		new Promise((resolve) => {
			process.stdin.once("end", resolve);
			process.once("SIGINT", resolve);
			process.once("SIGTERM", resolve);
			process.stdin.resume();
		}),
	]);
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
