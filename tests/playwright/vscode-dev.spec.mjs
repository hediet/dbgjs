import { mkdtemp, rm } from "node:fs/promises";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { chromium, expect, test } from "@playwright/test";
import {
	allocatePort,
	buildLiveTest,
	readCdpEndpoint,
	run,
} from "./live-test-harness.mjs";

test("Rust reducer hits an authored vscode.dev typing breakpoint", async () => {
	test.setTimeout(600_000);
	const executable = await buildLiveTest();
	const userDataDir = await mkdtemp(join(tmpdir(), "cdp-vscode-dev-"));
	const debuggingPort = await allocatePort();
	const channel = process.env.PLAYWRIGHT_CHANNEL ?? "chrome";
	const context = await chromium.launchPersistentContext(userDataDir, {
		...(channel === "bundled" ? {} : { channel }),
		headless: true,
		args: [
			`--remote-debugging-port=${debuggingPort}`,
			"--remote-allow-origins=*",
			"--no-first-run",
			"--no-default-browser-check",
		],
	});

	try {
		const page = context.pages()[0] ?? (await context.newPage());
		await page.goto("https://vscode.dev/", {
			waitUntil: "domcontentloaded",
			timeout: 120_000,
		});
		await expect(page.locator(".monaco-workbench")).toBeVisible({ timeout: 120_000 });
		const pageSession = await context.newCDPSession(page);
		const { targetInfo } = await pageSession.send("Target.getTargetInfo");
		const endpoint = await readCdpEndpoint(debuggingPort);
		const result = await run(
			executable,
			[
				"reducer_hits_an_authored_vscode_dev_typing_breakpoint",
				"--ignored",
				"--exact",
				"--nocapture",
			],
			{
				CDP_WS_ENDPOINT: endpoint,
				VSCODE_TARGET_ID: targetInfo.targetId,
			},
		);
		expect(result.code, result.output).toBe(0);
		expect(result.output).toContain("1 passed");
	} finally {
		await context.close();
		await rm(userDataDir, { recursive: true, force: true });
	}
});
