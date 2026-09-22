import type { DbgServiceClient } from "./dbgServiceClient.js";

type ContextClient = DbgServiceClient["contexts"];
type SourceClient = DbgServiceClient["sources"];
type TargetClient = DbgServiceClient["targets"];

type MethodResult<TMethod extends (...args: never[]) => unknown> =
	Awaited<ReturnType<TMethod>>;

export type ContextSummary = MethodResult<ContextClient["list_contexts"]>[number];
export type ContextSnapshot = MethodResult<ContextClient["get_context"]>;
export type ContextKind =
	Parameters<ContextClient["put_context"]>[0]["kind"];
export type ConnectionSnapshot = ContextSnapshot["connections"][number];
export type ConnectionRef =
	Parameters<ContextClient["connect_connection"]>[0]["connectionRef"];
export type TargetRef =
	Parameters<TargetClient["get_target"]>[0]["targetRef"];
export type ConnectionConfiguration =
	Parameters<ContextClient["put_connection"]>[0]["configuration"];
export type PlaywrightChannel =
	Extract<ConnectionConfiguration, { kind: "playwright" }>["channel"];
export type TargetNodeSnapshot = ContextSnapshot["targetForest"][number];
export type TargetSnapshot = TargetNodeSnapshot["target"];
export type BreakpointSnapshot = ContextSnapshot["breakpoints"][number];
export type BreakpointSpec =
	Parameters<ContextClient["put_breakpoint_spec"]>[0]["specification"];
export type SourceSnapshotInfo = MethodResult<SourceClient["list_sources"]>[number];
export type SourceContentSnapshot = MethodResult<SourceClient["show_source"]>;
export type TargetDebuggerSnapshot = MethodResult<TargetClient["get_target"]>;
export type PauseSnapshot = NonNullable<TargetDebuggerSnapshot["pause"]>;
export type FrameSnapshot = PauseSnapshot["frames"][number];
export type SourceLocation = FrameSnapshot["raw"];
export type EvaluationSnapshot = MethodResult<TargetClient["evaluate_target"]>;
export type VariableSnapshot = MethodResult<TargetClient["get_scope_variables"]>[number];
export type ObservationResult = MethodResult<ContextClient["observe_context"]>;
export type ObservationCursor =
	Parameters<ContextClient["observe_context"]>[0]["cursor"];
export type TargetWaitPredicate =
	Parameters<TargetClient["wait_target"]>[0]["predicate"];
export type StepKind =
	Parameters<TargetClient["step_target"]>[0]["kind"];

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
