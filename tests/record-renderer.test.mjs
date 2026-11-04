import assert from "node:assert/strict";
import { test } from "node:test";
import { recordRenderer, validateRecordingOptions, withTargetRelay } from "../scripts/record-renderer.mjs";

test("native recorder validates the actual duration and WebM output contract", async () => {
	const valid = { ffmpeg: "encoder", output: "video.webm", durationMs: 1000 };
	for (const durationMs of [40, 1000, 300_000]) {
		assert.doesNotThrow(() => validateRecordingOptions({ ...valid, durationMs }));
	}
	for (const durationMs of [0, 39, 300_001, NaN, Infinity, 1000.5, "1000"]) {
		assert.throws(() => validateRecordingOptions({ ...valid, durationMs }), /--duration-ms/);
	}
	assert.throws(() => validateRecordingOptions({ ...valid, ffmpeg: "" }), /--ffmpeg/);
	assert.throws(() => validateRecordingOptions({ ...valid, output: "video.mp4" }), /\.webm/);
	await assert.rejects(recordRenderer({ ...valid, output: undefined }), /--output/);
});

test("native relay reports executable launch errors instead of waiting for CDP", async () => {
	await assert.rejects(
		withTargetRelay({ dbgjs: "dbgjs-nonexistent-executable-for-test" }, async (relay) => {
			await relay.call("Runtime.enable");
		}),
		/ENOENT/,
	);
});
