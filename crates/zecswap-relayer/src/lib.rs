//! A relayer for users with no account on the settlement chain: it sends what a swap's own key
//! signed, and the reveal, and keeps a fee from the payout. It runs with its own key, apart
//! from any maker: a maker that learned the user share before the claim landed could hold the
//! claim back until its window closes at `t1`, then refund and race the user for the ZEC. With
//! `railgun_sends` it also sends wallets' own Railgun transactions as their broadcaster, paid
//! by a fee note to its own Railgun wallet (`sends`), and with `deposits` it shields deposit
//! addresses' public tokens into their owners' Railgun balances on their EIP-3009
//! authorizations, for a fee they commit to (`deposits`).

pub mod api;
mod deposits;
#[cfg(test)]
mod deposits_tests;
#[cfg(test)]
mod funding_tests;
mod lanes;
#[cfg(test)]
mod lanes_tests;
#[cfg(test)]
mod logging_tests;
mod monitor;
#[cfg(test)]
mod monitor_tests;
mod pricing;
#[cfg(test)]
mod retries_tests;
#[cfg(test)]
mod reveals_tests;
mod reverse;
mod sends;
#[cfg(test)]
mod terms_tests;
mod turns;
#[cfg(test)]
mod turns_tests;

use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use alloy_primitives::{Address, B256};
use anyhow::{Context as _, ensure};
use serde::Deserialize;
use tracing::{info, warn};
use zecswap_api::Network;
use zecswap_api::relayer::{Claim, Payout, Sent, Terms};
use zecswap_api::server::MonitorToken;
use zecswap_chain::evm::funding::FundingPolicy;
use zecswap_chain::evm::judge::{self, Go, Refusal};
use zecswap_chain::evm::railgun::{MAX_CALLDATA_BYTES, SendPolicy};
use zecswap_chain::evm::{
    BlockId, Head, OnChainSwap, PrivateKeySigner, RpcMeter, Settlement, Stage, Windows, endpoint,
    wall_now,
};
use zecswap_core::{Domain, SecretShare, signer};
use zecswap_railgun::{Keys, ShieldNote};

pub use deposits::DepositsSnapshot;
pub use lanes::drain_lanes;
pub use sends::{Sending, SendsSnapshot};

use crate::lanes::Lanes;
use crate::pricing::SwapFee;
use crate::turns::{Step, Steps, Turns};

#[derive(Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Config {
    /// Testnet or mainnet: the chains, Railgun and token the relayer takes.
    pub network: Network,
    pub evm_rpc: String,
    /// The ZecSwap settlement contract.
    pub contract: Address,
    /// The one token whose swaps it sends for, in whose base units it takes its fees.
    pub token: Address,
    /// The one maker whose swaps it sends for.
    pub maker: Address,
    pub listen: SocketAddr,
    /// Token base units each payout keeps at least, for the gas of the claim and the payout: the
    /// floor of its fee.
    pub fee: u64,
    /// With `providers`, the gas a payout's fee pays for, priced at the gas price and ETH price
    /// when it is quoted: the claim and the payout, or the refund and its payout. None leaves
    /// the fee at `fee`.
    #[serde(default)]
    pub fee_gas: u64,
    /// Live ETH and USDC prices, asked in order (keys as the maker's: `ZCASH_CMC_KEY`,
    /// `ALCHEMY_API_KEY`): with them each fee follows gas, never under its floor. Without them
    /// every fee is its floor.
    #[serde(default)]
    pub providers: Vec<zecswap_prices::Provider>,
    /// The margin of a fee priced by gas over what the gas costs, in basis points.
    #[serde(default)]
    pub fee_margin_bps: u32,
    /// Seconds a claim's window must still have when the relayer reveals in it, on arrival and
    /// again when the reveal's turn to broadcast comes: less, and the claim could land after `t1`,
    /// reverted, the share public with the maker's refund window open. A reverse refund's window,
    /// once open, lasts for good.
    pub claim_margin: u64,
    /// Claims and reverse refunds sent through a private RPC; required on mainnet.
    #[serde(default)]
    pub reveals: Option<RevealsConfig>,
    /// How many of its largest transactions each lane's account keeps ETH for.
    #[serde(default = "default_top_up_transactions")]
    pub top_up_transactions: u64,
    /// Opt-in sponsorship of reverse swaps' escrow funding from Railgun.
    #[serde(default)]
    pub reverse_funding: Option<ReverseFundingConfig>,
    /// Opt-in sending of wallets' own Railgun transactions, private sends and withdrawals, as
    /// their broadcaster, on whichever chain the contract is. Needs `RELAYER_RAILGUN_SEED`.
    #[serde(default)]
    pub railgun_sends: Option<RailgunSendsConfig>,
    /// Telegram messages when the relayer's own account runs low on ETH and once it is topped
    /// up; queued in the `railgun_sends` journal.
    #[serde(default)]
    pub gas_alerts: Option<zecswap_alerts::GasAlerts>,
    /// Opt-in shielding of deposit addresses' tokens, the relayer's `token`, into their owners'
    /// Railgun balances, through the Deposits contract.
    #[serde(default)]
    pub deposits: Option<DepositsConfig>,
}

