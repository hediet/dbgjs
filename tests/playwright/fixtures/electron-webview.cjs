const { writeFileSync } = require("node:fs");
const { join } = require("node:path");

const url = process.env.DBGJS_ELECTRON_FIXTURE_URL;
const profile = process.env.DBGJS_ELECTRON_FIXTURE_PROFILE;
if (!url || !profile) throw new Error("Electron fixture requires URL and profile environment");
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
	diagnostics.proxy = await window.webContents.session.resolveProxy(url)
		.catch((error) => `proxy resolution failed: ${error}`);
	reportStartup("loading", diagnostics);
	process.stdout.write(`${JSON.stringify({ kind: "ready", pid: process.pid })}\n`);
	return window.loadURL(url).then(async () => {
		reportStartup("loaded");
		window.webContents.debugger.attach("1.3");
		try {
			const { targetInfos } = await window.webContents.debugger.sendCommand("Target.getTargets");
			const { targetInfo } = await window.webContents.debugger.sendCommand("Target.getTargetInfo");
			process.stdout.write(`${JSON.stringify({ kind: "loaded", rootTargetId: targetInfo.targetId,
				targetInfos: targetInfos
				.filter((target) => target.type === "iframe")
				.map(({ targetId, type, url, parentId, openerId }) =>
					({ targetId, type, url, parentId, openerId })) })}\n`);
		} finally {
			window.webContents.debugger.detach();
		}
		let buffered = "";
		process.stdin.setEncoding("utf8");
		process.stdin.on("data", (chunk) => {
			buffered += chunk;
			for (;;) {
				const end = buffered.indexOf("\n");
				if (end < 0) break;
				const action = JSON.parse(buffered.slice(0, end));
				buffered = buffered.slice(end + 1);
				const source = action.kind === "remove"
					? 'document.querySelector("#primary").remove()'
					: `(() => { const frame = document.createElement("iframe");
						frame.id = "primary"; frame.src = ${JSON.stringify(action.url)};
						document.body.append(frame); })()`;
				window.webContents.executeJavaScript(source).then(() => {
					process.stdout.write(`${JSON.stringify({ kind: action.kind })}\n`);
				}).catch((error) => {
					process.stderr.write(`Electron fixture ${action.kind} failed: ${error}\n`);
				});
			}
		});
	});
}).catch((error) => {
	reportStartup("error", { ...diagnostics, error: String(error.stack ?? error) });
	process.stderr.write(`Electron fixture initialization failed: ${error.stack ?? error}\n`);
	app.quit();
});

process.stdin.resume();
process.stdin.once("end", () => app.quit());
