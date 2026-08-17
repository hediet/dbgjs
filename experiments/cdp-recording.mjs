import { createHash } from "node:crypto";
import { mkdir, readFile, writeFile } from "node:fs/promises";
import { join } from "node:path";
import { isDeepStrictEqual } from "node:util";

const FORMAT = "cdp-recording";
const VERSION = 1;

export class CdpRecording {
	#blobs = new Map();

	constructor(metadata = {}) {
		this.metadata = structuredClone(metadata);
		this.frames = [];
	}

	append(direction, message, raw = JSON.stringify(message)) {
		if (direction !== "incoming" && direction !== "outgoing") {
			throw new Error(`Invalid recording direction: ${direction}`);
		}
		const parsed = JSON.parse(raw);
		if (!isDeepStrictEqual(parsed, message)) {
			throw new Error("Raw CDP frame does not match its parsed message");
		}

		const blob = sha256(raw);
		this.#blobs.set(blob, raw);
		this.frames.push({
			sequence: this.frames.length,
			direction,
			blob,
		});
	}

	get blobCount() {
		return this.#blobs.size;
	}

	get recordingId() {
		return sha256(this.#framesText());
	}

	materialize() {
		return this.frames.map((frame) => ({
			direction: frame.direction,
			message: JSON.parse(this.#requiredBlob(frame.blob)),
		}));
	}

	derive(redact, metadata = {}) {
		const derived = new CdpRecording({
			...metadata,
			derivedFrom: this.recordingId,
		});
		for (const frame of this.frames) {
			const message = JSON.parse(this.#requiredBlob(frame.blob));
			const redacted = redact(structuredClone(message), {
				sequence: frame.sequence,
				direction: frame.direction,
			});
			derived.append(frame.direction, redacted);
		}
		return derived;
	}

	async save(directory) {
		const blobsDirectory = join(directory, "blobs");
		await mkdir(blobsDirectory, { recursive: true });

		for (const [digest, raw] of this.#blobs) {
			await writeExclusiveOrVerify(join(blobsDirectory, `${digest}.json`), raw);
		}

		const framesText = this.#framesText();
		await writeFile(join(directory, "frames.jsonl"), framesText, {
			encoding: "utf8",
			flag: "wx",
		});
		await writeFile(
			join(directory, "manifest.json"),
			JSON.stringify(
				{
					format: FORMAT,
					version: VERSION,
					recordingId: this.recordingId,
					frameCount: this.frames.length,
					blobCount: this.#blobs.size,
					metadata: this.metadata,
				},
				null,
				2,
			),
			{ encoding: "utf8", flag: "wx" },
		);
	}

	static async load(directory) {
		const manifest = JSON.parse(
			await readFile(join(directory, "manifest.json"), "utf8"),
		);
		if (manifest.format !== FORMAT || manifest.version !== VERSION) {
			throw new Error(
				`Unsupported CDP recording ${manifest.format}@${manifest.version}`,
			);
		}

		const framesText = await readFile(
			join(directory, "frames.jsonl"),
			"utf8",
		);
		if (sha256(framesText) !== manifest.recordingId) {
			throw new Error("CDP recording frame index failed its digest check");
		}

		const recording = new CdpRecording(manifest.metadata);
		recording.frames = framesText
			.split("\n")
			.filter(Boolean)
			.map((line) => JSON.parse(line));
		if (recording.frames.length !== manifest.frameCount) {
			throw new Error("CDP recording frame count does not match its manifest");
		}

		for (const [index, frame] of recording.frames.entries()) {
			if (frame.sequence !== index) {
				throw new Error(`Invalid CDP frame sequence at index ${index}`);
			}
			const raw = await readFile(
				join(directory, "blobs", `${frame.blob}.json`),
				"utf8",
			);
			if (sha256(raw) !== frame.blob) {
				throw new Error(`CDP recording blob failed digest check: ${frame.blob}`);
			}
			recording.#blobs.set(frame.blob, raw);
		}
		if (recording.#blobs.size !== manifest.blobCount) {
			throw new Error("CDP recording blob count does not match its manifest");
		}
		return recording;
	}

	#framesText() {
		return this.frames.map((frame) => JSON.stringify(frame)).join("\n") + "\n";
	}

	#requiredBlob(digest) {
		const raw = this.#blobs.get(digest);
		if (raw === undefined) {
			throw new Error(`Missing CDP recording blob: ${digest}`);
		}
		return raw;
	}
}

export class RecordingPeer {
	constructor(inner, recording) {
		this.inner = inner;
		this.recording = recording;
		inner.onMessage((message, raw) => {
			this.recording.append("incoming", message, raw);
			this.listener?.(message);
		});
	}

