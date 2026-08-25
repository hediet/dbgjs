export interface ContextSummary {
	readonly agentInstanceId: string;
	readonly id: string;
	readonly displayName: string;
	readonly revision: number;
	readonly connectionCount: number;
	readonly breakpointCount: number;
}

export interface ContextSnapshot {
	readonly agentInstanceId: string;
	readonly id: string;
	readonly displayName: string;
	readonly revision: number;
	readonly connections: readonly ConnectionSnapshot[];
	readonly targetForest: readonly TargetNodeSnapshot[];
	readonly breakpoints: readonly BreakpointSnapshot[];
}

export interface ConnectionSnapshot {
	readonly id: string;
	readonly configuration: ConnectionConfiguration;
	readonly generation: number;
	readonly status: TaggedValue;
	readonly targets: readonly TargetSnapshot[];
}

export type ConnectionConfiguration =
	| { readonly kind: "directCdp"; readonly endpoint: string; }
	| { readonly kind: "nodeInspector"; readonly endpoint: string; }
	| { readonly kind: "process"; readonly processId: number; }
	| { readonly kind: "processTree"; readonly rootPid: number; }
	| {
			readonly kind: "playwright";
			readonly url: string;
			readonly playwrightPackage?: string;
			readonly channel: PlaywrightChannel;
			readonly headless: boolean;
			readonly ignoreHttpsErrors: boolean;
	  }
	| {
			readonly kind: "chrome";
			readonly url: string;
			readonly executable: string;
			readonly headless: boolean;
			readonly userDataDir?: string;
			readonly args: readonly string[];
	  }
	| {
			readonly kind: "node";
			readonly program: string;
			readonly args: readonly string[];
			readonly cwd: string;
			readonly runtimeExecutable: string;
			readonly runtimeArgs: readonly string[];
			readonly env: Readonly<Record<string, string>>;
	  };

export type PlaywrightChannel =
	| "bundled"
	| "chrome"
	| "chromeBeta"
	| "chromeDev"
	| "chromeCanary"
	| "msedge"
	| "msedgeBeta"
	| "msedgeDev"
	| "msedgeCanary";

export interface TargetSnapshot {
	readonly targetId: string;
	readonly targetType: string;
	readonly title: string;
	readonly url: string;
	readonly attached: boolean;
	readonly parentId?: string;
	readonly openerId?: string;
	readonly browserContextId?: string;
	readonly subtype?: string;
}

export interface TargetNodeSnapshot {
	readonly connectionId: string;
	readonly connectionGeneration: number;
	readonly target: TargetSnapshot;
	readonly parentTargetId?: string;
}

export interface BreakpointSnapshot {
	readonly id: string;
	readonly sourcePath: string;
	readonly line: number;
	readonly column: number;
	readonly status: TaggedValue;
	readonly enabled: boolean;
	readonly condition?: string;
	readonly targetSelector?: string;
}

export interface SourceSnapshotInfo {
	readonly path: string;
	readonly kind: string;
	readonly status: string;
	readonly connectionId?: string;
	readonly targetId?: string;
	readonly sourceMapUrl?: string;
}

export interface SourceContentSnapshot {
	readonly path: string;
	readonly content: string;
}

export interface SourceLocation {
	readonly sourceUrl: string;
	readonly line: number;
	readonly column: number;
}

export type FrameProjectionSnapshot =
	| { readonly kind: "raw" | "pending"; }
	| { readonly kind: "resolved"; readonly location: SourceLocation; }
	| { readonly kind: "failed"; readonly message: string; };

export interface FrameSnapshot {
	readonly index: number;
	readonly functionName: string;
	readonly raw: SourceLocation;
	readonly projected: FrameProjectionSnapshot;
	readonly scopes: readonly ScopeSnapshot[];
	readonly breadcrumb?: string;
}

export interface ScopeSnapshot {
	readonly index: number;
	readonly kind: string;
	readonly name?: string;
}

export interface PauseSnapshot {
	readonly epoch: number;
	readonly reason: string;
	readonly frames: readonly FrameSnapshot[];
}

