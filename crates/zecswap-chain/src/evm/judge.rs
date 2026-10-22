//! What a party may reveal now, judged on one read of the chain: a swap's stage and deadlines as of
//! the newest block, that block's time and number, and the wall clock. A window opens by the
//! block's time; how much of it is left goes by the later of the block's time and the wall clock,
//! as either can only make a party act earlier. The third audit's port spec answers each case, and
//! `tests/windows_spec.rs` replays its vectors.

use super::{Head, Reveal, Stage, Window, Windows};

/// Blocks a ready must have above it before a forward claim is revealed on it ahead of `t0`: a
/// claim sent on a ready a reorganisation drops lands reverted, its share public, the swap the
/// maker's to cancel.
pub const READY_DEPTH: u64 = 2;
/// Blocks above a reverse escrow's ready that make it final where the finalized block doesn't hold
/// it yet: the maker's claim is sent only on a final ready, as its user can't sign one again.
pub const FINAL_DEPTH: u64 = 64;

/// Why a reveal isn't sent now.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Refusal {
    /// Its window is shut: the swap is the other side's now, or no one's.
    Shut,
    /// From `t1` a forward swap is its maker's to refund; its claim reopens at this time.
    MakersUntil(u64),
    /// Its window ends within the margin: sent now, it might land after and revert.
    TooSoon,
    /// A ready of ours may land first, and would revert it with the share public.
    ReadyMayLand,
}

/// When a reveal may go.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Go {
    Now,
    /// Once the swap, read as of this block, is ready there: deep enough before `t0` for a forward
    /// claim; for a reverse claim, that or ready as of the finalized block.
    OnceReadyAt(u64),
}

/// A forward claim, the user's or a relayer's for it, with `margin` left in its window.
pub fn forward_claim(
    windows: Windows,
    stage: Stage,
    (t0, t1): (u64, u64),
    head: &Head,
    wall: u64,
    margin: u64,
) -> Result<Go, Refusal> {
    let now = head.timestamp;
    let window = windows.window(Reveal::Claim, false, stage, (t0, t1), now);
    if window == Window::Shut {
        return Err(match stage {
            Stage::Open | Stage::Ready if now >= t1 => {
                Refusal::MakersUntil(t1.saturating_add(windows.late))
            }
            _ => Refusal::Shut,
        });
    }
    if !window.leaves(now.max(wall), margin) {
        return Err(Refusal::TooSoon);
    }
    Ok(if now < t0 {
        Go::OnceReadyAt(head.number.saturating_sub(READY_DEPTH))
    } else {
        Go::Now
    })
}

/// A reverse escrow's claim, its maker's, with `margin` left in its window.
pub fn reverse_claim(
    windows: Windows,
    stage: Stage,
    (t0, t1): (u64, u64),
    head: &Head,
    wall: u64,
    margin: u64,
) -> Result<Go, Refusal> {
    let window = windows.window(Reveal::Claim, true, stage, (t0, t1), head.timestamp);
    if window == Window::Shut {
        return Err(Refusal::Shut);
    }
    if !window.leaves(head.timestamp.max(wall), margin) {
        return Err(Refusal::TooSoon);
    }
    Ok(Go::OnceReadyAt(head.number.saturating_sub(FINAL_DEPTH)))
}

/// A reverse escrow's refund, its user's or a relayer's for it: `ready_signed_until` is the
/// latest deadline of a ready its user signed, and `ready_may_land` whether one the sender sent may
/// still land.
pub fn reverse_refund(
    windows: Windows,
    stage: Stage,
    (t0, t1): (u64, u64),
    head: &Head,
    ready_signed_until: Option<u64>,
    ready_may_land: bool,
) -> Result<(), Refusal> {
    let now = head.timestamp;
    if windows.window(Reveal::Refund, true, stage, (t0, t1), now) == Window::Shut {
        return Err(Refusal::Shut);
    }
    let signed =
        stage == Stage::Open && now < t0 && ready_signed_until.is_some_and(|until| now <= until);
    if signed || ready_may_land {
        return Err(Refusal::ReadyMayLand);
    }
    Ok(())
}

/// A reverse escrow's ready, signed to be good for `ttl` seconds: only on an escrow not yet ready,
/// and with time to land before `t0`, from which none is taken.
pub fn reverse_ready(stage: Stage, t0: u64, head: &Head, ttl: u64) -> Result<(), Refusal> {
    if stage != Stage::Open {
        return Err(Refusal::Shut);
    }
    if head.timestamp.saturating_add(ttl) >= t0 {
        return Err(Refusal::TooSoon);
    }
    Ok(())
}