	onMessage(listener) {
		this.listener = listener;
	}

	send(message) {
		const raw = JSON.stringify(message);
		this.recording.append("outgoing", message, raw);
		this.inner.send(message);
	}

	close() {
		this.inner.close();
	}
}

export class StrictReplayPeer {
	#index = 0;
	#requestIds = new Map();
	#scheduled = false;

	constructor(transcript) {
		this.transcript = transcript;
	}

	onMessage(listener) {
		this.listener = listener;
	}

	send(actual) {
		const frame = this.transcript[this.#index++];
		if (frame?.direction !== "outgoing") {
			throw new Error(
				`Replay expected outgoing frame at index ${this.#index - 1}`,
			);
		}
		const expected = frame.message;
		if (
			expected.sessionId !== actual.sessionId ||
			expected.method !== actual.method ||
			!isDeepStrictEqual(expected.params, actual.params)
		) {
			throw new Error(
				`Replay command mismatch:\nexpected ${JSON.stringify(expected)}\nactual   ${JSON.stringify(actual)}`,
			);
		}
		this.#requestIds.set(
			requestKey(expected.sessionId, expected.id),
			actual.id,
		);
		this.#scheduleIncoming();
	}

	#scheduleIncoming() {
		if (this.#scheduled) {
			return;
		}
		this.#scheduled = true;
		queueMicrotask(() => {
			this.#scheduled = false;
			while (this.transcript[this.#index]?.direction === "incoming") {
				const recorded = structuredClone(
					this.transcript[this.#index++].message,
				);
				if (recorded.id !== undefined) {
					const key = requestKey(recorded.sessionId, recorded.id);
					const remapped = this.#requestIds.get(key);
					if (remapped === undefined) {
						throw new Error(`No replay request-id mapping for ${key}`);
					}
					recorded.id = remapped;
				}
				this.listener(recorded);
			}
		});
	}

	assertComplete() {
		if (this.#index !== this.transcript.length) {
			throw new Error(
				`Replay left ${this.transcript.length - this.#index} frame(s) unconsumed`,
			);
		}
	}

	close() {}
}

export class CdpConnection {
	#pending = new Map();
	#eventWaiters = [];

	constructor(peer) {
		this.peer = peer;
		peer.onMessage((message) => this.#accept(message));
	}

	#accept(message) {
		const key = requestKey(message.sessionId, message.id);
		const pending = this.#pending.get(key);
		if (pending) {
			this.#pending.delete(key);
			if (message.error) {
				pending.reject(
					new Error(`${message.error.code}: ${message.error.message}`),
				);
			} else {
				pending.resolve(message.result);
			}
			return;
		}

		if (message.method) {
			const index = this.#eventWaiters.findIndex(
				(waiter) =>
					waiter.sessionId === message.sessionId &&
					waiter.method === message.method,
			);
			if (index !== -1) {
				const [waiter] = this.#eventWaiters.splice(index, 1);
				waiter.resolve(message);
			}
			return;
		}

		throw new Error(`Unexpected CDP response for ${key}`);
	}

	call(sessionId, id, method, params) {
		const key = requestKey(sessionId, id);
		if (this.#pending.has(key)) {
			throw new Error(`Duplicate pending CDP request ${key}`);
		}

		const result = new Promise((resolve, reject) => {
			this.#pending.set(key, { resolve, reject });
		});
		this.peer.send({
			id,
			method,
			params,
			...(sessionId === undefined ? {} : { sessionId }),
		});
		return result;
	}

	waitForEvent(sessionId, method) {
		return new Promise((resolve) => {
			this.#eventWaiters.push({ sessionId, method, resolve });
		});
	}

	close() {
		this.peer.close();
	}
}

async function writeExclusiveOrVerify(path, content) {
	try {
		await writeFile(path, content, { encoding: "utf8", flag: "wx" });
	} catch (error) {
		if (error.code !== "EEXIST") {
			throw error;
		}
		const existing = await readFile(path, "utf8");
		if (existing !== content) {
			throw new Error(`Existing content-addressed blob differs: ${path}`);
		}
	}
}

function requestKey(sessionId, id) {
	return `${sessionId ?? "<root>"}:${id}`;
}

function sha256(content) {
	return createHash("sha256").update(content).digest("hex");
}