export interface TargetDebuggerSnapshot {
	readonly contextId: string;
	readonly connectionId: string;
	readonly targetId: string;
	readonly connectionGeneration: number;
	readonly revision: number;
	readonly phase: TaggedValue;
	readonly logs: readonly ConsoleMessageSnapshot[];
	readonly pause?: PauseSnapshot;
}

export interface ConsoleMessageSnapshot {
	readonly index: number;
	readonly values: readonly string[];
}

export interface EvaluationSnapshot {
	readonly expression: string;
	readonly kind: string;
	readonly value?: unknown;
	readonly unserializableValue?: string;
	readonly description?: string;
	readonly objectId?: string;
}

export interface VariableSnapshot {
	readonly name: string;
	readonly kind: string;
	readonly value?: unknown;
	readonly unserializableValue?: string;
	readonly description?: string;
	readonly objectId?: string;
}

export interface TaggedValue {
	readonly kind: string;
	readonly [key: string]: unknown;
}

export interface ContextObservationResult {
	readonly snapshot?: ContextSnapshot;
}

export interface BreakpointSpec {
	readonly sourcePath: string;
	readonly line: number;
	readonly column: number;
	readonly enabled: boolean;
	readonly condition?: string;
	readonly targetSelector?: string;
}

type JsonRecord = Record<string, unknown>;

export function parseContextSummaries(value: unknown): readonly ContextSummary[] {
	return array(value, "context summaries").map((item) => {
		const object = record(item, "context summary");
		return {
			agentInstanceId: string(object.agentInstanceId, "agentInstanceId"),
			id: string(object.id, "id"),
			displayName: string(object.displayName, "displayName"),
			revision: number(object.revision, "revision"),
			connectionCount: number(object.connectionCount, "connectionCount"),
			breakpointCount: number(object.breakpointCount, "breakpointCount"),
		};
	});
}

export function parseContextSnapshot(value: unknown): ContextSnapshot {
	const object = record(value, "context snapshot");
	return {
		agentInstanceId: string(object.agentInstanceId, "agentInstanceId"),
		id: string(object.id, "id"),
		displayName: string(object.displayName, "displayName"),
		revision: number(object.revision, "revision"),
		connections: array(object.connections, "connections").map(parseConnection),
		targetForest: array(object.targetForest, "targetForest").map(parseTargetNode),
		breakpoints: array(object.breakpoints, "breakpoints").map(parseBreakpoint),
	};
}

export function parseObservationResult(value: unknown): ContextObservationResult {
	const object = record(value, "observation result");
	const kind = string(object.kind, "kind");
	if (kind === "historyGap") {
		return { snapshot: parseContextSnapshot(object.current) };
	}
	if (kind !== "items") {
		throw new Error(`Unsupported observation result kind: ${kind}`);
	}
	const items = array(object.items, "items");
	const last = items.at(-1);
	if (last === undefined) {
		return {};
	}
	return { snapshot: parseContextSnapshot(record(last, "observation").snapshot) };
}

export function parseTargetDebuggerSnapshot(value: unknown): TargetDebuggerSnapshot {
	const object = record(value, "target debugger snapshot");
	const pauseValue = object.pause;
	return {
		contextId: string(object.contextId, "contextId"),
		connectionId: string(object.connectionId, "connectionId"),
		targetId: string(object.targetId, "targetId"),
		connectionGeneration: number(object.connectionGeneration, "connectionGeneration"),
		revision: number(object.revision, "revision"),
		phase: tagged(object.phase, "phase"),
		logs: array(object.logs, "logs").map((item) => {
			const log = record(item, "console message");
			return {
				index: number(log.index, "index"),
				values: array(log.values, "values").map((value) => string(value, "console value")),
			};
		}),
		...(pauseValue === null || pauseValue === undefined
			? {}
			: { pause: parsePause(pauseValue) }),
	};
}

export function parseSourceInfos(value: unknown): readonly SourceSnapshotInfo[] {
	return array(value, "sources").map((item) => {
		const object = record(item, "source");
		return {
			path: string(object.path, "path"),
			kind: string(object.kind, "kind"),
			status: string(object.status, "status"),
			...optionalStringProperty(object, "connectionId"),
			...optionalStringProperty(object, "targetId"),
			...optionalStringProperty(object, "sourceMapUrl"),
		};
	});
}

