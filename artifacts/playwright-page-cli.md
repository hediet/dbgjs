# Playwright against a selected `jsdbg` page

This is a real Chromium/Playwright E2E run. The daemon control plane uses authenticated local IPC; Playwright receives a one-shot, capability-URL WebSocket bound only to loopback.

```console
$ jsdbg context create --context :playwright-e2e
Context playwright-e2e  rev 1
  Name: playwright-e2e
  Connections: none
```

```console
$ jsdbg connection add ws://127.0.0.1:55706/devtools/browser/c6553fb9-a58c-4745-9d90-f6097c8b5f5d --context :playwright-e2e --connection browser --connect
Context playwright-e2e  rev 4
  Name: playwright-e2e
  Connections:
    browser  [connected to Chrome/152.0.7977.65; CDP 1.3; generation 1]
      Configuration: direct CDP at ws://127.0.0.1:55706/devtools/browser/c6553fb9-a58c-4745-9d90-f6097c8b5f5d
      Targets:
        0C567088F22C4FBB906C5A81677199DB  Omnibox Popup  chrome://omnibox-popup.top-chrome/
        unrelated owner page  unrelated owner page  about:blank  [a CDP client is attached]
        jsdbg Playwright E2E  jsdbg Playwright E2E  http://127.0.0.1:55705/  [a CDP client is attached]
          iframe  http://localhost:55704/child  http://localhost:55704/child  [a CDP client is attached]
        E40E08EE0C80D84E93350602CE46F61C  Omnibox Popup  chrome://omnibox-popup.top-chrome/omnibox_popup_aim.html
```

```console
$ jsdbg page playwright --eval "await page.mouse.wheel(0, 800)" --context :playwright-e2e --connection browser --target 27C0DC8614E13113CCEAD319493065AB

```

```console
$ jsdbg page playwright --eval "return { title: await page.title(), scrollY: await page.evaluate(() => scrollY) };" --context :playwright-e2e --connection browser --target 27C0DC8614E13113CCEAD319493065AB
{
  "title": "jsdbg Playwright E2E",
  "scrollY": 800
}
```

```console
$ jsdbg page playwright - --context :playwright-e2e --connection browser --target 27C0DC8614E13113CCEAD319493065AB <<'JS'
return { url: page.url(), marker: await page.locator("#marker").textContent() };
JS
{
  "url": "http://127.0.0.1:55705/",
  "marker": "selected page"
}
```

```console
$ jsdbg page playwright --eval "return { text: await page.frameLocator(\"#oopif\").locator(\"#oopif-marker\").textContent() };" --context :playwright-e2e --connection browser --target 27C0DC8614E13113CCEAD319493065AB
{
  "text": "cross-origin iframe"
}
```

```console
$ jsdbg page playwright --eval "const [chooser] = await Promise.all([\n\t\t\t\t\tpage.waitForEvent(\"filechooser\"),\n\t\t\t\t\tpage.locator(\"#file\").click(),\n\t\t\t\t]);\n\t\t\t\treturn { multiple: chooser.isMultiple(), title: await page.title() };" --context :playwright-e2e --connection browser --target 27C0DC8614E13113CCEAD319493065AB
{
  "multiple": false,
  "title": "jsdbg Playwright E2E"
}
```

```console
$ jsdbg page playwright --eval "await page.setExtraHTTPHeaders({ targetId: \"opaque-header-value\" });\n\t\t\t\tawait page.goto(\"http://127.0.0.1:55705/opaque-header\");\n\t\t\t\treturn {\n\t\t\t\t\theader: await page.locator(\"#target-id-header\").textContent(),\n\t\t\t\t\toopif: await page.frameLocator(\"#oopif\").locator(\"#oopif-marker\").textContent(),\n\t\t\t\t};" --context :playwright-e2e --connection browser --target 27C0DC8614E13113CCEAD319493065AB
{
  "header": "opaque-header-value",
  "oopif": "cross-origin iframe"
}
```

```console
$ jsdbg page playwright --eval "const session = await page.context().newCDPSession(page);\n\t\t\t\tconst evaluation = await session.send(\"Runtime.evaluate\", {\n\t\t\t\t\texpression: \"document.title\",\n\t\t\t\t\treturnByValue: true,\n\t\t\t\t});\n\t\t\t\tawait session.detach();\n\t\t\t\treturn {\n\t\t\t\t\tauxiliarySessionTitle: evaluation.result.value,\n\t\t\t\t\tpageTitleAfterDetach: await page.title(),\n\t\t\t\t};" --context :playwright-e2e --connection browser --target 27C0DC8614E13113CCEAD319493065AB
{
  "auxiliarySessionTitle": "jsdbg Playwright E2E",
  "pageTitleAfterDetach": "jsdbg Playwright E2E"
}
```

```console
$ jsdbg page playwright --eval "async function rejection(operation) {\n\t\t\t\t\t\ttry { await operation(); return null; } catch (error) { return error.message; }\n\t\t\t\t\t}\n\t\t\t\t\treturn {\n\t\t\t\t\t\tnewPage: await rejection(() => page.context().newPage()),\n\t\t\t\t\t\tnewContext: await rejection(() => page.context().browser().newContext()),\n\t\t\t\t\t\tclearCookies: await rejection(() => page.context().clearCookies()),\n\t\t\t\t\t};" --context :playwright-e2e --connection browser --target 27C0DC8614E13113CCEAD319493065AB
{
  "newPage": "browserContext.newPage: Protocol error (Target.createTarget): CDP method 'Target.createTarget' is outside the selected page allowlist",
  "newContext": "browser.newContext: Protocol error (Target.createBrowserContext): CDP method 'Target.createBrowserContext' is outside the selected page allowlist",
  "clearCookies": "browserContext.clearCookies: Protocol error (Storage.clearCookies): CDP method 'Storage.clearCookies' is outside the selected page allowlist"
}
```

```console
$ jsdbg page playwright --eval "<pending page.evaluate>" --context :playwright-e2e --connection browser --target 27C0DC8614E13113CCEAD319493065AB
jsdbg: page.evaluate: Target page, context or browser has been closed
    at eval (<anonymous>:3:12)
    at main (<worktree>\[eval1]:31:49)
```

A verified cross-origin iframe, file chooser interception, and an opaque `targetId` HTTP header worked through the selected page. Detaching an auxiliary CDP session left the page proxy usable. Browser-wide operations were rejected, the unrelated page remained untouched, and destroying the selected page cancelled its pending proxy command promptly. The original Playwright owner remained connected.
