import assert from "node:assert/strict";
import { readFile } from "node:fs/promises";
import test from "node:test";
import { platforms, platformKey } from "../npm/dbgjs/lib/platform.mjs";

test("the entry package exposes the dbgjs CLI and TUI launchers", async () => {
	const manifest = JSON.parse(await readFile(new URL("../npm/dbgjs/package.json", import.meta.url)));
	assert.equal(manifest.name, "@hediet/dbgjs");
	assert.deepEqual(manifest.bin, {
		dbgjs: "bin/dbgjs.mjs",
		"dbgjs-tui": "bin/dbgjs-tui.mjs",
	});
	for (const [binary, path] of Object.entries(manifest.bin)) {
		const launcher = await readFile(new URL(`../npm/dbgjs/${path}`, import.meta.url), "utf8");
		assert.ok(launcher.includes(`launch("${binary}")`));
	}
});

test("every supported host resolves to its exact optional dependency", async () => {
	const manifest = JSON.parse(await readFile(new URL("../npm/dbgjs/package.json", import.meta.url)));
	for (const [key, value] of Object.entries(platforms)) {
		assert.equal(platformKey(value.os, value.cpu, value.libc), key);
		assert.equal(manifest.optionalDependencies[`@hediet/dbgjs-${key}`], manifest.version);
	}
	assert.equal(Object.keys(manifest.optionalDependencies).length, Object.keys(platforms).length);
});

test("unsupported platforms and musl fail explicitly", () => {
	for (const args of [["win32", "arm64"], ["darwin", "x64"], ["linux", "x64"], ["linux", "riscv64", "2.35"], ["freebsd", "x64"]]) {
		assert.throws(() => platformKey(...args), /Unsupported dbgjs platform/);
	}
});
