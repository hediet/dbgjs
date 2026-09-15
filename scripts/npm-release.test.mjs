import assert from "node:assert/strict";
import test from "node:test";
import { platforms } from "../npm/dbgjs/lib/platform.mjs";
import { assertTrustedRun, finalizeRelease, GithubRepository, prepareRelease, reserveNightlyVersion } from "./npm-release.mjs";

test("new versions produce nightly and stable; stable tags follow artifact upload", async () => {
	const fixture = createFixture();
	const state = await prepareRelease(fixture);
	assert.equal(state.nightly, true);
	assert.equal(state.stable, true);
	assert.deepEqual(fixture.packed.map(({ version, tag }) => ({ version, tag })), [
		{ version: "0.2.0-nightly.20260915.1", tag: "next" },
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
	assert.equal(fixture.packed[0].tag, "next");
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

test("failed packaging retains the nightly reservation but does not claim stable", async () => {
	const fixture = createFixture();
	const pack = fixture.pack;
	fixture.pack = async () => { throw new Error("pack failed"); };
	await assert.rejects(prepareRelease(fixture), /pack failed/);
	assert.deepEqual([...fixture.tags.keys()], ["nightly-builds/20260915/1"]);
	fixture.pack = pack;
	await prepareRelease(fixture);
	assert.equal(fixture.packed[0].version, "0.2.0-nightly.20260915.1");
});

test("stable promotion requires full macOS tests but nightly does not", async () => {
	const fixture = createFixture();
	fixture.github.requireStableTests = async () => { throw new Error("Full macOS suite missing"); };
	await assert.rejects(prepareRelease(fixture), /Full macOS suite missing/);
	assert.equal(fixture.tags.has("release-candidates/v0.2.0"), false);
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

test("daily indices start at 1, span base versions, and reset on the next UTC date", async () => {
	const { github, run } = createFixture();
	assert.equal(await reserveNightlyVersion(github, run, "0.2.0"), "0.2.0-nightly.20260915.1");
	assert.equal(await reserveNightlyVersion(github, { ...run, id: 124 }, "0.3.0"), "0.3.0-nightly.20260915.2");
	assert.equal(await reserveNightlyVersion(github, {
		...run, id: 125, created_at: "2026-09-16T00:00:00Z",
	}, "0.3.0"), "0.3.0-nightly.20260916.1");
	assert.equal(await reserveNightlyVersion(github, run, "0.2.0"), "0.2.0-nightly.20260915.1");
});

test("concurrent runs reserve distinct daily indices; concurrent retries reuse one reservation", async () => {
	const { github, run, tags } = createFixture();
	const versions = await Promise.all([
		reserveNightlyVersion(github, run, "0.2.0"),
		reserveNightlyVersion(github, { ...run, id: 124 }, "0.2.0"),
		reserveNightlyVersion(github, run, "0.2.0"),
	]);
	assert.equal(versions[0], versions[2]);
	assert.deepEqual([...new Set(versions)].sort(), ["0.2.0-nightly.20260915.1", "0.2.0-nightly.20260915.2"]);
	assert.equal(tags.size, 2);
});

test("daily allocation uses numeric maximum, not tag count or lexicographic order", async () => {
	const { github, run, tags } = createFixture();
	for (const index of [2, 10]) {
		tags.set(`nightly-builds/20260915/${index}`, {
			sha: run.head_sha,
			message: JSON.stringify({ sha: run.head_sha, runId: index, version: `0.2.0-nightly.20260915.${index}` }),
		});
	}
	assert.equal(await reserveNightlyVersion(github, run, "0.2.0"), "0.2.0-nightly.20260915.11");
});

test("nightly reservations reject changed identities and malformed dates", async () => {
	const { github, run } = createFixture();
	await reserveNightlyVersion(github, run, "0.2.0");
	await assert.rejects(reserveNightlyVersion(github, { ...run, head_sha: "b".repeat(40) }, "0.2.0"), /identity/);
	await assert.rejects(reserveNightlyVersion(github, run, "0.3.0"), /0.3.0-nightly/);
	for (const created_at of ["not a date", "2026-02-30T00:00:00Z"]) {
		await assert.rejects(reserveNightlyVersion(github, { ...run, created_at }, "0.2.0"));
	}
});

test("nightly reservation failures propagate and persistent contention is bounded", async () => {
	const { github, run } = createFixture();
	github.createTag = async () => { throw new Error("permission denied"); };
	await assert.rejects(reserveNightlyVersion(github, run, "0.2.0"), /permission denied/);
	let attempts = 0;
	github.createTag = async () => { attempts++; return false; };
	await assert.rejects(reserveNightlyVersion(github, run, "0.2.0"), /contended/);
	assert.equal(attempts, 20);
});

test("GitHub only treats an existing ref as retryable; it never updates refs", async (t) => {
	const github = new GithubRepository("hediet/dbgjs", "test-token");
	let status = 422;
	let message = "Reference already exists";
	t.mock.method(globalThis, "fetch", async (url, options) => {
		assert.equal(options.method, "POST");
		if (url.endsWith("/git/tags")) return Response.json({ sha: "a".repeat(40) });
		assert.ok(url.endsWith("/git/refs"));
		assert.equal(JSON.parse(options.body).ref, "refs/tags/nightly-builds/20260915/1");
		return Response.json({ message }, { status });
	});
	const args = ["nightly-builds/20260915/1", "b".repeat(40), "{}"];
	assert.equal(await github.createTag(...args, { allowExisting: true }), false);
	await assert.rejects(github.createTag(...args), /422.*Reference already exists/);
	message = "Validation Failed";
	await assert.rejects(github.createTag(...args, { allowExisting: true }), /422.*Validation Failed/);
	status = 403;
	message = "Forbidden";
	await assert.rejects(github.createTag(...args, { allowExisting: true }), /403.*Forbidden/);
});

function createFixture() {
	const run = {
		id: 123,
		created_at: "2026-09-15T23:59:59Z",
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
			getTagNames: async (prefix) => [...tags.keys()].filter((name) => name.startsWith(prefix)),
			createTag: async (name, sha, message, { allowExisting = false } = {}) => {
				if (allowExisting && tags.has(name)) return false;
				assert.equal(tags.has(name), false, "Tags must be created atomically, never updated.");
				tags.set(name, { sha, message });
				return true;
			},
		},
		download: async () => {},
		inspect: async () => ({ baseVersion: "0.2.0" }),
		pack: async (options) => { packed.push(options); },
	};
}
