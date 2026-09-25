//! Each scenario plays one user through one outcome and checks both chains afterwards.

use std::cmp::Ordering;
use std::fmt::Display;
use std::sync::Arc;
use std::time::{Duration, Instant};

use anyhow::{Context as _, Result, anyhow, bail, ensure};
use rand_core::{OsRng, RngCore};
use zcash_address::ZcashAddress;
use zcash_protocol::consensus::Parameters;
use zecswap_chain::base::{OnChainSwap, Settlement, Stage};
use zecswap_chain::zcash::AccountUuid;
use zecswap_client::{MakerApi, User, UserSwap};
use zecswap_maker::Status;

use crate::env::{Env, MakerNode, Zcash};

pub(crate) const ALL: [&str; 6] = [
    "happy",
    "no-deposit",
    "underpaid",
    "silent-maker",
    "never-claimed",
    "abandoned-claim",
];

const POLL: Duration = Duration::from_secs(15);
const TIMEOUT: Duration = Duration::from_secs(80 * 60);

pub(crate) fn select(only: &[String]) -> Result<Vec<&'static str>> {
    if only.is_empty() {
        return Ok(ALL.to_vec());
    }
    only.iter()
        .map(|name| {
            ALL.iter()
                .find(|known| **known == name.as_str())
                .copied()
                .ok_or_else(|| anyhow!("unknown scenario {name}; known: {}", ALL.join(", ")))
        })
        .collect()
}

pub(crate) fn deposits(names: &[&str]) -> usize {
    names.iter().filter(|name| **name != "no-deposit").count()
}

pub(crate) async fn run(env: Arc<Env>, name: &'static str) -> Result<()> {
    let player = Player::join(env, name, name == "silent-maker").await?;
    match name {
        "happy" => happy(&player).await,
        "no-deposit" => no_deposit(&player).await,
        "underpaid" => underpaid(&player).await,
        "silent-maker" => silent_maker(&player).await,
        "never-claimed" => never_claimed(&player).await,
        "abandoned-claim" => abandoned_claim(&player).await,
        other => bail!("unknown scenario {other}"),
    }
}

/// Deposit, `ready`, claim; the maker sweeps the ZEC. The claim resumes under a lock taken
/// beforehand, as after an interruption, and claiming again once done changes nothing.
async fn happy(p: &Player) -> Result<()> {
    let swap = p.open().await?;
    let account = p.deposit(&swap, swap.quote.deposit_zat).await?;
    p.wait_for(&swap, Stage::Ready).await?;
    p.user.lock_claim(&swap).await?;
    p.claim_lock_until(&swap).await?;
    p.claim(&swap).await?;
    let (user, swap) = (&p.user, &swap);
    p.poll("a repeated claim to withdraw nothing", move || async move {
        Ok(match user.claim(swap).await? {
            0 => Step::Done(()),
            amount => Step::Fail(anyhow!("a repeated claim withdrew {amount}")),
        })
    })
    .await?;
    p.expect_swept_by_maker(swap).await?;
    p.forget(account).await
}

/// Nothing arrives, so the maker cancels, revealing a share that completes the key.
async fn no_deposit(p: &Player) -> Result<()> {
    let swap = p.open().await?;
    p.user.verify(&swap).await?;
    p.wait_for(&swap, Stage::Refunded).await?;
    p.user.refund_key(&swap).await?;
    p.expect_settled(&swap).await?;
    Ok(())
}

/// A short deposit is refused; the user takes it back with the maker's revealed share.
async fn underpaid(p: &Player) -> Result<()> {
    let swap = p.open().await?;
    let account = p.deposit(&swap, swap.quote.deposit_zat - 10_000).await?;
    p.wait_for(&swap, Stage::Refunded).await?;
    p.sweep_back(&swap, account).await
}

/// The maker never attests the deposit, so after `t0` the user claims without it; the
/// maker still collects the ZEC when it comes back.
async fn silent_maker(p: &Player) -> Result<()> {
    let swap = p.open().await?;
    let account = p.deposit(&swap, swap.quote.deposit_zat).await?;
    let t0 = p.user.state(&swap).await?.t0;
    p.wait_for_chain_time(t0).await?;
    let stage = p.user.state(&swap).await?.stage;
    ensure!(
        stage == Stage::Open,
        "the silent maker moved the swap to {stage:?}"
    );
    p.claim(&swap).await?;
    p.node().wake().await;
    p.expect_swept_by_maker(&swap).await?;
    p.forget(account).await
}