export function parseSourceContent(value: unknown): SourceContentSnapshot {
	const object = record(value, "source content");
	return {
		path: string(object.path, "path"),
		content: string(object.content, "content"),
	};
}

export function parseEvaluation(value: unknown): EvaluationSnapshot {
	const object = record(value, "evaluation");
	return {
		expression: string(object.expression, "expression"),
		kind: string(object.kind, "kind"),
		...(object.value === undefined || object.value === null ? {} : { value: object.value }),
		...optionalStringProperty(object, "unserializableValue"),
		...optionalStringProperty(object, "description"),
		...optionalStringProperty(object, "objectId"),
	};
}

export function parseVariables(value: unknown): readonly VariableSnapshot[] {
	return array(value, "variables").map((item) => {
		const object = record(item, "variable");
		return {
			name: string(object.name, "name"),
			kind: string(object.kind, "kind"),
			...(object.value === undefined || object.value === null ? {} : { value: object.value }),
			...optionalStringProperty(object, "unserializableValue"),
			...optionalStringProperty(object, "description"),
			...optionalStringProperty(object, "objectId"),
		};
	});
}

function parseConnection(value: unknown): ConnectionSnapshot {
	const object = record(value, "connection");
	return {
		id: string(object.id, "id"),
		configuration: parseConnectionConfiguration(object.configuration),
		generation: number(object.generation, "generation"),
		status: tagged(object.status, "status"),
		targets: array(object.targets, "targets").map(parseTarget),
	};
}

function parseConnectionConfiguration(value: unknown): ConnectionConfiguration {
	const object = record(value, "connection configuration");
	const kind = string(object.kind, "connection configuration kind");
	switch (kind) {
		case "directCdp":
		case "nodeInspector":
			return { kind, endpoint: string(object.endpoint, "endpoint") };
		case "process":
			return { kind, processId: number(object.processId, "processId") };
		case "processTree":
			return { kind, rootPid: number(object.rootPid, "rootPid") };
		case "playwright":
			return {
				kind,
				url: string(object.url, "url"),
				...optionalStringProperty(object, "playwrightPackage"),
				channel: string(object.channel, "channel") as PlaywrightChannel,
				headless: boolean(object.headless, "headless"),
				ignoreHttpsErrors: boolean(object.ignoreHttpsErrors, "ignoreHttpsErrors"),
			};
		case "chrome":
			return {
				kind,
				url: string(object.url, "url"),
				executable: string(object.executable, "executable"),
				headless: boolean(object.headless, "headless"),
				...optionalStringProperty(object, "userDataDir"),
				args: array(object.args, "args").map((item) => string(item, "argument")),
			};
		case "node": {
			const envObject = record(object.env, "environment");
			return {
				kind,
				program: string(object.program, "program"),
				args: array(object.args, "args").map((item) => string(item, "argument")),
				cwd: string(object.cwd, "cwd"),
				runtimeExecutable: string(object.runtimeExecutable, "runtimeExecutable"),
				runtimeArgs: array(object.runtimeArgs, "runtimeArgs")
					.map((item) => string(item, "runtime argument")),
				env: Object.fromEntries(
					Object.entries(envObject)
						.map(([key, item]) => [key, string(item, `environment '${key}'`)]),
				),
			};
		}
		default:
			throw new Error(`Unsupported connection configuration kind: ${kind}`);
	}
}

function parseTarget(value: unknown): TargetSnapshot {
	const object = record(value, "target");
	return {
		targetId: string(object.targetId, "targetId"),
		targetType: string(object.targetType, "targetType"),
		title: string(object.title, "title"),
		url: string(object.url, "url"),
		attached: boolean(object.attached, "attached"),
		...optionalStringProperty(object, "parentId"),
		...optionalStringProperty(object, "openerId"),
		...optionalStringProperty(object, "browserContextId"),
		...optionalStringProperty(object, "subtype"),
	};
}

function parseTargetNode(value: unknown): TargetNodeSnapshot {
	const object = record(value, "target node");
	return {
		connectionId: string(object.connectionId, "connectionId"),
		connectionGeneration: number(object.connectionGeneration, "connectionGeneration"),
		target: parseTarget(object.target),
		...optionalStringProperty(object, "parentTargetId"),
	};
}

