setInterval(() => {
	globalThis.counter = (globalThis.counter ?? 0) + 1;
}, 100);
