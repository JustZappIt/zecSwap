//! A relayer for users with no account on the settlement chain: it sends what a swap's own key
//! signed, and the reveal, and keeps a fee from the payout. It runs with its own key, apart
//! from any maker: a maker that learned the user share before the claim landed could hold the
//! claim back until the lock lapses, then refund and race the user for the ZEC.

pub mod api;
#[cfg(test)]
mod funding_tests;
#[cfg(test)]
mod logging_tests;
mod reverse;

use std::net::SocketAddr;
use std::path::Path;

use alloy_primitives::{Address, B256};
use anyhow::Context as _;
use serde::Deserialize;
use tracing::{info, warn};
use zecswap_api::relayer::{Claim, LockClaim, Payout, Sent, Terms};
use zecswap_chain::evm::funding::{FundingPolicy, MAX_CALLDATA_BYTES};
use zecswap_chain::evm::{OnChainSwap, PrivateKeySigner, Settlement, Stage};
use zecswap_core::{Domain, SecretShare, signer};
use zecswap_railgun::ShieldNote;

#[derive(Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Config {
    pub evm_rpc: String,
    /// The ZecSwap settlement contract.
    pub contract: Address,
    pub listen: SocketAddr,
    /// Token base units kept from each payout, for the gas of the lock, the claim and the
    /// payout.
    pub fee: u64,
    /// Seconds a claim lock must still have when the relayer reveals under it: less, and the
    /// claim could land after the lock lapses, handing the maker the next turn knowing both
    /// halves.
    pub claim_margin: u64,
    /// Opt-in sponsorship of initial Railgun funding on Sepolia.
    #[serde(default)]
    pub reverse_funding: Option<ReverseFundingConfig>,
}

#[derive(Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ReverseFundingConfig {
    pub relay_adapt: Address,
    pub token: Address,
    pub maker: Address,
    pub max_gas_limit: u64,
    pub max_gas_price_wei: u64,
    /// Escrow-token base units each funding pays this relayer for its gas.
    pub fee: u64,
}

impl ReverseFundingConfig {
    fn policy(&self, account: Address) -> FundingPolicy {
        FundingPolicy {
            relay_adapt: self.relay_adapt,
            token: self.token,
            maker: self.maker,
            max_gas_limit: self.max_gas_limit,
            max_gas_price_wei: self.max_gas_price_wei.into(),
            fee: self.fee.into(),
            fee_recipient: account,
        }
    }
}

impl Config {
    pub fn load(path: &Path) -> anyhow::Result<Self> {
        let text =
            std::fs::read_to_string(path).with_context(|| format!("reading {}", path.display()))?;
        toml::from_str(&text).with_context(|| format!("parsing {}", path.display()))
    }
}

#[derive(Debug, thiserror::Error)]
pub enum RelayerError {
    #[error("{0}")]
    Rejected(String),
    #[error(transparent)]
    Internal(#[from] anyhow::Error),
}

impl From<zecswap_chain::Error> for RelayerError {
    fn from(e: zecswap_chain::Error) -> Self {
        RelayerError::Internal(e.into())
    }
}

type Result<T> = std::result::Result<T, RelayerError>;

pub struct Relayer {
    config: Config,
    account: Address,
    domain: Domain,
    settlement: Settlement,
}

impl Relayer {
    pub async fn new(config: Config, key: PrivateKeySigner) -> anyhow::Result<Self> {
        let account = key.address();
        let settlement = Settlement::connect(&config.evm_rpc, config.contract, key)?;
        let domain = Domain {
            chain_id: settlement.chain_id().await?,
            contract: config.contract.into(),
        };
        if let Some(funding) = &config.reverse_funding {
            funding
                .policy(account)
                .validate_config(domain.chain_id, account)?;
            settlement
                .check_funding_adapter(funding.relay_adapt)
                .await?;
        }
        Ok(Self {
            config,
            account,
            domain,
            settlement,
        })
    }

    pub fn listen(&self) -> SocketAddr {
        self.config.listen
    }

    pub fn terms(&self) -> Terms {
        Terms {
            relayer: self.account,
            chain_id: self.domain.chain_id,
            contract: self.config.contract,
            fee: self.config.fee.into(),
            reverse_funding: self.config.reverse_funding.as_ref().map(|funding| {
                zecswap_api::relayer::ReverseFundingTerms {
                    relay_adapt: funding.relay_adapt,
                    token: funding.token,
                    maker: funding.maker,
                    max_gas_limit: funding.max_gas_limit,
                    max_gas_price_wei: funding.max_gas_price_wei.into(),
                    max_calldata_bytes: MAX_CALLDATA_BYTES,
                    fee: funding.fee.into(),
                }
            }),
        }
    }

