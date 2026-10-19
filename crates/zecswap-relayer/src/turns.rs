//! A swap's requests take turns and run to their end, and each of its one-time steps is sent
//! once: a request posted again, after its caller gave up waiting or the relayer restarted, gets
//! the first one's transaction, never a second.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use alloy_primitives::{Address, B256};
use tokio::sync::OwnedMutexGuard;
use zecswap_api::relayer::Sent;
use zecswap_chain::evm::Settlement;
use zecswap_chain::evm::railgun::Status;

use crate::{Relayer, RelayerError, Result};

/// How long a transaction its node doesn't know may still be on its way there, rather than gone.
const MISSING_FOR: Duration = Duration::from_secs(5 * 60);
/// How long a step's transaction is remembered: swaps settle within hours.
const REMEMBERED_FOR: Duration = Duration::from_secs(24 * 60 * 60);
pub(crate) const UNMINED: &str =
    "the transaction is sent and not mined yet; post the same request again later";
const EARLIER: &str = "transactions sent before the relayer restarted are not mined yet; post the same request again later";

/// Each swap's requests, and each deposit address's, one at a time in the order they came.
#[derive(Clone, Default)]
pub(crate) struct Turns(Arc<Mutex<HashMap<B256, Arc<tokio::sync::Mutex<()>>>>>);

/// A swap's turn, until dropped.
pub(crate) struct Turn {
    turns: Turns,
    swap_id: B256,
    held: Option<OwnedMutexGuard<()>>,
}

impl Turns {
    /// How many swaps have requests running or waiting.
    #[cfg(test)]
    pub(crate) fn swaps(&self) -> usize {
        self.0.lock().unwrap().len()
    }

    /// Waits for the swap's earlier requests to end.
    pub(crate) async fn take(&self, swap_id: B256) -> Turn {
        let line = Arc::clone(self.0.lock().unwrap().entry(swap_id).or_default());
        let held = line.lock_owned().await;
        Turn {
            turns: self.clone(),
            swap_id,
            held: Some(held),
        }
    }
}

impl Drop for Turn {
    fn drop(&mut self) {
        let mut lines = self.turns.0.lock().unwrap();
        self.held.take();
        if lines
            .get(&self.swap_id)
            .is_some_and(|line| Arc::strong_count(line) == 1)
        {
            lines.remove(&self.swap_id);
        }
    }
}

/// A swap's steps that can each land once.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub(crate) enum Step {
    Claim,
    Payout,
    Ready,
    Refund,
    RefundPayout,
    Funding,
}

/// The transaction the relayer sent for each one-time step of a swap.
#[derive(Default)]
pub(crate) struct Steps {
    sent: Mutex<HashMap<(B256, Step), Record>>,
    /// Each sending account's nonce when the relayer started, where transactions it had sent
    /// were not mined yet: a step among them would be sent again, so the account sends none
    /// until they are.
    earlier: Mutex<HashMap<Address, u64>>,
}

#[derive(Clone, Copy)]
struct Record {
    tx: B256,
    landed: bool,
    at: Instant,
}

impl Steps {
    /// Notes which of `senders` have transactions from before this start not mined yet.
    pub(crate) async fn note_earlier(&self, senders: &[&Settlement]) -> anyhow::Result<()> {
        for sender in senders {
            let Some(account) = sender.account() else {
                continue;
            };
            let (mined, sent) = sender.nonces().await?;
            if sent > mined {
                self.earlier.lock().unwrap().insert(account, sent);
            }
        }
        Ok(())
    }

    fn get(&self, swap_id: B256, step: Step) -> Option<Record> {
        self.sent.lock().unwrap().get(&(swap_id, step)).copied()
    }

    fn record(&self, swap_id: B256, step: Step, tx: B256, landed: bool) {
        let mut sent = self.sent.lock().unwrap();
        sent.retain(|_, record| record.at.elapsed() < REMEMBERED_FOR);
        sent.insert(
            (swap_id, step),
            Record {
                tx,
                landed,
                at: Instant::now(),
            },
        );
    }

    fn forget(&self, swap_id: B256, step: Step) {
        self.sent.lock().unwrap().remove(&(swap_id, step));
    }

    /// Refuses for now while `sender` has transactions from before this start not mined.
    async fn after_earlier(&self, sender: &Settlement) -> Result<()> {
        let Some(account) = sender.account() else {
            return Ok(());
        };
        let Some(until) = self.earlier.lock().unwrap().get(&account).copied() else {
            return Ok(());
        };
        let (mined, sent) = sender.nonces().await?;
        // All mined, or those left dropped.
        if mined >= until || sent <= mined {
            self.earlier.lock().unwrap().remove(&account);
            return Ok(());
        }
        Err(RelayerError::Unsettled(EARLIER))
    }
}

