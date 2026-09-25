import { Console } from "node:console";
import { pathToFileURL } from "node:url";

const MAX_RESULT_BYTES = 1024 * 1024;
const DEFAULT_TIMEOUT_MS = 27_000;
const progressEnabled = process.env.DBGJS_PLAYWRIGHT_PROGRESS === "1";
const requestedTimeout = Number(process.env.DBGJS_PLAYWRIGHT_TIMEOUT_MS);
const timeoutMs = Number.isFinite(requestedTimeout) && requestedTimeout > 0
	? Math.min(requestedTimeout, DEFAULT_TIMEOUT_MS)
	: DEFAULT_TIMEOUT_MS;
const started = performance.now();
const deadline = started + timeoutMs;

main().catch((error) => finish({
	ok: false,
	error: formatError(error),
	failure: failureDetails(error),
}, 1));

async function main() {
	const { chromium, endpoint, program } = await runPhase("starting", async () => {
		const endpoint = process.env.DBGJS_PLAYWRIGHT_ENDPOINT;
		const packagePath = process.env.DBGJS_PLAYWRIGHT_PACKAGE;
		if (!endpoint || !packagePath) {
			throw new Error("Playwright execution configuration is incomplete");
		}
		let program = "";
		process.stdin.setEncoding("utf8");
		for await (const chunk of process.stdin) program += chunk;
		const { chromium } = await import(pathToFileURL(packagePath));
		return { chromium, endpoint, program };
	});
	const browser = await runPhase("connecting", async () => {
		try {
			return await chromium.connectOverCDP(endpoint);
		} catch (error) {
			if (/page identity lookup|proxy deadline during/.test(String(error?.message))) {
				throw new Error(`Playwright initializing failed: ${error.message}`, { cause: error });
			}
			throw error;
		}
	}, Math.min(2_000, timeoutMs / 4));
	let result;
	let executionError;
	try {
		const pages = await runPhase("initializing", () =>
			browser.contexts().flatMap((context) => context.pages()),
		);
		if (pages.length !== 1) {
			throw new Error(
				`virtual CDP endpoint exposed ${pages.length} pages; expected exactly one`,
			);
		}
		result = await runPhase("executing", async () => {
			Object.defineProperty(globalThis, "page", {
				value: pages[0],
				configurable: true,
			});
			const AsyncFunction = Object.getPrototypeOf(async function () {}).constructor;
			const programConsole = new Console({
				stdout: process.stderr,
				stderr: process.stderr,
				colorMode: false,
			});
			const value = await new AsyncFunction("console", program)(programConsole);
			if (value === undefined) return { ok: true, hasValue: false };
			assertJsonValue(value, "$", new Set());
			const serialized = JSON.stringify({ ok: true, hasValue: true, value });
			if (serialized === undefined) {
				throw new Error("Playwright program result is not JSON serializable");
			}
			if (Buffer.byteLength(serialized) > MAX_RESULT_BYTES) {
				throw new Error(`Playwright program result exceeds ${MAX_RESULT_BYTES} bytes`);
			}
			return JSON.parse(serialized);
		}, Math.min(1_500, timeoutMs / 4));
	} catch (error) {
		executionError = error;
	} finally {
		delete globalThis.page;
		try {
			await runPhase("closing", () => browser.close());
		} catch (error) {
			if (executionError) {
				executionError.cleanup = error;
				throw executionError;
			}
			throw error;
		}
	}
	if (executionError) throw executionError;
	finish(result, 0);
}

async function runPhase(phase, operation, reserveMs = 0) {
	const elapsedMs = Math.round(performance.now() - started);
	if (progressEnabled) {
		process.stderr.write(`DBGJS_PLAYWRIGHT_PROGRESS:${JSON.stringify({ phase, elapsedMs })}\n`);
	}
	const remaining = Math.max(0, deadline - performance.now() - reserveMs);
	if (remaining === 0) throw new PhaseError(phase, "timeout", `Playwright ${phase} exceeded its deadline after ${elapsedMs} ms`);
	let timer;
	try {
		return await Promise.race([
			Promise.resolve().then(operation),
			new Promise((_, reject) => {
				timer = setTimeout(
					() => reject(new PhaseError(phase, "timeout",
						`Playwright ${phase} exceeded its deadline after ${Math.round(performance.now() - started)} ms`,
					)),
					remaining,
				);
			}),
		]);
	} catch (error) {
		throw error instanceof PhaseError ? error : new PhaseError(phase, "failed", error?.message ?? String(error), { cause: error });
	} finally {
		clearTimeout(timer);
	}
}

class PhaseError extends Error {
	constructor(phase, kind, message, options) {
		super(message, options);
		this.phase = phase;
		this.kind = kind;
	}
}

function failureDetails(error) {
	const detail = {
		phase: error.phase ?? "starting",
		kind: error.kind ?? "failed",
		message: redactEndpoint(error.message ?? String(error)),
	};
	if (error.cleanup) detail.cleanup = failureDetails(error.cleanup);
	return detail;
}

function assertJsonValue(value, path, seen) {
	if (
		value === null ||
		typeof value === "string" ||
		typeof value === "boolean"
	) return;
	if (typeof value === "number") {
		if (!Number.isFinite(value)) throw new Error(`${path} is not a finite JSON number`);
		return;
	}
	if (typeof value !== "object") {
		throw new Error(`${path} has non-JSON type ${typeof value}`);
	}
	if (seen.has(value)) throw new Error(`${path} contains a cycle`);
	seen.add(value);
	if (Array.isArray(value)) {
		for (let index = 0; index < value.length; index++) {
			assertJsonValue(value[index], `${path}[${index}]`, seen);
		}
	} else {
		const prototype = Object.getPrototypeOf(value);
		if (prototype !== Object.prototype && prototype !== null) {
			throw new Error(`${path} is not a plain JSON object`);
		}
		for (const [key, child] of Object.entries(value)) {
			assertJsonValue(child, `${path}.${key}`, seen);
		}
	}
	seen.delete(value);
}

function finish(value, exitCode) {
	const text = `${JSON.stringify(value)}\n`;
	process.stdout.write(text, () => {
		process.exit(exitCode);
	});
}

function formatError(error) {
	if (!(error instanceof Error)) return redactEndpoint(String(error));
	let text = error.stack ?? `${error.name}: ${error.message}`;
	let cause = error.cause;
	while (cause !== undefined) {
		text += `\nCaused by: ${cause instanceof Error ? cause.stack ?? cause.message : String(cause)}`;
		cause = cause instanceof Error ? cause.cause : undefined;
	}
	if (error.cleanup) text += `\nCleanup failed: ${formatError(error.cleanup)}`;
	return redactEndpoint(text);
}

function redactEndpoint(text) {
	const endpoint = process.env.DBGJS_PLAYWRIGHT_ENDPOINT;
	return endpoint ? text.replaceAll(endpoint, "<playwright-endpoint>") : text;
}
