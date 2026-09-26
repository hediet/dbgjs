import assert from "node:assert/strict";
import { access } from "node:fs/promises";
import { dirname, resolve } from "node:path";
import test from "node:test";
import { checkGeneratedDocs, compareRecordings, formatCommand, normalize, renderDocuments, renderSteps, repositoryRoot } from "./transcript.mjs";

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
	assert.equal(normalize("$node-root:process-1234", [["1234", "NODE_PID"]]),
		"$node-root:process-$NODE_PID");
	assert.equal(normalize("process-1234", [["1234", "NODE_PID"]], { argument: true }),
		"process-$NODE_PID");
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
	assert.equal(formatCommand(["process", "attach", "1234"]), "dbgjs process attach 1234");
	assert.equal(formatCommand(["screenshot", "capture", "--output", "C:\\demo\\editor.png"]),
		"dbgjs screenshot capture --output 'C:\\demo\\editor.png'");
	assert.equal(formatCommand(["http://127.0.0.1:3000/orders"], "curl.exe"),
		"curl.exe http://127.0.0.1:3000/orders");
});

test("hosted runner elevation does not change the recorded VS Code title", () => {
	assert.equal(normalize('"Untitled-1 - readme-demo - Visual Studio Code [Administrator]"', []),
		'"Untitled-1 - readme-demo - Visual Studio Code"');
	assert.equal(normalize("application state [Administrator]", []), "application state [Administrator]");
});

test("Markdown preserves real identities and URLs while replay compares normalized values", () => {
	const captured = (pid, prefix) => ({
		vscodeVersion: "test",
		normalization: { demo: [[pid, "PID"], [prefix, "SOURCE"]] },
		steps: [{
			id: "source", scenario: "demo", executable: "dbgjs",
			args: ["process", "attach", pid], comparison: "exact", stderr: "",
			output: `Process ${pid}\n${prefix}textModel.ts:1505\n1234 hit lines\n`,
		}],
	});
	const before = captured("5678", "https://example.org/build-one/src/vs/");
	const after = captured("9123", "https://example.org/build-two/src/vs/");
	const copy = structuredClone(before);
	compareRecordings(after, before);
	assert.deepEqual(before, copy, "Comparing must not rewrite the recording.");
	const markdown = renderSteps(before.steps);
	assert.match(markdown, /dbgjs process attach 5678/);
	assert.match(markdown, /https:\/\/example\.org\/build-one\/src\/vs\/textModel\.ts:1505/);
	assert.match(markdown, /1234 hit lines/);
	assert.doesNotMatch(markdown, /\$PID|\$SOURCE/);
	after.steps[0].output = after.steps[0].output.replace("1234 hit lines", "1235 hit lines");
	assert.throws(() => compareRecordings(after, before));
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

test("coverage progress timing can vary without hiding other stderr changes", () => {
	const before = recording("exact");
	const after = recording("exact");
	before.steps[0].args = after.steps[0].args = ["coverage", "capture", "--id", "background"];
	before.steps[0].stderr = "dbgjs: Still waiting after 20s. Coverage capture records raw ranges without fetching source maps; mapping and enrichment happen when viewing the stored capture with `dbgjs coverage show`. The current command is continuing.\n";
	compareRecordings(after, before);
	assert.match(renderSteps(before.steps), /Still waiting after 20s/);
	after.steps[0].stderr = "Unexpected source-map failure";
	assert.throws(() => compareRecordings(after, before));
});

test("heap object identities vary without weakening CDP method or JavaScript comparison", () => {
	const capture = (heapId, remoteId) => ({
		normalization: { demo: [[`"objectId":"${heapId}"`, "HEAP_OBJECT_ID_FIELD"], [remoteId, "HEAP_REMOTE_OBJECT"]] },
		steps: [
			{ id: "heap-object", scenario: "demo", args: ["target", "cdp", "HeapProfiler.getObjectByHeapObjectId", "--params",
				JSON.stringify({ objectId: heapId, objectGroup: "readme-heap" })], output: "", stderr: "", comparison: "live" },
			{ id: "heap-eval", scenario: "demo", args: ["target", "cdp", "Runtime.callFunctionOn", "--params",
				JSON.stringify({ objectId: remoteId, functionDeclaration: "function () { return this.getLineCount(); }", returnByValue: true })],
			output: '{"result":{"type":"number","value":1}}', stderr: "", comparison: "exact" },
		],
	});
	const before = capture("12345", "-123.1.10");
	const after = capture("67890", "-456.1.20");
	compareRecordings(after, before);
	assert.match(renderSteps(before.steps), /"objectId":"12345"/);
	assert.match(renderSteps(before.steps), /"objectId":"-123\.1\.10"/);
	const changed = structuredClone(after);
	changed.steps[1].args[4] = changed.steps[1].args[4].replace("getLineCount", "getLineContent");
	assert.throws(() => compareRecordings(changed, before));
	after.steps[0].args[4] = after.steps[0].args[4].replace("readme-heap", "different-group");
	assert.throws(() => compareRecordings(after, before));
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
