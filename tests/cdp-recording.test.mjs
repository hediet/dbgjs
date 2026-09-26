import assert from "node:assert/strict";
import { mkdtemp, readFile, rm, writeFile } from "node:fs/promises";
import { tmpdir } from "node:os";
import { join } from "node:path";
import test from "node:test";

import {
	CdpConnection,
	CdpRecording,
	StrictReplayPeer,
} from "./support/cdp-recording.mjs";

test("recordings persist, deduplicate, and derive redacted copies", async () => {
	const directory = await mkdtemp(join(tmpdir(), "cdp-recording-test-"));
	const redactedDirectory = await mkdtemp(
		join(tmpdir(), "cdp-recording-redacted-test-"),
	);
	try {
		const recording = new CdpRecording({ containsSensitiveData: true });
		const message = {
			id: 1,
			method: "Network.setExtraHTTPHeaders",
			params: { headers: { Authorization: "secret" } },
		};
		recording.append("outgoing", message);
		recording.append("outgoing", message);
		assert.equal(recording.blobCount, 1);

		await recording.save(directory);
		const loaded = await CdpRecording.load(directory);
		assert.equal(loaded.recordingId, recording.recordingId);
		assert.deepEqual(loaded.materialize(), [
			{ direction: "outgoing", message },
			{ direction: "outgoing", message },
		]);

		const redacted = loaded.derive((value) => {
			if (value.params?.headers?.Authorization) {
				value.params.headers.Authorization = "<redacted>";
			}
			return value;
		});
		await redacted.save(redactedDirectory);
		const loadedRedacted = await CdpRecording.load(redactedDirectory);
		assert.equal(
			loadedRedacted.materialize()[0].message.params.headers.Authorization,
			"<redacted>",
		);
		assert.equal(
			loaded.materialize()[0].message.params.headers.Authorization,
			"secret",
		);
		assert.equal(
			loadedRedacted.metadata.derivedFrom,
			loaded.recordingId,
		);
	} finally {
		await rm(directory, { recursive: true, force: true });
		await rm(redactedDirectory, { recursive: true, force: true });
	}
});

test("loading rejects tampered content-addressed blobs", async () => {
	const directory = await mkdtemp(join(tmpdir(), "cdp-recording-tamper-test-"));
	try {
		const recording = new CdpRecording();
		recording.append("incoming", { id: 1, result: { ok: true } });
		await recording.save(directory);
		const [frame] = recording.frames;
		const blobPath = join(directory, "blobs", `${frame.blob}.json`);
		assert.match(await readFile(blobPath, "utf8"), /"ok":true/);
		await writeFile(blobPath, '{"id":1,"result":{"ok":false}}');

		await assert.rejects(
			() => CdpRecording.load(directory),
			/failed digest check/,
		);
	} finally {
		await rm(directory, { recursive: true, force: true });
	}
});

test("strict offline replay remaps overlapping root and session ids", async () => {
	const recording = new CdpRecording();
	recording.append("outgoing", {
		id: 1,
		method: "Browser.getVersion",
		params: {},
	});
	recording.append("outgoing", {
		id: 1,
		sessionId: "session-a",
		method: "Runtime.enable",
		params: {},
	});
	recording.append("incoming", {
		sessionId: "session-a",
		method: "Runtime.executionContextCreated",
		params: { context: { id: 7 } },
	});
	recording.append("incoming", {
		id: 1,
		result: { product: "RecordedChrome/1" },
	});
	recording.append("incoming", {
		id: 1,
		sessionId: "session-a",
		result: {},
	});

	const replay = new StrictReplayPeer(recording.materialize());
	const connection = new CdpConnection(replay);
	const event = connection.waitForEvent(
		"session-a",
		"Runtime.executionContextCreated",
	);
	const [version, runtimeEnabled] = await Promise.all([
		connection.call(undefined, 1001, "Browser.getVersion", {}),
		connection.call("session-a", 1001, "Runtime.enable", {}),
	]);

	assert.equal(version.product, "RecordedChrome/1");
	assert.deepEqual(runtimeEnabled, {});
	assert.equal((await event).params.context.id, 7);
	replay.assertComplete();
});

test("strict offline replay preserves protocol errors", async () => {
	const recording = new CdpRecording();
	recording.append("outgoing", {
		id: 4,
		sessionId: "session-a",
		method: "Runtime.evaluate",
		params: { expression: "invalid" },
	});
	recording.append("incoming", {
		id: 4,
		sessionId: "session-a",
		error: { code: -32000, message: "Evaluation failed" },
	});

	const replay = new StrictReplayPeer(recording.materialize());
	const connection = new CdpConnection(replay);
	await assert.rejects(
		connection.call("session-a", 4004, "Runtime.evaluate", {
			expression: "invalid",
		}),
		/-32000: Evaluation failed/,
	);
	replay.assertComplete();
});

test("strict replay rejects command divergence", () => {
	const recording = new CdpRecording();
	recording.append("outgoing", {
		id: 1,
		method: "Debugger.enable",
		params: {},
	});

	const replay = new StrictReplayPeer(recording.materialize());
	const connection = new CdpConnection(replay);
	assert.throws(
		() => connection.call(undefined, 10, "Runtime.enable", {}),
		/Replay command mismatch/,
	);
});
