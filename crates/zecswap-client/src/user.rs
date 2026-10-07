use std::time::{Duration, Instant};

use alloy_primitives::{Address, B256};
use anyhow::{Context as _, Result, bail, ensure};
use rand::{rand_core::UnwrapErr, rngs::SysRng};
use serde::{Deserialize, Serialize};
use zcash_protocol::consensus::NetworkType;
use zecswap_api::relayer::{self, Claim, LockClaim, Sent};
use zecswap_api::{Acceptance, Quote};
use zecswap_chain::evm::{OnChainSwap, Settlement, Stage, swap_id};
use zecswap_core::{
    Domain, JointAccount, Payout, SpendKey, SwapContext, Terms, UserSwapKeys, derive_user_keys,
};
use zecswap_railgun::{Keys as RailgunKeys, ShieldNote};
use zeroize::Zeroizing;

use crate::api::{MakerApi, RelayerApi};

/// Ten confirmations take about 12.5 minutes; a swap must leave room to get them before `t0`.
pub const MIN_TIME_TO_T0: u64 = 25 * 60;
/// Until `t0` an unresponsive maker holds the deposit hostage.
pub const MAX_TIME_TO_T0: u64 = 2 * 60 * 60;
/// Never reveal under a claim lock with less than this left: the claim must land before it
/// lapses, or the maker gets the next turn knowing both halves.
pub const CLAIM_MARGIN: u64 = 5 * 60;
/// How long a relayer has to land a signed claim lock. It must stay under the contract's lock
/// duration, which makes a signature good for one lock only.
const LOCK_SIGNATURE_TTL: u64 = 2 * 60;
const CATCH_UP: Duration = Duration::from_secs(60);
/// Railgun's wallets open the first wallet of a seed.
pub(crate) const RAILGUN_WALLET: u32 = 0;

/// What a user keeps about a swap it accepted; its keys re-derive from the seed and index.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct UserSwap {
    pub index: u32,
    pub quote: Quote,
    pub swap_id: B256,
    /// The deadlines the maker reported opening with, which complete the swap's terms.
    pub t0: u64,
    pub t1: u64,
}

/// How a user is paid, and who sends its transactions.
pub enum Route {
    /// To the account the settlement connection sends as, which takes the claim lock, claims
    /// and withdraws itself.
    Account,
    /// Into the user's Railgun wallet, the one its Railgun seed opens. Each swap's own key
    /// signs, the relayer sends, and the user needs no account on the chain.
    Railgun {
        relayer: RelayerApi,
        /// The most the relayer may keep from a payout, in token base units.
        max_fee: u128,
    },
}

/// What a claim paid the user.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Paid {
    /// Token base units that left the contract for the user: withdrawn to the account, or
    /// shielded into Railgun before Railgun's own fee.
    pub amount: u128,
    /// The transaction that paid it; none if an earlier call already had.
    pub tx: Option<B256>,
}

/// A user with a wallet seed, which its swaps' keys derive from, paid to an account or into the
/// Railgun wallet of its Railgun seed.
pub struct User {
    seed: Zeroizing<Vec<u8>>,
    network: NetworkType,
    settlement: Settlement,
    maker: MakerApi,
    token: Address,
    route: Route,
    railgun: RailgunKeys,
    min_time_to_t0: u64,
}

impl User {
    pub fn new(
        seed: &[u8],
        railgun_seed: &[u8],
        network: NetworkType,
        settlement: Settlement,
        maker: MakerApi,
        token: Address,
        route: Route,
    ) -> Self {
        Self {
            seed: Zeroizing::new(seed.to_vec()),
            railgun: RailgunKeys::from_seed(railgun_seed, RAILGUN_WALLET),
            network,
            settlement,
            maker,
            token,
            route,
            min_time_to_t0: MIN_TIME_TO_T0,
        }
    }

    /// Accepts a `t0` as soon as `seconds` away, instead of `MIN_TIME_TO_T0`: enough for a
    /// deposit to get the confirmations its maker waits for, when that is fewer than 10.
    pub fn with_min_time_to_t0(mut self, seconds: u64) -> Self {
        self.min_time_to_t0 = seconds;
        self
    }

