import assert from "node:assert/strict";
import { execFileSync } from "node:child_process";
import { mkdir, mkdtemp, readFile, readdir, rename, rm, writeFile } from "node:fs/promises";
import { tmpdir } from "node:os";
import { join } from "node:path";
import test from "node:test";
import { platforms } from "../dbgjs/lib/platform.mjs";
import { inspectCandidatePackages, prepareReleasePackages } from "./release-pack.mjs";

const gitHead = "1234567890abcdef1234567890abcdef12345678";

test("candidate packages become stable and nightly tarballs with matching dependencies and unchanged binaries", async () => {
	const directory = await mkdtemp(join(tmpdir(), "dbgjs-release-test-"));
	const input = join(directory, "input");
	const fixture = join(directory, "fixture");
	const packageDirectory = join(fixture, "package");
	const entry = JSON.parse(await readFile(new URL("../dbgjs/package.json", import.meta.url), "utf8"));
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
				...manifest, private: true, files: ["bin"], gitHead, gitDirty: false,
			}));
			execFileSync("tar", ["-czf", join(input, `${key}.tar.gz`), "-C", fixture, "package"]);
		}
		const packageCount = Object.keys(platforms).length + 1;
		const inspected = await inspectCandidatePackages(input);
		assert.equal(inspected.packages.length, packageCount);
		assert.equal(inspected.gitHead, gitHead);
		assert.equal(inspected.gitDirty, false);
		for (const [version, tag, channel] of [
			[entry.version, "latest", "stable"],
			[`${entry.version}-next.20260915.2`, "next", "nightly"],
			[`${entry.version}-nightly.20260915.1`, "next", "nightly"],
		]) {
			const result = await prepareReleasePackages({ input, output: join(directory, "output"), version, tag });
			assert.equal(result.packages.length, packageCount);
			assert.equal(result.gitHead, gitHead);
			assert.equal(result.gitDirty, false);
			for (const packed of result.packages) {
				const manifest = JSON.parse(execFileSync("tar", ["-xOzf", packed.path, "package/package.json"], { encoding: "utf8" }));
				assert.equal(manifest.private, undefined);
				assert.equal(manifest.version, version);
				assert.equal(manifest.gitHead, gitHead);
				assert.equal(manifest.gitDirty, false);
				assert.equal(manifest.publishConfig.tag, tag);
				assert.ok(packed.path.includes(`npm-${channel}-`));
				assert.ok(packed.filename.endsWith(`-${version}.tgz`));
				assert.equal(execFileSync("tar", ["-xOzf", packed.path, "package/bin/dbgjs"], { encoding: "utf8" }), "binary fixture\n");
				if (manifest.name === "@hediet/dbgjs") {
					assert.deepEqual(Object.values(manifest.optionalDependencies), Object.keys(platforms).map(() => version));
				} else {
					const platform = platforms[manifest.name.slice("@hediet/dbgjs-".length)];
					assert.deepEqual(manifest.os, [platform.os]);
					assert.deepEqual(manifest.cpu, [platform.cpu]);
				}
			}
		}
		for (const [version, tag] of [
			[entry.version, "next"],
			[`${entry.version}-next.123`, "next"],
			[`${entry.version}-next.20260915.0`, "next"],
			[`${entry.version}-next.20260915.01`, "next"],
			[`${entry.version}-next.20260915.1`, "nightly"],
			[`${entry.version}-next.20260915.1`, "latest"],
		]) {
			await assert.rejects(prepareReleasePackages({ input, output: directory, version, tag }), /must agree/);
		}
		for (const name of ["@hediet/dbgjs", "@hediet/dbgjs-win32-x64"]) {
			const key = name === "@hediet/dbgjs" ? "dbgjs" : "win32-x64";
			const archive = join(input, `${key}.tar.gz`);
			const original = await readFile(archive);
			const manifest = JSON.parse(execFileSync("tar", ["-xOzf", archive, "package/package.json"], { encoding: "utf8" }));
			for (const [changes, error] of [
				[{ gitHead: "b".repeat(40) }, /Candidate commits must match/],
				[{ gitHead: null }, /gitHead/],
				[{ gitHead: undefined }, /gitHead/],
				[{ gitHead: "1234567" }, /gitHead/],
				[{ gitHead: "z".repeat(40) }, /gitHead/],
				[{ gitDirty: true }, /gitDirty false/],
				[{ gitDirty: null }, /gitDirty false/],
				[{ gitDirty: undefined }, /gitDirty false/],
				[{ gitDirty: "false" }, /gitDirty false/],
			]) {
				await writeFile(join(packageDirectory, "package.json"), JSON.stringify({ ...manifest, ...changes }));
				execFileSync("tar", ["-czf", archive, "-C", fixture, "package"]);
				await assert.rejects(inspectCandidatePackages(input), error);
				await assert.rejects(prepareReleasePackages({
					input, output: join(directory, "rejected"), version: entry.version, tag: "latest",
				}), error);
				await writeFile(archive, original);
			}
		}
		const [filename] = await readdir(input);
		await rename(join(input, filename), join(directory, filename));
		await assert.rejects(inspectCandidatePackages(input), /Missing candidates/);
	} finally {
		await rm(directory, { recursive: true, force: true });
	}
});
