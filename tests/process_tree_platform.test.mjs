import assert from "node:assert/strict";
import { execFile as execFileCallback, spawn } from "node:child_process";
import { randomUUID } from "node:crypto";
import { mkdtemp, open, readFile, readdir, readlink, rm, writeFile } from "node:fs/promises";
import { tmpdir } from "node:os";
import { basename, join, posix } from "node:path";
import { test } from "node:test";
import { promisify } from "node:util";

const source = (await readFile(new URL("../packages/dbgjs/src/providers/process_tree.mjs", import.meta.url), "utf8"))
	.replace(/^\s*import .+;$/gm, "")
	.replace('const rootPid = Number(required("DBGJS_PROCESS_ROOT_PID"));', "const rootPid = testRootPid;")
	.replace("main().catch(reportError);", "");

function helper(rootPid = process.pid, overrides = {}) {
	return new Function(
		"execFileCallback", "randomUUID", "open", "readFile", "readdir", "readlink", "rm",
		"basename", "join", "posix", "tmpdir", "promisify", "testRootPid", "overrides",
		`${source}
		if (overrides.fetchJson) fetchJson = overrides.fetchJson;
		if (overrides.ensureInspector) ensureInspector = overrides.ensureInspector;
		return { parseUnixProcesses, parseLsofListeners, parseLinuxTcpListeners,
			isNodeProcess, listProcesses, listListeners, ensureRootEndpoint, ensureInspector,
			processInstanceId, electronFusePath, electronInspectorFuseEnabled,
			assertElectronInspectorSignalSupported };`,
	)(execFileCallback, randomUUID, open, readFile, readdir, readlink, rm,
		basename, join, posix, tmpdir, promisify, rootPid, overrides);
}

test("Electron with browser-only CDP activates the main-process inspector, not the browser endpoint", async () => {
	const listeners = [{ pid: 42, port: 1234 }];
	const browser = "ws://127.0.0.1:1234/devtools/browser/test";
	const inspector = "ws://127.0.0.1:5678/node-test";
	for (const electron of [false, true]) {
		let activations = 0;
		const api = helper(42, {
			fetchJson: async (url) => url.endsWith("/json/version") ? {
				Browser: "Chrome/148.0",
				"User-Agent": `Chrome/148.0${electron ? " Electron/42.10.0" : ""}`,
				webSocketDebuggerUrl: browser,
			} : [],
			ensureInspector: async (pid, knownListeners) => {
				assert.equal(pid, 42);
				assert.equal(knownListeners, listeners);
				activations++;
				return inspector;
			},
		});
		assert.equal(await api.ensureRootEndpoint(42, listeners), electron ? inspector : browser);
		assert.equal(activations, electron ? 1 : 0);
	}
});

test("root selection reuses existing Node inspectors and reports Electron activation errors", async () => {
	const inspector = "ws://127.0.0.1:5678/node-test";
	const listeners = [{ pid: 42, port: 5678 }];
	const api = helper(42, {
		fetchJson: async () => [{ type: "node", webSocketDebuggerUrl: inspector }],
		ensureInspector: async () => assert.fail("existing inspector must be reused"),
	});
	assert.equal(await api.ensureRootEndpoint(42, listeners), inspector);

	const failing = helper(42, {
		fetchJson: async (url) => url.endsWith("/json/version") ? {
			"User-Agent": "Electron/42.10.0",
			webSocketDebuggerUrl: "ws://127.0.0.1:5678/devtools/browser/test",
		} : [],
		ensureInspector: async () => { throw new Error("inspector activation failed"); },
	});
	await assert.rejects(failing.ensureRootEndpoint(42, listeners), /inspector activation failed/);
});

test("Unix ps parsing preserves macOS executable names, arguments and stable start times", () => {
	const api = helper();
	const commands = [
		"  10  1 Mon Sep 14 12:34:56 2026 /Applications/Visual Studio Code.app/Contents/MacOS/Electron --user-data-dir=/Users/test/Code Profile",
		"  11 10 Mon Sep 14 12:34:57 2026 Code Helper (Plugin) --type=utility --utility-sub-type=node.mojom.NodeService --nolazy --inspect=127.0.0.1:45678",
		"  12 10 Mon Sep 14 12:34:58 2026 node server.js",
	].join("\n");
	const names = [
		"10 /Applications/Visual Studio Code.app/Contents/MacOS/Electron",
		"11 /Applications/Visual Studio Code.app/Contents/Frameworks/Code Helper (Plugin).app/Contents/MacOS/Code Helper (Plugin)",
		"12 node",
	].join("\n");
	const processes = api.parseUnixProcesses(commands, names);
	assert.equal(processes.length, 3);
	assert.equal(processes[0].name, "Electron");
	assert.equal(processes[0].commandLine.endsWith("--user-data-dir=/Users/test/Code Profile"), true);
	assert.equal(processes[1].name, "Code Helper (Plugin)");
	assert.equal(processes[1].executablePath,
		"/Applications/Visual Studio Code.app/Contents/Frameworks/Code Helper (Plugin).app/Contents/MacOS/Code Helper (Plugin)");
	assert.equal(processes[1].parentPid, 10);
	assert.equal(api.isNodeProcess(processes[1]), true);
	assert.equal(api.isNodeProcess(processes[2]), true);
	assert.equal(api.processInstanceId(processes[0]),
		String(Date.parse("Mon Sep 14 12:34:56 2026") / 1000).padStart(20, "0"));
	assert.throws(() => api.parseUnixProcesses("10 1 Mon Bad 14 12:34:56 2026 node", ""), /start time/);
});

