import assert from "node:assert/strict";
import { execFileSync } from "node:child_process";
import { appendFile, mkdir, readFile, writeFile } from "node:fs/promises";
import { join, resolve } from "node:path";
import { pathToFileURL } from "node:url";
import { platforms } from "../npm/dbgjs/lib/platform.mjs";
import { inspectCandidatePackages, prepareReleasePackages } from "./npm-release-pack.mjs";

export async function prepareRelease({ github, sourceRunId, download, inspect, pack, directory }) {
	const run = await github.getRun(sourceRunId);
	assertTrustedRun(run, github.repository);
	const artifacts = await github.getArtifactNames(run.id);
	const expected = ["npm-dbgjs", ...Object.keys(platforms).map((key) => `npm-${key}`)];
	const present = expected.filter((name) => artifacts.includes(name));
	if (present.length === 0) {
		console.log("No package artifacts: documentation-only CI produces no release.");
		return { nightly: false, stable: false };
	}
	assert.deepEqual(present, expected, "Successful CI must provide the complete native package set.");

	const input = join(directory, `source-${run.id}`);
	await download(run.id, expected, input);
	const { baseVersion } = await inspect(input);
	assert.match(baseVersion, /^(0|[1-9]\d*)\.(0|[1-9]\d*)\.(0|[1-9]\d*)$/);
	const tag = `v${baseVersion}`;
	const released = await github.getTag(tag);
	const claimName = `release-candidates/${tag}`;
	const claim = released ? undefined : await github.getTag(claimName);
	let candidate = { version: baseVersion, runId: run.id, sha: run.head_sha };
	if (claim) {
		candidate = JSON.parse(claim.message);
		assert.equal(candidate.version, baseVersion, "Candidate version must match its tag.");
		assert.equal(candidate.sha, claim.sha, "Candidate tag must identify its recorded commit.");
		assert.ok(Number.isSafeInteger(candidate.runId) && candidate.runId > 0, "Invalid candidate run ID.");
	}

	const output = join(directory, "release");
	const nightlyVersion = await reserveNightlyVersion(github, run, baseVersion);
	await pack({ input, output, version: nightlyVersion, tag: "next" });
	if (!released) {
		await github.requireStableTests(candidate.runId);
		let stableInput = input;
		if (candidate.runId !== run.id) {
			const candidateRun = await github.getRun(candidate.runId);
			assertTrustedRun(candidateRun, github.repository);
			assert.equal(candidateRun.head_sha, candidate.sha, "Reserved stable run changed identity.");
			stableInput = join(directory, `source-${candidate.runId}`);
			await download(candidate.runId, expected, stableInput);
			assert.equal((await inspect(stableInput)).baseVersion, baseVersion);
		} else {
			assert.equal(candidate.sha, run.head_sha);
		}
		await pack({ input: stableInput, output, version: baseVersion, tag: "latest" });
		if (!claim) {
			await github.createTag(claimName, candidate.sha, JSON.stringify(candidate));
		}
	}
	return { nightly: true, stable: !released, candidate };
}

