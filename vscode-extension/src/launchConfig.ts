import { constants } from "node:fs";
import { access } from "node:fs/promises";
import { delimiter, isAbsolute, join } from "node:path";
import { pathToFileURL } from "node:url";
import * as vscode from "vscode";
import type {
	ConnectionConfiguration,
	PlaywrightChannel,
} from "./apiTypes.js";
import type { TargetNodeSnapshot } from "./apiTypes.js";
import type { TargetReference } from "./model.js";

export type DbgjsLaunchConfiguration =
	| { readonly runtime: "context"; }
	| {
			readonly runtime: "target";
			readonly contextId: string;
			readonly connectionId: string;
			readonly connectionGeneration: number;
			readonly targetId: string;
	  }
	| {
			readonly runtime: "node";
			readonly program: string;
			readonly args?: readonly string[];
			readonly cwd?: string;
			readonly runtimeExecutable?: string;
			readonly runtimeArgs?: readonly string[];
			readonly env?: Readonly<Record<string, string | null>>;
	  }
	| {
			readonly runtime: "playwright";
			readonly url: string;
			readonly playwrightPackage?: string;
			readonly channel?: PlaywrightChannel;
			readonly headless?: boolean;
			readonly ignoreHttpsErrors?: boolean;
	  }
	| {
			readonly runtime: "chrome";
			readonly url: string;
			readonly executablePath?: string;
			readonly headless?: boolean;
			readonly userDataDir?: string;
			readonly args?: readonly string[];
	  };

export interface ResolvedLaunch {
	readonly connectionId?: string;
	readonly configuration?: ConnectionConfiguration;
	readonly target?: TargetReference;
}

export async function resolveLaunch(
	value: unknown,
	sessionId: string,
): Promise<ResolvedLaunch> {
	const configuration = parseLaunch(value);
	switch (configuration.runtime) {
		case "context":
			return {};
		case "target":
			return {
				target: {
					contextId: configuration.contextId,
					connectionId: configuration.connectionId,
					connectionGeneration: configuration.connectionGeneration,
					targetId: configuration.targetId,
				},
			};
		case "node": {
			const connectionId = `vscode-${sessionId}`;
			const cwd = configuration.cwd ?? firstWorkspacePath();
			const env = Object.fromEntries(
				Object.entries(configuration.env ?? {})
					.filter((entry): entry is [string, string] => entry[1] !== null),
			);
			return {
				connectionId,
				configuration: {
					kind: "node",
					program: configuration.program,
					args: configuration.args ?? [],
					cwd,
					runtimeExecutable: configuration.runtimeExecutable ?? "node",
					runtimeArgs: configuration.runtimeArgs ?? [],
					env,
				},
			};
		}
		case "playwright": {
			const connectionId = `vscode-${sessionId}`;
			return {
				connectionId,
				configuration: {
					kind: "playwright",
					url: normalizeRuntimeUrl(configuration.url),
					playwrightPackage: configuration.playwrightPackage
						?? await findWorkspacePlaywright(),
					channel: configuration.channel ?? "bundled",
					headless: configuration.headless ?? true,
					ignoreHttpsErrors: configuration.ignoreHttpsErrors ?? false,
				},
			};
		}
		case "chrome": {
			const connectionId = `vscode-${sessionId}`;
			return {
				connectionId,
				configuration: {
					kind: "chrome",
					url: normalizeRuntimeUrl(configuration.url),
					executable: configuration.executablePath ?? await findInstalledChrome(),
					headless: configuration.headless ?? false,
					...(configuration.userDataDir === undefined
						? {}
						: { userDataDir: configuration.userDataDir }),
					args: configuration.args ?? [],
				},
			};
		}
	}
}

