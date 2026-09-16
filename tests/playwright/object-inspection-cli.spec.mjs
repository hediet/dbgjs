import { mkdtemp, rm, writeFile } from "node:fs/promises";
import { tmpdir } from "node:os";
import { join, resolve } from "node:path";
import { build } from "esbuild";
import { expect, test } from "@playwright/test";
import { run } from "./live-test-harness.mjs";

test("heap and live inspection resolve mapped locations and bounded property previews", async () => {
	test.setTimeout(300_000);
	const built = await run("cargo", ["build", "--bins"], {}, { timeoutMs: 180_000 });
	expect(built.code, built.output).toBe(0);
	const suffix = process.platform === "win32" ? ".exe" : "";
	const targetDirectory = process.env.CARGO_TARGET_DIR ?? resolve("target");
	const cli = join(targetDirectory, "debug", `dbgjs${suffix}`);
	const directory = await mkdtemp(join(tmpdir(), "dbgjs-object-inspection-"));
	const source = join(directory, "provider-with-resolvable-full-path.js");
	const bundle = join(directory, "provider.min.cjs");
	const environment = {
		DBGJS_SERVICE_EXE: join(targetDirectory, "debug", `dbgjs-service${suffix}`),
		DBGJS_SERVICE_STATE: join(directory, "state", "service.json"),
	};
	const command = async (args) => {
		const result = await run(cli, args, environment, { timeoutMs: 90_000 });
		expect(result.code, result.output).toBe(0);
		return result.stdout;
	};
	const json = async (args) => JSON.parse(await command(["--json", ...args]));
	await writeFile(source, `
function provideModels() { return "haiku"; }
class Provider {
  constructor() {
    this.name = "Claude Haiku 4.5";
    this.description = "0123456789012345678901234567890123456789";
    this.self = this;
  }
}
globalThis.__getterCalls = 0;
Object.defineProperty(Provider.prototype, "expensive", { value: "x".repeat(5_000_000) });
Object.defineProperty(Provider.prototype, "trap", { get() { globalThis.__getterCalls++; throw new Error("getter invoked"); } });
globalThis.__inspection = { fn: provideModels, bound: provideModels.bind(null), object: new Provider() };
globalThis.__inspection.inherited = Object.create(globalThis.__inspection.object);
const accessorPrototype = Object.create(null);
Object.defineProperty(accessorPrototype, "constructor", { get() { globalThis.__getterCalls++; throw new Error("constructor getter invoked"); } });
globalThis.__inspection.accessorConstructor = Object.create(accessorPrototype);
setInterval(() => {}, 1000);
`);
	await build({
		entryPoints: [source], outfile: bundle, platform: "node", format: "cjs",
		bundle: true, minifyWhitespace: true, sourcemap: true,
	});
	try {
		await command(["context", "create", ":object-inspection", "--set"]);
		await command([
			"connection", "add", "--node", bundle, "--connection", "runtime",
			"--runtime-executable", process.execPath, "--connect",
		]);
		await command(["target", "attach", "--connection", "runtime", "--set"]);
		await command(["target", "release"]);
		await expect(async () => {
			const ready = await json(["value", "typeof globalThis.__inspection"]);
			expect(ready.preview.preview).toContain("object");
		}).toPass({ timeout: 15_000 });

		for (const [expression, kind] of [
			["globalThis.__inspection.fn", "function"],
			["globalThis.__inspection.bound", "boundTarget"],
			["globalThis.__inspection.object", "constructor"],
			["globalThis.__inspection.inherited", "constructor"],
		]) {
			const live = await json(["value", expression]);
			const location = live.preview.source.locations.find((location) => location.kind === kind);
			expect(location, JSON.stringify(live)).toBeDefined();
			expect(location.origin).toBe("live");
			expect(location.position.mapping).toBe("authored");
			expect(location.position.resolved.sourceUrl).toContain("provider-with-resolvable-full-path.js");
			expect(location.position.resolved.sourceUrl).not.toContain("...");
			const printed = await command(["value", expression]);
			expect(printed).toContain(location.position.resolved.sourceUrl);
			expect(printed.length).toBeLessThan(20_000);
			await command(["source", "show", location.position.resolved.sourceUrl, "--line", String(location.position.resolved.line), "--context-lines", "1"]);
		}
		const getters = await json(["value", "globalThis.__getterCalls"]);
		expect(getters.preview.preview).toBe("0");
		const evaluated = await json(["target", "eval", "globalThis.__inspection.fn"]);
		expect(evaluated.preview.source.locations.some((location) =>
			location.position.resolved.sourceUrl.includes("provider-with-resolvable-full-path.js"),
		)).toBe(true);
		expect(evaluated.preview.reference).toBeNull();
		await json(["value", "globalThis.__inspection.accessorConstructor"]);
		const getterCheck = await json(["value", "globalThis.__getterCalls"]);
		expect(getterCheck.preview.preview).toBe("0");

		await command(["heap", "capture", "--id", "objects"]);
		const functions = await json(["heap", "select", "objects", "--type", "closure", "--name", "provideModels", "--limit", "10"]);
		const fn = functions.nodes.find((node) => node.source.locations.some((location) => location.origin === "heapSnapshot"));
		expect(fn, JSON.stringify(functions)).toBeDefined();
		const shown = await command(["heap", "show", fn.reference]);
		expect(shown).toContain("provider-with-resolvable-full-path.js");
		const heapSource = fn.source.locations.find((location) => location.origin === "heapSnapshot");
		expect(shown).toContain(heapSource.position.resolved.sourceUrl);
		expect(shown).toContain("source [heapSnapshot; function; authored]");
		expect(shown).not.toContain("source [live; function; authored]");
		const functionDetails = await json(["heap", "show", fn.reference]);
		const prototype = functionDetails.references.find((reference) => reference.name === "prototype");
		expect(prototype).toBeDefined();
		const prototypeDetails = await json(["heap", "show", prototype.target]);
		const constructor = prototypeDetails.references.find((reference) => reference.name === "constructor");
		expect(constructor).toBeDefined();
		expect(constructor.targetLocations).toBeDefined();
		expect(constructor.targetLocations.locations.some((location) =>
			location.position.resolved.sourceUrl === heapSource.position.resolved.sourceUrl,
		)).toBe(true);
		const prototypeText = await command(["heap", "show", prototype.target]);
		const constructorLine = prototypeText.split("\n").findIndex((line) => line.includes('property "constructor"'));
		expect(prototypeText.split("\n").slice(constructorLine, constructorLine + 4).join("\n"))
			.toContain(heapSource.position.resolved.sourceUrl);

		const objects = await json(["heap", "select", "objects", "--type", "object", "--name", "Provider", "--limit", "10"]);
		const instance = objects.nodes.find((node) => node.source.locations.some((location) => location.kind === "constructor"));
		expect(instance, JSON.stringify(objects)).toBeDefined();
		const properties = await command(["heap", "show", instance.reference]);
		expect(properties).toContain("Claude Haiku 4.5");
		expect(properties).toContain("01234567890123456789");
		expect(properties).not.toContain("0123456789012345678901234567890123456789");
		expect(properties).toContain(instance.reference);
		expect(properties).toContain("source [heapSnapshot; constructor; authored]");
		expect(properties).not.toContain("source [live; constructor; authored]");
		expect(properties.length).toBeLessThan(20_000);
	} finally {
		await run(cli, ["context", "delete", "--context", ":object-inspection"], environment);
		await run(cli, ["service", "stop"], environment);
		await rm(directory, { recursive: true, force: true });
	}
});
