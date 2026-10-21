---
name: record-code-oss
description: Record a Code OSS or VS Code Electron renderer as WebM video using dbgjs and Playwright screencast. Use for UI demos, recordings, screenshots, FFmpeg setup, attached-CDP recording, and diagnosing blank videos.
---

# Record Code OSS with dbgjs

Record the selected workbench renderer, not the whole desktop. Use the existing
`dbgjs playwright` primitive; no separate video CLI is needed for short demos.
This workflow uses Playwright **1.62.1**, as locked in this repository, and
requires dbgjs's page relay to allow `Page.startScreencast`,
`Page.screencastFrameAck`, and `Page.stopScreencast`.

## 1. Prepare the recording tools

Run from the dbgjs checkout in PowerShell. Activate the existing Node version
manager first if Node is not already available:

```powershell
fnm env --shell powershell | Out-String | Invoke-Expression
npm ci
npx playwright install ffmpeg
cargo build --locked --bin dbgjs --bin dbgjs-service
$dbgjs = (Resolve-Path .\target\debug\dbgjs.exe).Path
$env:DBGJS_SERVICE_EXE = (Resolve-Path .\target\debug\dbgjs-service.exe).Path
$env:DBGJS_PLAYWRIGHT_PACKAGE = (Resolve-Path .\node_modules\playwright\index.mjs).Path
$recordingRoot = Join-Path $PWD 'artifacts\code-oss-recording'
New-Item -ItemType Directory -Force $recordingRoot | Out-Null
$env:DBGJS_SERVICE_STATE = Join-Path $recordingRoot 'service.json'
```

Configure Node/PATH and Playwright **before the first command starts the
dedicated daemon**: it inherits that environment. A stale dedicated daemon can
be stopped with `& $dbgjs service stop` and restarted by the next command.
Do not stop a shared daemon. Use a fresh output directory for each run.
On other platforms use the matching binary names; existing-process discovery
and inspector activation in this workflow are Windows-specific.

Only the FFmpeg download is required for an existing Electron window.
`npx playwright install chromium` is needed only if also launching Playwright's
own Chromium (for example, to run the browser regression test).

### What FFmpeg does

Playwright receives JPEG screencast frames from Chromium over CDP, assigns
timestamps, and feeds them to its managed FFmpeg executable to encode a
silent VP8 WebM (25 fps encoding target, not a guarantee of 25 distinct
captured frames per second). On Windows this is normally under
`%LOCALAPPDATA%\ms-playwright\ffmpeg-<revision>\ffmpeg-win64.exe`;
`PLAYWRIGHT_BROWSERS_PATH` can change that location.

No system `ffmpeg`, `ffprobe`, PATH modification for FFmpeg, or manual frame
encoder is required. Playwright selects the matching cached executable.
Its minimal FFmpeg build is not a general-purpose transcoding installation;
use a separately installed full FFmpeg only for optional MP4/GIF conversion.

### How the FFmpeg install works

`npx playwright install ffmpeg` runs the checkout's npm-installed Playwright
CLI. It reads the pinned FFmpeg revision from `playwright-core/browsers.json`,
downloads a platform-specific prebuilt ZIP from Playwright's CDN (with mirror
retries), extracts it into the managed cache, checks the executable, and writes
an `INSTALLATION_COMPLETE` marker. Existing completed installations are reused.
It does not compile FFmpeg, install an npm-global executable, or use
winget/chocolatey. The normal per-user install needs neither administrator
access nor a PATH change.

Inspect the exact plan without downloading:

```powershell
npx playwright install --dry-run ffmpeg
```

For this lockfile on Windows, revision **1011** downloads from
`https://cdn.playwright.dev/dbazure/download/playwright/builds/ffmpeg/1011/ffmpeg-win64.zip`
into `%LOCALAPPDATA%\ms-playwright\ffmpeg-1011`. The plan also lists the Windows
Winldd dependency. `PLAYWRIGHT_BROWSERS_PATH` overrides the cache directory
(`0` selects a package-local cache); `PLAYWRIGHT_DOWNLOAD_HOST` overrides the
download host. Playback verification with Chromium requires its separate
browser installation. Do not assume the managed FFmpeg supports `ffprobe` or
general conversion commands: this build rejected the `null` output muxer.

## 2. Start an isolated Code OSS

For a **prepared** source checkout with its matching Electron runtime, compiled
output, built-in extensions, and native dependencies:

