use alloy_primitives::{Address, B256};
use anyhow::{Context, Result, ensure};
use rand::{rand_core::UnwrapErr, rngs::SysRng};
use serde::{Deserialize, Serialize};
use zecswap_api::reverse::{Authorization, Quote, QuoteRequest, Refund};
use zecswap_api::{Acceptance, relayer};
use zecswap_chain::evm::{OnChainSwap, Settlement, Stage, reverse_funding_calls, swap_id};
use zecswap_chain::zcash::{AccountUuid, Lightwalletd, Wallet};
use zecswap_core::{
    Domain, JointAccount, NetworkType, Payout, SpendKey, SwapContext, Terms, UserSwapKeys,
    derive_user_keys,
};
use zecswap_railgun::Keys as RailgunKeys;
use zeroize::Zeroizing;

use crate::api::{MakerApi, RelayerApi};
use crate::user::{CLAIM_MARGIN, RAILGUN_WALLET};

const SIGNATURE_TTL: u64 = 120;
const MAX_READY_WAIT: u64 = 24 * 60 * 60;

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ReverseSwap {
    pub index: u32,
    pub quote: Quote,
    pub swap_id: B256,
}

/// A user with a wallet seed, which its swaps' keys derive from, refunded into the Railgun wallet
/// of its Railgun seed.
pub struct ReverseUser {
    seed: Zeroizing<Vec<u8>>,
    railgun: RailgunKeys,
    network: NetworkType,
    settlement: Settlement,
    maker: MakerApi,
    relayer: RelayerApi,
    token: Address,
    max_fee: u128,
}

