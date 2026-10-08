import { spawn } from "node:child_process";
import { once } from "node:events";
import { mkdtemp, rm } from "node:fs/promises";
import { tmpdir } from "node:os";
import { join, resolve } from "node:path";
import { expect, test as base } from "@playwright/test";
import { run } from "./live-test-harness.mjs";

const suffix = process.platform === "win32" ? ".exe" : "";
const binaries = process.env.DBGJS_TEST_BIN_DIR ?? "target/debug";
const cli = resolve(binaries, `dbgjs${suffix}`);
const service = resolve(binaries, `dbgjs-service${suffix}`);

const test = base.extend({
	runtime: async ({}, use, testInfo) => {
		const directory = await mkdtemp(join(tmpdir(), "dbgjs-values-"));
		const environment = {
			DBGJS_SERVICE_EXE: service,
			DBGJS_SERVICE_STATE: join(directory, "service.json"),
		};
		const transcript = [];
		let nodeOutput = "";
		const node = spawn(process.execPath, [
			"--inspect=0",
			"-e",
			"globalThis.fixtureReady = true; setInterval(() => {}, 1000);",
		], { stdio: ["ignore", "pipe", "pipe"] });
		node.stdout.on("data", (chunk) => { nodeOutput += chunk; });
		node.stderr.on("data", (chunk) => { nodeOutput += chunk; });
		let nodeError;
		node.on("error", (error) => { nodeError = error; });
		const command = async (args, options = {}) => {
			const result = await run(cli, args, environment, {
				timeoutMs: 15_000,
				...options,
			});
			transcript.push({ args, ...result });
			expect(result.timedOut, result.output).toBe(false);
			return result;
		};
		const ok = async (args, options) => {
			const result = await command(args, options);
			expect(result.code, `${args.join(" ")}\n${result.output}`).toBe(0);
			return result.stdout;
		};
		const json = async (args, options) =>
			JSON.parse(await ok(["--json", ...args], options));
		try {
			await expect.poll(() => {
				if (nodeError) throw nodeError;
				return nodeOutput.match(/ws:\/\/127\.0\.0\.1:\d+\/[^\s]+/)?.[0];
			}, { timeout: 10_000 }).toBeTruthy();
			const endpoint = nodeOutput.match(/ws:\/\/127\.0\.0\.1:\d+\/[^\s]+/)[0];
			await ok(["context", "create", "--context", ":values", "--set"]);
			await ok(["connection", "add", "--node-inspector", endpoint, "--connection", "runtime", "--connect"]);
			await ok(["target", "attach", "--connection", "runtime", "--set"]);
			await use({ command, ok, json });
		} finally {
			const stopped = await run(cli, ["service", "stop"], environment, { timeoutMs: 10_000 });
			transcript.push({ args: ["service", "stop"], ...stopped });
			if (node.exitCode === null && node.signalCode === null) {
				const exited = once(node, "exit");
				node.kill();
				await exited;
			}
			await testInfo.attach("cli-transcript", {
				body: Buffer.from(JSON.stringify({ transcript, nodeOutput }, null, 2)),
				contentType: "application/json",
			});
			await rm(directory, { recursive: true, force: true });
		}
	},
});

test.beforeAll(async () => {
	if (!process.env.DBGJS_TEST_BIN_DIR) {
		const build = await run("cargo", [
			"build", "--locked", "--bin", "dbgjs", "--bin", "dbgjs-service",
		], {}, { timeoutMs: 600_000 });
		expect(build.code, build.output).toBe(0);
	}
});

function reference(output, kind = "[ve]") {
	const match = output.match(new RegExp(`@${kind}[A-Za-z0-9_-]+`));
	expect(match, output).not.toBeNull();
	return match[0];
}

