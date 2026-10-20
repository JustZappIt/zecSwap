use std::time::{Duration, Instant};

use alloy_primitives::{Address, B256};
use anyhow::{Context as _, Result, bail, ensure};
use rand::{rand_core::UnwrapErr, rngs::SysRng};
use serde::{Deserialize, Serialize};
use zecswap_api::relayer::{self, Claim, Sent};
use zecswap_api::{Acceptance, Network, Quote};
use zecswap_chain::evm::{
    BlockId, OnChainSwap, Reveal, Settlement, Stage, Window, swap_id, wall_now,
};
use zecswap_core::{
    Domain, JointAccount, Payout, SpendKey, SwapContext, Terms, UserSwapKeys, derive_user_keys,
};
use zecswap_railgun::{Keys as RailgunKeys, ShieldNote};
use zeroize::Zeroizing;

use crate::api::{MakerApi, RelayerApi, refused};

/// Ten confirmations take about 12.5 minutes; a swap must leave room to get them before `t0`.
pub const MIN_TIME_TO_T0: u64 = 25 * 60;
/// Until `t0` an unresponsive maker holds the deposit hostage.
pub const MAX_TIME_TO_T0: u64 = 2 * 60 * 60;
/// Never reveal with less than this left in the claim's window: a claim landing after `t1`
/// reverts, its share public, with the swap the maker's to refund.
pub const CLAIM_MARGIN: u64 = 5 * 60;
/// Blocks deep a ready must be before a claim is revealed on it ahead of `t0`: one a
/// reorganisation drops would leave a claim sent on it reverted, its share public, the swap the
/// maker's to cancel.
pub const READY_DEPTH: u64 = 12;
/// How long a claim waits for its ready to be deep enough.
const DEPTH_WAIT: Duration = Duration::from_secs(10 * 60);
const CATCH_UP: Duration = Duration::from_secs(60);
/// How long a relayer request that may have been sent is watched for on the chain before it is
/// posted again.
const RELAY_RETRY: Duration = Duration::from_secs(15);
/// How long a relayer request goes on being posted while the chain doesn't show it: a payout
/// never expires, but a run should end some time.
const RELAY_PATIENCE: Duration = Duration::from_secs(30 * 60);
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
    /// The ticket the maker answered the accept with; none in a swap kept from before makers
    /// gave them.
    #[serde(default)]
    pub ticket: Option<String>,
}

