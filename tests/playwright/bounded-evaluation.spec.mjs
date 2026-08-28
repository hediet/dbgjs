import { mkdtemp, rm } from "node:fs/promises";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { chromium, expect, test } from "@playwright/test";
import {
	allocatePort,
	readCdpEndpoint,
	run,
} from "./live-test-harness.mjs";

test("target evaluation bounds large primitive CDP transfer", async () => {
	test.setTimeout(600_000);
	const userDataDir = await mkdtemp(join(tmpdir(), "cdp-bounded-evaluation-"));
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
		const endpoint = await readCdpEndpoint(debuggingPort);
		const result = await run(
			"cargo",
			[
				"test",
				"--lib",
				"target_debugger::tests::live_evaluation_bounds_large_primitive_transfer",
				"--",
				"--ignored",
				"--exact",
				"--nocapture",
			],
			{ CDP_WS_ENDPOINT: endpoint },
		);
		expect(result.code, result.output).toBe(0);
		expect(result.output).toContain("1 passed");
		expect(result.output).toContain("resultBytes<2048");
	} finally {
		await context.close();
		await rm(userDataDir, { recursive: true, force: true });
	}
});
