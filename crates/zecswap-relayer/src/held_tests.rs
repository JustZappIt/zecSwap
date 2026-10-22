//! A claim handed over before its window opens is held: checked as `/v1/claim` checks one, all but
//! its window, kept through a restart, and sent the moment the window opens, on a ready two
//! blocks deep or at `t0`, at the fee it was held with, once however often it is posted or looked
//! at. A refunded swap's is refused, or dropped where held, its share deleted with it.

use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

use alloy::providers::Provider;
use axum::http::StatusCode;
use serde_json::json;
use zecswap_chain::evm::judge::READY_DEPTH;
use zecswap_core::{NetworkType, derive_maker_share, derive_user_keys};

use super::*;
use crate::retries_tests::{Deployment, FEE, SEED, Swap, deployment, post};

/// Claims held in a journal in `dir`.
pub(crate) fn held_in(dir: &Path) -> Option<HeldClaimsConfig> {
    Some(HeldClaimsConfig {
        journal: dir.join("held-claims.sqlite"),
    })
}

/// A relayer process holding claims in `dir`, its sending accounts topped up.
async fn holding(chain: &Deployment, dir: &Path) -> Arc<Relayer> {
    let config = Config {
        held_claims: held_in(dir),
        ..chain.config()
    };
    let relayer = chain.relayer_with(config).await;
    relayer.top_up().await.unwrap();
    relayer
}

async fn hold(relayer: &Arc<Relayer>, claim: &Claim) -> (StatusCode, serde_json::Value) {
    post(api::router(Arc::clone(relayer)), "/v1/claim/hold", claim).await
}

fn held(relayer: &Relayer) -> &Held {
    relayer.held.as_ref().unwrap()
}

/// What the monitor shows of the claims held.
fn shown(relayer: &Relayer) -> serde_json::Value {
    serde_json::to_value(relayer.monitor_snapshot()).unwrap()["heldClaims"].clone()
}

/// Looks at the claims held, as the relayer's task does, until it holds none.
pub(crate) async fn until_none_held(relayer: &Arc<Relayer>) {
    for _ in 0..150 {
        relayer.held_pass().await.unwrap();
        if held(relayer).count() == 0 {
            return;
        }
        tokio::time::sleep(Duration::from_millis(200)).await;
    }
    panic!("a claim is still held");
}

/// The swap's claim, its payout keeping `fee`.
fn claim_paying(chain: &Deployment, swap: &Swap, fee: u128) -> Claim {
    let relayer = chain.relayer_key.address().into();
    let mut claim = chain.claim(swap);
    claim.payout.fee = fee;
    claim.payout.signature = swap
        .keys
        .auth
        .sign(&chain.domain.payout(&swap.id, &relayer, fee))
        .into();
    claim
}

/// Readies the swap in a block of its own, mining held back from then on: the ready's block.
async fn ready_alone(chain: &Deployment, swap: &Swap) -> u64 {
    chain.mine_every(0).await;
    let (sent, _) = chain.sent(chain.maker_account).await;
    let mined = async {
        chain.sent_reaches(chain.maker_account, sent + 1).await;
        chain.mine().await;
    };
    let (ready, ()) = tokio::join!(chain.maker.ready(swap.id, &swap.terms), mined);
    let receipt = chain.chain.get_transaction_receipt(ready.unwrap()).await;
    receipt.unwrap().unwrap().block_number.unwrap()
}

/// Looks at the claims held in a task of its own, as the relayer's does.
fn look(relayer: &Arc<Relayer>) -> tokio::task::JoinHandle<Result<()>> {
    let relayer = Arc::clone(relayer);
    tokio::spawn(async move { relayer.held_pass().await })
}

async fn claimed_and_paid_out(chain: &Deployment, swap: &Swap) -> bool {
    let landed = chain.maker.swap(swap.id, &swap.terms).await.unwrap();
    landed.is_some_and(|landed| landed.stage == Stage::Claimed && landed.paid_out)
}

async fn refund(chain: &Deployment, swap: &Swap, index: u64) {
    let share = derive_maker_share(&[9; 32], index).unwrap();
    chain
        .maker
        .refund(swap.id, &swap.terms, &share)
        .await
        .unwrap();
}

