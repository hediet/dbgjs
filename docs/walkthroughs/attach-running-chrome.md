# Attach to an already-running Google Chrome

This workflow starts **installed Google Chrome independently**, then attaches
dbgjs. It does not use `connection add --chrome` (which launches a browser),
Playwright's bundled Chromium, or the user's everyday Chrome profile.

## Chrome setup and approval

Verified on Windows with **Google Chrome 156.0.8078.48**:

1. Start Chrome with a fresh, absolute `--user-data-dir`. Do not sign in.
2. Open `chrome://inspect/#remote-debugging`.
3. Check **Allow remote debugging for this browser instance**.
4. Run the dbgjs connection command below.
5. In Chrome's **Allow remote debugging?** native dialog, click **Allow**.
   This grants the requesting debugger access to the disposable browser.

No `chrome://flags` change, remote-debugging launch flag, or restart was needed
to enable this setting in the running Chrome 156 instance. The setting alone
permits *requests*, not silent control. Chrome displays an automation banner
after approval. The actual prompt says **Allow**, not “I approve the incoming
CDP connection”. Cancellation must remain an option.

This Chrome listener returned **404** for both `/json/version` and `/json/list`.
That is not evidence that debugging is disabled. Chrome wrote its port and
browser WebSocket path to `DevToolsActivePort` in the user-data directory.
dbgjs reads that file for a recognized browser with an explicit absolute
`--user-data-dir`, checks that the selected PID owns the listening port, then
requests the WebSocket connection. It never edits Chrome preferences, guesses
the normal profile, accepts consent, or launches/restarts Chrome.

Recognized browsers come from one list in the service
(`CHROMIUM_BROWSER_EXECUTABLES` in `process_discovery.rs`), covering Windows,
Linux and macOS executable names of Chrome, Chromium, Edge, Brave and Opera.
The profile is read from the exact argv on Linux, quote-aware on Windows, and
up to the next ` --` switch on macOS, where `ps` joins arguments with spaces.
Only Chrome 156 on Windows has been verified end to end; other browsers must
offer the same request-based remote-debugging setting.

In this run the UI listener used `127.0.0.1:9222`. Inspect the actual value;
do not assume that a listener on 9222 belongs to the intended browser.
Do not allow public/private network access in a Windows Firewall prompt for
this loopback-only demo. That OS prompt is separate from Chrome's CDP approval.

## Reproducible PowerShell instructions

Use matching CLI and service builds. An interface-hash mismatch is a binary
version problem, not a Chrome approval problem. If building:

```powershell
cargo build --locked --bin dbgjs --bin dbgjs-service
if ($LASTEXITCODE -ne 0) { throw 'dbgjs build failed' }
$dbgjs = (Resolve-Path .\target\debug\dbgjs.exe).Path
$env:DBGJS_SERVICE_EXE = (Resolve-Path .\target\debug\dbgjs-service.exe).Path
```

Do not stop a shared daemon merely to overwrite its running executable.
For an installed matching pair, set `$dbgjs` and `DBGJS_SERVICE_EXE` to that
pair instead. Activate Node before starting the dedicated service; process
discovery needs Node with built-in WebSocket support (Node 22+ works).

