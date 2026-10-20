//! What the maker reads of the settlement chain. A pass reads every swap it watches in a call
//! or two, whatever their number; what it read serves the Zcash worker and status requests
//! until the next pass; and a swap whose claim or refund a finalized block holds is never read
//! again, since nothing can undo it.

use std::collections::{HashMap, HashSet};
use std::time::{Duration, Instant};

use anyhow::{Context as _, Result};
use tracing::warn;
use zecswap_api::reverse::Phase;
use zecswap_chain::evm::{B256, BlockId, Head, OnChainSwap, Stage, SwapState, on_chain};
use zecswap_core::Terms;

use super::Maker;

/// The finalized block moves once an epoch, 32 blocks, so settled swaps are read there at most
/// once in this many: about a minute on Ethereum.
const FINALIZED_EVERY: u64 = 5;
/// What a pass read serves others for this long: a few watchtower ticks.
pub(super) const READ_SERVES: Duration = Duration::from_secs(45);
/// How long the maker's inventory, as last read, prices quotes; its own opens clear it.
const INVENTORY_FOR: Duration = Duration::from_secs(30);
/// How long a reverse swap's phase answers status requests.
const PHASE_FOR: Duration = Duration::from_secs(10);
/// How long a sent transaction the node has no receipt of waits before it is asked again.
const UNMINED_RECHECK: Duration = Duration::from_secs(300);
/// A settled swap is archived a day after it settles: past that, nothing here is wanted.
const FORGET_AFTER: Duration = Duration::from_secs(25 * 60 * 60);

#[derive(Default)]
pub(super) struct ChainCache {
    /// Each swap, of either direction, as a pass last read it at the latest block.
    latest: HashMap<B256, (Instant, Option<OnChainSwap>)>,
    head: Option<(Instant, Head)>,
    /// Swaps a finalized block holds claimed or refunded.
    finalized: HashMap<B256, (Instant, OnChainSwap)>,
    /// The latest block when each settled swap was last read at the finalized one.
    finalized_read: HashMap<B256, u64>,
    inventory: Option<(Instant, u128)>,
    /// Reverse escrows found to refund to their quoted note, which the contract never changes.
    verified_reverse: HashSet<B256>,
    reverse_phases: HashMap<B256, (Instant, Phase)>,
    unmined: HashMap<B256, Instant>,
    /// Whether the last pass found swaps that may still change on the chain.
    busy: bool,
    reconciled: Option<Instant>,
}

/// What one pass read of the swaps it watches.
pub(super) struct Reads {
    head: Option<Head>,
    latest: HashMap<B256, Option<SwapState>>,
    confirmed: HashMap<B256, Option<SwapState>>,
    confirmed_time: Option<u64>,
    finalized: HashMap<B256, OnChainSwap>,
}

/// What a pass needs of a swap past the latest block, given what the latest block shows.
pub(super) struct Confirm {
    /// Its state some confirmations deep.
    pub state: bool,
    /// That block's time.
    pub time: bool,
}

impl Reads {
    /// The latest block's time; none where every swap was finalized and nothing was read.
    pub(super) fn now(&self) -> Option<u64> {
        self.head.map(|head| head.timestamp)
    }

    /// The swap at the latest block, or as a finalized block holds it.
    pub(super) fn latest(&self, id: B256, terms: &Terms) -> Result<Option<OnChainSwap>> {
        if let Some(chain) = self.finalized.get(&id) {
            return Ok(Some(chain.clone()));
        }
        let state = self.latest.get(&id).context("the pass read no such swap")?;
        Ok(on_chain(id, *state, terms)?)
    }

    /// The swap some confirmations deep: `None` where the pass didn't read it there, `Some(None)`
    /// where it isn't there.
    pub(super) fn confirmed(&self, id: B256, terms: &Terms) -> Result<Option<Option<OnChainSwap>>> {
        if let Some(chain) = self.finalized.get(&id) {
            return Ok(Some(Some(chain.clone())));
        }
        Ok(self
            .confirmed
            .get(&id)
            .map(|state| on_chain(id, *state, terms))
            .transpose()?)
    }

    /// The time of the block the confirmed reads are of, where the pass read it.
    pub(super) fn confirmed_time(&self) -> Option<u64> {
        self.confirmed_time
    }
}

fn same(a: &OnChainSwap, b: &OnChainSwap) -> bool {
    a.stage == b.stage && a.secret == b.secret
}

