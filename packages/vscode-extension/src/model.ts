import { createHash } from "node:crypto";
import { posix, win32 } from "node:path";
import type { ConnectionRef, TargetNodeSnapshot, TargetRef } from "./apiTypes.js";

export interface TargetReference extends Readonly<ConnectionRef>, Readonly<Pick<TargetRef, "targetId">> {
	readonly connectionGeneration: number;
}

export function normalizeContextPath(value: string): string {
	if (/^[A-Za-z]:[^\\/]/.test(value)) {
		throw new Error("Drive-relative Windows context paths are not supported");
	}
	if (/^[A-Za-z]:[\\/]/.test(value) || value.startsWith("\\\\") || value.startsWith("//")) {
		const normalized = win32.normalize(value).toLowerCase();
		return /^\\\\[^\\]+\\[^\\]+\\$/.test(normalized)
			? normalized.slice(0, -1)
			: normalized;
	}
	if (value.startsWith("/")) {
		return posix.normalize(value).toLowerCase();
	}
	throw new Error("Context path must be absolute");
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
