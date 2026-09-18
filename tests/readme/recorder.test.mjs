import assert from "node:assert/strict";
import { mkdtemp, readFile, rm } from "node:fs/promises";
import { tmpdir } from "node:os";
import { join } from "node:path";
import test from "node:test";
import { createRecorder } from "./recorder.mjs";

test("recorder retains raw external commands and invocation order", async () => {
	const output = await mkdtemp(join(tmpdir(), "dbgjs-recorder-test-"));
	try {
		const recorder = await createRecorder({
			cli: process.execPath, environment: {}, output, name: "demo",
			replacements: [["1234", "PID"]],
		});
		const options = { executable: process.execPath, displayExecutable: "node" };
		const slow = ["-e", 'setTimeout(() => console.log("Process 1234"), 150)'];
		await Promise.all([
			recorder.command("slow", slow, "exact",
				(text) => assert.equal(text.trim(), "Process 1234"), options),
			recorder.command("fast", ["-e", 'console.log("second")'], "exact",
				(text) => assert.equal(text.trim(), "second"), options),
		]);
		assert.deepEqual(recorder.steps.map((step) => step.id), ["slow", "fast"]);
		assert.deepEqual(recorder.steps[0].args, slow);
		assert.equal(recorder.steps[0].output, "Process 1234\n");
		assert.equal(recorder.steps[0].executable, "node");
		assert.equal(recorder.steps[0].scenario, "demo");
		assert.deepEqual(JSON.parse(await readFile(join(output, "demo-recording.json"), "utf8")), recorder.steps);
		const log = (await readFile(join(output, "demo-commands.jsonl"), "utf8")).trim().split("\n").map(JSON.parse);
		assert.ok(log.some((entry) => entry.stdout === "Process 1234\n" && entry.executable === process.execPath));
		await assert.rejects(recorder.command("failure", ["-e", "process.exit(3)"], "exact",
			() => assert.fail("Failed commands must not reach verification."), options));
	} finally {
		await rm(output, { recursive: true, force: true });
	}
});
