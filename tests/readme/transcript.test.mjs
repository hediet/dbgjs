import assert from "node:assert/strict";
import { access } from "node:fs/promises";
import { dirname, resolve } from "node:path";
import test from "node:test";
import { checkGeneratedDocs, compareRecordings, formatCommand, normalize, renderDocuments, repositoryRoot } from "./transcript.mjs";

test("README is rendered from the checked-in real CLI recording", async () => {
	const recording = await checkGeneratedDocs();
	for (const [path, text] of await renderDocuments(recording)) {
		assert.doesNotMatch(text, /[ \t]+\n/, `${path} has trailing whitespace`);
		for (const [, link] of text.matchAll(/\[[^\]]+\]\(([^)\s]+)\)/g)) {
			if (/^(https?:|#)/.test(link)) continue;
			await access(resolve(repositoryRoot, dirname(path), link.split("#")[0]));
		}
	}
});

test("identity normalization leaves measured numbers and source lines intact", () => {
	assert.equal(normalize(
		"p:1234 PID 1234 process-tree-1234 w:1234/1\n1234 HL, 91234 RL\nline 1234\r\n",
		[["1234", "VSCODE_PID"]],
	), "p:$VSCODE_PID PID $VSCODE_PID process-tree-$VSCODE_PID w:$VSCODE_PID/1\n1234 HL, 91234 RL\nline 1234");
	assert.equal(normalize("1234", [["1234", "RENDERER_PID"]], { argument: true }), "$RENDERER_PID");
	assert.equal(normalize("1234", [["1234", "RENDERER_PID"]]), "1234");
	assert.equal(normalize("p:12345", [["1234", "VSCODE_PID"]]), "p:12345");
	assert.equal(normalize("Configuration: process 1234", [["1234", "NODE_PID"]]),
		"Configuration: process $NODE_PID");
	assert.throws(() => normalize("text", [["", "EMPTY"]]));
});

test("commands quote actual argument vectors for PowerShell", () => {
	assert.equal(
		formatCommand(["target", "eval", "document.querySelector('title').textContent"]),
		"dbgjs target eval 'document.querySelector(''title'').textContent'",
	);
	assert.equal(formatCommand(["target", "type", ""]), "dbgjs target type ''");
	assert.equal(formatCommand(["target", "attach", "--target", "$node-root:runtime"]),
		"dbgjs target attach --target '$node-root:runtime'");
	assert.equal(formatCommand(["process", "attach", "$NODE_PID"]), "dbgjs process attach $NODE_PID");
	assert.equal(formatCommand(["screenshot", "capture", "--output", "$ARTIFACTS\\editor.png"]),
		'dbgjs screenshot capture --output "$ARTIFACTS\\editor.png"');
});

function recording(comparison, output = "recorded output") {
	return {
		vscodeVersion: "test",
		steps: [{ id: "example", args: ["target", "show"], output, stderr: "", comparison }],
	};
}

test("replay rejects changed commands, policies, and stable output", () => {
	assert.throws(() => compareRecordings(recording("exact", "different"), recording("exact")));
	assert.throws(() => compareRecordings(recording("live"), recording("exact")));
	const changed = recording("live");
	changed.steps[0].args = ["target", "list"];
	assert.throws(() => compareRecordings(changed, recording("live")));
	const missing = recording("live");
	missing.steps = [];
	assert.throws(() => compareRecordings(missing, recording("live")));
});

test("live measurements can vary without weakening command comparison", () => {
	compareRecordings(recording("live", "timing from another machine"), recording("live"));
	compareRecordings(recording("exact"), recording("exact"));
});

test("excerpt checks retain source formatting while allowing unshown diagnostics to vary", () => {
	const before = recording("excerpt", "file.ts:3:1\n  3 | run();\nold diagnostic");
	const after = recording("excerpt", "file.ts:3:1\n  3 | run();\nnew diagnostic");
	before.steps[0].maxOutputLines = 2;
	after.steps[0].maxOutputLines = 2;
	compareRecordings(after, before);
	after.steps[0].output = "file.ts:3:1\n3: run();\nnew diagnostic";
	assert.throws(() => compareRecordings(after, before));
});