impl Relayer {
    /// Runs `request`, counted as `operation`, once the swap's earlier requests have ended, and
    /// to its own end whether or not its caller still waits.
    pub(crate) async fn in_turn<F>(
        self: &Arc<Self>,
        operation: &'static str,
        swap_id: B256,
        request: impl FnOnce(Arc<Self>) -> F,
    ) -> Result<Sent>
    where
        F: Future<Output = Result<Sent>> + Send + 'static,
    {
        let request = request(Arc::clone(self));
        self.in_line(operation, swap_id, request, Self::observe)
            .await
    }

    /// Runs `request` once the earlier requests under `key` have ended, and to its own end
    /// whether or not its caller still waits: cut off half way, it would leave a transaction
    /// sent that a repeat knows nothing of. `count` counts how it ended, as `operation`.
    pub(crate) async fn in_line<T, F>(
        self: &Arc<Self>,
        operation: &'static str,
        key: B256,
        request: F,
        count: fn(&Arc<Self>, &'static str, Result<T>) -> Result<Sent>,
    ) -> Result<Sent>
    where
        T: Send + 'static,
        F: Future<Output = Result<T>> + Send + 'static,
    {
        let turn = self.turns.take(key).await;
        let relayer = Arc::clone(self);
        tokio::spawn(async move {
            let done = request.await;
            drop(turn);
            count(&relayer, operation, done)
        })
        .await
        .unwrap_or_else(|e| {
            self.observe(
                operation,
                Err(anyhow::anyhow!("the request stopped: {e}").into()),
            )
        })
    }

    /// Sends a swap's one-time `step` from `sender`: the transaction, or none where one sent for
    /// it before landed. While one sent before may still land, nothing is sent.
    pub(crate) async fn send_once(
        &self,
        swap_id: B256,
        step: Step,
        sender: &Settlement,
        send: impl Future<Output = std::result::Result<B256, zecswap_chain::Error>>,
    ) -> Result<Option<B256>> {
        if let Some(sent) = self.steps.get(swap_id, step) {
            if sent.landed {
                return Ok(None);
            }
            match sender.transaction_status(sent.tx).await? {
                Status::Mined { succeeded: true } => {
                    self.steps.record(swap_id, step, sent.tx, true);
                    return Ok(None);
                }
                Status::Mined { succeeded: false } => {}
                Status::Unknown if sent.at.elapsed() >= MISSING_FOR => {}
                Status::Pending | Status::Unknown => {
                    return Err(RelayerError::Unsettled(UNMINED));
                }
            }
            self.steps.forget(swap_id, step);
        }
        self.steps.after_earlier(sender).await?;
        match send.await {
            Ok(tx) => {
                self.steps.record(swap_id, step, tx, true);
                Ok(Some(tx))
            }
            Err(zecswap_chain::Error::Unconfirmed(tx)) => {
                self.steps.record(swap_id, step, tx, false);
                Err(RelayerError::Unsettled(UNMINED))
            }
            Err(e) => Err(e.into()),
        }
    }

    /// Sends a swap's escrow funding from `sender`, whose hash comes back before it is mined: a
    /// repeat while that one may still land gets its hash and sends nothing. On mainnet an app
    /// posted its first funding three times in 20 s, and the relayer signed two.
    pub(crate) async fn fund_once(
        &self,
        swap_id: B256,
        sender: &Settlement,
        send: impl Future<Output = Result<Option<B256>>>,
    ) -> Result<Option<B256>> {
        if let Some(sent) = self.steps.get(swap_id, Step::Funding) {
            match sender.transaction_status(sent.tx).await? {
                Status::Pending => return Ok(Some(sent.tx)),
                Status::Unknown if sent.at.elapsed() < MISSING_FOR => return Ok(Some(sent.tx)),
                // Landed, reverted or gone: the escrow on chain decides, or a new send.
                Status::Mined { .. } | Status::Unknown => {
                    self.steps.forget(swap_id, Step::Funding);
                }
            }
        }
        self.steps.after_earlier(sender).await?;
        let sent = send.await?;
        if let Some(tx) = sent {
            self.steps.record(swap_id, Step::Funding, tx, false);
        }
        Ok(sent)
    }
}
