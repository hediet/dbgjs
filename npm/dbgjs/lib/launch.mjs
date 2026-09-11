import { spawnSync } from "node:child_process";
import { createRequire } from "node:module";
import { dirname, join } from "node:path";
import { platformKey } from "./platform.mjs";

const require = createRequire(import.meta.url);

export function launch(binary) {
	try {
		const key = platformKey(
			process.platform,
			process.arch,
			process.platform === "linux" ? process.report.getReport().header.glibcVersionRuntime : undefined,
		);
		const packageName = `@hediet/dbgjs-${key}`;
		let manifest;
		try {
			manifest = require.resolve(`${packageName}/package.json`);
		} catch (error) {
			if (error.code !== "MODULE_NOT_FOUND") throw error;
			throw new Error(`Missing ${packageName}. Reinstall @hediet/dbgjs with optional dependencies enabled. For unpublished builds, install the matching platform .tgz alongside the entry-package .tgz.`, { cause: error });
		}
		const entryVersion = require("../package.json").version;
		if (require(manifest).version !== entryVersion) {
			throw new Error(`${packageName} must match @hediet/dbgjs version ${entryVersion}. Reinstall both packages together.`);
		}
		const executable = join(dirname(manifest), "bin", `${binary}${process.platform === "win32" ? ".exe" : ""}`);
		const result = spawnSync(executable, process.argv.slice(2), {
			stdio: "inherit",
			env: {
				...process.env,
				DBGJS_NODE: process.env.DBGJS_NODE ?? process.execPath,
				DBGJS_PLAYWRIGHT_PACKAGE: process.env.DBGJS_PLAYWRIGHT_PACKAGE ?? join(dirname(require.resolve("playwright/package.json")), "index.mjs"),
			},
		});
		if (result.error) throw result.error;
		if (result.signal) process.kill(process.pid, result.signal);
		process.exit(result.status ?? 1);
	} catch (error) {
		console.error(`dbgjs: ${error.message}`);
		process.exitCode = 1;
	}
}