```powershell
fnm env --shell powershell | Out-String | Invoke-Expression
$run = Join-Path $env:TEMP ('dbgjs-chrome-' + [guid]::NewGuid())
$profile = Join-Path $run 'profile'
New-Item -ItemType Directory -Path $run | Out-Null
$env:DBGJS_SERVICE_STATE = Join-Path $run 'service.json'
$chrome = Join-Path $env:LOCALAPPDATA 'Google\Chrome\Application\chrome.exe'
# Use the actual installed path; system installs may be under Program Files.
if (!(Test-Path $chrome)) { throw 'Locate the installed Google Chrome executable' }

# Chrome starts independently, before any dbgjs command.
$browser = Start-Process $chrome -PassThru -ArgumentList @(
    "--user-data-dir=`"$profile`"",
    '--no-first-run', '--no-default-browser-check',
    '--new-window', 'about:blank'
)
```

Open the settings URL and enable requests manually in this disposable window.
No accessibility switch is needed for a person to approve. For Windows UI
Automation inspection, `--force-renderer-accessibility` can additionally be
passed at launch; it is **not** a remote-debugging or consent-bypass flag.

```powershell
& $dbgjs context create :chrome-demo 'Disposable Chrome' --set
if ($LASTEXITCODE -ne 0) { throw 'Context creation failed' }
& $dbgjs connection add --process $browser.Id --connection chrome --connect
# While this waits, approve the native Chrome dialog. After 3 seconds dbgjs
# prints a reminder to stderr; it gives up after 120 seconds without approval.
if ($LASTEXITCODE -ne 0) { throw 'Chrome attachment failed; inspect Chrome and the error' }
& $dbgjs target list --connection chrome
# Copy a page target ID from the list; do not select browser_ui/worker targets.
$pageId = '<page target ID>'
& $dbgjs target eval 'document.title' --connection chrome --target $pageId
if ($LASTEXITCODE -ne 0) { throw 'Page evaluation failed' }
```

Use **`connection add --process`**, not `process attach p:<pid>` for this
browser-root workflow. The latter currently uses the Node/Electron process-tree
root shortcut and can attempt `Runtime.enable` on a browser endpoint.
The single-process connection preserves browser topology and exposes its tabs.
Specify the page explicitly; several tabs and browser UI targets may exist.

### Manual endpoint fallback

With older dbgjs versions that support direct CDP, read the two lines yourself:

```powershell
$lines = Get-Content (Join-Path $profile 'DevToolsActivePort')
$endpoint = "ws://127.0.0.1:$($lines[0])$($lines[1])"
& $dbgjs connection add $endpoint --connection chrome --connect
```

Validate PID/port ownership before using a stale file. This still triggers
Chrome's approval dialog. `--set` is not accepted by this positional direct-CDP
form; use explicit connection/target selectors.

### Distinct legacy launch-flag route

`--remote-debugging-port=0 --remote-debugging-address=127.0.0.1` is a
different, launch-time opt-in route. It needs a non-default `--user-data-dir`
on Chrome 136+ and must be present when that profile's browser process starts.
Passing flags to a second invocation sharing an already-running profile does
not retrofit that process. Do not use it to avoid demonstrating the
approval-gated UI route, and never disable Chrome's profile protections.

## Native approval and whole-screen video

The approval is browser chrome, not a page DOM element. The Windows demo used
UI Automation (`UIAutomationClient`/`UIAutomationTypes`) to find the
**Allow remote debugging?** dialog beneath the exact disposable Chrome window,
then invoke its **Allow** button's `InvokePattern`. Scope by the window handle
and verify its process ID before acting. Do not search the entire desktop for
an arbitrary “Allow” button. Page locators, `target eval`, and tab screenshots
are not substitutes for this native interaction.

For an explicitly authorized disposable window, this invokes the native
approval without clicking unrelated desktop buttons. Obtain `$chromeHwnd`
from the intended window (not an arbitrary Chrome window if several exist):

```powershell
Add-Type -AssemblyName UIAutomationClient, UIAutomationTypes
$window = [System.Windows.Automation.AutomationElement]::FromHandle($chromeHwnd)
if ($window.Current.ProcessId -ne $browser.Id) { throw 'Wrong browser window' }
$byName = [System.Windows.Automation.AutomationElement]::NameProperty
$descendants = [System.Windows.Automation.TreeScope]::Descendants
$dialog = $window.FindFirst($descendants,
    (New-Object System.Windows.Automation.PropertyCondition($byName, 'Allow remote debugging?')))
if (!$dialog) { throw 'Native Chrome approval dialog not found' }
$allow = $dialog.FindFirst($descendants,
    (New-Object System.Windows.Automation.PropertyCondition($byName, 'Allow')))
