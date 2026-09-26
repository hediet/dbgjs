import { isRpcFailure, type Result } from "@hediet/linkrpc";

/** Adapt a final RPC result to throwing callers without losing its branded diagnostic. */
export function unwrapRpcResult<T, E extends { readonly message: string }>(result: Result<T, E>): T {
	if (isRpcFailure(result)) {
		throw new Error(result.error.message, { cause: result });
	}
	return result;
}
