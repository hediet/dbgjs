import assert from "node:assert/strict";
import { execFileSync } from "node:child_process";
import { access } from "node:fs/promises";
import { dirname, join } from "node:path";

export async function resolveCodeExecutable(downloadedPath, platform = process.platform, readBundleExecutable = readPlistExecutable) {
	let executable = downloadedPath;
	if (platform === "darwin") {
		const contents = dirname(dirname(downloadedPath));
		const name = readBundleExecutable(join(contents, "Info.plist")).trim();
		assert.ok(name && !/[\\/]/.test(name) && name !== "." && name !== "..",
			"CFBundleExecutable must be a filename.");
		executable = join(contents, "MacOS", name);
	}
	await access(executable);
	return executable;
}

function readPlistExecutable(plist) {
	return execFileSync("/usr/bin/plutil", ["-extract", "CFBundleExecutable", "raw", "-o", "-", plist], {
		encoding: "utf8", timeout: 10_000,
	});
}
