//! The third audit's port spec, replayed: in every case its oracle decides (`vectors.json`, which
//! `port_spec.py` writes), whether a party may reveal now on one read, the judgements the relayer
//! and clients call agree. The maker's forward reveals replay in `zecswap-maker`'s policy tests.

use std::path::Path;

use serde::Deserialize;
use zecswap_chain::evm::judge::{self, Go};
use zecswap_chain::evm::{B256, Head, Stage, Windows};

#[derive(Deserialize)]
pub struct Vector {
    pub reveal: String,
    pub view: View,
    pub own: Own,
    pub margins: Margins,
    pub expect: Expect,
}

/// One read: the swap at the newest block, that block, and the wall clock.
#[derive(Deserialize)]
pub struct View {
    pub direction: String,
    pub stage: String,
    pub head_ts: u64,
    pub head_number: u64,
    pub wall_now: u64,
    pub t0: u64,
    pub t1: u64,
    pub min_window: u64,
    pub late_window: u64,
    pub ready_block: Option<u64>,
    /// -1 where the node gives no finalized block.
    pub finalized_number: i64,
    pub railgun_accepts: bool,
    pub deposit_confirmed: bool,
    pub wallet_synced: bool,
}

/// What the party recorded before sending.
#[derive(Deserialize)]
pub struct Own {
    pub ready_sent: bool,
    pub cancel_sent: bool,
    pub refund_sent: bool,
    pub ready_sig_deadlines: Vec<u64>,
    pub own_ready_pending: bool,
}

#[derive(Deserialize)]
pub struct Margins {
    pub cancel: u64,
    pub refund_late_stop: u64,
    pub claim_app: u64,
    pub claim_relayer: u64,
    pub ready_land: u64,
    pub ready_sig_ttl: u64,
    pub ready_depth: u64,
    pub reverse_ready_depth: u64,
}

#[derive(Deserialize)]
pub struct Expect {
    pub send: bool,
    pub reason: String,
}

fn vectors() -> Vec<Vector> {
    let path = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/windows-spec-vectors.json");
    serde_json::from_slice(&std::fs::read(path).unwrap()).unwrap()
}

/// The swap's stage, or none where it isn't on the chain.
fn stage(stage: &str) -> Option<Stage> {
    match stage {
        "open" => Some(Stage::Open),
        "ready" => Some(Stage::Ready),
        "claimed" => Some(Stage::Claimed),
        "refunded" => Some(Stage::Refunded),
        "none" => None,
        other => panic!("no stage {other}"),
    }
}

/// Whether the reveal goes now, as the relayer and clients judge it, or none for the maker's
/// forward reveals.
fn may_send(vector: &Vector) -> Option<bool> {
    let (view, own, margins) = (&vector.view, &vector.own, &vector.margins);
    let windows = Windows {
        min: view.min_window,
        late: view.late_window,
    };
    let head = Head {
        number: view.head_number,
        timestamp: view.head_ts,
        hash: B256::ZERO,
    };
    let deadlines = (view.t0, view.t1);
    let forward = view.direction == "forward";
    // The swap, read as of `block`, is ready there.
    let ready_at = |block: u64| view.ready_block.is_some_and(|ready| ready <= block);
    let reveal = vector.reveal.as_str();
    if matches!(reveal, "maker_ready" | "maker_cancel" | "maker_refund") {
        return None;
    }
    // No swap on the chain: nothing to reveal.
    let Some(stage) = stage(&view.stage) else {
        return Some(false);
    };
    Some(match reveal {
        "user_claim" | "relayer_claim" => {
            let margin = if reveal == "user_claim" {
                margins.claim_app
            } else {
                margins.claim_relayer
            };
            let judged =
                judge::forward_claim(windows, stage, deadlines, &head, view.wall_now, margin);
            forward
                && view.railgun_accepts
                && match judged {
                    Ok(Go::Now) => true,
                    Ok(Go::OnceReadyAt(block)) => ready_at(block),
                    Err(_) => false,
                }
        }
        "maker_claim" => {
            let judged = judge::reverse_claim(
                windows,
                stage,
                deadlines,
                &head,
                view.wall_now,
                margins.claim_relayer,
            );
            !forward
                && match judged {
                    Ok(Go::Now) => true,
                    Ok(Go::OnceReadyAt(block)) => ready_at(block),
                    Err(_) => false,
                }
        }
        "user_refund" | "relayer_refund" => {
            let pending = reveal == "relayer_refund" && own.own_ready_pending;
            let signed_until = own.ready_sig_deadlines.iter().max().copied();
            !forward
                && judge::reverse_refund(windows, stage, deadlines, &head, signed_until, pending)
                    .is_ok()
        }
        "user_ready" => {
            !forward
                && !own.refund_sent
                && view.deposit_confirmed
                && judge::reverse_ready(stage, view.t0, &head, margins.ready_sig_ttl).is_ok()
        }
        other => panic!("no reveal {other}"),
    })
}

#[test]
fn every_claim_refund_and_ready_is_judged_as_the_port_spec_decides() {
    let vectors = vectors();
    assert_eq!(vectors.len(), 97);
    let mut wrong = Vec::new();
    let mut replayed = 0;
    for (index, vector) in vectors.iter().enumerate() {
        // The depths the spec decides on are the ones the services wait for.
        assert_eq!(vector.margins.ready_depth, judge::READY_DEPTH);
        assert_eq!(
            vector.margins.reverse_ready_depth,
            judge::REVERSE_READY_DEPTH
        );
        let Some(send) = may_send(vector) else {
            continue;
        };
        replayed += 1;
        if send != vector.expect.send {
            wrong.push(format!(
                "#{index} {}: sends {send}, the spec says {} ({})",
                vector.reveal, vector.expect.send, vector.expect.reason
            ));
        }
    }
    assert!(wrong.is_empty(), "{wrong:#?}");
    assert_eq!(
        replayed, 61,
        "every claim, refund and ready the spec decides"
    );
}
