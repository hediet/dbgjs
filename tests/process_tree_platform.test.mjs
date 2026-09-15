import assert from "node:assert/strict";
import { execFile as execFileCallback, spawn } from "node:child_process";
import { randomUUID } from "node:crypto";
import { readFile, readdir, readlink, rm } from "node:fs/promises";
import { tmpdir } from "node:os";
import { basename, join } from "node:path";
import { test } from "node:test";
import { promisify } from "node:util";

const source = (await readFile(new URL("../src/providers/process_tree.mjs", import.meta.url), "utf8"))
	.replace(/^\s*import .+;$/gm, "")
	.replace('const rootPid = Number(required("DBGJS_PROCESS_ROOT_PID"));', "const rootPid = testRootPid;")
	.replace("main().catch(reportError);", "");

function helper(rootPid = process.pid) {
	return new Function(
		"execFileCallback", "randomUUID", "readFile", "readdir", "readlink", "rm",
		"basename", "join", "tmpdir", "promisify", "testRootPid",
		`${source}\nreturn { parseUnixProcesses, parseLsofListeners, parseLinuxTcpListeners,
			isNodeProcess, listProcesses, listListeners, ensureRootEndpoint, ensureInspector,
			processInstanceId };`,
	)(execFileCallback, randomUUID, readFile, readdir, readlink, rm,
		basename, join, tmpdir, promisify, rootPid);
}

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
	assert.equal(processes[1].parentPid, 10);
	assert.equal(api.isNodeProcess(processes[1]), true);
	assert.equal(api.isNodeProcess(processes[2]), true);
	assert.equal(api.processInstanceId(processes[0]),
		String(Date.parse("Mon Sep 14 12:34:56 2026") / 1000).padStart(20, "0"));
	assert.throws(() => api.parseUnixProcesses("10 1 Mon Bad 14 12:34:56 2026 node", ""), /start time/);
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