impl Maker {
    /// Reads `swaps` at the latest block, and `confirmations` deep those `confirm` asks for:
    /// two calls for any number, three where a block's time is wanted, none for swaps a
    /// finalized block holds settled.
    pub(super) async fn read_swaps(
        &self,
        swaps: &[(B256, Terms)],
        confirmations: u64,
        confirm: impl Fn(B256, Option<&SwapState>, u64) -> Confirm,
    ) -> Result<Reads> {
        let finalized: HashMap<_, _> = {
            let cache = self.chain.lock().unwrap();
            swaps
                .iter()
                .filter_map(|(id, _)| Some((*id, cache.finalized.get(id)?.1.clone())))
                .collect()
        };
        let live: Vec<_> = swaps
            .iter()
            .filter(|(id, _)| !finalized.contains_key(id))
            .collect();
        let mut reads = Reads {
            head: None,
            latest: HashMap::new(),
            confirmed: HashMap::new(),
            confirmed_time: None,
            finalized,
        };
        if live.is_empty() {
            return Ok(reads);
        }
        let head = self.settlement.head().await?;
        let ids: Vec<_> = live.iter().map(|(id, _)| *id).collect();
        // As of the head itself: a decision never takes one block's time with another's state,
        // as reads that a load-balanced RPC answers from nodes a block apart would.
        let states = self
            .settlement
            .swap_states(&ids, BlockId::hash(head.hash))
            .await?;
        reads.latest = ids.iter().copied().zip(states).collect();
        reads.head = Some(head);
        let wanted: Vec<_> = ids
            .iter()
            .map(|id| (*id, confirm(*id, reads.latest[id].as_ref(), head.timestamp)))
            .collect();
        // `confirmations` deep is the block that many back counting the latest.
        if let Some(height) = head.number.checked_sub(confirmations.saturating_sub(1)) {
            let deep: Vec<_> = wanted
                .iter()
                .filter(|(_, wants)| wants.state)
                .map(|(id, _)| *id)
                .collect();
            if !deep.is_empty() {
                let states = self
                    .settlement
                    .swap_states(&deep, BlockId::number(height))
                    .await?;
                reads.confirmed = deep.into_iter().zip(states).collect();
            }
            if wanted.iter().any(|(_, wants)| wants.time) {
                reads.confirmed_time = Some(self.settlement.block_timestamp(height).await?);
            }
        }
        self.finalize(&live, &reads, head.number).await;
        let mut cache = self.chain.lock().unwrap();
        let now = Instant::now();
        cache
            .latest
            .retain(|_, (at, _)| at.elapsed() < READ_SERVES * 10);
        for (id, terms) in &live {
            if let Ok(chain) = reads.latest(*id, terms) {
                cache.latest.insert(*id, (now, chain));
            }
        }
        cache.head = Some((now, head));
        Ok(reads)
    }

    /// Adds the swaps a finalized block holds claimed or refunded, as the latest does, to those
    /// never read again: each read there once in `FINALIZED_EVERY` blocks, and only once its
    /// claim or refund is confirmed, so it settles before it is asked about.
    async fn finalize(&self, live: &[&(B256, Terms)], reads: &Reads, head: u64) {
        let candidates: Vec<_> = {
            let mut cache = self.chain.lock().unwrap();
            let read = &mut cache.finalized_read;
            read.retain(|_, at| head.saturating_sub(*at) < 10_000);
            live.iter()
                .filter_map(|(id, terms)| {
                    let latest = reads.latest(*id, terms).ok()??;
                    let confirmed = reads.confirmed(*id, terms).ok()???;
                    let due = read
                        .get(id)
                        .is_none_or(|at| head >= at.saturating_add(FINALIZED_EVERY));
                    let settled = matches!(latest.stage, Stage::Claimed | Stage::Refunded)
                        && same(&confirmed, &latest);
                    (due && settled).then(|| {
                        read.insert(*id, head);
                        (*id, terms, latest)
                    })
                })
                .collect()
        };
        if candidates.is_empty() {
            return;
        }
        let ids: Vec<_> = candidates.iter().map(|(id, _, _)| *id).collect();
        let states = match self
            .settlement
            .swap_states(&ids, BlockId::finalized())
            .await
        {
            Ok(states) => states,
            Err(e) => {
                warn!("reading settled swaps at the finalized block: {e:#}");
                return;
            }
        };
        let mut cache = self.chain.lock().unwrap();
        cache
            .finalized
            .retain(|_, (at, _)| at.elapsed() < FORGET_AFTER);
        for ((id, terms, latest), state) in candidates.into_iter().zip(states) {
            if on_chain(id, state, terms)
                .ok()
                .flatten()
                .is_some_and(|finalized| same(&finalized, &latest))
            {
                cache.finalized.insert(id, (Instant::now(), latest));
                cache.finalized_read.remove(&id);
            }
        }
    }

