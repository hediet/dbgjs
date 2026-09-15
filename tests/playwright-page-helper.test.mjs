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

function runProgram(program) {
	const child = spawnSync(process.execPath, ["--input-type=module", "--eval", helper], {
		input: program,
		encoding: "utf8",
		timeout: 10_000,
		env: {
			...process.env,
			DBGJS_PLAYWRIGHT_ENDPOINT: "mock:playwright-page",
			DBGJS_PLAYWRIGHT_PACKAGE: packagePath,
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
