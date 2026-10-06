//! What the maker does next for a swap, decided from what it can observe right now.
//!
//! Decisions use confirmed observations and persisted cancellation intent so a restart
//! or reorg cannot turn a cancellation back into readiness.

use anyhow::{Result, ensure};
use serde::Deserialize;
use zecswap_chain::evm::{OnChainSwap, Stage};
use zecswap_chain::zcash::Funds;

#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Timing {
    /// Seconds a quote stays acceptable.
    pub quote_ttl: u64,
    /// `t0 = open + t0_after`: from then the user may claim without `ready`.
    pub t0_after: u64,
    /// `t1 = open + t1_after`: from then a `Ready` swap may be refunded.
    pub t1_after: u64,
    /// Cancel an open swap that has received nothing for this long.
    pub cancel_after: u64,
    /// Cancel an unconfirmed deposit this long before `t0`, rather than let a claim land
    /// against ZEC that may never confirm.
    pub t0_margin: u64,
    /// Never reveal under a lock with less than this left to run.
    pub reveal_margin: u64,
    /// Seconds between watchtower passes.
    pub tick: u64,
}

impl Timing {
    /// Rejects settings under which the watchtower could not act in time.
    pub fn check(&self, lock_duration: u64) -> Result<()> {
        ensure!(
            self.t0_after < self.t1_after,
            "t1_after must exceed t0_after"
        );
        ensure!(
            0 < self.tick && self.tick < self.t0_margin,
            "tick must be positive and shorter than t0_margin"
        );
        ensure!(
            self.reveal_margin + self.tick < lock_duration,
            "the contract's {lock_duration}s lock leaves no tick to reveal in"
        );
        Ok(())
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Action {
    Wait,
    MarkReady,
    LockRefund,
    Refund,
    Sweep,
    Settle,
}

pub struct Observation {
    pub now: u64,
    pub opened_at: u64,
    pub expected_zat: u64,
    pub chain: OnChainSwap,
    pub funds: Funds,
    /// Whether the wallet is available and its last successful sync is recent.
    pub synced: bool,
    /// Whether the terminal EVM stage is also present at the required confirmation depth.
    pub chain_confirmed: bool,
    /// Local cancellation intent survives a reorg of the refund lock or reveal.
    pub cancelling: bool,
    /// `None` until a sweep is recorded, then whether it has enough confirmations.
    pub sweep_confirmed: Option<bool>,
    /// The contract's lock duration, which is also the length of each side's turn.
    pub lock_duration: u64,
}

pub fn decide(obs: &Observation, timing: &Timing) -> Action {
    let chain = &obs.chain;
    let now = obs.now;
    match chain.stage {
        Stage::Claimed if obs.synced && obs.funds.spendable > 0 => Action::Sweep,
        Stage::Claimed
            if obs.chain_confirmed && obs.synced && obs.sweep_confirmed == Some(true) =>
        {
            Action::Settle
        }
        Stage::Claimed => Action::Wait,
        Stage::Refunded if obs.chain_confirmed => Action::Settle,
        Stage::Refunded => Action::Wait,
        _ if now < chain.refund_lock_until => {
            if now + timing.reveal_margin < chain.refund_lock_until {
                Action::Refund
            } else {
                Action::Wait
            }
        }
        _ if now < chain.claim_lock_until => Action::Wait,
        // Our refund lock lapsed unused, so the next turn is the user's.
        _ if chain.refund_lock_until > chain.claim_lock_until
            && now < chain.refund_lock_until + obs.lock_duration =>
        {
            Action::Wait
        }
        // A cancellation we started can't be undone by `ready`; finish it.
        Stage::Open if obs.cancelling || chain.refund_lock_until != 0 => Action::LockRefund,
        Stage::Open => open_action(obs, timing),
        Stage::Ready if now >= chain.t1 => Action::LockRefund,
        Stage::Ready => Action::Wait,
    }
}

fn open_action(obs: &Observation, timing: &Timing) -> Action {
    let funds = obs.funds;
    if obs.synced && funds.spendable >= obs.expected_zat {
        return Action::MarkReady;
    }
    // A stale wallet can miss a deposit, so only a fresh one may conclude none came.
    let nothing_arrived =
        obs.synced && funds.total == 0 && obs.now >= obs.opened_at + timing.cancel_after;
    let underpaid = obs.synced && funds.total > 0 && funds.spendable == funds.total;
    let out_of_time = obs.now + timing.t0_margin >= obs.chain.t0;
    if nothing_arrived || underpaid || out_of_time {
        Action::LockRefund
    } else {
        Action::Wait
    }
}

#[cfg(test)]
mod tests {
    use rand::{rand_core::UnwrapErr, rngs::SysRng};
    use zecswap_chain::evm::Address;
    use zecswap_core::SecretShare;

    use super::*;

    const OPENED: u64 = 1_000_000;
    const T0: u64 = OPENED + 2_700;
    const DEPOSIT: u64 = 5_000_000;
    const LOCK: u64 = 7_200;

    fn timing() -> Timing {
        Timing {
            quote_ttl: 120,
            t0_after: 2_700,
            t1_after: 6_300,
            cancel_after: 900,
            t0_margin: 300,
            reveal_margin: 600,
            tick: 15,
        }
    }

    fn observe(stage: Stage, now: u64, (total, spendable): (u64, u64)) -> Observation {
        Observation {
            now,
            opened_at: OPENED,
            expected_zat: DEPOSIT,
            chain: OnChainSwap {
                stage,
                maker: Address::ZERO,
                user: Address::ZERO,
                token: Address::ZERO,
                amount: 1,
                t0: T0,
                t1: OPENED + 6_300,
                claim_lock_until: 0,
                refund_lock_until: 0,
                maker_share: SecretShare::random(UnwrapErr(SysRng)).public(),
                user_share: SecretShare::random(UnwrapErr(SysRng)).public(),
                secret: [0; 32],
                payout_note: None,
                paid_out: false,
            },
            funds: Funds { total, spendable },
            synced: true,
            chain_confirmed: true,
            cancelling: false,
            sweep_confirmed: None,
            lock_duration: LOCK,
        }
    }

    fn with_refund_lock(mut obs: Observation, until: u64) -> Observation {
        obs.chain.refund_lock_until = until;
        obs
    }

    #[test]
    fn a_deposit_still_confirming_is_waited_for_until_t0_gets_close() {
        for funds in [(DEPOSIT, 0), (DEPOSIT, DEPOSIT - 1)] {
            let decide_at = |now| decide(&observe(Stage::Open, now, funds), &timing());
            assert_eq!(decide_at(T0 - 301), Action::Wait);
            assert_eq!(decide_at(T0 - 300), Action::LockRefund);
        }
    }

    #[test]
    fn the_share_is_revealed_only_under_our_lock_with_time_to_spare() {
        let now = OPENED + 7_000;
        let decide_at = |until| {
            let obs = with_refund_lock(observe(Stage::Ready, now, (DEPOSIT, DEPOSIT)), until);
            decide(&obs, &timing())
        };
        assert_eq!(decide_at(now + 601), Action::Refund);
        assert_eq!(decide_at(now + 600), Action::Wait);
        assert_eq!(decide_at(now - 1), Action::Wait);
    }

    #[test]
    fn a_lapsed_lock_of_ours_gives_the_user_a_turn_before_we_lock_again() {
        let lapsed = OPENED + 7_000;
        for stage in [Stage::Open, Stage::Ready] {
            let decide_at = |now| {
                let obs = with_refund_lock(observe(stage, now, (DEPOSIT, DEPOSIT)), lapsed);
                decide(&obs, &timing())
            };
            assert_eq!(decide_at(lapsed + LOCK - 1), Action::Wait);
            assert_eq!(decide_at(lapsed + LOCK), Action::LockRefund);
        }
    }

    #[test]
    fn a_stale_wallet_never_cancels_for_a_missing_deposit_but_still_reveals() {
        let now = OPENED + 1_000;
        let mut stale = observe(Stage::Open, now, (0, 0));
        stale.synced = false;
        assert_eq!(decide(&stale, &timing()), Action::Wait);
        let held = with_refund_lock(stale, now + LOCK);
        assert_eq!(decide(&held, &timing()), Action::Refund);
    }

    #[test]
    fn stalled_zcash_still_cancels_before_t0_even_with_a_stale_balance() {
        for funds in [(0, 0), (DEPOSIT, DEPOSIT)] {
            let mut obs = observe(Stage::Open, T0 - 300, funds);
            obs.synced = false;
            assert_eq!(decide(&obs, &timing()), Action::LockRefund);
            obs.chain.refund_lock_until = obs.now + LOCK;
            assert_eq!(decide(&obs, &timing()), Action::Refund);
        }
    }

    #[test]
    fn a_reorg_cannot_undo_local_cancellation_intent() {
        let mut obs = observe(Stage::Open, OPENED + 10, (DEPOSIT, DEPOSIT));
        obs.cancelling = true;
        assert_eq!(decide(&obs, &timing()), Action::LockRefund);
    }

    #[test]
    fn terminal_stages_and_sweeps_must_be_confirmed_before_settlement() {
        let mut refund = observe(Stage::Refunded, T0, (0, 0));
        refund.chain_confirmed = false;
        assert_eq!(decide(&refund, &timing()), Action::Wait);
        refund.chain_confirmed = true;
        refund.synced = false;
        assert_eq!(decide(&refund, &timing()), Action::Settle);

        let mut claim = observe(Stage::Claimed, T0, (0, 0));
        for (evm, synced, sweep, expected) in [
            (true, true, None, Action::Wait),
            (true, true, Some(false), Action::Wait),
            (false, true, Some(true), Action::Wait),
            (true, false, Some(true), Action::Wait),
            (true, true, Some(true), Action::Settle),
        ] {
            claim.chain_confirmed = evm;
            claim.synced = synced;
            claim.sweep_confirmed = sweep;
            assert_eq!(decide(&claim, &timing()), expected);
        }
        claim.funds = Funds {
            total: DEPOSIT,
            spendable: DEPOSIT,
        };
        assert_eq!(decide(&claim, &timing()), Action::Sweep);
    }
}
