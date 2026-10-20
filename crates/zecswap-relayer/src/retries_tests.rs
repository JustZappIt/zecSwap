//! A claim or payout posted again while the first is on its way, whether its client gave up
//! waiting or the relayer restarted, sends nothing more and answers once that one lands: never a
//! second transaction, never a 500. Mining is paused to hold the first in the node's pool.

use std::path::Path;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

use alloy::network::{EthereumWallet, TransactionBuilder};
use alloy::node_bindings::{Anvil, AnvilInstance};
use alloy::providers::{DynProvider, Provider, ProviderBuilder};
use alloy::rpc::types::TransactionRequest;
use axum::body::{Body, to_bytes};
use axum::http::{Request, StatusCode};
use tower::ServiceExt;
use zecswap_chain::evm::{U256, deploy, swap_id};
use zecswap_core::{NetworkType, UserSwapKeys, derive_maker_share, derive_user_keys};

use super::*;
use crate::turns::Step;

pub(crate) const SEED: [u8; 64] = [7; 64];
pub(crate) const FEE: u128 = 20_000;

pub(crate) struct Deployment {
    pub(crate) anvil: AnvilInstance,
    pub(crate) url: String,
    pub(crate) chain: DynProvider,
    /// Anvil's third account, which sends what neither the maker nor the relayer does.
    pub(crate) funder: DynProvider,
    pub(crate) maker: Settlement,
    pub(crate) maker_account: Address,
    pub(crate) contract: Address,
    pub(crate) token: Address,
    pub(crate) domain: Domain,
    pub(crate) relayer_key: PrivateKeySigner,
    pub(crate) wallet: zecswap_railgun::Keys,
}

/// A swap open and ready, its ready deep enough to claim on, paying into the user's Railgun
/// wallet.
pub(crate) struct Swap {
    pub(crate) id: B256,
    pub(crate) terms: zecswap_core::Terms,
    pub(crate) keys: UserSwapKeys,
}

pub(crate) async fn deployment() -> Option<Deployment> {
    let Ok(anvil) = Anvil::new().block_time(1).try_spawn() else {
        eprintln!("skipped: anvil is not installed");
        return None;
    };
    let artifacts = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../contracts/out");
    if !artifacts.join("MockRailgun.sol").exists() {
        eprintln!("skipped: run `forge build` in contracts/");
        return None;
    }
    let creation = |name: &str| {
        let artifact: serde_json::Value = serde_json::from_slice(
            &std::fs::read(artifacts.join(format!("{name}.sol/{name}.json"))).unwrap(),
        )
        .unwrap();
        alloy_primitives::hex::decode(artifact["bytecode"]["object"].as_str().unwrap()).unwrap()
    };
    let url = anvil.endpoint();
    let maker_key = PrivateKeySigner::from(anvil.keys()[0].clone());
    let relayer_key = PrivateKeySigner::from(anvil.keys()[1].clone());
    let funder_key = PrivateKeySigner::from(anvil.keys()[2].clone());
    let maker_account = maker_key.address();
    let railgun = deploy(&url, maker_key.clone(), creation("MockRailgun"))
        .await
        .unwrap();
    let token = deploy(&url, maker_key.clone(), creation("TestToken"))
        .await
        .unwrap();
    let mut code = creation("ZecSwap");
    code.extend_from_slice(&U256::from(600).to_be_bytes::<32>());
    code.extend_from_slice(&U256::from(1_200).to_be_bytes::<32>());
    code.extend_from_slice(&railgun.into_word().0);
    let contract = deploy(&url, maker_key.clone(), code).await.unwrap();
    let maker = Settlement::connect(&url, contract, maker_key).unwrap();
    maker
        .mint_test_token(token, maker_account, 10_000_000)
        .await
        .unwrap();
    maker.add_inventory(token, 10_000_000).await.unwrap();
    let domain = Domain {
        chain_id: maker.chain_id().await.unwrap(),
        contract: contract.into(),
    };
    let chain = ProviderBuilder::new()
        .connect_http(url.parse().unwrap())
        .erased();
    let funder = ProviderBuilder::new()
        .wallet(EthereumWallet::from(funder_key))
        .connect_http(url.parse().unwrap())
        .erased();
    Some(Deployment {
        anvil,
        url,
        chain,
        funder,
        maker,
        maker_account,
        contract,
        token,
        domain,
        relayer_key,
        wallet: zecswap_railgun::Keys::from_seed(&SEED, 0),
    })
}