test("JSON inspection and exact export preserve ordinary JSON values", async ({ runtime }) => {
	const value = {
		count: 2,
		text: "hello\n\"world\" 😀",
		fraction: 1.25,
		yes: true,
		nothing: null,
		items: [1, "two", false, { nested: ["three"] }],
		__protoLike: { constructor: "ordinary data" },
		$dbgjs: "application-owned key",
	};
	const expression = `(${JSON.stringify(value)})`;
	expect(await runtime.json(["target", "eval", expression])).toEqual(value);
	const exact = await runtime.ok(["target", "eval", expression, "--json-expect"]);
	expect(JSON.parse(exact)).toEqual(value);
	expect(await runtime.json(["target", "eval", "-", "--full"], { input: expression })).toEqual(value);
	for (const primitive of [null, false, 0, "", "hello", 1.5]) {
		expect(await runtime.json(["target", "eval", JSON.stringify(primitive)])).toEqual(primitive);
	}
	const sharedExpression = "(() => { const shared = { n: 1 }; return { a: shared, b: shared }; })()";
	const shared = { a: { n: 1 }, b: { n: 1 } };
	expect(await runtime.json(["target", "eval", sharedExpression])).toEqual(shared);
	expect(JSON.parse(await runtime.ok(["target", "eval", sharedExpression, "--json-expect"]))).toEqual(shared);
	const specialKeys = '{"__proto__":{"literal":true},"constructor":"data","$dbgjs":{"kind":"promise"}}';
	expect(await runtime.json(["target", "eval", `JSON.parse(${JSON.stringify(specialKeys)})`])).toEqual(JSON.parse(specialKeys));
});

test("a promise can be awaited repeatedly without repeating application effects", async ({ runtime }) => {
	const initial = await runtime.ok(["target", "eval", `(
		globalThis.calls = (globalThis.calls || 0) + 1,
		new Promise(resolve => { globalThis.finish = resolve; })
	)`]);
	expect(initial).toMatch(/promise/i);
	expect(initial).toMatch(/pending/i);
	const promise = reference(initial, "v");
	expect(initial).toContain(`Still running; continue with: dbgjs value await ${promise} --timeout 2s`);
	const pending = await runtime.ok(["value", "await", promise, "--timeout", "100ms"]);
	expect(pending).toMatch(/pending/i);
	expect(pending).toContain(promise);

	const waiting = runtime.ok(["value", "await", promise, "--timeout", "5s", "--json"]);
	const progress = await runtime.json(["target", "eval", "(finish({ saved: true, version: 42 }), calls)"]);
	expect(progress).toBe(1);
	expect(JSON.parse(await waiting)).toEqual({ saved: true, version: 42 });
	expect(await runtime.json(["value", "await", promise, "--timeout", "100ms"])).toEqual({
		saved: true, version: 42,
	});
	expect(await runtime.json(["target", "eval", "calls"])).toBe(1);
});

test("explicit await preserves the distinction between expression completion and returned promises", async ({ runtime }) => {
	await runtime.ok(["target", "eval", `(
		globalThis.barCalls = 0,
		globalThis.fooCalls = 0,
		globalThis.bar = () => (barCalls++, new Promise(r => { globalThis.finishBar = r; })),
		globalThis.foo = value => (fooCalls++, new Promise(r => { globalThis.finishFoo = () => r(value + 1); })),
		true
	)`]);
	const suspended = await runtime.ok(["target", "eval", "foo(await bar())", "--timeout", "100ms"]);
	expect(suspended).toMatch(/evaluation.*pending/i);
	const evaluation = reference(suspended, "e");
	expect(await runtime.json(["target", "eval", "({ barCalls, fooCalls })"])).toEqual({ barCalls: 1, fooCalls: 0 });
	await runtime.ok(["target", "eval", "(finishBar(41), true)"]);
	const returned = await runtime.ok(["value", "await", evaluation, "--timeout", "5s"]);
	expect(returned).toMatch(/promise.*pending/i);
	const promise = reference(returned, "v");
	expect(await runtime.json(["target", "eval", "({ barCalls, fooCalls })"])).toEqual({ barCalls: 1, fooCalls: 1 });
	await runtime.ok(["target", "eval", "(finishFoo(), true)"]);
	expect(await runtime.json(["value", "await", promise, "--timeout", "5s"])).toBe(42);

	const explicit = await runtime.ok(["target", "eval", "await foo(await bar())", "--timeout", "100ms"]);
	const explicitRef = reference(explicit, "e");
	await runtime.ok(["target", "eval", "(finishBar(99), true)"]);
	const stillPending = await runtime.ok(["value", "await", explicitRef, "--timeout", "100ms"]);
	expect(stillPending).toContain(explicitRef);
	expect(stillPending).toMatch(/pending/i);
	await runtime.ok(["target", "eval", "(finishFoo(), true)"]);
	expect(await runtime.json(["value", "await", explicitRef, "--timeout", "5s"])).toBe(100);
	expect(await runtime.json(["target", "eval", "({ barCalls, fooCalls })"])).toEqual({ barCalls: 2, fooCalls: 2 });
});

