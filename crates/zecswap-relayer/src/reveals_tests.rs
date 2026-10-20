//! Claims and reverse refunds reveal a share in their calldata. Through a private RPC, as on
//! mainnet, they go to it alone and one at a time: one that may still land is sent again by no
//! one, neither the relayer that sent it nor one restarted, until its window has passed. However
//! they go, one whose lock ran under the margin while it waited its turn is refused, unsent. A
//! stand-in for Flashbots Protect reads anvil, and another, for the relayer's node, shows what
//! reached it.

use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

use alloy::providers::Provider;
use alloy::rpc::types::TransactionRequest;
use alloy_primitives::{Bytes, keccak256};
use axum::http::StatusCode;
use zecswap_api::reverse::{Authorization, Refund};
use zecswap_chain::evm::railgun::Status;
use zecswap_chain::evm::{U256, reverse_funding_calls, reverse_swap_id};
use zecswap_core::{NetworkType, ReverseOpen, UserSwapKeys, derive_maker_share, derive_user_keys};
use zecswap_railgun::ShieldNote;

use super::*;
use crate::deposits_tests::{Called, Protect, Taking, filled};
use crate::retries_tests::{Deployment, FEE, SEED, Swap, deployment, post};

/// The blocks Protect's stand-in tries a transaction for, and those after the one a reveal went
/// out in from which the relayer takes it as dropped.
const BLOCKS: u64 = 2;
const WINDOW: u64 = BLOCKS + 5;

/// The relayer's config, reading through `node` and revealing through `protect`.
fn private_config(chain: &Deployment, node: &Protect, protect: &Protect) -> Config {
    Config {
        evm_rpc: node.url.clone(),
        reveals: Some(RevealsConfig {
            private_rpc: protect.url.clone(),
        }),
        ..chain.config()
    }
}

/// Stand-ins for the relayer's node and Protect, and a relayer sending through them, its sending
/// accounts topped up.
async fn private_relayer(chain: &Deployment) -> (Protect, Protect, Arc<Relayer>) {
    let node = Protect::start(&chain.chain, "/").await;
    let protect = Protect::start(&chain.chain, &format!("/?hint=hash&blockRange={BLOCKS}")).await;
    let relayer = chain
        .relayer_with(private_config(chain, &node, &protect))
        .await;
    relayer.top_up().await.unwrap();
    (node, protect, relayer)
}

/// A swap open and ready, with the window a start leaves passed.
async fn ready(chain: &Deployment) -> Swap {
    let swap = chain.open(0).await;
    chain.mine_blocks(WINDOW).await;
    swap
}