```powershell
$codeRoot = 'D:\dev\microsoft\vscode'
$profile = Join-Path $recordingRoot 'profile'
$extensions = Join-Path $recordingRoot 'extensions'
$workspace = Join-Path $recordingRoot 'workspace'
New-Item -ItemType Directory -Force $profile, $extensions, $workspace | Out-Null
# Match scripts\code.bat's development launch without its synchronous wait
# or prelaunch build; the checkout must already be prepared.
$env:NODE_ENV = 'development'
$env:VSCODE_DEV = '1'
$env:VSCODE_CLI = '1'
Remove-Item Env:ELECTRON_RUN_AS_NODE -ErrorAction SilentlyContinue
$codeProcess = Start-Process `
    -FilePath (Join-Path $codeRoot '.build\electron\Code - OSS.exe') `
    -WorkingDirectory $codeRoot `
    -ArgumentList @('.', '--user-data-dir', "`"$profile`"", '--extensions-dir', "`"$extensions`"", '--disable-extensions', '--skip-welcome', '--skip-release-notes', '--new-window', "`"$workspace`"") `
    -RedirectStandardOutput (Join-Path $recordingRoot 'launch.stdout.log') `
    -RedirectStandardError (Join-Path $recordingRoot 'launch.stderr.log') `
    -PassThru
$codeProcess.Id
```

Wait for the window to load before selecting it. If it exits, inspect the launch
logs and fix application setup before trying debugger attachment.

If compilation or native modules are missing, use the Code OSS checkout's
normal setup/build procedure first; do not fabricate a runnable application
by stubbing modules. Do not install/rebuild dependencies in a shared dirty
checkout without agreement. A missing
`@vscode/policy-watcher` native binding is an **application startup failure**,
not a dbgjs or FFmpeg failure.

### Targeted native dependency repair on Windows

When authorized to repair an existing checkout, first read its `.npmrc` and
launch error. Match its Electron target, headers, architecture and ABI, not
the system Node runtime. Do not replace a dirty checkout's dependencies with a
blanket install, change its lockfiles, or stub required native modules.

For a missing addon that already has a configured `build\binding.sln`, rebuild
that solution using the installed Visual Studio Build Tools. Activate Node in
the same shell: SQLite's custom build steps invoke `node`.

```powershell
fnm env --shell powershell | Out-String | Invoke-Expression
$msbuild = 'C:\Program Files (x86)\Microsoft Visual Studio\2022\BuildTools\MSBuild\Current\Bin\MSBuild.exe'
$addon = Join-Path $codeRoot 'node_modules\@vscode\policy-watcher'
& $msbuild (Join-Path $addon 'build\binding.sln') /p:Configuration=Release /p:Platform=x64 /verbosity:minimal /nologo
if ($LASTEXITCODE -ne 0) { throw 'Native addon build failed' }
```

Replace only the package diagnosed as missing. This repaired `@vscode/policy-watcher`,
`@vscode/spdlog`, `@vscode/sqlite3`, and `@vscode/deviceid` in the tested checkout.
For an addon with no configured solution, use the checkout's pinned node-gyp:

```powershell
# Confirm these parameters against this checkout's .npmrc before running.
$electronVersion = '42.8.1'
$nodeGyp = Join-Path $codeRoot 'build\npm\gyp\node_modules\.bin\node-gyp.cmd'
Push-Location (Join-Path $codeRoot 'node_modules\native-keymap')
try {
    & $nodeGyp rebuild "--target=$electronVersion" --dist-url=https://electronjs.org/headers --arch=x64
    if ($LASTEXITCODE -ne 0) { throw 'Native addon rebuild failed' }
} finally { Pop-Location }
```

This repaired `native-keymap` and `native-is-elevated` for Electron 42.8.1
(Node module ABI 146). Recheck launch logs after each diagnosed repair.
`--disable-extensions` does not guarantee all built-in extensions are disabled:
uncompiled built-ins may still report activation errors. Distinguish those
from failure to create the workbench; do not expand a recording task into
repairing unrelated extensions or account authentication.

Alternatively attach to an existing Code OSS window, with permission to
interact with it. Do not run the demo below against someone's working editor:
it creates an unsaved editor. Isolated profiles avoid account data, extensions,
and unrelated workspace content leaking into the recording.

## 3. Discover and select the workbench window

