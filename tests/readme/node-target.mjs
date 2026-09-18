globalThis.readmeApp = {
	name: "orders",
	orders: [{ id: "first", total: 17 }, { id: "second", total: 25 }],
	ticks: 0,
};

setInterval(() => { readmeApp.ticks++; }, 50);
process.on("message", (message) => {
	if (message === "probe") {
		process.send?.({ type: "probe", pid: process.pid, ticks: readmeApp.ticks });
	}
});
process.send?.({ type: "ready", pid: process.pid });
