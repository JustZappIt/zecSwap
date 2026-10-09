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
    /// EVM depth required before a forward escrow is considered settled.
    #[serde(default = "default_evm_confirmations")]
    pub evm_confirmations: NonZeroU32,
    pub data_dir: PathBuf,
    pub listen: SocketAddr,
    pub pricing: Pricing,
    pub timing: Timing,
    #[serde(default)]
    pub reverse: Option<ReverseConfig>,
    /// Optional native-gas alerts using the existing Telegram delivery queue.
    #[serde(default)]
    pub gas_alerts: Option<GasAlerts>,
    /// The most swaps, of both directions, waiting on their users to pay in at once: past it,
    /// accepts are refused, so spam can tie up only so much inventory and gas. No limit if absent.
    #[serde(default)]
    pub max_awaiting_deposit: Option<usize>,
    /// A Privacy Pass token each accept spends, so a device does only as many swaps a day as
    /// the issuer gives it tokens, without the maker learning which device asked.
    #[serde(default)]
    pub tokens: Option<zecswap_tokens::server::Config>,
}

#[derive(Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct GasAlerts {
    pub interval_seconds: u64,
    pub accounts: Vec<GasAlertAccount>,
}

#[derive(Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct GasAlertAccount {
    pub label: String,
    pub address: Address,
    pub low_wei: u64,
    pub recovery_wei: u64,
}

impl GasAlerts {
    pub fn check(&self) -> Result<()> {
        anyhow::ensure!(
            (60..=3600).contains(&self.interval_seconds),
            "gas alert interval must be 60..3600 seconds"
        );
        anyhow::ensure!(
            !self.accounts.is_empty() && self.accounts.len() <= 8,
            "gas alerts require 1..8 accounts"
        );
        let mut seen = std::collections::HashSet::new();
        for account in &self.accounts {
            anyhow::ensure!(
                !account.label.is_empty()
                    && account.label.len() <= 40
                    && account
                        .label
                        .bytes()
                        .all(|b| b.is_ascii_alphanumeric() || b" -_".contains(&b)),
                "invalid gas alert account label"
            );
            anyhow::ensure!(
                !account.address.is_zero() && seen.insert(account.address),
                "gas alert addresses must be nonzero and unique"
            );
            anyhow::ensure!(
                account.low_wei > 0 && account.recovery_wei > account.low_wei,
                "gas recovery threshold must exceed a positive low threshold"
            );
        }
        Ok(())
    }
}

fn default_evm_confirmations() -> NonZeroU32 {
    NonZeroU32::new(12).unwrap()
}

#[derive(Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ReverseConfig {
    pub evm_confirmations: NonZeroU32,
    pub funding_window: u64,
    pub ready_after: u64,
    pub refund_after: u64,
    pub deposit_margin: u64,
    pub fee_reserve_zat: u64,
}

impl ReverseConfig {
    pub fn check(&self) -> Result<()> {
        anyhow::ensure!(
            self.refund_after <= 24 * 60 * 60 && self.fee_reserve_zat > 0,
            "reverse swaps require a fee reserve and must finish within 24 hours"
        );
        anyhow::ensure!(
            self.funding_window > 0 && self.deposit_margin > 0,
            "reverse funding window and deposit margin must be positive"
        );
        anyhow::ensure!(
            self.funding_window
                .checked_add(self.deposit_margin)
                .is_some_and(|min| min < self.ready_after),
            "reverse ready deadline must leave time to confirm the deposit"
        );
        anyhow::ensure!(
            self.ready_after < self.refund_after,
            "reverse refund deadline must follow ready"
        );
        Ok(())
    }
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
    pub zcash_seed: Option<Zeroizing<Vec<u8>>>,
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
            zcash_seed: match std::env::var("MAKER_ZCASH_SEED") {
                Ok(value) => {
                    let value = Zeroizing::new(value);
                    let seed = Zeroizing::new(
                        hex::decode(value.trim()).context("MAKER_ZCASH_SEED must be hex")?,
                    );
                    anyhow::ensure!(
                        (32..=252).contains(&seed.len()),
                        "MAKER_ZCASH_SEED must contain 32 to 252 bytes"
                    );
                    Some(seed)
                }
                Err(std::env::VarError::NotPresent) => None,
                Err(e) => return Err(e).context("MAKER_ZCASH_SEED"),
            },
        })
    }
}

fn env(name: &str) -> Result<Zeroizing<String>> {
    Ok(Zeroizing::new(
        std::env::var(name).with_context(|| format!("{name} is not set"))?,
    ))
}
