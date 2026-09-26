import * as vscode from "vscode";
import type {
	ConnectionSnapshot,
	TargetNodeSnapshot,
	TargetSnapshot,
} from "./apiTypes.js";
import type { WorkspaceContextController } from "./workspaceContext.js";

type TreeElement =
	| { readonly kind: "context"; }
	| { readonly kind: "connection"; readonly connection: ConnectionSnapshot; }
	| {
			readonly kind: "target";
			readonly connection: ConnectionSnapshot;
			readonly node: TargetNodeSnapshot;
	  }
	| { readonly kind: "error"; readonly error: Error; };

export class TargetTreeProvider
implements vscode.TreeDataProvider<TreeElement>, vscode.Disposable {
	private readonly changeEmitter = new vscode.EventEmitter<TreeElement | undefined>();
	private readonly subscriptions: vscode.Disposable[];

	public readonly onDidChangeTreeData = this.changeEmitter.event;

	public constructor(private readonly controller: WorkspaceContextController) {
		this.subscriptions = [
			controller.onDidChangeSnapshot(() => this.changeEmitter.fire(undefined)),
			controller.onDidChangeError(() => this.changeEmitter.fire(undefined)),
		];
	}

	public getTreeItem(element: TreeElement): vscode.TreeItem {
		switch (element.kind) {
			case "context": {
				const item = new vscode.TreeItem(
					this.controller.displayName,
					vscode.TreeItemCollapsibleState.Expanded,
				);
				item.description = this.controller.contextId;
				item.iconPath = new vscode.ThemeIcon("workspace-trusted");
				return item;
			}
			case "connection": {
				const item = new vscode.TreeItem(
					element.connection.id,
					vscode.TreeItemCollapsibleState.Expanded,
				);
				item.description = element.connection.status.kind;
				item.iconPath = new vscode.ThemeIcon(
					element.connection.status.kind === "connected" ? "debug-disconnect" : "plug",
				);
				return item;
			}
			case "target":
				return targetTreeItem(
					element.node.target,
					this.targetChildren(element.node).length > 0,
				);
			case "error": {
				const item = new vscode.TreeItem(element.error.message);
				item.iconPath = new vscode.ThemeIcon("error");
				item.tooltip = element.error.stack ?? element.error.message;
				return item;
			}
		}
	}

	public getChildren(element?: TreeElement): TreeElement[] {
		if (element === undefined) {
			return [{ kind: "context" }];
		}
		if (element.kind === "context") {
			if (this.controller.error !== undefined && this.controller.snapshot === undefined) {
				return [{ kind: "error", error: this.controller.error }];
			}
			return (this.controller.snapshot?.connections ?? []).map((connection) => ({
				kind: "connection",
				connection,
			}));
		}
		if (element.kind === "connection") {
			return (this.controller.snapshot?.targetForest ?? [])
				.filter((node) =>
					node.connectionId === element.connection.id
					&& node.parentTargetId === null
				)
				.map((node) => ({
					kind: "target",
					connection: element.connection,
					node,
				}));
		}
		if (element.kind === "target") {
			return this.targetChildren(element.node).map((node) => ({
				kind: "target",
				connection: element.connection,
				node,
			}));
		}
		return [];
	}

	private targetChildren(node: TargetNodeSnapshot): readonly TargetNodeSnapshot[] {
		return (this.controller.snapshot?.targetForest ?? []).filter((candidate) =>
			candidate.connectionId === node.connectionId
			&& candidate.connectionGeneration === node.connectionGeneration
			&& candidate.parentTargetId === node.target.targetId
		);
	}

	public dispose(): void {
		for (const subscription of this.subscriptions) {
			subscription.dispose();
		}
		this.changeEmitter.dispose();
	}
}

function targetTreeItem(target: TargetSnapshot, hasChildren: boolean): vscode.TreeItem {
	const label = target.title || target.url || target.targetId;
	const item = new vscode.TreeItem(
		label,
		hasChildren
			? vscode.TreeItemCollapsibleState.Collapsed
			: vscode.TreeItemCollapsibleState.None,
	);
	item.description = `${target.targetType}${target.attached ? " • attached" : ""}`;
	item.tooltip = [
		target.url,
		`Target: ${target.targetId}`,
		target.parentId === null ? undefined : `Parent: ${target.parentId}`,
		target.openerId === null ? undefined : `Opener: ${target.openerId}`,
	].filter((value): value is string => value !== undefined).join("\n");
	item.contextValue = target.attached ? "dbgjs.attachedTarget" : "dbgjs.target";
	item.iconPath = new vscode.ThemeIcon(targetIcon(target.targetType));
	return item;
}

function targetIcon(type: string): string {
	switch (type) {
		case "page":
			return "browser";
		case "worker":
		case "service_worker":
		case "shared_worker":
			return "server-process";
		default:
			return "debug-alt";
	}
}
