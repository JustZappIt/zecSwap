//! What the maker does next for a swap, decided from what it can observe right now.
//!
//! Decisions use confirmed observations and persisted intent, so a restart or reorg can turn
//! neither a cancellation back into readiness nor a ready into a cancellation. Every reveal is
//! judged by its window: sent only once the newest block is in it, and with `reveal_margin` to
//! spare before it ends by the later of the chain's clock and the wall clock.

use anyhow::{Result, ensure};
use serde::Deserialize;
use zecswap_chain::evm::{OnChainSwap, Reveal, Stage, Windows};
use zecswap_chain::zcash::Funds;

/// What an open must leave for its own inclusion beyond the contract's minimum window before
/// `t0`: one mined later reverts, and the swap never opens.
pub const OPEN_INCLUSION: u64 = 10 * 60;

#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Timing {
    /// Seconds a quote stays acceptable.
    pub quote_ttl: u64,
    /// `t0 = open + t0_after`: from then the user may claim without `ready`.
    pub t0_after: u64,
    /// `t1 = open + t1_after`: from then the maker refunds a swap its user hasn't claimed.
    pub t1_after: u64,
    /// Cancel an open swap that has received nothing for this long.
    pub cancel_after: u64,
    /// Cancel a deposit still unconfirmed this long before `t0`: from `t0` the user may claim,
    /// paid in or not.
    pub t0_margin: u64,
    /// Never send a reveal with less than this left before its window ends: it could land after,
    /// reverted, the maker's share public.
    pub reveal_margin: u64,
    /// Seconds between watchtower passes.
    pub tick: u64,
}

