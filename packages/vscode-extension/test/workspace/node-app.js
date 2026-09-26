import { spawn } from "node:child_process";
import { fileURLToPath } from "node:url";

spawn(process.execPath, [fileURLToPath(new URL("./node-child.js", import.meta.url))], {
	stdio: "inherit",
});

setInterval(() => {
	const state = {
		counter: (globalThis.counter ?? 0) + 1,
		nested: { label: "watch" },
	};
	globalThis.counter = state.counter;
	debugger;
}, 100);
