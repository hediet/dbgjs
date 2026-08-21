import { mkdtemp, rm } from "node:fs/promises";
import { createServer } from "node:net";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { pathToFileURL } from "node:url";

main().catch((error) => {
	process.stdout.write(
		`${JSON.stringify({ error: error instanceof Error ? error.stack : String(error) })}\n`,
	);
	process.exitCode = 1;
});

async function main() {
	const url = process.env.JSDBG_PROVIDER_URL;
	const requestedChannel = process.env.JSDBG_PROVIDER_CHANNEL;
	const mode = process.env.JSDBG_PROVIDER_MODE;
	const playwrightPackage = process.env.JSDBG_PLAYWRIGHT_PACKAGE;
	if (!url || !requestedChannel || !mode) {
		throw new Error("Playwright provider configuration is incomplete");
	}
	if (!playwrightPackage) {
		throw new Error("JSDBG_PLAYWRIGHT_PACKAGE is not set");
	}
	const { chromium } = await import(pathToFileURL(playwrightPackage));

	const port = await allocatePort();
	const userDataDir = await mkdtemp(join(tmpdir(), "jsdbg-playwright-"));
	let context;
	try {
		context = await chromium.launchPersistentContext(userDataDir, {
			...(requestedChannel === "bundled" ? {} : { channel: requestedChannel }),
			headless: mode === "headless",
			args: [
				`--remote-debugging-port=${port}`,
				"--remote-allow-origins=*",
				"--no-first-run",
				"--no-default-browser-check",
			],
		});
		const page = context.pages()[0] ?? (await context.newPage());
		await page.goto(url, { waitUntil: "load" });
		const endpoint = await readCdpEndpoint(port);
		process.stdout.write(`${JSON.stringify({ endpoint })}\n`);
		await new Promise((resolve) => {
			process.stdin.once("end", resolve);
			process.once("SIGINT", resolve);
			process.once("SIGTERM", resolve);
			process.stdin.resume();
		});
	} finally {
		try {
			await context?.close();
		} finally {
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
		throw new Error("could not allocate Chromium debugging port");
	}
	await new Promise((resolve, reject) =>
		server.close((error) => (error ? reject(error) : resolve())),
	);
	return address.port;
}

async function readCdpEndpoint(port) {
	const endpoint = `http://127.0.0.1:${port}/json/version`;
	const deadline = Date.now() + 15_000;
	let lastError;
	while (Date.now() < deadline) {
		try {
			const response = await fetch(endpoint);
			if (response.ok) {
				const version = await response.json();
				if (typeof version.webSocketDebuggerUrl === "string") {
					return version.webSocketDebuggerUrl;
				}
			}
		} catch (error) {
			lastError = error;
		}
		await new Promise((resolve) => setTimeout(resolve, 50));
	}
	throw new Error(`Chromium did not expose CDP at ${endpoint}: ${lastError}`);
}
