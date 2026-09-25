import assert from "node:assert/strict";
import { spawnSync } from "node:child_process";
import { readFileSync } from "node:fs";
import { fileURLToPath } from "node:url";
import { test } from "node:test";

const helper = readFileSync(
	new URL("../src/providers/playwright_page.mjs", import.meta.url),
	"utf8",
);
const packagePath = fileURLToPath(
	new URL("./fixtures/playwright-page-browser.mjs", import.meta.url),
);

function runProgram(program, phase, timeoutMs = 10_000) {
	const child = spawnSync(process.execPath, ["--input-type=module", "--eval", helper], {
		input: program,
		encoding: "utf8",
		timeout: 3_500,
		env: {
			...process.env,
			DBGJS_PLAYWRIGHT_ENDPOINT: "mock:playwright-page",
			DBGJS_PLAYWRIGHT_PACKAGE: packagePath,
			...(phase ? { DBGJS_FIXTURE_PHASE: phase } : {}),
			DBGJS_PLAYWRIGHT_TIMEOUT_MS: String(timeoutMs),
		},
	});
	assert.ifError(child.error);
	assert.equal(child.signal, null);
	return {
		status: child.status,
		result: JSON.parse(child.stdout),
		stderr: child.stderr,
	};
}

for (const [phase, program] of [
	["connecting", "return 1"],
	["executing", "await new Promise(() => setInterval(() => {}, 1000))"],
	["closing", "return 1"],
]) {
	test(`${phase} stall reports its phase instead of waiting for the outer watchdog`, () => {
		const output = runProgram(program, phase, 450);
		assert.equal(output.status, 1);
		assert.match(output.result.error, new RegExp(phase));
		assert.match(output.result.error, /deadline|limit|timed out/i);
	});
}

test("cleanup failure after a successful program is attributed to closing", () => {
	const output = runProgram("return { success: true }", "closing", 450);
	assert.equal(output.status, 1);
	assert.match(output.result.error, /closing/);
	assert.doesNotMatch(output.result.error, /executing.*failed/);
});

test("program failure is retained when closing also fails", () => {
	const output = runProgram('throw new Error("primary fixture failure")', "closing-error", 450);
	assert.equal(output.status, 1);
	assert.match(output.result.error, /primary fixture failure/);
	assert.match(output.result.error, /fixture closing failed/);
	assert.equal(output.result.failure.phase, "executing");
	assert.equal(output.result.failure.cleanup.phase, "closing");
	assert.doesNotMatch(JSON.stringify(output.result), /mock:playwright-page/);
});

for (const [expression, message] of [
	["null", "null"],
	["undefined", "undefined"],
	['"fixture string failure"', "fixture string failure"],
]) {
	for (const phase of [undefined, "closing-error"]) {
		test(`throw ${expression} reports executing${phase ? " and closing" : ""}`, () => {
			const output = runProgram(`throw ${expression}`, phase);
			assert.equal(output.status, 1);
			assert.equal(output.result.failure.phase, "executing");
			assert.equal(output.result.failure.kind, "failed");
			assert.equal(output.result.failure.message, message);
			assert.match(output.result.error, new RegExp(message));
			if (phase) {
				assert.equal(output.result.failure.cleanup.phase, "closing");
				assert.match(output.result.error, /fixture closing failed/);
			} else {
				assert.equal(output.result.failure.cleanup, undefined);
			}
		});
	}
}

test("closing is bounded even when execution consumed its budget", () => {
	const output = runProgram('await new Promise(() => setInterval(() => {}, 1000))', "closing", 450);
	assert.equal(output.status, 1);
	assert.match(output.result.error, /executing.*deadline/);
	assert.match(output.result.error, /closing.*deadline/);
	assert.equal(output.result.failure.phase, "executing");
	assert.equal(output.result.failure.cleanup.kind, "timeout");
});

test("immediate proxy setup errors reach the caller without waiting for the deadline", () => {
	const output = runProgram("return 1", "connection-error", 300);
	assert.equal(output.status, 1);
	assert.match(output.result.error, /fixture proxy setup failed/);
	assert.doesNotMatch(output.result.error, /exceeded its deadline/);
	assert.doesNotMatch(output.result.error, /mock:playwright-page/);
});

test("frame initialization failures are distinct from user execution", () => {
	const output = runProgram("return 1", "initialization-error", 450);
	assert.equal(output.status, 1);
	assert.match(output.result.error, /fixture frame initialization failed/);
	assert.doesNotMatch(output.result.error, /executing.*deadline/);
});

test("proxy identity failure is attributed to initialization", () => {
	const output = runProgram("return 1", "identity-error", 450);
	assert.equal(output.status, 1);
	assert.match(output.result.error, /Playwright initializing failed.*Page.getFrameTree/);
});

test("timeout diagnostic does not include code or capability endpoint", () => {
	const output = runProgram(
		'await new Promise(() => setInterval(() => {}, 1000)); return "private-document-value"',
		undefined,
		450,
	);
	assert.equal(output.status, 1);
	assert.match(output.result.error, /executing.*deadline/);
	assert.doesNotMatch(output.result.error, /private-document-value|mock:playwright-page/);
});

test("console.log does not corrupt the helper's JSON result", () => {
	assert.deepEqual(
		runProgram('console.log(await page.locator("body").innerText())'),
		{
			status: 0,
			result: { ok: true, hasValue: false },
			stderr: "fixture body\n",
		},
	);
});

test("returned values and standard console formatting are preserved", () => {
	assert.deepEqual(
		runProgram(`
			console.log("value=%d %s", 42, "text");
			console.info("info", { value: true });
			console.debug("debug");
			console.warn("warn");
			console.error("error");
			console.dir({ nested: { value: 1 } });
			return { text: await page.locator("body").innerText(), values: [null, 42, false] };
		`),
		{
			status: 0,
			result: {
				ok: true,
				hasValue: true,
				value: { text: "fixture body", values: [null, 42, false] },
			},
			stderr: "value=42 text\ninfo { value: true }\ndebug\nwarn\nerror\n{ nested: { value: 1 } }\n",
		},
	);
});

test("the program console is scoped without replacing Node's global console", () => {
	assert.deepEqual(
		runProgram(`
			const { default: nodeConsole } = await import("node:console");
			return {
				scoped: console !== globalThis.console,
				globalUnchanged: globalThis.console === nodeConsole,
			};
		`),
		{
			status: 0,
			result: { ok: true, hasValue: true, value: { scoped: true, globalUnchanged: true } },
			stderr: "",
		},
	);
});

test("logging before an exception preserves the structured error", () => {
	const output = runProgram('console.log("before failure"); throw new Error("fixture failure")');
	assert.equal(output.status, 1);
	assert.equal(output.result.ok, false);
	assert.match(output.result.error, /Error: fixture failure/);
	assert.equal(output.stderr, "before failure\n");
});

test("non-JSON return values are still rejected after logging", () => {
	const output = runProgram('console.log("before invalid result"); return 1n');
	assert.equal(output.status, 1);
	assert.equal(output.result.ok, false);
	assert.match(output.result.error, /non-JSON type bigint/);
	assert.equal(output.stderr, "before invalid result\n");
});
