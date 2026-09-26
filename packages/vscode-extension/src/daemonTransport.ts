import { LinkRpcConnection } from "@hediet/linkrpc";
import { connectNdjson } from "@hediet/linkrpc/node";
import { createConnection } from "node:net";

export interface DaemonConnection {
	readonly connection: LinkRpcConnection<undefined>;
	readonly onClose: (listener: () => void) => { dispose(): void };
	close(): void;
}

export async function connectDaemon(
	endpoint: string,
	token: string,
	log?: (message: string) => void,
): Promise<DaemonConnection> {
	log?.(`Connecting to daemon transport ${endpoint}`);
	const socket = createConnection(endpoint);
	const closeListeners = new Set<() => void>();
	let connection: LinkRpcConnection<undefined> | undefined;
	let closed = false;
	let rejectConnecting: ((error: Error) => void) | undefined;

	const close = (): void => {
		if (closed) {
			return;
		}
		closed = true;
		connection?.close();
		socket.destroy();
		log?.("Daemon transport closed");
		try {
			for (const listener of closeListeners) {
				listener();
			}
		} finally {
			closeListeners.clear();
		}
	};

	socket.on("error", (error) => {
		log?.(`Daemon transport error: ${error.message}`);
		rejectConnecting?.(error);
		close();
	});
	socket.on("close", close);

	try {
		await new Promise<void>((resolve, reject) => {
			rejectConnecting = reject;
			socket.once("connect", resolve);
			socket.once("close", () => reject(new Error("Daemon transport closed while connecting")));
		});
		rejectConnecting = undefined;
		log?.("Connected to daemon transport");

		socket.write(`${JSON.stringify({ hello: 1, token })}\n`);
		const { transport } = await connectNdjson({
			input: socket,
			output: socket,
			onClose: close,
			trace: (direction, message) => {
				log?.(`${direction === "send" ? "extension -> daemon" : "daemon -> extension"} ${JSON.stringify(message)}`);
			},
		});
		connection = LinkRpcConnection.fromTransport(transport);
		if (closed) {
			connection.close();
		}
	} catch (error) {
		rejectConnecting = undefined;
		close();
		throw error;
	}

	return {
		connection,
		onClose: (listener) => {
			if (closed) {
				queueMicrotask(listener);
				return { dispose: () => undefined };
			}
			closeListeners.add(listener);
			return { dispose: () => closeListeners.delete(listener) };
		},
		close,
	};
}