/// The transactions Protect was given to broadcast, once there are `count`.
async fn broadcasts(protect: &Protect, count: usize) -> Vec<Called> {
    for _ in 0..300 {
        let sent = protect.calls(Some("eth_sendRawTransaction"));
        if sent.len() >= count {
            return sent;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    panic!("Protect never got broadcast {count}");
}

/// The hash of a broadcast's transaction.
fn hash(broadcast: &Called) -> B256 {
    let raw: Bytes = serde_json::from_value(broadcast.params[0].clone()).unwrap();
    keccak256(&raw)
}

/// Whether a broadcast's transaction carries `swap`'s share.
fn carries(broadcast: &Called, swap: &Swap) -> bool {
    let share = alloy_primitives::hex::encode(swap.keys.share.to_be_bytes());
    broadcast.params.to_string().contains(&share)
}

fn transaction(body: &serde_json::Value, index: usize) -> B256 {
    body["transactions"][index]
        .as_str()
        .unwrap()
        .parse()
        .unwrap()
}

async fn claimed_and_paid_out(chain: &Deployment, swap: &Swap) -> bool {
    let landed = chain
        .maker
        .swap(swap.id, &swap.terms)
        .await
        .unwrap()
        .unwrap();
    landed.stage == Stage::Claimed && landed.paid_out
}

#[tokio::test]
async fn a_reveal_goes_through_the_private_rpc_alone() {
    let Some(chain) = deployment().await else {
        return;
    };
    let (node, protect, relayer) = private_relayer(&chain).await;
    let app = api::router(relayer);
    let swap = ready(&chain).await;

    let (status, body) = post(app, "/v1/claim", chain.claim(&swap)).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert!(claimed_and_paid_out(&chain, &swap).await);
    // Protect got its chain asked at the start, and the claim: every read, nonce and simulation
    // went to the node, none of them carrying the share, as a gas estimate of the claim would.
    let methods: Vec<String> = protect
        .calls(None)
        .into_iter()
        .map(|call| call.method)
        .collect();
    assert_eq!(methods, ["eth_chainId", "eth_sendRawTransaction"]);
    assert_eq!(
        hash(&broadcasts(&protect, 1).await[0]),
        transaction(&body, 0)
    );
    // The node broadcast the payout, and saw the share nowhere.
    let public = node.calls(Some("eth_sendRawTransaction"));
    assert!(
        public
            .iter()
            .any(|sent| hash(sent) == transaction(&body, 1))
    );
    assert!(!node.calls(None).iter().any(|call| carries(call, &swap)));
}

#[tokio::test]
async fn a_reveal_that_may_still_land_is_never_sent_again_until_its_window_passes() {
    let Some(chain) = deployment().await else {
        return;
    };
    let (_node, protect, relayer) = private_relayer(&chain).await;
    let app = api::router(relayer);
    let swap = ready(&chain).await;

    protect.take(Taking::Holds);
    let first = tokio::spawn(post(app.clone(), "/v1/claim", chain.claim(&swap)));
    broadcasts(&protect, 1).await;
    // Its client gives up and posts the claim again while Protect may still land the first.
    let again = tokio::spawn(post(app.clone(), "/v1/claim", chain.claim(&swap)));
    tokio::time::sleep(Duration::from_secs(2)).await;
    assert!(!first.is_finished() && !again.is_finished());
    assert_eq!(
        protect.calls(Some("eth_sendRawTransaction")).len(),
        1,
        "sent again while the first may still land"
    );

    // Protect never lands the first, and lands what comes next.
    protect.take(Taking::Lands);
    let (status, body) = first.await.unwrap();
    assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE, "{body}");
    assert_eq!(body["code"], "unavailable");
    let (status, body) = again.await.unwrap();
    assert_eq!(status, StatusCode::OK, "{body}");
    let sent = protect.calls(Some("eth_sendRawTransaction"));
    assert_eq!(sent.len(), 2);
    // Its window counts from the block it was signed in, at most one before Protect took it.
    assert!(
        sent[1].block + 1 >= sent[0].block + WINDOW,
        "sent anew in block {}, the first in {}",
        sent[1].block,
        sent[0].block
    );
    assert_eq!(hash(&sent[1]), transaction(&body, 0));
    let dropped = chain.maker.transaction_status(hash(&sent[0])).await;
    assert_eq!(dropped.unwrap(), Status::Unknown);
    assert!(claimed_and_paid_out(&chain, &swap).await);
}

#[tokio::test]
async fn a_restarted_relayer_sends_no_reveal_until_one_sent_before_can_no_longer_land() {
    let Some(chain) = deployment().await else {
        return;
    };
    let (node, protect, before) = private_relayer(&chain).await;
    let swap = ready(&chain).await;
    protect.take(Taking::Holds);
    let _first = tokio::spawn(post(api::router(before), "/v1/claim", chain.claim(&swap)));
    broadcasts(&protect, 1).await;

    // A deploy starts the relayer again while Protect holds the claim, which no node shows.
    let after = chain
        .relayer_with(private_config(&chain, &node, &protect))
        .await;
    let app = api::router(after);
    let (status, body) = post(app.clone(), "/v1/claim", chain.claim(&swap)).await;
    assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE, "{body}");
    assert!(
        body["error"].as_str().unwrap().contains("restarted"),
        "{body}"
    );
    assert_eq!(protect.calls(Some("eth_sendRawTransaction")).len(), 1);

    // Once the window from its start has passed, the claim goes again, and lands.
    protect.take(Taking::Lands);
    chain.mine_blocks(WINDOW).await;
    let (status, body) = post(app, "/v1/claim", chain.claim(&swap)).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    let sent = protect.calls(Some("eth_sendRawTransaction"));
    assert_eq!(sent.len(), 2);
    assert_eq!(hash(&sent[1]), transaction(&body, 0));
    let landed = chain.maker.swap(swap.id, &swap.terms).await.unwrap();
    assert_eq!(landed.unwrap().stage, Stage::Claimed);
}