#[derive(Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RevealsConfig {
    /// Flashbots Protect's RPC, to send claims and reverse refunds through instead of the public
    /// mempool: each reveals a share in its calldata, and one that landed after its window closed
    /// would revert with the share public, the other side's window open. Protect lands a
    /// transaction only if it succeeds, and drops one it couldn't land within its URL's
    /// `blockRange` blocks (25 if none), so they go one at a time. Its default endpoint, never
    /// `/fast`, sharing the hash alone (`hint=hash`), and naming the builders to reach beside
    /// its own (`builder`).
    pub private_rpc: String,
}

#[derive(Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ReverseFundingConfig {
    /// Railgun's own Relay Adapt on the chain unless given; one given must be it, where the
    /// network has Railgun's own deployment.
    #[serde(default)]
    pub relay_adapt: Option<Address>,
    pub max_gas_limit: u64,
    pub max_gas_price_wei: u64,
    /// Escrow-token base units each funding pays this relayer for its gas at least.
    pub fee: u64,
    /// With `providers`, the gas the funding fee pays for: the funding and its ready.
    #[serde(default)]
    pub fee_gas: u64,
}

#[derive(Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RailgunSendsConfig {
    /// Base units of the relayer's token that each send's fee notes must pay it at least; with
    /// `providers`, its gas at the quoted rate if more.
    pub fee: u64,
    pub max_gas_limit: u64,
    pub max_gas_price_wei: u64,
    /// The SQLite file each send is recorded in before it is broadcast.
    pub journal: PathBuf,
}

#[derive(Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DepositsConfig {
    /// The Deposits contract: its token must be the relayer's, its Railgun the settlement's.
    pub contract: Address,
    /// Token base units each deposit's fee keeps at least before its percentage: the floor, and
    /// what a rescue pays.
    pub fee: u64,
    /// With `providers`, the gas the fee pays for, priced at the gas price and ETH price when it
    /// is quoted: a first deposit from an address, its vault made.
    #[serde(default)]
    pub fee_gas: u64,
    /// The fee's part of what is deposited, in basis points, on top of `fee`.
    #[serde(default = "default_deposit_fee_bps")]
    pub fee_bps: u32,
    pub max_gas_limit: u64,
    pub max_gas_price_wei: u64,
    /// The least a deposit moves, in token base units.
    pub min_value: u64,
    /// The latest an authorization's `validBefore` may be, in seconds from now.
    #[serde(default = "default_deposit_max_validity")]
    pub max_validity: u64,
    /// The most deposits sent and not yet mined at once: 1 with `private_rpc`.
    #[serde(default = "default_deposit_max_pending")]
    pub max_pending: u64,
    /// The SQLite file each deposit is recorded in before it is broadcast.
    pub journal: PathBuf,
    /// Flashbots Protect's RPC, to send deposits through instead of the public mempool, where an
    /// address could see its deposit coming and make it fail, leaving the relayer the gas. It
    /// lands a deposit only if it succeeds, and drops one it couldn't land within its URL's
    /// `blockRange` blocks (25 if none); every later nonce would wait on that one, so deposits go
    /// one at a time. Required on mainnet.
    #[serde(default)]
    pub private_rpc: Option<String>,
}

fn default_top_up_transactions() -> u64 {
    8
}

fn default_deposit_fee_bps() -> u32 {
    10
}

fn default_deposit_max_validity() -> u64 {
    24 * 60 * 60
}

fn default_deposit_max_pending() -> u64 {
    8
}

impl Config {
    pub fn load(path: &Path) -> anyhow::Result<Self> {
        let text =
            std::fs::read_to_string(path).with_context(|| format!("reading {}", path.display()))?;
        let config: Self =
            toml::from_str(&text).with_context(|| format!("parsing {}", path.display()))?;
        config.check_reveals()?;
        Ok(config)
    }

    /// Reveals go through a private RPC that keeps them to itself until they land, and on
    /// mainnet through nothing else.
    fn check_reveals(&self) -> anyhow::Result<()> {
        match &self.reveals {
            None => ensure!(
                self.network != Network::Mainnet,
                "on mainnet claims and refunds go through a revert-protecting private RPC: set [reveals] private_rpc"
            ),
            Some(reveals) => zecswap_chain::evm::check_keeps_secrets(&reveals.private_rpc)
                .context("[reveals] private_rpc")?,
        }
        Ok(())
    }

