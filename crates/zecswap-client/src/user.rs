use std::time::{Duration, Instant};

use alloy_primitives::{Address, B256};
use anyhow::{Context as _, Result, bail, ensure};
use rand_core::OsRng;
use serde::{Deserialize, Serialize};
use zcash_protocol::consensus::NetworkType;
use zecswap_api::{Acceptance, Quote};
use zecswap_chain::base::{OnChainSwap, Settlement, Stage, swap_id};
use zecswap_core::{JointAccount, SpendKey, SwapContext, UserSwapKeys, derive_user_keys};
use zeroize::Zeroizing;

use crate::api::MakerApi;

/// Ten confirmations take about 12.5 minutes; a swap must leave room to get them before `t0`.
pub const MIN_TIME_TO_T0: u64 = 25 * 60;
/// Until `t0` an unresponsive maker holds the deposit hostage.
pub const MAX_TIME_TO_T0: u64 = 2 * 60 * 60;
/// Never reveal under a claim lock with less than this left: the claim must land before it
/// lapses, or the maker gets the next turn knowing both halves.
pub const CLAIM_MARGIN: u64 = 5 * 60;
const CATCH_UP: Duration = Duration::from_secs(60);

/// What a user keeps about a swap it accepted; its keys re-derive from the seed and index.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct UserSwap {
    pub index: u32,
    pub quote: Quote,
    pub swap_id: B256,
}

/// A user with a wallet seed and a payout account on Base.
pub struct User {
    seed: Zeroizing<Vec<u8>>,
    network: NetworkType,
    settlement: Settlement,
    maker: MakerApi,
    token: Address,
}

impl User {
    pub fn new(
        seed: &[u8],
        network: NetworkType,
        settlement: Settlement,
        maker: MakerApi,
        token: Address,
    ) -> Self {
        Self {
            seed: Zeroizing::new(seed.to_vec()),
            network,
            settlement,
            maker,
            token,
        }
    }

    pub fn payout(&self) -> Address {
        self.settlement.account()
    }

    pub fn settlement(&self) -> &Settlement {
        &self.settlement
    }

    /// Takes a quote for `units`, checks the maker's proof, and accepts it with the share at
    /// `index`. The index is spent from this call on, whatever its outcome.
    pub async fn open(&self, index: u32, units: u32) -> Result<UserSwap> {
        let quote = self.maker.quote(units, self.payout()).await?;
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

        let keys = self.keys(index)?;
        let payout: [u8; 20] = self.payout().into();
        let acceptance = Acceptance {
            user_share: keys.share.public(),
            user_proof: context.prove_user(&quote.maker_share, &keys.share, &payout, OsRng),
            viewing_keys: keys.viewing,
        };
        let accepted = self.maker.accept(quote.quote_id, &acceptance).await?;
        let swap_id = swap_id(&acceptance.user_share);
        ensure!(
            accepted.swap_id == swap_id,
            "the maker reported another swap"
        );
        Ok(UserSwap {
            index,
            quote,
            swap_id,
        })
    }

    /// Checks the swap as the contract records it and derives the deposit account from the
    /// on-chain shares. Nothing may be deposited unless this succeeds.
    pub async fn verify(&self, swap: &UserSwap) -> Result<JointAccount> {
        let chain = self.caught_up(swap, |_| true).await?;
        let keys = self.keys(swap.index)?;
        ensure!(chain.stage == Stage::Open, "the swap is {:?}", chain.stage);
        ensure!(
            chain.maker_share == swap.quote.maker_share,
            "the on-chain maker share is not the quoted one"
        );
        ensure!(
            chain.user_share == keys.share.public(),
            "the on-chain user share is not ours"
        );
        ensure!(
            chain.user == self.payout(),
            "the payout goes to someone else"
        );
        ensure!(
            chain.token == self.token && chain.amount == swap.quote.amount,
            "the payout differs from the quote"
        );
        let now = self.settlement.now().await?;
        ensure!(
            (now + MIN_TIME_TO_T0..=now + MAX_TIME_TO_T0).contains(&chain.t0),
            "t0 is {}s away, outside {MIN_TIME_TO_T0}..={MAX_TIME_TO_T0}s",
            chain.t0.saturating_sub(now)
        );
        Ok(JointAccount::derive(
            &chain.maker_share,
            &chain.user_share,
            &keys.viewing,
        )?)
    }

    pub async fn state(&self, swap: &UserSwap) -> Result<OnChainSwap> {
        self.settlement
            .swap(swap.swap_id)
            .await?
            .context("the swap is not on-chain")
    }

    pub async fn lock_claim(&self, swap: &UserSwap) -> Result<()> {
        self.settlement.lock_claim(swap.swap_id).await?;
        Ok(())
    }

    /// Reveals the user share under a claim lock with time to spare, taking the lock if
    /// needed, then withdraws the payout. Safe to call again after an interruption at any
    /// point; returns the amount withdrawn.
    pub async fn claim(&self, swap: &UserSwap) -> Result<u128> {
        let chain = self.state(swap).await?;
        let amount = match chain.stage {
            Stage::Refunded => bail!("the maker refunded the swap"),
            // Revealed before an interruption: withdraw whatever is still credited.
            Stage::Claimed => self.settlement.balance_of(chain.user, chain.token).await?,
            Stage::Open | Stage::Ready => {
                self.hold_claim_lock(swap, &chain).await?;
                let keys = self.keys(swap.index)?;
                self.settlement.claim(swap.swap_id, &keys.share).await?;
                chain.amount
            }
        };
        if amount > 0 {
            // Apart from the claim, so a paused token can delay the payout but not the reveal.
            self.settlement
                .withdraw(chain.token, amount, chain.user)
                .await?;
        }
        Ok(amount)
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

    async fn hold_claim_lock(&self, swap: &UserSwap, chain: &OnChainSwap) -> Result<()> {
        let now = self.settlement.now().await?;
        if chain.claim_lock_until > now + CLAIM_MARGIN {
            return Ok(());
        }
        ensure!(
            chain.claim_lock_until <= now,
            "the claim lock lapses too soon to reveal under; claim once the next turn is ours"
        );
        self.settlement.lock_claim(swap.swap_id).await?;
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
        let deadline = Instant::now() + CATCH_UP;
        loop {
            if let Some(chain) = self.settlement.swap(swap.swap_id).await?
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

    fn context(&self, chain_id: u64, quote_id: B256) -> SwapContext {
        SwapContext {
            chain_id,
            contract: self.settlement.contract().into(),
            quote_id: quote_id.0,
        }
    }
}