/// A reverse escrow, funded by the user through Relay Adapt's stand-in, as its user signs for
/// the relayer.
struct Escrow {
    id: B256,
    terms: zecswap_core::Terms,
    keys: UserSwapKeys,
    note: ShieldNote,
}

impl Escrow {
    async fn open(chain: &Deployment, index: u32) -> Self {
        let keys = derive_user_keys(&SEED, NetworkType::Test, 0, index).unwrap();
        let note = chain.wallet.note(&keys.note_entropy).unwrap();
        let maker_share = derive_maker_share(&[9; 32], index.into()).unwrap().public();
        let now = chain.maker.now().await.unwrap();
        let open = ReverseOpen {
            maker: chain.maker_account.into(),
            user: keys.auth.address(),
            token: chain.token.into(),
            amount: 500_000,
            maker_share,
            user_share: keys.share.public(),
            t0: now + 3_600,
            t1: now + 7_200,
            refund_note: note.commitment(),
            deadline: now + 300,
        };
        let funder = chain.anvil.addresses()[2];
        chain
            .maker
            .mint_test_token(chain.token, funder, open.amount)
            .await
            .unwrap();
        let signature = keys.auth.sign(&chain.domain.open_reverse(&open));
        for (to, data) in reverse_funding_calls(chain.contract, &open, &signature) {
            let request = TransactionRequest::default().to(to).input(data.into());
            let sent = chain.funder.send_transaction(request).await.unwrap();
            assert!(sent.get_receipt().await.unwrap().status());
        }
        Self {
            id: reverse_swap_id(keys.auth.address().into(), &maker_share),
            terms: open.terms(),
            keys,
            note,
        }
    }

    async fn ready(&self, chain: &Deployment) -> Authorization {
        let deadline = chain.maker.now().await.unwrap() + 60;
        Authorization {
            swap_id: self.id,
            terms: (&self.terms).into(),
            deadline,
            signature: self
                .keys
                .auth
                .sign(&chain.domain.ready(&self.id, deadline))
                .into(),
        }
    }

    fn refund(&self, chain: &Deployment) -> Refund {
        let relayer = chain.relayer_key.address().into();
        let payout = chain.domain.refund_payout(&self.id, &relayer, FEE);
        Refund {
            swap_id: self.id,
            terms: (&self.terms).into(),
            secret: self.keys.share.to_be_bytes().into(),
            payout: Payout {
                swap_id: self.id,
                terms: (&self.terms).into(),
                note: (&self.note).into(),
                fee: FEE,
                signature: self.keys.auth.sign(&payout).into(),
            },
        }
    }
}