test("macOS main and utility processes use the outer app's Electron framework fuses", () => {
	const api = helper();
	const app = "/Applications/Visual Studio Code.app/Contents";
	const expected = `${app}/Frameworks/Electron Framework.framework/Electron Framework`;
	assert.equal(api.electronFusePath(`${app}/MacOS/Code`, "darwin"), expected);
	assert.equal(api.electronFusePath(`${app}/Frameworks/Code Helper (Plugin).app/Contents/MacOS/Code Helper (Plugin)`, "darwin"), expected);
	assert.equal(api.electronFusePath("/opt/code/code", "linux"), "/opt/code/code");
	assert.throws(() => api.electronFusePath("Code", "darwin"), /executable path/);
});

test("Unix Electron activation requires enabled inspector fuses in every binary slice", async () => {
	const directory = await mkdtemp(join(tmpdir(), "dbgjs-electron-fuses-"));
	try {
		const binary = join(directory, "electron");
		const api = helper();
		const sentinel = Buffer.from("dL7pKGdnNz796PbbjQWNKmHXBZaB9tsX");
		const wire = (state) => Buffer.concat([sentinel, Buffer.from([1, 4, 48, 48, 48, state])]);
		for (const [contents, enabled] of [
			[wire(49), true],
			[wire(48), false],
			[wire(114), false],
			[Buffer.from("not Electron"), false],
			[wire(49).subarray(0, -1), false],
			[Buffer.concat([wire(49), wire(48)]), false],
			[Buffer.concat([wire(49), wire(49)]), true],
			[Buffer.concat([Buffer.alloc(1024 * 1024 - 10), wire(49)]), true],
			[Buffer.concat([sentinel, Buffer.from([2, 4, 48, 48, 48, 49])]), false],
		]) {
			await writeFile(binary, contents);
			assert.equal(await api.electronInspectorFuseEnabled(binary), enabled);
		}
		await writeFile(binary, wire(49));
		await api.assertElectronInspectorSignalSupported({ pid: 42, commandLine: "code" }, binary);
		await api.assertElectronInspectorSignalSupported({
			pid: 42, commandLine: "Code Helper --type=utility --utility-sub-type=node.mojom.NodeService",
		}, binary);
		await assert.rejects(api.assertElectronInspectorSignalSupported({
			pid: 42, commandLine: "Code Helper --type=renderer",
		}, binary), /non-Node/);
		await writeFile(binary, wire(48));
		await assert.rejects(api.assertElectronInspectorSignalSupported({ pid: 42, commandLine: "code" }, binary), /not enabled/);
	} finally {
		await rm(directory, { recursive: true, force: true });
	}
});

test("macOS lsof parsing keeps PID ownership and deduplicates dual-stack sockets", () => {
	assert.deepEqual(helper().parseLsofListeners(
		"p101\nn127.0.0.1:9229\nn[::1]:9229\np202\nn*:45678\n",
	), [{ pid: 101, port: 9229 }, { pid: 202, port: 45678 }]);
});

test("Linux proc TCP parsing selects listening sockets by inode", () => {
	const sockets = helper().parseLinuxTcpListeners([
		"sl local_address rem_address st tx_queue rx_queue tr tm->when retrnsmt uid timeout inode",
		"0: 0100007F:240D 00000000:0000 0A 00000000:00000000 00:00000000 00000000 1000 0 12345 1",
		"1: 0100007F:1234 00000000:0000 01 00000000:00000000 00:00000000 00000000 1000 0 54321 1",
		"2: 00000000000000000000000001000000:ABCD 00000000000000000000000000000000:0000 0A 00000000:00000000 00:00000000 00000000 1000 0 67890 1",
	].join("\n"));
	assert.deepEqual([...sockets], [["12345", 9229], ["67890", 43981]]);
});

test("discovers and connects to the inspector owned by a real process", { timeout: 30_000 }, async () => {
	const child = spawn(process.execPath, ["--inspect=0", "-e", "setInterval(() => {}, 1000)"], {
		stdio: ["ignore", "ignore", "pipe"],
	});
	try {
		await new Promise((resolve, reject) => {
			child.once("error", reject);
			child.stderr.on("data", (chunk) => {
				if (String(chunk).includes("Debugger listening on")) {
					resolve();
				}
			});
			child.once("exit", (code) => reject(new Error(`inspected child exited: ${code}`)));
		});
		const api = helper(child.pid);
		const processes = await api.listProcesses();
		const childProcess = processes.find((candidate) => candidate.pid === child.pid);
		assert.ok(childProcess);
		assert.ok(api.isNodeProcess(childProcess));
		const listeners = await api.listListeners(processes);
		assert.ok(listeners.some((listener) => listener.pid === child.pid));
		const endpoint = await api.ensureRootEndpoint(child.pid, listeners);
		assert.match(endpoint, /^ws:\/\/127\.0\.0\.1:\d+\//);
		const socket = new WebSocket(endpoint);
		const identity = await new Promise((resolve, reject) => {
			socket.onopen = () => socket.send(JSON.stringify({
				id: 1, method: "Runtime.evaluate",
				params: { expression: "process.pid", returnByValue: true },
			}));
			socket.onmessage = ({ data }) => {
				const message = JSON.parse(data);
				if (message.id === 1) {
					resolve(message.result.result.value);
				}
			};
			socket.onerror = reject;
		});
		socket.close();
		assert.equal(identity, child.pid);
	} finally {
		child.kill();
	}
});