export async function reserveNightlyVersion(github, run, baseVersion) {
	assert.match(run.created_at, /^\d{4}-\d{2}-\d{2}T\d{2}:\d{2}:\d{2}Z$/);
	const timestamp = new Date(run.created_at);
	assert.equal(timestamp.toISOString(), run.created_at.replace("Z", ".000Z"), "Invalid CI creation date.");
	const date = timestamp.toISOString().slice(0, 10).replaceAll("-", "");
	const prefix = `nightly-builds/${date}/`;
	for (let attempt = 0; attempt < 20; attempt++) {
		const names = await github.getTagNames(prefix);
		const claims = await Promise.all(names.map(async (name) => {
			const index = name.slice(prefix.length);
			assert.match(index, /^[1-9]\d*$/, "Nightly indices must start at 1.");
			assert.ok(Number.isSafeInteger(Number(index)));
			const tag = await github.getTag(name);
			assert.ok(tag?.message, `Missing nightly reservation: ${name}`);
			const claim = JSON.parse(tag.message);
			assert.equal(claim.sha, tag.sha, "Nightly reservation changed commit.");
			assert.ok(Number.isSafeInteger(claim.runId) && claim.runId > 0);
			assert.match(claim.version, new RegExp(`^\\d+\\.\\d+\\.\\d+-(?:nightly|next)\\.${date}\\.${index}$`));
			return { ...claim, index: Number(index) };
		}));
		const existing = claims.filter((claim) => claim.runId === run.id);
		assert.ok(existing.length <= 1, "CI run has multiple nightly reservations.");
		if (existing.length) {
			assert.equal(existing[0].sha, run.head_sha, "Reserved nightly run changed identity.");
			assert.equal(existing[0].version.split("-")[0], baseVersion, "Reserved nightly base version changed.");
			return existing[0].version;
		}
		const index = claims.reduce((max, claim) => Math.max(max, claim.index), 0) + 1;
		assert.ok(Number.isSafeInteger(index), "Nightly index overflow.");
		const version = `${baseVersion}-next.${date}.${index}`;
		const created = await github.createTag(`${prefix}${index}`, run.head_sha,
			JSON.stringify({ version, runId: run.id, sha: run.head_sha }), { allowExisting: true });
		if (created) return version;
	}
	throw new Error("Nightly reservation remained contended after 20 attempts; retry the release.");
}

export async function finalizeRelease(github, state) {
	assert.equal(state.stable, true);
	const { version, sha, runId } = state.candidate;
	const tag = `v${version}`;
	const claim = await github.getTag(`release-candidates/${tag}`);
	assert.ok(claim, "Stable release must have a durable candidate.");
	assert.equal(claim.sha, sha);
	assert.deepEqual(JSON.parse(claim.message), state.candidate);
	const released = await github.getTag(tag);
	if (released) {
		assert.equal(released.sha, sha, "Stable tags are immutable.");
		return;
	}
	await github.createTag(tag, sha, `Stable npm artifacts for ${version}; CI run ${runId}.`);
}

export function assertTrustedRun(run, repository) {
	assert.equal(run.repository.full_name, repository, "Release source must belong to this repository.");
	assert.equal(run.head_repository.full_name, repository, "Fork artifacts cannot be released.");
	assert.equal(run.head_branch, "main", "Only main builds can produce releases.");
	assert.ok(["push", "workflow_dispatch"].includes(run.event), "PR builds cannot produce releases.");
	assert.equal(run.path, ".github/workflows/ci.yml", "Only CI artifacts can produce releases.");
	assert.equal(run.status, "completed");
	assert.equal(run.conclusion, "success", "Only green CI runs can produce releases.");
	assert.match(run.head_sha, /^[a-f0-9]{40}$/);
	assert.ok(Number.isSafeInteger(run.id) && run.id > 0);
}

export class GithubRepository {
	constructor(repository, token) {
		assert.match(repository, /^[\w.-]+\/[\w.-]+$/);
		assert.ok(token, "GH_TOKEN is required.");
		this.repository = repository;
		this._token = token;
	}

	async getRun(id) {
		assert.ok(Number.isSafeInteger(id) && id > 0);
		return this._request(`actions/runs/${id}`);
	}

	async getArtifactNames(id) {
		const names = [];
		for (let page = 1; ; page++) {
			const result = await this._request(`actions/runs/${id}/artifacts?per_page=100&page=${page}`);
			for (const artifact of result.artifacts) {
				names.push(artifact.name);
			}
			if (result.artifacts.length < 100) return names;
		}
	}

