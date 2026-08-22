import { createHash } from "node:crypto";
import type { ConnectionSnapshot, TargetSnapshot } from "./apiTypes.js";

export interface TargetNode {
	readonly target: TargetSnapshot;
	readonly children: readonly TargetNode[];
}

export function computeWorkspaceContextId(workspaceUris: readonly string[]): string {
	const normalized = [...workspaceUris].sort().join("\n");
	const digest = createHash("sha256").update(normalized).digest("hex").slice(0, 16);
	return `vscode-${digest}`;
}

export function buildTargetForest(connection: ConnectionSnapshot): readonly TargetNode[] {
	const children = new Map<string, TargetSnapshot[]>();
	const roots: TargetSnapshot[] = [];
	const ids = new Set(connection.targets.map((target) => target.targetId));

	for (const target of connection.targets) {
		const parent = target.parentId !== undefined && ids.has(target.parentId)
			? target.parentId
			: target.openerId !== undefined && ids.has(target.openerId)
				? target.openerId
				: undefined;
		if (parent === undefined) {
			roots.push(target);
		} else {
			const existing = children.get(parent) ?? [];
			existing.push(target);
			children.set(parent, existing);
		}
	}

	const seen = new Set<string>();
	const build = (target: TargetSnapshot): TargetNode => {
		if (seen.has(target.targetId)) {
			return { target, children: [] };
		}
		seen.add(target.targetId);
		return {
			target,
			children: (children.get(target.targetId) ?? []).map(build),
		};
	};
	const forest = roots.map(build);
	for (const target of connection.targets) {
		if (!seen.has(target.targetId)) {
			forest.push(build(target));
		}
	}
	return forest;
}

export function targetKey(connectionId: string, targetId: string): string {
	return `${connectionId}\0${targetId}`;
}