test("await syntax detection ignores strings, comments, and nested async functions", async ({ runtime }) => {
	expect(await runtime.json(["target", "eval", "'await missing()'"])).toBe("await missing()");
	expect(await runtime.json(["target", "eval", "/* await missing() */ 7"])).toBe(7);
	expect(await runtime.json(["target", "eval", "(async () => await missing(), 8)"])).toBe(8);
	expect(await runtime.json(["target", "eval", "await Promise.resolve({ answer: 42 })"])).toEqual({ answer: 42 });
	expect(await runtime.json(["target", "eval", "Promise.resolve(42)", "--await"])).toBe(42);
	const object = await runtime.json(["target", "eval", "({ p: Promise.resolve(42) })"]);
	expect(object.p).not.toBe(42);
	expect(typeof object.p).toBe("object");
});

test("an evaluation continuation preserves an explicit --await request", async ({ runtime }) => {
	await runtime.ok(["target", "eval", `(
		globalThis.startCalls = 0,
		globalThis.first = () => new Promise(r => { globalThis.finishFirst = r; }),
		globalThis.last = value => (startCalls++, new Promise(r => { globalThis.finishLast = () => r(value); })),
		true
	)`]);
	const initial = await runtime.ok([
		"target", "eval", "last(await first())", "--await", "--timeout", "100ms",
	]);
	const evaluation = reference(initial, "e");
	await runtime.ok(["target", "eval", "(finishFirst({ result: 42 }), true)"]);
	const pending = await runtime.ok(["value", "await", evaluation, "--timeout", "100ms"]);
	expect(pending).toMatch(/pending/i);
	const continuation = reference(pending);
	await runtime.ok(["target", "eval", "(finishLast(), true)"]);
	expect(await runtime.json(["value", "await", continuation, "--timeout", "5s"])).toEqual({ result: 42 });
	expect(await runtime.json(["target", "eval", "startCalls"])).toBe(1);
});

test("completed evaluation continuations expose expandable children of the result", async ({ runtime }) => {
	const pending = await runtime.ok([
		"target", "eval", "await new Promise(r => { globalThis.finishObject = r; })", "--timeout", "100ms",
	]);
	const evaluation = reference(pending, "e");
	await runtime.ok(["target", "eval", "(finishObject({ child: { answer: 42 } }), true)"]);
	await runtime.ok(["value", "await", evaluation, "--timeout", "5s"]);
	const children = JSON.parse(await runtime.ok(["value", "children", evaluation, "--describe"]));
	const child = children.properties.find(property => property.name === "child").value.reference;
	expect(child).toMatch(/^@v/);
	expect(await runtime.json(["value", "show", child])).toEqual({ answer: 42 });
});

test("retained results expand the original object and release makes the reference stale", async ({ runtime }) => {
	const created = await runtime.ok(["target", "eval", `(
		globalThis.created = (globalThis.created || 0) + 1,
		{ id: created, document: { text: "hello" } }
	)`, "--retain"]);
	const value = reference(created, "v");
	const shown = await runtime.ok(["value", "show", value]);
	expect(shown).toContain("document");
	expect(shown).toContain("id");
	const children = await runtime.ok(["value", "children", value]);
	expect(children).toContain("document");
	expect(await runtime.json(["target", "eval", "created"])).toBe(1);
	await runtime.ok(["value", "release", value]);
	const stale = await runtime.command(["value", "show", value]);
	expect(stale.code).not.toBe(0);
	expect(stale.output).toMatch(/released|stale|unknown|expired/i);
	expect(await runtime.json(["target", "eval", "created"])).toBe(1);
});

test("JSON export rejects lossy values while inspection handles cycles and accessors without executing them", async ({ runtime }) => {
	await runtime.ok(["target", "eval", `(
		globalThis.getterCalls = 0,
		globalThis.toJsonCalls = 0,
		globalThis.cycle = { label: "cycle" },
		cycle.self = cycle,
		globalThis.accessor = { get danger() { getterCalls++; return 42; } },
		globalThis.custom = { toJSON() { toJsonCalls++; return 42; } },
		true
	)`]);
	for (const expression of ["cycle", "accessor", "custom", "1n", "undefined", "NaN", "Infinity", "({ a: undefined })", "[,1]"]) {
		const inspected = await runtime.json(["target", "eval", expression]);
		expect(inspected).toBeDefined();
		const exact = await runtime.command(["target", "eval", expression, "--json-expect"]);
		expect(exact.code, expression).not.toBe(0);
		expect(exact.stdout.trim(), expression).toBe("");
		expect(exact.stderr.trim(), expression).not.toBe("");
	}
	expect(await runtime.json(["target", "eval", "({ getterCalls, toJsonCalls })"])).toEqual({
		getterCalls: 0, toJsonCalls: 0,
	});
});

