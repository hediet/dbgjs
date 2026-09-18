import assert from "node:assert/strict";
import { execFileSync } from "node:child_process";
import { mkdir, mkdtemp, rename, rm, writeFile } from "node:fs/promises";
import { tmpdir } from "node:os";
import { join } from "node:path";
import test from "node:test";
import { changedPaths, classifyChanges, selectTestPlatforms } from "./ci-changes.mjs";

test("documentation-only changes do not start Rust or packaging jobs", () => {
	assert.deepEqual(
		classifyChanges([
			"plan.md",
			"docs/cli-design.md",
			"todo/dbgjs-heap-and-source-debugging.md",
			"external/README.md",
			".github/skills/orthogonal-primitives/SKILL.md",
		]),
		{ rust: false, packages: false },
	);
});

test("root README changes run no Rust compilation or tests", () => {
	assert.deepEqual(classifyChanges(["README.md"]), {
		rust: false,
		packages: false,
	});
});

test("all native build inputs invalidate both jobs", () => {
	for (const path of [
		"Cargo.toml",
		"Cargo.lock",
		"rust-toolchain.toml",
		".cargo/config.toml",
		"package.json",
		"package-lock.json",
		"src/bin/dbgjs.rs",
		"src/providers/node.mjs",
		"src/embedded-help.md",
		"crates/cdp-protocol/build.rs",
		"external/linkrpc/rust/crates/linkrpc/src/lib.rs",
		"tests/transcripts/bounded-evaluation.txt",
		".github/workflows/ci.yml",
		"scripts/ci-changes.mjs",
		"future-build-input",
	]) {
		assert.deepEqual(classifyChanges([path]), { rust: true, packages: true }, path);
	}
});

test("npm-only changes skip Linux and Windows x64 Rust jobs", () => {
	for (const path of ["npm/dbgjs/package.json", "npm/dbgjs/README.md", "scripts/npm-pack.mjs"]) {
		assert.deepEqual(classifyChanges([path]), { rust: false, packages: true }, path);
	}
});

test("mixed changes retain required jobs regardless of order", () => {
	for (const paths of [
		["src/lib.rs", "docs/cli-design.md"],
		["npm/dbgjs/README.md", "src/lib.rs"],
		["docs/cli-design.md", "npm/dbgjs/package.json"],
	]) {
		const expected = paths.includes("src/lib.rs")
			? { rust: true, packages: true }
			: { rust: false, packages: true };
		assert.deepEqual(classifyChanges(paths), expected);
		assert.deepEqual(classifyChanges(paths.toReversed()), expected);
	}
});

test("empty and extension-only changes skip the native pipeline", () => {
	assert.deepEqual(classifyChanges([]), { rust: false, packages: false });
	assert.deepEqual(classifyChanges(["vscode-extension/src/extension.ts"]), {
		rust: false,
		packages: false,
	});
});

test("Rust changes run full tests on Linux, Windows x64/ARM64 and ARM64 macOS", () => {
	const expected = [
		{ os: "windows-2022", target: "x86_64-pc-windows-msvc" },
		{ os: "ubuntu-22.04", target: "x86_64-unknown-linux-gnu" },
		{ os: "windows-11-arm", target: "aarch64-pc-windows-msvc" },
		{ os: "macos-15", target: "aarch64-apple-darwin" },
	];
	assert.deepEqual(selectTestPlatforms({ rust: true, packages: true }), expected);
});

test("npm-only changes always require the full Windows and macOS ARM64 suites", () => {
	for (const path of ["npm/dbgjs/package.json", "scripts/npm-pack.mjs"]) {
		assert.deepEqual(selectTestPlatforms(classifyChanges([path])), [
			{ os: "windows-11-arm", target: "aarch64-pc-windows-msvc" },
			{ os: "macos-15", target: "aarch64-apple-darwin" },
		]);
	}
});

test("documentation-only and extension-only changes still skip all native tests", () => {
	for (const paths of [[], ["README.md"], ["docs/ci.md"], ["vscode-extension/src/extension.ts"]]) {
		assert.deepEqual(selectTestPlatforms(classifyChanges(paths)), []);
	}
});

test("Git diffs handle docs-only commits, initial runs, and renamed build inputs", async () => {
	const directory = await mkdtemp(join(tmpdir(), "dbgjs-ci-changes-"));
	const git = (...args) =>
		execFileSync("git", args, { cwd: directory, encoding: "utf8" }).trim();
	const commit = () => {
		git("add", ".");
		git("-c", "user.name=CI test", "-c", "user.email=ci@example.invalid", "-c", "commit.gpgsign=false", "commit", "-qm", "fixture");
		return git("rev-parse", "HEAD");
	};
	try {
		git("init", "--quiet");
		await mkdir(join(directory, "src"));
		await mkdir(join(directory, "docs"));
		await writeFile(join(directory, "src", "lib.rs"), "pub fn fixture() {}\n");
		const base = commit();
		assert.deepEqual(classifyChanges(changedPaths(undefined, "HEAD", directory)), {
			rust: true, packages: true,
		});
		await writeFile(join(directory, "docs", "usage.md"), "# Usage\n");
		const docs = commit();
		assert.deepEqual(changedPaths(base, docs, directory), ["docs/usage.md"]);
		assert.deepEqual(classifyChanges(changedPaths(base, docs, directory)), {
			rust: false, packages: false,
		});
		await rename(join(directory, "src", "lib.rs"), join(directory, "docs", "example.md"));
		const moved = commit();
		assert.deepEqual(changedPaths(docs, moved, directory), ["docs/example.md", "src/lib.rs"]);
		assert.deepEqual(classifyChanges(changedPaths(docs, moved, directory)), {
			rust: true, packages: true,
		});
	} finally {
		await rm(directory, { recursive: true, force: true });
	}
});
