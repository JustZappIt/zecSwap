//! A relayer for users with no account on the settlement chain: it sends what a swap's own key
//! signed, and the reveal, and keeps a fee from the payout. It runs with its own key, apart
//! from any maker: a maker that learned the user share before the claim landed could hold the
//! claim back until the lock lapses, then refund and race the user for the ZEC. With
//! `railgun_sends` it also sends wallets' own Railgun transactions as their broadcaster, paid
//! by a fee note to its own Railgun wallet (`sends`).

pub mod api;
#[cfg(test)]
mod funding_tests;
#[cfg(test)]
mod logging_tests;
mod monitor;
#[cfg(test)]
mod monitor_tests;
mod pricing;
mod reverse;
mod sends;
#[cfg(test)]
mod terms_tests;

use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use alloy_primitives::{Address, B256};
use anyhow::{Context as _, ensure};
use serde::Deserialize;
use tracing::{info, warn};
use zecswap_api::relayer::{Claim, LockClaim, Payout, Sent, Terms};
use zecswap_api::server::MonitorToken;
use zecswap_chain::evm::funding::FundingPolicy;
use zecswap_chain::evm::railgun::{MAX_CALLDATA_BYTES, SendPolicy};
use zecswap_chain::evm::{OnChainSwap, PrivateKeySigner, Settlement, Stage};
use zecswap_core::{Domain, SecretShare, signer};
use zecswap_railgun::{Keys, ShieldNote};

pub use sends::{Sending, SendsSnapshot};

use crate::pricing::SwapFee;

#[derive(Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Config {
    pub evm_rpc: String,
    /// The ZecSwap settlement contract.
    pub contract: Address,
    /// The one token whose swaps it sends for, in whose base units it takes its fees.
    pub token: Address,
    /// The one maker whose swaps it sends for.
    pub maker: Address,
    pub listen: SocketAddr,
    /// Token base units each payout keeps at least, for the gas of the lock, the claim and the
    /// payout: the floor of its fee.
    pub fee: u64,
    /// With `providers`, the gas a payout's fee pays for, priced at the gas price and ETH price
    /// when it is quoted: the claim lock, the claim and the payout, or a refund's lock, the
    /// refund and its payout. None leaves the fee at `fee`.
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
    /// Seconds a claim lock must still have when the relayer reveals under it: less, and the
    /// claim could land after the lock lapses, handing the maker the next turn knowing both
    /// halves.
    pub claim_margin: u64,
    /// Opt-in sponsorship of initial Railgun funding on Sepolia.
    #[serde(default)]
    pub reverse_funding: Option<ReverseFundingConfig>,
    /// Opt-in sending of wallets' own Railgun transactions, private sends and withdrawals, as
    /// their broadcaster, on whichever chain the contract is. Needs `RELAYER_RAILGUN_SEED`.
    #[serde(default)]
    pub railgun_sends: Option<RailgunSendsConfig>,
}

#[derive(Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ReverseFundingConfig {
    pub relay_adapt: Address,
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

impl Config {
    pub fn load(path: &Path) -> anyhow::Result<Self> {
        let text =
            std::fs::read_to_string(path).with_context(|| format!("reading {}", path.display()))?;
        toml::from_str(&text).with_context(|| format!("parsing {}", path.display()))
    }

