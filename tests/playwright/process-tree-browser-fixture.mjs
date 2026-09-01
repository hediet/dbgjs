import { spawn } from "node:child_process";
import { createServer } from "node:http";
import { mkdir } from "node:fs/promises";
import { close as closeInspector } from "node:inspector";
import {
	allocatePort,
	findChromeExecutable,
	readCdpEndpoint,
} from "./live-test-harness.mjs";

const profileDirectory = process.argv[2];
if (!profileDirectory) {
	throw new Error("profile directory argument is required");
}

const childServer = createServer((_request, response) => {
	response.writeHead(200, { "content-type": "text/html; charset=utf-8" });
	response.end('<strong id="oopif-marker">nested browser-root iframe</strong>');
});
await listen(childServer, "localhost");
const childAddress = childServer.address();
if (typeof childAddress !== "object" || childAddress === null) {
	throw new Error("child fixture server did not bind to TCP");
}

const pageServer = createServer((_request, response) => {
	response.writeHead(200, { "content-type": "text/html; charset=utf-8" });
	response.end(`<!doctype html>
<title>Process tree browser demo</title>
<main>
	<strong id="page-marker">page discovered through the process tree</strong>
	<iframe id="oopif" src="http://localhost:${childAddress.port}/child"></iframe>
</main>`);
});
await listen(pageServer, "127.0.0.1");
const pageAddress = pageServer.address();
if (typeof pageAddress !== "object" || pageAddress === null) {
	throw new Error("page fixture server did not bind to TCP");
}

await mkdir(profileDirectory, { recursive: true });
const debuggingPort = await allocatePort();
const browser = spawn(
	await findChromeExecutable(),
	[
		"--headless=new",
		"--disable-extensions",
		`--remote-debugging-port=${debuggingPort}`,
		"--remote-allow-origins=*",
		"--site-per-process",
		"--no-first-run",
		"--no-default-browser-check",
		`--user-data-dir=${profileDirectory}`,
		"about:blank",
	],
	{ stdio: "ignore" },
);
const endpoint = await readCdpEndpoint(debuggingPort);
const pageUrl = `http://127.0.0.1:${pageAddress.port}/`;
const debugOrigin = new URL(endpoint).origin.replace("ws:", "http:");
const created = await fetch(`${debugOrigin}/json/new?${encodeURIComponent(pageUrl)}`, {
	method: "PUT",
});
if (!created.ok) {
	throw new Error(`could not create fixture page: ${await created.text()}`);
}
await poll(async () => {
	const { targetInfos } = await cdpRequest(endpoint, "Target.getTargets");
	return targetInfos.some(
		(target) =>
			target.type === "iframe" &&
			target.url === `http://localhost:${childAddress.port}/child`,
	);
});
process.stdout.write(
	`${JSON.stringify({
		kind: "ready",
		url: pageUrl,
		debuggingPort,
	})}\n`,
);

process.stdin.resume();
await new Promise((resolve) => process.stdin.once("end", resolve));
await Promise.race([
	cdpRequest(endpoint, "Browser.close").catch(() => undefined),
	new Promise((resolve) => setTimeout(resolve, 2_000)),
]);
browser.kill();
await Promise.all([close(pageServer), close(childServer)]);
closeInspector();

function listen(server, host) {
	return new Promise((resolve, reject) => {
		server.once("error", reject);
		server.listen(0, host, resolve);
	});
}

function close(server) {
	return new Promise((resolve, reject) => {
		server.close((error) => (error ? reject(error) : resolve()));
	});
}

function cdpRequest(endpoint, method) {
	return new Promise((resolve, reject) => {
		const socket = new WebSocket(endpoint);
		socket.onopen = () => {
			socket.send(JSON.stringify({ id: 1, method }));
		};
		socket.onmessage = (event) => {
			const message = JSON.parse(event.data);
			if (message.id !== 1) {
				return;
			}
			socket.close();
			if (message.error) {
				reject(new Error(message.error.message));
			} else {
				resolve(message.result);
			}
		};
		socket.onerror = () => reject(new Error("CDP WebSocket failed"));
	});
}

async function poll(operation) {
	const deadline = Date.now() + 15_000;
	while (Date.now() < deadline) {
		if (await operation()) {
			return;
		}
		await new Promise((resolve) => setTimeout(resolve, 50));
	}
	throw new Error("timed out waiting for the OOPIF target");
}