```powershell
& $dbgjs context create :code-oss-video 'Code OSS video' --set
& $dbgjs process list --root vscode --no-cmd-line --no-trim
# Replace this example with the actual Code - OSS window reference.
& $dbgjs process attach w:12345/1 --set
& $dbgjs playwright 'return { title: await page.title(), url: page.url(), screencast: typeof page.screencast?.start };'
```

Check every exit code (`$LASTEXITCODE`) before continuing. Choose the isolated
window by title, application and root PID. Use its `w:<main-pid>/<window-id>`
reference, not an extension host, agent host, arbitrary iframe, or the first
renderer PID. A window selector resolves its primary `webContents`.

No remote-debugging port is required when dbgjs attaches through the Windows
Electron process bridge. If using an explicitly enabled browser CDP endpoint
instead, bind it to loopback; do not expose debugger access on the network.

## 4. Record and finalize in ONE Playwright invocation

The public `page.screencast` API can start video on an **existing attached
page**. `page.video()` stays `null` unless automatic context-level recording was
configured when that page was created. Do not try to retrofit `recordVideo`
onto `page.context()` or create a new browser context through the page-scoped
relay: neither records the existing Code OSS renderer.

The following is a small Windows demo. Replace the action section for the
actual task, but retain the frame check and `finally`. Keep absolute output
paths and make sure the selected window is ready and visible:

```powershell
$env:DBGJS_RECORDING_OUTPUT = $recordingRoot
@'
const { resolve, join } = await import('node:path');
const { mkdir, stat } = await import('node:fs/promises');
if (!page.screencast?.start) throw new Error('Playwright 1.62.1 screencast API required');
if (!process.env.DBGJS_RECORDING_OUTPUT) throw new Error('DBGJS_RECORDING_OUTPUT required');
const output = resolve(process.env.DBGJS_RECORDING_OUTPUT);
await mkdir(output, { recursive: true });
const video = join(output, 'code-oss.webm');
const screenshot = join(output, 'code-oss.png');
page.setDefaultTimeout(5000);
await page.setViewportSize({ width: 1280, height: 720 });
await page.locator('.monaco-workbench').waitFor({ state: 'visible' });
let frames = 0;
await page.screencast.start({
    path: video,
    size: { width: 1280, height: 720 },
    onFrame: () => { frames++; },
});
try {
    // Replace these demo actions; record only a disposable window.
    await page.locator('.monaco-workbench').click();
    await page.keyboard.press('Control+n');
    await page.locator('.monaco-editor.focused').waitFor({ state: 'visible' });
    await page.keyboard.type('Hello from a dbgjs recording!', { delay: 60 });
    await page.waitForTimeout(500);
    await page.screenshot({ path: screenshot });
} finally {
    await page.screencast.stop();
}
if (frames < 2) throw new Error(`Only ${frames} screencast frames; inspect relay support and window visibility`);
const bytes = (await stat(video)).size;
if (!bytes) throw new Error('Empty video output');
return { video, screenshot, frames, bytes, title: await page.title() };
'@ | & $dbgjs playwright -
if ($LASTEXITCODE -ne 0) { throw 'Recording failed; inspect diagnostics and do not publish the output' }
```

`stop()` finalizes the encoder and saves the requested file **before** returning.
Open the WebM in a video player and verify duration, dimensions, visible actions,
and readable text. `size` specifies output dimensions; it does not resize the
native Code OSS window. Match its aspect ratio and choose an appropriate
window size to avoid scaling/padding. PNG is a separate still, not the video.

### Important bounds and failure modes

- One `dbgjs playwright` call has a 30-second outer deadline. The helper has at
  most 27 seconds, and the proxy capability at most 24 seconds. Connection,
  actions, encoding and saving must all fit. Aim for demos well below 20
  seconds; lower action timeouts and include cleanup time.
- Start/actions/stop must be in that same call. Each invocation creates and
  closes a fresh Playwright connection. A second invocation cannot stop or
  retrieve the first invocation's recording.
- A timeout, killed helper, target closure or detach can prevent finalization.
  Do not advertise partial output as successful.
- **A valid WebM can be entirely white.** Playwright's encoder synthesizes a
  white frame if it receives none. Old dbgjs relays reject screencast commands;
  Playwright can swallow that start error. Require real frame callbacks,
  visible changes, and playback, not merely file existence/size.
