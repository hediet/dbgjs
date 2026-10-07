use std::collections::BTreeSet;
use std::fs;
use std::path::{Path, PathBuf};
use std::time::Duration;

use linkrpc::prelude::DirectoryServiceClient;
use serde::{Deserialize, Serialize};
use tokio::time::timeout;

use super::local_rpc::{
    LocalRpcError, LocalServiceEndpoint, default_service_directory, open_endpoint_connection,
    read_endpoint,
};
use crate::api::service_api::{self, DbgServiceClient, ServiceDescription, ServiceInterface};

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct DiscoveredService {
    pub state_file: PathBuf,
    pub status: ServiceStatus,
    pub compatible: bool,
    pub description: Option<ServiceDescription>,
    pub error: Option<String>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum ServiceStatus {
    Running,
    Stale,
    Unreachable,
    Invalid,
}

pub async fn list_services(
    directory: &Path,
    explicit_state_file: Option<&Path>,
    expected_fingerprint: &str,
) -> Result<Vec<DiscoveredService>, LocalRpcError> {
    let mut paths = BTreeSet::new();
    let registry = directory.join("services");
    match fs::read_dir(&registry) {
        Ok(entries) => {
            for entry in entries {
                let entry = entry?;
                if entry.file_type()?.is_dir() {
                    paths.insert(entry.path().join("service.json"));
                }
            }
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(error) => return Err(error.into()),
    }
    paths.insert(directory.join("service.json"));
    if let Some(path) = explicit_state_file {
        paths.insert(path.to_owned());
    }
    let mut services = Vec::new();
    for path in paths {
        let endpoint = match read_endpoint(&path) {
            Ok(endpoint) => endpoint,
            Err(LocalRpcError::Io(error)) if error.kind() == std::io::ErrorKind::NotFound => {
                continue;
            }
            Err(error) => {
                services.push(DiscoveredService {
                    state_file: path,
                    status: ServiceStatus::Invalid,
                    compatible: false,
                    description: None,
                    error: Some(error.to_string()),
                });
                continue;
            }
        };
        let result = timeout(Duration::from_secs(2), probe_service(&endpoint)).await;
        let (status, description, error) = match result {
            Ok(Ok(description)) => (ServiceStatus::Running, Some(description), None),
            Ok(Err(error)) => {
                let stale = error.is_stale_endpoint();
                (
                    if stale {
                        ServiceStatus::Stale
                    } else {
                        ServiceStatus::Unreachable
                    },
                    endpoint.description,
                    Some(error.to_string()),
                )
            }
            Err(_) => (
                ServiceStatus::Unreachable,
                endpoint.description,
                Some("Service discovery probe timed out".into()),
            ),
        };
        services.push(DiscoveredService {
            state_file: path,
            status,
            compatible: status == ServiceStatus::Running
                && description.as_ref().is_some_and(|description| {
                    description.contract_fingerprint == expected_fingerprint
                }),
            description,
            error,
        });
    }
    Ok(services)
}

pub async fn list_available_services() -> Result<Vec<DiscoveredService>, LocalRpcError> {
    let explicit = std::env::var_os("DBGJS_SERVICE_STATE").map(PathBuf::from);
    list_services(
        &default_service_directory(),
        explicit.as_deref(),
        &service_api::service_description().contract_fingerprint,
    )
    .await
}

async fn probe_service(
    endpoint: &LocalServiceEndpoint,
) -> Result<ServiceDescription, LocalRpcError> {
    let connection = open_endpoint_connection(endpoint).await?;
    let probe = async {
        let directory = DirectoryServiceClient::new(connection.clone());
        let mut cursor = None;
        let mut interfaces = Vec::new();
        loop {
            let listing = directory
                .list(None, None, cursor.clone(), Some(100), None)
                .await
                .map_err(|error| LocalRpcError::Rpc(format!("{error:?}")))?;
            interfaces.extend(
                listing
                    .items
                    .into_iter()
                    .filter(|item| {
                        item.service_id.is_empty() && item.interface_id.starts_with("dev.dbgjs.")
                    })
                    .map(|item| ServiceInterface {
                        id: item.interface_id,
                        hash: item.interface_hash,
                    }),
            );
            match listing.next_cursor {
                Some(next) if cursor.as_ref() != Some(&next) => cursor = Some(next),
                Some(_) => {
                    return Err(LocalRpcError::Rpc(
                        "Service directory repeated its cursor".into(),
                    ));
                }
                None => break,
            }
        }
        interfaces.sort();
        let client = DbgServiceClient::new(connection.clone());
        let discovery = service_api::discovery_api::interface();
        let description = if interfaces.iter().any(|interface| {
            interface.id == discovery.id() && interface.hash == discovery.schema_hash()
        }) {
            let description = client
                .discovery
                .describe()
                .await
                .map_err(|error| LocalRpcError::Rpc(format!("{error:?}")))?;
            let mut declared = description.interfaces.clone();
            declared.sort();
            if declared != interfaces
                || description.contract_fingerprint
                    != service_api::contract_fingerprint(&interfaces)
            {
                return Err(LocalRpcError::Rpc(
                    "Service discovery contract does not match reflection".into(),
                ));
            }
            description
        } else {
            // The original lifecycle probe stays readable without calling changing debugger APIs.
            let info = client
                .service
                .service_info()
                .await
                .map_err(|error| LocalRpcError::Rpc(format!("{error:?}")))?;
            ServiceDescription {
                process_id: info.process_id,
                version: "unknown (legacy service)".into(),
                git_commit: "unknown".into(),
                contract_fingerprint: service_api::contract_fingerprint(&interfaces),
                interfaces,
            }
        };
        if description.process_id != endpoint.process_id {
            return Err(LocalRpcError::EndpointOwnerChanged {
                expected: endpoint.process_id,
                actual: description.process_id,
            });
        }
        Ok(description)
    };
    tokio::select! {
        result = probe => result,
        () = connection.run() => Err(LocalRpcError::Rpc("Service closed during discovery".into())),
    }
}

pub fn check_startup_policy(
    services: &[DiscoveredService],
    state_file: &Path,
    refuse_incompatible: bool,
) -> Result<(), LocalRpcError> {
    if services.iter().any(|service| {
        service.state_file == state_file
            && service.status == ServiceStatus::Running
            && service.compatible
    }) {
        return Ok(());
    }
    for service in services {
        if service.status == ServiceStatus::Invalid {
            return Err(LocalRpcError::Rpc(format!(
                "Cannot determine available services: {}: {}",
                service.state_file.display(),
                service.error.as_deref().unwrap_or("invalid endpoint"),
            )));
        }
        if service.status == ServiceStatus::Unreachable {
            return Err(LocalRpcError::Rpc(format!(
                "Cannot determine whether service {} is running: {}",
                service.state_file.display(),
                service.error.as_deref().unwrap_or("probe failed"),
            )));
        }
        if refuse_incompatible && service.status == ServiceStatus::Running && !service.compatible {
            let Some(description) = service.description.as_ref() else {
                return Err(LocalRpcError::Rpc(
                    "Running service has no discovery description".into(),
                ));
            };
            return Err(LocalRpcError::Rpc(format!(
                "Incompatible dbgjs service running (PID {}, version {}, contract {}) at {}. \
                 Use Show Services or explicitly Start Compatible Service.",
                description.process_id,
                description.version,
                description.contract_fingerprint,
                service.state_file.display(),
            )));
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::super::local_rpc::{connect_existing, serve_local};
    use super::*;
    #[cfg(unix)]
    use linkrpc::prelude::{CallCtx, JsonRpcError, link_rpc_interface};
    use tokio::sync::watch;

    #[cfg(unix)]
    #[link_rpc_interface(id = "dev.dbgjs.cdp-debugger")]
    trait LegacyApi {
        async fn service_info() -> Result<service_api::ServiceInfo, JsonRpcError>;
        async fn shutdown() -> Result<bool, JsonRpcError>;
    }

    #[cfg(unix)]
    struct Legacy;

    #[cfg(unix)]
    #[async_trait::async_trait]
    impl LegacyApi for Legacy {
        async fn service_info(
            &self,
            _: &CallCtx,
        ) -> Result<service_api::ServiceInfo, JsonRpcError> {
            Ok(service_api::ServiceInfo {
                process_id: std::process::id(),
                agent_instance_id: "legacy".into(),
            })
        }
        async fn shutdown(&self, _: &CallCtx) -> Result<bool, JsonRpcError> {
            panic!("discovery must not shut down legacy services")
        }
    }

    fn discovered(status: ServiceStatus, compatible: bool) -> DiscoveredService {
        DiscoveredService {
            state_file: PathBuf::from("selected.json"),
            status,
            compatible,
            description: Some(service_api::service_description()),
            error: Some("probe diagnostic".into()),
        }
    }

    #[test]
    fn startup_policy_distinguishes_absent_incompatible_and_unknown() {
        let path = Path::new("selected.json");
        assert!(check_startup_policy(&[], path, true).is_ok());
        assert!(
            check_startup_policy(&[discovered(ServiceStatus::Stale, false)], path, true).is_ok()
        );
        assert!(
            check_startup_policy(&[discovered(ServiceStatus::Running, false)], path, true).is_err()
        );
        assert!(
            check_startup_policy(&[discovered(ServiceStatus::Running, false)], path, false).is_ok()
        );
        for status in [ServiceStatus::Invalid, ServiceStatus::Unreachable] {
            assert!(check_startup_policy(&[discovered(status, false)], path, true).is_err());
        }
        let services = [
            discovered(ServiceStatus::Running, true),
            discovered(ServiceStatus::Running, false),
            discovered(ServiceStatus::Invalid, false),
        ];
        assert!(check_startup_policy(&services, path, true).is_ok());
        assert!(check_startup_policy(&services[..1], Path::new("different.json"), true).is_ok());
    }

    #[tokio::test]
    async fn discovery_probes_live_services_and_never_lists_tokens() {
        let directory = tempfile::tempdir().unwrap();
        let description = service_api::service_description();
        let state_file = directory
            .path()
            .join("services")
            .join(&description.contract_fingerprint)
            .join("service.json");
        let (shutdown, receiver) = watch::channel(false);
        let server_path = state_file.clone();
        let server = tokio::spawn(async move {
            serve_local(&server_path, shutdown, receiver).await.unwrap();
        });
        let client = timeout(Duration::from_secs(5), async {
            loop {
                if state_file.exists() {
                    break connect_existing(&state_file).await.unwrap();
                }
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .unwrap();
        let endpoint = read_endpoint(&state_file).unwrap();
        let services = list_services(
            directory.path(),
            Some(&state_file),
            &description.contract_fingerprint,
        )
        .await
        .unwrap();
        assert_eq!(services.len(), 1);
        assert_eq!(services[0].status, ServiceStatus::Running);
        assert!(services[0].compatible);
        assert_eq!(services[0].description, Some(description.clone()));
        let json = serde_json::to_string(&services).unwrap();
        assert!(!json.contains(&endpoint.token));
        assert!(!json.contains("\"token\""));
        let incompatible = list_services(directory.path(), None, "different-contract")
            .await
            .unwrap();
        assert!(!incompatible[0].compatible);
        assert!(check_startup_policy(&incompatible, &state_file, true).is_err());
        assert!(client.discovery.describe().await.is_ok());

        let mut wrong_token = endpoint.clone();
        wrong_token.token = "wrong-token".into();
        assert!(probe_service(&wrong_token).await.is_err());
        let mut wrong_owner = endpoint;
        wrong_owner.process_id = wrong_owner.process_id.wrapping_add(1);
        assert!(matches!(
            probe_service(&wrong_owner).await,
            Err(LocalRpcError::EndpointOwnerChanged { .. })
        ));

        client.service.shutdown().await.unwrap();
        server.await.unwrap();
        assert!(
            list_services(directory.path(), None, &description.contract_fingerprint)
                .await
                .unwrap()
                .is_empty()
        );
        let invalid = directory.path().join("service.json");
        fs::write(&invalid, "{").unwrap();
        let services = list_services(directory.path(), None, "").await.unwrap();
        assert_eq!(services[0].status, ServiceStatus::Invalid);
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn legacy_services_are_discovered_without_shutting_them_down() {
        use super::super::local_rpc::{LocalTransportEndpoint, ensure_service};
        use linkrpc::prelude::{LinkRpcConnection, RegisterOptions};
        use linkrpc_tokio::ndjson::NdjsonTransport;
        use std::sync::Arc;

        let directory = tempfile::tempdir().unwrap();
        let socket = directory.path().join("legacy.sock");
        let listener = tokio::net::UnixListener::bind(&socket).unwrap();
        let endpoint = LocalServiceEndpoint {
            process_id: std::process::id(),
            transport: LocalTransportEndpoint::UnixSocket { path: socket },
            token: "legacy-test-token".into(),
            description: None,
        };
        fs::write(
            directory.path().join("service.json"),
            serde_json::to_vec(&endpoint).unwrap(),
        )
        .unwrap();
        let server = tokio::spawn(async move {
            loop {
                let (stream, _) = listener.accept().await.unwrap();
                tokio::spawn(async move {
                    let transport = NdjsonTransport::from_stream(stream);
                    let preamble = transport.read_preamble().await.unwrap().unwrap();
                    assert_eq!(preamble.token.as_deref(), Some("legacy-test-token"));
                    let connection = LinkRpcConnection::new(Box::new(transport));
                    connection
                        .register_service(
                            Arc::new(LegacyApiServer::new(Arc::new(Legacy))),
                            RegisterOptions::default(),
                        )
                        .unwrap();
                    connection.enable_reflection();
                    connection.run().await;
                });
            }
        });
        let description = service_api::service_description();
        let current_state = directory
            .path()
            .join("services")
            .join(&description.contract_fingerprint)
            .join("service.json");
        let services = list_services(directory.path(), None, &description.contract_fingerprint)
            .await
            .unwrap();
        assert_eq!(services.len(), 1);
        assert_eq!(services[0].status, ServiceStatus::Running);
        assert!(!services[0].compatible);
        assert_eq!(
            services[0].description.as_ref().unwrap().interfaces.len(),
            1
        );
        assert!(check_startup_policy(&services, &current_state, true).is_err());
        assert!(!current_state.exists());
        assert!(matches!(
            ensure_service(&directory.path().join("service.json")).await,
            Err(LocalRpcError::InterfaceHashMismatch { .. })
        ));
        let (shutdown, receiver) = watch::channel(false);
        let current_path = current_state.clone();
        let current_server = tokio::spawn(async move {
            serve_local(&current_path, shutdown, receiver)
                .await
                .unwrap();
        });
        let client = timeout(Duration::from_secs(5), async {
            loop {
                if current_state.exists() {
                    break connect_existing(&current_state).await.unwrap();
                }
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .unwrap();
        let services = list_services(directory.path(), None, &description.contract_fingerprint)
            .await
            .unwrap();
        assert_eq!(services.len(), 2);
        assert!(
            services
                .iter()
                .all(|service| service.status == ServiceStatus::Running)
        );
        assert_eq!(
            services.iter().filter(|service| service.compatible).count(),
            1
        );
        assert!(check_startup_policy(&services, &current_state, true).is_ok());
        client.service.shutdown().await.unwrap();
        current_server.await.unwrap();
        server.abort();
        assert!(server.await.unwrap_err().is_cancelled());
    }
}
