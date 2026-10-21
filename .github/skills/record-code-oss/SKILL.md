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
& $dbgjs context delete :code-oss-video
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
- The source Code OSS launch at `D:\dev\microsoft\vscode` exited before creating
  a workbench because its `@vscode/policy-watcher` native binding was missing.
  Its unrelated changes were preserved; no dependency repair was attempted.
  The workbench-specific demo actions therefore remain unverified on that
  checkout. Passing the Electron fixture is not proof of a working Code OSS
  source build.

References: [Playwright Screencast](https://playwright.dev/docs/api/class-screencast),
[videos](https://playwright.dev/docs/videos),
[CDP attachment](https://playwright.dev/docs/api/class-browsertype#browser-type-connect-over-cdp),
and the [process attachment walkthrough](../../../docs/walkthroughs/debugging-vscode-processes.md).
For version-specific behavior inspect the locked Playwright package's
`src/server/screencast.ts`, `videoRecorder.ts`, `chromium/crPage.ts`, and
`src/client/screencast.ts` (distributed here in `lib/coreBundle.js`).
