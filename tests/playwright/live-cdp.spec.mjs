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

test("generated Rust client hits a real JavaScript breakpoint", async () => {
	test.setTimeout(600_000); // Includes a possible cold Rust/codegen build; runtime has its own 30s deadline.
	const executable = await buildLiveTest();
	const userDataDir = await mkdtemp(join(tmpdir(), "cdp-playwright-"));
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
			executable,
			[
				"generated_client_hits_a_real_breakpoint_in_playwright_chromium",
				"--ignored",
				"--exact",
				"--nocapture",
			],
			{ CDP_WS_ENDPOINT: endpoint },
		);
		expect(result.code, result.output).toBe(0);
		expect(result.output).toContain("1 passed");
		const heapSnapshot = await run(
			executable,
			[
				"heap_snapshot_streams_to_a_devtools_compatible_file",
				"--ignored",
				"--exact",
				"--nocapture",
			],
			{ CDP_WS_ENDPOINT: endpoint },
		);
		expect(heapSnapshot.code, heapSnapshot.output).toBe(0);
		expect(heapSnapshot.output).toContain("1 passed");
	} finally {
		await context.close();
		await rm(userDataDir, { recursive: true, force: true });
	}
});
