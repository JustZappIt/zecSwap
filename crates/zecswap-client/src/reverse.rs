use alloy_primitives::{Address, B256};
use anyhow::{Context, Result, ensure};
use rand::{rand_core::UnwrapErr, rngs::SysRng};
use serde::{Deserialize, Serialize};
use zecswap_api::reverse::{Authorization, Quote, QuoteRequest, Refund};
use zecswap_api::{Acceptance, Network, relayer};
use zecswap_chain::evm::{OnChainSwap, Settlement, Stage, reverse_funding_calls, reverse_swap_id};
use zecswap_chain::zcash::{AccountUuid, Lightwalletd, Wallet};
use zecswap_core::{
    Domain, JointAccount, Payout, SpendKey, SwapContext, Terms, UserSwapKeys, derive_user_keys,
};
use zecswap_railgun::Keys as RailgunKeys;
use zeroize::Zeroizing;

use crate::api::{MakerApi, RelayerApi};
use crate::user::RAILGUN_WALLET;

const SIGNATURE_TTL: u64 = 120;
const MAX_READY_WAIT: u64 = 24 * 60 * 60;

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ReverseSwap {
    pub index: u32,
    pub quote: Quote,
    pub swap_id: B256,
    /// The ticket the maker answered the accept with, once accepted.
    #[serde(default)]
    pub ticket: Option<String>,
    /// The latest deadline of a ready this user signed: until the chain is past it, or past `t0`,
    /// that ready may still land, and would revert a refund sent meanwhile with the user's share
    /// public, the maker's to claim with.
    #[serde(default)]
    pub ready_signed_until: Option<u64>,
    /// A refund was asked for: no ready is ever signed after it.
    #[serde(default)]
    pub refunding: bool,
}

/// A user with a wallet seed, which its swaps' keys derive from, refunded into the Railgun wallet
/// of its Railgun seed.
pub struct ReverseUser {
    seed: Zeroizing<Vec<u8>>,
    railgun: RailgunKeys,
    network: Network,
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
        network: Network,
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
        let (chain_id, railgun) = (
            self.settlement.chain_id().await?,
            self.settlement.railgun().await?,
        );
        self.network.check(chain_id, railgun, self.token)?;
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
            swap_id: reverse_swap_id(keys.auth.address().into(), &quote.terms.maker_share),
            quote,
            index,
            ticket: None,
            ready_signed_until: None,
            refunding: false,
        };
        self.verify_quote(&swap).await?;
        Ok(swap)
    }

    /// Persist `swap` again once accepted: it then holds the swap's ticket.
    pub async fn accept(&self, swap: &mut ReverseSwap) -> Result<()> {
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
            token_request: None,
        };
        let quote_id = swap.quote.terms.quote_id;
        let accepted = self
            .maker
            .accept_reverse(quote_id, swap.swap_id, &acceptance)
            .await?;
        ensure!(
            accepted.swap_id == swap.swap_id,
            "maker reported a different reverse swap"
        );
        swap.ticket = Some(accepted.ticket);
        Ok(())
    }

    /// As `User::collect_token`: the token handed back once the escrow is funded.
    pub async fn collect_token(&self, swap: &ReverseSwap) -> Result<bool> {
        self.maker.collect_reverse_token(swap.swap_id).await
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

    /// Second authorization: only after an independent sync of this swap's joint account, and
    /// never once a refund was asked for. The signature's deadline goes into `swap`, and `save`
    /// persists it, before the signature is handed out: a ready that may still land holds any
    /// refund back.
    pub async fn ready(
        &self,
        swap: &mut ReverseSwap,
        wallet: &mut Wallet,
        client: &mut Lightwalletd,
        account: AccountUuid,
        save: impl FnOnce(&ReverseSwap) -> Result<()>,
    ) -> Result<()> {
        ensure!(
            !swap.refunding,
            "a refund was asked for; the escrow is never readied after it"
        );
        let chain = self.state(swap).await?;
        if matches!(chain.stage, Stage::Ready | Stage::Claimed) {
            return Ok(());
        }
        ensure!(chain.stage == Stage::Open, "escrow cannot be readied");
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
        swap.ready_signed_until = Some(swap.ready_signed_until.unwrap_or(0).max(deadline));
        save(swap)?;
        self.relayer
            .ready_reverse(
                &Authorization {
                    swap_id: swap.swap_id,
                    terms: (&self.terms(swap)?).into(),
                    deadline,
                    signature: signature.into(),
                },
                swap.ticket.as_deref(),
            )
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

    /// Refunds the escrow into the committed note: an unready one once no ready signature of
    /// ours can land, a ready one from `t1`, judged on the escrow and the newest block read as of
    /// one block. Asking for it goes into `swap`, which `save` persists, before the share goes
    /// out: no ready is signed after it. Retry after each chain update.
    pub async fn refund(
        &self,
        swap: &mut ReverseSwap,
        save: impl FnOnce(&ReverseSwap) -> Result<()>,
    ) -> Result<()> {
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
            self.relayer
                .reverse_refund_payout(&payout, swap.ticket.as_deref())
                .await?;
            return Ok(());
        }
        let (chain, head) = self
            .settlement
            .read(swap.swap_id, &self.terms(swap)?)
            .await?;
        let chain = chain.context("reverse escrow is not on-chain")?;
        match chain.stage {
            Stage::Ready => ensure!(
                head.timestamp >= chain.t1,
                "a ready escrow is its maker's to claim until t1; refund it from t1"
            ),
            _ => {
                if let Some(until) = swap.ready_signed_until {
                    ensure!(
                        head.timestamp > until || head.timestamp >= chain.t0,
                        "a ready signed until {until} may still land; refund once the chain is past it"
                    );
                }
            }
        }
        swap.refunding = true;
        save(swap)?;
        self.relayer
            .refund_reverse(
                &Refund {
                    swap_id: swap.swap_id,
                    terms: escrow,
                    secret: keys.share.to_be_bytes().into(),
                    payout,
                },
                swap.ticket.as_deref(),
            )
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
            swap.swap_id == reverse_swap_id(quote.user, &quote.terms.maker_share),
            "wrong reverse swap id"
        );
        // Escrows on the lock contract deployed before follow its rules, not ours; and one funded
        // at its deadline must still leave the contract's minimum window before t0.
        let windows = self
            .settlement
            .windows()
            .await
            .context("the quote's contract is not the windows contract")?;
        ensure!(
            quote.funding_deadline < quote.ready_deadline
                && quote.ready_deadline < quote.refund_after
                && quote.ready_deadline - quote.funding_deadline >= windows.min
                && quote.refund_after - quote.ready_deadline >= windows.min
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
        Ok(derive_user_keys(
            &self.seed,
            self.network.zcash_type(),
            0,
            index,
        )?)
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
