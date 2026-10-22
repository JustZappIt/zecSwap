//! Forward claims held until their window opens (`POST /v1/claim/hold`): a wallet hands its claim
//! over once its deposit is out, and the relayer sends it as `/v1/claim` would the moment the
//! window opens, so its user may leave the app. The relayer then holds the user share long before
//! the claim reveals it. Given to the maker, the share would let the maker refund and sweep the
//! deposit too; the relayer alone can't spend the deposit with it, as the joint account's `nk` and
//! `rivk` go to the maker alone. So the share is the most sensitive thing the relayer keeps: no
//! log, error, metric or alert carries it, nor any request before its claim's own send (which
//! goes as `/v1/claim`'s, unsimulated through `[reveals]` where the relayer has it), and it is
//! deleted with its claim once the swap is paid out or refunded.

use std::collections::HashMap;
use std::path::Path;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use alloy_primitives::{Address, B256};
use rusqlite::{Connection, OptionalExtension, params};
use tokio::sync::Notify;
use tracing::{info, warn};
use zecswap_api::relayer::{Claim, Sent};
use zecswap_chain::evm::judge::Refusal;
use zecswap_chain::evm::{BlockId, Head, Stage, SwapState, on_chain, wall_now};
use zecswap_core::SecretShare;
use zecswap_railgun::ShieldNote;

use crate::monitor::HeldClaims;
use crate::sends::{now, word};
use crate::{ClaimWindow, Relayer, RelayerError, Result};

/// How often held claims are looked at for a new block: Ethereum's come 12 s apart.
const TICK: Duration = Duration::from_secs(4);
/// The longest a held claim that can't go before a known time waits between looks, so that a
/// refund meanwhile deletes it this soon.
const SLOW_LOOK: u64 = 300;
/// Seconds a held claim waits after a send of it failed, or while Railgun takes no payout.
const RETRY: u64 = 30;
/// What the sends of held claims are counted as.
const HELD: &str = "held_claim";

/// What a hold came to.
pub enum Holding {
    /// Held, to be sent once its window opens.
    Held,
    /// The swap is claimed: answered as `/v1/claim` answers it.
    Claimed(Sent),
}

/// Why a held claim is deleted.
#[derive(Clone, Copy)]
enum Done {
    /// Its swap is claimed and paid out.
    PaidOut,
    /// Its swap was refunded: it can never be sent.
    Refunded,
}

/// The claims held, one a swap, each as it was posted, until its swap is paid out or refunded.
pub(crate) struct Held {
    db: Mutex<Connection>,
    chain_id: u64,
    contract: Address,
    /// Claims held now, counted again on each change.
    held: AtomicU64,
    sent: AtomicU64,
    dropped: AtomicU64,
    /// When each is looked at next; none for one not looked at since the relayer started.
    looks: Mutex<HashMap<B256, Look>>,
    /// Woken by each claim held, so that one whose window is open goes at once.
    wake: Notify,
}

#[derive(Clone, Copy, Default)]
struct Look {
    /// Not before this time: a claim that can't go sooner, or whose send just failed, waits.
    after: u64,
    /// The newest block it was judged at: it is judged once a block.
    judged: Option<u64>,
}

impl Held {
    pub(crate) fn open(path: &Path, chain_id: u64, contract: Address) -> rusqlite::Result<Self> {
        let db = Connection::open(path)?;
        // A deleted claim's share is overwritten, not left in a free page.
        db.execute_batch(
            "PRAGMA secure_delete = ON;
             CREATE TABLE IF NOT EXISTS held_claims (
                 chain_id INTEGER NOT NULL,
                 contract BLOB NOT NULL,
                 swap_id BLOB NOT NULL,
                 claim TEXT NOT NULL,
                 held_at INTEGER NOT NULL,
                 PRIMARY KEY (chain_id, contract, swap_id)
             );",
        )?;
        let held = Self {
            db: Mutex::new(db),
            chain_id,
            contract,
            held: AtomicU64::new(0),
            sent: AtomicU64::new(0),
            dropped: AtomicU64::new(0),
            looks: Mutex::default(),
            wake: Notify::new(),
        };
        held.recount(&held.db.lock().unwrap())?;
        Ok(held)
    }

