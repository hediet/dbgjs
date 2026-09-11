import { HubRpcConnection, type IMessageTransport, type JsonRpcMessage } from "@vscode/hubrpc";
import { createConnection, type Socket } from "node:net";

export interface LegacyHubConnection {
	readonly connection: HubRpcConnection<undefined>;
	readonly onClose: (listener: () => void) => { dispose(): void };
	close(): void;
}

export async function connectLegacyHub(
	endpoint: string,
	token: string,
	log?: (message: string) => void,
): Promise<LegacyHubConnection> {
	log?.(`Connecting to daemon transport ${endpoint}`);
	const socket = createConnection(endpoint);
	await new Promise<void>((resolve, reject) => {
		socket.once("connect", resolve);
		socket.once("error", reject);
	});

	log?.("Connected to daemon transport");
	const transport = new LegacyNdjsonTransport(socket, log);
	socket.write(`${JSON.stringify({ hello: 1, token })}\n`);
	const connection = HubRpcConnection.fromTransport(transport);
	return {
		connection,
		onClose: (listener) => transport.onClose(listener),
		close: () => connection.close(),
	};
}

class LegacyNdjsonTransport implements IMessageTransport<JsonRpcMessage, JsonRpcMessage> {
	private listener: ((message: JsonRpcMessage) => void) | undefined;
	private buffer = "";
	private readonly closeListeners = new Set<() => void>();
	private closed = false;

	public constructor(
		private readonly socket: Socket,
		private readonly log?: (message: string) => void,
	) {
		socket.setEncoding("utf8");
		socket.on("data", (chunk: string) => this.accept(chunk));
		socket.on("close", () => this.fireClose());
		socket.on("error", (error) => {
			this.log?.(`Daemon transport error: ${error.message}`);
			this.fireClose();
		});
	}

	public send(message: JsonRpcMessage): void {
		if (this.closed) {
			throw new Error("dbgjs HubRPC transport is closed");
		}
		const line = JSON.stringify(message);
		this.log?.(`extension -> daemon ${line}`);
		this.socket.write(`${line}\n`);
	}

	public setListener(listener: ((message: JsonRpcMessage) => void) | undefined): void {
		this.listener = listener;
	}

	public dispose(): void {
		this.socket.destroy();
		this.fireClose();
	}

	public onClose(listener: () => void): { dispose(): void } {
		if (this.closed) {
			queueMicrotask(listener);
			return { dispose: () => undefined };
		}
		this.closeListeners.add(listener);
		return { dispose: () => this.closeListeners.delete(listener) };
	}

	private accept(chunk: string): void {
		this.buffer += chunk;
		for (;;) {
			const newline = this.buffer.indexOf("\n");
			if (newline < 0) {
				return;
			}
			const line = this.buffer.slice(0, newline).trim();
			this.buffer = this.buffer.slice(newline + 1);
			if (line.length === 0) {
				continue;
			}
			this.log?.(`daemon -> extension ${line}`);
			try {
				const message: unknown = JSON.parse(line);
				if (isJsonRpcMessage(message)) {
					this.listener?.(message);
				} else {
					this.dispose();
				}
			} catch {
				this.dispose();
			}
		}
	}

	private fireClose(): void {
		if (this.closed) {
			return;
		}
		this.closed = true;
		this.log?.("Daemon transport closed");
		for (const listener of this.closeListeners) {
			listener();
		}
		this.closeListeners.clear();
	}
}

function isJsonRpcMessage(value: unknown): value is JsonRpcMessage {
	return typeof value === "object"
		&& value !== null
		&& "jsonrpc" in value
		&& value.jsonrpc === "2.0";
}
