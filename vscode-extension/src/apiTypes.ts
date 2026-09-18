import type { InterfaceClient } from "@hediet/linkrpc";
import { DebuggerService } from "./generated/debuggerService.js";

export type DebuggerServiceClient = InterfaceClient<typeof DebuggerService>;

type MethodResult<TMethod extends keyof DebuggerServiceClient> =
	Awaited<ReturnType<DebuggerServiceClient[TMethod]>>;

export type ContextSummary = MethodResult<"list_contexts">[number];
export type ContextSnapshot = MethodResult<"get_context">;
export type ContextKind =
	Parameters<DebuggerServiceClient["put_context"]>[0]["kind"];
export type ConnectionSnapshot = ContextSnapshot["connections"][number];
export type ConnectionConfiguration =
	Parameters<DebuggerServiceClient["put_connection"]>[0]["configuration"];
export type PlaywrightChannel =
	Extract<ConnectionConfiguration, { kind: "playwright" }>["channel"];
export type TargetNodeSnapshot = ContextSnapshot["targetForest"][number];
export type TargetSnapshot = TargetNodeSnapshot["target"];
export type BreakpointSnapshot = ContextSnapshot["breakpoints"][number];
export type BreakpointSpec =
	Parameters<DebuggerServiceClient["put_breakpoint_spec"]>[0]["specification"];
export type SourceSnapshotInfo = MethodResult<"list_sources">[number];
export type SourceContentSnapshot = MethodResult<"show_source">;
export type TargetDebuggerSnapshot = MethodResult<"get_target">;
export type PauseSnapshot = NonNullable<TargetDebuggerSnapshot["pause"]>;
export type FrameSnapshot = PauseSnapshot["frames"][number];
export type SourceLocation = FrameSnapshot["raw"];
export type EvaluationSnapshot = MethodResult<"evaluate_target">;
export type VariableSnapshot = MethodResult<"get_scope_variables">[number];
export type ObservationResult = MethodResult<"observe_context">;
export type ObservationCursor =
	Parameters<DebuggerServiceClient["observe_context"]>[0]["cursor"];
export type TargetWaitPredicate =
	Parameters<DebuggerServiceClient["wait_target"]>[0]["predicate"];
export type StepKind =
	Parameters<DebuggerServiceClient["step_target"]>[0]["kind"];

export function observationSnapshot(
	result: ObservationResult,
): ContextSnapshot | undefined {
	if (result.kind === "historyGap") {
		return result.current;
	}
	return result.items.at(-1)?.snapshot;
}

export function breakpointStatusKind(
	status: BreakpointSnapshot["status"],
): string {
	if (typeof status === "string") {
		return status;
	}
	const [kind] = Object.keys(status);
	if (kind === undefined) {
		throw new Error("Breakpoint status has no variant");
	}
	return kind;
}

export function breakpointStatusError(
	status: BreakpointSnapshot["status"],
): string | undefined {
	return typeof status === "object" && status !== null && "failed" in status
		? status.failed.message
		: undefined;
}