    /// The funding sponsorship's policy, if enabled, for the relayer's own token and maker.
    fn funding_policy(&self, account: Address) -> Option<FundingPolicy> {
        self.reverse_funding.as_ref().map(|funding| FundingPolicy {
            relay_adapt: funding.relay_adapt,
            token: self.token,
            maker: self.maker,
            max_gas_limit: funding.max_gas_limit,
            max_gas_price_wei: funding.max_gas_price_wei.into(),
            fee: funding.fee.into(),
            fee_recipient: account,
        })
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
    /// A send was attempted and its outcome is unknown: the same request is to be posted again.
    #[error("{0}")]
    Unsettled(&'static str),
    /// Sends are priced by gas, and gas can't be priced now: the same request is to be posted
    /// again later.
    #[error("the relayer can't price gas right now; post the same request again later")]
    Unpriced,
    #[error(transparent)]
    Internal(#[from] anyhow::Error),
}

impl From<zecswap_chain::Error> for RelayerError {
    fn from(e: zecswap_chain::Error) -> Self {
        RelayerError::Internal(e.into())
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
            RelayerError::Rejected(_) | RelayerError::Spent(_) => None,
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
    monitor: monitor::Monitor,
    sends: Option<sends::Sends>,
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
        let settlement = Settlement::connect(&config.evm_rpc, config.contract, key)?;
        let domain = Domain {
            chain_id: settlement.chain_id().await?,
            contract: config.contract.into(),
        };
        if let Some(policy) = config.funding_policy(account) {
            policy.validate_config(domain.chain_id, account)?;
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
                let railgun = settlement.railgun().await?;
                ensure!(!railgun.is_zero(), "the contract pays into no Railgun");
                let policy = SendPolicy {
                    railgun,
                    token: config.token,
                    fee: sends.fee.into(),
                    max_gas_limit: sends.max_gas_limit,
                    max_gas_price_wei: sends.max_gas_price_wei.into(),
                };
                info!(railgun = %keys.address(), proxy = %railgun, "sending Railgun transactions");
                Some(sends::Sends::open(policy, keys, &sends.journal)?)
            }
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
        if sends.is_some() && history.is_none() {
            warn!("ALCHEMY_API_KEY is not set: send costs are recorded without USD values");
        }
        Ok(Self {
            config,
            account,
            domain,
            settlement,
            monitor: monitor::Monitor::new(MonitorToken::from_env("RELAYER_MONITOR_TOKEN")?),
            sends,
            pricing,
            history,
        })
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

    pub fn monitor_snapshot(&self) -> monitor::MonitorSnapshot {
        self.monitor.snapshot(monitor::Deployment {
            relayer: self.account,
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
            },
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
            reverse_funding: self.config.reverse_funding.as_ref().zip(funding).map(
                |(config, (fee, _))| zecswap_api::relayer::ReverseFundingTerms {
                    relay_adapt: config.relay_adapt,
                    token: self.config.token,
                    maker: self.config.maker,
                    max_gas_limit: config.max_gas_limit,
                    max_gas_price_wei: config.max_gas_price_wei.into(),
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
        }
    }

    /// The gas price now, read at most every fifteen seconds; none where it isn't needed or
    /// can't be read.
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

    #[tracing::instrument(skip_all, fields(swap_id = %request.swap_id, operation = "lock_claim"), err(level = "warn"))]
    pub async fn lock_claim(&self, request: LockClaim) -> Result<Sent> {
        let (swap, terms) = self.railgun_swap(request.swap_id, &request.terms).await?;
        let digest = self.domain.lock_claim(&request.swap_id, request.deadline);
        self.check_signed(&swap, &digest, &request.signature.0)?;
        let tx = self
            .settlement
            .lock_claim_with_sig(
                request.swap_id,
                &terms,
                request.deadline,
                &request.signature.0,
            )
            .await?;
        info!(id = %request.swap_id, %tx, "took the claim lock");
        Ok(Sent {
            transactions: vec![tx],
        })
    }

    /// Reveals the user share, then pays out. Everything the payout needs is checked before the
    /// reveal; if the payout still fails, the swap stays claimed and `payout` can be retried.
    #[tracing::instrument(skip_all, fields(swap_id = %request.swap_id, operation = "claim"), err(level = "warn"))]
    pub async fn claim(&self, request: Claim) -> Result<Sent> {
        if request.payout.swap_id != request.swap_id || request.payout.terms != request.terms {
            return Err(RelayerError::Rejected(
                "the claim and its payout name different swaps or terms".into(),
            ));
        }
        let (swap, terms) = self.railgun_swap(request.swap_id, &request.terms).await?;
        if !matches!(swap.stage, Stage::Open | Stage::Ready) {
            return Err(RelayerError::Rejected(format!(
                "the swap is {:?}",
                swap.stage
            )));
        }
        let note = self.check_payout(&swap, &request.payout).await?;
        let now = self.settlement.now().await?;
        if swap.claim_lock_until < now + self.config.claim_margin {
            return Err(RelayerError::Rejected(
                "no claim lock with enough time left to reveal under".into(),
            ));
        }
        if !self.settlement.railgun_accepts(swap.token).await? {
            return Err(RelayerError::Rejected(
                "Railgun is not accepting payouts now".into(),
            ));
        }
        let secret = SecretShare::from_be_bytes(&request.secret.0)
            .map_err(|e| RelayerError::Rejected(e.to_string()))?;
        let claim = self
            .settlement
            .claim(request.swap_id, &terms, &secret)
            .await?;
        info!(id = %request.swap_id, tx = %claim, "claimed");
        let mut transactions = vec![claim];
        match self.send_payout(&request.payout, &terms, &note).await {
            Ok(tx) => transactions.push(tx),
            Err(e) => {
                warn!(id = %request.swap_id, "the payout after the claim failed: {e:#}");
                self.monitor
                    .failed("payout", e.failure_kind().unwrap_or("internal"));
            }
        }
        Ok(Sent { transactions })
    }

    #[tracing::instrument(skip_all, fields(swap_id = %request.swap_id, operation = "payout"), err(level = "warn"))]
    pub async fn payout(&self, request: Payout) -> Result<Sent> {
        let (swap, terms) = self.railgun_swap(request.swap_id, &request.terms).await?;
        if swap.stage != Stage::Claimed || swap.paid_out {
            return Err(RelayerError::Rejected(
                "nothing is waiting to be paid out".into(),
            ));
        }
        let note = self.check_payout(&swap, &request).await?;
        Ok(Sent {
            transactions: vec![self.send_payout(&request, &terms, &note).await?],
        })
    }

    /// Shields what Railgun sent back to a swap's vault to the note the user signed for.
    #[tracing::instrument(skip_all, fields(swap_id = %request.swap_id, operation = "rescue"), err(level = "warn"))]
    pub async fn rescue(&self, request: zecswap_api::relayer::Rescue) -> Result<Sent> {
        let (swap, terms) = self.railgun_swap(request.swap_id, &request.terms).await?;
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

    async fn send_payout(
        &self,
        request: &Payout,
        terms: &zecswap_core::Terms,
        note: &ShieldNote,
    ) -> Result<B256> {
        let tx = self
            .settlement
            .payout(
                request.swap_id,
                terms,
                note,
                request.fee,
                &request.signature.0,
            )
            .await?;
        info!(id = %request.swap_id, %tx, fee = request.fee, "paid out into Railgun");
        Ok(tx)
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

    /// Only swaps that pay into Railgun, whose payout is the relayer's fee.
    async fn railgun_swap(
        &self,
        id: B256,
        terms: &zecswap_api::Terms,
    ) -> Result<(OnChainSwap, zecswap_core::Terms)> {
        let terms = zecswap_core::Terms::from(terms);
        self.admit(terms.token, terms.maker)?;
        let swap = self.swap(id, &terms).await?;
        if swap.payout_note.is_none() {
            return Err(RelayerError::Rejected(
                "the swap pays an account, not Railgun".into(),
            ));
        }
        Ok((swap, terms))
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

    /// The swap as the chain has it, if `terms` are the ones it opened with. Checked before
    /// anything is sent: the contract would revert a call carrying other terms, at our expense.
    async fn swap(&self, id: B256, terms: &zecswap_core::Terms) -> Result<OnChainSwap> {
        match self.settlement.swap(id, terms).await {
            Ok(Some(swap)) => Ok(swap),
            Ok(None) => Err(RelayerError::Rejected("no such swap".into())),
            Err(zecswap_chain::Error::WrongTerms(_)) => Err(RelayerError::Rejected(
                "the swap opened on other terms".into(),
            )),
            Err(e) => Err(e.into()),
        }
    }
}
