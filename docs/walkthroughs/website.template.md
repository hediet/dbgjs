# Launch and automate a website

These commands were recorded against a real local HTTP website. The URLs,
installation paths, and process identities are the actual values from that
run; use your application's URL and your installed browser when following
along. Commands use PowerShell quoting.

## Launch with Playwright

Create a context, then let dbgjs launch Playwright's Chromium and select the page.

{{example:web-context,web-playwright-connect}}

Interact through real page input and inspect the result:

{{example:web-playwright-input}}

Disconnect the owned browser when finished.

{{example:web-playwright-disconnect}}

## Launch installed Chrome

Start a fresh context and point dbgjs at an installed Chrome executable.
This is an independent example, not a second connection added to the Playwright
context above.

{{example:web-chrome-context,web-chrome-connect}}

The selected page supports the same evaluation, debugger, coverage, screenshot,
and Playwright commands as other page targets.

{{example:web-chrome-eval}}

{{example:web-chrome-disconnect}}

The generator supplies the [local website fixture](../../tests/readme/website.html)
so this flow does not depend on the contents of a third-party website.

[Back to the feature overview](../../README.md) ·
[Generation and replay rules](../development/readme-generation.md)
