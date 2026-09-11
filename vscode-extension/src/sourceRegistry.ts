import type { DebugProtocol } from "@vscode/debugprotocol";
import { basename, isAbsolute } from "node:path";
import * as vscode from "vscode";
import type { SourceLocation, SourceSnapshotInfo } from "./apiTypes.js";
import type { WorkspaceContextController } from "./workspaceContext.js";

export class SourceRegistry {
	private nextReference = 1;
	private readonly pathToReference = new Map<string, number>();
	private readonly referenceToPath = new Map<number, string>();

	public constructor(private readonly controller: WorkspaceContextController) {}

	public sourceForLocation(location: SourceLocation): DebugProtocol.Source {
		return this.sourceForPath(location.sourceUrl);
	}

	public sourceForInfo(source: SourceSnapshotInfo): DebugProtocol.Source {
		return this.sourceForPath(source.path);
	}

	public sourcePath(source: DebugProtocol.Source): string | undefined {
		if (source.sourceReference !== undefined && source.sourceReference > 0) {
			return this.referenceToPath.get(source.sourceReference);
		}
		return source.path;
	}

	public async content(sourceReference: number): Promise<{ content: string; mimeType?: string }> {
		const path = this.referenceToPath.get(sourceReference);
		if (path === undefined) {
			throw new Error(`Unknown dbgjs source reference ${sourceReference}`);
		}
		const source = await this.controller.client.showSource(this.controller.contextId, path);
		return {
			content: source.content,
			...mimeType(path),
		};
	}

	private sourceForPath(path: string): DebugProtocol.Source {
		const uri = workspaceUri(path);
		if (uri !== undefined) {
			return {
				name: basename(uri.path) || path,
				path: uri.fsPath,
			};
		}
		const reference = this.referenceFor(path);
		return {
			name: sourceName(path),
			sourceReference: reference,
			origin: path,
		};
	}

	private referenceFor(path: string): number {
		const existing = this.pathToReference.get(path);
		if (existing !== undefined) {
			return existing;
		}
		const reference = this.nextReference++;
		this.pathToReference.set(path, reference);
		this.referenceToPath.set(reference, path);
		return reference;
	}
}

function workspaceUri(path: string): vscode.Uri | undefined {
	if (path.startsWith("file:")) {
		return vscode.Uri.parse(path);
	}
	if (isAbsolute(path)) {
		return vscode.Uri.file(path);
	}
	const normalized = path.replaceAll("\\", "/");
	return vscode.workspace.textDocuments
		.map((document) => document.uri)
		.find((uri) => uri.path.replaceAll("\\", "/").endsWith(`/${normalized}`));
}

function sourceName(path: string): string {
	const withoutQuery = path.split("?", 1)[0] ?? path;
	const slash = Math.max(withoutQuery.lastIndexOf("/"), withoutQuery.lastIndexOf("\\"));
	return withoutQuery.slice(slash + 1) || path;
}

function mimeType(path: string): { mimeType?: string } {
	if (path.endsWith(".ts") || path.endsWith(".tsx")) {
		return { mimeType: "text/typescript" };
	}
	if (path.endsWith(".js") || path.endsWith(".jsx") || path.endsWith(".mjs")) {
		return { mimeType: "text/javascript" };
	}
	return {};
}