    /// Claims held now.
    pub(crate) fn count(&self) -> u64 {
        self.held.load(Ordering::Relaxed)
    }

    pub(crate) fn snapshot(&self) -> HeldClaims {
        HeldClaims {
            held: self.count(),
            sent: self.sent.load(Ordering::Relaxed),
            dropped: self.dropped.load(Ordering::Relaxed),
        }
    }

    fn recount(&self, db: &Connection) -> rusqlite::Result<()> {
        let held: i64 = db.query_row(
            "SELECT COUNT(*) FROM held_claims WHERE chain_id = ?1 AND contract = ?2",
            params![self.chain_id as i64, self.contract.as_slice()],
            |row| row.get(0),
        )?;
        self.held.store(held as u64, Ordering::Relaxed);
        Ok(())
    }

    /// The swaps whose claims are held, the longest held first.
    fn swaps(&self) -> rusqlite::Result<Vec<B256>> {
        let db = self.db.lock().unwrap();
        let mut query = db.prepare(
            "SELECT swap_id FROM held_claims WHERE chain_id = ?1 AND contract = ?2
             ORDER BY held_at",
        )?;
        query
            .query_map(
                params![self.chain_id as i64, self.contract.as_slice()],
                |row| word(row, 0),
            )?
            .collect()
    }

    /// The claim held for `swap`, as it was posted.
    fn stored(&self, swap: B256) -> rusqlite::Result<Option<String>> {
        self.db
            .lock()
            .unwrap()
            .query_row(
                "SELECT claim FROM held_claims
                 WHERE chain_id = ?1 AND contract = ?2 AND swap_id = ?3",
                params![
                    self.chain_id as i64,
                    self.contract.as_slice(),
                    swap.as_slice()
                ],
                |row| row.get(0),
            )
            .optional()
    }

    pub(crate) fn claim(&self, swap: B256) -> Result<Option<Claim>> {
        self.stored(swap)?
            .map(|claim| serde_json::from_str(&claim))
            .transpose()
            // Its error would quote what failed to parse, the share perhaps.
            .map_err(|_| RelayerError::Internal(anyhow::anyhow!("a held claim no longer reads")))
    }

    /// Holds `claim`, a claim on `swap` as posted, in place of any held for it.
    fn hold(&self, swap: B256, claim: &str) -> rusqlite::Result<()> {
        let db = self.db.lock().unwrap();
        db.execute(
            "INSERT INTO held_claims (chain_id, contract, swap_id, claim, held_at)
             VALUES (?1, ?2, ?3, ?4, ?5)
             ON CONFLICT (chain_id, contract, swap_id) DO UPDATE SET claim = excluded.claim",
            params![
                self.chain_id as i64,
                self.contract.as_slice(),
                swap.as_slice(),
                claim,
                now() as i64,
            ],
        )?;
        self.recount(&db)
    }

    /// Deletes the claim held for `swap`, its share with it, as `done`: counted and logged once.
    fn delete(&self, swap: B256, done: Done) -> rusqlite::Result<()> {
        let deleted = {
            let db = self.db.lock().unwrap();
            let deleted = db.execute(
                "DELETE FROM held_claims
                 WHERE chain_id = ?1 AND contract = ?2 AND swap_id = ?3",
                params![
                    self.chain_id as i64,
                    self.contract.as_slice(),
                    swap.as_slice()
                ],
            )?;
            self.recount(&db)?;
            deleted > 0
        };
        self.looks.lock().unwrap().remove(&swap);
        match (deleted, done) {
            (false, _) => {}
            (true, Done::PaidOut) => {
                info!(swap_id = %swap, step = "deleted", "deleted a held claim, its swap paid out");
            }
            (true, Done::Refunded) => {
                self.dropped.fetch_add(1, Ordering::Relaxed);
                info!(swap_id = %swap, step = "dropped", "deleted a held claim, its swap refunded");
            }
        }
        Ok(())
    }