impl ReverseUser {
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        seed: &[u8],
        railgun_seed: &[u8],
        network: NetworkType,
        settlement: Settlement,
        maker: MakerApi,
        relayer: RelayerApi,
        token: Address,
        max_fee: u128,
    ) -> Self {
        Self {
            seed: Zeroizing::new(seed.to_vec()),
            railgun: RailgunKeys::from_seed(railgun_seed, RAILGUN_WALLET),
            network,
            settlement,
            maker,
            relayer,
            token,
            max_fee,
        }
    }

    /// Persist this result and the spent index before accepting or funding it.
    pub async fn quote(&self, index: u32, units: u32) -> Result<ReverseSwap> {
        let keys = self.keys(index)?;
        let note = self.railgun.note(&keys.note_entropy)?;
        let quote = self
            .maker
            .reverse_quote(&QuoteRequest {
                units,
                user: keys.auth.address().into(),
                refund_note: note.commitment().into(),
            })
            .await?;
        let swap = ReverseSwap {
            swap_id: swap_id(keys.auth.address().into(), &quote.terms.maker_share),
            quote,
            index,
        };
        self.verify_quote(&swap).await?;
        Ok(swap)
    }

    pub async fn accept(&self, swap: &ReverseSwap) -> Result<()> {
        self.verify_quote(swap).await?;
        let keys = self.keys(swap.index)?;
        let acceptance = Acceptance {
            user_share: keys.share.public(),
            user_proof: self.context(swap).prove_user(
                &swap.quote.terms.maker_share,
                &keys.share,
                &Payout {
                    user: keys.auth.address(),
                    note: Some(swap.quote.refund_note.0),
                },
                UnwrapErr(SysRng),
            ),
            viewing_keys: keys.viewing,
        };
        ensure!(
            self.maker
                .accept_reverse(swap.quote.terms.quote_id, &acceptance)
                .await?
                .swap_id
                == swap.swap_id,
            "maker reported a different reverse swap"
        );
        Ok(())
    }

    /// Import before funding and persist the account/birthday so an interrupted scan resumes.
    pub async fn watch_deposit(
        &self,
        swap: &ReverseSwap,
        wallet: &mut Wallet,
        client: &mut Lightwalletd,
    ) -> Result<AccountUuid> {
        self.verify_quote(swap).await?;
        Ok(wallet
            .import_joint(
                client,
                &self.joint(swap)?,
                &format!("reverse {}", swap.swap_id),
            )
            .await?)
    }

    /// First authorization: approve + open, inside one successful Relay Adapt transaction.
    pub async fn funding_calls(&self, swap: &ReverseSwap) -> Result<[(Address, Vec<u8>); 2]> {
        self.verify_quote(swap).await?;
        ensure!(
            self.settlement
                .swap(swap.swap_id, &self.terms(swap)?)
                .await?
                .is_none(),
            "reverse escrow already exists"
        );
        let now = self.settlement.now().await?;
        ensure!(now < swap.quote.funding_deadline, "funding deadline passed");
        ensure!(
            swap.quote.ready_deadline > now.saturating_add(crate::MIN_TIME_TO_T0)
                && swap.quote.refund_after <= now.saturating_add(MAX_READY_WAIT),
            "reverse deadlines leave too little time or lock funds too long"
        );
        let keys = self.keys(swap.index)?;
        let terms = swap.quote.open(keys.share.public());
        let signature = keys.auth.sign(&self.domain(swap).open_reverse(&terms));
        Ok(reverse_funding_calls(
            swap.quote.terms.contract,
            &terms,
            &signature,
        ))
    }

    /// Second authorization: only after an independent sync of this swap's joint account.
    pub async fn ready(
        &self,
        swap: &ReverseSwap,
        wallet: &mut Wallet,
        client: &mut Lightwalletd,
        account: AccountUuid,
    ) -> Result<()> {
        let chain = self.state(swap).await?;
        if matches!(chain.stage, Stage::Ready | Stage::Claimed) {
            return Ok(());
        }
        ensure!(
            chain.stage == Stage::Open && chain.refund_lock_until == 0,
            "escrow cannot be readied"
        );
        ensure!(
            wallet.tracks_joint(account, &self.joint(swap)?)?,
            "wrong deposit account"
        );
        wallet.sync(client).await?;
        ensure!(
            wallet.funds(account)?.spendable >= swap.quote.terms.deposit_zat,
            "ZEC deposit is not confirmed in full"
        );
        let now = self.settlement.now().await?;
        ensure!(
            now.saturating_add(SIGNATURE_TTL) < chain.t0,
            "too little time left to authorize ready"
        );
        let deadline = now + SIGNATURE_TTL;
        let signature = self
            .keys(swap.index)?
            .auth
            .sign(&self.domain(swap).ready(&swap.swap_id, deadline));
        self.relayer
            .ready_reverse(&Authorization {
                swap_id: swap.swap_id,
                terms: (&self.terms(swap)?).into(),
                deadline,
                signature: signature.into(),
            })
            .await?;
        Ok(())
    }

    /// The escrow as the chain has it; an error unless it holds the quoted terms and refunds
    /// to the quoted note.
    pub async fn state(&self, swap: &ReverseSwap) -> Result<OnChainSwap> {
        self.verify_quote(swap).await?;
        let chain = self
            .settlement
            .swap(swap.swap_id, &self.terms(swap)?)
            .await?
            .context("reverse escrow is not on-chain")?;
        let funding = self
            .settlement
            .reverse_funding(swap.swap_id)
            .await?
            .context("not a reverse escrow")?;
        ensure!(
            funding.refund_note == swap.quote.refund_note,
            "reverse escrow refunds to another note"
        );
        Ok(chain)
    }

    /// What `openReverse` commits the escrow to, from the quote and our share.
    pub fn terms(&self, swap: &ReverseSwap) -> Result<Terms> {
        let keys = self.keys(swap.index)?;
        Ok(swap.quote.open(keys.share.public()).terms())
    }

    pub async fn receive_key(&self, swap: &ReverseSwap) -> Result<SpendKey> {
        let chain = self.state(swap).await?;
        ensure!(
            chain.stage == Stage::Claimed,
            "maker has not released its share"
        );
        Ok(self.joint(swap)?.spend_key(
            &chain.revealed()?.context("maker share missing")?,
            &self.keys(swap.index)?.share,
        )?)
    }

    /// Retry after each chain update. The first call takes a lock; only a later observation reveals.
    pub async fn refund(&self, swap: &ReverseSwap) -> Result<()> {
        let chain = self.state(swap).await?;
        ensure!(
            chain.stage != Stage::Claimed,
            "swap already completed; sweep the ZEC"
        );
        if chain.stage == Stage::Refunded && chain.paid_out {
            return Ok(());
        }
        let terms = self.relayer.terms().await?;
        ensure!(
            terms.chain_id == swap.quote.terms.chain_id
                && terms.contract == swap.quote.terms.contract,
            "relayer serves another deployment"
        );
        ensure!(
            terms.relayer != swap.quote.terms.maker,
            "refund relayer must be separate from the maker"
        );
        ensure!(
            terms.fee <= self.max_fee && terms.fee < chain.amount,
            "refund fee exceeds the limit"
        );
        let keys = self.keys(swap.index)?;
        let note = self.railgun.note(&keys.note_entropy)?;
        let escrow = zecswap_api::Terms::from(&self.terms(swap)?);
        let payout = relayer::Payout {
            swap_id: swap.swap_id,
            terms: escrow.clone(),
            note: (&note).into(),
            fee: terms.fee,
            signature: keys
                .auth
                .sign(&self.domain(swap).refund_payout(
                    &swap.swap_id,
                    &terms.relayer.into(),
                    terms.fee,
                ))
                .into(),
        };
        if chain.stage == Stage::Refunded {
            self.relayer.reverse_refund_payout(&payout).await?;
            return Ok(());
        }
        let now = self.settlement.now().await?;
        if chain.refund_lock_until <= now {
            let deadline = now + SIGNATURE_TTL;
            self.relayer
                .lock_reverse_refund(&Authorization {
                    swap_id: swap.swap_id,
                    terms: escrow,
                    deadline,
                    signature: keys
                        .auth
                        .sign(&self.domain(swap).lock_refund(&swap.swap_id, deadline))
                        .into(),
                })
                .await?;
            return Ok(());
        }
        ensure!(
            chain.refund_lock_until > now.saturating_add(CLAIM_MARGIN),
            "refund lock expires too soon to reveal safely"
        );
        self.relayer
            .refund_reverse(&Refund {
                swap_id: swap.swap_id,
                terms: escrow,
                secret: keys.share.to_be_bytes().into(),
                payout,
            })
            .await?;
        Ok(())
    }

    async fn verify_quote(&self, swap: &ReverseSwap) -> Result<()> {
        let quote = &swap.quote;
        let keys = self.keys(swap.index)?;
        let note = self.railgun.note(&keys.note_entropy)?;
        ensure!(
            quote.terms.chain_id == self.settlement.chain_id().await?
                && quote.terms.contract == self.settlement.contract()
                && quote.terms.token == self.token,
            "quote serves another deployment"
        );
        ensure!(
            !quote.terms.maker.is_zero() && quote.terms.amount > 0 && quote.terms.deposit_zat > 0,
            "invalid quote amount or maker"
        );
        ensure!(
            quote.user == Address::from(keys.auth.address())
                && quote.refund_note == B256::from(note.commitment()),
            "quote uses another user or refund note"
        );
        ensure!(
            swap.swap_id == swap_id(quote.user, &quote.terms.maker_share),
            "wrong reverse swap id"
        );
        ensure!(
            quote.funding_deadline < quote.ready_deadline
                && quote.ready_deadline < quote.refund_after
                && quote.refund_after - quote.funding_deadline <= MAX_READY_WAIT,
            "invalid reverse deadlines"
        );
        self.context(swap)
            .verify_maker(&quote.terms.maker_share, &quote.terms.maker_proof)?;
        Ok(())
    }

    fn joint(&self, swap: &ReverseSwap) -> Result<JointAccount> {
        let keys = self.keys(swap.index)?;
        Ok(JointAccount::derive(
            &swap.quote.terms.maker_share,
            &keys.share.public(),
            &keys.viewing,
        )?)
    }

    fn keys(&self, index: u32) -> Result<UserSwapKeys> {
        Ok(derive_user_keys(&self.seed, self.network, 0, index)?)
    }
    fn domain(&self, swap: &ReverseSwap) -> Domain {
        Domain {
            chain_id: swap.quote.terms.chain_id,
            contract: swap.quote.terms.contract.into(),
        }
    }
    fn context(&self, swap: &ReverseSwap) -> SwapContext {
        SwapContext {
            chain_id: swap.quote.terms.chain_id,
            contract: swap.quote.terms.contract.into(),
            quote_id: swap.quote.terms.quote_id.0,
        }
    }
}
