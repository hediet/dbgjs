# Native screen and window capture (October 2026)

`dbgjs screenshot capture` captures the attached CDP page. When an agent drives
VS Code (or another app) on a remote machine through a dbgjs hub service, the
native OS UI stays invisible: macOS privacy (TCC) prompts, VS Code's native
dialogs, and other applications the agent under test controls. This note records
the first version of host-level capture and what remains open.

## What exists

A new service interface, `dev.dbgjs.desktop` (`DesktopApi`), runs in the
debugger service process, so it captures the machine that runs the service,
also when the CLI reaches that service through `--service-endpoint`:

| Method | Result |
| --- | --- |
| `list_displays()` | id, name, position, size, scale factor, primary flag |
| `list_windows()` | visible top-level windows front to back: id, pid, app name, title, bounds, minimized/focused |
| `capture_display(display_id?)` | PNG of one display; primary when omitted |
| `capture_window(window_id)` | PNG of one top-level window |

CLI:

```sh
dbgjs screenshot displays
dbgjs screenshot windows
dbgjs screenshot capture --screen [--display <display-id>] [--output <path>]
dbgjs screenshot capture --window <id:<window-id>|pid:<process-id>|title-text> [--output <path>]
```

The CLI resolves `pid:` and text selectors against `list_windows` (text matches
the title or the application name, case-insensitively) and refuses ambiguous
selectors, listing the candidates' `id:` selectors. These commands need no
context; `--context` still selects the (remote) service, `--target` and
`--connection` are rejected.

The implementation uses the [`xcap`](https://crates.io/crates/xcap) crate on
Windows and macOS and reports `Unsupported` elsewhere.

## Platform findings

### Windows (verified locally)

- Works from a service process in the signed-in user's session (the hub-started
  `dbgjs-service.exe` runs in session 1 here). Verified through a
  `cmd-stdio:` LinkRPC endpoint on a three-monitor, mixed-DPI setup
  (5120x2160 at 125 % plus two 1440x2560 portrait displays at negative
  coordinates): each display and an occluded window captured correctly.
- xcap uses GDI: `BitBlt` from the desktop for displays and
  `PrintWindow(PW_RENDERFULLCONTENT)` for windows, so occluded Chromium/Electron
  windows are captured with their content. Minimized windows cannot be captured
  and are rejected.
- The service process is not DPI-aware, which would mix logical and physical
  coordinates on scaled displays. Each operation therefore switches its thread to
  per-monitor-v2 DPI awareness; all reported coordinates and image sizes are
  physical pixels.
- Before capturing, the service rejects two states that otherwise yield black or
  failing captures: running in session 0 (a Windows service), and an
  inaccessible input desktop (`OpenInputDesktop` fails while the workstation is
  locked or a secure desktop such as a UAC prompt is active). UAC prompts are
  therefore not capturable by design.
- Display ids are `HMONITOR` values and window ids are `HWND` values; both are
  only valid until displays are reconfigured or the window closes.
- A 5120x2160 capture is about 2.3 MB of PNG (3.1 MB base64 on the wire). With
  a release build, `screenshot capture --screen` took about 0.3 s end to end,
  including spawning the `cmd-stdio:` service.
- The release `dbgjs-service.exe` grew by roughly 0.8 MB (36.4 MB versus
  35.6 MB for a release build of a nearby `main`; not a controlled comparison).

### macOS (compiles; not yet run)

- Capture needs the Screen Recording TCC permission. Without it, macOS returns
  no image (`screencapture` prints "could not create image from display") or an
  image without other apps' windows. The service checks
  `CGPreflightScreenCaptureAccess()` first and returns a `PermissionDenied`
  error explaining where to grant it. It never calls
  `CGRequestScreenCaptureAccess()`, which would raise a prompt on the remote
  machine.
- TCC attributes the request to the *responsible process*: the terminal app when
  the service was started from a terminal, otherwise the executable launchd
  started. Under the launchd-started hub that is most likely the hub's own
  executable (for a Node-based hub, its `node` binary), not `dbgjs-service`.
  That explains the observed "could not create image from display": nobody
  granted the hub's executable, and a background process cannot show the
  consent prompt. To verify on the Mac:
  - `launchctl print gui/$(id -u)/<hub-label>` should find the hub. A job in
    the `system/` domain (a LaunchDaemon) runs outside the GUI session and
    cannot capture at all; it must be a LaunchAgent loaded into the user's Aqua
    session.
  - Stream `log stream --predicate 'subsystem == "com.apple.TCC"'` while
    capturing to see which client TCC evaluates, then add that executable with
    "+" under System Settings > Privacy & Security > Screen & System Audio
    Recording and restart the hub.
- Grants for unsigned or ad-hoc signed executables are keyed to their code
  hash, so every rebuild or update of the responsible binary may silently drop
  the permission.
- xcap uses `CGWindowListCreateImage`, deprecated since macOS 14 in favor of
  ScreenCaptureKit (`SCScreenshotManager`). It still works on current macOS,
  but macOS 15 periodically asks the user to re-confirm apps that use it.
- Display coordinates are points; captures have `scale_factor` times more
  pixels. `friendly_name` is avoided because it calls AppKit off the main
  thread.

### Linux (unsupported)

xcap supports X11 (xcb) and Wayland (xdg-desktop-portal, PipeWire), but needs
xcb, D-Bus, and PipeWire development libraries at build time and links them at
run time. That would change the release build and the self-contained npm
binaries, so the dependency is limited to Windows and macOS for now (the lock
file still lists xcap's Linux dependencies, which are not built). On Wayland the
portal also asks the user interactively, which a remote agent cannot answer.

## Alternatives considered

- **Shelling out** (`screencapture -x -D <n>` / `-l <window-id>` on macOS):
  Apple-maintained and permission-equivalent, but window enumeration still needs
  CoreGraphics, and Windows has no built-in equivalent.
- **Direct `windows-rs` / `objc2` code:** more code for the same calls xcap
  already wraps; worth it only if xcap's behavior needs to change (for example,
  ScreenCaptureKit).
- **`screenshots` crate:** xcap's deprecated predecessor.
- **Windows Graphics Capture** (xcap's `wgc` feature): handles some
  hardware-accelerated content better, but draws a capture border on some
  Windows versions and needs a D3D device; GDI was sufficient in testing.

## Open questions

- Which process is TCC's responsible process under the hub's launchd job, and
  is the job a LaunchAgent in the Aqua session? Should the hub or dbgjs ship a
  signed helper app with a stable identity so the grant survives updates?
- Should the CLI offer an explicit `--request-permission` that calls
  `CGRequestScreenCaptureAccess()` while a user is at the remote machine?
- Move macOS capture to ScreenCaptureKit before Apple removes the legacy API?
- Should all displays be stitched into one image (`--display all`), or is one
  display per capture enough?
- Large PNGs travel base64-encoded in one JSON-RPC response. Should captures
  support JPEG, downscaling (`--max-width`), or a region?
- Elevated windows (UIPI) and windows excluded via
  `SetWindowDisplayAffinity` are untested; disconnected or minimized RDP
  sessions likely capture black frames without being detected.
- Linux: shell out to `grim`/`gnome-screenshot`/`import`, or load the xcb and
  portal libraries dynamically?
