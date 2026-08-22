import { spawn } from "node:child_process";

spawn(process.execPath, [new URL("./node-child.js", import.meta.url).pathname], {
	stdio: "inherit",
});

setInterval(() => {
	globalThis.counter = (globalThis.counter ?? 0) + 1;
}, 100);
