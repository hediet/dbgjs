import assert from "node:assert/strict";
import { execFileSync } from "node:child_process";
import { mkdir, mkdtemp, rename, rm, writeFile } from "node:fs/promises";
import { tmpdir } from "node:os";
import { join } from "node:path";
import test from "node:test";
import { changedPaths, classifyChanges } from "./ci-changes.mjs";

test("documentation-only changes do not start Rust or packaging jobs", () => {
	assert.deepEqual(
		classifyChanges([
			"README.md",
			"plan.md",
			"docs/cli-design.md",
			"todo/dbgjs-heap-and-source-debugging.md",
			"external/README.md",
			".github/skills/orthogonal-primitives/SKILL.md",
		]),
		{ rust: false, packages: false },
	);
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
		"external/hubrpc-rust/crates/hubrpc/src/lib.rs",
		"tests/transcripts/bounded-evaluation.txt",
		".github/workflows/ci.yml",
		"scripts/ci-changes.mjs",
		"future-build-input",
	]) {
		assert.deepEqual(classifyChanges([path]), { rust: true, packages: true }, path);
	}
});

test("npm-only changes package binaries without running Rust unit tests", () => {
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