    pub fn settlement(&self) -> &Settlement {
        &self.settlement
    }

    /// The Railgun wallet payouts into Railgun go to.
    pub fn railgun(&self) -> &RailgunKeys {
        &self.railgun
    }

    /// Takes a quote for `units`, checks the maker's proof, and accepts it with the share at
    /// `index`. The index is spent from this call on, whatever its outcome.
    pub async fn open(&self, index: u32, units: u32) -> Result<UserSwap> {
        let keys = self.keys(index)?;
        let payout = self.payout(&keys)?;
        let quote = self
            .maker
            .quote(units, payout.user.into(), payout.note.map(B256::from))
            .await?;
        let chain_id = self.settlement.chain_id().await?;
        ensure!(quote.chain_id == chain_id, "the quote is for another chain");
        ensure!(
            quote.contract == self.settlement.contract() && quote.token == self.token,
            "the quote is for another contract or token"
        );
        let context = self.context(chain_id, quote.quote_id);
        context
            .verify_maker(&quote.maker_share, &quote.maker_proof)
            .context("the maker's share proof")?;

        let acceptance = Acceptance {
            user_share: keys.share.public(),
            user_proof: context.prove_user(
                &quote.maker_share,
                &keys.share,
                &payout,
                UnwrapErr(SysRng),
            ),
            viewing_keys: keys.viewing,
            token_request: None,
        };
        let swap_id = swap_id(quote.maker, &acceptance.user_share);
        let accepted = self
            .maker
            .accept(quote.quote_id, swap_id, &acceptance)
            .await?;
        ensure!(
            accepted.swap_id == swap_id,
            "the maker reported another swap"
        );
        Ok(UserSwap {
            index,
            quote,
            swap_id,
            t0: accepted.t0,
            t1: accepted.t1,
        })
    }

    /// Holds the token the swap hands back once paid into, where the maker takes tokens:
    /// whether it now holds it. Spend it later, not right away: a spend just after it comes
    /// back could be tied to this swap.
    pub async fn collect_token(&self, swap: &UserSwap) -> Result<bool> {
        self.maker.collect_token(swap.swap_id).await
    }

    /// Checks the swap as the contract records it and derives the deposit account from the
    /// on-chain shares. Nothing may be deposited unless this succeeds.
    pub async fn verify(&self, swap: &UserSwap) -> Result<JointAccount> {
        // Read against the terms we expect, so the chain vouches for every one of them: the
        // quoted maker, token, amount and share, our share and payout, and the deadlines.
        let chain = self.caught_up(swap, |_| true).await?;
        let keys = self.keys(swap.index)?;
        ensure!(chain.stage == Stage::Open, "the swap is {:?}", chain.stage);
        let now = self.settlement.now().await?;
        let min = self.min_time_to_t0;
        ensure!(
            (now + min..=now + MAX_TIME_TO_T0).contains(&chain.t0),
            "t0 is {}s away, outside {min}..={MAX_TIME_TO_T0}s",
            chain.t0.saturating_sub(now)
        );
        Ok(JointAccount::derive(
            &chain.maker_share,
            &chain.user_share,
            &keys.viewing,
        )?)
    }

    /// The swap as the chain has it; an error if it opened on terms other than `terms` gives.
    pub async fn state(&self, swap: &UserSwap) -> Result<OnChainSwap> {
        self.settlement
            .swap(swap.swap_id, &self.terms(swap)?)
            .await?
            .context("the swap is not on-chain")
    }

    /// The terms the swap must have opened with: the quote, our share and payout, and the
    /// deadlines the maker reported. Every call on the swap supplies them.
    pub fn terms(&self, swap: &UserSwap) -> Result<Terms> {
        let keys = self.keys(swap.index)?;
        let payout = self.payout(&keys)?;
        Ok(Terms {
            maker: swap.quote.maker.into(),
            token: self.token.into(),
            amount: swap.quote.amount,
            maker_share: swap.quote.maker_share,
            user_share: keys.share.public(),
            user: payout.user,
            t0: swap.t0,
            t1: swap.t1,
            payout_note: payout.note.unwrap_or_default(),
        })
    }

