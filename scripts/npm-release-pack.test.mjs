import assert from "node:assert/strict";
import { execFileSync } from "node:child_process";
import { mkdir, mkdtemp, readFile, readdir, rename, rm, writeFile } from "node:fs/promises";
import { tmpdir } from "node:os";
import { join } from "node:path";
import test from "node:test";
import { platforms } from "../npm/dbgjs/lib/platform.mjs";
import { inspectCandidatePackages, prepareReleasePackages } from "./npm-release-pack.mjs";

test("candidate packages become stable and nightly tarballs with matching dependencies and unchanged binaries", async () => {
	const directory = await mkdtemp(join(tmpdir(), "dbgjs-release-test-"));
	const input = join(directory, "input");
	const fixture = join(directory, "fixture");
	const packageDirectory = join(fixture, "package");
	const entry = JSON.parse(await readFile(new URL("../npm/dbgjs/package.json", import.meta.url), "utf8"));
	try {
		await mkdir(input);
		await mkdir(join(packageDirectory, "bin"), { recursive: true });
		await writeFile(join(packageDirectory, "bin", "dbgjs"), "binary fixture\n", { mode: 0o755 });
		for (const [key, manifest] of [
			["dbgjs", entry],
			...Object.entries(platforms).map(([key, platform]) => [key, {
				name: `@hediet/dbgjs-${key}`, version: entry.version, os: [platform.os], cpu: [platform.cpu],
			}]),
		]) {
			await writeFile(join(packageDirectory, "package.json"), JSON.stringify({
				...manifest, private: true, files: ["bin"],
			}));
			execFileSync("tar", ["-czf", join(input, `${key}.tar.gz`), "-C", fixture, "package"]);
		}
		assert.equal((await inspectCandidatePackages(input)).packages.length, 5);
		for (const [version, tag, channel] of [
			[entry.version, "latest", "stable"],
			[`${entry.version}-nightly.20260915.1`, "next", "nightly"],
		]) {
			const result = await prepareReleasePackages({ input, output: join(directory, "output"), version, tag });
			assert.equal(result.packages.length, 5);
			for (const packed of result.packages) {
				const manifest = JSON.parse(execFileSync("tar", ["-xOzf", packed.path, "package/package.json"], { encoding: "utf8" }));
				assert.equal(manifest.private, undefined);
				assert.equal(manifest.version, version);
				assert.equal(manifest.publishConfig.tag, tag);
				assert.ok(packed.path.includes(`npm-${channel}-`));
				assert.ok(packed.filename.endsWith(`-${version}.tgz`));
				assert.equal(execFileSync("tar", ["-xOzf", packed.path, "package/bin/dbgjs"], { encoding: "utf8" }), "binary fixture\n");
				if (manifest.name === "@hediet/dbgjs") {
					assert.deepEqual(Object.values(manifest.optionalDependencies), Object.keys(platforms).map(() => version));
				}
			}
		}
		for (const [version, tag] of [
			[entry.version, "next"],
			[`${entry.version}-nightly.123`, "next"],
			[`${entry.version}-nightly.20260915.0`, "next"],
			[`${entry.version}-nightly.20260915.01`, "next"],
			[`${entry.version}-nightly.20260915.1`, "nightly"],
			[`${entry.version}-nightly.20260915.1`, "latest"],
		]) {
			await assert.rejects(prepareReleasePackages({ input, output: directory, version, tag }), /must agree/);
		}
		const [filename] = await readdir(input);
		await rename(join(input, filename), join(directory, filename));
		await assert.rejects(inspectCandidatePackages(input), /Missing candidates/);
	} finally {
		await rm(directory, { recursive: true, force: true });
	}
});
