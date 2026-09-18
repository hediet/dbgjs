import assert from "node:assert/strict";
import { appendFile, mkdir, writeFile } from "node:fs/promises";
import { join } from "node:path";
import { run } from "../playwright/live-test-harness.mjs";
import { normalize } from "./transcript.mjs";

export async function createRecorder({ cli, environment, output, name, replacements = [] }) {
	await mkdir(output, { recursive: true });
	const log = join(output, `${name}-commands.jsonl`);
	await writeFile(log, "");
	const steps = [];
	async function execute(args) {
		console.log(`> dbgjs ${args.join(" ")}`);
		const result = await run(cli, args, environment, { timeoutMs: 240_000 });
		await appendFile(log, JSON.stringify({ args, ...result }) + "\n");
		assert.equal(result.code, 0, `${args.join(" ")}\n${result.output}`);
		return result;
	}
	async function command(id, args, comparison, verify, options = {}) {
		assert.ok(!steps.some((step) => step.id === id), `Duplicate recorded command ${id}`);
		const result = await execute(args);
		await verify(result.stdout);
		steps.push({
			id,
			args: args.map((arg) => normalize(arg, replacements, { argument: true })),
			output: normalize(result.stdout, replacements),
			stderr: normalize(result.stderr, replacements),
			comparison,
			...options,
		});
		await writeFile(join(output, `${name}-recording.json`), JSON.stringify(steps, null, 2) + "\n");
		return result.stdout;
	}
	async function json(args) {
		return JSON.parse((await execute(["--json", ...args])).stdout);
	}
	return { steps, command, execute, json };
}