	async requireStableTests(id) {
		for (let page = 1; ; page++) {
			const result = await this._request(`actions/runs/${id}/jobs?per_page=100&page=${page}`);
			const macos = result.jobs.find((job) => job.name === "Test (aarch64-apple-darwin)");
			if (macos) {
				assert.equal(macos.conclusion, "success", "Stable packages require the full macOS suite.");
				assert.ok(macos.steps.some((step) => step.name === "Test workspace" && step.conclusion === "success"),
					"Stable packages require successful macOS workspace tests, not just smoke tests.");
				return;
			}
			if (result.jobs.length < 100) {
				throw new Error("Stable packages require the full macOS suite on the selected CI run.");
			}
		}
	}

	async getTag(name) {
		const ref = await this._request(`git/ref/tags/${name}`, { allowMissing: true });
		if (!ref) return undefined;
		if (ref.object.type === "commit") return { sha: ref.object.sha };
		assert.equal(ref.object.type, "tag");
		const tag = await this._request(`git/tags/${ref.object.sha}`);
		assert.equal(tag.object.type, "commit", "Release tags must point directly to commits.");
		return { sha: tag.object.sha, message: tag.message };
	}

	async getTagNames(prefix) {
		const refs = await this._request(`git/matching-refs/tags/${prefix}`);
		return refs.map((ref) => {
			assert.ok(ref.ref.startsWith(`refs/tags/${prefix}`));
			return ref.ref.slice("refs/tags/".length);
		});
	}

	async createTag(name, sha, message, { allowExisting = false } = {}) {
		const tag = await this._request("git/tags", {
			body: { tag: name, object: sha, type: "commit", message },
		});
		const ref = await this._request("git/refs", {
			body: { ref: `refs/tags/${name}`, sha: tag.sha }, allowExisting,
		});
		return ref !== undefined;
	}

	async _request(path, { body, allowMissing = false, allowExisting = false } = {}) {
		const response = await fetch(`https://api.github.com/repos/${this.repository}/${path}`, {
			method: body ? "POST" : "GET",
			headers: {
				Authorization: `Bearer ${this._token}`,
				Accept: "application/vnd.github+json",
				"X-GitHub-Api-Version": "2022-11-28",
				...(body ? { "Content-Type": "application/json" } : {}),
			},
			body: body ? JSON.stringify(body) : undefined,
			signal: AbortSignal.timeout(30_000),
		});
		if (allowMissing && response.status === 404) return undefined;
		if (!response.ok) {
			const detail = await response.text();
			if (allowExisting && response.status === 422 &&
				JSON.parse(detail).message === "Reference already exists") return undefined;
			throw new Error(`GitHub ${path}: ${response.status} ${detail}`);
		}
		return response.json();
	}
}

async function main() {
	const github = new GithubRepository(process.env.GITHUB_REPOSITORY, process.env.GH_TOKEN);
	const directory = resolve("artifacts");
	const statePath = join(directory, "release-state.json");
	if (process.argv[2] === "prepare") {
		assert.ok(process.env.GITHUB_OUTPUT, "GITHUB_OUTPUT is required.");
		const state = await prepareRelease({
			github,
			sourceRunId: Number(process.env.SOURCE_RUN_ID),
			directory,
			download: async (runId, names, destination) => {
				await mkdir(destination, { recursive: true });
				execFileSync("gh", [
					"run", "download", String(runId), "--repo", github.repository,
					"--dir", destination, ...names.flatMap((name) => ["--name", name]),
				], { stdio: "inherit", timeout: 180_000 });
			},
			inspect: inspectCandidatePackages,
			pack: prepareReleasePackages,
		});
		await mkdir(directory, { recursive: true });
		await writeFile(statePath, JSON.stringify(state, null, 2) + "\n");
		await appendFile(process.env.GITHUB_OUTPUT, `nightly=${state.nightly}\nstable=${state.stable}\n`);
	} else if (process.argv[2] === "finalize") {
		await finalizeRelease(github, JSON.parse(await readFile(statePath, "utf8")));
	} else {
		throw new Error("Usage: node scripts/npm-release.mjs <prepare|finalize>");
	}
}

if (process.argv[1] && import.meta.url === pathToFileURL(resolve(process.argv[1])).href) {
	await main();
}