impl Deployment {
    /// The relayer's config, for its key on anvil's node.
    pub(crate) fn config(&self) -> Config {
        Config {
            network: zecswap_api::Network::Testnet,
            evm_rpc: self.url.clone(),
            contract: self.contract,
            token: self.token,
            maker: self.maker_account,
            listen: "127.0.0.1:0".parse().unwrap(),
            fee: FEE as u64,
            fee_gas: 0,
            providers: vec![],
            fee_margin_bps: 0,
            claim_margin: 30,
            reveals: None,
            top_up_transactions: 8,
            reverse_funding: None,
            railgun_sends: None,
            gas_alerts: None,
            deposits: None,
        }
    }

    /// A relayer process for the relayer's key, as `Relayer::new` makes it.
    pub(crate) async fn relayer(&self) -> Arc<Relayer> {
        self.relayer_with(self.config()).await
    }

    /// A relayer process for the relayer's key and `config`, as `Relayer::new` makes it.
    pub(crate) async fn relayer_with(&self, config: Config) -> Arc<Relayer> {
        let key = &self.relayer_key;
        let meter = RpcMeter::default();
        let relayer = Relayer {
            settlement: Settlement::connect_metered(
                &config.evm_rpc,
                self.contract,
                key.clone(),
                &meter,
            )
            .unwrap(),
            lanes: crate::lanes::Lanes::of(&config, key, &meter).await.unwrap(),
            config,
            account: key.address(),
            domain: self.domain,
            windows: zecswap_chain::evm::Windows {
                min: 600,
                late: 1_200,
            },
            turns: Default::default(),
            steps: Default::default(),
            monitor: crate::monitor::Monitor::new(MonitorToken::default()),
            sends: None,
            funding: None,

            deposits: None,
            pricing: None,
            history: None,
        };
        let lanes = &relayer.lanes;
        relayer
            .steps
            .note_earlier(&[
                &relayer.settlement,
                &lanes.claims,
                &lanes.locks,
                &lanes.funding,
            ])
            .await
            .unwrap();
        Arc::new(relayer)
    }

    pub(crate) async fn open(&self, index: u32) -> Swap {
        let keys = derive_user_keys(&SEED, NetworkType::Test, 0, index).unwrap();
        let now = self.maker.now().await.unwrap();
        let terms = zecswap_core::Terms {
            maker: self.maker_account.into(),
            token: self.token.into(),
            amount: 1_000_000,
            maker_share: derive_maker_share(&[9; 32], index.into()).unwrap().public(),
            user_share: keys.share.public(),
            user: keys.auth.address(),
            t0: now + 3_600,
            t1: now + 7_200,
            payout_note: self.wallet.note(&keys.note_entropy).unwrap().commitment(),
        };
        self.maker.open(&terms).await.unwrap();
        let id = swap_id(self.maker_account, &keys.share.public());
        self.maker.ready(id, &terms).await.unwrap();
        self.mine_blocks(crate::READY_DEPTH).await;
        Swap { id, terms, keys }
    }

    fn payout(&self, swap: &Swap) -> Payout {
        let relayer = self.relayer_key.address().into();
        Payout {
            swap_id: swap.id,
            terms: (&swap.terms).into(),
            note: (&self.wallet.note(&swap.keys.note_entropy).unwrap()).into(),
            fee: FEE,
            signature: swap
                .keys
                .auth
                .sign(&self.domain.payout(&swap.id, &relayer, FEE))
                .into(),
        }
    }