impl Timing {
    /// Rejects settings under which the watchtower could not act in time on a contract with
    /// `windows`.
    pub fn check(&self, windows: Windows) -> Result<()> {
        ensure!(
            self.t0_after < self.t1_after,
            "t1_after must exceed t0_after"
        );
        ensure!(
            0 < self.tick && self.tick < self.t0_margin,
            "tick must be positive and shorter than t0_margin"
        );
        ensure!(
            self.t0_after >= windows.min + OPEN_INCLUSION,
            "t0_after must leave the contract's {}s minimum window, and {OPEN_INCLUSION}s for the open to land, before t0",
            windows.min
        );
        ensure!(
            self.t1_after - self.t0_after >= windows.min,
            "t1 must follow t0 by the contract's {}s minimum window",
            windows.min
        );
        // Decided at t0 - t0_margin, a cancel must still leave its margin a pass later.
        ensure!(
            self.reveal_margin + self.tick < self.t0_margin,
            "t0_margin must exceed reveal_margin by a tick, or an unconfirmed deposit is never cancelled"
        );
        ensure!(
            self.reveal_margin + self.tick < windows.late,
            "the contract's {}s late window leaves no pass to refund in",
            windows.late
        );
        Ok(())
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Action {
    Wait,
    MarkReady,
    /// Reveals the maker's share: a cancel before `t0`, a refund from `t1`.
    Refund,
    Sweep,
    Settle,
}

pub struct Observation {
    /// The newest block's time, which `chain` is read as of: windows open by it.
    pub now: u64,
    /// The wall clock: windows end by the later of it and `now`.
    pub wall: u64,
    pub opened_at: u64,
    pub expected_zat: u64,
    pub chain: OnChainSwap,
    pub funds: Funds,
    /// Whether the wallet is available and its last successful sync is recent.
    pub synced: bool,
    /// Whether the terminal EVM stage is also present at the required confirmation depth.
    pub chain_confirmed: bool,
    /// A cancel of ours was started, which `ready` must never follow.
    pub cancelling: bool,
    /// A ready of ours was sent: mined first on our account, it would revert any cancel sent
    /// after it, the maker's share public.
    pub readied: bool,
    /// `None` until a sweep is recorded, then whether it has enough confirmations.
    pub sweep_confirmed: Option<bool>,
    pub windows: Windows,
}

pub fn decide(obs: &Observation, timing: &Timing) -> Action {
    let chain = &obs.chain;
    match chain.stage {
        Stage::Claimed if obs.synced && obs.funds.spendable > 0 => return Action::Sweep,
        Stage::Claimed
            if obs.chain_confirmed && obs.synced && obs.sweep_confirmed == Some(true) =>
        {
            return Action::Settle;
        }
        Stage::Refunded if obs.chain_confirmed => return Action::Settle,
        Stage::Claimed | Stage::Refunded => return Action::Wait,
        Stage::Open | Stage::Ready => {}
    }
    let (now, latest) = (obs.now, obs.now.max(obs.wall));
    let deadlines = (chain.t0, chain.t1);
    let refund = obs
        .windows
        .window(Reveal::Refund, false, chain.stage, deadlines, now);
    let refund = |wanted: bool| {
        if wanted && refund.leaves(latest, timing.reveal_margin) {
            Action::Refund
        } else {
            Action::Wait
        }
    };
    // From t1 the maker refunds, for the late window; then its user may claim again.
    if now >= chain.t1 {
        return refund(true);
    }
    // Until t1 a ready swap is its user's to claim, and from t0 an unready one too.
    if chain.stage == Stage::Ready || now >= chain.t0 {
        return Action::Wait;
    }
    // A cancellation we started can't be undone by `ready`; finish it.
    if obs.cancelling {
        return refund(true);
    }
    if obs.readied {
        return Action::Wait;
    }
    let funds = obs.funds;
    if obs.synced && funds.spendable >= obs.expected_zat {
        // A ready mined from t0 reverts, and from t0 the user claims anyway.
        return if latest + timing.reveal_margin < chain.t0 {
            Action::MarkReady
        } else {
            Action::Wait
        };
    }
    // A stale wallet can miss a deposit, so only a fresh one may conclude none came.
    let nothing_arrived =
        obs.synced && funds.total == 0 && now >= obs.opened_at + timing.cancel_after;
    let underpaid = obs.synced && funds.total > 0 && funds.spendable == funds.total;
    let out_of_time = latest + timing.t0_margin >= chain.t0;
    refund(nothing_arrived || underpaid || out_of_time)
}

#[cfg(test)]
mod tests {
    use rand::{rand_core::UnwrapErr, rngs::SysRng};
    use zecswap_chain::evm::Address;
    use zecswap_core::SecretShare;

    use super::*;

    const OPENED: u64 = 1_000_000;
    const T0: u64 = OPENED + 5_400;
    const T1: u64 = OPENED + 9_000;
    const DEPOSIT: u64 = 5_000_000;
    const WINDOWS: Windows = Windows {
        min: 3_600,
        late: 86_400,
    };

    fn timing() -> Timing {
        Timing {
            quote_ttl: 120,
            t0_after: 5_400,
            t1_after: 9_000,
            cancel_after: 900,
            t0_margin: 3_600,
            reveal_margin: 600,
            tick: 15,
        }
    }

    fn observe(stage: Stage, now: u64, (total, spendable): (u64, u64)) -> Observation {
        Observation {
            now,
            wall: now,
            opened_at: OPENED,
            expected_zat: DEPOSIT,
            chain: OnChainSwap {
                stage,
                maker: Address::ZERO,
                user: Address::ZERO,
                token: Address::ZERO,
                amount: 1,
                t0: T0,
                t1: T1,
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
            readied: false,
            sweep_confirmed: None,
            windows: WINDOWS,
        }
    }

    fn decide_at(stage: Stage, now: u64, funds: (u64, u64)) -> Action {
        decide(&observe(stage, now, funds), &timing())
    }

    #[test]
    fn the_staged_timings_fit_the_windows_and_the_old_ones_do_not() {
        timing().check(WINDOWS).unwrap();
        let lock_era = Timing {
            t0_after: 3_600,
            t1_after: 7_200,
            t0_margin: 1_800,
            ..timing()
        };
        assert!(lock_era.check(WINDOWS).is_err(), "an open could never land");
        // A cancel decided at t0 - t0_margin under a margin as long would never go out.
        let even = Timing {
            t0_margin: 600,
            ..timing()
        };
        assert!(even.check(WINDOWS).is_err());
    }

    #[test]
    fn nothing_arrived_is_cancelled_after_cancel_after_and_never_within_its_margin_of_t0() {
        assert_eq!(decide_at(Stage::Open, OPENED + 899, (0, 0)), Action::Wait);
        assert_eq!(decide_at(Stage::Open, OPENED + 900, (0, 0)), Action::Refund);
        assert_eq!(decide_at(Stage::Open, T0 - 601, (0, 0)), Action::Refund);
        assert_eq!(decide_at(Stage::Open, T0 - 600, (0, 0)), Action::Wait);
        assert_eq!(decide_at(Stage::Open, T0, (0, 0)), Action::Wait);
    }

    #[test]
    fn a_deposit_still_confirming_is_waited_for_until_t0_margin() {
        for funds in [(DEPOSIT, 0), (DEPOSIT, DEPOSIT - 1)] {
            assert_eq!(decide_at(Stage::Open, T0 - 3_601, funds), Action::Wait);
            assert_eq!(decide_at(Stage::Open, T0 - 3_600, funds), Action::Refund);
        }
    }

    #[test]
    fn a_wall_clock_ahead_of_the_chain_ends_the_cancel_early_and_never_opens_a_refund() {
        let mut obs = observe(Stage::Open, T0 - 1_000, (0, 0));
        obs.wall = T0 - 500;
        assert_eq!(decide(&obs, &timing()), Action::Wait);
        // From t1 by the wall alone the refund's window is not yet open: a claim may still land.
        let mut obs = observe(Stage::Ready, T1 - 30, (DEPOSIT, DEPOSIT));
        obs.wall = T1 + 30;
        assert_eq!(decide(&obs, &timing()), Action::Wait);
    }

    #[test]
    fn a_confirmed_deposit_is_readied_while_a_ready_can_land_before_t0() {
        let paid = (DEPOSIT, DEPOSIT);
        assert_eq!(decide_at(Stage::Open, T0 - 601, paid), Action::MarkReady);
        assert_eq!(decide_at(Stage::Open, T0 - 600, paid), Action::Wait);
        // Late and unready, it is the user's to claim from t0; the maker waits for t1.
        assert_eq!(decide_at(Stage::Open, T0, paid), Action::Wait);
    }

    #[test]
    fn a_ready_sent_is_never_followed_by_a_cancel() {
        let mut obs = observe(Stage::Open, T0 - 3_000, (0, 0));
        obs.readied = true;
        obs.synced = false;
        assert_eq!(decide(&obs, &timing()), Action::Wait);
    }

    #[test]
    fn a_cancel_started_is_finished_and_never_readied() {
        let mut obs = observe(Stage::Open, OPENED + 10, (DEPOSIT, DEPOSIT));
        obs.cancelling = true;
        assert_eq!(decide(&obs, &timing()), Action::Refund);
        obs.now = T0 - 600;
        obs.wall = obs.now;
        assert_eq!(decide(&obs, &timing()), Action::Wait);
    }

    #[test]
    fn stalled_zcash_still_cancels_before_t0_even_with_a_stale_balance() {
        for funds in [(0, 0), (DEPOSIT, DEPOSIT)] {
            let mut obs = observe(Stage::Open, T0 - 3_600, funds);
            obs.synced = false;
            assert_eq!(decide(&obs, &timing()), Action::Refund);
            obs.now = T0 - 3_601;
            obs.wall = obs.now;
            assert_eq!(
                decide(&obs, &timing()),
                Action::Wait,
                "a stale wallet waits"
            );
        }
    }

    #[test]
    fn from_t1_the_maker_refunds_for_the_late_window_then_leaves_the_swap_to_its_user() {
        for stage in [Stage::Open, Stage::Ready] {
            assert_eq!(decide_at(stage, T1 - 1, (0, 0)), Action::Wait);
            assert_eq!(decide_at(stage, T1, (0, 0)), Action::Refund);
            assert_eq!(decide_at(stage, T1 + 86_400 - 601, (0, 0)), Action::Refund);
            assert_eq!(decide_at(stage, T1 + 86_400 - 600, (0, 0)), Action::Wait);
            assert_eq!(decide_at(stage, T1 + 86_400, (0, 0)), Action::Wait);
        }
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

    /// The third audit's port spec, replayed for the maker's forward reveals: in every case its
    /// oracle decides whether a cancel, a ready or a refund may go now, the policy, asked when it
    /// would want to send one, sends it exactly then. Its claims, refunds and readies replay in
    /// `zecswap-chain`'s `windows_spec`.
    #[test]
    fn every_cancel_ready_and_refund_is_decided_as_the_port_spec_decides() {
        let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../zecswap-chain/tests/windows-spec-vectors.json");
        let vectors: Vec<serde_json::Value> =
            serde_json::from_slice(&std::fs::read(path).unwrap()).unwrap();
        let (mut replayed, mut wrong) = (0, Vec::new());
        for (index, vector) in vectors.iter().enumerate() {
            let (view, own, margins) = (&vector["view"], &vector["own"], &vector["margins"]);
            let number = |value: &serde_json::Value| value.as_u64().unwrap();
            let (reveal, margin, wanted) = match vector["reveal"].as_str().unwrap() {
                "maker_cancel" => ("cancel", number(&margins["cancel"]), Action::Refund),
                "maker_ready" => ("ready", number(&margins["ready_land"]), Action::MarkReady),
                "maker_refund" => (
                    "refund",
                    number(&margins["refund_late_stop"]),
                    Action::Refund,
                ),
                _ => continue,
            };
            replayed += 1;
            let stage = match view["stage"].as_str().unwrap() {
                "open" => Stage::Open,
                "ready" => Stage::Ready,
                "claimed" => Stage::Claimed,
                "refunded" => Stage::Refunded,
                // The maker decides only on swaps the chain holds.
                _ => {
                    if vector["expect"]["send"].as_bool().unwrap() {
                        wrong.push(format!("#{index}: a swap not on the chain"));
                    }
                    continue;
                }
            };
            let timing = Timing {
                reveal_margin: margin,
                ..timing()
            };
            let now = number(&view["head_ts"]);
            // Its deposit confirmed in full, or for a cancel, none come by `cancel_after`.
            let confirmed = view["deposit_confirmed"].as_bool().unwrap();
            let funds = if confirmed {
                (DEPOSIT, DEPOSIT)
            } else {
                (0, 0)
            };
            let mut observation = observe(stage, now, funds);
            observation.wall = number(&view["wall_now"]);
            observation.opened_at = now.saturating_sub(timing.cancel_after);
            observation.chain.t0 = number(&view["t0"]);
            observation.chain.t1 = number(&view["t1"]);
            observation.windows = Windows {
                min: number(&view["min_window"]),
                late: number(&view["late_window"]),
            };
            observation.synced = view["wallet_synced"].as_bool().unwrap();
            observation.cancelling = own["cancel_sent"].as_bool().unwrap();
            observation.readied = own["ready_sent"].as_bool().unwrap();
            let action = decide(&observation, &timing);
            let send = action == wanted && (reveal != "cancel" || now < observation.chain.t1);
            let expected = vector["expect"]["send"].as_bool().unwrap();
            if send != expected {
                wrong.push(format!(
                    "#{index} {reveal}: decides {action:?}, the spec says {expected} ({})",
                    vector["expect"]["reason"]
                ));
            }
        }
        assert!(wrong.is_empty(), "{wrong:#?}");
        assert_eq!(
            replayed, 36,
            "every cancel, ready and refund the spec decides"
        );
    }
}
