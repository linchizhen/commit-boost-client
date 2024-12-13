use std::{
    collections::{HashMap, HashSet},
    path::{Path, PathBuf},
    sync::Arc,
};

use alloy::{
    primitives::{address, Address, U256},
    providers::ProviderBuilder,
    rpc::types::beacon::BlsPublicKey,
    sol,
};
use eyre::{bail, ensure, Context};
use serde::{Deserialize, Serialize};
use tracing::{debug, info};
use url::Url;

use super::{load_optional_env_var, PbsConfig, RelayConfig, MUX_PATH_ENV};
use crate::{pbs::RelayClient, types::Chain};

#[derive(Debug, Deserialize, Serialize)]
pub struct PbsMuxes {
    /// List of PBS multiplexers
    #[serde(rename = "mux")]
    pub muxes: Vec<MuxConfig>,
}

#[derive(Debug, Clone)]
pub struct RuntimeMuxConfig {
    pub id: String,
    pub config: Arc<PbsConfig>,
    pub relays: Vec<RelayClient>,
}

impl PbsMuxes {
    pub async fn validate_and_fill(
        self,
        chain: Chain,
        default_pbs: &PbsConfig,
    ) -> eyre::Result<HashMap<BlsPublicKey, RuntimeMuxConfig>> {
        let mut muxes = self.muxes;

        for mux in muxes.iter_mut() {
            ensure!(!mux.relays.is_empty(), "mux config {} must have at least one relay", mux.id);

            if let Some(loader) = &mux.loader {
                let extra_keys = loader.load(&mux.id, chain, default_pbs.rpc_url.clone()).await?;
                mux.validator_pubkeys.extend(extra_keys);
            }

            ensure!(
                !mux.validator_pubkeys.is_empty(),
                "mux config {} must have at least one validator pubkey",
                mux.id
            );
        }

        // check that validator pubkeys are in disjoint sets
        let mut unique_pubkeys = HashSet::new();
        for mux in muxes.iter() {
            for pubkey in mux.validator_pubkeys.iter() {
                if !unique_pubkeys.insert(pubkey) {
                    bail!("duplicate validator pubkey in muxes: {pubkey}");
                }
            }
        }

        let mut configs = HashMap::new();
        // fill the configs using the default pbs config and relay entries
        for mux in muxes {
            info!(
                id = mux.id,
                keys = mux.validator_pubkeys.len(),
                relays = mux.relays.len(),
                "using mux"
            );

            let mut relay_clients = Vec::with_capacity(mux.relays.len());
            for config in mux.relays.into_iter() {
                relay_clients.push(RelayClient::new(config)?);
            }

            let config = PbsConfig {
                timeout_get_header_ms: mux
                    .timeout_get_header_ms
                    .unwrap_or(default_pbs.timeout_get_header_ms),
                late_in_slot_time_ms: mux
                    .late_in_slot_time_ms
                    .unwrap_or(default_pbs.late_in_slot_time_ms),
                ..default_pbs.clone()
            };
            let config = Arc::new(config);

            let runtime_config = RuntimeMuxConfig { id: mux.id, config, relays: relay_clients };
            for pubkey in mux.validator_pubkeys.iter() {
                configs.insert(*pubkey, runtime_config.clone());
            }
        }

        Ok(configs)
    }
}

/// Configuration for the PBS Multiplexer
#[derive(Debug, Deserialize, Serialize)]
pub struct MuxConfig {
    /// Identifier for this mux config
    pub id: String,
    /// Relays to use for this mux config
    pub relays: Vec<RelayConfig>,
    /// Which validator pubkeys to match against this mux config
    #[serde(default)]
    pub validator_pubkeys: Vec<BlsPublicKey>,
    /// Loader for extra validator pubkeys
    pub loader: Option<MuxKeysLoader>,
    pub timeout_get_header_ms: Option<u64>,
    pub late_in_slot_time_ms: Option<u64>,
}

