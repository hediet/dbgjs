# Playwright against a selected `jsdbg` page

This is a real Chromium/Playwright E2E run. The daemon control plane uses authenticated local IPC; Playwright receives a one-shot, capability-URL WebSocket bound only to loopback.

```console
$ jsdbg context create --context :playwright-e2e
Context playwright-e2e  rev 1
  Name: playwright-e2e
  Connections: none
```

```console
$ jsdbg connection add ws://127.0.0.1:45229/devtools/browser/0b026440-c0bb-4d28-a042-4455dafa8466 --context :playwright-e2e --connection browser --connect
Context playwright-e2e  rev 4
  Name: playwright-e2e
  Connections:
    browser  [connected to HeadlessChrome/151.0.7922.34; CDP 1.3; generation 1]
      Configuration: direct CDP at ws://127.0.0.1:45229/devtools/browser/0b026440-c0bb-4d28-a042-4455dafa8466
      Targets:
        jsdbg Playwright E2E  jsdbg Playwright E2E  http://127.0.0.1:33357/  [a CDP client is attached]
        unrelated owner page  unrelated owner page  about:blank  [a CDP client is attached]
```

```console
$ jsdbg page playwright --eval "await page.mouse.wheel(0, 800)" --context :playwright-e2e --connection browser --target CA13CA948E4043DCC81C7347F1D37803

```

```console
$ jsdbg page playwright --eval "return { title: await page.title(), scrollY: await page.evaluate(() => scrollY) };" --context :playwright-e2e --connection browser --target CA13CA948E4043DCC81C7347F1D37803
{
  "title": "jsdbg Playwright E2E",
  "scrollY": 800
}
```

```console
$ jsdbg page playwright - --context :playwright-e2e --connection browser --target CA13CA948E4043DCC81C7347F1D37803 <<'JS'
return { url: page.url(), marker: await page.locator("#marker").textContent() };
JS
{
  "url": "http://127.0.0.1:33357/",
  "marker": "selected page"
}
```

```console
$ jsdbg page playwright --eval "async function rejection(operation) {\n\t\t\t\t\t\ttry { await operation(); return null; } catch (error) { return error.message; }\n\t\t\t\t\t}\n\t\t\t\t\treturn {\n\t\t\t\t\t\tnewPage: await rejection(() => page.context().newPage()),\n\t\t\t\t\t\tnewContext: await rejection(() => page.context().browser().newContext()),\n\t\t\t\t\t\tclearCookies: await rejection(() => page.context().clearCookies()),\n\t\t\t\t\t};" --context :playwright-e2e --connection browser --target CA13CA948E4043DCC81C7347F1D37803
{
  "newPage": "browserContext.newPage: Protocol error (Target.createTarget): CDP method 'Target.createTarget' is outside the selected page allowlist",
  "newContext": "browser.newContext: Protocol error (Target.createBrowserContext): CDP method 'Target.createBrowserContext' is outside the selected page allowlist",
  "clearCookies": "browserContext.clearCookies: Protocol error (Storage.clearCookies): CDP method 'Storage.clearCookies' is outside the selected page allowlist"
}
```

```console
$ jsdbg page playwright --eval "<pending page.evaluate>" --context :playwright-e2e --connection browser --target CA13CA948E4043DCC81C7347F1D37803
jsdbg: page.evaluate: Target page, context or browser has been closed
    at eval (<anonymous>:3:12)
    at main (<worktree>/[eval1]:31:49)
```

Browser-wide operations were rejected, the unrelated page remained untouched, and destroying the selected page cancelled its pending proxy command promptly. The original Playwright owner remained connected.
