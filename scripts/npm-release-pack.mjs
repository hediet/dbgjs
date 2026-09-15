import assert from "node:assert/strict";
import { execFileSync } from "node:child_process";
import { mkdir, mkdtemp, readFile, readdir, rm, writeFile } from "node:fs/promises";
import { tmpdir } from "node:os";
import { join, resolve } from "node:path";
import { pathToFileURL } from "node:url";
import { parseArgs } from "node:util";
import { platforms } from "../npm/dbgjs/lib/platform.mjs";
import { npm } from "./npm-tools.mjs";

export async function prepareReleasePackages({ input, output, version, tag }) {
	assert.ok(
		(tag === "latest" && /^\d+\.\d+\.\d+$/.test(version)) ||
		(tag === "next" && /^\d+\.\d+\.\d+-(?:next|nightly)\.[1-9]\d{7}\.[1-9]\d*$/.test(version)),
		"Release version and publication tag must agree.",
	);
	const candidates = await inspectCandidatePackages(input);
	assert.equal(version.split("-")[0], candidates.baseVersion);
	const staging = await mkdtemp(join(tmpdir(), "dbgjs-release-"));
	const packages = [];
	try {
		for (const candidate of candidates.packages) {
			const directory = join(staging, candidate.key);
			await mkdir(directory);
			execFileSync("tar", ["-xzf", candidate.path, "-C", directory], { stdio: "pipe" });
			const packageDirectory = join(directory, "package");
			const manifest = JSON.parse(await readFile(join(packageDirectory, "package.json"), "utf8"));
			delete manifest.private;
			manifest.version = version;
			manifest.publishConfig = { ...manifest.publishConfig, tag };
			if (manifest.name === "@hediet/dbgjs") {
				for (const platform of Object.keys(platforms)) {
					manifest.optionalDependencies[`@hediet/dbgjs-${platform}`] = version;
				}
			}
			await writeFile(join(packageDirectory, "package.json"), JSON.stringify(manifest, null, 2) + "\n");
			const destination = resolve(output, `npm-${tag === "latest" ? "stable" : "nightly"}-${candidate.key}`);
			await mkdir(destination, { recursive: true });
			const packed = JSON.parse(npm(["pack", "--ignore-scripts", "--json", "--pack-destination", destination], {
				cwd: packageDirectory,
			}));
			assert.equal(packed.length, 1);
			assert.ok(packed[0].filename.endsWith(".tgz"));
			packages.push({ name: manifest.name, filename: packed[0].filename, path: join(destination, packed[0].filename) });
		}
		return { baseVersion: candidates.baseVersion, version, packages };
	} finally {
		await rm(staging, { recursive: true, force: true });
	}
}

export async function inspectCandidatePackages(input) {
	const files = await findArchives(resolve(input));
	const expected = new Map([
		["@hediet/dbgjs", "dbgjs"],
		...Object.keys(platforms).map((key) => [`@hediet/dbgjs-${key}`, key]),
	]);
	const packages = [];
	let baseVersion;
	for (const path of files) {
		const entries = execFileSync("tar", ["-tzf", path], { encoding: "utf8" }).trim().split(/\r?\n/);
		assert.ok(entries.every((entry) =>
			entry.startsWith("package/") && !entry.includes("\\") && !entry.split("/").includes("..")),
		"Candidate archives must contain only relative package paths.");
		const manifest = JSON.parse(execFileSync("tar", ["-xOzf", path, "package/package.json"], { encoding: "utf8" }));
		assert.ok(expected.has(manifest.name), `Unexpected or duplicate candidate: ${manifest.name}`);
		assert.equal(manifest.private, true, "CI candidates must not be publishable.");
		assert.match(manifest.version, /^(0|[1-9]\d*)\.(0|[1-9]\d*)\.(0|[1-9]\d*)$/);
		baseVersion ??= manifest.version;
		assert.equal(manifest.version, baseVersion, "Candidate versions must match.");
		if (manifest.name === "@hediet/dbgjs") {
			for (const platform of Object.keys(platforms)) {
				assert.equal(manifest.optionalDependencies?.[`@hediet/dbgjs-${platform}`], baseVersion);
			}
		}
		packages.push({ path, key: expected.get(manifest.name), name: manifest.name });
		expected.delete(manifest.name);
	}
	assert.equal(expected.size, 0, `Missing candidates: ${[...expected.keys()].join(", ")}`);
	return { baseVersion, packages };
}

async function findArchives(directory) {
	const result = [];
	for (const entry of await readdir(directory, { withFileTypes: true })) {
		const path = join(directory, entry.name);
		if (entry.isDirectory()) result.push(...await findArchives(path));
		else if (entry.isFile() && entry.name.endsWith(".tar.gz")) result.push(path);
	}
	return result;
}

if (process.argv[1] && import.meta.url === pathToFileURL(resolve(process.argv[1])).href) {
	const { values } = parseArgs({ options: {
		input: { type: "string" }, output: { type: "string" },
		version: { type: "string" }, tag: { type: "string" },
	} });
	assert.ok(values.input && values.output && values.version && values.tag,
		"Usage: node scripts/npm-release-pack.mjs --input <candidates> --output <packages> --version <version> --tag <next|latest>");
	console.log(JSON.stringify(await prepareReleasePackages(values)));
}
