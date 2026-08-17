import { defineConfig } from "@playwright/test";

export default defineConfig({
	testDir: "./tests/playwright",
	fullyParallel: false,
	workers: 1,
	timeout: 180_000,
	reporter: "line",
});