    fn count_sent(&self, swap: B256) {
        self.sent.fetch_add(1, Ordering::Relaxed);
        info!(swap_id = %swap, step = "sent", "sent a held claim");
    }

    /// Whether the claim held for `swap` is due a look at `now` and, given the newest block, in
    /// it.
    fn due(&self, swap: B256, now: u64, block: Option<u64>) -> bool {
        self.looks.lock().unwrap().get(&swap).is_none_or(|look| {
            now >= look.after
                && block
                    .zip(look.judged)
                    .is_none_or(|(block, judged)| block > judged)
        })
    }

    fn judged(&self, swap: B256, block: u64) {
        let mut looks = self.looks.lock().unwrap();
        looks.entry(swap).or_default().judged = Some(block);
    }

    /// Looks at the claim held for `swap` again no sooner than `after`.
    fn wait(&self, swap: B256, after: u64) {
        self.looks.lock().unwrap().entry(swap).or_default().after = after;
    }
}

impl Relayer {
    /// Holds a forward claim until its window opens: checked as `claim` checks one, all but its
    /// window and whether Railgun takes the payout, which are judged as it goes.
    #[tracing::instrument(skip_all, fields(swap_id = %request.swap_id, operation = "hold_claim"), err(level = "warn"))]
    pub async fn hold_claim(&self, request: Claim) -> Result<Holding> {
        let held = self
            .held
            .as_ref()
            .ok_or_else(|| RelayerError::Rejected("this relayer holds no claims".into()))?;
        let id = request.swap_id;
        let (swap, ..) = self.read_claim(&request).await?;
        match swap.stage {
            Stage::Claimed => return Ok(Holding::Claimed(self.payout(request.payout).await?)),
            Stage::Open | Stage::Ready => {}
            Stage::Refunded => {
                held.delete(id, Done::Refunded)?;
                return Err(RelayerError::Rejected(format!(
                    "the swap is {:?}",
                    swap.stage
                )));
            }
        }
        // As read, so the same claim posted again is held already.
        let claim = serde_json::to_string(&request)
            .map_err(|_| RelayerError::Internal(anyhow::anyhow!("a claim doesn't serialize")))?;
        let stored = held.stored(id)?;
        if stored.as_deref() == Some(claim.as_str()) {
            return Ok(Holding::Held);
        }
        self.check_payout(&swap, &request.payout).await?;
        held.hold(id, &claim)?;
        let step = if stored.is_some() { "replaced" } else { "held" };
        info!(swap_id = %id, step, "holding a claim until its window opens");
        held.wake.notify_one();
        Ok(Holding::Held)
    }

    /// Counts a hold as `observe` counts a request: one held sends nothing, and a claimed swap's
    /// payout what it sent.
    pub(crate) fn observe_holding(
        self: &Arc<Self>,
        operation: &'static str,
        result: Result<Holding>,
    ) -> Result<Holding> {
        match result {
            Ok(Holding::Claimed(sent)) => self.observe(operation, Ok(sent)).map(Holding::Claimed),
            Ok(Holding::Held) => {
                self.monitor.sent(operation, 0);
                Ok(Holding::Held)
            }
            Err(error) => {
                self.unsent(operation, &error);
                Err(error)
            }
        }
    }

    /// Sends each held claim the moment its window opens, judged on each new block, for as long
    /// as the relayer runs.
    pub async fn run_held_claims(self: Arc<Self>) {
        let Some(held) = &self.held else {
            return;
        };
        loop {
            let pause = match self.held_pass().await {
                Ok(()) => TICK,
                Err(error) => {
                    // RPC errors can carry the node's URL: log the kind alone.
                    let failure_kind = error.failure_kind().unwrap_or("refused");
                    warn!(
                        operation = HELD,
                        failure_kind, "judging the held claims failed; retrying"
                    );
                    Duration::from_secs(RETRY)
                }
            };
            tokio::select! {
                () = tokio::time::sleep(pause) => {}
                () = held.wake.notified() => {}
            }
        }
    }