#[tokio::test]
async fn a_claim_held_before_the_ready_goes_once_the_ready_is_two_blocks_deep() {
    let Some(chain) = deployment().await else {
        return;
    };
    let dir = tempfile::tempdir().unwrap();
    let relayer = holding(&chain, dir.path()).await;
    let claims = relayer.lane_accounts()["claims"];
    let swap = chain.opened(0).await;
    let (sent, _) = chain.sent(claims).await;

    let (status, body) = hold(&relayer, &chain.claim(&swap)).await;
    assert_eq!((status, body), (StatusCode::OK, json!({"held": true})));
    assert_eq!(shown(&relayer), json!({"held": 1, "sent": 0, "dropped": 0}));
    // Unready before t0, the swap is its maker's to cancel.
    relayer.held_pass().await.unwrap();
    let ready = ready_alone(&chain, &swap).await;
    for _ in 0..READY_DEPTH {
        relayer.held_pass().await.unwrap();
        assert_eq!(chain.sent(claims).await.0, sent, "sent on a shallow ready");
        chain.mine().await;
    }
    // Two blocks deep, the claim goes in the block it was judged at.
    let pass = look(&relayer);
    chain.sent_reaches(claims, sent + 1).await;
    let head = chain.maker.head().await.unwrap();
    assert_eq!(head.number, ready + READY_DEPTH);
    chain.mine_every(1).await;
    pass.await.unwrap().unwrap();
    assert!(claimed_and_paid_out(&chain, &swap).await);
    // Paid out, it is deleted.
    until_none_held(&relayer).await;
    assert_eq!(shown(&relayer), json!({"held": 0, "sent": 1, "dropped": 0}));
    assert_eq!(chain.sent(claims).await, (sent + 1, sent + 1));
}

#[tokio::test]
async fn a_claim_held_on_a_silent_maker_goes_at_t0() {
    let Some(chain) = deployment().await else {
        return;
    };
    let dir = tempfile::tempdir().unwrap();
    let relayer = holding(&chain, dir.path()).await;
    let claims = relayer.lane_accounts()["claims"];
    let swap = chain.opened(0).await;
    let (sent, _) = chain.sent(claims).await;
    let (status, _) = hold(&relayer, &chain.claim(&swap)).await;
    assert_eq!(status, StatusCode::OK);

    chain.mine_every(0).await;
    let mine_at = async |time: u64| {
        chain
            .chain
            .raw_request::<_, serde_json::Value>("evm_setNextBlockTimestamp".into(), (time,))
            .await
            .unwrap();
        chain.mine().await;
    };
    mine_at(swap.terms.t0 - 1).await;
    relayer.held_pass().await.unwrap();
    assert_eq!(chain.sent(claims).await.0, sent, "sent before t0, unready");
    // The maker never readied it: from t0 it is its user's to claim.
    mine_at(swap.terms.t0).await;
    let pass = look(&relayer);
    chain.sent_reaches(claims, sent + 1).await;
    assert_eq!(chain.maker.now().await.unwrap(), swap.terms.t0);
    chain.mine_every(1).await;
    pass.await.unwrap().unwrap();
    assert!(claimed_and_paid_out(&chain, &swap).await);
    until_none_held(&relayer).await;
    assert_eq!(shown(&relayer), json!({"held": 0, "sent": 1, "dropped": 0}));
}

#[tokio::test]
async fn a_hold_for_a_refunded_swap_or_one_failing_a_check_is_refused_and_nothing_held() {
    let Some(chain) = deployment().await else {
        return;
    };
    let dir = tempfile::tempdir().unwrap();
    let relayer = holding(&chain, dir.path()).await;
    let swap = chain.opened(0).await;
    let refused = async |relayer: &Arc<Relayer>, claim: &Claim, why: &str| {
        let (status, body) = hold(relayer, claim).await;
        assert_eq!(status, StatusCode::BAD_REQUEST, "{why}: {body}");
        assert_eq!(body["code"], "rejected");
        let error = body["error"].as_str().unwrap();
        assert!(error.contains(why), "{why}: {error}");
    };

    let mut split = chain.claim(&swap);
    split.payout.swap_id = B256::repeat_byte(1);
    refused(&relayer, &split, "different swaps or terms").await;
    let mut stranger = chain.claim(&swap);
    let other = derive_user_keys(&SEED, NetworkType::Test, 0, 9).unwrap();
    stranger.secret = other.share.to_be_bytes().into();
    refused(&relayer, &stranger, "not the swap's").await;
    let cheap = claim_paying(&chain, &swap, FEE - 1);
    refused(&relayer, &cheap, "less than the relayer's").await;
    let mut forged = chain.claim(&swap);
    forged.payout.signature = other
        .auth
        .sign(&chain.domain.payout(&swap.id, &relayer.account.into(), FEE))
        .into();
    refused(&relayer, &forged, "not signed by the swap's user").await;
    // A relayer without a journal holds none.
    let unheld = chain.relayer().await;
    refused(&unheld, &chain.claim(&swap), "holds no claims").await;
    assert_eq!(held(&relayer).count(), 0);

    // The maker cancelled the swap: its claim can never go.
    refund(&chain, &swap, 0).await;
    refused(&relayer, &chain.claim(&swap), "Refunded").await;
    assert_eq!(shown(&relayer), json!({"held": 0, "sent": 0, "dropped": 0}));
    let operations = &serde_json::to_value(relayer.monitor_snapshot()).unwrap()["operations"];
    assert_eq!(operations["hold_claim"]["refused"], 5);
}