    pub(crate) fn claim(&self, swap: &Swap) -> Claim {
        Claim {
            swap_id: swap.id,
            terms: (&swap.terms).into(),
            secret: swap.keys.share.to_be_bytes().into(),
            payout: self.payout(swap),
        }
    }

    /// Mines a block every `seconds`; never, with none.
    pub(crate) async fn mine_every(&self, seconds: u64) {
        self.chain
            .raw_request::<_, serde_json::Value>("evm_setIntervalMining".into(), (seconds,))
            .await
            .unwrap();
    }

    pub(crate) async fn mine(&self) {
        self.chain
            .raw_request::<_, serde_json::Value>("evm_mine".into(), ())
            .await
            .unwrap();
    }

    /// Mines `blocks` blocks at once.
    pub(crate) async fn mine_blocks(&self, blocks: u64) {
        self.chain
            .raw_request::<_, serde_json::Value>("anvil_mine".into(), (U256::from(blocks),))
            .await
            .unwrap();
    }

    /// How many transactions `account` has sent, those in the pool included; and mined.
    pub(crate) async fn sent(&self, account: Address) -> (u64, u64) {
        let count = || self.chain.get_transaction_count(account);
        (
            count().pending().await.unwrap(),
            count().latest().await.unwrap(),
        )
    }

    /// Waits for `account` to have sent `count` transactions.
    pub(crate) async fn sent_reaches(&self, account: Address, count: u64) {
        for _ in 0..300 {
            if self.sent(account).await.0 >= count {
                return;
            }
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
        panic!("{account} never sent its transaction {count}");
    }
}

pub(crate) async fn post(
    app: axum::Router,
    path: &'static str,
    body: impl serde::Serialize,
) -> (StatusCode, serde_json::Value) {
    let request = Request::post(path)
        .header("Content-Type", "application/json")
        .body(Body::from(serde_json::to_vec(&body).unwrap()))
        .unwrap();
    let response = app.oneshot(request).await.unwrap();
    let status = response.status();
    let body = to_bytes(response.into_body(), 1024 * 1024).await.unwrap();
    (status, serde_json::from_slice(&body).unwrap())
}

#[tokio::test]
async fn a_claim_or_payout_posted_again_while_the_first_is_on_its_way_sends_nothing_more() {
    let Some(chain) = deployment().await else {
        return;
    };
    let relayer = chain.relayer().await;
    relayer.top_up().await.unwrap();
    let (claims, own) = (relayer.lane_accounts()["claims"], relayer.account);
    let app = api::router(Arc::clone(&relayer));
    let swap = chain.open(0).await;
    let ((claims_before, _), (own_before, _)) = (chain.sent(claims).await, chain.sent(own).await);

    chain.mine_every(0).await;
    // The client gives up once the reveal is out, as at its timeout, and posts the claim again.
    let first = tokio::spawn(post(app.clone(), "/v1/claim", chain.claim(&swap)));
    chain.sent_reaches(claims, claims_before + 1).await;
    first.abort();
    let again = tokio::spawn(post(app.clone(), "/v1/claim", chain.claim(&swap)));
    // The reveal lands and the payout goes out; another client's payout comes as well.
    chain.mine().await;
    chain.sent_reaches(own, own_before + 1).await;
    let paid_again = tokio::spawn(post(app.clone(), "/v1/payout", chain.payout(&swap)));
    tokio::time::sleep(Duration::from_millis(500)).await;
    assert!(
        !again.is_finished() && !paid_again.is_finished(),
        "a repeat ran while the first was on its way"
    );
    assert_eq!(chain.sent(claims).await.0, claims_before + 1);
    assert_eq!(chain.sent(own).await.0, own_before + 1);

    chain.mine_every(1).await;
    for request in [again, paid_again] {
        let (status, body) = request.await.unwrap();
        assert_eq!(status, StatusCode::OK, "{body}");
        assert_eq!(body["transactions"], serde_json::json!([]));
    }
    let landed = chain
        .maker
        .swap(swap.id, &swap.terms)
        .await
        .unwrap()
        .unwrap();
    assert!(landed.stage == Stage::Claimed && landed.paid_out);
    assert_eq!(
        chain.sent(claims).await,
        (claims_before + 1, claims_before + 1)
    );
    assert_eq!(chain.sent(own).await, (own_before + 1, own_before + 1));
    // The first claim is counted, though its client never read the answer.
    let operations = &serde_json::to_value(relayer.monitor_snapshot()).unwrap()["operations"];
    assert_eq!(operations["claim"]["sent"], 2);
    assert_eq!(operations["claim"]["failed"], 0);
    assert_eq!(operations["payout"]["failed"], 0);
}

#[tokio::test]
async fn a_relayer_restarted_while_a_payout_is_on_its_way_sends_no_other() {
    let Some(chain) = deployment().await else {
        return;
    };
    let before = chain.relayer().await;
    before.top_up().await.unwrap();
    let own = before.account;
    let swap = chain.open(0).await;
    chain
        .maker
        .claim(swap.id, &swap.terms, &swap.keys.share)
        .await
        .unwrap();
    let (own_before, _) = chain.sent(own).await;

    chain.mine_every(0).await;
    let first = tokio::spawn({
        let (before, payout) = (Arc::clone(&before), chain.payout(&swap));
        async move { before.payout(payout).await }
    });
    chain.sent_reaches(own, own_before + 1).await;
    // A deploy starts the relayer again while the payout waits in the pool.
    let after = chain.relayer().await;
    let app = api::router(Arc::clone(&after));
    let (status, body) = post(app.clone(), "/v1/payout", chain.payout(&swap)).await;
    assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE, "{body}");
    assert_eq!(body["code"], "unavailable");
    assert_eq!(chain.sent(own).await.0, own_before + 1);

