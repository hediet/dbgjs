import { spawn } from "node:child_process";
import { access } from "node:fs/promises";
import { createServer } from "node:net";
import { delimiter, join } from "node:path";
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

export async function findChromeExecutable() {
	const explicit = process.env.JSDBG_CHROME_EXECUTABLE;
	const candidates = explicit ? [explicit] : chromeCandidates();
	for (const candidate of candidates) {
		try {
			await access(candidate);
			return candidate;
		} catch {}
	}
	throw new Error(
		`Chrome executable not found. Checked:\n${candidates.map((candidate) => `- ${candidate}`).join("\n")}`,
	);
}

export async function findPageTargetId(
	cli,
	contextId,
	connectionId,
	urlPrefix,
	environment,
) {
	const result = await run(
		cli,
		["--json", "context", "show", "--context", contextId],
		environment,
	);
	if (result.code !== 0) {
		throw new Error(`Could not read context '${contextId}':\n${result.output}`);
	}
	const context = JSON.parse(result.output);
	const connection = context.connections.find(
		(candidate) => candidate.id === connectionId,
	);
	const target = connection?.targets.find(
		(candidate) =>
			candidate.targetType === "page" && candidate.url.startsWith(urlPrefix),
	);
	if (target === undefined) {
		throw new Error(
			`Connection '${connectionId}' has no page target starting with '${urlPrefix}'`,
		);
	}
	return target.targetId;
}

function chromeCandidates() {
	if (process.platform === "win32") {
		return [
			process.env.PROGRAMFILES,
			process.env["PROGRAMFILES(X86)"],
			process.env.LOCALAPPDATA,
		]
			.filter(Boolean)
			.flatMap((directory) => [
				join(directory, "Google", "Chrome", "Application", "chrome.exe"),
				join(directory, "Chromium", "Application", "chrome.exe"),
			]);
	}
	if (process.platform === "darwin") {
		return [
			"/Applications/Google Chrome.app/Contents/MacOS/Google Chrome",
			"/Applications/Chromium.app/Contents/MacOS/Chromium",
		];
	}
	const pathDirectories = (process.env.PATH ?? "").split(delimiter).filter(Boolean);
	return [
		"/usr/bin/google-chrome",
		"/usr/bin/google-chrome-stable",
		"/usr/bin/chromium",
		"/usr/bin/chromium-browser",
		...pathDirectories.flatMap((directory) => [
			join(directory, "google-chrome"),
			join(directory, "google-chrome-stable"),
			join(directory, "chromium"),
			join(directory, "chromium-browser"),
		]),
	];
}

export function run(command, args, extraEnvironment, options = {}) {
	return new Promise((resolve, reject) => {
		const startedAt = performance.now();
		const child = spawn(command, args, {
			cwd: process.cwd(),
			env: { ...process.env, ...extraEnvironment },
			stdio: [options.input === undefined ? "ignore" : "pipe", "pipe", "pipe"],
			detached: process.platform !== "win32",
		});
		if (options.input !== undefined) {
			child.stdin.end(options.input);
		}
		let output = "";
		let timedOut = false;
		const timer =
			options.timeoutMs === undefined
				? undefined
				: setTimeout(() => {
						timedOut = true;
						output += `\nTimed out after ${options.timeoutMs}ms\n`;
						killProcessTree(child);
					}, options.timeoutMs);
		child.stdout.on("data", (chunk) => {
			output += chunk;
		});
		child.stderr.on("data", (chunk) => {
			output += chunk;
		});
		child.once("error", (error) => {
			if (timer !== undefined) clearTimeout(timer);
			reject(error);
		});
		child.once("exit", (code) => {
			if (timer !== undefined) clearTimeout(timer);
			resolve({
				code,
				output,
				timedOut,
				durationMs: performance.now() - startedAt,
			});
		});
	});
}

function killProcessTree(child) {
	if (child.pid === undefined) return;
	if (process.platform === "win32") {
		const killer = spawn(
			"taskkill",
			["/PID", String(child.pid), "/T", "/F"],
			{ stdio: "ignore" },
		);
		killer.unref();
		return;
	}
	try {
		process.kill(-child.pid, "SIGKILL");
	} catch {
		child.kill("SIGKILL");
	}
}
