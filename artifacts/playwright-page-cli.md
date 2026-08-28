# Playwright against a selected `jsdbg` page

This is a real Chromium/Playwright E2E run. The daemon control plane uses authenticated local IPC; Playwright receives a one-shot, capability-URL WebSocket bound only to loopback.

```console
$ jsdbg context create --context :playwright-e2e
Context playwright-e2e  rev 1
  Name: playwright-e2e
  Connections: none
```

```console
$ jsdbg connection add ws://127.0.0.1:46715/devtools/browser/448317aa-0e54-4d3e-a01f-fad1a6fb4d0a --context :playwright-e2e --connection browser --connect
Context playwright-e2e  rev 4
  Name: playwright-e2e
  Connections:
    browser  [connected to HeadlessChrome/151.0.7922.34; CDP 1.3; generation 1]
      Configuration: direct CDP at ws://127.0.0.1:46715/devtools/browser/448317aa-0e54-4d3e-a01f-fad1a6fb4d0a
      Targets:
        page  jsdbg Playwright E2E  http://127.0.0.1:46003/  [a CDP client is attached]
```

```console
$ jsdbg page playwright --eval "await page.mouse.wheel(0, 800)" --context :playwright-e2e --connection browser --target 6EF898A6C327DC85089F705396C58CC6

```

```console
$ jsdbg page playwright --eval "return { title: await page.title(), scrollY: await page.evaluate(() => scrollY) };" --context :playwright-e2e --connection browser --target 6EF898A6C327DC85089F705396C58CC6
{
  "title": "jsdbg Playwright E2E",
  "scrollY": 800
}
```

```console
$ jsdbg page playwright - --context :playwright-e2e --connection browser --target 6EF898A6C327DC85089F705396C58CC6 <<'JS'
return { url: page.url(), marker: await page.locator("#marker").textContent() };
JS
{
  "url": "http://127.0.0.1:46003/",
  "marker": "selected page"
}
```

The original Playwright owner remained connected after all three one-shot proxy sessions.
