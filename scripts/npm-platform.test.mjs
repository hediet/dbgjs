import assert from "node:assert/strict";
import { readFile } from "node:fs/promises";
import test from "node:test";
import { platforms, platformKey } from "../npm/jsdbg/lib/platform.mjs";

test("every supported host resolves to its exact optional dependency", async () => {
	const manifest = JSON.parse(await readFile(new URL("../npm/jsdbg/package.json", import.meta.url)));
	for (const [key, value] of Object.entries(platforms)) {
		assert.equal(platformKey(value.os, value.cpu, value.libc), key);
		assert.equal(manifest.optionalDependencies[`@hediet/jsdbg-${key}`], manifest.version);
	}
	assert.equal(Object.keys(manifest.optionalDependencies).length, Object.keys(platforms).length);
});

test("unsupported platforms and musl fail explicitly", () => {
	for (const args of [["win32", "arm64"], ["linux", "x64"], ["linux", "riscv64", "2.35"], ["freebsd", "x64"]]) {
		assert.throws(() => platformKey(...args), /Unsupported jsdbg platform/);
	}
});