function parseBreakpoint(value: unknown): BreakpointSnapshot {
	const object = record(value, "breakpoint");
	return {
		id: string(object.id, "id"),
		sourcePath: string(object.sourcePath, "sourcePath"),
		line: number(object.line, "line"),
		column: number(object.column, "column"),
		status: parseBreakpointStatus(object.status),
		enabled: boolean(object.enabled, "enabled"),
		...optionalStringProperty(object, "condition"),
		...optionalStringProperty(object, "targetSelector"),
	};
}

function parseBreakpointStatus(value: unknown): TaggedValue {
	if (typeof value === "string") {
		return { kind: value };
	}
	const object = record(value, "breakpoint status");
	if (object.kind !== undefined) {
		return tagged(object, "breakpoint status");
	}
	const entries = Object.entries(object);
	if (entries.length !== 1) {
		throw new Error("Expected breakpoint status to contain one variant");
	}
	const entry = entries[0];
	if (entry === undefined) {
		throw new Error("Expected breakpoint status to contain one variant");
	}
	const [kind, fields] = entry;
	const payload = record(fields, `breakpoint status '${kind}'`);
	return { kind, ...payload };
}

function parsePause(value: unknown): PauseSnapshot {
	const object = record(value, "pause");
	return {
		epoch: number(object.epoch, "epoch"),
		reason: string(object.reason, "reason"),
		frames: array(object.frames, "frames").map((frame) => {
			const value = record(frame, "frame");
			return {
				index: number(value.index, "index"),
				functionName: string(value.functionName, "functionName"),
				raw: parseLocation(value.raw),
				projected: parseProjection(value.projected),
				scopes: array(value.scopes, "scopes").map((scope) => {
					const scopeValue = record(scope, "scope");
					return {
						index: number(scopeValue.index, "index"),
						kind: string(scopeValue.kind, "kind"),
						...optionalStringProperty(scopeValue, "name"),
					};
				}),
				...optionalStringProperty(value, "breadcrumb"),
			};
		}),
	};
}

function parseProjection(value: unknown): FrameProjectionSnapshot {
	const object = record(value, "frame projection");
	const kind = string(object.kind, "kind");
	if (kind === "raw" || kind === "pending") {
		return { kind };
	}
	if (kind === "resolved") {
		return { kind, location: parseLocation(object.location) };
	}
	if (kind === "failed") {
		return { kind, message: string(object.message, "message") };
	}
	throw new Error(`Unsupported frame projection kind: ${kind}`);
}

function parseLocation(value: unknown): SourceLocation {
	const object = record(value, "source location");
	return {
		sourceUrl: string(object.sourceUrl, "sourceUrl"),
		line: number(object.line, "line"),
		column: number(object.column, "column"),
	};
}

function tagged(value: unknown, label: string): TaggedValue {
	if (typeof value === "string") {
		return { kind: value };
	}
	const object = record(value, label);
	return { ...object, kind: string(object.kind, `${label}.kind`) };
}

function optionalStringProperty<TName extends string>(
	object: JsonRecord,
	name: TName,
): Partial<Record<TName, string>> {
	const value = object[name];
	return value === undefined || value === null ? {} : { [name]: string(value, name) } as Record<TName, string>;
}

function record(value: unknown, label: string): JsonRecord {
	if (typeof value !== "object" || value === null || Array.isArray(value)) {
		throw new Error(`Expected ${label} to be an object`);
	}
	return value as JsonRecord;
}

function array(value: unknown, label: string): readonly unknown[] {
	if (!Array.isArray(value)) {
		throw new Error(`Expected ${label} to be an array`);
	}
	return value;
}

function string(value: unknown, label: string): string {
	if (typeof value !== "string") {
		throw new Error(`Expected ${label} to be a string`);
	}
	return value;
}

function number(value: unknown, label: string): number {
	if (typeof value !== "number" || !Number.isFinite(value)) {
		throw new Error(`Expected ${label} to be a finite number`);
	}
	return value;
}

function boolean(value: unknown, label: string): boolean {
	if (typeof value !== "boolean") {
		throw new Error(`Expected ${label} to be a boolean`);
	}
	return value;
}