/// Through the public mempool as on testnet: what makes them wait is a transfer of the claims
/// account's, waiting to be mined. Meanwhile the claim's window, which ends at `t1`, runs under
/// the margin, while the reverse refund's window, open for good on an unready escrow, cannot.
#[tokio::test]
async fn a_claim_whose_window_ran_under_the_margin_while_it_waited_its_turn_is_refused_unsent() {
    let Some(chain) = deployment().await else {
        return;
    };
    let relayer = chain.relayer().await;
    relayer.top_up().await.unwrap();
    let app = api::router(Arc::clone(&relayer));
    let escrow = Escrow::open(&chain, 1).await;
    let swap = chain.open(0).await;
    let claims = relayer.lane_accounts()["claims"];
    let (sent, _) = chain.sent(claims).await;

    chain.mine_every(0).await;
    let busy = tokio::spawn({
        let relayer = Arc::clone(&relayer);
        async move {
            let to = Address::repeat_byte(9);
            relayer.lanes.claims.send_eth(to, U256::from(1)).await
        }
    });
    chain.sent_reaches(claims, sent + 1).await;
    let estimates = || relayer.rpc_meter().usage().requests["eth_estimateGas"];
    let estimated = estimates();
    let claim = tokio::spawn(post(app.clone(), "/v1/claim", chain.claim(&swap)));
    let refund = tokio::spawn(post(
        app.clone(),
        "/v1/reverse/refund",
        escrow.refund(&chain),
    ));
    // Both pass their checks on arrival and their simulations, and wait for the turn.
    for _ in 0..100 {
        if estimates() >= estimated + 2 {
            break;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    assert_eq!(estimates(), estimated + 2);
    let now = swap.terms.t1 - 25;
    chain
        .chain
        .raw_request::<_, serde_json::Value>("evm_setNextBlockTimestamp".into(), (now,))
        .await
        .unwrap();
    chain.mine().await;
    busy.await.unwrap().unwrap();
    chain.mine_every(1).await;

    let (status, body) = claim.await.unwrap();
    assert_eq!(status, StatusCode::BAD_REQUEST, "{body}");
    assert_eq!(body["code"], "rejected");
    let error = body["error"].as_str().unwrap();
    assert!(error.contains("ends too soon"), "{error}");
    let (status, body) = refund.await.unwrap();
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(chain.sent(claims).await.0, sent + 2, "the claim went out");
    let forward = chain
        .maker
        .swap(swap.id, &swap.terms)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(forward.stage, Stage::Ready);
    let reverse = chain.maker.swap(escrow.id, &escrow.terms).await.unwrap();
    assert_eq!(reverse.unwrap().stage, Stage::Refunded);
}

/// A reverse refund waits for a ready the relayer sent for the same escrow while that one may
/// still land: mined first, it would revert the refund with the user's share public.
#[tokio::test]
async fn a_reverse_refund_waits_for_the_relayers_own_ready_to_land_or_fail() {
    let Some(chain) = deployment().await else {
        return;
    };
    let relayer = chain.relayer().await;
    relayer.top_up().await.unwrap();
    let app = api::router(Arc::clone(&relayer));
    let escrow = Escrow::open(&chain, 1).await;
    let readies = relayer.lane_accounts()["locks"];
    let (sent, _) = chain.sent(readies).await;
    let claims = relayer.lane_accounts()["claims"];
    let (refunds, _) = chain.sent(claims).await;

    chain.mine_every(0).await;
    let ready = tokio::spawn(post(
        app.clone(),
        "/v1/reverse/ready",
        escrow.ready(&chain).await,
    ));
    chain.sent_reaches(readies, sent + 1).await;
    // The refund posted meanwhile takes its turn after the ready's request, which waits for its
    // transaction to mine; the relayer that answered it may have restarted since.
    let restarted = chain.relayer().await;
    let (status, body) = post(
        api::router(Arc::clone(&restarted)),
        "/v1/reverse/refund",
        escrow.refund(&chain),
    )
    .await;
    assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE, "{body}");
    assert_eq!(chain.sent(claims).await.0, refunds, "the refund went out");

    chain.mine_every(1).await;
    let (status, body) = ready.await.unwrap();
    assert_eq!(status, StatusCode::OK, "{body}");
    // Ready now, the escrow is the maker's to claim until t1: the refund is refused, unsent.
    let (status, body) = post(app, "/v1/reverse/refund", escrow.refund(&chain)).await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "{body}");
    assert!(
        body["error"].as_str().unwrap().contains("from t1"),
        "{body}"
    );
    assert_eq!(chain.sent(claims).await.0, refunds);
}

