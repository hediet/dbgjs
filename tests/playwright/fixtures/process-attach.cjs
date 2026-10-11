const { createServer } = require("node:http");
const { Session } = require("node:inspector");
const { WebSocketServer } = require("ws");

let rejectDebugger = true;
const server = createServer((request, response) => {
	if (request.url === "/allow-debugger") {
		rejectDebugger = false;
		response.end("ok");
		return;
	}
	response.setHeader("content-type", "application/json");
	response.end(JSON.stringify(request.url === "/json/list" ? [{
		type: "node",
		webSocketDebuggerUrl: `ws://127.0.0.1:${server.address().port}/inspector`,
	}] : {}));
});
const sockets = new WebSocketServer({ server });
sockets.on("connection", socket => {
	const session = new Session();
	session.connect();
	session.on("inspectorNotification", message => socket.send(JSON.stringify(message)));
	socket.on("message", data => {
		const { id, method, params } = JSON.parse(data.toString());
		if (method === "Debugger.enable" && rejectDebugger) {
			socket.send(JSON.stringify({ id, error: { code: -32000, message: "fixture attachment denied" } }));
			return;
		}
		session.post(method, params, (error, result) => {
			if (socket.readyState === socket.OPEN) {
				socket.send(JSON.stringify(error
					? { id, error: { code: -32000, message: error.message } }
					: { id, result: result ?? {} }));
			}
		});
	});
	socket.on("close", () => session.disconnect());
});
server.listen(0, "127.0.0.1", () => process.stdout.write(`${server.address().port}\n`));
