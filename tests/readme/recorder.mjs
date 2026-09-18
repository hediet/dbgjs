import assert from "node:assert/strict";
import { appendFile, mkdir, writeFile } from "node:fs/promises";
import { join } from "node:path";
import { run } from "../playwright/live-test-harness.mjs";

export async function createRecorder({ cli, environment, output, name, replacements = [] }) {
	await mkdir(output, { recursive: true });
	const log = join(output, `${name}-commands.jsonl`);
	await writeFile(log, "");
	const steps = [];
	let saved = Promise.resolve();
	async function execute(args, executable = cli, displayExecutable = "dbgjs") {
		console.log(`> ${displayExecutable} ${args.join(" ")}`);
		const result = await run(executable, args, environment, { timeoutMs: 240_000 });
		await appendFile(log, JSON.stringify({ executable, args, ...result }) + "\n");
		assert.equal(result.code, 0, `${args.join(" ")}\n${result.output}`);
		return result;
	}
	async function command(id, args, comparison, verify, options = {}) {
		assert.ok(!steps.some((step) => step.id === id), `Duplicate recorded command ${id}`);
		const { executable = cli, displayExecutable = executable === cli ? "dbgjs" : executable, ...displayOptions } = options;
		const step = {
			id,
			scenario: name,
			executable: displayExecutable,
			args,
			output: "",
			stderr: "",
			comparison,
			...displayOptions,
		};
		steps.push(step);
		const result = await execute(args, executable, displayExecutable);
		step.output = result.stdout;
		step.stderr = result.stderr;
		await verify(result.stdout);
		saved = saved.then(() => writeFile(join(output, `${name}-recording.json`), JSON.stringify(steps, null, 2) + "\n"));
		await saved;
		return result.stdout;
	}
	async function json(args) {
		return JSON.parse((await execute(["--json", ...args])).stdout);
	}
	return { steps, command, execute, json, replacements };
}