if (!$allow -or !$allow.Current.IsEnabled) { throw 'Approval button unavailable' }
$invoke = $allow.GetCurrentPattern([System.Windows.Automation.InvokePattern]::Pattern)
$invoke.Invoke()
```

dbgjs `target record` and Playwright screencasts capture only a renderer.
For a whole-screen Windows demo, use a full external FFmpeg build with
`gdigrab`, not Playwright's minimal encoder:

```powershell
& $ffmpeg -hide_banner -f gdigrab -framerate 15 -draw_mouse 1 `
    -i desktop -t 60 -c:v libx264 -preset veryfast -crf 20 `
    -pix_fmt yuv420p -movflags +faststart (Join-Path $run 'chrome-attach.mp4')
if ($LASTEXITCODE -ne 0) { throw 'Desktop recording failed' }
```

`desktop` captures the complete virtual desktop, including native prompts.
Keep the RDP desktop unlocked, connected, and at a fixed resolution. A locked
or secure desktop can produce access-denied/invalid-handle errors; reconnect
or dismiss the relevant OS prompt yourself, never bypass the protection.
Confirm physical pixel dimensions (DPI-scaled PowerShell screen dimensions may
differ), decode the video, and inspect frames showing the prompt and result.
Use a neutral full-screen backdrop and a disposable Chrome window; avoid
unrelated screen content, notifications and simultaneous user input.
Do not publish failed, blank, cropped, or sensitive captures as proof.

## Cleanup and current limitations

```powershell
& $dbgjs context delete --context :chrome-demo
& $dbgjs service stop # only the dedicated DBGJS_SERVICE_STATE above
# Close only the disposable Chrome windows. Disconnecting leaves Chrome running.
# After confirming all processes using $profile have exited, remove that exact
# profile directory, not the user's normal profile or the whole artifacts tree.
```

- No built-in desktop recorder or native Chrome consent automation in dbgjs.
- No automatic discovery of the normal profile or relative profile paths.
- PID ownership checks reject stale/reused-port files; approval remains in Chrome.
- No automatic tab choice in a multi-target browser; use `target list`.
- New requests/reconnections can require renewed approval; keep Chrome visible.
- Browser-root `process attach` remains a separate process-tree limitation.

## Validation evidence

The local Windows demo used the official per-user Google Chrome installation
at `%LOCALAPPDATA%\Google\Chrome\Application\chrome.exe`, not Chrome for Testing.
The Google-signed installer was downloaded from
`https://dl.google.com/chrome/install/latest/chrome_installer.exe` after
`winget --scope user` reported no applicable Chrome installer.
The full user-installed encoder was Gyan FFmpeg 9.0.2 (`winget install --id
Gyan.FFmpeg --exact --scope user`).

- Chrome 156.0.8078.48 was running as PID 26960 before dbgjs attached. Enabling
  the UI setting did not change that PID or require a restart.
- Direct WebSocket and improved `connection add --process` both reached the
  native approval dialog; native **Allow** enabled evaluation of an existing tab.
- Native **Cancel** produced HTTP 403, no targets and a failed connection.
  This exposed a CLI bug: failed connections previously exited zero.
  `connection add --connect` and `connection connect` now exit nonzero and
  retain configuration for inspection/retry without changing selection.
- The full-screen H.264 MP4 is
  `artifacts/chrome-attach-demo/chrome-attach-full-screen.mp4` (local, not committed):
  **55 seconds, 1732x1680, 823 decoded frames**. FFmpeg reported
  `Capturing whole desktop ... at (0,0)`. Full decoding succeeded; decoded
  frames around 13 seconds show the native prompt, and later frames show the
  green success message written through dbgjs. No audio was recorded.
- Nine process-discovery tests, 129 CLI unit tests, and both process-attachment
  integration tests passed locally. The browser integration used the real
  installed Chrome independently started with an isolated headless profile;
  the separate headed video proves the UI approval path.

References: Chrome's [approval-based connection flow (144+)](
https://developer.chrome.com/blog/chrome-devtools-mcp-debug-your-browser-session)
and [default-profile launch-flag restrictions (136+)](
https://developer.chrome.com/blog/remote-debugging-port).
