import assert from "node:assert/strict";
import { spawn } from "node:child_process";
import { mkdtemp, mkdir, readFile, rm } from "node:fs/promises";
import { tmpdir } from "node:os";
import { join, resolve } from "node:path";
import { createInterface } from "node:readline";
import { once } from "node:events";
import { setTimeout as delay } from "node:timers/promises";
import { parseArgs } from "node:util";
import { platformKey } from "../npm/dbgjs/lib/platform.mjs";
import { npm } from "./npm-tools.mjs";

const { values } = parseArgs({ options: { platform: { type: "string" }, artifacts: { type: "string" }, candidate: { type: "boolean", default: false } } });
const host = platformKey(process.platform, process.arch, process.platform === "linux" ? process.report.getReport().header.glibcVersionRuntime : undefined);
assert.equal(values.platform, host, "Smoke tests must run natively on the packaged platform.");
if (!values.artifacts) throw new Error("--artifacts must point to the tarball directory.");
const manifest = JSON.parse(await readFile(new URL("../npm/dbgjs/package.json", import.meta.url), "utf8"));
const extension = values.candidate ? "tar.gz" : "tgz";
const tarballs = [
	resolve(values.artifacts, `hediet-dbgjs-${host}-${manifest.version}.${extension}`),
	resolve(values.artifacts, `hediet-dbgjs-${manifest.version}.${extension}`),
];

async function runProcess(command, args, options) {
	const child = spawn(command, args, { ...options, stdio: ["ignore", "pipe", "pipe"] });
	let stdout = "";
	let stderr = "";
	child.stdout.setEncoding("utf8");
	child.stderr.setEncoding("utf8");
	child.stdout.on("data", (data) => { stdout += data; });
	child.stderr.on("data", (data) => { stderr += data; });
	const timeout = setTimeout(() => child.kill(), 30_000);
	const [code, signal] = await once(child, "exit");
	clearTimeout(timeout);
	if (code !== 0) {
		throw new Error(`${command} exited with ${code ?? signal}:\n${stderr}${stdout}`);
	}
	return stdout;
}

const directory = await mkdtemp(join(tmpdir(), "dbgjs-installed-"));
try {
	for (const global of [false, true]) {
		const prefix = join(directory, global ? "global" : "local");
		await mkdir(prefix);
		npm(["install", ...(global ? ["--global"] : []), "--prefix", prefix, "--ignore-scripts", "--no-audit", "--no-fund", ...tarballs], { cwd: directory, stdio: "pipe", timeout: 180_000 });
		const modules = global && process.platform !== "win32" ? join(prefix, "lib", "node_modules") : join(prefix, "node_modules");
		const launcher = join(modules, "@hediet", "dbgjs", "bin", "dbgjs.mjs");
		const nativeBin = join(modules, "@hediet", `dbgjs-${host}`, "bin");
		const env = { ...process.env, DBGJS_SERVICE_STATE: join(prefix, "service.json") };
		delete env.DBGJS_SERVICE_EXE;
		delete env.DBGJS_PLAYWRIGHT_PACKAGE;
		delete env.DBGJS_NODE;
		const run = (...args) => runProcess(process.execPath, [launcher, ...args], { cwd: prefix, env });
		assert.match(await run("--help"), /dbgjs/);
		await assert.rejects(() => run("not-a-real-command"), /exited with/);
		const executable = (name) => join(nativeBin, `${name}${process.platform === "win32" ? ".exe" : ""}`);
		const provenance = JSON.parse(await run("--json", "--version"));
		assert.deepEqual(provenance, JSON.parse(await runProcess(executable("dbgjs"), ["--json", "--version"], { cwd: prefix, env })));
		assert.equal(provenance.version, manifest.version);
		assert.match(provenance.gitCommit, /^[0-9a-f]{40}$/);
		assert.equal(typeof provenance.gitDirty, "boolean");
		for (const name of ["dbgjs", `dbgjs-${host}`]) {
			const installed = JSON.parse(await readFile(join(modules, "@hediet", name, "package.json"), "utf8"));
			assert.equal(installed.gitHead, provenance.gitCommit);
			assert.equal(installed.gitDirty, provenance.gitDirty);
		}
		if (process.platform === "win32") {
			for (const name of ["dbgjs", "dbgjs-service", "dbgjs-tui"]) {
				const binary = await readFile(executable(name));
				const peOffset = binary.readUInt32LE(0x3c);
				assert.equal(binary.toString("ascii", 0, 2), "MZ");
				assert.equal(binary.readUInt32LE(peOffset), 0x00004550, `${name} must be a PE executable.`);
				assert.equal(binary.readUInt16LE(peOffset + 4), process.arch === "arm64" ? 0xaa64 : 0x8664,
					`${name} must match the native Windows architecture, not run under emulation.`);
			}
		}
		assert.match(await runProcess(executable("dbgjs"), ["--help"], { cwd: prefix, env }), /dbgjs/);
		assert.match(await runProcess(executable("dbgjs-tui"), ["--help"], { cwd: prefix, env }), /usage: dbgjs-tui/);
		for (const name of ["dbgjs", "dbgjs-tui"]) {
			const shim = global
				? join(prefix, ...(process.platform === "win32" ? [`${name}.cmd`] : ["bin", name]))
				: join(prefix, "node_modules", ".bin", process.platform === "win32" ? `${name}.cmd` : name);
			assert.match(await runProcess(shim, ["--help"], {
				cwd: prefix, env, shell: process.platform === "win32",
			}), /dbgjs/);
		}
		const node = spawn(process.execPath, ["-e", "const i=require('node:inspector');i.open(0,'127.0.0.1');console.log(i.url());setInterval(()=>{},1000)"], { stdio: ["ignore", "pipe", "pipe"] });
		const exited = once(node, "exit");
		const lines = createInterface({ input: node.stdout });
		try {
			const [endpoint] = await Promise.race([
				once(lines, "line"),
				exited.then(() => { throw new Error("Node inspector exited before startup."); }),
				delay(10_000, undefined, { ref: false }).then(() => { throw new Error("Node inspector startup timed out."); }),
			]);
			await run("--json", "context", "create", "--context", "npm-smoke");
			const connected = JSON.parse(await run("--json", "connection", "add", "--node-inspector", endpoint, "--connection", "runtime", "--context", "npm-smoke", "--connect"));
			assert.equal(connected.connections[0].targets[0].targetId, "$node-root:runtime");
			await run("--json", "target", "attach", "--context", "npm-smoke", "--target", "$node-root:runtime");
			const result = JSON.parse(await run("--json", "target", "eval", "6 * 7", "--context", "npm-smoke", "--target", "$node-root:runtime"));
			assert.equal(result.preview.preview, "42");
			console.log(`${global ? "Global" : "Local"} installed tarballs: native binaries, shims, service startup, and Node evaluation passed.`);
		} finally {
			lines.close();
			try {
				await run("service", "stop");
			} finally {
				node.kill();
				await exited;
			}
		}
	}
} finally {
	await rm(directory, { recursive: true, force: true, maxRetries: 20, retryDelay: 100 });
}