    /// Takes the claim lock, directly or through the relayer.
    pub async fn lock_claim(&self, swap: &UserSwap) -> Result<()> {
        let terms = self.terms(swap)?;
        match &self.route {
            Route::Account => {
                self.settlement.lock_claim(swap.swap_id, &terms).await?;
            }
            Route::Railgun { relayer, .. } => {
                let deadline = self.settlement.now().await? + LOCK_SIGNATURE_TTL;
                let digest = self.domain(swap).lock_claim(&swap.swap_id.0, deadline);
                let request = LockClaim {
                    swap_id: swap.swap_id,
                    terms: (&terms).into(),
                    deadline,
                    signature: self.keys(swap.index)?.auth.sign(&digest).into(),
                };
                relayer.lock_claim(&request).await?;
            }
        }
        Ok(())
    }

    /// Reveals the user share under a claim lock with time to spare, taking the lock if
    /// needed, then pays the user. Safe to call again after an interruption at any point.
    pub async fn claim(&self, swap: &UserSwap) -> Result<Paid> {
        match &self.route {
            Route::Account => self.claim_to_account(swap).await,
            Route::Railgun { relayer, max_fee } => {
                self.claim_into_railgun(swap, relayer, *max_fee).await
            }
        }
    }

    /// Once the maker has refunded, combines its revealed share with ours: the key that
    /// takes the deposit back.
    pub async fn refund_key(&self, swap: &UserSwap) -> Result<SpendKey> {
        let chain = self.state(swap).await?;
        ensure!(
            chain.stage == Stage::Refunded,
            "the swap is {:?}",
            chain.stage
        );
        let e = chain
            .revealed()?
            .context("a refunded swap stores the maker share")?;
        let keys = self.keys(swap.index)?;
        let joint = JointAccount::derive(&chain.maker_share, &chain.user_share, &keys.viewing)?;
        Ok(joint.spend_key(&e, &keys.share)?)
    }

    async fn claim_to_account(&self, swap: &UserSwap) -> Result<Paid> {
        let chain = self.state(swap).await?;
        let amount = match chain.stage {
            Stage::Refunded => bail!("the maker refunded the swap"),
            // Revealed before an interruption: withdraw whatever is still credited.
            Stage::Claimed => self.settlement.balance_of(chain.user, chain.token).await?,
            Stage::Open | Stage::Ready => {
                self.hold_claim_lock(swap, &chain).await?;
                let keys = self.keys(swap.index)?;
                self.settlement
                    .claim(swap.swap_id, &self.terms(swap)?, &keys.share)
                    .await?;
                chain.amount
            }
        };
        if amount == 0 {
            return Ok(Paid { amount, tx: None });
        }
        // Apart from the claim, so a paused token can delay the payout but not the reveal.
        let tx = self
            .settlement
            .withdraw(chain.token, amount, chain.user)
            .await?;
        Ok(Paid {
            amount,
            tx: Some(tx),
        })
    }

