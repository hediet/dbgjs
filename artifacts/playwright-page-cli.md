# Playwright against a selected `dbgjs` page

This is a real Chromium/Playwright E2E run. The daemon control plane uses authenticated local IPC; Playwright receives a one-shot, capability-URL WebSocket bound only to loopback.

```console
$ dbgjs context create --context :playwright-e2e
Context playwright-e2e  rev 1
  Name: playwright-e2e
  Connections: none
```

```console
$ dbgjs connection add ws://127.0.0.1:57262/devtools/browser/7d60dd17-1cf6-4120-9dae-6b3df8081bc3 --context :playwright-e2e --connection browser --connect
Context playwright-e2e  rev 4
  Name: playwright-e2e
  Connections:
    browser  [connected to HeadlessChrome/151.0.7922.34; CDP 1.3; generation 1]
      Configuration: direct CDP at ws://127.0.0.1:57262/devtools/browser/7d60dd17-1cf6-4120-9dae-6b3df8081bc3
      Targets:
        dbgjs Playwright E2E  dbgjs Playwright E2E  http://127.0.0.1:57261/  [a CDP client is attached]
          iframe  http://localhost:57260/child  http://localhost:57260/child  [a CDP client is attached]
        unrelated owner page  unrelated owner page  about:blank  [a CDP client is attached]
```

```console
$ dbgjs playwright "await page.mouse.wheel(0, 800)" --context :playwright-e2e --connection browser --target 54A6C0F9F0894967C2402DC8CF96F6CF

```

```console
$ dbgjs playwright "return { title: await page.title(), scrollY: await page.evaluate(() => scrollY) };" --context :playwright-e2e --connection browser --target 54A6C0F9F0894967C2402DC8CF96F6CF
{
  "title": "dbgjs Playwright E2E",
  "scrollY": 800
}
```

```console
$ dbgjs playwright - --context :playwright-e2e --connection browser --target 54A6C0F9F0894967C2402DC8CF96F6CF <<'JS'
return { url: page.url(), marker: await page.locator("#marker").textContent() };
JS
{
  "url": "http://127.0.0.1:57261/",
  "marker": "selected page"
}
```

```console
$ dbgjs playwright "return { text: await page.frameLocator(\"#oopif\").locator(\"#oopif-marker\").textContent() };" --context :playwright-e2e --connection browser --target 54A6C0F9F0894967C2402DC8CF96F6CF
{
  "text": "cross-origin iframe"
}
```

```console
$ dbgjs playwright "const [chooser] = await Promise.all([\n\t\t\t\t\tpage.waitForEvent(\"filechooser\"),\n\t\t\t\t\tpage.locator(\"#file\").click(),\n\t\t\t\t]);\n\t\t\t\treturn { multiple: chooser.isMultiple(), title: await page.title() };" --context :playwright-e2e --connection browser --target 54A6C0F9F0894967C2402DC8CF96F6CF
{
  "multiple": false,
  "title": "dbgjs Playwright E2E"
}
```

```console
$ dbgjs playwright "await page.setExtraHTTPHeaders({ targetId: \"opaque-header-value\" });\n\t\t\t\tawait page.goto(\"http://127.0.0.1:57261/opaque-header\");\n\t\t\t\treturn {\n\t\t\t\t\theader: await page.locator(\"#target-id-header\").textContent(),\n\t\t\t\t\toopif: await page.frameLocator(\"#oopif\").locator(\"#oopif-marker\").textContent(),\n\t\t\t\t};" --context :playwright-e2e --connection browser --target 54A6C0F9F0894967C2402DC8CF96F6CF
{
  "header": "opaque-header-value",
  "oopif": "cross-origin iframe"
}
```

```console
$ dbgjs playwright "const session = await page.context().newCDPSession(page);\n\t\t\t\tconst evaluation = await session.send(\"Runtime.evaluate\", {\n\t\t\t\t\texpression: \"document.title\",\n\t\t\t\t\treturnByValue: true,\n\t\t\t\t});\n\t\t\t\tawait session.detach();\n\t\t\t\treturn {\n\t\t\t\t\tauxiliarySessionTitle: evaluation.result.value,\n\t\t\t\t\tpageTitleAfterDetach: await page.title(),\n\t\t\t\t};" --context :playwright-e2e --connection browser --target 54A6C0F9F0894967C2402DC8CF96F6CF
{
  "auxiliarySessionTitle": "dbgjs Playwright E2E",
  "pageTitleAfterDetach": "dbgjs Playwright E2E"
}
```

```console
$ dbgjs playwright "async function rejection(operation) {\n\t\t\t\t\t\ttry { await operation(); return null; } catch (error) { return error.message; }\n\t\t\t\t\t}\n\t\t\t\t\treturn {\n\t\t\t\t\t\tnewPage: await rejection(() => page.context().newPage()),\n\t\t\t\t\t\tnewContext: await rejection(() => page.context().browser().newContext()),\n\t\t\t\t\t\tclearCookies: await rejection(() => page.context().clearCookies()),\n\t\t\t\t\t};" --context :playwright-e2e --connection browser --target 54A6C0F9F0894967C2402DC8CF96F6CF
{
  "newPage": "browserContext.newPage: Protocol error (Target.createTarget): CDP method 'Target.createTarget' is outside the selected page allowlist",
  "newContext": "browser.newContext: Protocol error (Target.createBrowserContext): CDP method 'Target.createBrowserContext' is outside the selected page allowlist",
  "clearCookies": "browserContext.clearCookies: Protocol error (Storage.clearCookies): CDP method 'Storage.clearCookies' is outside the selected page allowlist"
}
```

```console
$ dbgjs playwright "<pending page.evaluate>" --context :playwright-e2e --connection browser --target 54A6C0F9F0894967C2402DC8CF96F6CF
dbgjs: page.evaluate: Target page, context or browser has been closed
    at eval (<anonymous>:3:12)
    at main (<worktree>\[eval1]:31:49)
```

A verified cross-origin iframe, file chooser interception, and an opaque `targetId` HTTP header worked through the selected page. Detaching an auxiliary CDP session left the page proxy usable. Browser-wide operations were rejected, the unrelated page remained untouched, and destroying the selected page cancelled its pending proxy command promptly. The original Playwright owner remained connected.