/// The user deposits and never claims; from `t1` the maker refunds and the user sweeps
/// the deposit back.
async fn never_claimed(p: &Player) -> Result<()> {
    let swap = p.open().await?;
    let account = p.deposit(&swap, swap.quote.deposit_zat).await?;
    p.wait_for(&swap, Stage::Ready).await?;
    p.wait_for(&swap, Stage::Refunded).await?;
    p.sweep_back(&swap, account).await
}

/// The user takes the claim lock and walks away; once it lapses and `t1` passes, the
/// maker refunds.
async fn abandoned_claim(p: &Player) -> Result<()> {
    let swap = p.open().await?;
    let account = p.deposit(&swap, swap.quote.deposit_zat).await?;
    p.wait_for(&swap, Stage::Ready).await?;
    p.user.lock_claim(&swap).await?;
    let lock_until = p.claim_lock_until(&swap).await?;
    p.log(format!("abandoned the claim lock, held until {lock_until}"));
    let refunded = p.wait_for(&swap, Stage::Refunded).await?;
    ensure!(
        refunded.refund_lock_until > lock_until,
        "the refund lock overlapped the claim lock"
    );
    p.sweep_back(&swap, account).await
}

struct Player {
    env: Arc<Env>,
    name: &'static str,
    silent_maker: bool,
    user: User,
}

enum Step<T> {
    Done(T),
    Wait,
    Fail(anyhow::Error),
}

impl Player {
    async fn join(env: Arc<Env>, name: &'static str, silent_maker: bool) -> Result<Self> {
        let maker = MakerApi::new(node(&env, silent_maker).url().await)?;
        let settlement = Settlement::connect(&env.base_rpc, env.contract, env.payout_key())?;
        let network = env.network.network_type();
        let user = User::new(&env.seed, network, settlement, maker, env.token);
        Ok(Self {
            env,
            name,
            silent_maker,
            user,
        })
    }

    fn node(&self) -> &MakerNode {
        node(&self.env, self.silent_maker)
    }

    fn log(&self, message: impl Display) {
        self.env.log(self.name, message);
    }

    async fn open(&self) -> Result<UserSwap> {
        let swap = self.user.open(OsRng.next_u32() >> 1, 1).await?;
        self.log(format!(
            "opened {} for {} zat",
            swap.swap_id, swap.quote.deposit_zat
        ));
        Ok(swap)
    }

    /// Verifies the swap on-chain, then watches its deposit account and pays into it.
    async fn deposit(&self, swap: &UserSwap, zatoshis: u64) -> Result<AccountUuid> {
        let joint = self.user.verify(swap).await?;
        let network = self.env.network.network_type();
        let to: ZcashAddress = joint.unified_address(network).parse()?;
        let mut zcash = self.env.zcash.lock().await;
        let Zcash { wallet, client } = &mut *zcash;
        let account = wallet.import_joint(client, &joint, self.name).await?;
        wallet.sync(client).await?;
        let payment = [(to, zatoshis)];
        let txid = wallet.pay(
            &self.env.prover,
            self.env.treasury,
            &self.env.treasury_key,
            &payment,
        )?;
        wallet.broadcast(client, txid).await?;
        self.log(format!("deposited {zatoshis} zat in {txid}"));
        Ok(account)
    }

    async fn claim(&self, swap: &UserSwap) -> Result<()> {
        let amount = self.user.claim(swap).await?;
        let (chain, token, payout) = (self.user.settlement(), self.env.token, self.user.payout());
        // The balance read can reach a node that hasn't seen the withdrawal yet.
        self.poll("the payout to arrive", move || async move {
            let balance = chain.token_balance(token, payout).await?;
            Ok(match balance.cmp(&amount) {
                Ordering::Equal => Step::Done(()),
                Ordering::Less => Step::Wait,
                Ordering::Greater => Step::Fail(anyhow!("the payout account holds {balance}")),
            })
        })
        .await?;
        self.log(format!("claimed and withdrew {amount}"));
        Ok(())
    }

