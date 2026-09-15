import assert from "node:assert/strict";

export function discoveryResult(tree, extensionHost) {
	const root = tree.processes.find((process) => process.processId === tree.rootProcessId);
	assert.ok(root, "Discovered forest must contain its root process.");
	assert.equal(root.role, "vscode-main");
	assert.equal(extensionHost.role, "extension-host");
	assert.equal(extensionHost.attachable, true);
	return {
		root: { role: root.role },
		extensionHost: { role: extensionHost.role, attachable: extensionHost.attachable },
	};
}

export function breakpointResult(breakpoint) {
	assert.equal(breakpoint.applications.length, 1);
	const application = breakpoint.applications[0];
	assert.equal(application.status.kind, "installed");
	assert.ok(application.mapping?.projection.length > 0, "Breakpoint must use a source-map projection.");
	return {
		id: breakpoint.id,
		sourcePath: breakpoint.sourcePath,
		line: breakpoint.line,
		column: breakpoint.column,
		mapping: {
			sourceUrl: application.mapping.sourceUrl,
			requestedLine: application.mapping.requestedLine,
			requestedColumn: application.mapping.requestedColumn,
		},
		status: application.status.kind,
	};
}

export function pauseResult(target) {
	assert.ok(target.pause, "Target must be paused.");
	const frame = target.pause.frames[0];
	assert.ok(frame, "Pause must include a top stack frame.");
	assert.equal(frame.projected.kind, "resolved", "Top frame must resolve to authored TypeScript.");
	return {
		functionName: frame.functionName,
		location: frame.projected.location,
	};
}
