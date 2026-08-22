import assert from "node:assert/strict";
import test from "node:test";
import type { ConnectionSnapshot } from "../apiTypes.js";
import { parseObservationResult } from "../apiTypes.js";
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
