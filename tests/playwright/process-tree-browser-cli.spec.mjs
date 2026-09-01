import { test } from "@playwright/test";
import { generateProcessTreeBrowserTranscript } from "./target-tree-cli-transcripts.mjs";

test("process tree composes a descendant browser root and its targets", async () => {
	test.skip(process.platform !== "win32", "process-tree discovery is currently Windows-only");
	test.setTimeout(300_000);

	await generateProcessTreeBrowserTranscript({
		build: process.env.JSDBG_SKIP_BUILD !== "1",
		write: false,
	});
});
