import { Console } from "node:console";
import { pathToFileURL } from "node:url";

const MAX_RESULT_BYTES = 1024 * 1024;

main().catch((error) => finish({ ok: false, error: formatError(error) }, 1));

async function main() {
	const endpoint = process.env.DBGJS_PLAYWRIGHT_ENDPOINT;
	const packagePath = process.env.DBGJS_PLAYWRIGHT_PACKAGE;
	if (!endpoint || !packagePath) {
		throw new Error("Playwright execution configuration is incomplete");
	}
	let program = "";
	process.stdin.setEncoding("utf8");
	for await (const chunk of process.stdin) program += chunk;
	const { chromium } = await import(pathToFileURL(packagePath));
	const browser = await chromium.connectOverCDP(endpoint);
	let result;
	try {
		const pages = browser.contexts().flatMap((context) => context.pages());
		if (pages.length !== 1) {
			throw new Error(
				`virtual CDP endpoint exposed ${pages.length} pages; expected exactly one`,
			);
		}
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
		if (value === undefined) {
			result = { ok: true, hasValue: false };
		} else {
			assertJsonValue(value, "$", new Set());
			const serialized = JSON.stringify({ ok: true, hasValue: true, value });
			if (serialized === undefined) {
				throw new Error("Playwright program result is not JSON serializable");
			}
			if (Buffer.byteLength(serialized) > MAX_RESULT_BYTES) {
				throw new Error(`Playwright program result exceeds ${MAX_RESULT_BYTES} bytes`);
			}
			result = JSON.parse(serialized);
		}
	} finally {
		delete globalThis.page;
		await browser.close();
	}
	finish(result, 0);
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
		process.exitCode = exitCode;
	});
}

function formatError(error) {
	if (!(error instanceof Error)) return String(error);
	let text = error.stack ?? `${error.name}: ${error.message}`;
	let cause = error.cause;
	while (cause !== undefined) {
		text += `\nCaused by: ${cause instanceof Error ? cause.stack ?? cause.message : String(cause)}`;
		cause = cause instanceof Error ? cause.cause : undefined;
	}
	return text;
}
