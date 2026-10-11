import { spawn } from "node:child_process";
import { once } from "node:events";
import { mkdtemp, readFile, rm } from "node:fs/promises";
import { tmpdir } from "node:os";
import { join, resolve } from "node:path";
import { createInterface } from "node:readline";
import { expect, test } from "@playwright/test";
import { run } from "./live-test-harness.mjs";

test("process attachment failures retain usable connections without changing selection", async ({}, testInfo) => {
	test.setTimeout(180_000);
	const binaries = process.env.DBGJS_TEST_BIN_DIR ?? "target/debug";
	const suffix = process.platform === "win32" ? ".exe" : "";
	const cli = resolve(binaries, `dbgjs${suffix}`);
	const directory = await mkdtemp(join(tmpdir(), "dbgjs-process-attach-"));
	const environment = {
		DBGJS_SERVICE_EXE: resolve(binaries, `dbgjs-service${suffix}`),
		DBGJS_SERVICE_STATE: join(directory, "service.json"),
	};
	const fixture = spawn(process.execPath, ["-e", await readFile("tests/playwright/fixtures/process-attach.cjs", "utf8")],
		{ stdio: ["ignore", "pipe", "inherit"] });
	const exited = once(fixture, "exit");
	const lines = createInterface({ input: fixture.stdout });
	const transcript = [];
	const command = async args => {
		const result = await run(cli, args, environment, { timeoutMs: 45_000 });
		transcript.push({ args, ...result });
		expect(result.timedOut, result.output).toBe(false);
		return result;
	};
	const json = async args => {
		const result = await command(["--json", ...args]);
		expect(result.code, result.output).toBe(0);
		return JSON.parse(result.stdout);
	};
	try {
		const [port] = await once(lines, "line");
		await json(["context", "create", ":attach", "--set"]);
		const selectionFile = join(directory, "service.selection.json");
		const selection = await readFile(selectionFile, "utf8");
		const connectionId = `process-${fixture.pid}`;
		for (const [attempt, prefix] of [[0, []], [1, ["--json"]]]) {
			const failed = await command([...prefix, "process", "attach", String(fixture.pid), "--set"]);
			expect(failed.code).not.toBe(0);
			expect(failed.stdout).toBe("");
			expect(failed.stderr).toContain("fixture attachment denied");
			expect(failed.stderr).toContain(attempt === 0 ? "new connection" : "reused connection");
			expect(failed.stderr).toContain(connectionId);
			expect(failed.stderr).toContain("Available targets:");
			expect(failed.stderr).toContain("dbgjs target list");
			const context = await json(["context", "show"]);
			expect(context.connections.find(c => c.id === connectionId).status.kind).toBe("connected");
			const available = await json(["target", "list", "--context", ":attach", "--connection", connectionId]);
			expect(available.targets.length).toBeGreaterThan(0);
			expect(await readFile(selectionFile, "utf8")).toBe(selection);
		}
		expect((await fetch(`http://127.0.0.1:${port}/allow-debugger`)).status).toBe(200);
		await json(["process", "attach", String(fixture.pid), "--set"]);
		expect(await json(["target", "eval", "6 * 7"])).toBe(42);
		await json(["process", "attach", String(fixture.pid), "--set"]);
		await json(["connection", "disconnect", "--connection", connectionId]);
		await json(["connection", "delete", "--connection", connectionId]);
		await json(["process", "attach", String(fixture.pid), "--set"]);
		const selected = await readFile(selectionFile, "utf8");
		await command(["service", "stop"]);
		fixture.kill();
		await exited;
		const unavailable = await command(["process", "attach", String(fixture.pid), "--set"]);
		expect(unavailable.code).not.toBe(0);
		expect(unavailable.stderr).not.toContain("Available targets:");
		expect(unavailable.stderr).toContain("process connection setup failed");
		expect(await readFile(selectionFile, "utf8")).toBe(selected);
	} finally {
		await run(cli, ["service", "stop"], environment, { timeoutMs: 10_000 });
		lines.close();
		if (fixture.exitCode === null && fixture.signalCode === null) {
			fixture.kill();
			await exited;
		}
		await testInfo.attach("cli-transcript", {
			body: Buffer.from(JSON.stringify(transcript, null, 2)), contentType: "application/json",
		});
		await rm(directory, { recursive: true, force: true });
	}
});
