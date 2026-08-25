setInterval(() => {
	globalThis.childCounter = (globalThis.childCounter ?? 0) + 1;
	debugger;
}, 100);
