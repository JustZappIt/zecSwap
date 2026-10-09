//! Each scenario plays one user through one outcome and checks both chains afterwards.

use std::cmp::Ordering;
use std::fmt::Display;
use std::sync::Arc;
use std::time::{Duration, Instant};

use anyhow::{Context as _, Result, anyhow, bail, ensure};
use rand::{Rng, rand_core::UnwrapErr, rngs::SysRng};
use zcash_address::ZcashAddress;
use zcash_protocol::consensus::Parameters;
use zecswap_chain::evm::{OnChainSwap, Settlement, Stage};
use zecswap_chain::zcash::AccountUuid;
use zecswap_client::{MakerApi, Paid, RelayerApi, Route, User, UserSwap};
use zecswap_core::derive_user_keys;
use zecswap_maker::Status;

use crate::env::{Env, MAX_RELAYER_FEE, MakerNode, Needs, RELAYER_FEE, Zcash};

/// Paid to an account, on any chain.
const ACCOUNT: [&str; 6] = [
    "happy",
    "no-deposit",
    "underpaid",
    "silent-maker",
    "never-claimed",
    "abandoned-claim",
];
/// Paid into Railgun, where the deployment has it, and private sends from it.
const RAILGUN: [&str; 4] = [
    "railgun-happy",
    "railgun-resume",
    "railgun-no-deposit",
    SEND,
];
/// Private Railgun sends and withdrawals the relayer sends as their broadcaster.
const SEND: &str = "railgun-send";

const POLL: Duration = Duration::from_secs(15);
const TIMEOUT: Duration = Duration::from_secs(80 * 60);

/// The named scenarios, or every one this chain can run.
pub(crate) fn select(only: &[String], railgun: bool) -> Result<Vec<&'static str>> {
    let known: Vec<&'static str> = ACCOUNT
        .iter()
        .chain(if railgun { &RAILGUN[..] } else { &[] })
        .copied()
        .collect();
    if only.is_empty() {
        return Ok(known);
    }
    only.iter()
        .map(|name| {
            known
                .iter()
                .find(|known| **known == name.as_str())
                .copied()
                .ok_or_else(|| {
                    anyhow!(
                        "unknown scenario {name} here; known: {} (Railgun ones need ZECSWAP_E2E_RAILGUN)",
                        known.join(", ")
                    )
                })
        })
        .collect()
}

pub(crate) fn needs(names: &[&str]) -> Needs {
    let sends = names.iter().filter(|name| **name == SEND).count();
    let relayed = names.iter().filter(|name| pays_into_railgun(name)).count();
    Needs {
        accounts: names.len() - relayed - sends,
        relayed,
        sends,
        deposits: names
            .iter()
            .filter(|name| **name != SEND && !name.ends_with("no-deposit"))
            .count(),
    }
}

fn pays_into_railgun(name: &str) -> bool {
    name.starts_with("railgun-") && name != SEND
}

pub(crate) async fn run(env: Arc<Env>, name: &'static str) -> Result<()> {
    if name == SEND {
        return crate::sends::railgun_send(&env).await;
    }
    let player = Player::join(env, name, name == "silent-maker").await?;
    match name {
        "happy" => happy(&player).await,
        "no-deposit" | "railgun-no-deposit" => no_deposit(&player).await,
        "underpaid" => underpaid(&player).await,
        "silent-maker" => silent_maker(&player).await,
        "never-claimed" => never_claimed(&player).await,
        "abandoned-claim" => abandoned_claim(&player).await,
        "railgun-happy" => railgun_happy(&player).await,
        "railgun-resume" => railgun_resume(&player).await,
        other => bail!("unknown scenario {other}"),
    }
}

/// Deposit, `ready`, claim; the maker sweeps the ZEC. The claim resumes under a lock taken
/// beforehand, as after an interruption, and claiming again once done changes nothing. Paid
/// into, the swap hands its accept's token back, and the device, whose issuer gives it one
/// accept a day, pays for a second swap with it: one it walks away from.
async fn happy(p: &Player) -> Result<()> {
    let swap = p.open().await?;
    let account = p.deposit(&swap, swap.quote.deposit_zat).await?;
    p.wait_for(&swap, Stage::Ready).await?;
    p.collect_token(&swap).await?;
    p.user.lock_claim(&swap).await?;
    p.claim_lock_until(&swap).await?;
    p.claim(&swap).await?;
    let (user, swap) = (&p.user, &swap);
    p.poll("a repeated claim to withdraw nothing", move || async move {
        Ok(match user.claim(swap).await?.amount {
            0 => Step::Done(()),
            amount => Step::Fail(anyhow!("a repeated claim withdrew {amount}")),
        })
    })
    .await?;
    p.expect_swept_by_maker(swap).await?;
    p.forget(account).await?;
    let second = p.open().await?;
    p.log("paid for a second swap with the token the first handed back");
    p.walk_away(&second).await
}

