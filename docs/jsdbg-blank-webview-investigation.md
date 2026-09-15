# Investigating a blank VS Code webview with jsdbg

Date: 2026-09-10  
Bug: [microsoft/vscode#335418](https://github.com/microsoft/vscode/issues/335418), assigned to Alex Ross (`alexr00`)  
Environment: Windows, VS Code Insiders 1.138.0-insider, commit `1f398ff5fa56ae9b782dc3a55856562233efd72e`.

## Result

The GitHub issue webview requested `webview-pr-description.js` from extension
version **0.167.2026090912**, but its live `localResourceRoots` allowed only the
`dist` directory of **0.165.2026090715**. VS Code returned **401**; the script did
not initialize the application, leaving its root empty.

The requested file existed. This was not a missing bundle, a frozen workbench,
or evidence of a GitHub authentication failure. The user subsequently confirmed
that a newly opened PR view worked while the existing view remained blank.

The update/restore event that introduced the mismatch was not traced. An update
in another window followed by a reload is a hypothesis, not a verified repro.

## 1. Discover the window and capture its original appearance

I started with `jsdbg --help`, `jsdbg context list`, and:

```powershell
jsdbg process list --vscode --no-cmd-line
```

The desired window was `w:62364/1`, titled
`vscode - #335118 Webview resource...`. I created an isolated debugging context
under the session's artifact directory. Attaching by window/PID encountered
ambiguities described in the [problems report](jsdbg-blank-webview-problems.md).
Listing targets exposed the intended Electron webContents as `renderer-1`.

The following examples abbreviate the actual session context as `$ctx`; target
IDs and source coordinates are specific to this investigation.

```powershell
$ctx = 'C:\Users\hdieterichs\.copilot\session-state\20a880e2-b6f7-489e-afc4-f230f63d4ac9\files\jsdbg'
jsdbg context create $ctx 'Blank webview investigation'
jsdbg process attach p:51268 --context $ctx --set
# The process attachment reported multiple webContents; select explicitly:
jsdbg target list --context $ctx
jsdbg target attach --target renderer-1 --context $ctx --set
jsdbg screenshot capture --target renderer-1 --context $ctx --output 'C:\Users\hdieterichs\.copilot\session-state\20a880e2-b6f7-489e-afc4-f230f63d4ac9\files\blank-window-initial.png'
```

The screenshot was taken before DOM/resource investigation or diagnostic probes.
It showed a rendered workbench with an empty central editor, not an entirely
blank application. I initially described the editor as a chat; inspecting its
iframe corrected that identification to the GitHub Issues/PR extension.

## 2. Inspect the editor and its nested frame

I used `jsdbg target cdp Runtime.evaluate` with `returnByValue: true` to inspect
the workbench DOM. Unlike the abbreviated `target eval` preview, this returned
the complete small structured result.

The editor contained a webview iframe with:

- `extensionId=GitHub.vscode-pull-request-github` in its URL;
- class `webview ready`;
- a nonzero rectangle, approximately 1260 x 1043 CSS pixels.

The workbench's `Page.getFrameTree` returned only its root frame.
`Target.getTargets` exposed the out-of-process webview iframe; jsdbg's target
inventory subsequently showed it beneath the renderer.

```powershell
$frame = 'renderer-1/target/813B560AF240615D60C61730F455BF30'
jsdbg target attach --target $frame --context $ctx
```

Inside this outer webview document, a same-origin inner iframe held the
extension's HTML. Evaluating through `document.querySelector("iframe")` exposed
its `contentDocument` and `contentWindow` without navigating or reloading it.

The inner document was `complete`, its body text was empty, and `#app` had no
children. Its external script referenced:

```text
github.vscode-pull-request-github-0.167.2026090912/dist/webview-pr-description.js
```

The script nonce matched the document's CSP nonce. The outer webview had an
active service worker.

## 3. Read the original resource result

The decisive passive observation came from the inner frame's Resource Timing
entries:

```javascript
document.querySelector("iframe").contentWindow.performance
  .getEntriesByType("resource")
  .map(r => r.toJSON())
```

For the script, this returned:

```json
{
  "responseStatus": 401,
  "contentType": "text/plain",
  "decodedBodySize": 0,
  "encodedBodySize": 0,
  "deliveryType": "cache",
  "duration": 6.600000008940697
}
```

PowerShell `Get-Item` independently confirmed that the requested bundle existed
on disk (378,597 bytes). Another extension version, `0.167.2026091004`, was also
installed, but that alone did not explain the denial.

An earlier `fetch` probe from the outer frame returned `TypeError: Failed to
fetch`. That result was inconclusive: the outer document's CSP did not permit
arbitrary fetches. It was **not** used to establish the 401; Resource Timing was.

## 4. Capture the live authorization inputs

Reading VS Code's resource-loading implementation connected 401 to
`AccessDenied`. To distinguish a real root mismatch from speculation about
extension updates, I inspected the actual installed workbench bundle and located
its resource-loading function.

At the relevant point in this build's minified function:

- `o` was the requested URI;
- `e.roots` was the allowed-root array;
- `a` was the resolved authorized resource, or `undefined`.

I enabled the debugger and installed a conditional breakpoint that recorded
those values but always returned `false`, so it did not pause the workbench:

```powershell
jsdbg target cdp Debugger.enable --target renderer-1 --context $ctx
jsdbg target cdp Debugger.setBreakpointByUrl --params '{"lineNumber":5929,"columnNumber":12480,"urlRegex":"workbench\\.desktop\\.main\\.js$","condition":"(globalThis.__blankWebviewResourceProbe = {request:o.toString(),roots:e.roots.map(r=>r.toString()),allowedResource:a?.toString()},false)"}' --target renderer-1 --context $ctx
```

CDP resolved the breakpoint to zero-based line 5929, column 12491.
These coordinates and minified variable names must be rediscovered for another
build; they are not a stable API.

To exercise authorization without executing the bundle, I requested the same
URL through a detached `Image` created in the inner frame, adding
`?blankWindowProbe=1`. Its image-error callback was expected for JavaScript
content and was not itself diagnostic. The breakpoint's captured inputs were:

```text
request:
  .../github.vscode-pull-request-github-0.167.2026090912/
      dist/webview-pr-description.js?blankWindowProbe=1

roots:
  .../github.vscode-pull-request-github-0.165.2026090715/dist

allowedResource:
  undefined
```

This proved the immediate cause: the requested bundle was outside the actual
allowed roots. The new-versus-existing view behavior further supports a
per-panel configuration/restore issue, but the new panel's roots were not
inspected and the lifecycle bug's ownership was not determined.

## 5. Preserve evidence and clean up

I removed the conditional breakpoint, deleted the temporary global, detached the
extra raw CDP session, stopped the hung CLI invocation, and disconnected only
this investigation's jsdbg connection. Its final state was `disconnected` with
zero targets.

No application source was changed, no resource permissions were widened, and
the existing view was neither reloaded nor repaired.

Session evidence files:

- `blank-window-initial.png`: original screenshot.
- `blank-webview-diagnosis.json`: structured observations and limitations.

Both are stored under the session artifact directory used above and surfaced
in the conversation. The public bug omits personal filesystem prefixes and
unrelated window contents.
