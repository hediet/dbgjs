const { writeFileSync } = require("node:fs");
const { createServer } = require("node:http");
const { join } = require("node:path");

const url = process.env.DBGJS_ELECTRON_FIXTURE_URL;
const profile = process.env.DBGJS_ELECTRON_FIXTURE_PROFILE;
const controlToken = process.env.DBGJS_ELECTRON_FIXTURE_CONTROL_TOKEN;
if (!url || !profile || !controlToken) {
	throw new Error("Electron fixture requires URL, profile, and control token environment");
}
const startupState = join(profile, "fixture-startup.json");
const reportStartup = (phase, detail) => writeFileSync(startupState, JSON.stringify({
	phase, detail, electron: process.versions.electron,
	stdout: Boolean(process.stdout), stderr: Boolean(process.stderr),
}));
reportStartup("module-entry");

const { app, BrowserWindow } = require("electron");
process.mainModule ??= module;
reportStartup("module");

app.setPath("userData", profile);
app.commandLine.appendSwitch("site-per-process");
app.disableHardwareAcceleration();

const diagnostics = {};
app.whenReady().then(async () => {
	reportStartup("ready");
	const window = new BrowserWindow({
		show: true,
		webPreferences: {
			backgroundThrottling: false,
			contextIsolation: true,
			nodeIntegration: false,
		},
	});
	window.webContents.on("render-process-gone", (_event, details) => {
		diagnostics.rendererGone = details;
		reportStartup("renderer-gone", diagnostics);
	});
	window.webContents.on("did-fail-load", (_event, code, description, validatedURL, isMainFrame) => {
		diagnostics.loadFailure = { code, description, validatedURL, isMainFrame };
		reportStartup("load-failed", diagnostics);
	});
	const control = createServer((request, response) => {
		if (request.method !== "POST" || request.url !== "/action" ||
			request.headers["x-dbgjs-fixture-token"] !== controlToken) {
			response.writeHead(404).end();
			return;
		}
		let body = "";
		request.on("data", (chunk) => {
			body += chunk;
			if (body.length > 4096) request.destroy();
		});
		request.on("end", async () => {
			try {
				const action = JSON.parse(body);
				if (action.kind === "shutdown") {
					response.end('{"kind":"shutdown"}');
					setImmediate(() => app.quit());
					return;
				}
				if (action.kind !== "remove" && action.kind !== "replace") {
					throw new Error(`Unknown fixture action: ${action.kind}`);
				}
				const source = action.kind === "remove"
					? 'document.querySelector("#primary").remove()'
					: `(() => { const frame = document.createElement("iframe");
						frame.id = "primary"; frame.src = ${JSON.stringify(action.url)};
						document.body.append(frame); })()`;
				await window.webContents.executeJavaScript(source);
				response.end(JSON.stringify({ kind: action.kind }));
			} catch (error) {
				response.writeHead(500).end(String(error));
			}
		});
	});
	await new Promise((resolve, reject) => {
		control.once("error", reject);
		control.listen(0, "127.0.0.1", resolve);
	});
	diagnostics.controlPort = control.address().port;
	diagnostics.proxy = await window.webContents.session.resolveProxy(url)
		.catch((error) => `proxy resolution failed: ${error}`);
	reportStartup("loading", diagnostics);
	process.stdout.write(`${JSON.stringify({ kind: "ready", pid: process.pid })}\n`);
	return window.loadURL(url).then(async () => {
		reportStartup("loaded", diagnostics);
		window.webContents.debugger.attach("1.3");
		try {
			const { targetInfos } = await window.webContents.debugger.sendCommand("Target.getTargets");
			const { targetInfo } = await window.webContents.debugger.sendCommand("Target.getTargetInfo");
			process.stdout.write(`${JSON.stringify({ kind: "loaded", controlPort: diagnostics.controlPort,
				rootTargetId: targetInfo.targetId,
				targetInfos: targetInfos
				.filter((target) => target.type === "iframe")
				.map(({ targetId, type, url, parentId, openerId }) =>
					({ targetId, type, url, parentId, openerId })) })}\n`);
		} finally {
			window.webContents.debugger.detach();
		}
	});
}).catch((error) => {
	reportStartup("error", { ...diagnostics, error: String(error.stack ?? error) });
	process.stderr.write(`Electron fixture initialization failed: ${error.stack ?? error}\n`);
	app.quit();
});

process.stdin.resume();
process.stdin.once("end", () => {
	diagnostics.stdinEnded = true;
	reportStartup("stdin-ended", diagnostics);
});