    chain.mine_every(1).await;
    assert_eq!(first.await.unwrap().unwrap().transactions.len(), 1);
    let (status, body) = post(app, "/v1/payout", chain.payout(&swap)).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["transactions"], serde_json::json!([]));
    assert_eq!(chain.sent(own).await, (own_before + 1, own_before + 1));

    // A send that failed on a swap that shows its step done: one sent before did it.
    let failed = || Err::<Option<B256>, _>(RelayerError::from(anyhow::anyhow!("WrongStage()")));
    let terms = &swap.terms;
    let paid = after.unless_landed(swap.id, terms, failed(), |swap| swap.paid_out);
    assert_eq!(paid.await.unwrap(), None);
    let refunded = after.unless_landed(swap.id, terms, failed(), |swap| {
        swap.stage == Stage::Refunded
    });
    assert!(matches!(refunded.await, Err(RelayerError::Internal(_))));
}

async fn never() -> std::result::Result<B256, zecswap_chain::Error> {
    panic!("sent again")
}

#[tokio::test]
async fn a_step_sent_before_answers_until_it_is_known_not_to_have_landed() {
    let Some(chain) = deployment().await else {
        return;
    };
    let relayer = chain.relayer().await;
    let sender = &relayer.settlement;
    let unmined = |tx| async move { Err::<B256, _>(zecswap_chain::Error::Unconfirmed(tx)) };
    let unsettled = |result: Result<Option<B256>>| {
        assert!(
            matches!(result, Err(RelayerError::Unsettled(_))),
            "{result:?}"
        );
    };
    let swap = B256::repeat_byte(1);

    chain.mine_every(0).await;
    let transfer = TransactionRequest::default()
        .to(Address::repeat_byte(7))
        .value(U256::from(1));
    let pending = *chain
        .funder
        .send_transaction(transfer)
        .await
        .unwrap()
        .tx_hash();
    unsettled(
        relayer
            .send_once(swap, Step::Payout, sender, unmined(pending))
            .await,
    );
    // While it may still land, nothing is sent.
    unsettled(relayer.send_once(swap, Step::Payout, sender, never()).await);
    chain.mine().await;
    for _ in 0..2 {
        let landed = relayer.send_once(swap, Step::Payout, sender, never());
        assert_eq!(landed.await.unwrap(), None);
    }
    // Another of the swap's steps is its own.
    let claim = B256::repeat_byte(2);
    let claimed = relayer.send_once(swap, Step::Claim, sender, async { Ok(claim) });
    assert_eq!(claimed.await.unwrap(), Some(claim));

    // One that reverted is sent again.
    let reverting = TransactionRequest::default()
        .to(chain.contract)
        .input(vec![0xde, 0xad, 0xbe, 0xef].into())
        .gas_limit(100_000);
    let reverted = *chain
        .funder
        .send_transaction(reverting)
        .await
        .unwrap()
        .tx_hash();
    chain.mine().await;
    let other = B256::repeat_byte(3);
    unsettled(
        relayer
            .send_once(other, Step::Payout, sender, unmined(reverted))
            .await,
    );
    let resent = B256::repeat_byte(4);
    let again = relayer.send_once(other, Step::Payout, sender, async { Ok(resent) });
    assert_eq!(again.await.unwrap(), Some(resent));

    // One its node doesn't know may still be on its way there.
    let third = B256::repeat_byte(5);
    let unknown = B256::repeat_byte(6);
    unsettled(
        relayer
            .send_once(third, Step::Payout, sender, unmined(unknown))
            .await,
    );
    unsettled(
        relayer
            .send_once(third, Step::Payout, sender, never())
            .await,
    );
    chain.mine_every(1).await;
}