    #[tracing::instrument(skip_all, fields(swap_id = %request.swap_id, operation = "lock_claim"), err(level = "warn"))]
    pub async fn lock_claim(&self, request: LockClaim) -> Result<Sent> {
        let swap = self.railgun_swap(request.swap_id).await?;
        let digest = self.domain.lock_claim(&request.swap_id, request.deadline);
        self.check_signed(&swap, &digest, &request.signature.0)?;
        let tx = self
            .settlement
            .lock_claim_with_sig(request.swap_id, request.deadline, &request.signature.0)
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
        let swap = self.railgun_swap(request.swap_id).await?;
        if !matches!(swap.stage, Stage::Open | Stage::Ready) {
            return Err(RelayerError::Rejected(format!(
                "the swap is {:?}",
                swap.stage
            )));
        }
        let note = self.check_payout(&swap, &request.payout)?;
        let now = self.settlement.now().await?;
        if swap.claim_lock_until < now + self.config.claim_margin {
            return Err(RelayerError::Rejected(
                "no claim lock with enough time left to reveal under".into(),
            ));
        }
        let secret = SecretShare::from_be_bytes(&request.secret.0)
            .map_err(|e| RelayerError::Rejected(e.to_string()))?;
        let claim = self.settlement.claim(request.swap_id, &secret).await?;
        info!(id = %request.swap_id, tx = %claim, "claimed");
        let mut transactions = vec![claim];
        match self.send_payout(&request.payout, &note).await {
            Ok(tx) => transactions.push(tx),
            Err(e) => warn!(id = %request.swap_id, "the payout after the claim failed: {e:#}"),
        }
        Ok(Sent { transactions })
    }

    #[tracing::instrument(skip_all, fields(swap_id = %request.swap_id, operation = "payout"), err(level = "warn"))]
    pub async fn payout(&self, request: Payout) -> Result<Sent> {
        let swap = self.railgun_swap(request.swap_id).await?;
        if swap.stage != Stage::Claimed || swap.paid_out {
            return Err(RelayerError::Rejected(
                "nothing is waiting to be paid out".into(),
            ));
        }
        let note = self.check_payout(&swap, &request)?;
        Ok(Sent {
            transactions: vec![self.send_payout(&request, &note).await?],
        })
    }

    /// Shields what Railgun sent back to a swap's vault to the note the user signed for.
    #[tracing::instrument(skip_all, fields(swap_id = %request.swap_id, operation = "rescue"), err(level = "warn"))]
    pub async fn rescue(&self, request: zecswap_api::relayer::Rescue) -> Result<Sent> {
        let swap = self.railgun_swap(request.swap_id).await?;
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
        self.check_fee(request.fee)?;
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

    async fn send_payout(&self, request: &Payout, note: &ShieldNote) -> Result<B256> {
        let tx = self
            .settlement
            .payout(request.swap_id, note, request.fee, &request.signature.0)
            .await?;
        info!(id = %request.swap_id, %tx, fee = request.fee, "paid out into Railgun");
        Ok(tx)
    }

    /// A payout the contract will take: to the committed note, for at least the relayer's fee,
    /// signed by the swap's user for this relayer.
    fn check_payout(&self, swap: &OnChainSwap, request: &Payout) -> Result<ShieldNote> {
        let note = ShieldNote::from(&request.note);
        if swap.payout_note != Some(note.commitment().into()) {
            return Err(RelayerError::Rejected(
                "the note is not the one the swap pays".into(),
            ));
        }
        self.check_fee(request.fee)?;
        let digest = self
            .domain
            .payout(&request.swap_id, &self.account.into(), request.fee);
        self.check_signed(swap, &digest, &request.signature.0)?;
        Ok(note)
    }

    fn check_fee(&self, fee: u128) -> Result<()> {
        if fee < self.config.fee.into() {
            return Err(RelayerError::Rejected(format!(
                "a fee of {fee} is less than the relayer's {}",
                self.config.fee
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
    async fn railgun_swap(&self, id: B256) -> Result<OnChainSwap> {
        let swap = self
            .settlement
            .swap(id)
            .await?
            .ok_or_else(|| RelayerError::Rejected("no such swap".into()))?;
        if swap.payout_note.is_none() {
            return Err(RelayerError::Rejected(
                "the swap pays an account, not Railgun".into(),
            ));
        }
        Ok(swap)
    }
}
