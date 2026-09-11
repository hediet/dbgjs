async (token) => {
	const electronRequire = typeof require === "function"
		? require
		: process?.mainModule?.require?.bind(process.mainModule);
	if (typeof electronRequire !== "function") {
		throw new Error("renderer bridge requires the Electron main-process context");
	}
	const { app, webContents } = electronRequire("electron");
	const net = electronRequire("node:net");
	const registryKey = Symbol.for("hediet.dbgjs.rendererBridge");
	const previous = globalThis[registryKey];
	if (previous?.owner === "dbgjs" && typeof previous.bridge?.dispose === "function") {
		await previous.bridge.dispose("replaced by a new dbgjs bridge");
	}

	const maxMessageBytes = 128 * 1024 * 1024;
	const maxBufferedBytes = 128 * 1024 * 1024;
	// A renderer that is genuinely blocked never answers `Page.waitForDebugger`; a renderer that
	// cannot be blocked rejects it. Anything still pending after this window is really paused.
	const waitForDebuggerGraceMs = 150;
	// Safety net so an armed-but-never-attached renderer cannot stay frozen forever.
	const waitForDebuggerReleaseMs = 30_000;
	const rendererClients = new Map();
	const startupBlocks = new Map();
	const watched = new Set();
	const pendingSockets = new Set();
	const pendingAttachments = new Set();
	let controlSocket;
	let controlTimer;
	let disposed = false;
	let discovering = false;
	let waitForDebuggerOnStart = false;
	let server;
	let bridge;

	const find = (webContentsId) => {
		const contents = webContents.fromId(webContentsId);
		if (!contents || contents.isDestroyed()) {
			throw new Error(`Electron webContents ${webContentsId} does not exist`);
		}
		return contents;
	};
	const descriptor = (contents) => ({
		webContentsId: contents.id,
		processId: safeProcessId(contents),
		hostWebContentsId: safeRelatedWebContentsId(contents, "hostWebContents"),
		openerWebContentsId: safeRelatedWebContentsId(contents, "opener"),
		type: contents.getType(),
		title: safeTitle(contents),
		url: safeUrl(contents),
		waitingForDebugger: startupBlocks.get(contents.id)?.blocked === true,
		attached: isAttached(contents),
	});
	const safeRelatedWebContentsId = (contents, property) => {
		try {
			const related = contents[property];
			return related && !related.isDestroyed() ? related.id : undefined;
		} catch {
			return undefined;
		}
	};
	// A bridge-owned startup block is dbgjs's own pre-attachment, not a foreign debugger.
	const isAttached = (contents) => {
		try {
			return contents.debugger.isAttached() && !startupBlocks.has(contents.id);
		} catch {
			return false;
		}
	};
	const safeProcessId = (contents) => {
		try {
			return contents.getOSProcessId();
		} catch {
			return 0;
		}
	};
	const safeTitle = (contents) => {
		try {
			return contents.getTitle();
		} catch {
			return "";
		}
	};
	const safeUrl = (contents) => {
		try {
			return contents.getURL();
		} catch {
			return "";
		}
	};
	const writeFrame = (socket, value) => {
		if (socket.destroyed || socket.writableEnded) {
			return false;
		}
		const line = `${JSON.stringify(value)}\n`;
		if (Buffer.byteLength(line) > maxMessageBytes) {
			socket.destroy(new Error("renderer bridge message exceeded 128 MiB"));
			return false;
		}
		if (socket.writableLength + Buffer.byteLength(line) > maxBufferedBytes) {
			socket.destroy(new Error("renderer bridge socket exceeded 128 MiB of buffered data"));
			return false;
		}
		return socket.write(line);
	};
	const writeClosed = (socket, reason) => {
		writeFrame(socket, { kind: "closed", reason });
	};
	// Control frames carry an id so the debugger service can wait until discovery or startup
	// blocking is actually armed before it reports success.
	const acknowledge = (socket, frame) => {
		if (Number.isInteger(frame?.id)) {
			writeFrame(socket, { kind: "ack", id: frame.id });
		}
	};
	const publish = (kind, payload) => {
		if (!discovering || !controlSocket || controlSocket.destroyed) {
			return;
		}
		writeFrame(controlSocket, { kind, ...payload });
	};
	const publishTarget = (kind, contents) => {
		publish(kind, { target: descriptor(contents) });
	};
	/// Renderer discovery is event driven: Electron reports every webContents through its own
	/// lifecycle events, so the bridge never polls the operating system for renderer processes.
	const watch = (contents) => {
		if (watched.has(contents.id) || contents.isDestroyed()) {
			return;
		}
		watched.add(contents.id);
		const changed = () => publishTarget("targetInfoChanged", contents);
		for (const event of ["did-navigate", "did-navigate-in-page", "did-finish-load", "did-stop-loading"]) {
			contents.on(event, changed);
		}
		contents.on("page-title-updated", changed);
		contents.once("destroyed", () => {
			watched.delete(contents.id);
			startupBlocks.delete(contents.id);
			publish("targetDestroyed", { webContentsId: contents.id });
		});
	};
	/// Blocks the very first script execution of a freshly created webContents. `Page.waitForDebugger`
	/// only settles once the renderer is resumed, so a command that is still pending after the grace
	/// window is proof that startup is genuinely paused.
	const blockStartup = async (contents) => {
		const entry = { contents, blocked: false, released: false, timer: undefined };
		startupBlocks.set(contents.id, entry);
		let attached = false;
		try {
			if (!contents.debugger.isAttached()) {
				contents.debugger.attach("1.3");
				attached = true;
			}
		} catch (error) {
			startupBlocks.delete(contents.id);
			return { blocked: false, reason: String(error?.message || error) };
		}
		entry.attachedByBridge = attached;
		let settled;
		const finish = (reason) => {
			settled = { blocked: false, reason };
			const wasBlocked = entry.blocked;
			entry.blocked = false;
			clearTimeout(entry.timer);
			if (startupBlocks.get(contents.id) === entry) {
				startupBlocks.delete(contents.id);
			}
			if (wasBlocked && !contents.isDestroyed()) {
				publishTarget("targetInfoChanged", contents);
			}
			return settled;
		};
		const pending = contents.debugger.sendCommand("Page.waitForDebugger").then(
			() => finish("resumed"),
			(error) => finish(String(error?.message || error)),
		);
		const outcome = await Promise.race([
			pending,
			delay(waitForDebuggerGraceMs).then(() => undefined),
		]);
		if (outcome) {
			if (entry.attachedByBridge && !rendererClients.has(contents.id)) {
				detachQuietly(contents);
			}
			startupBlocks.delete(contents.id);
			return outcome;
		}
		entry.blocked = true;
		entry.timer = setTimeout(() => {
			if (!rendererClients.has(contents.id)) {
				void releaseStartup(contents.id, true);
			}
		}, waitForDebuggerReleaseMs);
		entry.timer.unref?.();
		return { blocked: true };
	};
	const releaseStartup = async (webContentsId, detach) => {
		const entry = startupBlocks.get(webContentsId);
		if (!entry || entry.released) {
			return;
		}
		entry.released = true;
		startupBlocks.delete(webContentsId);
		clearTimeout(entry.timer);
		if (entry.contents.isDestroyed()) {
			return;
		}
		try {
			await entry.contents.debugger.sendCommand("Runtime.runIfWaitingForDebugger");
		} catch {
			// The renderer already resumed or went away.
		}
		if (detach && entry.attachedByBridge) {
			detachQuietly(entry.contents);
		}
		publishTarget("targetInfoChanged", entry.contents);
	};
	const detachQuietly = (contents) => {
		try {
			if (!contents.isDestroyed() && contents.debugger.isAttached()) {
				contents.debugger.detach();
			}
		} catch {
			// Detaching is best effort.
		}
	};
	const onWebContentsCreated = (contents) => {
		watch(contents);
		if (!waitForDebuggerOnStart) {
			publishTarget("targetCreated", contents);
			return;
		}
		void blockStartup(contents)
			.catch(() => undefined)
			.then(() => publishTarget("targetCreated", contents));
	};
	const onAppWebContentsCreated = (_event, contents) => {
		onWebContentsCreated(contents);
	};
	const delay = (milliseconds) => new Promise((resolve) => {
		const timer = setTimeout(resolve, milliseconds);
		timer.unref?.();
	});
	const removeDebuggerListeners = (entry) => {
		entry.contents.debugger.off("message", entry.onMessage);
		entry.contents.debugger.off("detach", entry.onDetach);
	};
	const releaseRenderer = async (entry, reason, notify = true) => {
		if (entry.releasePromise) {
			return entry.releasePromise;
		}
		entry.releasePromise = (async () => {
			rendererClients.delete(entry.webContentsId);
			removeDebuggerListeners(entry);
			await releaseStartup(entry.webContentsId, false);
			let cleanupError;
			if (!entry.contents.isDestroyed() && entry.contents.debugger.isAttached()) {
				try {
					entry.contents.debugger.detach();
				} catch (error) {
					cleanupError = String(error?.message || error);
				}
			}
			if (notify) {
				writeClosed(
					entry.socket,
					cleanupError ? `${reason}; debugger cleanup failed: ${cleanupError}` : reason,
				);
			}
			entry.socket.end();
		})();
		return entry.releasePromise;
	};
	const routeDebuggerMessage = (entry, method, params, sessionId) => {
		const envelope = { method, params: params ?? {} };
		if (sessionId) {
			envelope.sessionId = sessionId;
		}
		writeFrame(entry.socket, { kind: "cdp", envelope });
	};
	const sendRendererCommand = async (entry, envelope) => {
		try {
			const result = await entry.contents.debugger.sendCommand(
				envelope.method,
				envelope.params ?? {},
				envelope.sessionId,
			);
			if (!Object.hasOwn(envelope, "id")) {
				return;
			}
			const response = { id: envelope.id, result: result ?? {} };
			if (envelope.sessionId) {
				response.sessionId = envelope.sessionId;
			}
			writeFrame(entry.socket, { kind: "cdp", envelope: response });
		} catch (error) {
			if (!Object.hasOwn(envelope, "id")) {
				await releaseRenderer(
					entry,
					`renderer CDP notification failed: ${String(error?.message || error)}`,
				);
				return;
			}
			const response = {
				id: envelope.id,
				error: {
					code: Number.isInteger(error?.code) ? error.code : -32000,
					message: String(error?.message || error),
				},
			};
			if (envelope.sessionId) {
				response.sessionId = envelope.sessionId;
			}
			writeFrame(entry.socket, { kind: "cdp", envelope: response });
		}
	};
	const attachRenderer = async (socket, webContentsId, force) => {
		if (disposed) {
			throw new Error("renderer bridge is disposed");
		}
		const previousClient = rendererClients.get(webContentsId);
		if (previousClient && !force) {
			throw new Error(`Electron webContents ${webContentsId} already has a dbgjs client`);
		}
		let stolen = false;
		if (previousClient) {
			await releaseRenderer(previousClient, "renderer debugger ownership was stolen by --force");
			stolen = true;
		}
		const contents = find(webContentsId);
		const startup = startupBlocks.get(webContentsId);
		const externalOwner = !startup && contents.debugger.isAttached();
		if (externalOwner && !force) {
			throw new Error(
				`Electron webContents ${webContentsId} is already attached by another debugger`,
			);
		}
		if (externalOwner) {
			contents.debugger.detach();
			stolen = true;
		}
		const entry = {
			webContentsId,
			contents,
			socket,
			adoptedStartup: Boolean(startup),
			releasePromise: undefined,
			onMessage: undefined,
			onDetach: undefined,
		};
		entry.onMessage = (_event, method, params, sessionId) => {
			routeDebuggerMessage(entry, method, params, sessionId);
		};
		entry.onDetach = (_event, reason) => {
			void releaseRenderer(
				entry,
				reason || "Electron renderer debugger detached",
			);
		};
		contents.debugger.on("message", entry.onMessage);
		contents.debugger.on("detach", entry.onDetach);
		try {
			if (!entry.adoptedStartup) {
				contents.debugger.attach("1.3");
			}
			rendererClients.set(webContentsId, entry);
			if (disposed || socket.destroyed) {
				await releaseRenderer(
					entry,
					disposed ? "renderer bridge was disposed during attachment" : "renderer socket closed during attachment",
					false,
				);
				throw new Error(
					disposed ? "renderer bridge was disposed during attachment" : "renderer socket closed during attachment",
				);
			}
			return { entry, stolen };
		} catch (error) {
			removeDebuggerListeners(entry);
			if (!entry.adoptedStartup && !contents.isDestroyed() && contents.debugger.isAttached()) {
				contents.debugger.detach();
			}
			throw error;
		}
	};
	const processRendererFrame = async (entry, frame) => {
		if (frame?.kind === "cdp" && frame.envelope) {
			await sendRendererCommand(entry, frame.envelope);
			return;
		}
		if (frame?.kind === "close") {
			await releaseRenderer(entry, "Electron renderer transport closed by debugger service");
			return;
		}
		throw new Error("invalid renderer bridge client frame");
	};
	const readLines = (socket, onLine) => {
		let buffer = "";
		socket.setEncoding("utf8");
		socket.on("data", (chunk) => {
			buffer += chunk;
			if (Buffer.byteLength(buffer) > maxMessageBytes) {
				socket.destroy(new Error("renderer bridge message exceeded 128 MiB"));
				return;
			}
			for (;;) {
				const newline = buffer.indexOf("\n");
				if (newline < 0) {
					break;
				}
				const line = buffer.slice(0, newline);
				buffer = buffer.slice(newline + 1);
				if (!line) {
					continue;
				}
				try {
					onLine(JSON.parse(line));
				} catch (error) {
					socket.destroy(error);
					return;
				}
			}
		});
	};
	const acceptSocket = (socket) => {
		pendingSockets.add(socket);
		socket.setNoDelay(true);
		let authenticated = false;
		let rendererEntry;
		readLines(socket, (frame) => {
			if (!authenticated) {
				authenticated = true;
				void (async () => {
					if (frame?.token !== token) {
						throw new Error("renderer bridge authentication failed");
					}
					if (frame.role === "control") {
						if (controlSocket && !controlSocket.destroyed) {
							throw new Error("renderer bridge already has a control client");
						}
						controlSocket = socket;
						pendingSockets.delete(socket);
						clearTimeout(controlTimer);
						writeFrame(socket, { ready: true });
						return;
					}
					if (frame.role === "renderer" && Number.isInteger(frame.webContentsId)) {
						const attachment = attachRenderer(socket, frame.webContentsId, frame.force === true);
						pendingAttachments.add(attachment);
						try {
							const result = await attachment;
							rendererEntry = result.entry;
							writeFrame(socket, { ready: true, stolen: result.stolen });
						} finally {
							pendingAttachments.delete(attachment);
						}
						pendingSockets.delete(socket);
						return;
					}
					throw new Error("invalid renderer bridge handshake");
				})().catch((error) => {
					writeFrame(socket, {
						ready: false,
						error: String(error?.message || error),
					});
					socket.end();
				});
				return;
			}
			if (socket === controlSocket) {
				if (frame?.kind === "dispose") {
					void bridge.dispose("disposed by debugger service", socket);
					return;
				}
				if (frame?.kind === "setDiscovery") {
					discovering = frame.enabled === true;
					acknowledge(socket, frame);
					return;
				}
				if (frame?.kind === "setWaitForDebuggerOnStart") {
					bridge.setWaitForDebuggerOnStart(frame.enabled === true);
					acknowledge(socket, frame);
					return;
				}
				socket.destroy(new Error("invalid renderer bridge control frame"));
				return;
			}
			if (!rendererEntry) {
				socket.destroy(new Error("renderer bridge handshake is still pending"));
				return;
			}
			void processRendererFrame(rendererEntry, frame).catch((error) => {
				void releaseRenderer(rendererEntry, String(error?.message || error));
			});
		});
		socket.once("close", () => {
			pendingSockets.delete(socket);
			if (socket === controlSocket) {
				controlSocket = undefined;
				void bridge.dispose("renderer bridge control socket closed");
			} else if (rendererEntry) {
				void releaseRenderer(rendererEntry, "renderer bridge socket closed", false);
			}
		});
		socket.on("error", () => {
			// The close handler owns cleanup.
		});
	};

	server = net.createServer(acceptSocket);
	await new Promise((resolve, reject) => {
		const onError = (error) => {
			server.off("listening", onListening);
			reject(error);
		};
		const onListening = () => {
			server.off("error", onError);
			resolve();
		};
		server.once("error", onError);
		server.once("listening", onListening);
		server.listen({ host: "127.0.0.1", port: 0, exclusive: true });
	});

	bridge = {
		endpoint() {
			const address = server.address();
			if (!address || typeof address === "string") {
				throw new Error("renderer bridge server has no TCP address");
			}
			return { port: address.port };
		},
		list() {
			return webContents.getAllWebContents()
				.filter((contents) => !contents.isDestroyed())
				.filter((contents) => safeProcessId(contents) > 0 || startupBlocks.has(contents.id))
				.map(descriptor);
		},
		setWaitForDebuggerOnStart(enabled) {
			waitForDebuggerOnStart = enabled === true;
			if (!waitForDebuggerOnStart) {
				for (const webContentsId of [...startupBlocks.keys()]) {
					if (!rendererClients.has(webContentsId)) {
						void releaseStartup(webContentsId, true);
					}
				}
			}
			return { waitForDebuggerOnStart };
		},
		async dispose(reason = "renderer bridge disposed", acknowledgeSocket) {
			if (disposed) {
				return;
			}
			disposed = true;
			waitForDebuggerOnStart = false;
			app.off("web-contents-created", onAppWebContentsCreated);
			clearTimeout(controlTimer);
			const serverClosed = new Promise((resolve) => server.close(resolve));
			const releases = Promise.all(
				[...rendererClients.values()].map((entry) => releaseRenderer(entry, reason)),
			);
			const resumed = Promise.all(
				[...startupBlocks.keys()].map((webContentsId) => releaseStartup(webContentsId, true)),
			);
			for (const socket of pendingSockets) {
				socket.destroy();
			}
			await Promise.allSettled([...pendingAttachments]);
			await releases;
			await resumed;
			if (acknowledgeSocket && !acknowledgeSocket.destroyed) {
				writeFrame(acknowledgeSocket, { kind: "disposed" });
				acknowledgeSocket.end();
			}
			if (controlSocket && controlSocket !== acknowledgeSocket) {
				controlSocket.destroy();
			}
			await serverClosed;
			if (globalThis[registryKey]?.bridge === this) {
				delete globalThis[registryKey];
			}
		},
	};
	for (const contents of webContents.getAllWebContents()) {
		watch(contents);
	}
	app.on("web-contents-created", onAppWebContentsCreated);
	globalThis[registryKey] = { owner: "dbgjs", token, bridge };
	controlTimer = setTimeout(() => {
		if (!controlSocket) {
			void bridge.dispose("renderer bridge control handshake timed out");
		}
	}, 10_000);
	controlTimer.unref?.();
	return bridge;
}