#[tokio::test]
async fn a_funding_posted_again_while_the_first_may_land_gets_its_hash_and_sends_nothing() {
    let Some(chain) = deployment().await else {
        return;
    };
    let relayer = chain.relayer().await;
    let lane = relayer.lanes.funding.account().unwrap();
    chain
        .chain
        .raw_request::<_, ()>(
            "anvil_setBalance".into(),
            (lane, U256::from(10u128.pow(18))),
        )
        .await
        .unwrap();
    // A funding's stand-in: a transfer from the funding account, its hash back before it mines.
    let key = crate::lanes::derive(&chain.relayer_key, "funding").unwrap();
    let wallet = ProviderBuilder::new()
        .wallet(EthereumWallet::from(key))
        .connect_http(chain.url.parse().unwrap())
        .erased();
    let sends = AtomicUsize::new(0);
    let fund = || async {
        sends.fetch_add(1, Ordering::SeqCst);
        let request = TransactionRequest::default()
            .with_to(Address::repeat_byte(9))
            .with_value(U256::from(1));
        let pending = wallet.send_transaction(request).await.unwrap();
        Ok::<_, RelayerError>(Some(*pending.tx_hash()))
    };
    let id = B256::repeat_byte(5);

    chain.mine_every(0).await;
    let first = relayer
        .fund_once(id, &relayer.lanes.funding, fund())
        .await
        .unwrap();
    assert!(first.is_some());
    let again = relayer
        .fund_once(id, &relayer.lanes.funding, fund())
        .await
        .unwrap();
    assert_eq!(again, first);
    // A restarted relayer, which knows nothing of it, waits for it to mine.
    let restarted = chain.relayer().await;
    let waits = restarted
        .fund_once(id, &restarted.lanes.funding, fund())
        .await;
    assert!(matches!(waits, Err(RelayerError::Unsettled(_))));
    assert_eq!(sends.load(Ordering::SeqCst), 1);
    assert_eq!(chain.sent(lane).await, (1, 0));

    // Once it landed, the escrow on chain answers.
    chain.mine().await;
    let escrow = async { Ok(None) };
    assert_eq!(
        relayer
            .fund_once(id, &relayer.lanes.funding, escrow)
            .await
            .unwrap(),
        None
    );
    let escrow = async { Ok(None) };
    assert_eq!(
        restarted
            .fund_once(id, &restarted.lanes.funding, escrow)
            .await
            .unwrap(),
        None
    );
    assert_eq!(chain.sent(lane).await, (1, 1));
}
