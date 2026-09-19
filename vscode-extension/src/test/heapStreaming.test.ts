import assert from "node:assert/strict";
import { execFile, spawn } from "node:child_process";
import { once } from "node:events";
import { mkdtemp, readFile, rm, stat } from "node:fs/promises";
import { tmpdir } from "node:os";
import { join, resolve } from "node:path";
import { createInterface } from "node:readline";
import test from "node:test";
import { promisify } from "node:util";
import { ErrorCode, RpcError } from "@hediet/linkrpc";
import { parseEndpointFile } from "../daemonClient.js";
import { connectDaemon, type DaemonConnection } from "../daemonTransport.js";
import { DebuggerService } from "../generated/debuggerService.js";

const execute = promisify(execFile);
type Progress = typeof DebuggerService.members.capture_heap_snapshot._serverStream;

test("generated TS streams real heap progress from the Rust daemon and CLI", { timeout: 180_000 }, async () => {
	const root = await mkdtemp(join(tmpdir(), "dbgjs-heap-streaming-"));
	const repository = resolve("..");
	const binarySuffix = process.platform === "win32" ? ".exe" : "";
	const cli = join(repository, "target/debug", `dbgjs${binarySuffix}`);
	const state = join(root, "service.json");
	const environment = {
		...process.env,
		DBGJS_SERVICE_STATE: state,
		DBGJS_SERVICE_EXE: join(repository, "target/debug", `dbgjs-service${binarySuffix}`),
	};
	const command = (args: string[]) => execute(cli, args, { env: environment, timeout: 60_000 });
	const node = spawn(process.execPath, ["-e", `
		globalThis.objects = Array.from({length: 30000}, (_, i) => ({i, value: 'heap-progress-' + i}));
		const inspector = require('node:inspector');
		inspector.open(0, '127.0.0.1', false);
		console.log(inspector.url());
		setInterval(() => {}, 1000);
	`], { stdio: ["ignore", "pipe", "inherit"] });
	const lines = createInterface({ input: node.stdout });
	const links: DaemonConnection[] = [];
	let started = false;
	try {
		const [inspector] = await once(lines, "line");
		assert.equal(typeof inspector, "string");
		await command(["context", "list"]);
		started = true;
		const endpoint = parseEndpointFile(JSON.parse(await readFile(state, "utf8")));
		const wire: string[] = [];
		const connect = async () => {
			const link = await connectDaemon(endpoint.address, endpoint.token, message => wire.push(message));
			links.push(link);
			return link;
		};
		const link = await connect();
		const client = link.connection.get(DebuggerService);
		const scope = {
			contextId: "heap-streaming",
			connectionId: "runtime",
			targetId: "$node-root:runtime",
		};
		await client.put_context({ contextId: scope.contextId, kind: "named", displayName: null });
		await client.put_connection({
			contextId: scope.contextId,
			connectionId: scope.connectionId,
			configuration: { kind: "nodeInspector", endpoint: inspector },
		});
		await client.connect_connection({ contextId: scope.contextId, connectionId: scope.connectionId });
		await client.attach_target({ ...scope, options: { force: false, expectedConnectionGeneration: null } });

		const parameters = {
			...scope, captureId: "typescript", captureNumericValue: false, exposeInternals: false,
		};
		const messages: Progress[] = [];
		const call = client.capture_heap_snapshot(parameters, { onMessage: message => messages.push(message) });
		const result = await call;
		assert.ok(messages.length > 1, "progress arrives before the final response");
		assert.equal(messages[0]?.bytesWritten, 0, "a new capture does not replay stale progress");
		assert.equal(messages.at(-1)?.finished, true);
		assert.equal(messages.at(-1)?.bytesWritten, result.bytesWritten);
		assert.ok(result.bytesWritten > 0);
		assert.ok(!wire.some(message => message.includes("::get_heap_snapshot_progress")));
		assert.deepEqual(await client.get_heap_snapshot_progress(scope), messages.at(-1));

		let firstProgress!: () => void;
		const observed = new Promise<void>(resolve => { firstProgress = resolve; });
		const cancelled = client.capture_heap_snapshot(
			{ ...parameters, captureId: "cancelled" },
			{ onMessage: () => firstProgress() },
		);
		const cancelledResult = cancelled.then(() => null, (error: unknown) => error);
		await observed;
		await cancelled.cancel("heap integration cancellation");
		const cancellationError = await cancelledResult;
		assert.ok(cancellationError instanceof RpcError);
		assert.equal(cancellationError.code, ErrorCode.cancelled);

		const disconnectedLink = await connect();
		let disconnectProgress!: () => void;
		const disconnectObserved = new Promise<void>(resolve => { disconnectProgress = resolve; });
		const disconnected = disconnectedLink.connection.get(DebuggerService).capture_heap_snapshot(
			{ ...parameters, captureId: "disconnected" },
			{ onMessage: () => disconnectProgress() },
		);
		const disconnectedResult = disconnected.then(() => null, (error: unknown) => error);
		await disconnectObserved;
		disconnectedLink.close();
		const disconnectError = await disconnectedResult;
		assert.ok(disconnectError instanceof RpcError);
		assert.equal(disconnectError.code, ErrorCode.peerDisconnected);

		// A new command must work after cancelled/orphaned CDP operations drain their chunks.
		for (const captureId of ["cancelled", "disconnected"]) {
			const recovered: Progress[] = [];
			const recovery = await client.capture_heap_snapshot(
				{ ...parameters, captureId },
				{ onMessage: message => recovered.push(message) },
			);
			assert.equal(recovered[0]?.bytesWritten, 0);
			assert.equal(recovered.at(-1)?.bytesWritten, recovery.bytesWritten);
			assert.equal(recovered.at(-1)?.finished, true);
		}

		const cancelledPath = join(root, "cancelled.heapsnapshot");
		let snapshotProgress!: () => void;
		const snapshotObserved = new Promise<void>(resolve => { snapshotProgress = resolve; });
		const cancelledSnapshot = client.take_heap_snapshot({
			...scope, path: cancelledPath, captureNumericValue: false, exposeInternals: false,
		}, { onMessage: () => snapshotProgress() });
		const cancelledSnapshotResult = cancelledSnapshot.then(() => null, (error: unknown) => error);
		await snapshotObserved;
		await cancelledSnapshot.cancel("snapshot cancellation");
		const snapshotError = await cancelledSnapshotResult;
		assert.ok(snapshotError instanceof RpcError);
		assert.equal(snapshotError.code, ErrorCode.cancelled);
		assert.ok(Array.isArray(JSON.parse(await readFile(cancelledPath, "utf8")).nodes));

		const selection = ["--context", ":heap-streaming", "--target", scope.targetId];
		const capture = await command(["heap", "capture", "--id", "cli", ...selection]);
		assert.match(capture.stderr, /Heap snapshot:/);
		const classes = await command(["heap", "classes", "--capture", "--filter", "^Object$", "--max-lines", "1", ...selection]);
		assert.match(classes.stderr, /Heap snapshot:/);
		const destination = join(root, "snapshot.heapsnapshot");
		const snapshot = await command(["heap", "snapshot", destination, ...selection]);
		assert.match(snapshot.stderr, /Heap snapshot:/);
		assert.ok((await stat(destination)).size > 0);
		assert.ok(Array.isArray(JSON.parse(await readFile(destination, "utf8")).nodes));
	} finally {
		for (const link of links) link.close();
		lines.close();
		node.kill();
		if (started) await command(["service", "stop"]);
		await rm(root, { recursive: true, force: true });
	}
});
