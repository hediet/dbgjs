import assert from "node:assert/strict";
import test from "node:test";
import { platforms } from "../npm/dbgjs/lib/platform.mjs";
import { assertTrustedRun, finalizeRelease, prepareRelease } from "./npm-release.mjs";

test("new versions produce nightly and stable; stable tags follow artifact upload", async () => {
	const fixture = createFixture();
	const state = await prepareRelease(fixture);
	assert.equal(state.nightly, true);
	assert.equal(state.stable, true);
	assert.deepEqual(fixture.packed.map(({ version, tag }) => ({ version, tag })), [
		{ version: "0.2.0-nightly.123", tag: "nightly" },
		{ version: "0.2.0", tag: "latest" },
	]);
	assert.ok(fixture.tags.has("release-candidates/v0.2.0"));
	assert.equal(fixture.tags.has("v0.2.0"), false);
	await finalizeRelease(fixture.github, state);
	assert.equal(fixture.tags.get("v0.2.0").sha, fixture.run.head_sha);
	await finalizeRelease(fixture.github, state);
});

test("already released versions still produce nightly packages", async () => {
	const fixture = createFixture();
	fixture.tags.set("v0.2.0", { sha: "b".repeat(40) });
	const state = await prepareRelease(fixture);
	assert.equal(state.stable, false);
	assert.equal(state.nightly, true);
	assert.equal(fixture.packed.length, 1);
	assert.equal(fixture.packed[0].tag, "nightly");
});

test("a new manifest version is eligible even when the previous version was released", async () => {
	const fixture = createFixture();
	fixture.tags.set("v0.1.0", { sha: "b".repeat(40) });
	const state = await prepareRelease(fixture);
	assert.equal(state.stable, true);
	assert.equal(state.candidate.version, "0.2.0");
});

test("interrupted stable publication resumes its original candidate on a newer green run", async () => {
	const fixture = createFixture();
	const first = await prepareRelease(fixture);
	const newer = { ...fixture.run, id: 124, head_sha: "b".repeat(40) };
	fixture.github.getRun = async (id) => id === 124 ? newer : fixture.run;
	fixture.sourceRunId = 124;
	fixture.packed.length = 0;
	const resumed = await prepareRelease(fixture);
	assert.deepEqual(resumed.candidate, first.candidate);
	assert.match(fixture.packed[0].input, /source-124$/);
	assert.match(fixture.packed[1].input, /source-123$/);
	await finalizeRelease(fixture.github, resumed);
	assert.equal(fixture.tags.get("v0.2.0").sha, fixture.run.head_sha);
});

test("a retry keeps the same nightly version and never moves a stable tag", async () => {
	const fixture = createFixture();
	const first = await prepareRelease(fixture);
	const second = await prepareRelease(fixture);
	assert.deepEqual(first, second);
	fixture.tags.set("v0.2.0", { sha: "b".repeat(40) });
	await assert.rejects(finalizeRelease(fixture.github, first), /immutable/);
});

test("documentation-only builds skip releases and partial artifact sets fail", async () => {
	const fixture = createFixture();
	fixture.github.getArtifactNames = async () => [];
	assert.deepEqual(await prepareRelease(fixture), { nightly: false, stable: false });
	assert.equal(fixture.packed.length, 0);
	fixture.github.getArtifactNames = async () => ["npm-dbgjs"];
	await assert.rejects(prepareRelease(fixture), /complete native package set/);
});

test("failed packaging does not reserve a version", async () => {
	const fixture = createFixture();
	fixture.pack = async () => { throw new Error("pack failed"); };
	await assert.rejects(prepareRelease(fixture), /pack failed/);
	assert.equal(fixture.tags.size, 0);
});

test("stable promotion requires full macOS tests but nightly does not", async () => {
	const fixture = createFixture();
	fixture.github.requireStableTests = async () => { throw new Error("Full macOS suite missing"); };
	await assert.rejects(prepareRelease(fixture), /Full macOS suite missing/);
	assert.equal(fixture.tags.size, 0);
	fixture.tags.set("v0.2.0", { sha: "b".repeat(40) });
	const state = await prepareRelease(fixture);
	assert.equal(state.nightly, true);
	assert.equal(state.stable, false);
});

test("unavailable reserved artifacts fail instead of replacing stable contents", async () => {
	const fixture = createFixture();
	await prepareRelease(fixture);
	const newer = { ...fixture.run, id: 124, head_sha: "b".repeat(40) };
	fixture.github.getRun = async (id) => id === 124 ? newer : fixture.run;
	fixture.sourceRunId = 124;
	fixture.download = async (id) => {
		if (id === 123) throw new Error("Reserved artifacts expired");
	};
	await assert.rejects(prepareRelease(fixture), /expired/);
	assert.equal(fixture.tags.has("v0.2.0"), false);
	assert.equal(fixture.tags.get("release-candidates/v0.2.0").sha, fixture.run.head_sha);
});

test("only successful same-repository main CI is trusted", () => {
	const { run, github } = createFixture();
	assertTrustedRun(run, github.repository);
	for (const changes of [
		{ event: "pull_request" },
		{ head_branch: "feature" },
		{ conclusion: "failure" },
		{ status: "in_progress" },
		{ path: ".github/workflows/unrelated.yml" },
		{ head_repository: { full_name: "someone/fork" } },
		{ repository: { full_name: "someone/fork" } },
	]) {
		assert.throws(() => assertTrustedRun({ ...run, ...changes }, github.repository));
	}
});

test("failed bump followed by a green commit releases the green commit", async () => {
	const fixture = createFixture();
	fixture.run.conclusion = "failure";
	await assert.rejects(prepareRelease(fixture), /green/);
	assert.equal(fixture.tags.size, 0);
	fixture.run.conclusion = "success";
	fixture.run.head_sha = "c".repeat(40);
	const state = await prepareRelease(fixture);
	await finalizeRelease(fixture.github, state);
	assert.equal(fixture.tags.get("v0.2.0").sha, "c".repeat(40));
});

function createFixture() {
	const run = {
		id: 123,
		repository: { full_name: "hediet/dbgjs" },
		head_repository: { full_name: "hediet/dbgjs" },
		head_branch: "main",
		head_sha: "a".repeat(40),
		event: "push",
		path: ".github/workflows/ci.yml",
		status: "completed",
		conclusion: "success",
	};
	const tags = new Map();
	const packed = [];
	return {
		run, tags, packed,
		sourceRunId: run.id,
		directory: "artifacts",
		github: {
			repository: "hediet/dbgjs",
			getRun: async () => run,
			requireStableTests: async () => {},
			getArtifactNames: async () => ["npm-dbgjs", ...Object.keys(platforms).map((p) => `npm-${p}`)],
			getTag: async (name) => tags.get(name),
			createTag: async (name, sha, message) => {
				assert.equal(tags.has(name), false, "Tags must be created atomically, never updated.");
				tags.set(name, { sha, message });
			},
		},
		download: async () => {},
		inspect: async () => ({ baseVersion: "0.2.0" }),
		pack: async (options) => { packed.push(options); },
	};
}