test("await failures preserve useful rejection details and never become successful JSON exports", async ({ runtime }) => {
	for (const expression of [
		"Promise.reject(new Error('save failed'))",
		"Promise.reject('save failed')",
		"await Promise.reject(new Error('save failed'))",
	]) {
		const rejected = await runtime.command(["target", "eval", expression, "--await", "--timeout", "2s", "--json-expect"]);
		expect(rejected.code, rejected.output).not.toBe(0);
		expect(rejected.output).toContain("save failed");
		expect(rejected.stdout.trim()).toBe("");
	}
	expect(await runtime.json(["target", "eval", "42"])).toBe(42);
});

test("paused top-level await is rejected without resuming or executing effects", async ({ runtime }) => {
	await runtime.ok(["target", "eval", "(globalThis.pausedEffects = 0, true)"]);
	await runtime.ok(["target", "cdp", "Debugger.pause"]);
	await runtime.ok(["target", "wait", "paused", "0", "5000"]);
	const rejected = await runtime.command(["target", "eval", "await (pausedEffects++, Promise.resolve(42))"]);
	expect(rejected.code).not.toBe(0);
	expect(rejected.output).toMatch(/paused|pause|frame/i);
	const target = await runtime.json(["target", "show"]);
	expect(target.pause).not.toBeNull();
	expect(await runtime.json(["target", "eval", "pausedEffects"])).toBe(0);
	await runtime.ok(["target", "resume"]);
	expect(await runtime.json(["target", "eval", "fixtureReady"])).toBe(true);
});

test("live references cannot resolve to objects from a replacement connection", async ({ runtime }) => {
	const initial = await runtime.ok(["target", "eval", "({ beforeDisconnect: true })", "--retain"]);
	const old = reference(initial, "v");
	await runtime.ok(["connection", "disconnect", "--connection", "runtime"]);
	await runtime.ok(["connection", "connect", "--connection", "runtime"]);
	await runtime.ok(["target", "attach", "--connection", "runtime"]);
	const replacement = await runtime.ok(["target", "eval", "({ replacement: true })", "--retain"]);
	expect(reference(replacement, "v")).not.toBe(old);
	const stale = await runtime.command(["value", "show", old]);
	expect(stale.code).not.toBe(0);
	expect(stale.output).toMatch(/stale|unknown|expired|invalid|belongs/i);
});

test("heap value previews remain captured after live mutation and service restart", async ({ runtime }) => {
	test.setTimeout(180_000);
	await runtime.ok(["target", "eval", `(
		globalThis.HeapValueFixture = class HeapValueFixture {
			constructor() { this.label = "captured-label"; this.child = { nested: "captured-child" }; }
		},
		globalThis.heapFixture = new HeapValueFixture(),
		true
	)`]);
	await runtime.ok(["heap", "capture", "--id", "values"], { timeoutMs: 60_000 });
	const selected = await runtime.json([
		"heap", "select", "values", "--type", "object", "--name", "HeapValueFixture", "--limit", "10",
	], { timeoutMs: 60_000 });
	const instance = selected.nodes.find((node) => node.preview?.includes("captured-label"));
	expect(instance, JSON.stringify(selected)).toBeDefined();
	await runtime.ok(["target", "eval", '(heapFixture.label = "changed-live", true)']);
	const enriched = await runtime.ok(["heap", "show", instance.reference]);
	expect(enriched).toContain("captured-label");
	expect(enriched).not.toContain("changed-live");
	await runtime.ok(["connection", "disconnect", "--connection", "runtime"]);
	await runtime.ok(["service", "stop"]);
	const offline = await runtime.ok(["heap", "show", instance.reference], { timeoutMs: 60_000 });
	expect(offline).toContain("captured-label");
	expect(offline).not.toContain("changed-live");
});

