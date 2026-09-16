import assert from "node:assert/strict";
import { execFileSync } from "node:child_process";
import { mkdir, mkdtemp, readFile, readdir, rm, writeFile } from "node:fs/promises";
import { tmpdir } from "node:os";
import { join, resolve } from "node:path";
import test from "node:test";
import { packPackages } from "./npm-pack.mjs";

const entry = JSON.parse(await readFile(new URL("../npm/dbgjs/package.json", import.meta.url), "utf8"));
const gitCommit = "1234567890abcdef1234567890abcdef12345678";

test("both package manifests carry binary provenance, including dirty candidates", async () => {
	const directory = await mkdtemp(join(tmpdir(), "dbgjs-pack-test-"));
	try {
		const binDir = join(directory, "bin");
		await mkdir(binDir);
		for (const name of ["dbgjs", "dbgjs-service", "dbgjs-tui"]) {
			await writeFile(join(binDir, `${name}.exe`), `binary fixture: ${name}\n`);
		}
		for (const candidate of [false, true]) {
			const output = join(directory, String(candidate));
			let calls = 0;
			await packPackages({ platform: "win32-x64", "bin-dir": binDir, output, candidate }, (binary, args, options) => {
				assert.equal(binary, join(binDir, "dbgjs.exe"));
				assert.deepEqual(args, ["--json", "--version"]);
				assert.equal(options.encoding, "utf8");
				calls++;
				return JSON.stringify({ version: entry.version, gitCommit, gitDirty: candidate });
			});
			assert.equal(calls, 1);
			const archives = await readdir(output);
			assert.equal(archives.length, 2);
			const names = [];
			for (const archive of archives) {
				assert.ok(archive.endsWith(candidate ? ".tar.gz" : ".tgz"));
				const manifest = JSON.parse(execFileSync("tar", [
					"-xOzf", join(output, archive), "package/package.json",
				], { encoding: "utf8" }));
				names.push(manifest.name);
				assert.equal(manifest.gitHead, gitCommit);
				assert.equal(manifest.gitDirty, candidate);
				assert.equal(manifest.private, candidate ? true : undefined);
				assert.equal(manifest.version, entry.version);
			}
			assert.deepEqual(names.sort(), ["@hediet/dbgjs", "@hediet/dbgjs-win32-x64"]);
		}
	} finally {
		await rm(directory, { recursive: true, force: true });
	}
});

test("packaging rejects unknown or invalid binary provenance and mismatched versions", async () => {
	const valid = { version: entry.version, gitCommit, gitDirty: false };
	for (const changes of [
		{ gitCommit: null }, { gitCommit: undefined }, { gitCommit: "1234567" },
		{ gitCommit: "z".repeat(40) }, { gitCommit: 123 },
		{ gitDirty: null }, { gitDirty: undefined }, { gitDirty: "false" },
		{ version: "999.0.0" },
	]) {
		await assert.rejects(packPackages({
			platform: "linux-x64-gnu", "bin-dir": "unused", output: "unused",
		}, (binary, args) => {
			assert.equal(binary, resolve("unused", "dbgjs"));
			assert.deepEqual(args, ["--json", "--version"]);
			return JSON.stringify({ ...valid, ...changes });
		}), /Binary/);
	}
	await assert.rejects(packPackages({
		platform: "win32-x64", "bin-dir": "unused", output: "unused",
	}, () => "not JSON"), SyntaxError);
	await assert.rejects(packPackages({
		platform: "win32-x64", "bin-dir": "unused", output: "unused",
	}, () => { throw new Error("native executable failed"); }), /native executable failed/);
});