export function parseLaunch(value: unknown): DbgjsLaunchConfiguration {
	const object = objectValue(value, "launch configuration");
	const runtime = object.runtime ?? "context";
	if (runtime === "context") {
		return { runtime };
	}
	if (runtime === "target") {
		return {
			runtime,
			contextId: requiredString(object.contextId, "contextId"),
			connectionId: requiredString(object.connectionId, "connectionId"),
			connectionGeneration: requiredNumber(
				object.connectionGeneration,
				"connectionGeneration",
			),
			targetId: requiredString(object.targetId, "targetId"),
		};
	}
	if (runtime === "node") {
		return {
			runtime,
			program: requiredString(object.program, "program"),
			...optionalStringArray(object, "args"),
			...optionalString(object, "cwd"),
			...optionalString(object, "runtimeExecutable"),
			...optionalStringArray(object, "runtimeArgs"),
			...(object.env === undefined
				? {}
				: { env: nullableStringRecord(object.env, "env") }),
		};
	}

	if (runtime === "playwright") {
		return {
			runtime,
			url: requiredString(object.url, "url"),
			...optionalString(object, "playwrightPackage"),
			...(object.channel === undefined
				? {}
				: { channel: playwrightChannel(object.channel) }),
			...optionalBoolean(object, "headless"),
			...optionalBoolean(object, "ignoreHttpsErrors"),
		};
	}
	if (runtime === "chrome") {
		return {
			runtime,
			url: requiredString(object.url, "url"),
			...optionalString(object, "executablePath"),
			...optionalBoolean(object, "headless"),
			...optionalString(object, "userDataDir"),
			...optionalStringArray(object, "args"),
		};
	}
	throw new Error(`Unsupported dbgjs runtime '${String(runtime)}'`);
}

export function targetDebugConfiguration(
	contextId: string,
	node: TargetNodeSnapshot,
): vscode.DebugConfiguration {
	return {
		type: "dbgjs",
		request: "attach",
		name: targetSessionName(node),
		runtime: "target",
		contextId,
		connectionId: node.connectionId,
		connectionGeneration: node.connectionGeneration,
		targetId: node.target.targetId,
	};
}

export function targetReferenceFromConfiguration(
	value: vscode.DebugConfiguration,
): TargetReference | undefined {
	try {
		const parsed = parseLaunch(value);
		return parsed.runtime === "target"
			? {
				contextId: parsed.contextId,
				connectionId: parsed.connectionId,
				connectionGeneration: parsed.connectionGeneration,
				targetId: parsed.targetId,
			}
			: undefined;
	} catch {
		return undefined;
	}
}

function targetSessionName(node: TargetNodeSnapshot): string {
	return node.target.title
		|| node.target.url
		|| `${node.target.targetType} ${node.target.targetId}`;
}

function normalizeRuntimeUrl(value: string): string {
	return isAbsolute(value) ? pathToFileURL(value).toString() : value;
}

export async function findInstalledChrome(
	platform: NodeJS.Platform = process.platform,
	environment: NodeJS.ProcessEnv = process.env,
): Promise<string> {
	const candidates = chromeCandidates(platform, environment);
	for (const candidate of candidates) {
		if (candidate.includes("/") || candidate.includes("\\")) {
			if (await isExecutable(candidate)) {
				return candidate;
			}
			continue;
		}
		const resolved = await findOnPath(candidate, environment.PATH);
		if (resolved !== undefined) {
			return resolved;
		}
	}
	throw new Error(
		"No installed Chrome or Chromium executable was found. Set 'executablePath' in the dbgjs launch configuration.",
	);
}

async function findWorkspacePlaywright(): Promise<string> {
	for (const folder of vscode.workspace.workspaceFolders ?? []) {
		const candidate = vscode.Uri.joinPath(folder.uri, "node_modules", "playwright", "index.mjs");
		try {
			await vscode.workspace.fs.stat(candidate);
			if (candidate.scheme !== "file") {
				throw new Error(
					`Playwright at ${candidate.toString()} is not on the extension host filesystem`,
				);
			}
			return candidate.fsPath;
		} catch (error) {
			if (error instanceof Error && error.message.includes("not on the extension host")) {
				throw error;
			}
		}
	}
	throw new Error(
		"Playwright was not found in a workspace node_modules folder. Install 'playwright' in the workspace or set 'playwrightPackage'.",
	);
}

function firstWorkspacePath(): string {
	const folder = vscode.workspace.workspaceFolders?.[0];
	if (folder === undefined || folder.uri.scheme !== "file") {
		throw new Error("Node.js launch requires a local or remote filesystem workspace folder");
	}
	return folder.uri.fsPath;
}