test("JSON limits are explicit and overridable without losing the retained value", async ({ runtime }) => {
	const expression = `({text: "x".repeat(4000), nested: {a: {b: 42}}})`;
	const bounded = await runtime.ok(["target", "eval", expression, "--json", "--max-output-bytes", "256"]);
	expect(Buffer.byteLength(bounded)).toBeLessThanOrEqual(256);
	expect(JSON.parse(bounded).$dbgjs.truncated).toBe(true);
	const exact = await runtime.command(["target", "eval", expression, "--json-expect", "--max-output-bytes", "256"]);
	expect(exact.code).not.toBe(0);
	expect(exact.stdout).toBe("");
	expect(JSON.parse(await runtime.ok([
		"target", "eval", expression, "--json-expect", "--max-output-bytes", "8192",
	]))).toEqual({ text: "x".repeat(4000), nested: { a: { b: 42 } } });
	const retained = reference(await runtime.ok(["target", "eval", "'x'.repeat(1000)", "--retain", "--max-string-length", "10"]));
	expect(await runtime.json(["value", "show", retained, "--max-string-length", "2000"])).toBe("x".repeat(1000));
	const excessive = await runtime.command(["target", "eval", "1", "--max-depth", "1000000"]);
	expect(excessive.code).not.toBe(0);
});

test("settled object references stay inspectable with different limits and explicit release", async ({ runtime }) => {
	const initial = await runtime.ok(["target", "eval", "Promise.resolve({ child: { nested: 42 } })"]);
	const promise = reference(initial);
	const settled = await runtime.ok(["value", "await", promise, "--max-depth", "1"]);
	const result = reference(settled);
	expect(result).not.toBe(promise);
	expect(await runtime.json(["value", "show", result, "--max-depth", "8"])).toEqual({ child: { nested: 42 } });
	expect(await runtime.json(["value", "await", promise, "--max-depth", "8"])).toEqual({ child: { nested: 42 } });
	await runtime.ok(["value", "release", promise]);
	expect(await runtime.json(["value", "show", result])).toEqual({ child: { nested: 42 } });
});

test("references acquired while paused expire on resume without invalidating running references", async ({ runtime }) => {
	const running = reference(await runtime.ok(["target", "eval", "({ running: true })", "--retain"]));
	await runtime.ok(["target", "cdp", "Debugger.pause"]);
	await runtime.ok(["target", "wait", "paused", "0", "5000"]);
	const paused = reference(await runtime.ok(["target", "eval", "({ paused: true })", "--retain"]));
	await runtime.ok(["target", "resume"]);
	const stale = await runtime.command(["value", "show", paused]);
	expect(stale.code).not.toBe(0);
	expect(stale.output).toMatch(/pause|stale|expired/i);
	expect(await runtime.json(["value", "show", running])).toEqual({ running: true });
});

test("child pages preserve expandable references and application effects are not repeated", async ({ runtime }) => {
	const root = reference(await runtime.ok(["target", "eval", "[{a:1},{b:2},{c:3}]", "--retain"]));
	const first = JSON.parse(await runtime.ok(["value", "children", root, "--max-properties", "2", "--describe"]));
	expect(first.nextStart).toBe(2);
	expect(first.properties.map((p) => p.name)).toEqual(["0", "1"]);
	expect(await runtime.json(["value", "show", first.properties[0].value.reference])).toEqual({ a: 1 });
	const last = JSON.parse(await runtime.ok(["value", "children", root, "--max-properties", "2", "--start", "2", "--describe"]));
	expect(last.nextStart).toBeNull();
	expect(last.properties.map((p) => p.name)).toEqual(["2"]);
	expect(await runtime.json(["value", "show", last.properties[0].value.reference])).toEqual({ c: 3 });
});

test("synchronous execution deadline reports failure and the next CLI command still works", async ({ runtime }) => {
	const failed = await runtime.command(["target", "eval", "(() => { while (true) {} })()"]);
	expect(failed.code).not.toBe(0);
	expect(failed.output).toMatch(/timed out|terminated/i);
	expect(failed.durationMs).toBeLessThan(5000);
	expect(await runtime.json(["target", "eval", "42"])).toBe(42);
});

test("strict pending export keeps continuation available in stderr", async ({ runtime }) => {
	const failed = await runtime.command([
		"target", "eval", "new Promise(r => { globalThis.finishExact = r; })",
		"--await", "--timeout", "100ms", "--json-expect",
	]);
	expect(failed.code).not.toBe(0);
	expect(failed.stdout).toBe("");
	const promise = reference(failed.stderr);
	await runtime.ok(["target", "eval", "(finishExact(42), true)"]);
	expect(await runtime.json(["value", "await", promise])).toBe(42);
});
