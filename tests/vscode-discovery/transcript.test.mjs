import assert from "node:assert/strict";
import test from "node:test";
import { breakpointResult, discoveryResult, pauseResult } from "./transcript.mjs";

test("discovery transcript excludes process identity noise but validates actual roles", () => {
	for (const pid of [123, 456]) {
		const extensionHost = { processId: pid + 1, role: "extension-host", attachable: true };
		const tree = { rootProcessId: pid, processes: [{ processId: pid, role: "vscode-main" }, extensionHost] };
		assert.deepEqual(discoveryResult(tree, extensionHost), {
			root: { role: "vscode-main" }, extensionHost: { role: "extension-host", attachable: true },
		});
		assert.throws(() => discoveryResult(tree, { ...extensionHost, attachable: false }));
		assert.throws(() => discoveryResult(tree, { ...extensionHost, role: "node-utility" }));
	}
});

test("breakpoint transcript requires a real installed source-map application", () => {
	const mapping = { sourceUrl: "dbgjs-fixture:///fixture.ts", requestedLine: 5, requestedColumn: 2, projection: ["map-123"] };
	const application = { mapping, status: { kind: "installed", backendId: "volatile" } };
	const breakpoint = { id: "fixture", sourcePath: mapping.sourceUrl, line: 5, column: 2, applications: [application] };
	assert.deepEqual(breakpointResult(breakpoint), {
		id: "fixture", sourcePath: mapping.sourceUrl, line: 5, column: 2,
		mapping: { sourceUrl: mapping.sourceUrl, requestedLine: 5, requestedColumn: 2 },
		status: "installed",
	});
	assert.throws(() => breakpointResult({ ...breakpoint, applications: [] }));
	assert.throws(() => breakpointResult({ ...breakpoint, applications: [{ ...application, mapping: { ...mapping, projection: [] } }] }));
	assert.throws(() => breakpointResult({ ...breakpoint, applications: [{ ...application, status: { kind: "installing" } }] }));
});

test("paused transcript preserves exact authored location and rejects unmapped frames", () => {
	const location = { sourceUrl: "dbgjs-fixture:///fixture.ts", line: 5, column: 9 };
	const frame = { functionName: "compute", projected: { kind: "resolved", location } };
	assert.deepEqual(pauseResult({ pause: { epoch: 99, frames: [frame] } }), { functionName: "compute", location });
	assert.throws(() => pauseResult({ pause: null }));
	for (const kind of ["raw", "pending", "failed"]) {
		assert.throws(() => pauseResult({ pause: { frames: [{ ...frame, projected: { kind } }] } }));
	}
});
