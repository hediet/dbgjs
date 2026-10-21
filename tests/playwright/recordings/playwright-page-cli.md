# Playwright against a selected `dbgjs` page

This is a real Chromium/Playwright E2E run. The daemon control plane uses authenticated local IPC; Playwright receives a one-shot, capability-URL WebSocket bound only to loopback.

```console
$ dbgjs context create --context :playwright-e2e
Context playwright-e2e  rev 1
  Name: playwright-e2e
  Connections: none
```

```console
$ dbgjs connection add ws://127.0.0.1:65122/devtools/browser/bfac1c1b-2228-40a2-9af8-c6f17f126589 --context :playwright-e2e --connection browser --connect
Context playwright-e2e  rev 4
  Name: playwright-e2e
  Connections:
    browser  [connected to HeadlessChrome/151.0.7922.34; CDP 1.3; generation 1]
      Configuration: direct CDP at ws://127.0.0.1:65122/devtools/browser/bfac1c1b-2228-40a2-9af8-c6f17f126589
      Targets:
        dbgjs Playwright E2E  dbgjs Playwright E2E  http://127.0.0.1:65121/  [a CDP client is attached]
          iframe  http://localhost:65120/child  http://localhost:65120/child  [a CDP client is attached]
        unrelated owner page  unrelated owner page  about:blank  [a CDP client is attached]
```

```console
$ dbgjs playwright "await page.mouse.wheel(0, 800)" --context :playwright-e2e --connection browser --target 8F5D417226D0FB24E72A8947A74AD420

```

```console
$ dbgjs playwright "return { title: await page.title(), scrollY: await page.evaluate(() => scrollY) };" --context :playwright-e2e --connection browser --target 8F5D417226D0FB24E72A8947A74AD420
{
  "title": "dbgjs Playwright E2E",
  "scrollY": 800
}
```

```console
$ dbgjs playwright - --context :playwright-e2e --connection browser --target 8F5D417226D0FB24E72A8947A74AD420 <<'JS'
return { url: page.url(), marker: await page.locator("#marker").textContent() };
JS
{
  "url": "http://127.0.0.1:65121/",
  "marker": "selected page"
}
```

```console
$ dbgjs playwright "return { text: await page.frameLocator(\"#oopif\").locator(\"#oopif-marker\").textContent() };" --context :playwright-e2e --connection browser --target 8F5D417226D0FB24E72A8947A74AD420
{
  "text": "cross-origin iframe"
}
```

```console
$ dbgjs playwright "const [chooser] = await Promise.all([\n\t\t\t\t\tpage.waitForEvent(\"filechooser\"),\n\t\t\t\t\tpage.locator(\"#file\").click(),\n\t\t\t\t]);\n\t\t\t\treturn { multiple: chooser.isMultiple(), title: await page.title() };" --context :playwright-e2e --connection browser --target 8F5D417226D0FB24E72A8947A74AD420
{
  "multiple": false,
  "title": "dbgjs Playwright E2E"
}
```

```console
$ dbgjs playwright "await page.setExtraHTTPHeaders({ targetId: \"opaque-header-value\" });\n\t\t\t\tawait page.goto(\"http://127.0.0.1:65121/opaque-header\");\n\t\t\t\treturn {\n\t\t\t\t\theader: await page.locator(\"#target-id-header\").textContent(),\n\t\t\t\t\toopif: await page.frameLocator(\"#oopif\").locator(\"#oopif-marker\").textContent(),\n\t\t\t\t};" --context :playwright-e2e --connection browser --target 8F5D417226D0FB24E72A8947A74AD420
{
  "header": "opaque-header-value",
  "oopif": "cross-origin iframe"
}
```

```console
$ dbgjs playwright "const session = await page.context().newCDPSession(page);\n\t\t\t\tconst evaluation = await session.send(\"Runtime.evaluate\", {\n\t\t\t\t\texpression: \"document.title\",\n\t\t\t\t\treturnByValue: true,\n\t\t\t\t});\n\t\t\t\tawait session.detach();\n\t\t\t\treturn {\n\t\t\t\t\tauxiliarySessionTitle: evaluation.result.value,\n\t\t\t\t\tpageTitleAfterDetach: await page.title(),\n\t\t\t\t};" --context :playwright-e2e --connection browser --target 8F5D417226D0FB24E72A8947A74AD420
{
  "auxiliarySessionTitle": "dbgjs Playwright E2E",
  "pageTitleAfterDetach": "dbgjs Playwright E2E"
}
```