    /// One look at each held claim that is due, all on one read of the newest block: sends those
    /// whose window is open, pays out those claimed, and deletes those paid out or refunded.
    pub(crate) async fn held_pass(self: &Arc<Self>) -> Result<()> {
        let Some(held) = &self.held else {
            return Ok(());
        };
        let now = wall_now();
        let mut due = held.swaps()?;
        due.retain(|swap| held.due(*swap, now, None));
        if due.is_empty() {
            return Ok(());
        }
        let head = self.settlement.head().await?;
        due.retain(|swap| held.due(*swap, now, Some(head.number)));
        if due.is_empty() {
            return Ok(());
        }
        let states = self
            .settlement
            .swap_states(&due, BlockId::hash(head.hash))
            .await?;
        for (id, state) in due.into_iter().zip(states) {
            held.judged(id, head.number);
            let judged = match held.claim(id) {
                Ok(Some(claim)) => self.judge_held(held, claim, state, head).await,
                Ok(None) => Ok(()),
                Err(error) => Err(error),
            };
            if let Err(error) = judged {
                held.wait(id, wall_now() + RETRY);
                let failure_kind = error.failure_kind().unwrap_or("refused");
                warn!(
                    swap_id = %id,
                    step = "failed",
                    failure_kind,
                    "judging or sending a held claim failed; trying again"
                );
            }
        }
        Ok(())
    }

    /// Sends a held claim whose window is open, pays out one whose swap is claimed, and deletes
    /// one paid out or refunded; one that must wait for a known time is looked at again then.
    async fn judge_held(
        self: &Arc<Self>,
        held: &Held,
        claim: Claim,
        state: Option<SwapState>,
        head: Head,
    ) -> Result<()> {
        let id = claim.swap_id;
        let terms = zecswap_core::Terms::from(&claim.terms);
        let Ok(Some(swap)) = on_chain(id, state, &terms) else {
            // Not on the chain on its terms, as when a reorganisation took its open away.
            held.wait(id, wall_now() + SLOW_LOOK);
            return Ok(());
        };
        let note = ShieldNote::from(&claim.payout.note);
        match swap.stage {
            Stage::Refunded => held.delete(id, Done::Refunded)?,
            Stage::Claimed if swap.paid_out => held.delete(id, Done::PaidOut)?,
            Stage::Claimed => {
                if !self.settlement.railgun_accepts(swap.token).await? {
                    held.wait(id, wall_now() + RETRY);
                    return Ok(());
                }
                self.in_turn(HELD, id, |relayer| async move {
                    let tx = relayer.send_payout(&claim.payout, &terms, &note).await?;
                    Ok(Sent {
                        transactions: tx.into_iter().collect(),
                    })
                })
                .await?;
            }
            Stage::Open | Stage::Ready => {
                match self.claim_window(id, &swap, &terms, head).await? {
                    ClaimWindow::Open => {}
                    // From t1 it reopens at t1 + LATE_WINDOW, unless the maker refunds first.
                    ClaimWindow::Shut(Refusal::MakersUntil(_) | Refusal::TooSoon) => {
                        let reopens = swap.t1.saturating_add(self.windows.late);
                        held.wait(id, reopens.min(wall_now() + SLOW_LOOK));
                        return Ok(());
                    }
                    ClaimWindow::Shallow | ClaimWindow::Shut(_) => return Ok(()),
                }
                if !self.settlement.railgun_accepts(swap.token).await? {
                    held.wait(id, wall_now() + RETRY);
                    return Ok(());
                }
                let secret = SecretShare::from_be_bytes(&claim.secret.0)
                    .map_err(|e| RelayerError::Rejected(e.to_string()))?;
                self.in_turn(HELD, id, |relayer| async move {
                    let send = relayer.send_claim(id, &terms, &secret, &claim.payout, &note);
                    let (sent, payout) = Box::pin(send).await?;
                    if sent.is_some()
                        && let Some(held) = &relayer.held
                    {
                        held.count_sent(id);
                    }
                    Ok(Sent {
                        transactions: sent.into_iter().chain(payout).collect(),
                    })
                })
                .await?;
            }
        }
        Ok(())
    }
}
