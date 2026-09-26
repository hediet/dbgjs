import type { InterfaceClient, LinkRpcConnection } from "@hediet/linkrpc";
import {
	BrowserAutomationApi, BrowserAutomationApiRoot,
	CaptureApi, CaptureApiRoot,
	CdpAccessApi, CdpAccessApiRoot,
	ContextApi, ContextApiRoot,
	CoverageApi, CoverageApiRoot,
	CpuProfilerApi, CpuProfilerApiRoot,
	HeapProfilerApi, HeapProfilerApiRoot,
	RelayApi, RelayApiRoot,
	ServiceApi, ServiceApiRoot,
	SourceApi, SourceApiRoot,
	TargetDebuggerApi, TargetDebuggerApiRoot,
} from "./generated/interfaces.js";

/**
 * Typed facets sharing a caller-owned connection and its lifetime.
 * Declared application failures remain RpcFailure values for callers to narrow.
 */
export class DbgServiceClient {
	public readonly service: InterfaceClient<typeof ServiceApi>;
	public readonly contexts: InterfaceClient<typeof ContextApi>;
	public readonly sources: InterfaceClient<typeof SourceApi>;
	public readonly captures: InterfaceClient<typeof CaptureApi>;
	public readonly targets: InterfaceClient<typeof TargetDebuggerApi>;
	public readonly cdp: InterfaceClient<typeof CdpAccessApi>;
	public readonly relay: InterfaceClient<typeof RelayApi>;
	public readonly browser: InterfaceClient<typeof BrowserAutomationApi>;
	public readonly coverage: InterfaceClient<typeof CoverageApi>;
	public readonly cpu: InterfaceClient<typeof CpuProfilerApi>;
	public readonly heap: InterfaceClient<typeof HeapProfilerApi>;

	public constructor(connection: LinkRpcConnection<undefined>) {
		this.service = connection.get(ServiceApiRoot);
		this.contexts = connection.get(ContextApiRoot);
		this.sources = connection.get(SourceApiRoot);
		this.captures = connection.get(CaptureApiRoot);
		this.targets = connection.get(TargetDebuggerApiRoot);
		this.cdp = connection.get(CdpAccessApiRoot);
		this.relay = connection.get(RelayApiRoot);
		this.browser = connection.get(BrowserAutomationApiRoot);
		this.coverage = connection.get(CoverageApiRoot);
		this.cpu = connection.get(CpuProfilerApiRoot);
		this.heap = connection.get(HeapProfilerApiRoot);
	}
}
