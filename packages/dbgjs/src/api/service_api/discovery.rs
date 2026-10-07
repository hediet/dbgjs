use super::*;
use sha2::{Digest, Sha256};

#[link_rpc_interface(id = "dev.dbgjs.discovery.v1")]
pub trait DiscoveryApi {
    async fn describe() -> Result<ServiceDescription, JsonRpcError>;
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct ServiceDescription {
    pub process_id: u32,
    pub version: String,
    pub git_commit: String,
    pub contract_fingerprint: String,
    pub interfaces: Vec<ServiceInterface>,
}

#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct ServiceInterface {
    pub id: String,
    pub hash: String,
}

pub fn contract_fingerprint(interfaces: &[ServiceInterface]) -> String {
    let mut sorted = interfaces.to_vec();
    sorted.sort();
    let mut digest = Sha256::new();
    digest.update(b"dbgjs-service-contract-v1\0");
    for interface in sorted {
        for value in [interface.id, interface.hash] {
            digest.update((value.len() as u64).to_be_bytes());
            digest.update(value.as_bytes());
        }
    }
    format!("{:x}", digest.finalize())
}

pub fn service_description() -> ServiceDescription {
    let interfaces = interfaces()
        .into_iter()
        .map(|definition| ServiceInterface {
            id: definition.id().to_owned(),
            hash: definition.schema_hash().to_owned(),
        })
        .collect::<Vec<_>>();
    ServiceDescription {
        process_id: std::process::id(),
        version: env!("CARGO_PKG_VERSION").to_owned(),
        git_commit: env!("DBGJS_BUILD_GIT_COMMIT").to_owned(),
        contract_fingerprint: contract_fingerprint(&interfaces),
        interfaces,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fingerprint_tracks_the_complete_contract_not_order() {
        let interfaces = service_description().interfaces;
        let expected = contract_fingerprint(&interfaces);
        let mut reversed = interfaces.clone();
        reversed.reverse();
        assert_eq!(contract_fingerprint(&reversed), expected);
        for index in 0..interfaces.len() {
            let mut changed = interfaces.clone();
            changed[index].hash.push('x');
            assert_ne!(contract_fingerprint(&changed), expected);
            changed = interfaces.clone();
            changed.remove(index);
            assert_ne!(contract_fingerprint(&changed), expected);
        }
        let mut added = interfaces;
        added.push(ServiceInterface {
            id: "extra".into(),
            hash: "hash".into(),
        });
        assert_ne!(contract_fingerprint(&added), expected);
    }
}
