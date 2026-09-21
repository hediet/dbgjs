import type { InterfaceClient, LinkRpcConnection } from "@hediet/linkrpc";
import { BrowserAutomationApi } from "./generated/browserAutomationApi.js";
import { CaptureApi } from "./generated/captureApi.js";
import { CdpAccessApi } from "./generated/cdpAccessApi.js";
import { ContextApi } from "./generated/contextApi.js";
import { CoverageApi } from "./generated/coverageApi.js";
import { CpuProfilerApi } from "./generated/cpuProfilerApi.js";
import { HeapProfilerApi } from "./generated/heapProfilerApi.js";
import { RelayApi } from "./generated/relayApi.js";
import { ServiceApi } from "./generated/serviceApi.js";
import { SourceApi } from "./generated/sourceApi.js";
import { TargetDebuggerApi } from "./generated/targetDebuggerApi.js";

/** Typed facets sharing a caller-owned connection and its lifetime. */
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
		this.service = connection.get(ServiceApi);
		this.contexts = connection.get(ContextApi);
		this.sources = connection.get(SourceApi);
		this.captures = connection.get(CaptureApi);
		this.targets = connection.get(TargetDebuggerApi);
		this.cdp = connection.get(CdpAccessApi);
		this.relay = connection.get(RelayApi);
		this.browser = connection.get(BrowserAutomationApi);
		this.coverage = connection.get(CoverageApi);
		this.cpu = connection.get(CpuProfilerApi);
		this.heap = connection.get(HeapProfilerApi);
	}
}