/// How a user is paid, and who sends its transactions.
pub enum Route {
    /// To the account the settlement connection sends as, which claims and withdraws itself.
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
    network: Network,
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
        network: Network,
        mut settlement: Settlement,
        maker: MakerApi,
        token: Address,
        route: Route,
    ) -> Self {
        settlement.reveal_with_margin(CLAIM_MARGIN);
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
        // The maker checks its chain is its own network's too: a deposit on another Zcash
        // network than the maker's would never be seen.
        let railgun = self.settlement.railgun().await?;
        self.network.check(chain_id, railgun, self.token)?;
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
            ticket: Some(accepted.ticket),
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
        // Swaps on the lock contract deployed before follow its rules, not ours.
        self.settlement
            .windows()
            .await
            .context("the swap's contract is not the windows contract")?;
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

    /// Reveals the user share in the claim's window with time to spare, then pays the user. Safe
    /// to call again after an interruption at any point.
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
                self.claim_window(swap).await?;
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

        let ticket = swap.ticket.as_deref();
        // The transaction that paid, where this call's requests sent it.
        let mut paid = None;
        match chain.stage {
            Stage::Refunded => bail!("the maker refunded the swap"),
            Stage::Claimed if chain.paid_out => {
                return Ok(Paid {
                    amount: 0,
                    tx: None,
                });
            }
            // Revealed before an interruption: only the payout is left.
            Stage::Claimed => {}
            Stage::Open | Stage::Ready => {
                // A payout Railgun won't take would sit in the contract; better the swap
                // unwinds, which leaves the ZEC with the user.
                ensure!(
                    self.settlement.railgun_accepts(chain.token).await?,
                    "Railgun is not taking the payout now; the share stays secret"
                );
                let window = self.claim_window(swap).await?;
                let claim = Claim {
                    swap_id: swap.swap_id,
                    terms: swap_terms,
                    secret: keys.share.to_be_bytes().into(),
                    payout: payout.clone(),
                };
                // Posted again only while the window keeps the margin, as it did at first.
                let claimed = |chain: &OnChainSwap| chain.stage == Stage::Claimed;
                let until = match window {
                    Window::Until(end) => end.saturating_sub(CLAIM_MARGIN),
                    _ => u64::MAX,
                };
                let sent = self
                    .relayed(swap, || relayer.claim(&claim, ticket), claimed, until)
                    .await?;
                self.caught_up(swap, claimed).await?;
                // The relayer reveals first and pays out after; a payout that failed is retried.
                paid = sent
                    .filter(|sent| sent.transactions.len() == 2)
                    .and_then(|sent| last(&sent));
            }
        }
        if paid.is_none() {
            let paid_out = |chain: &OnChainSwap| chain.paid_out;
            let sent = self
                .relayed(swap, || relayer.payout(&payout, ticket), paid_out, u64::MAX)
                .await?;
            paid = sent.and_then(|sent| last(&sent));
        }
        self.caught_up(swap, |chain| chain.paid_out).await?;
        Ok(Paid {
            amount: chain.amount - terms.fee,
            tx: paid,
        })
    }

    /// Posts a relayer request until the chain shows what it does: what the relayer answered,
    /// none where the chain showed it first. A request that timed out or failed with a `5xx`
    /// may still land, and the relayer sends nothing twice, so it is watched for and then
    /// posted again, until chain time `until`; a refusal sent nothing, and ends it.
    async fn relayed<F: Future<Output = Result<Sent>>>(
        &self,
        swap: &UserSwap,
        post: impl Fn() -> F,
        landed: impl Fn(&OnChainSwap) -> bool,
        until: u64,
    ) -> Result<Option<Sent>> {
        let terms = self.terms(swap)?;
        let give_up = Instant::now() + RELAY_PATIENCE;
        loop {
            let failure = match post().await {
                Ok(sent) => return Ok(Some(sent)),
                Err(e) if refused(&e) => return Err(e),
                Err(e) => e,
            };
            let watched = Instant::now() + RELAY_RETRY;
            loop {
                if let Some(chain) = self.settlement.swap(swap.swap_id, &terms).await?
                    && landed(&chain)
                {
                    return Ok(None);
                }
                if Instant::now() >= watched {
                    break;
                }
                tokio::time::sleep(Duration::from_secs(2)).await;
            }
            if self.settlement.now().await? >= until || Instant::now() >= give_up {
                return Err(failure.context("the relayer's request never showed on the chain"));
            }
        }
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

    /// The claim's window, once it is open with `CLAIM_MARGIN` to spare and, before `t0`, the
    /// swap's ready is `READY_DEPTH` blocks deep, which it waits for: judged on the swap and the
    /// newest block read as of one block, by the later of its time and the wall clock.
    async fn claim_window(&self, swap: &UserSwap) -> Result<Window> {
        let terms = self.terms(swap)?;
        let windows = self.settlement.windows().await?;
        let give_up = Instant::now() + DEPTH_WAIT;
        loop {
            let (chain, head) = self.settlement.read(swap.swap_id, &terms).await?;
            let chain = chain.context("the swap is not on-chain")?;
            let deadlines = (chain.t0, chain.t1);
            let window =
                windows.window(Reveal::Claim, false, chain.stage, deadlines, head.timestamp);
            if window == Window::Shut {
                let late = chain.t1.saturating_add(windows.late);
                if head.timestamp >= chain.t1 {
                    bail!(
                        "from t1 the swap is its maker's to refund, which gives the deposit back; it can be claimed again from {late}"
                    );
                }
                bail!("the swap can be claimed once ready, or from t0");
            }
            ensure!(
                window.leaves(head.timestamp.max(wall_now()), CLAIM_MARGIN),
                "the claim's window ends too soon to claim in; the share stays secret"
            );
            if head.timestamp >= chain.t0 {
                return Ok(window);
            }
            let deep = BlockId::number(head.number.saturating_sub(READY_DEPTH - 1));
            let then = self.settlement.swap_at(swap.swap_id, &terms, deep).await?;
            if then.is_some_and(|then| then.stage == Stage::Ready) {
                return Ok(window);
            }
            ensure!(
                Instant::now() < give_up,
                "the swap's ready never got {READY_DEPTH} blocks deep"
            );
            tokio::time::sleep(Duration::from_secs(5)).await;
        }
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
        Ok(derive_user_keys(
            &self.seed,
            self.network.zcash_type(),
            0,
            index,
        )?)
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