- **HiDPI can break the frame callback without breaking encoding.** In the
  tested Electron 42.8.1 workbench, CDP reported a fractional `deviceWidth`
  (`1446.4000244140625`), while Playwright 1.62.1's frame-event schema requires
  integer viewport dimensions. Zero `onFrame` callbacks also reproduced with
  direct Playwright, so this symptom alone does not prove relay rejection.
  Setting an explicit integer viewport before recording restored callbacks
  (249 frames in the verified direct-CDP demo). Output `size` alone is not
  sufficient; viewport emulation does not promise native-window resizing.
- If the live workbench works via direct CDP but dbgjs locators or utility
  evaluation time out, stop retrying the same call. The tested checkout
  intermittently succeeded through dbgjs, then repeatedly failed with
  `Playwright executing exceeded its deadline` or
  `context relay cleanup exceeded its deadline`. The cause is unresolved;
  do not claim waiting for source discovery or reconnecting fixes it.
  Use the isolated direct-CDP fallback below and report that it bypasses dbgjs.
- For diagnosis within a `dbgjs playwright` program, use
  `page.context().newCDPSession(page)` and explicitly send
  `Page.startScreencast`, `Page.screencastFrameAck`, `Page.stopScreencast`.
  An allowlist error indicates an old relay. The numeric `params.sessionId`
  for frame acknowledgement is a frame sequence, not the string routing
  `sessionId`. Stop and detach that auxiliary session in `finally`.
- Keep the window unminimized. Capture is the renderer surface: no native
  title bar, OS dialogs, desktop, other windows, or audio. Webviews/compositing
  may vary; review the actual output rather than promising desktop fidelity.
- Avoid parallel recorders/tracing screencasts on the same target. Do not close
  `page`, its context, or the application merely to save a video.

### Direct-CDP fallback for a running isolated workbench

Before launching the disposable application, add
`'--remote-debugging-address=127.0.0.1', '--remote-debugging-port=0'`
to the launch argument list above. Read the dynamically allocated endpoint from
that profile's `DevToolsActivePort`; never guess a port or attach to a different
profile. This is a separate Playwright connection, not dbgjs's selected-page
relay. Stop only the dedicated dbgjs daemon first to avoid overlapping recorders.

Run from the dbgjs checkout, using its installed Playwright package:

