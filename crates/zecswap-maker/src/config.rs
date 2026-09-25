use std::net::SocketAddr;
use std::num::NonZeroU32;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use serde::Deserialize;
use zecswap_chain::evm::{Address, PrivateKeySigner};
use zecswap_chain::zcash::Network;
use zeroize::Zeroizing;

use crate::policy::Timing;
use crate::pricing::Pricing;

#[derive(Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Config {
    pub network: Chain,
    pub lightwalletd: String,
    /// The EVM chain the settlement contract is on.
    #[serde(alias = "base_rpc")]
    pub evm_rpc: String,
    /// The ZecSwap settlement contract.
    pub contract: Address,
    /// The token paid out to users.
    pub token: Address,
    /// The maker's own Zcash address, where claimed deposits are swept.
    pub sweep_to: String,
    /// Confirmations a deposit needs before the maker marks the swap ready. By default 10, or
    /// 3 for the maker's own notes: after `ready` the user can claim, so a reorganization that
    /// undoes a counted deposit costs the maker the payout.
    #[serde(default)]
    pub confirmations: Option<NonZeroU32>,
    pub data_dir: PathBuf,
    pub listen: SocketAddr,
    pub pricing: Pricing,
    pub timing: Timing,
}

#[derive(Clone, Copy, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Chain {
    Mainnet,
    Testnet,
}

impl Config {
    pub fn load(path: &Path) -> Result<Self> {
        let text =
            std::fs::read_to_string(path).with_context(|| format!("reading {}", path.display()))?;
        toml::from_str(&text).with_context(|| format!("parsing {}", path.display()))
    }

    pub fn network(&self) -> Network {
        match self.network {
            Chain::Mainnet => Network::MainNetwork,
            Chain::Testnet => Network::TestNetwork,
        }
    }
}

/// Read from the environment so they never sit in a config file.
#[derive(Clone)]
pub struct Secrets {
    /// Signs the maker's transactions on the settlement chain (`MAKER_PRIVATE_KEY`).
    pub evm_key: PrivateKeySigner,
    /// Every maker share derives from this and a quote nonce (`MAKER_ROOT_SECRET`, 32 bytes hex).
    pub root: Zeroizing<[u8; 32]>,
}

impl Secrets {
    pub fn from_env() -> Result<Self> {
        let evm_key = env("MAKER_PRIVATE_KEY")?
            .parse()
            .context("MAKER_PRIVATE_KEY")?;
        let root_hex = env("MAKER_ROOT_SECRET")?;
        let root = hex::decode(root_hex.trim_start_matches("0x"))
            .ok()
            .and_then(|bytes| <[u8; 32]>::try_from(bytes).ok())
            .context("MAKER_ROOT_SECRET must be 32 bytes of hex")?;
        Ok(Self {
            evm_key,
            root: Zeroizing::new(root),
        })
    }
}

fn env(name: &str) -> Result<Zeroizing<String>> {
    Ok(Zeroizing::new(
        std::env::var(name).with_context(|| format!("{name} is not set"))?,
    ))
}
