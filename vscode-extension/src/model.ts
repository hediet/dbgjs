import { createHash } from "node:crypto";
import type { TargetNodeSnapshot } from "./apiTypes.js";

export interface TargetReference {
	readonly contextId: string;
	readonly connectionId: string;
	readonly connectionGeneration: number;
	readonly targetId: string;
}

export function computeWorkspaceContextId(workspaceUris: readonly string[]): string {
	const normalized = [...workspaceUris].sort().join("\n");
	const digest = createHash("sha256").update(normalized).digest("hex").slice(0, 16);
	return `vscode-${digest}`;
}

export function breakpointId(path: string, line: number, column: number): string {
	const digest = createHash("sha256")
		.update(`${path}\0${line}\0${column}`)
		.digest("hex")
		.slice(0, 20);
	return `vscode-${digest}`;
}

export function targetKey(
	target: TargetReference,
): string {
	return `${target.contextId}\0${target.connectionId}\0`
		+ `${target.connectionGeneration}\0${target.targetId}`;
}

export function targetReference(
	contextId: string,
	node: TargetNodeSnapshot,
): TargetReference {
	return {
		contextId,
		connectionId: node.connectionId,
		connectionGeneration: node.connectionGeneration,
		targetId: node.target.targetId,
	};
}

export function findTargetNode(
	forest: readonly TargetNodeSnapshot[],
	target: TargetReference,
): TargetNodeSnapshot | undefined {
	return forest.find(
		(node) => targetKey(targetReference(target.contextId, node)) === targetKey(target),
	);
}
