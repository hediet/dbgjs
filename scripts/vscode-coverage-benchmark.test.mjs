import assert from "node:assert/strict";
import { mkdir, mkdtemp, realpath, rm, writeFile } from "node:fs/promises";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { test } from "node:test";
import { pathToFileURL } from "node:url";
import { extractWorkload, installedBundlePath, isolatedEnvironment } from "./vscode-coverage-benchmark.mjs";
import { run } from "../tests/playwright/live-test-harness.mjs";

test("removes inherited per-process Git configuration without changing the parent environment", () => {
	const original = { PATH: "path", GIT_CONFIG_COUNT: "1", GIT_CONFIG_KEY_0: "key", GIT_CONFIG_VALUE_0: "", ELECTRON_RUN_AS_NODE: "1", VSCODE_PORTABLE: "portable-data" };
	const cleaned = isolatedEnvironment(original);
	assert.equal(cleaned.PATH, "path");
	assert.equal(cleaned.GIT_CONFIG_COUNT, undefined);
	assert.equal(cleaned.GIT_CONFIG_KEY_0, undefined);
	assert.equal(cleaned.GIT_CONFIG_VALUE_0, undefined);
	assert.equal(cleaned.ELECTRON_RUN_AS_NODE, undefined);
	assert.equal(cleaned.VSCODE_PORTABLE, undefined);
	assert.equal(original.GIT_CONFIG_COUNT, "1");
});

function snapshot() {
	return {
		sources: [{
			generatedUrl: "vscode-file://vscode-app/c:/Code/resources/app/out/vs/workbench/workbench.desktop.main.js",
			functions: [
				{ name: "revertRangeMappings", authoredLocation: { sourceUrl: "../../../src/vs/editor/browser/widget/diffEditor/diffEditorWidget.ts" }, ranges: [{ count: 1 }] },
				{ name: "fallback", authoredLocation: null, generatedLocation: { line: 10, column: 20 }, ranges: [{ count: 0 }] },
				{ name: "duplicate", authoredLocation: null, generatedLocation: { line: 10, column: 20 }, ranges: [{ count: 1 }] },
			],
		}],
	};
}

test("preserves zero-hit and repeated fallback lookups from the real projection workload", () => {
	assert.deepEqual(extractWorkload(snapshot()).lookups, [{ line: 10, column: 20 }, { line: 10, column: 20 }]);
});

test("rejects captures without mapped revert execution or a fallback workload", () => {
	const input = snapshot();
	input.sources[0].functions[0].ranges[0].count = 0;
	assert.throws(() => extractWorkload(input), /Revert must execute/);
	input.sources[0].functions[0].ranges[0].count = 1;
	input.sources[0].functions.length = 1;
	assert.throws(() => extractWorkload(input), /No generated-fallback/);
});

test("rejects ambiguous bundles and invalid coordinates", () => {
	const input = snapshot();
	input.sources.push(input.sources[0]);
	assert.throws(() => extractWorkload(input), /Expected one/);
	input.sources.pop();
	input.sources[0].functions[1].generatedLocation.column = 0;
	assert.throws(() => extractWorkload(input), /positive generated coordinates/);
});

test("resolves installed bundle URLs but rejects files outside the installation", async (t) => {
	const root = await mkdtemp(join(tmpdir(), "dbgjs-bundle-path-"));
	t.after(() => rm(root, { recursive: true }));
	const installation = join(root, "installed code");
	const executable = process.platform === "darwin"
		? join(installation, "MacOS", "Electron")
		: join(installation, "Code");
	const bundle = join(installation, "resources", "workbench.desktop.main.js");
	await mkdir(join(installation, "resources"), { recursive: true });
	if (process.platform === "darwin") await mkdir(join(installation, "MacOS"));
	await writeFile(bundle, "fixture");
	const url = pathToFileURL(bundle);
	const vscodeUrl = `vscode-file://vscode-app${url.pathname}`;
	assert.equal(await installedBundlePath(vscodeUrl, executable), await realpath(bundle));
	assert.equal(await installedBundlePath(url.href, executable), await realpath(bundle));
	const outside = join(root, "outside.js");
	await writeFile(outside, "outside");
	await assert.rejects(installedBundlePath(pathToFileURL(outside).href, executable), /must belong/);
	await assert.rejects(installedBundlePath("https://example.invalid/bundle.js", executable), /local installed/);
});

test("terminal helper keeps progress stderr separate from machine-readable stdout", async () => {
	const result = await run(process.execPath, ["-e", 'console.log(JSON.stringify({ count: 1 })); console.error("progress hint");'], {});
	assert.equal(result.code, 0);
	assert.deepEqual(JSON.parse(result.stdout), { count: 1 });
	assert.match(result.stderr, /progress hint/);
	assert.match(result.output, /progress hint/);
});
