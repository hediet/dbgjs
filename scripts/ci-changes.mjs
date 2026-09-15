import { execFileSync } from "node:child_process";
import { appendFileSync } from "node:fs";
import { resolve } from "node:path";
import { pathToFileURL } from "node:url";

export function classifyChanges(paths) {
	let rust = false;
	let packages = false;
	for (const path of paths) {
		if (
			path.startsWith("docs/") ||
			path.startsWith("todo/") ||
			path.startsWith(".github/skills/") ||
			path.startsWith("vscode-extension/") ||
			path === "external/README.md" ||
			(!path.includes("/") && path.endsWith(".md"))
		) {
			continue;
		}
		packages = true;
		if (!path.startsWith("npm/") && !path.startsWith("scripts/npm-")) {
			rust = true;
		}
	}
	return { rust, packages };
}

export function changedPaths(base, head = "HEAD", cwd) {
	const args =
		!base || /^0+$/.test(base)
			? ["ls-files", "-z"]
			: ["diff", "--no-renames", "--name-only", "-z", base, head, "--"];
	return execFileSync("git", args, { encoding: "utf8", cwd })
		.split("\0")
		.filter(Boolean);
}

export function selectTestPlatforms(changes) {
	const platforms = [];
	if (changes.rust) {
		platforms.push(
			{ os: "windows-2022", target: "x86_64-pc-windows-msvc" },
			{ os: "ubuntu-22.04", target: "x86_64-unknown-linux-gnu" },
		);
	}
	if (changes.packages) {
		platforms.push({ os: "macos-15", target: "aarch64-apple-darwin" });
	}
	return platforms;
}

if (
	process.argv[1] &&
	import.meta.url === pathToFileURL(resolve(process.argv[1])).href
) {
	const paths = changedPaths(process.env.BASE_SHA, process.env.HEAD_SHA || "HEAD");
	const changes = classifyChanges(paths);
	const testPlatforms = selectTestPlatforms(changes);
	console.log(JSON.stringify({ paths, ...changes, testPlatforms }, null, 2));
	if (!process.env.GITHUB_OUTPUT) {
		throw new Error("GITHUB_OUTPUT must be set when running change detection.");
	}
	appendFileSync(
		process.env.GITHUB_OUTPUT,
		`rust=${changes.rust}\npackages=${changes.packages}\ntests=${testPlatforms.length > 0}\ntest_matrix=${JSON.stringify({ include: testPlatforms })}\n`,
	);
}
