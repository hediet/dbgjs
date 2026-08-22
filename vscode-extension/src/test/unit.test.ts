import assert from "node:assert/strict";
import { chmod, mkdtemp, rm, writeFile } from "node:fs/promises";
import test from "node:test";
import { join } from "node:path";
import type { ConnectionSnapshot } from "../apiTypes.js";
import { parseObservationResult } from "../apiTypes.js";
import { findInstalledChrome, parseLaunch } from "../launchConfig.js";
import { buildTargetForest, computeWorkspaceContextId } from "../model.js";

test("workspace context identity is stable across folder ordering", () => {
	const first = computeWorkspaceContextId(["file:///b", "file:///a"]);
	const second = computeWorkspaceContextId(["file:///a", "file:///b"]);
	assert.equal(first, second);
	assert.match(first, /^vscode-[0-9a-f]{16}$/);
});

test("target forest prefers parent and falls back to opener", () => {
	const connection: ConnectionSnapshot = {
		id: "browser",
		configuration: {
			kind: "directCdp",
			endpoint: "ws://127.0.0.1/devtools/browser/test",
		},
		generation: 1,
		status: { kind: "connected" },
		targets: [
			target("page", "page"),
			target("worker", "worker", { openerId: "page" }),
			target("frame", "iframe", { parentId: "page" }),
			target("orphan", "worker", { parentId: "missing" }),
		],
	};

	const forest = buildTargetForest(connection);
	assert.deepEqual(
		forest.map((node) => ({
			id: node.target.targetId,
			children: node.children.map((child) => child.target.targetId),
		})),
		[
			{ id: "page", children: ["worker", "frame"] },
			{ id: "orphan", children: [] },
		],
	);
});

test("empty context observations represent idle long-poll timeouts", () => {
	assert.deepEqual(parseObservationResult({ kind: "items", items: [] }), {});
});

test("Node.js launch configurations preserve runtime and program arguments", () => {
	assert.deepEqual(parseLaunch({
		runtime: "node",
		program: "/workspace/server.js",
		args: ["--port", "3000"],
		runtimeArgs: ["--enable-source-maps"],
		env: { NODE_ENV: "test", OMITTED: null },
	}), {
		runtime: "node",
		program: "/workspace/server.js",
		args: ["--port", "3000"],
		runtimeArgs: ["--enable-source-maps"],
		env: { NODE_ENV: "test", OMITTED: null },
	});
});

test("installed Chrome discovery searches PATH", async () => {
	const directory = await mkdtemp(join(process.cwd(), ".jsdbg-chrome-test-"));
	const executable = join(directory, "google-chrome");
	try {
		await writeFile(executable, "#!/bin/sh\nexit 0\n");
		await chmod(executable, 0o755);
		assert.equal(
			await findInstalledChrome("linux", { PATH: directory }),
			executable,
		);
	} finally {
		await rm(directory, { recursive: true, force: true });
	}
});

function target(
	targetId: string,
	targetType: string,
	relations: { parentId?: string; openerId?: string } = {},
) {
	return {
		targetId,
		targetType,
		title: targetId,
		url: `https://example.test/${targetId}`,
		attached: true,
		...relations,
	};
}
