import express from "express";

const app = express();
const products = new Map([
	["notebook", { name: "Notebook", price: 12 }],
	["pencil", { name: "Pencil", price: 2 }],
]);
let ticks = 0;
let requests = 0;

app.get("/health", (_request, response) => response.json({ status: "ready" }));
app.get("/quote/:product", function quote(request, response) {
	const product = products.get(request.params.product);
	const quantity = Number(request.query.quantity ?? 1);
	if (!product) {
		response.status(404).json({ error: "Unknown product" });
		return;
	}
	if (!Number.isInteger(quantity) || quantity < 1) {
		response.status(400).json({ error: "Quantity must be a positive integer" });
		return;
	}
	const subtotal = product.price * quantity;
	const discount = quantity >= 3 ? subtotal * 0.1 : 0;
	requests++;
	response.json({ product: product.name, quantity, subtotal, discount, total: subtotal - discount });
});

const server = app.listen(0, "127.0.0.1", () => {
	console.log(`Express server PID ${process.pid}: http://127.0.0.1:${server.address().port}`);
	process.send?.({ type: "ready", pid: process.pid, port: server.address().port });
});
setInterval(() => { ticks++; }, 50);
process.on("message", (message) => {
	if (message === "probe") {
		process.send?.({ type: "probe", pid: process.pid, ticks, requests });
	}
});
