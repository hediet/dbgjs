import { spawn } from "node:child_process";
import { createServer } from "node:net";
import { expect } from "@playwright/test";

export async function buildLiveTest() {
	const result = await run(
		"cargo",
		["test", "--test", "live_generated_cdp", "--no-run", "--message-format=json"],
		{},
	);
	expect(result.code, result.output).toBe(0);
	const artifacts = result.output
		.split(/\r?\n/)
		.filter(Boolean)
		.map((line) => {
			try {
				return JSON.parse(line);
			} catch {
				return undefined;
			}
		})
		.filter(
			(message) =>
				message?.reason === "compiler-artifact" &&
				message?.target?.name === "live_generated_cdp" &&
				typeof message?.executable === "string",
		);
	const executable = artifacts.at(-1)?.executable;
	if (typeof executable !== "string") {
		throw new Error(`Cargo did not report the live test executable:\n${result.output}`);
	}
	return executable;
}

export async function allocatePort() {
	const server = createServer();
	await new Promise((resolve, reject) => {
		server.once("error", reject);
		server.listen(0, "127.0.0.1", resolve);
	});
	const address = server.address();
	if (typeof address !== "object" || address === null) {
		throw new Error("Could not allocate a TCP port");
	}
	await new Promise((resolve, reject) =>
		server.close((error) => (error ? reject(error) : resolve())),
	);
	return address.port;
}

export async function readCdpEndpoint(port) {
	const url = `http://127.0.0.1:${port}/json/version`;
	const deadline = Date.now() + 15_000;
	let lastError;
	while (Date.now() < deadline) {
		try {
			const response = await fetch(url);
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
	throw new Error(`Chromium did not expose CDP at ${url}: ${lastError}`);
}

export function run(command, args, extraEnvironment) {
	return new Promise((resolve, reject) => {
		const child = spawn(command, args, {
			cwd: process.cwd(),
			env: { ...process.env, ...extraEnvironment },
			stdio: ["ignore", "pipe", "pipe"],
		});
		let output = "";
		child.stdout.on("data", (chunk) => {
			output += chunk;
		});
		child.stderr.on("data", (chunk) => {
			output += chunk;
		});
		child.once("error", reject);
		child.once("exit", (code) => resolve({ code, output }));
	});
}