```powershell
& $dbgjs service stop
$env:DBGJS_RECORDING_OUTPUT = $recordingRoot
@'
import { chromium } from 'playwright';
import { readFileSync, statSync } from 'node:fs';
import { join, resolve } from 'node:path';
if (!process.env.DBGJS_RECORDING_OUTPUT) throw new Error('DBGJS_RECORDING_OUTPUT required');
const root = resolve(process.env.DBGJS_RECORDING_OUTPUT);
const port = readFileSync(join(root, 'profile', 'DevToolsActivePort'), 'utf8').split('\n')[0];
const browser = await chromium.connectOverCDP(`http://127.0.0.1:${port}`, { timeout: 10000 });
try {
    const pages = browser.contexts().flatMap(context => context.pages())
        .filter(page => page.url().includes('/workbench/'));
    if (pages.length !== 1) throw new Error(`Expected one workbench, found ${pages.length}`);
    const page = pages[0];
    page.setDefaultTimeout(5000);
    await page.setViewportSize({ width: 1280, height: 720 });
    await page.locator('.monaco-workbench').waitFor({ state: 'visible' });
    const video = join(root, 'workbench-demo.webm');
    const screenshot = join(root, 'workbench-demo.png');
    let frames = 0;
    await page.screencast.start({
        path: video, size: { width: 1280, height: 720 },
        onFrame: () => { frames++; },
    });
    try {
        await page.keyboard.press('Control+n');
        await page.locator('.monaco-editor.focused').waitFor({ state: 'visible' });
        await page.keyboard.type('Hello from a Code OSS recording!\n\nReal workbench. Real keyboard input.\nPlaywright screencast + managed FFmpeg.', { delay: 55 });
        await page.waitForTimeout(800);
        await page.screenshot({ path: screenshot });
    } finally { await page.screencast.stop(); }
    if (frames < 2) throw new Error(`Only ${frames} screencast frames`);
    const bytes = statSync(video).size;
    if (!bytes) throw new Error('Empty video output');
    console.log(JSON.stringify({ video, screenshot, frames, bytes, title: await page.title() }));
} finally { await browser.close(); }
'@ | node --input-type=module -
if ($LASTEXITCODE -ne 0) { throw 'Direct-CDP recording failed' }
```

For `connectOverCDP`, closing this Playwright browser connection disconnects it;
it does not shut down the externally launched workbench. Verify playback as
above. Keep this endpoint loopback-only and close the disposable application
when its remaining demo use is complete.

## 5. Distinguish other kinds of recordings

| Output | Purpose | Playable video? |
| --- | --- | --- |
| `dbgjs screenshot capture --output <path>` or `page.screenshot()` | One rendered frame | No |
| Playwright tracing with screenshots/snapshots | Debug actions, DOM and timeline in Trace Viewer | No, a trace ZIP is not a WebM |
| dbgjs CDP wire recordings / test transcripts | Protocol analysis and replay | No |
| `page.screencast.start({ path: '*.webm' })` then `stop()` | Existing renderer video | Yes |
| `recordVideo` at Playwright/Electron launch/context creation | Automatically record pages owned from creation | Yes, but not a retrofit for this attached window |

For longer sessions, dbgjs currently lacks a persistent, cancellable recording
lease with separate start/stop/status, progress, and robust finalization beyond
the one-shot deadline. That would require deliberate lifecycle/API work; do not
pretend a loop of screenshots or two separate `playwright` commands solves it.
An externally launched Playwright Electron application with launch-time
`recordVideo` is an alternative, but changes application ownership/launch and
is not the existing-process dbgjs workflow.

## 6. Cleanup and regression validation

```powershell
& $dbgjs context delete --context :code-oss-video
& $dbgjs service stop
```

These commands address only the dedicated daemon configured above. Detaching
does not close an externally launched Code OSS application. Close the disposable
window after discarding its demo editor, or stop only the exact application PID
you launched. Never stop every Code/Electron/Node process by name. Keep the WebM
and PNG; delete only known per-run profile/extension/workspace directories after
their processes exit. Do not commit profiles, service state, or recordings.
Remove the per-run `DBGJS_*` environment variables when finished.

To validate the relay change using existing repository tooling:

```powershell
cargo test --locked -p dbgjs playwright_proxy --lib
cargo build --locked --bin dbgjs --bin dbgjs-service
npx playwright install chromium
$env:DBGJS_TEST_BIN_DIR = 'target/debug'
npx playwright test playwright-page-cli.spec.mjs electron-process-tree.spec.mjs
```

The browser test checks frame delivery, final WebM decoding/dimensions/duration,
`page.video() === null`, and that unrelated pages remain usable. The Electron
test checks actual renderer-bridge frame delivery and finalized WebM output on
Windows. Use filename selectors as shown: Playwright treats arguments as regular
expressions, so literal Windows backslash paths do not select tests reliably.

### Validation evidence (2026-10-21, Windows)

- All 41 targeted Rust relay tests passed.
- Both targeted Playwright tests above passed with Playwright 1.62.1.
- Hosted CI for the relay change passed, including the Windows recording checks.
- The actual source-built Code OSS workbench ran after the targeted native
  repairs above, without changing unrelated source edits. Electron 42.8.1,
  Chromium 148.0.7778.280, Playwright 1.62.1 and managed FFmpeg revision 1011
  were used.
- A dbgjs-driven attempt saved a genuine 13.96-second 1280x720 video and PNG,
  but its frame guard failed on the fractional-viewport callback issue.
  Subsequent dbgjs automation stalled; this is not a reliable end-to-end pass.
- The direct-CDP fallback with an explicit integer viewport completed with
  249 callbacks and a 408375-byte WebM. Independent Chromium playback decoded
  9.04 seconds at 1280x720, with different initial/final frame samples.
  The PNG and decoded final frame showed the real workbench and typed demo.
  These results verify the application and direct recording workflow, not
  resolution of the dbgjs automation stalls.

References: [Playwright Screencast](https://playwright.dev/docs/api/class-screencast),
[videos](https://playwright.dev/docs/videos),
[CDP attachment](https://playwright.dev/docs/api/class-browsertype#browser-type-connect-over-cdp),
and the [process attachment walkthrough](../../../docs/walkthroughs/debugging-vscode-processes.md).
For version-specific behavior inspect the locked Playwright package's
`src/server/screencast.ts`, `videoRecorder.ts`, `chromium/crPage.ts`, and
`src/client/screencast.ts` (distributed here in `lib/coreBundle.js`).
