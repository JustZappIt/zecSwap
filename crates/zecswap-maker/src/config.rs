use std::net::SocketAddr;
use std::num::NonZeroU32;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use serde::Deserialize;
use zecswap_api::Network;
use zecswap_chain::evm::{Address, PrivateKeySigner, Windows};
use zeroize::Zeroizing;

use crate::policy::Timing;
use crate::pricing::Pricing;

#[derive(Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Config {
    /// Testnet or mainnet: the Zcash network, the chains, Railgun and token the maker takes, and
    /// the lightwalletd it uses unless `lightwalletd` names another.
    pub network: Network,
    #[serde(default)]
    pub lightwalletd: Option<String>,
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
    /// Telegram messages when the maker's own account runs low on ETH and once it is topped up.
    #[serde(default)]
    pub gas_alerts: Option<zecswap_alerts::GasAlerts>,
    /// Cancels, refunds and reverse claims, and every other contract call of the maker's, sent
    /// through a revert-protecting private RPC instead of the public mempool.
    #[serde(default)]
    pub reveals: Option<RevealsConfig>,
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
pub struct RevealsConfig {
    /// Flashbots Protect's RPC: its default endpoint, sharing the hash alone (`hint=hash`). A
    /// cancel or refund that would land after its window closed is dropped, never mined, and one
    /// dropped is sent again while its window lasts. Each holds the maker's account until it mines
    /// or `blockRange` + 5 blocks pass, so the maker's sends go one at a time.
    pub private_rpc: String,
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
    /// Rejects settings the contract, with `windows`, would refuse escrows under.
    pub fn check(&self, windows: Windows) -> Result<()> {
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
        // An escrow may be funded up to its funding deadline, and `openReverse` refuses a `t0`
        // under the minimum window from when it lands.
        anyhow::ensure!(
            self.ready_after - self.funding_window >= windows.min
                && self.refund_after - self.ready_after >= windows.min,
            "reverse t0 must follow the funding deadline, and t1 follow t0, by the contract's {}s minimum window",
            windows.min
        );
        Ok(())
    }
}

impl Config {
    pub fn load(path: &Path) -> Result<Self> {
        let text =
            std::fs::read_to_string(path).with_context(|| format!("reading {}", path.display()))?;
        toml::from_str(&text).with_context(|| format!("parsing {}", path.display()))
    }

    pub fn lightwalletd(&self) -> &str {
        self.lightwalletd
            .as_deref()
            .unwrap_or(self.network.lightwalletd())
    }
}

/// Read from the environment so they never sit in a config file.
#[derive(Clone)]
pub struct Secrets {
    /// Signs the maker's transactions on the settlement chain (`MAKER_PRIVATE_KEY`).
    pub evm_key: PrivateKeySigner,
    /// Every maker share derives from this and a quote nonce (`MAKER_ROOT_SECRET`, 32 bytes hex).
    pub root: Zeroizing<[u8; 32]>,
    /// Keys each swap's ticket (`MAKER_TICKET_SECRET`, 32 bytes hex). The gateway holds it too,
    /// so it must lead to nothing else of the maker's.
    pub ticket_key: Zeroizing<[u8; 32]>,
    pub zcash_seed: Option<Zeroizing<Vec<u8>>>,
}

impl Secrets {
    pub fn from_env() -> Result<Self> {
        let evm_key = env("MAKER_PRIVATE_KEY")?
            .parse()
            .context("MAKER_PRIVATE_KEY")?;
        Ok(Self {
            evm_key,
            root: secret("MAKER_ROOT_SECRET")?,
            ticket_key: secret("MAKER_TICKET_SECRET")?,
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

fn secret(name: &str) -> Result<Zeroizing<[u8; 32]>> {
    hex::decode(env(name)?.trim_start_matches("0x"))
        .ok()
        .and_then(|bytes| <[u8; 32]>::try_from(bytes).ok())
        .map(Zeroizing::new)
        .with_context(|| format!("{name} must be 32 bytes of hex"))
}

fn env(name: &str) -> Result<Zeroizing<String>> {
    Ok(Zeroizing::new(
        std::env::var(name).with_context(|| format!("{name} is not set"))?,
    ))
}
