import { mkdtemp, rm } from "node:fs/promises";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { chromium, expect, test } from "@playwright/test";
import {
	allocatePort,
	readCdpEndpoint,
	run,
} from "./live-test-harness.mjs";

test("CLI spawns its HubRPC service and connects it to Chromium", async () => {
	test.setTimeout(600_000);
	const userDataDir = await mkdtemp(join(tmpdir(), "cdp-cli-service-"));
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
		await context.pages()[0].goto("data:text/html,<title>Daemon View</title>");
		const endpoint = await readCdpEndpoint(debuggingPort);
		const result = await run(
			"cargo",
			[
				"test",
				"--test",
				"cli_service",
				"cli_service_connects_to_live_cdp",
				"--",
				"--exact",
				"--nocapture",
			],
			{ CDP_WS_ENDPOINT: endpoint },
		);
		expect(result.code, result.output).toBe(0);
		expect(result.output).toContain("1 passed");
	} finally {
		await context.close();
		await rm(userDataDir, { recursive: true, force: true });
	}
});