/// Nothing arrives, so the maker cancels, revealing a share that completes the key.
async fn no_deposit(p: &Player) -> Result<()> {
    let swap = p.open().await?;
    p.walk_away(&swap).await
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
    p.node().sleep().await;
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

/// Paid into Railgun: after `ready`, the relayer takes the signed claim lock, reveals, and
/// shields the payout to the note the swap committed to; the maker sweeps the ZEC.
async fn railgun_happy(p: &Player) -> Result<()> {
    let swap = p.open().await?;
    let account = p.deposit(&swap, swap.quote.deposit_zat).await?;
    p.wait_for(&swap, Stage::Ready).await?;
    p.claim(&swap).await?;
    let again = p.user.claim(&swap).await?;
    ensure!(again.tx.is_none(), "a repeated claim paid out again");
    p.expect_swept_by_maker(&swap).await?;
    p.forget(account).await
}

/// The app dies right after its share is revealed: the claim landed and the payout never
/// did. Claiming again only pays out.
async fn railgun_resume(p: &Player) -> Result<()> {
    let swap = p.open().await?;
    let account = p.deposit(&swap, swap.quote.deposit_zat).await?;
    p.wait_for(&swap, Stage::Ready).await?;
    p.user.lock_claim(&swap).await?;
    p.claim_lock_until(&swap).await?;
    p.reveal_only(&swap).await?;
    p.claim(&swap).await?;
    p.expect_swept_by_maker(&swap).await?;
    p.forget(account).await
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
        let maker =
            MakerApi::new(node(&env, silent_maker).url().await)?.with_tokens(env.tokens(name)?);
        let (settlement, route) = if pays_into_railgun(name) {
            let route = Route::Railgun {
                relayer: RelayerApi::new(env.relayer_url.clone())?,
                max_fee: MAX_RELAYER_FEE,
            };
            (Settlement::read_only(&env.evm_rpc, env.contract)?, route)
        } else {
            let settlement = Settlement::connect(&env.evm_rpc, env.contract, env.payout_key())?;
            (settlement, Route::Account)
        };
        let network = env.network.network_type();
        let user = User::new(
            &env.seed, &env.seed, network, settlement, maker, env.token, route,
        )
        .with_min_time_to_t0(env.min_time_to_t0);
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
        let swap = self.user.open(UnwrapErr(SysRng).next_u32() >> 1, 1).await?;
        // The index is logged so a failed run's deposits can be taken back with the user share.
        self.log(format!(
            "opened {} for {} zat, index {}",
            swap.swap_id, swap.quote.deposit_zat, swap.index
        ));
        Ok(swap)
    }

    /// Never pays into the swap: the maker cancels it, revealing a share that completes the
    /// key, and hands nothing back.
    async fn walk_away(&self, swap: &UserSwap) -> Result<()> {
        self.user.verify(swap).await?;
        self.wait_for(swap, Stage::Refunded).await?;
        self.user.refund_key(swap).await?;
        self.expect_settled(swap).await?;
        ensure!(
            !self.user.collect_token(swap).await?,
            "a swap walked away from handed its token back"
        );
        Ok(())
    }

    /// Waits for the swap to hand its accept's token back.
    async fn collect_token(&self, swap: &UserSwap) -> Result<()> {
        let user = &self.user;
        self.poll("the swap's token back", move || async move {
            Ok(if user.collect_token(swap).await? {
                Step::Done(())
            } else {
                Step::Wait
            })
        })
        .await?;
        self.log("the swap handed its token back");
        Ok(())
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
        let paid = self.user.claim(swap).await?;
        let chain = self.user.settlement();
        let Some(account) = chain.account() else {
            return self.expect_shielded(swap, paid).await;
        };
        let (token, amount) = (self.env.token, paid.amount);
        // The balance read can reach a node that hasn't seen the withdrawal yet.
        self.poll("the payout to arrive", move || async move {
            let balance = chain.token_balance(token, account).await?;
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

    /// The payout reached Railgun as the note the swap committed to, for the amount less the
    /// relayer's and Railgun's fees, and the user's Railgun wallet opens it.
    async fn expect_shielded(&self, swap: &UserSwap, paid: Paid) -> Result<()> {
        let tx = paid.tx.context("the claim paid nothing out")?;
        let chain = self.user.settlement();
        let notes = self
            .poll("the payout's receipt", move || async move {
                Ok(Step::Done(chain.shielded(tx).await?))
            })
            .await?;
        let [shielded] = notes.as_slice() else {
            bail!("the payout shielded {} notes", notes.len());
        };
        // The relayer's fee is its floor, or, priced by gas, what its terms quoted.
        let fee = swap.quote.amount - paid.amount;
        ensure!(
            shielded.note == self.user.payout_note(swap.index)?,
            "the payout went to another note"
        );
        ensure!(
            shielded.token == self.env.token
                && (u128::from(RELAYER_FEE)..=MAX_RELAYER_FEE).contains(&fee)
                && shielded.value + shielded.fee == paid.amount,
            "Railgun took {} + {} of {} after a relayer fee of {fee}",
            shielded.value,
            shielded.fee,
            paid.amount
        );
        let wallet = self.user.railgun();
        ensure!(
            wallet.open(&shielded.note).is_some(),
            "the Railgun wallet cannot open its own note"
        );
        self.log(format!(
            "shielded {} into {} in {tx}",
            shielded.value,
            wallet.address()
        ));
        Ok(())
    }

    /// Sends the claim alone, as a relayer that reveals and is then cut off would have. Anyone
    /// may send it while the lock is held.
    async fn reveal_only(&self, swap: &UserSwap) -> Result<()> {
        let network = self.env.network.network_type();
        let z = derive_user_keys(&self.env.seed, network, 0, swap.index)?.share;
        let maker = self.env.attentive.maker().await;
        let terms = self.user.terms(swap)?;
        let tx = maker.settlement().claim(swap.swap_id, &terms, &z).await?;
        self.log(format!("revealed the share alone in {tx}"));
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