```console
$ dbgjs playwright "let frames = 0;\n\t\t\t\tawait page.screencast.start({\n\t\t\t\t\tpath: \"D:\\\\dev\\\\hediet\\\\dbgjs\\\\.test-tmp\\\\playwright-page-5e0de4e9-eefb-4f64-8172-cc3abdfc43c6\\\\selected-page.webm\",\n\t\t\t\t\tsize: { width: 640, height: 480 },\n\t\t\t\t\tonFrame: () => { frames++; },\n\t\t\t\t});\n\t\t\t\ttry {\n\t\t\t\t\tfor (const color of [\"red\", \"green\", \"blue\"]) {\n\t\t\t\t\t\tawait page.locator(\"#marker\").evaluate((element, color) => {\n\t\t\t\t\t\t\telement.style.background = color;\n\t\t\t\t\t\t}, color);\n\t\t\t\t\t\tawait page.waitForTimeout(200);\n\t\t\t\t\t}\n\t\t\t\t} finally {\n\t\t\t\t\tawait page.screencast.stop();\n\t\t\t\t}\n\t\t\t\tif (frames < 2) throw new Error(\"No changing screencast frames received\");\n\t\t\t\treturn { receivedFrames: frames >= 2, automaticVideo: page.video() };" --context :playwright-e2e --connection browser --target 8F5D417226D0FB24E72A8947A74AD420
{
  "receivedFrames": true,
  "automaticVideo": null
}
```

```console
$ dbgjs playwright "async function rejection(operation) {\n\t\t\t\t\t\ttry { await operation(); return null; } catch (error) { return error.message; }\n\t\t\t\t\t}\n\t\t\t\t\treturn {\n\t\t\t\t\t\tnewPage: await rejection(() => page.context().newPage()),\n\t\t\t\t\t\tnewContext: await rejection(() => page.context().browser().newContext()),\n\t\t\t\t\t\tclearCookies: await rejection(() => page.context().clearCookies()),\n\t\t\t\t\t};" --context :playwright-e2e --connection browser --target 8F5D417226D0FB24E72A8947A74AD420
{
  "newPage": "browserContext.newPage: Protocol error (Target.createTarget): CDP method 'Target.createTarget' is outside the selected page allowlist",
  "newContext": "browser.newContext: Protocol error (Target.createBrowserContext): CDP method 'Target.createBrowserContext' is outside the selected page allowlist",
  "clearCookies": "browserContext.clearCookies: Protocol error (Storage.clearCookies): CDP method 'Storage.clearCookies' is outside the selected page allowlist"
}
```

```console
$ dbgjs playwright "<pending page.evaluate>" --context :playwright-e2e --connection browser --target 8F5D417226D0FB24E72A8947A74AD420
dbgjs: Playwright exited with exit code: 1: Error: page.evaluate: Execution context was destroyed, most likely because of a navigation.
    at runPhase (file:///D:/dev/hediet/dbgjs/[eval1]:116:47)
    at runNextTicks (node:internal/process/task_queues:65:5)
    at process.processImmediate (node:internal/timers:541:9)
    at async main (file:///D:/dev/hediet/dbgjs/[eval1]:54:12)
Caused by: page.evaluate: Execution context was destroyed, most likely because of a navigation.
    at eval (<anonymous>:3:12)
    at <worktree>\[eval1]:65:61
    at async runPhase (<worktree>\[eval1]:104:10)
    at async main (<worktree>\[eval1]:54:12); stderr:
```

A verified cross-origin iframe, file chooser interception, and an opaque `targetId` HTTP header worked through the selected page. Detaching an auxiliary CDP session left the page proxy usable. Browser-wide operations were rejected, the unrelated page remained untouched, and destroying the selected page cancelled its pending proxy command promptly. The original Playwright owner remained connected.
