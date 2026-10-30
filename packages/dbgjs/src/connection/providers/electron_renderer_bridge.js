async (token) => {
	const electronRequire = typeof require === "function"
		? require
		: process?.mainModule?.require?.bind(process.mainModule);
	if (typeof electronRequire !== "function") {
		throw new Error("renderer bridge requires the Electron main-process context");
	}
	const { app, BrowserWindow, webContents } = electronRequire("electron");
	const net = electronRequire("node:net");
	// Every dbgjs connection to this app installs its own bridge, and each bridge disposes itself
	// when its control socket closes or its handshake times out, so none can outlive its owner.
	// Electron allows one `webContents.debugger` attachment per window, so the bridges share it
	// through one process-wide registry and each client gets its own CDP session to the page.
	const sharedDebuggers = globalThis[Symbol.for("dbgjs.electronSharedDebuggers.v1")] ??= (() => {
		const windows = new Map();
		const detachDebugger = (contents) => {
			try {
				if (!contents.isDestroyed() && contents.debugger.isAttached()) {
					contents.debugger.detach();
				}
			} catch {
				// Detaching is best effort.
			}
		};
		const forget = (window) => {
			if (windows.get(window.contents.id) !== window) {
				return false;
			}
			windows.delete(window.contents.id);
			window.contents.debugger.off("message", window.onMessage);
			window.contents.debugger.off("detach", window.onDetach);
			return true;
		};
		const closeSession = (window, session, reason) => {
			if (session.closed) {
				return false;
			}
			session.closed = true;
			window.sessions.delete(session);
			for (const [sessionId, owner] of window.owners) {
				if (owner === session) {
					window.owners.delete(sessionId);
				}
			}
			session.listener.onDetach(reason);
			return true;
		};
		const attachShared = (contents, force) => {
			const existing = windows.get(contents.id);
			if (existing) {
				return { window: existing, stolen: false };
			}
			let stolen = false;
			if (contents.debugger.isAttached()) {
				if (!force) {
					throw new Error(`Electron webContents ${contents.id} is already attached by another debugger`);
				}
				contents.debugger.detach();
				stolen = true;
			}
			contents.debugger.attach("1.3");
			const window = { contents, holds: 0, sessions: new Set(), owners: new Map() };
			window.onMessage = (_event, method, params, sessionId) => {
				if (!sessionId) {
					// The shared root session only opens client sessions; it learns here when one ends.
					const ended = method === "Target.detachedFromTarget"
						&& [...window.sessions].find((session) => session.rootSessionId === params?.sessionId);
					if (ended && closeSession(window, ended, "Electron renderer session detached")) {
						release(window);
					}
					return;
				}
				const owner = window.owners.get(sessionId);
				if (!owner) {
					return;
				}
				if (method === "Target.attachedToTarget" && params?.sessionId) {
					window.owners.set(params.sessionId, owner);
				} else if (method === "Target.detachedFromTarget" && params?.sessionId) {
					window.owners.delete(params.sessionId);
				}
				owner.listener.onMessage(method, params, sessionId === owner.rootSessionId ? undefined : sessionId);
			};
			window.onDetach = (_event, reason) => {
				if (!forget(window)) {
					return;
				}
				window.holds = 0;
				for (const session of [...window.sessions]) {
					closeSession(window, session, reason || "Electron renderer debugger detached");
				}
			};
			contents.debugger.on("message", window.onMessage);
			contents.debugger.on("detach", window.onDetach);
			windows.set(contents.id, window);
			return { window, stolen };
		};
		const release = (window) => {
			if (windows.get(window.contents.id) === window && --window.holds === 0) {
				forget(window);
				detachDebugger(window.contents);
			}
		};
		return {
			/// Keeps the window's shared debugger attached and sends commands to its root session.
			hold(contents, force) {
				const { window, stolen } = attachShared(contents, force);
				window.holds++;
				let released = false;
				return {
					stolen,
					send: (method, params) => contents.debugger.sendCommand(method, params ?? {}),
					release() {
						if (!released) {
							released = true;
							release(window);
						}
					},
				};
			},
			/// Opens a client's own flattened session to the window. Commands without a session id go
			/// to it, and its events arrive without one, as if the client owned the debugger.
			async open(contents, force, listener) {
				const { window, stolen } = attachShared(contents, force);
				window.holds++;
				try {
					const { targetInfo } = await contents.debugger.sendCommand("Target.getTargetInfo");
					const { sessionId } = await contents.debugger.sendCommand(
						"Target.attachToTarget", { targetId: targetInfo.targetId, flatten: true },
					);
					if (windows.get(contents.id) !== window) {
						throw new Error("Electron renderer debugger detached during attachment");
					}
					const session = { rootSessionId: sessionId, listener, closed: false };
					window.sessions.add(session);
					window.owners.set(sessionId, session);
					return {
						stolen,
						async send(method, params, targetSessionId) {
							if (session.closed) {
								throw new Error("Electron renderer session is closed");
							}
							const result = await contents.debugger.sendCommand(
								method, params ?? {}, targetSessionId ?? sessionId,
							);
							if (method === "Target.attachToTarget" && result?.sessionId && !session.closed) {
								window.owners.set(result.sessionId, session);
							}
							return result;
						},
						async close() {
							if (session.closed) {
								return;
							}
							session.closed = true;
							window.sessions.delete(session);
							for (const [id, owner] of window.owners) {
								if (owner === session) {
									window.owners.delete(id);
								}
							}
							if (windows.get(contents.id) === window) {
								await contents.debugger.sendCommand("Target.detachFromTarget", { sessionId })
									.catch(() => {});
								release(window);
							}
						},
					};
				} catch (error) {
					release(window);
					throw error;
				}
			},
		};
	})();

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
		primaryWindowId: safePrimaryWindowId(contents),
		hostWebContentsId: safeRelatedWebContentsId(contents, "hostWebContents"),
		openerWebContentsId: safeRelatedWebContentsId(contents, "opener"),
		type: contents.getType(),
		title: safeTitle(contents),
		url: safeUrl(contents),
		waitingForDebugger: startupBlocks.get(contents.id)?.blocked === true,
		attached: isAttached(contents),
	});
	const safePrimaryWindowId = (contents) => {
		try {
			const window = BrowserWindow.fromWebContents(contents);
			return window && !window.isDestroyed() && window.webContents === contents
				? window.id : undefined;
		} catch {
			return undefined;
		}
	};
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
	const onAppBrowserWindowCreated = (_event, window) => {
		if (!window.isDestroyed() && !window.webContents.isDestroyed()) {
			publishTarget("targetInfoChanged", window.webContents);
		}
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
		const entry = { contents, blocked: false, released: false, timer: undefined, hold: undefined };
		startupBlocks.set(contents.id, entry);
		try {
			entry.hold = sharedDebuggers.hold(contents, false);
		} catch (error) {
			startupBlocks.delete(contents.id);
			return { blocked: false, reason: String(error?.message || error) };
		}
		let settled;
		const finish = (reason) => {
			settled = { blocked: false, reason };
			const wasBlocked = entry.blocked;
			entry.blocked = false;
			clearTimeout(entry.timer);
			if (startupBlocks.get(contents.id) === entry) {
				startupBlocks.delete(contents.id);
			}
			entry.hold.release();
			if (wasBlocked && !contents.isDestroyed()) {
				publishTarget("targetInfoChanged", contents);
			}
			return settled;
		};
		const pending = entry.hold.send("Page.waitForDebugger").then(
			() => finish("resumed"),
			(error) => finish(String(error?.message || error)),
		);
		const outcome = await Promise.race([
			pending,
			delay(waitForDebuggerGraceMs).then(() => undefined),
		]);
		if (outcome) {
			startupBlocks.delete(contents.id);
			return outcome;
		}
		entry.blocked = true;
		entry.timer = setTimeout(() => {
			if (!rendererClients.has(contents.id)) {
				void releaseStartup(contents.id);
			}
		}, waitForDebuggerReleaseMs);
		entry.timer.unref?.();
		return { blocked: true };
	};
	/// Resumes a renderer paused at startup; its pending `Page.waitForDebugger` then settles and
	/// releases the startup hold on the shared debugger.
	const releaseStartup = async (webContentsId) => {
		const entry = startupBlocks.get(webContentsId);
		if (!entry || entry.released) {
			return;
		}
		entry.released = true;
		startupBlocks.delete(webContentsId);
		clearTimeout(entry.timer);
		if (entry.contents.isDestroyed()) {
			entry.hold.release();
			return;
		}
		try {
			await entry.hold.send("Runtime.runIfWaitingForDebugger");
		} catch {
			// The renderer already resumed or went away.
		}
		publishTarget("targetInfoChanged", entry.contents);
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
	const releaseRenderer = async (entry, reason, notify = true) => {
		if (entry.releasePromise) {
			return entry.releasePromise;
		}
		entry.releasePromise = (async () => {
			if (rendererClients.get(entry.webContentsId) === entry) {
				rendererClients.delete(entry.webContentsId);
			}
			await releaseStartup(entry.webContentsId);
			await entry.session?.close();
			if (notify) {
				writeClosed(entry.socket, reason);
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
	const refreshAutoAttachedChildren = async (entry, parentSessionId) => {
		const key = parentSessionId || "";
		const parent = entry.autoAttachParents.get(key);
		if (!parent || parent.scanning || entry.releasePromise) {
			return;
		}
		parent.scanning = true;
		try {
			const { targetInfo } = await entry.session.send(
				"Target.getTargetInfo", {}, parentSessionId,
			);
			const { targetInfos } = await entry.session.send(
				"Target.getTargets", {}, parentSessionId,
			);
			if (entry.autoAttachParents.get(key) !== parent) {
				return;
			}
			const children = targetInfos.filter((info) =>
				info.type === "iframe" && info.parentId === targetInfo.targetId);
			const live = new Set(children.map((info) => info.targetId));
			for (const [targetId, sessionId] of parent.children) {
				if (live.has(targetId)) {
					continue;
				}
				parent.children.delete(targetId);
				routeDebuggerMessage(entry, "Target.detachedFromTarget",
					{ sessionId, targetId }, parentSessionId);
				try {
					await entry.session.send(
						"Target.detachFromTarget", { sessionId }, parentSessionId,
					);
				} catch {
					// The native target was already destroyed.
				}
			}
			for (const info of children) {
				if (parent.children.has(info.targetId)) {
					continue;
				}
				let sessionId;
				({ sessionId } = await entry.session.send(
					"Target.attachToTarget", { targetId: info.targetId, flatten: true },
					parentSessionId,
				));
				if (entry.autoAttachParents.get(key) !== parent) {
					await entry.session.send(
						"Target.detachFromTarget", { sessionId }, parentSessionId,
					).catch(() => {});
					return;
				}
				if (parent.children.has(info.targetId)) {
					if (parent.children.get(info.targetId) === sessionId) {
						continue;
					}
					await entry.session.send(
						"Target.detachFromTarget", { sessionId }, parentSessionId,
					).catch(() => {});
					continue;
				}
				parent.children.set(info.targetId, sessionId);
				routeDebuggerMessage(entry, "Target.attachedToTarget",
					{ sessionId, targetInfo: { ...info, attached: true }, waitingForDebugger: false },
					parentSessionId);
			}
		} finally {
			parent.scanning = false;
			if (parent.rescan) {
				parent.rescan = false;
				scheduleAutoAttachRefresh(entry, parentSessionId);
			}
		}
	};
	const scheduleAutoAttachRefresh = (entry, parentSessionId) => {
		const parent = entry.autoAttachParents.get(parentSessionId || "");
		if (!parent || entry.releasePromise) {
			return;
		}
		if (parent.scanning) {
			parent.rescan = true;
		} else if (!parent.timer) {
			parent.timer = setTimeout(() => {
				parent.timer = undefined;
				void refreshAutoAttachedChildren(entry, parentSessionId).catch(() => {});
			}, 20);
			parent.timer.unref?.();
		}
	};
	const addRemoteFrameTrees = async (entry, parentSessionId, result) => {
		const parent = entry.autoAttachParents.get(parentSessionId || "");
		if (!parent || !result?.frameTree) {
			return;
		}
		for (let attempt = 0; attempt < 50 && parent.scanning; attempt++) {
			await delay(10);
		}
		await refreshAutoAttachedChildren(entry, parentSessionId);
		const { targetInfos } = await entry.session.send(
			"Target.getTargets", {}, parentSessionId,
		);
		const findFrame = (tree, id) => {
			if (tree.frame.id === id) return tree;
			for (const child of tree.childFrames ?? []) {
				const found = findFrame(child, id);
				if (found) return found;
			}
			return undefined;
		};
		for (const info of targetInfos) {
			const sessionId = parent.children.get(info.targetId);
			if (!sessionId || info.type !== "iframe") continue;
			const parentTree = findFrame(result.frameTree, info.parentId);
			if (!parentTree || findFrame(result.frameTree, info.targetId)) continue;
			let childTree;
			try {
				({ frameTree: childTree } = await entry.session.send(
					"Page.getFrameTree", {}, sessionId,
				));
			} catch {
				continue;
			}
			if (!childTree?.frame || childTree.frame.id !== info.targetId) continue;
			childTree.frame.parentId = info.parentId;
			if (!childTree.frame.url && info.url) {
				childTree.frame.url = info.url;
			}
			// The child session owns its local descendants; projecting them here would race
			// Playwright's OOPIF attachment and create duplicate frame identities.
			(parentTree.childFrames ??= []).push({ frame: childTree.frame });
		}
	};
	const sendRendererCommand = async (entry, envelope) => {
		try {
			if (envelope.method === "Target.setAutoAttach" && envelope.params?.autoAttach) {
				const key = envelope.sessionId || "";
				if (!entry.autoAttachParents.has(key)) {
					entry.autoAttachParents.set(key, {
						children: new Map(),
						scanning: false, rescan: false, timer: undefined,
					});
				}
			}
			if (envelope.method === "Target.attachToTarget") {
				entry.rawAttaching.add(envelope.params?.targetId);
			}
			let result;
			try {
				result = await entry.session.send(
					envelope.method,
					envelope.params ?? {},
					envelope.sessionId,
				);
			} finally {
				if (envelope.method === "Target.attachToTarget") {
					entry.rawAttaching.delete(envelope.params?.targetId);
				}
			}
			// A startup pause waits in the shared root session, not in the client's own session.
			if (envelope.method === "Runtime.runIfWaitingForDebugger" && !envelope.sessionId) {
				await releaseStartup(entry.webContentsId);
			}
			if (envelope.method === "Page.getFrameTree") {
				await addRemoteFrameTrees(entry, envelope.sessionId, result);
				if (envelope.sessionId && result?.frameTree) {
					let announced = entry.announcedFrames.get(envelope.sessionId);
					if (!announced) {
						announced = new Set();
						entry.announcedFrames.set(envelope.sessionId, announced);
					}
					const main = result.frameTree.frame;
					if (main?.url && !announced.has(main.id)) {
						// Playwright ignores a child session's getFrameTree result; replay its
						// main-frame navigation before announcing local descendants.
						announced.add(main.id);
						routeDebuggerMessage(entry, "Page.frameNavigated",
							{ frame: main }, envelope.sessionId);
					}
					const announceChildren = (tree) => {
						for (const child of tree.childFrames ?? []) {
							if (!announced.has(child.frame.id)) {
								announced.add(child.frame.id);
								routeDebuggerMessage(entry, "Page.frameAttached",
									{ frameId: child.frame.id, parentFrameId: tree.frame.id },
									envelope.sessionId);
								routeDebuggerMessage(entry, "Page.frameNavigated",
									{ frame: child.frame }, envelope.sessionId);
							}
							announceChildren(child);
						}
					};
					announceChildren(result.frameTree);
				}
			}
			if (envelope.method === "Page.createIsolatedWorld" && envelope.sessionId) {
				const { frameTree } = await entry.session.send(
					"Page.getFrameTree", {}, envelope.sessionId,
				);
				const createChildWorlds = async (tree) => {
					for (const child of tree.childFrames ?? []) {
						// Existing local frames need the utility world too: Playwright
						// creates it explicitly only for the OOPIF's main frame.
						await entry.session.send("Page.createIsolatedWorld",
							{ ...envelope.params, frameId: child.frame.id }, envelope.sessionId)
							.catch(() => {});
						await createChildWorlds(child);
					}
				};
				await createChildWorlds(frameTree);
			}
			if (envelope.method === "Target.setAutoAttach") {
				const key = envelope.sessionId || "";
				if (envelope.params?.autoAttach) {
					scheduleAutoAttachRefresh(entry, envelope.sessionId);
				} else {
					const parent = entry.autoAttachParents.get(key);
					entry.autoAttachParents.delete(key);
					if (parent?.timer) clearTimeout(parent.timer);
					for (const sessionId of parent?.children.values() ?? []) {
						await entry.session.send(
							"Target.detachFromTarget", { sessionId }, envelope.sessionId,
						).catch(() => {});
					}
				}
			}
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
		const entry = {
			webContentsId,
			contents,
			socket,
			session: undefined,
			autoAttachParents: new Map(),
			announcedFrames: new Map(),
			rawAttaching: new Set(),
			releasePromise: undefined,
		};
		const onMessage = (method, params, sessionId) => {
			if (method === "Target.attachedToTarget" && params?.targetInfo?.type === "iframe") {
				const parent = entry.autoAttachParents.get(sessionId || "");
				const targetId = params.targetInfo.targetId;
				if (parent && !entry.rawAttaching.has(targetId)) {
					const existing = parent.children.get(targetId);
					if (existing && existing !== params.sessionId) {
						void entry.session?.send("Target.detachFromTarget",
							{ sessionId: params.sessionId }, sessionId).catch(() => {});
						return;
					}
					parent.children.set(targetId, params.sessionId);
				}
			}
			if (method === "Target.detachedFromTarget") {
				entry.announcedFrames.delete(params?.sessionId);
				for (const parent of entry.autoAttachParents.values()) {
					for (const [id, childSessionId] of parent.children) {
						if (childSessionId === params?.sessionId) parent.children.delete(id);
					}
				}
				scheduleAutoAttachRefresh(entry, sessionId);
			}
			if (method === "Target.targetCreated" || method === "Target.targetDestroyed"
				|| method === "Page.frameAttached" || method === "Page.frameDetached"
				|| method === "Page.frameNavigated") {
				scheduleAutoAttachRefresh(entry, sessionId);
			}
			if (method === "Page.frameDetached") {
				entry.announcedFrames.get(sessionId)?.delete(params?.frameId);
			}
			routeDebuggerMessage(entry, method, params, sessionId);
		};
		const onDetach = (reason) => {
			void releaseRenderer(entry, reason);
		};
		// Registered before the session opens, so a concurrent attach of this bridge sees the client.
		rendererClients.set(webContentsId, entry);
		try {
			const opened = await sharedDebuggers.open(contents, force, { onMessage, onDetach });
			entry.session = opened;
			stolen ||= opened.stolen;
		} catch (error) {
			if (rendererClients.get(webContentsId) === entry) {
				rendererClients.delete(webContentsId);
			}
			throw error;
		}
		if (disposed || socket.destroyed || entry.releasePromise) {
			const reason = disposed
				? "renderer bridge was disposed during attachment"
				: "renderer socket closed during attachment";
			await entry.session.close();
			await releaseRenderer(entry, reason, false);
			throw new Error(reason);
		}
		return { entry, stolen };
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
		resolveBrowserTargets(targetIds) {
			return Object.fromEntries(targetIds.flatMap((targetId) => {
				const contents = webContents.fromDevToolsTargetId(targetId);
				return contents && !contents.isDestroyed() ? [[targetId, contents.id]] : [];
			}));
		},
		setWaitForDebuggerOnStart(enabled) {
			waitForDebuggerOnStart = enabled === true;
			if (!waitForDebuggerOnStart) {
				for (const webContentsId of [...startupBlocks.keys()]) {
					if (!rendererClients.has(webContentsId)) {
						void releaseStartup(webContentsId);
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
			app.off("browser-window-created", onAppBrowserWindowCreated);
			clearTimeout(controlTimer);
			const serverClosed = new Promise((resolve) => server.close(resolve));
			const releases = Promise.all(
				[...rendererClients.values()].map((entry) => releaseRenderer(entry, reason)),
			);
			const resumed = Promise.all(
				[...startupBlocks.keys()].map((webContentsId) => releaseStartup(webContentsId)),
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
		},
	};
	for (const contents of webContents.getAllWebContents()) {
		watch(contents);
	}
	app.on("web-contents-created", onAppWebContentsCreated);
	app.on("browser-window-created", onAppBrowserWindowCreated);
	controlTimer = setTimeout(() => {
		if (!controlSocket) {
			void bridge.dispose("renderer bridge control handshake timed out");
		}
	}, 10_000);
	controlTimer.unref?.();
	return bridge;
}