#[tokio::test]
async fn a_claim_held_for_a_swap_refunded_meanwhile_is_dropped_its_share_deleted() {
    let Some(chain) = deployment().await else {
        return;
    };
    let dir = tempfile::tempdir().unwrap();
    let relayer = holding(&chain, dir.path()).await;
    let claims = relayer.lane_accounts()["claims"];
    let swap = chain.opened(0).await;
    let (sent, _) = chain.sent(claims).await;
    let (status, _) = hold(&relayer, &chain.claim(&swap)).await;
    assert_eq!(status, StatusCode::OK);
    let journal = dir.path().join("held-claims.sqlite");
    let share = alloy_primitives::hex::encode(swap.keys.share.to_be_bytes());
    let kept = || {
        let bytes = std::fs::read(&journal).unwrap();
        bytes.windows(share.len()).any(|at| at == share.as_bytes())
    };
    assert!(kept(), "the journal holds the claim");

    refund(&chain, &swap, 0).await;
    relayer.held_pass().await.unwrap();
    assert_eq!(shown(&relayer), json!({"held": 0, "sent": 0, "dropped": 1}));
    assert!(!kept(), "the share outlived its claim in the journal");
    assert_eq!(chain.sent(claims).await.0, sent);
}

#[tokio::test]
async fn a_held_claim_outlives_a_restart() {
    let Some(chain) = deployment().await else {
        return;
    };
    let dir = tempfile::tempdir().unwrap();
    let before = holding(&chain, dir.path()).await;
    let swap = chain.opened(0).await;
    let (status, _) = hold(&before, &chain.claim(&swap)).await;
    assert_eq!(status, StatusCode::OK);
    drop(before);

    // A deploy starts the relayer again before the window opens: it holds the claim still.
    let after = holding(&chain, dir.path()).await;
    assert_eq!(shown(&after), json!({"held": 1, "sent": 0, "dropped": 0}));
    chain.maker.ready(swap.id, &swap.terms).await.unwrap();
    chain.mine_blocks(READY_DEPTH).await;
    until_none_held(&after).await;
    assert!(claimed_and_paid_out(&chain, &swap).await);
    assert_eq!(shown(&after), json!({"held": 0, "sent": 1, "dropped": 0}));
}