    async fn claim_into_railgun(
        &self,
        swap: &UserSwap,
        relayer: &RelayerApi,
        max_fee: u128,
    ) -> Result<Paid> {
        let chain = self.state(swap).await?;
        let terms = relayer.terms().await?;
        ensure!(
            terms.chain_id == swap.quote.chain_id && terms.contract == swap.quote.contract,
            "the relayer serves another deployment"
        );
        ensure!(
            terms.fee <= max_fee && terms.fee < chain.amount,
            "the relayer asks a fee of {}",
            terms.fee
        );
        let keys = self.keys(swap.index)?;
        let note = self.railgun.note(&keys.note_entropy)?;
        let digest = self
            .domain(swap)
            .payout(&swap.swap_id.0, &terms.relayer.into(), terms.fee);
        let swap_terms = zecswap_api::Terms::from(&self.terms(swap)?);
        let payout = relayer::Payout {
            swap_id: swap.swap_id,
            terms: swap_terms.clone(),
            note: (&note).into(),
            fee: terms.fee,
            signature: keys.auth.sign(&digest).into(),
        };

        let sent = match chain.stage {
            Stage::Refunded => bail!("the maker refunded the swap"),
            Stage::Claimed if chain.paid_out => {
                return Ok(Paid {
                    amount: 0,
                    tx: None,
                });
            }
            // Revealed before an interruption: only the payout is left.
            Stage::Claimed => relayer.payout(&payout).await?,
            Stage::Open | Stage::Ready => {
                // A payout Railgun won't take would sit in the contract; better the swap
                // unwinds, which leaves the ZEC with the user.
                ensure!(
                    self.settlement.railgun_accepts(chain.token).await?,
                    "Railgun is not taking the payout now; the share stays secret"
                );
                self.hold_claim_lock(swap, &chain).await?;
                let claim = Claim {
                    swap_id: swap.swap_id,
                    terms: swap_terms,
                    secret: keys.share.to_be_bytes().into(),
                    payout: payout.clone(),
                };
                let sent = relayer.claim(&claim).await?;
                self.caught_up(swap, |chain| chain.stage == Stage::Claimed)
                    .await?;
                // The relayer reveals first and pays out after; a payout that failed is retried.
                if sent.transactions.len() < 2 {
                    relayer.payout(&payout).await?
                } else {
                    sent
                }
            }
        };
        self.caught_up(swap, |chain| chain.paid_out).await?;
        Ok(Paid {
            amount: chain.amount - terms.fee,
            tx: last(&sent),
        })
    }

    /// The note payouts of the swap at `index` go to, when it pays into Railgun.
    pub fn payout_note(&self, index: u32) -> Result<ShieldNote> {
        Ok(self.railgun.note(&self.keys(index)?.note_entropy)?)
    }

    fn payout(&self, keys: &UserSwapKeys) -> Result<Payout> {
        Ok(match self.route {
            Route::Account => Payout {
                user: self
                    .settlement
                    .account()
                    .context("paying an account needs a connection that sends as it")?
                    .into(),
                note: None,
            },
            Route::Railgun { .. } => Payout {
                user: keys.auth.address(),
                note: Some(self.railgun.note(&keys.note_entropy)?.commitment()),
            },
        })
    }

    async fn hold_claim_lock(&self, swap: &UserSwap, chain: &OnChainSwap) -> Result<()> {
        let now = self.settlement.now().await?;
        if chain.claim_lock_until > now + CLAIM_MARGIN {
            return Ok(());
        }
        ensure!(
            chain.claim_lock_until <= now,
            "the claim lock lapses too soon to reveal under; claim once the next turn is ours"
        );
        self.lock_claim(swap).await?;
        self.caught_up(swap, |chain| chain.claim_lock_until > now)
            .await?;
        Ok(())
    }

    /// Reads the swap once our RPC node shows what `seen` expects: it can lag the node a
    /// transaction went through.
    async fn caught_up(
        &self,
        swap: &UserSwap,
        seen: impl Fn(&OnChainSwap) -> bool,
    ) -> Result<OnChainSwap> {
        let terms = self.terms(swap)?;
        let deadline = Instant::now() + CATCH_UP;
        loop {
            if let Some(chain) = self.settlement.swap(swap.swap_id, &terms).await?
                && seen(&chain)
            {
                return Ok(chain);
            }
            ensure!(
                Instant::now() < deadline,
                "our RPC node never showed the swap as expected"
            );
            tokio::time::sleep(Duration::from_secs(2)).await;
        }
    }

    fn keys(&self, index: u32) -> Result<UserSwapKeys> {
        Ok(derive_user_keys(&self.seed, self.network, 0, index)?)
    }

    fn domain(&self, swap: &UserSwap) -> Domain {
        Domain {
            chain_id: swap.quote.chain_id,
            contract: swap.quote.contract.into(),
        }
    }

    fn context(&self, chain_id: u64, quote_id: B256) -> SwapContext {
        SwapContext {
            chain_id,
            contract: self.settlement.contract().into(),
            quote_id: quote_id.0,
        }
    }
}

fn last(sent: &Sent) -> Option<B256> {
    sent.transactions.last().copied()
}