#[test]
fn reveals_go_only_through_a_private_rpc_that_keeps_them_to_itself() {
    let dir = tempfile::tempdir().unwrap();
    let load = |network: &str, private_rpc: Option<&str>| {
        let mut text = format!(
            r#"
            network = "{network}"
            evm_rpc = "http://127.0.0.1:8545"
            contract = "0x2222222222222222222222222222222222222222"
            token = "0x3333333333333333333333333333333333333333"
            maker = "0x4444444444444444444444444444444444444444"
            listen = "127.0.0.1:8788"
            fee = 100000
            claim_margin = 300
            "#
        );
        if let Some(url) = private_rpc {
            text.push_str(&format!("[reveals]\nprivate_rpc = \"{url}\"\n"));
        }
        let path = dir.path().join("relayer.toml");
        std::fs::write(&path, text).unwrap();
        Config::load(&path).map_err(|error| format!("{error:#}"))
    };
    // Flashbots' Sepolia relay lands nothing, so testnet may go without; mainnet may not.
    assert!(load("testnet", None).is_ok());
    let refused = load("mainnet", None).err().unwrap();
    assert!(refused.contains("[reveals] private_rpc"), "{refused}");
    let builders = "https://rpc.flashbots.net?hint=hash&builder=Titan&builder=Quasar&builder=Eureka&blockRange=10";
    let config = load("mainnet", Some(builders)).unwrap();
    assert_eq!(config.reveals.unwrap().private_rpc, builders);

    for (url, why) in [
        (
            "https://rpc.flashbots.net/fast?hint=hash&blockRange=10",
            "TEE searchers see whole transactions, failed ones too",
        ),
        (
            "https://rpc.flashbots.net?blockRange=10",
            "its default hints share logs",
        ),
        (
            "https://rpc.flashbots.net?hint=hash&hint=default_logs",
            "logs",
        ),
        ("https://rpc.flashbots.net?hint=calldata", "calldata"),
        (
            "https://rpc.flashbots.net?hint=hash&HINT=logs",
            "names in any case",
        ),
        (
            "https://rpc.flashbots.net?hint=hash&canRevert=true",
            "it lands reverts",
        ),
        (
            "https://rpc.flashbots.net?hint=hash&useMempool=true",
            "the public mempool",
        ),
        (
            "https://rpc.flashbots.net?hint=hash&allowTee=true",
            "TEE searchers",
        ),
        (
            "https://rpc.flashbots.net?hint=hash&blockRange=10&blockrange=50",
            "which range it tries",
        ),
        (
            "https://rpc.flashbots.net?hint=hash&blockRange=0",
            "no range",
        ),
    ] {
        let refused = load("mainnet", Some(url)).err().unwrap_or_default();
        assert!(refused.contains("private RPC"), "{why}: {refused}");
    }
    // A refusal never echoes the URL, which can carry a key.
    for url in [
        "https://rpc.flashbots.net/SECRET?hint=hash",
        "https://rpc.flashbots.net?hint=hash&key=SECRET",
        "https://rpc.flashbots.net?hint=SECRET",
        "https://rpc.flashbots.net?hint=hash&blockRange=SECRET",
    ] {
        let refused = load("mainnet", Some(url)).err().unwrap_or_default();
        assert!(
            refused.contains("private RPC") && !refused.contains("SECRET"),
            "{refused}"
        );
    }

    // The relayer staged for mainnet loads.
    let deploy = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../deploy");
    let staged = std::fs::read_to_string(deploy.join("mainnet/relayer.toml")).unwrap();
    let path = dir.path().join("mainnet.toml");
    std::fs::write(&path, filled(&staged)).unwrap();
    let config = Config::load(&path).unwrap();
    assert_eq!(config.reveals.unwrap().private_rpc, builders);
}