#[tokio::test]
async fn a_held_claim_pays_the_fee_it_was_held_with_after_that_quote_expired() {
    let Some(chain) = deployment().await else {
        return;
    };
    let dir = tempfile::tempdir().unwrap();
    let prices = zecswap_prices::stand_in::start(StatusCode::OK).await;
    let pricing = crate::pricing::GasPricing::new(
        &[zecswap_prices::Provider::CoinMarketCap],
        &zecswap_prices::stand_in::keys(),
        1_000,
    )
    .unwrap()
    .served_by(&prices.url);
    pricing.cache_gas_price(crate::sends::now(), 1_000_000_000);
    let config = Config {
        fee_gas: 100_000,
        held_claims: held_in(dir.path()),
        ..chain.config()
    };
    let relayer = Arc::try_unwrap(chain.relayer_with(config).await)
        .ok()
        .unwrap();
    let relayer = Arc::new(Relayer {
        pricing: Some(pricing),
        ..relayer
    });
    relayer.top_up().await.unwrap();
    // 100,000 gas at 1 gwei is 266,666 at 2,666.67 USDC an ETH.
    let quoted = relayer.terms().await.fee;
    assert_eq!(quoted, 266_666);
    let swap = chain.opened(0).await;
    let claim = claim_paying(&chain, &swap, quoted);
    let (status, body) = hold(&relayer, &claim).await;
    assert_eq!(status, StatusCode::OK, "{body}");

    // An hour on, the quote has expired and gas costs twice as much.
    let pricing = relayer.pricing.as_ref().unwrap();
    let now = crate::sends::now();
    for quote in pricing.payouts.0.lock().unwrap().iter_mut() {
        quote.0 = now - 3_601;
    }
    pricing.cache_gas_price(now, 2_000_000_000);
    chain.maker.ready(swap.id, &swap.terms).await.unwrap();
    chain.mine_blocks(READY_DEPTH).await;
    let (status, body) = post(api::router(Arc::clone(&relayer)), "/v1/claim", &claim).await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "{body}");
    let error = body["error"].as_str().unwrap();
    assert!(error.contains("less than the relayer's 533333"), "{error}");

    // Held, it goes at that fee all the same.
    let earned = || chain.maker.token_balance(chain.token, relayer.account);
    let before = earned().await.unwrap();
    until_none_held(&relayer).await;
    assert!(claimed_and_paid_out(&chain, &swap).await);
    assert_eq!(earned().await.unwrap() - before, quoted);
}

#[tokio::test]
async fn the_same_hold_twice_is_held_once_and_its_claim_sent_once() {
    let Some(chain) = deployment().await else {
        return;
    };
    let dir = tempfile::tempdir().unwrap();
    let relayer = holding(&chain, dir.path()).await;
    let app = api::router(Arc::clone(&relayer));
    let claims = relayer.lane_accounts()["claims"];
    let swap = chain.opened(0).await;
    for _ in 0..2 {
        let (status, body) = hold(&relayer, &chain.claim(&swap)).await;
        assert_eq!((status, body), (StatusCode::OK, json!({"held": true})));
    }
    assert_eq!(held(&relayer).count(), 1);
    // A new payout, signed for another fee the relayer honors, takes the held one's place.
    let replaced = claim_paying(&chain, &swap, FEE + 1);
    let (status, _) = hold(&relayer, &replaced).await;
    assert_eq!(status, StatusCode::OK);
    let kept = held(&relayer).claim(swap.id).unwrap().unwrap();
    assert_eq!((held(&relayer).count(), kept.payout.fee), (1, FEE + 1));

    chain.maker.ready(swap.id, &swap.terms).await.unwrap();
    chain.mine_blocks(READY_DEPTH).await;
    let (sent, _) = chain.sent(claims).await;
    let earned = || chain.maker.token_balance(chain.token, relayer.account);
    let before = earned().await.unwrap();
    chain.mine_every(0).await;
    let pass = look(&relayer);
    chain.sent_reaches(claims, sent + 1).await;
    // Its app comes back meanwhile and posts it again, held and as a claim: each waits its turn.
    let again = tokio::spawn(post(app.clone(), "/v1/claim/hold", replaced.clone()));
    let claimed = tokio::spawn(post(app.clone(), "/v1/claim", replaced.clone()));
    tokio::time::sleep(Duration::from_millis(500)).await;
    assert!(!again.is_finished() && !claimed.is_finished());
    assert_eq!(chain.sent(claims).await.0, sent + 1);

    chain.mine_every(1).await;
    pass.await.unwrap().unwrap();
    for request in [again, claimed] {
        let (status, body) = request.await.unwrap();
        assert_eq!(
            (status, body),
            (StatusCode::OK, json!({"transactions": []}))
        );
    }
    assert_eq!(chain.sent(claims).await, (sent + 1, sent + 1));
    assert_eq!(earned().await.unwrap() - before, FEE + 1);
    until_none_held(&relayer).await;
    assert_eq!(shown(&relayer), json!({"held": 0, "sent": 1, "dropped": 0}));
}

#[test]
fn holding_claims_is_opt_in() {
    let example = include_str!("../relayer.example.toml");
    let config: Config = toml::from_str(example).unwrap();
    assert!(config.held_claims.is_none());
    let journal = "/var/lib/zecswap-relayer/held-claims.sqlite";
    let held = format!("{example}\n[held_claims]\njournal = \"{journal}\"\n");
    let config: Config = toml::from_str(&held).unwrap();
    assert_eq!(config.held_claims.unwrap().journal, Path::new(journal));
}