    /// The swap as a pass read it lately, or as a finalized block holds it; none if no pass did.
    pub(super) fn last_read(&self, id: B256) -> Option<Option<OnChainSwap>> {
        let cache = self.chain.lock().unwrap();
        if let Some((_, chain)) = cache.finalized.get(&id) {
            return Some(Some(chain.clone()));
        }
        cache
            .latest
            .get(&id)
            .filter(|(at, _)| at.elapsed() < READ_SERVES)
            .map(|(_, chain)| chain.clone())
    }

    /// The chain's clock: a pass's read of it within a few ticks, else read now.
    pub(super) async fn chain_now(&self) -> Result<u64> {
        let read = self.chain.lock().unwrap().head;
        if let Some((at, head)) = read
            && at.elapsed() < READ_SERVES
        {
            return Ok(head.timestamp);
        }
        Ok(self.settlement.now().await?)
    }

    /// The maker's inventory in the contract, read at most every half minute: its opens take
    /// from it, and clear the reading, while nothing else does.
    pub(super) async fn inventory_balance(&self) -> Result<u128, zecswap_chain::Error> {
        if let Some((at, balance)) = self.chain.lock().unwrap().inventory
            && at.elapsed() < INVENTORY_FOR
        {
            return Ok(balance);
        }
        let balance = self
            .settlement
            .balance_of(self.account, self.config.token)
            .await?;
        self.chain.lock().unwrap().inventory = Some((Instant::now(), balance));
        Ok(balance)
    }

    pub(super) fn forget_inventory(&self) {
        self.chain.lock().unwrap().inventory = None;
    }

    pub(super) fn reverse_verified(&self, id: B256) -> bool {
        self.chain.lock().unwrap().verified_reverse.contains(&id)
    }

    pub(super) fn verified(&self, id: B256) {
        self.chain.lock().unwrap().verified_reverse.insert(id);
    }

    pub(super) fn recent_phase(&self, id: B256) -> Option<Phase> {
        let cache = self.chain.lock().unwrap();
        cache
            .reverse_phases
            .get(&id)
            .filter(|(at, _)| at.elapsed() < PHASE_FOR)
            .map(|(_, phase)| *phase)
    }

    pub(super) fn remember_phase(&self, id: B256, phase: Phase) {
        let mut cache = self.chain.lock().unwrap();
        cache
            .reverse_phases
            .retain(|_, (at, _)| at.elapsed() < PHASE_FOR);
        cache.reverse_phases.insert(id, (Instant::now(), phase));
    }

    /// Whether the node had no receipt of `hash` lately.
    pub(super) fn recently_unmined(&self, hash: B256) -> bool {
        self.chain
            .lock()
            .unwrap()
            .unmined
            .get(&hash)
            .is_some_and(|at| at.elapsed() < UNMINED_RECHECK)
    }

    pub(super) fn unmined(&self, hash: B256) {
        let mut cache = self.chain.lock().unwrap();
        cache.unmined.retain(|_, at| at.elapsed() < FORGET_AFTER);
        cache.unmined.insert(hash, Instant::now());
    }

    /// Whether swaps live on the chain, so its events come often.
    pub(super) fn chain_busy(&self) -> bool {
        self.chain.lock().unwrap().busy
    }

    pub(super) fn set_chain_busy(&self, busy: bool) {
        self.chain.lock().unwrap().busy = busy;
    }

    /// Whether the newest blocks are due a second read, for events a reorganisation moved or
    /// dropped: every two minutes while swaps are live, every ten otherwise.
    pub(super) fn reconcile_due(&self) -> bool {
        let mut cache = self.chain.lock().unwrap();
        let every = Duration::from_secs(if cache.busy { 120 } else { 600 });
        if cache.reconciled.is_some_and(|at| at.elapsed() < every) {
            return false;
        }
        cache.reconciled = Some(Instant::now());
        true
    }
}