function chromeCandidates(
	platform: NodeJS.Platform,
	environment: NodeJS.ProcessEnv,
): readonly string[] {
	if (platform === "darwin") {
		return [
			"/Applications/Google Chrome.app/Contents/MacOS/Google Chrome",
			"/Applications/Chromium.app/Contents/MacOS/Chromium",
		];
	}
	if (platform === "win32") {
		return [
			join(environment.PROGRAMFILES ?? "", "Google", "Chrome", "Application", "chrome.exe"),
			join(environment["PROGRAMFILES(X86)"] ?? "", "Google", "Chrome", "Application", "chrome.exe"),
			join(environment.LOCALAPPDATA ?? "", "Google", "Chrome", "Application", "chrome.exe"),
		];
	}
	return ["google-chrome", "google-chrome-stable", "chromium", "chromium-browser"];
}

async function findOnPath(command: string, path: string | undefined): Promise<string | undefined> {
	for (const directory of path?.split(delimiter) ?? []) {
		const candidate = join(directory, command);
		if (await isExecutable(candidate)) {
			return candidate;
		}
	}
	return undefined;
}

async function isExecutable(path: string): Promise<boolean> {
	try {
		await access(path, constants.X_OK);
		return true;
	} catch {
		return false;
	}
}

function objectValue(value: unknown, label: string): Record<string, unknown> {
	if (typeof value !== "object" || value === null || Array.isArray(value)) {
		throw new Error(`Expected ${label} to be an object`);
	}
	return value as Record<string, unknown>;
}

function requiredString(value: unknown, name: string): string {
	if (typeof value !== "string" || value.length === 0) {
		throw new Error(`dbgjs launch property '${name}' must be a non-empty string`);
	}
	return value;
}

function requiredNumber(value: unknown, name: string): number {
	if (typeof value !== "number" || !Number.isSafeInteger(value) || value < 0) {
		throw new Error(`dbgjs launch property '${name}' must be a non-negative integer`);
	}
	return value;
}

function optionalString<TName extends string>(
	object: Record<string, unknown>,
	name: TName,
): Partial<Record<TName, string>> {
	return object[name] === undefined
		? {}
		: { [name]: requiredString(object[name], name) } as Record<TName, string>;
}

function optionalBoolean<TName extends string>(
	object: Record<string, unknown>,
	name: TName,
): Partial<Record<TName, boolean>> {
	const value = object[name];
	if (value === undefined) {
		return {};
	}
	if (typeof value !== "boolean") {
		throw new Error(`dbgjs launch property '${name}' must be a boolean`);
	}
	return { [name]: value } as Record<TName, boolean>;
}

function optionalStringArray<TName extends string>(
	object: Record<string, unknown>,
	name: TName,
): Partial<Record<TName, readonly string[]>> {
	const value = object[name];
	if (value === undefined) {
		return {};
	}
	if (!Array.isArray(value)
		|| !value.every((item): item is string => typeof item === "string")) {
		throw new Error(`dbgjs launch property '${name}' must be an array of strings`);
	}
	const result: Partial<Record<TName, readonly string[]>> = {};
	result[name] = value;
	return result;
}

function nullableStringRecord(
	value: unknown,
	name: string,
): Readonly<Record<string, string | null>> {
	const object = objectValue(value, name);
	for (const [key, item] of Object.entries(object)) {
		if (typeof item !== "string" && item !== null) {
			throw new Error(`dbgjs launch property '${name}.${key}' must be a string or null`);
		}
	}
	return object as Record<string, string | null>;
}

function playwrightChannel(value: unknown): PlaywrightChannel {
	const channels: readonly PlaywrightChannel[] = [
		"bundled", "chrome", "chromeBeta", "chromeDev", "chromeCanary",
		"msedge", "msedgeBeta", "msedgeDev", "msedgeCanary",
	];
	if (typeof value !== "string" || !channels.includes(value as PlaywrightChannel)) {
		throw new Error(`Unsupported Playwright channel '${String(value)}'`);
	}
	return value as PlaywrightChannel;
}