    async fn wait_for(&self, swap: &UserSwap, stage: Stage) -> Result<OnChainSwap> {
        let user = &self.user;
        let chain = self
            .poll(&format!("{stage:?}"), move || async move {
                let chain = user.state(swap).await?;
                Ok(if chain.stage == stage {
                    Step::Done(chain)
                } else if matches!(chain.stage, Stage::Claimed | Stage::Refunded) {
                    Step::Fail(anyhow!("the swap was {:?} instead", chain.stage))
                } else {
                    Step::Wait
                })
            })
            .await?;
        self.log(format!("the swap is {stage:?}"));
        Ok(chain)
    }

    /// The claim lock's expiry, once our RPC node has seen the lock.
    async fn claim_lock_until(&self, swap: &UserSwap) -> Result<u64> {
        let user = &self.user;
        self.poll("the claim lock", move || async move {
            let until = user.state(swap).await?.claim_lock_until;
            Ok(if until > 0 {
                Step::Done(until)
            } else {
                Step::Wait
            })
        })
        .await
    }

    async fn wait_for_chain_time(&self, time: u64) -> Result<()> {
        let chain = self.user.settlement();
        self.poll("t0", move || async move {
            let now = chain.now().await?;
            Ok(if now >= time {
                Step::Done(())
            } else {
                Step::Wait
            })
        })
        .await
    }

    /// Combines the maker's revealed share with ours and takes the deposit back.
    async fn sweep_back(&self, swap: &UserSwap, account: AccountUuid) -> Result<()> {
        let key = self.user.refund_key(swap).await?;
        let zcash = &self.env.zcash;
        self.poll("the deposit to confirm", move || async move {
            let funds = zcash.lock().await.wallet.funds(account)?;
            let confirmed = funds.total > 0 && funds.spendable == funds.total;
            Ok(if confirmed {
                Step::Done(())
            } else {
                Step::Wait
            })
        })
        .await?;
        let txid = {
            let mut zcash = self.env.zcash.lock().await;
            let Zcash { wallet, client } = &mut *zcash;
            let home: ZcashAddress = wallet.fresh_address(self.env.treasury)?.parse()?;
            let txid = wallet.sweep(&self.env.prover, account, &key, &home)?;
            wallet.broadcast(client, txid).await?;
            txid
        };
        self.log(format!("swept the deposit back in {txid}"));
        self.poll("the sweep to be mined", move || async move {
            let mined = zcash.lock().await.wallet.is_mined(txid)?;
            Ok(if mined { Step::Done(()) } else { Step::Wait })
        })
        .await?;
        let left = self.env.zcash.lock().await.wallet.funds(account)?.total;
        ensure!(left == 0, "{left} zat is still in the joint account");
        self.forget(account).await
    }

    async fn expect_swept_by_maker(&self, swap: &UserSwap) -> Result<()> {
        let status = self.expect_settled(swap).await?;
        let sweep = status.sweep.context("the maker settled without sweeping")?;
        self.log(format!("the maker swept the deposit in {sweep}"));
        Ok(())
    }

    async fn expect_settled(&self, swap: &UserSwap) -> Result<Status> {
        let node = self.node();
        self.poll("the maker to settle", move || async move {
            let status = node
                .maker()
                .await
                .status(swap.swap_id)?
                .context("the maker lost the swap")?;
            Ok(if status.settled {
                Step::Done(status)
            } else {
                Step::Wait
            })
        })
        .await
    }

    async fn forget(&self, account: AccountUuid) -> Result<()> {
        self.env.zcash.lock().await.wallet.forget(account)?;
        Ok(())
    }

    /// Runs `check` until it is done or fails; errors from the chains are retried.
    async fn poll<T, F: Future<Output = Result<Step<T>>>>(
        &self,
        what: &str,
        mut check: impl FnMut() -> F,
    ) -> Result<T> {
        let deadline = Instant::now() + TIMEOUT;
        loop {
            match check().await {
                Ok(Step::Done(value)) => return Ok(value),
                Ok(Step::Fail(e)) => return Err(e.context(format!("waiting for {what}"))),
                Ok(Step::Wait) => {}
                Err(e) => self.log(format!("retrying after: {e:#}")),
            }
            ensure!(Instant::now() < deadline, "timed out waiting for {what}");
            tokio::time::sleep(POLL).await;
        }
    }
}

fn node(env: &Env, silent_maker: bool) -> &MakerNode {
    if silent_maker {
        &env.silent
    } else {
        &env.attentive
    }
}