    /// The funding sponsorship's policy, if enabled, for the relayer's own token and maker,
    /// through Relay Adapt on `chain_id`.
    fn funding_policy(
        &self,
        account: Address,
        chain_id: u64,
    ) -> anyhow::Result<Option<FundingPolicy>> {
        let Some(funding) = &self.reverse_funding else {
            return Ok(None);
        };
        let network = self.network;
        let relay_adapt = match (funding.relay_adapt, network.relay_adapt(chain_id)) {
            (Some(given), Some(railguns)) => {
                ensure!(
                    given == railguns,
                    "relay_adapt {given} is not Railgun's {railguns} on {network}"
                );
                given
            }
            (given, railguns) => given.or(railguns).with_context(|| {
                format!("name relay_adapt: {network} has no Railgun of its own on chain {chain_id}")
            })?,
        };
        Ok(Some(FundingPolicy {
            relay_adapt,
            token: self.token,
            maker: self.maker,
            max_gas_limit: funding.max_gas_limit,
            max_gas_price_wei: funding.max_gas_price_wei.into(),
            fee: funding.fee.into(),
            fee_recipient: account,
        }))
    }
}

#[derive(Debug, thiserror::Error)]
pub enum RelayerError {
    #[error("{0}")]
    Rejected(String),
    /// A note the transaction spends is spent, or a transaction already sent spends it: those of
    /// the relayer's own that do, when it knows them.
    #[error("these notes are already spent, or a transaction already sent spends them")]
    Spent(Vec<B256>),
    /// A send was attempted and its outcome is unknown, or one is still to be mined, or the
    /// private RPC dropped it unmined: the same request is to be posted again.
    #[error("{0}")]
    Unsettled(&'static str),
    /// A send of these notes reverted lately: the same request is to be posted again in this
    /// many seconds.
    #[error("a send of these notes reverted lately; post the same request again in {0} seconds")]
    Reverted(u64),
    /// Sends are priced by gas, and gas can't be priced now: the same request is to be posted
    /// again later.
    #[error("the relayer can't price gas right now; post the same request again later")]
    Unpriced,
    /// Not now, nothing sent: the same request is to be posted again in this many seconds.
    #[error("{0}; post the same request again in {1} seconds")]
    Busy(&'static str, u64),
    /// What the request asks is done, or under way in these transactions of the relayer's.
    #[error("{1}")]
    Conflict(Vec<B256>, &'static str),
    #[error(transparent)]
    Internal(#[from] anyhow::Error),
}

impl From<zecswap_chain::Error> for RelayerError {
    fn from(e: zecswap_chain::Error) -> Self {
        match e {
            // Sent, and it may still land: the same request, posted again, learns how it went.
            zecswap_chain::Error::Unconfirmed(_) => RelayerError::Unsettled(turns::UNMINED),
            // It never landed and now can't: the same request, posted again, sends it anew.
            zecswap_chain::Error::Dropped(_) => RelayerError::Unsettled(turns::DROPPED),
            // Nothing was sent, the share still secret, and the window only runs down.
            e @ zecswap_chain::Error::Late(_) => RelayerError::Rejected(e.to_string()),
            e => RelayerError::Internal(e.into()),
        }
    }
}

impl From<rusqlite::Error> for RelayerError {
    fn from(e: rusqlite::Error) -> Self {
        RelayerError::Internal(e.into())
    }
}

impl RelayerError {
    /// What kind of failure this is, for the monitor; none for a refusal.
    fn failure_kind(&self) -> Option<&'static str> {
        match self {
            RelayerError::Rejected(_)
            | RelayerError::Spent(_)
            | RelayerError::Reverted(_)
            | RelayerError::Busy(..)
            | RelayerError::Conflict(..) => None,
            RelayerError::Unsettled(_) => Some("unconfirmed"),
            RelayerError::Unpriced => Some("internal"),
            RelayerError::Internal(error) => Some(
                error
                    .downcast_ref()
                    .map_or("internal", monitor::failure_kind),
            ),
        }
    }
}

type Result<T> = std::result::Result<T, RelayerError>;

pub struct Relayer {
    config: Config,
    account: Address,
    domain: Domain,
    settlement: Settlement,
    lanes: Lanes,
    /// The contract's window lengths, which every reveal is judged by.
    windows: Windows,
    turns: Turns,
    steps: Steps,
    monitor: monitor::Monitor,
    sends: Option<sends::Sends>,
    deposits: Option<deposits::Deposits>,
    /// With `reverse_funding`: what a funding must be, its fee the floor.
    funding: Option<FundingPolicy>,
    /// With `providers`: every fee priced by gas.
    pricing: Option<pricing::GasPricing>,
    /// Alchemy's price history, from `ALCHEMY_API_KEY`: it values what each send cost and
    /// earned. Without it they are recorded unvalued.
    history: Option<zecswap_prices::History>,
}

impl Relayer {
    /// `railgun` holds the relayer's own Railgun keys, which `railgun_sends` needs: never a
    /// maker's.
    pub async fn new(
        config: Config,
        key: PrivateKeySigner,
        railgun: Option<Keys>,
    ) -> anyhow::Result<Self> {
        let account = key.address();
        config.check_reveals()?;
        ensure!(
            config.top_up_transactions > 0,
            "top_up_transactions must be positive"
        );
        if let Some(alerts) = &config.gas_alerts {
            alerts.check()?;
            ensure!(
                config.railgun_sends.is_some(),
                "gas_alerts queue their messages in the railgun_sends journal"
            );
        }
        info!(endpoint = %endpoint(&config.evm_rpc), "settlement RPC");
        info!(
            private = ?config.reveals.as_ref().map(|reveals| endpoint(&reveals.private_rpc)),
            claim_margin = config.claim_margin,
            "claims and refunds"
        );
        let meter = RpcMeter::default();
        let lanes = Lanes::of(&config, &key, &meter).await?;
        let settlement =
            Settlement::connect_metered(&config.evm_rpc, config.contract, key, &meter)?;
        let domain = Domain {
            chain_id: settlement.chain_id().await?,
            contract: config.contract.into(),
        };
        let network = config.network;
        let proxy = settlement.railgun().await?;
        network
            .check(domain.chain_id, proxy, config.token)
            .with_context(|| format!("the relayer's config names {network}"))?;
        let windows = settlement
            .windows()
            .await
            .context("the relayer's contract must be the windows contract")?;
        let funding = config.funding_policy(account, domain.chain_id)?;
        if let Some(policy) = &funding {
            policy.validate_config(account)?;
            settlement.check_funding_adapter(policy.relay_adapt).await?;
        }
        let sends = match (&config.railgun_sends, railgun) {
            (None, _) => None,
            (Some(_), None) => anyhow::bail!("railgun_sends needs the relayer's Railgun keys"),
            (Some(sends), Some(keys)) => {
                ensure!(
                    config.maker != account,
                    "a relayer sending Railgun transactions must not be the maker"
                );
                ensure!(
                    sends.max_gas_limit > 0 && sends.max_gas_price_wei > 0,
                    "railgun_sends gas caps must be positive"
                );
                ensure!(!proxy.is_zero(), "the contract pays into no Railgun");
                let policy = SendPolicy {
                    railgun: proxy,
                    token: config.token,
                    fee: sends.fee.into(),
                    max_gas_limit: sends.max_gas_limit,
                    max_gas_price_wei: sends.max_gas_price_wei.into(),
                };
                let telegram = zecswap_alerts::Telegram::from_env()?;
                info!(railgun = %keys.address(), %proxy, alerts = telegram.enabled(), "sending Railgun transactions");
                Some(sends::Sends::open(policy, keys, &sends.journal, telegram)?)
            }
        };
        let deposits = match &config.deposits {
            Some(deposits) => Some(
                deposits::Deposits::open(
                    deposits,
                    &config,
                    &settlement,
                    lanes.deposits.account().context("a lane sends")?,
                    proxy,
                    zecswap_alerts::Telegram::from_env()?,
                )
                .await?,
            ),
            None => None,
        };
        let pricing = (!config.providers.is_empty())
            .then(|| {
                pricing::GasPricing::new(
                    &config.providers,
                    &zecswap_prices::Keys::from_env(),
                    config.fee_margin_bps,
                )
            })
            .transpose()?;
        info!(gas_priced = pricing.is_some(), "fees");
        let history = std::env::var("ALCHEMY_API_KEY")
            .ok()
            .filter(|key| !key.trim().is_empty())
            .map(|key| zecswap_prices::History::new(&zeroize::Zeroizing::new(key)))
            .transpose()?;
        if (sends.is_some() || deposits.is_some()) && history.is_none() {
            warn!("ALCHEMY_API_KEY is not set: send costs are recorded without USD values");
        }
        let steps = Steps::default();
        steps
            .note_earlier(&[&settlement, &lanes.claims, &lanes.locks, &lanes.funding])
            .await?;
        let relayer = Self {
            config,
            account,
            domain,
            settlement,
            lanes,
            windows,
            turns: Turns::default(),
            steps,
            monitor: monitor::Monitor::new(MonitorToken::from_env("RELAYER_MONITOR_TOKEN")?),
            sends,
            deposits,
            funding,
            pricing,
            history,
        };
        info!(relayer = %account, lanes = ?relayer.lane_accounts(), "sending accounts");
        Ok(relayer)
    }

    pub fn listen(&self) -> SocketAddr {
        self.config.listen
    }

    /// Counts a request for `operation` by how it ended. The gas of what it sent is read from
    /// the receipts after the reply, so monitoring never delays a user's request; a receipt not
    /// read within a few seconds goes uncounted.
    pub fn observe(
        self: &Arc<Self>,
        operation: &'static str,
        result: Result<Sent>,
    ) -> Result<Sent> {
        match &result {
            Ok(sent) => {
                self.monitor.sent(operation, sent.transactions.len() as u64);
                let (relayer, transactions) = (Arc::clone(self), sent.transactions.clone());
                tokio::spawn(async move {
                    for tx in transactions {
                        let cost = relayer.settlement.transaction_cost(tx);
                        if let Ok(Ok(Some((gas, wei)))) =
                            tokio::time::timeout(Duration::from_secs(5), cost).await
                        {
                            relayer.monitor.burned(operation, gas, wei);
                        }
                    }
                });
            }
            Err(error) => match error.failure_kind() {
                None => self.monitor.refused(operation),
                Some(kind) => self.monitor.failed(operation, kind),
            },
        }
        result
    }

    /// What every account of the relayer's has asked the RPC.
    pub fn rpc_meter(&self) -> RpcMeter {
        self.settlement.meter().clone()
    }

    pub fn monitor_snapshot(&self) -> monitor::MonitorSnapshot {
        self.monitor.snapshot(monitor::Deployment {
            rpc: self
                .settlement
                .meter()
                .report(endpoint(&self.config.evm_rpc)),
            relayer: self.account,
            accounts: self.lane_accounts(),
            chain_id: self.domain.chain_id,
            contract: self.config.contract,
            token: self.config.token,
            maker: self.config.maker,
            fees: monitor::Fees {
                payout: self.config.fee.to_string(),
                funding: self
                    .config
                    .reverse_funding
                    .as_ref()
                    .map(|funding| funding.fee.to_string()),
                sends: self
                    .sends
                    .as_ref()
                    .map(|sends| sends.policy.fee.to_string()),
                deposits: self
                    .config
                    .deposits
                    .as_ref()
                    .map(|deposits| deposits.fee.to_string()),
            },
            gas_alerts: self.config.gas_alerts.as_ref().map(|alerts| {
                let enabled = self
                    .sends
                    .as_ref()
                    .is_some_and(|sends| sends.telegram.enabled());
                alerts.status(enabled, "Relayer", self.account)
            }),
        })
    }

    /// What the relayer charges and offers now. Where it prices gas, asking quotes each fee at
    /// the gas price and ETH price now, and a quote is honored for a while to whatever was
    /// signed or proved with it.
    pub async fn terms(&self) -> Terms {
        let now = sends::now();
        let (fee, priced) = self.quote_swap_fee(SwapFee::Payout, now).await;
        let funding = match self.config.reverse_funding {
            Some(_) => Some(self.quote_swap_fee(SwapFee::Funding, now).await),
            None => None,
        };
        let deposit = match self.deposits {
            Some(_) => Some(self.quote_swap_fee(SwapFee::Deposit, now).await),
            None => None,
        };
        let rate = match (&self.sends, &self.pricing) {
            (Some(_), Some(pricing)) => pricing.quote_rate(now).await,
            _ => None,
        };
        let swap_fees_priced = priced || funding.is_some_and(|(_, priced)| priced);
        Terms {
            relayer: self.account,
            chain_id: self.domain.chain_id,
            contract: self.config.contract,
            fee,
            fee_expires_at: swap_fees_priced.then_some(now + pricing::SWAP_FEE_VALIDITY),
            reverse_funding: self
                .funding
                .as_ref()
                .zip(funding)
                .map(
                    |(policy, (fee, _))| zecswap_api::relayer::ReverseFundingTerms {
                        relay_adapt: policy.relay_adapt,
                        token: policy.token,
                        maker: policy.maker,
                        max_gas_limit: policy.max_gas_limit,
                        max_gas_price_wei: policy.max_gas_price_wei,
                        max_calldata_bytes: MAX_CALLDATA_BYTES,
                        fee,
                    },
                ),
            railgun_sends: self.sends.as_ref().map(|sends| {
                zecswap_api::relayer::RailgunSendTerms {
                    railgun_address: sends.keys.address(),
                    railgun_proxy: sends.policy.railgun,
                    token: sends.policy.token,
                    fee: sends.policy.fee,
                    fee_per_unit_gas: rate,
                    fee_expires_at: rate.map(|_| now + pricing::SEND_FEE_VALIDITY),
                    max_gas_limit: sends.policy.max_gas_limit,
                    max_gas_price_wei: sends.policy.max_gas_price_wei,
                    max_calldata_bytes: MAX_CALLDATA_BYTES,
                }
            }),
            deposits: self
                .deposits
                .as_ref()
                .zip(deposit)
                .map(|(deposits, (fee, priced))| {
                    deposits.terms(fee, priced.then_some(now + pricing::SEND_FEE_VALIDITY))
                }),
        }
    }

    /// A swap fee's floor and the gas it pays for.
    fn swap_fee(&self, fee: SwapFee) -> (u128, u64) {
        match fee {
            SwapFee::Payout => (self.config.fee.into(), self.config.fee_gas),
            SwapFee::Funding => self
                .config
                .reverse_funding
                .as_ref()
                .map_or((0, 0), |funding| (funding.fee.into(), funding.fee_gas)),
            SwapFee::Deposit => self
                .config
                .deposits
                .as_ref()
                .map_or((0, 0), |deposits| (deposits.fee.into(), deposits.fee_gas)),
        }
    }

    /// The gas price now, read at most once a minute; none where it isn't needed or can't be
    /// read.
    async fn gas_price(&self, pricing: &pricing::GasPricing, gas: u64, now: u64) -> Option<u128> {
        if gas == 0 {
            return None;
        }
        if let Some(price) = pricing.cached_gas_price(now) {
            return Some(price);
        }
        match self.settlement.gas_price().await {
            Ok(price) => {
                pricing.cache_gas_price(now, price);
                Some(price)
            }
            Err(e) => {
                warn!("could not read the gas price: {e:#}");
                None
            }
        }
    }

    /// A swap fee now, quoted, and whether gas priced it.
    async fn quote_swap_fee(&self, fee: SwapFee, now: u64) -> (u128, bool) {
        let (floor, gas) = self.swap_fee(fee);
        let Some(pricing) = &self.pricing else {
            return (floor, false);
        };
        let gas_price = self.gas_price(pricing, gas, now).await;
        pricing.quote_fee(fee, now, floor, gas, gas_price).await
    }

    /// The least a swap fee signed or proved now must pay.
    async fn honored_swap_fee(&self, fee: SwapFee) -> u128 {
        let (floor, gas) = self.swap_fee(fee);
        let Some(pricing) = &self.pricing else {
            return floor;
        };
        let now = sends::now();
        let gas_price = self.gas_price(pricing, gas, now).await;
        pricing.honored_fee(fee, now, floor, gas, gas_price).await
    }

    /// Reveals the user share, then pays out. Everything the payout needs is checked before the
    /// reveal, and the claim's window; if the payout still fails, the swap stays claimed and
    /// `payout` can be retried. A claim posted again once the share is revealed pays out what is
    /// still to be.
    #[tracing::instrument(skip_all, fields(swap_id = %request.swap_id, operation = "claim"), err(level = "warn"))]
    pub async fn claim(&self, request: Claim) -> Result<Sent> {
        if request.payout.swap_id != request.swap_id || request.payout.terms != request.terms {
            return Err(RelayerError::Rejected(
                "the claim and its payout name different swaps or terms".into(),
            ));
        }
        let (swap, terms, head) = self.railgun_swap(request.swap_id, &request.terms).await?;
        let secret = SecretShare::from_be_bytes(&request.secret.0)
            .map_err(|e| RelayerError::Rejected(e.to_string()))?;
        if secret.public() != swap.user_share {
            return Err(RelayerError::Rejected("the share is not the swap's".into()));
        }
        match swap.stage {
            Stage::Claimed => return self.payout(request.payout).await,
            Stage::Open | Stage::Ready => {}
            Stage::Refunded => {
                return Err(RelayerError::Rejected(format!(
                    "the swap is {:?}",
                    swap.stage
                )));
            }
        }
        let note = self.check_payout(&swap, &request.payout).await?;
        self.check_claim_window(request.swap_id, &swap, &terms, head)
            .await?;
        if !self.settlement.railgun_accepts(swap.token).await? {
            return Err(RelayerError::Rejected(
                "Railgun is not accepting payouts now".into(),
            ));
        }
        let id = request.swap_id;
        let claims = &self.lanes.claims;
        let sent = self
            .send_once(id, Step::Claim, claims, claims.claim(id, &terms, &secret))
            .await;
        let claim = self
            .unless_landed(id, &terms, sent, |swap| swap.stage == Stage::Claimed)
            .await?;
        if let Some(tx) = claim {
            info!(id = %id, %tx, "claimed");
        }
        let mut transactions: Vec<B256> = claim.into_iter().collect();
        match self.send_payout(&request.payout, &terms, &note).await {
            Ok(tx) => transactions.extend(tx),
            Err(e) => {
                warn!(id = %request.swap_id, "the payout after the claim failed: {e:#}");
                self.monitor
                    .failed("payout", e.failure_kind().unwrap_or("internal"));
            }
        }
        Ok(Sent { transactions })
    }

    /// That the swap's claim window is open as of the newest block and leaves `claim_margin` by
    /// the later of its time and the wall clock, and that before `t0` the ready that opened it is
    /// `READY_DEPTH` blocks deep: a claim sent on a ready a reorganisation then drops lands
    /// reverted, its share public, with the swap the maker's to cancel.
    async fn check_claim_window(
        &self,
        id: B256,
        swap: &OnChainSwap,
        terms: &zecswap_core::Terms,
        head: Head,
    ) -> Result<()> {
        let deadlines = (swap.t0, swap.t1);
        let margin = self.config.claim_margin;
        match judge::forward_claim(
            self.windows,
            swap.stage,
            deadlines,
            &head,
            wall_now(),
            margin,
        ) {
            Ok(Go::Now) => Ok(()),
            Ok(Go::OnceReadyAt(block)) => {
                let then = self
                    .settlement
                    .swap_at(id, terms, BlockId::number(block))
                    .await?;
                if then.is_none_or(|then| then.stage != Stage::Ready) {
                    return Err(RelayerError::Busy(
                        "the swap's ready is not yet deep enough to claim on before t0",
                        30,
                    ));
                }
                Ok(())
            }
            Err(Refusal::MakersUntil(late)) => Err(RelayerError::Rejected(format!(
                "from t1 the swap is its maker's to refund; it can be claimed again from {late}"
            ))),
            Err(Refusal::TooSoon) => Err(RelayerError::Rejected(
                "the claim's window ends too soon to land a claim in; nothing was sent".into(),
            )),
            Err(_) => Err(RelayerError::Rejected(
                "the swap can be claimed once ready, or from t0".into(),
            )),
        }
    }

    #[tracing::instrument(skip_all, fields(swap_id = %request.swap_id, operation = "payout"), err(level = "warn"))]
    pub async fn payout(&self, request: Payout) -> Result<Sent> {
        let (swap, terms, _) = self.railgun_swap(request.swap_id, &request.terms).await?;
        if swap.stage != Stage::Claimed {
            return Err(RelayerError::Rejected(
                "nothing is waiting to be paid out".into(),
            ));
        }
        // A repeat, once paid out, sends nothing.
        if swap.paid_out {
            return Ok(Sent {
                transactions: vec![],
            });
        }
        let note = self.check_payout(&swap, &request).await?;
        Ok(Sent {
            transactions: self
                .send_payout(&request, &terms, &note)
                .await?
                .into_iter()
                .collect(),
        })
    }

    /// Shields what Railgun sent back to a swap's vault to the note the user signed for.
    #[tracing::instrument(skip_all, fields(swap_id = %request.swap_id, operation = "rescue"), err(level = "warn"))]
    pub async fn rescue(&self, request: zecswap_api::relayer::Rescue) -> Result<Sent> {
        let (swap, terms, _) = self.railgun_swap(request.swap_id, &request.terms).await?;
        if !swap.paid_out {
            return Err(RelayerError::Rejected("the swap has not paid out".into()));
        }
        let authorization = zecswap_core::RescueAuthorization {
            nonce: request.nonce,
            deadline: request.deadline,
        };
        if self.settlement.now().await? > request.deadline
            || self.settlement.rescue_nonce(request.swap_id).await? != request.nonce
        {
            return Err(RelayerError::Rejected(
                "rescue approval is expired or already consumed".into(),
            ));
        }
        self.check_fee(request.fee).await?;
        let note = ShieldNote::from(&request.note);
        let digest = self.domain.rescue(
            &request.swap_id,
            &note.commitment(),
            &self.account.into(),
            request.fee,
            authorization,
        );
        self.check_signed(&swap, &digest, &request.signature.0)?;
        let tx = self
            .settlement
            .rescue(
                request.swap_id,
                &terms,
                &note,
                request.fee,
                &request.signature.0,
                authorization,
            )
            .await?;
        info!(id = %request.swap_id, %tx, "shielded a returned payout again");
        Ok(Sent {
            transactions: vec![tx],
        })
    }

    /// Pays a claimed swap out: the transaction, or none where a payout sent before landed.
    async fn send_payout(
        &self,
        request: &Payout,
        terms: &zecswap_core::Terms,
        note: &ShieldNote,
    ) -> Result<Option<B256>> {
        let id = request.swap_id;
        let payout = self
            .settlement
            .payout(id, terms, note, request.fee, &request.signature.0);
        let sent = self
            .send_once(id, Step::Payout, &self.settlement, payout)
            .await;
        let tx = self
            .unless_landed(id, terms, sent, |swap| swap.paid_out)
            .await?;
        if let Some(tx) = tx {
            info!(id = %id, %tx, fee = request.fee, "paid out into Railgun");
        }
        Ok(tx)
    }

    /// What a send came to, unless it failed on a swap that now shows `landed`: then one sent
    /// before did it after the swap was read for this request, and nothing is to be sent.
    async fn unless_landed<T: Into<Option<B256>>>(
        &self,
        id: B256,
        terms: &zecswap_core::Terms,
        sent: std::result::Result<T, impl Into<RelayerError>>,
        landed: impl Fn(&OnChainSwap) -> bool,
    ) -> Result<Option<B256>> {
        match sent.map_err(Into::into) {
            Ok(tx) => Ok(tx.into()),
            Err(RelayerError::Internal(error)) => match self.settlement.swap(id, terms).await {
                Ok(Some(swap)) if landed(&swap) => Ok(None),
                _ => Err(RelayerError::Internal(error)),
            },
            Err(error) => Err(error),
        }
    }

    /// A payout that can land: to the committed note, for at least the relayer's fee and less
    /// than the amount, signed by the swap's user for this relayer.
    async fn check_payout(&self, swap: &OnChainSwap, request: &Payout) -> Result<ShieldNote> {
        let note = ShieldNote::from(&request.note);
        if swap.payout_note != Some(note.commitment().into()) {
            return Err(RelayerError::Rejected(
                "the note is not the one the swap pays".into(),
            ));
        }
        self.check_fee(request.fee).await?;
        if request.fee >= swap.amount {
            return Err(RelayerError::Rejected(format!(
                "a fee of {} leaves nothing to shield",
                request.fee
            )));
        }
        let digest = self
            .domain
            .payout(&request.swap_id, &self.account.into(), request.fee);
        self.check_signed(swap, &digest, &request.signature.0)?;
        Ok(note)
    }

    async fn check_fee(&self, fee: u128) -> Result<()> {
        let least = self.honored_swap_fee(SwapFee::Payout).await;
        if fee < least {
            return Err(RelayerError::Rejected(format!(
                "a fee of {fee} is less than the relayer's {least}"
            )));
        }
        Ok(())
    }

    fn check_signed(
        &self,
        swap: &OnChainSwap,
        digest: &[u8; 32],
        signature: &[u8; 65],
    ) -> Result<()> {
        if signer(digest, signature) != Some(swap.user.into()) {
            return Err(RelayerError::Rejected(
                "not signed by the swap's user".into(),
            ));
        }
        Ok(())
    }

    /// Only swaps that pay into Railgun, whose payout is the relayer's fee; with the newest block
    /// the swap is read as of.
    async fn railgun_swap(
        &self,
        id: B256,
        terms: &zecswap_api::Terms,
    ) -> Result<(OnChainSwap, zecswap_core::Terms, Head)> {
        let terms = zecswap_core::Terms::from(terms);
        self.admit(terms.token, terms.maker)?;
        let (swap, head) = self.swap(id, &terms).await?;
        if swap.payout_note.is_none() {
            return Err(RelayerError::Rejected(
                "the swap pays an account, not Railgun".into(),
            ));
        }
        Ok((swap, terms, head))
    }

    /// Only swaps of this relayer's token and maker: its fee in any other token could be
    /// worthless, and another maker's swaps aren't its gas to spend.
    fn admit(&self, token: [u8; 20], maker: [u8; 20]) -> Result<()> {
        if Address::from(token) != self.config.token || Address::from(maker) != self.config.maker {
            return Err(RelayerError::Rejected(
                "this relayer serves another token or maker".into(),
            ));
        }
        Ok(())
    }

    /// The swap as the chain has it, if `terms` are the ones it opened with, and the newest block
    /// it is read as of. Checked before anything is sent: the contract would revert a call
    /// carrying other terms, at our expense.
    async fn swap(&self, id: B256, terms: &zecswap_core::Terms) -> Result<(OnChainSwap, Head)> {
        match self.settlement.read(id, terms).await {
            Ok((Some(swap), head)) => Ok((swap, head)),
            Ok((None, _)) => Err(RelayerError::Rejected("no such swap".into())),
            Err(zecswap_chain::Error::WrongTerms(_)) => Err(RelayerError::Rejected(
                "the swap opened on other terms".into(),
            )),
            Err(e) => Err(e.into()),
        }
    }
}
