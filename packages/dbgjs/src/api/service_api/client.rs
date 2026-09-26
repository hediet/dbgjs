use std::sync::Arc;

use linkrpc::connection::hub_connection::ConnError;
use linkrpc::prelude::{InterfaceDefinition, LinkRpcConnection, RegisterOptions};

use super::*;

macro_rules! service_bundle {
    ($($field:ident: $definition:ident => $api:ident, $client:ident, $server:ident;)+) => {
        #[derive(Clone)]
        pub struct DbgServiceClient {
            $(pub $field: $client,)+
        }

        impl DbgServiceClient {
            pub fn new(connection: LinkRpcConnection) -> Self {
                Self {
                    $($field: $client::new(connection.clone()),)+
                }
            }
        }

        pub fn interfaces() -> Vec<InterfaceDefinition> {
            vec![$($definition::interface(),)+]
        }

        pub fn register<S>(
            connection: &LinkRpcConnection,
            service: Arc<S>,
        ) -> Result<(), ConnError>
        where
            S: $($api +)+ 'static,
        {
            $(connection.register_service(
                Arc::new($server::new(service.clone())),
                RegisterOptions::default(),
            )?;)+
            Ok(())
        }
    };
}

service_bundle! {
    service: service_api => ServiceApi, ServiceApiClient, ServiceApiServer;
    contexts: context_api => ContextApi, ContextApiClient, ContextApiServer;
    sources: source_api => SourceApi, SourceApiClient, SourceApiServer;
    captures: capture_api => CaptureApi, CaptureApiClient, CaptureApiServer;
    targets: target_debugger_api => TargetDebuggerApi, TargetDebuggerApiClient, TargetDebuggerApiServer;
    cdp: cdp_access_api => CdpAccessApi, CdpAccessApiClient, CdpAccessApiServer;
    relay: relay_api => RelayApi, RelayApiClient, RelayApiServer;
    browser: browser_automation_api => BrowserAutomationApi, BrowserAutomationApiClient, BrowserAutomationApiServer;
    coverage: coverage_api => CoverageApi, CoverageApiClient, CoverageApiServer;
    cpu: cpu_profiler_api => CpuProfilerApi, CpuProfilerApiClient, CpuProfilerApiServer;
    heap: heap_profiler_api => HeapProfilerApi, HeapProfilerApiClient, HeapProfilerApiServer;
}