impl MuxConfig {
    /// Returns the env, actual path, and internal path to use for the file
    /// loader
    pub fn loader_env(&self) -> Option<(String, String, String)> {
        self.loader.as_ref().and_then(|loader| match loader {
            MuxKeysLoader::File(path_buf) => {
                let path =
                    path_buf.to_str().unwrap_or_else(|| panic!("invalid path: {:?}", path_buf));
                let internal_path = get_mux_path(&self.id);

                Some((get_mux_env(&self.id), path.to_owned(), internal_path))
            }
            MuxKeysLoader::Registry { .. } => None,
        })
    }
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(untagged)]
pub enum MuxKeysLoader {
    /// A file containing a list of validator pubkeys
    File(PathBuf),
    Registry {
        registry: NORegistry,
        node_operator_id: u64,
    },
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub enum NORegistry {
    #[serde(alias = "lido")]
    Lido,
}

impl MuxKeysLoader {
    pub async fn load(
        &self,
        mux_id: &str,
        chain: Chain,
        rpc_url: Option<Url>,
    ) -> eyre::Result<Vec<BlsPublicKey>> {
        match self {
            Self::File(config_path) => {
                // First try loading from env
                let path: PathBuf = load_optional_env_var(&get_mux_env(mux_id))
                    .map(PathBuf::from)
                    .unwrap_or(config_path.clone());
                let file = load_file(path)?;
                serde_json::from_str(&file).wrap_err("failed to parse mux keys file")
            }

            Self::Registry { registry, node_operator_id } => match registry {
                NORegistry::Lido => {
                    let Some(rpc_url) = rpc_url else {
                        bail!("Lido registry requires RPC URL to be set in the PBS config");
                    };

                    fetch_lido_registry_keys(rpc_url, chain, U256::from(*node_operator_id)).await
                }
            },
        }
    }
}

fn load_file<P: AsRef<Path> + std::fmt::Debug>(path: P) -> eyre::Result<String> {
    std::fs::read_to_string(&path).wrap_err(format!("Unable to find mux keys file: {path:?}"))
}

/// A different env var for each mux
fn get_mux_env(mux_id: &str) -> String {
    format!("{MUX_PATH_ENV}_{mux_id}")
}

/// Path to the mux file
fn get_mux_path(mux_id: &str) -> String {
    format!("/{mux_id}-mux_keys.json")
}

sol! {
    #[allow(missing_docs)]
    #[sol(rpc)]
    LidoRegistry,
    "src/abi/LidoNORegistry.json"
}

fn lido_registry_address(chain: Chain) -> eyre::Result<Address> {
    match chain {
        Chain::Mainnet => Ok(address!("55032650b14df07b85bF18A3a3eC8E0Af2e028d5")),
        Chain::Holesky => Ok(address!("595F64Ddc3856a3b5Ff4f4CC1d1fb4B46cFd2bAC")),
        Chain::Sepolia => Ok(address!("33d6E15047E8644F8DDf5CD05d202dfE587DA6E3")),
        _ => bail!("Lido registry not supported for chain: {chain:?}"),
    }
}

async fn fetch_lido_registry_keys(
    rpc_url: Url,
    chain: Chain,
    node_operator_id: U256,
) -> eyre::Result<Vec<BlsPublicKey>> {
    debug!(
        "loading operator keys from Lido registry: chain={:?}, node_operator_id={}",
        chain, node_operator_id
    );

    let provider = ProviderBuilder::new().on_http(rpc_url);
    let registry_address = lido_registry_address(chain)?;
    let registry = LidoRegistry::new(registry_address, provider);

    let total_keys =
        registry.getTotalSigningKeyCount(node_operator_id).call().await?._0.try_into()?;

    debug!("fetching {total_keys} total keys");

    const CALL_BATCH_SIZE: u64 = 250u64;
    const BLS_PK_LEN: usize = BlsPublicKey::len_bytes();

    let mut keys = vec![];
    let mut offset = 0;

    while offset < total_keys {
        let limit = CALL_BATCH_SIZE.min(total_keys - offset);

        let pubkeys = registry
            .getSigningKeys(node_operator_id, U256::from(offset), U256::from(limit))
            .call()
            .await?
            .pubkeys;

        ensure!(
            pubkeys.len() % BLS_PK_LEN == 0,
            "unexpected number of keys in batch, expected multiple of {BLS_PK_LEN}, got {}",
            pubkeys.len()
        );

        for chunk in pubkeys.chunks(BLS_PK_LEN) {
            keys.push(BlsPublicKey::try_from(chunk)?);
        }

        offset += limit;

        if offset % 1000 == 0 {
            debug!("fetched {offset} keys");
        }
    }

    ensure!(keys.len() == total_keys as usize, "expected {total_keys} keys, got {}", keys.len());
    let unique: Vec<_> = keys.iter().collect::<HashSet<_>>().into_iter().collect();
    ensure!(unique.len() == keys.len(), "found duplicate keys in registry");

    Ok(keys)
}

#[cfg(test)]
mod tests {
    use alloy::{primitives::U256, providers::ProviderBuilder};
    use url::Url;

    use super::*;

    #[tokio::test]
    async fn test_lido_registry_address() -> eyre::Result<()> {
        let url = Url::parse("https://ethereum-rpc.publicnode.com")?;
        let provider = ProviderBuilder::new().on_http(url);

        let registry =
            LidoRegistry::new(address!("55032650b14df07b85bF18A3a3eC8E0Af2e028d5"), provider);

        const LIMIT: usize = 3;
        let node_operator_id = U256::from(1);

        let total_keys: u64 =
            registry.getTotalSigningKeyCount(node_operator_id).call().await?._0.try_into()?;

        assert!(total_keys > LIMIT as u64);

        let pubkeys = registry
            .getSigningKeys(node_operator_id, U256::ZERO, U256::from(LIMIT))
            .call()
            .await?
            .pubkeys;

        let mut vec = vec![];
        for chunk in pubkeys.chunks(BlsPublicKey::len_bytes()) {
            vec.push(BlsPublicKey::try_from(chunk)?);
        }

        assert_eq!(vec.len(), LIMIT);

        Ok(())
    }
}
