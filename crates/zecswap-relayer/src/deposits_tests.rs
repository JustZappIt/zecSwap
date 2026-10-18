//! Deposits on anvil, through the Deposits contract and the EIP-3009 test token, with Railgun's
//! stand-in: a deposit goes once however often it is posted; what the relayer can't stand behind
//! is refused before it reads the chain; another of the same address waits for the first; one that
//! reverted holds its address back for the cooldown; a full queue waits; a rescue shields what came
//! back; each mined deposit is costed, with one message; and the fees drain with the lanes.

use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

use alloy::network::TransactionBuilder;
use alloy::node_bindings::{Anvil, AnvilInstance};
use alloy::providers::{DynProvider, Provider, ProviderBuilder};
use alloy::rpc::types::TransactionRequest;
use alloy::sol_types::SolCall;
use axum::body::{Body, to_bytes};
use axum::http::{HeaderMap, Request, StatusCode, header};
use tower::ServiceExt;
use zecswap_alerts::Telegram;
use zecswap_api::relayer::{RescueDeposit, ShieldDeposit, deposit_fee};
use zecswap_chain::evm::railgun::Status;
use zecswap_chain::evm::{RpcMeter, U256, deploy};
use zecswap_core::{
    DepositKeys, DepositsDomain, NetworkType, ReceiveAuthorization, RescueAuthorization,
    derive_deposit_keys,
};

use super::*;
use crate::deposits::Deposits;
use crate::lanes::Lanes;
use crate::sends::now;

const VALUE: u128 = 25_000_000;
const FLOOR: u64 = 100_000;
const FEE_BPS: u32 = 10;
const MIN_VALUE: u64 = 2_000_000;

alloy::sol! {
    function transfer(address to, uint256 amount) returns (bool);
}

struct Chain {
    _anvil: AnvilInstance,
    url: String,
    chain: DynProvider,
    /// Anvil's first account, which deploys and mints the test token.
    funder: Settlement,
    contract: Address,
    deposits: Address,
    token: Address,
    railgun: Address,
    relayer_key: PrivateKeySigner,
    chain_id: u64,
}

/// A wallet's deposit address 0 and its Railgun wallet, from the seed of `seed`s.
struct Depositor {
    keys: DepositKeys,
    railgun: zecswap_railgun::Keys,
}

impl Depositor {
    fn new(seed: u8) -> Self {
        let seed = [seed; 64];
        Self {
            keys: derive_deposit_keys(&seed, NetworkType::Test, 0, 0).unwrap(),
            railgun: zecswap_railgun::Keys::from_seed(&seed, 0),
        }
    }

    fn address(&self) -> Address {
        self.keys.key.address().into()
    }
}

async fn chain() -> Option<Chain> {
    let Ok(anvil) = Anvil::new().block_time(1).try_spawn() else {
        eprintln!("skipped: anvil is not installed");
        return None;
    };
    let artifacts = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../contracts/out");
    if !artifacts.join("Deposits.sol").exists() {
        eprintln!("skipped: run `forge build` in contracts/");
        return None;
    }
    let url = anvil.endpoint();
    let funder_key = PrivateKeySigner::from(anvil.keys()[0].clone());
    let relayer_key = PrivateKeySigner::from(anvil.keys()[1].clone());
    let deployed = |code: Vec<u8>| deploy(&url, funder_key.clone(), code);
    let railgun = deployed(creation(&artifacts, "MockRailgun")).await.unwrap();
    let token = deployed(creation(&artifacts, "TestToken")).await.unwrap();
    let mut code = creation(&artifacts, "ZecSwap");
    code.extend_from_slice(&U256::from(600).to_be_bytes::<32>());
    code.extend_from_slice(&railgun.into_word().0);
    let contract = deployed(code).await.unwrap();
    let deposits = deployed(deposits_code(&artifacts, token, railgun))
        .await
        .unwrap();
    let funder = Settlement::connect(&url, contract, funder_key).unwrap();
    let chain_id = funder.chain_id().await.unwrap();
    let chain = ProviderBuilder::new()
        .connect_http(url.parse().unwrap())
        .erased();
    Some(Chain {
        _anvil: anvil,
        url,
        chain,
        funder,
        contract,
        deposits,
        token,
        railgun,
        relayer_key,
        chain_id,
    })
}

fn creation(artifacts: &Path, name: &str) -> Vec<u8> {
    let artifact: serde_json::Value = serde_json::from_slice(
        &std::fs::read(artifacts.join(format!("{name}.sol/{name}.json"))).unwrap(),
    )
    .unwrap();
    alloy_primitives::hex::decode(artifact["bytecode"]["object"].as_str().unwrap()).unwrap()
}

/// A Deposits contract of `token`, into `railgun`, screening nobody.
fn deposits_code(artifacts: &Path, token: Address, railgun: Address) -> Vec<u8> {
    let mut code = creation(artifacts, "Deposits");
    code.extend_from_slice(&token.into_word().0);
    code.extend_from_slice(&railgun.into_word().0);
    code.extend_from_slice(&[0; 32]);
    code
}

impl Chain {
    fn config(&self, dir: &Path) -> Config {
        Config {
            network: zecswap_api::Network::Testnet,
            evm_rpc: self.url.clone(),
            contract: self.contract,
            token: self.token,
            maker: Address::repeat_byte(4),
            listen: "127.0.0.1:0".parse().unwrap(),
            fee: 1,
            fee_gas: 0,
            providers: vec![],
            fee_margin_bps: 0,
            claim_margin: 30,
            top_up_transactions: 8,
            reverse_funding: None,
            railgun_sends: None,
            gas_alerts: None,
            deposits: Some(DepositsConfig {
                contract: self.deposits,
                fee: FLOOR,
                fee_gas: 0,
                fee_bps: FEE_BPS,
                max_gas_limit: 2_000_000,
                max_gas_price_wei: 50_000_000_000,
                min_value: MIN_VALUE,
                max_validity: 86_400,
                max_pending: 8,
                journal: dir.join("deposits.sqlite"),
            }),
        }
    }

    /// The relayer as `Relayer::new` makes it, with its messages queued and never delivered, and
    /// its lanes topped up.
    async fn relayer(&self, config: Config) -> Arc<Relayer> {
        let key = &self.relayer_key;
        let meter = RpcMeter::default();
        let settlement =
            Settlement::connect_metered(&self.url, self.contract, key.clone(), &meter).unwrap();
        let lanes = Lanes::connect_metered(&self.url, self.contract, key, &meter).unwrap();
        let deposits = match &config.deposits {
            Some(deposits) => Some(
                Deposits::open(
                    deposits,
                    &config,
                    &settlement,
                    lanes.deposits.account().unwrap(),
                    self.railgun,
                    Telegram::new(Some("1:test"), Some("1")).unwrap(),
                )
                .await
                .unwrap(),
            ),
            None => None,
        };
        let relayer = Arc::new(Relayer {
            config,
            account: key.address(),
            domain: Domain {
                chain_id: self.chain_id,
                contract: self.contract.into(),
            },
            settlement,
            lanes,
            lock_duration: 600,
            turns: Default::default(),
            steps: Default::default(),
            monitor: crate::monitor::Monitor::new(MonitorToken::default()),
            sends: None,
            deposits,
            funding: None,
            pricing: None,
            history: None,
        });
        relayer.top_up().await.unwrap();
        relayer
    }

    /// `who`'s shield of `value` for `fee`, valid until `valid_before`, as a wallet signs it for
    /// the relayer its terms name.
    async fn shield(
        &self,
        relayer: &Relayer,
        who: &Depositor,
        value: u128,
        fee: u128,
        valid_before: u64,
    ) -> ShieldDeposit {
        let terms = relayer.terms().await.deposits.unwrap();
        let token = relayer.settlement.deposits(self.deposits).await.unwrap();
        let note = who
            .railgun
            .note(&who.keys.shield_entropy(valid_before))
            .unwrap();
        let nonce = self
            .domain()
            .deposit_nonce(&note.words(), fee, &terms.relayer.into());
        let digest = token
            .token_domain
            .receive_with_authorization(&ReceiveAuthorization {
                from: who.keys.key.address(),
                to: self.deposits.into(),
                value,
                valid_after: 0,
                valid_before,
                nonce,
            });
        ShieldDeposit {
            chain_id: self.chain_id,
            contract: self.deposits,
            from: who.address(),
            value,
            valid_after: 0,
            valid_before,
            note: (&note).into(),
            fee,
            signature: who.keys.key.sign(&digest).into(),
        }
    }

    /// `who`'s rescue of what came back to its vault, its `nonce`th, for `fee`.
    async fn rescue(
        &self,
        relayer: &Relayer,
        who: &Depositor,
        nonce: u64,
        fee: u128,
    ) -> RescueDeposit {
        let terms = relayer.terms().await.deposits.unwrap();
        let note = who.railgun.note(&who.keys.rescue_entropy(nonce)).unwrap();
        let authorization = RescueAuthorization {
            nonce,
            deadline: now() + 3_600,
        };
        let digest = self.domain().rescue(
            &who.keys.key.address(),
            &note.commitment(),
            &terms.relayer.into(),
            fee,
            authorization,
        );
        RescueDeposit {
            chain_id: self.chain_id,
            contract: self.deposits,
            from: who.address(),
            note: (&note).into(),
            fee,
            nonce,
            deadline: authorization.deadline,
            signature: who.keys.key.sign(&digest).into(),
        }
    }

    fn domain(&self) -> DepositsDomain {
        DepositsDomain {
            chain_id: self.chain_id,
            contract: self.deposits.into(),
        }
    }

    async fn mint(&self, to: Address, amount: u128) {
        self.funder
            .mint_test_token(self.token, to, amount)
            .await
            .unwrap();
    }

    async fn balance(&self, owner: Address) -> u128 {
        self.funder.token_balance(self.token, owner).await.unwrap()
    }

    /// Mines a block every `seconds`; never, with none.
    async fn mine_every(&self, seconds: u64) {
        self.chain
            .raw_request::<_, serde_json::Value>("evm_setIntervalMining".into(), (seconds,))
            .await
            .unwrap();
    }

    async fn mine(&self) {
        self.chain
            .raw_request::<_, serde_json::Value>("evm_mine".into(), ())
            .await
            .unwrap();
    }

    async fn status(&self, tx: B256) -> Status {
        self.funder.transaction_status(tx).await.unwrap()
    }

    /// Waits for `tx` to be mined, and whether it succeeded.
    async fn mined(&self, tx: B256) -> bool {
        for _ in 0..100 {
            if let Status::Mined { succeeded } = self.status(tx).await {
                return succeeded;
            }
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
        panic!("{tx} never mined");
    }

    /// How many transactions `account` has sent, those in the pool included.
    async fn sent(&self, account: Address) -> u64 {
        self.chain
            .get_transaction_count(account)
            .pending()
            .await
            .unwrap()
    }
}

async fn post(
    relayer: &Arc<Relayer>,
    path: &'static str,
    body: impl serde::Serialize,
) -> (StatusCode, HeaderMap, serde_json::Value) {
    let request = Request::post(path)
        .header("Content-Type", "application/json")
        .body(Body::from(serde_json::to_vec(&body).unwrap()))
        .unwrap();
    let response = api::router(Arc::clone(relayer))
        .oneshot(request)
        .await
        .unwrap();
    let (status, headers) = (response.status(), response.headers().clone());
    let body = to_bytes(response.into_body(), 1024 * 1024).await.unwrap();
    (status, headers, serde_json::from_slice(&body).unwrap())
}

fn transaction(body: &serde_json::Value) -> B256 {
    body["transactions"][0].as_str().unwrap().parse().unwrap()
}

fn retry_after(headers: &HeaderMap) -> u64 {
    headers[header::RETRY_AFTER]
        .to_str()
        .unwrap()
        .parse()
        .unwrap()
}

fn fee() -> u128 {
    deposit_fee(FLOOR.into(), FEE_BPS, VALUE)
}

#[tokio::test]
async fn a_deposit_is_sent_once_however_often_it_is_posted() {
    let Some(chain) = chain().await else {
        return;
    };
    let dir = tempfile::tempdir().unwrap();
    let relayer = chain.relayer(chain.config(dir.path())).await;
    let terms = relayer.terms().await.deposits.unwrap();
    let lane = relayer.lane_accounts()["deposits"];
    assert_eq!(terms.relayer, lane);
    assert_eq!((terms.contract, terms.token), (chain.deposits, chain.token));
    assert_eq!((terms.fee, terms.fee_bps), (FLOOR.into(), FEE_BPS));
    assert_eq!(terms.min_value, MIN_VALUE.into());
    assert_eq!(terms.fee_expires_at, None);

    let who = Depositor::new(1);
    chain.mint(who.address(), VALUE).await;
    let request = chain
        .shield(&relayer, &who, VALUE, fee(), now() + 3_600)
        .await;
    let sent_before = chain.sent(lane).await;
    chain.mine_every(0).await;
    let (status, _, first) = post(&relayer, "/v1/deposits/shield", &request).await;
    assert_eq!(status, StatusCode::OK, "{first}");
    let tx = transaction(&first);
    let (status, _, again) = post(&relayer, "/v1/deposits/shield", &request).await;
    assert_eq!((status, &again), (StatusCode::OK, &first));
    assert_eq!(chain.sent(lane).await, sent_before + 1);

    chain.mine().await;
    assert!(chain.mined(tx).await);
    chain.mine_every(1).await;
    let (status, _, after) = post(&relayer, "/v1/deposits/shield", &request).await;
    assert_eq!((status, &after), (StatusCode::OK, &first));
    assert_eq!(chain.sent(lane).await, sent_before + 1);

    let shielded = chain.funder.shielded(tx).await.unwrap();
    let value = VALUE - fee();
    assert_eq!(shielded.len(), 1);
    assert_eq!(shielded[0].value, value - value * 25 / 10_000);
    assert_eq!(
        shielded[0].note,
        who.railgun
            .note(&who.keys.shield_entropy(request.valid_before))
            .unwrap()
    );
    assert_eq!(chain.balance(lane).await, fee());
    assert_eq!(chain.balance(who.address()).await, 0);
    let operations = &serde_json::to_value(relayer.monitor_snapshot()).unwrap()["operations"];
    assert_eq!(operations["shield_deposit"]["sent"], 1);
}

#[tokio::test]
async fn what_the_relayer_cant_stand_behind_is_refused_before_it_reads_the_chain() {
    let Some(chain) = chain().await else {
        return;
    };
    let dir = tempfile::tempdir().unwrap();
    let relayer = chain.relayer(chain.config(dir.path())).await;
    let (who, other) = (Depositor::new(1), Depositor::new(2));
    let now = now();
    let good = chain
        .shield(&relayer, &who, VALUE, fee(), now + 3_600)
        .await;
    let signed = |value, fee, valid_before| chain.shield(&relayer, &who, value, fee, valid_before);
    let not_valid_yet = ShieldDeposit {
        valid_after: now + 600,
        ..good.clone()
    };
    let by_another = ShieldDeposit {
        from: who.address(),
        ..chain
            .shield(&relayer, &other, VALUE, fee(), now + 3_600)
            .await
    };
    let below_minimum = u128::from(MIN_VALUE) - 1;
    let cases = [
        (
            ShieldDeposit {
                chain_id: 1,
                ..good.clone()
            },
            "for another chain",
        ),
        (
            ShieldDeposit {
                contract: chain.contract,
                ..good.clone()
            },
            "to another contract",
        ),
        (
            signed(below_minimum, 1, now + 3_600).await,
            "under the minimum",
        ),
        (
            signed(1 << 120, fee(), now + 3_600).await,
            "more than a note holds",
        ),
        (signed(VALUE, VALUE, now + 3_600).await, "all fee"),
        (not_valid_yet, "not valid yet"),
        (signed(VALUE, fee(), now + 600).await, "expiring too soon"),
        (
            signed(VALUE, fee(), now + 2 * 86_400).await,
            "valid too long",
        ),
        (
            ShieldDeposit {
                fee: fee() + 1,
                ..good.clone()
            },
            "its fee altered",
        ),
        (by_another, "signed by another address"),
        (
            signed(VALUE, fee() - 1, now + 3_600).await,
            "a unit under the fee",
        ),
    ];
    let stale = signed(VALUE, fee() - 1, now + 3_600).await;
    let before = relayer.rpc_meter().usage().total();
    for (request, what) in cases {
        let (status, _, body) = post(&relayer, "/v1/deposits/shield", &request).await;
        assert_eq!(status, StatusCode::BAD_REQUEST, "{what}: {body}");
        assert_eq!(body["code"], "rejected", "{what}");
    }
    let (_, _, stale) = post(&relayer, "/v1/deposits/shield", &stale).await;
    assert!(
        stale["error"]
            .as_str()
            .unwrap()
            .contains("fetch the terms again")
    );
    assert_eq!(
        relayer.rpc_meter().usage().total(),
        before,
        "a refusal read the chain"
    );
    let operations = &serde_json::to_value(relayer.monitor_snapshot()).unwrap()["operations"];
    assert_eq!(operations["shield_deposit"]["refused"], 12);

    // One its address can't pay for is refused once the chain is read, unsent.
    let lane = relayer.lane_accounts()["deposits"];
    let sent = chain.sent(lane).await;
    let (status, _, body) = post(&relayer, "/v1/deposits/shield", &good).await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "{body}");
    assert!(body["error"].as_str().unwrap().contains("holds less"));
    assert_eq!(chain.sent(lane).await, sent);

    let config = chain.config(dir.path());
    let disabled = chain
        .relayer(Config {
            deposits: None,
            ..config
        })
        .await;
    assert!(disabled.terms().await.deposits.is_none());
    let (status, _, body) = post(&disabled, "/v1/deposits/shield", &good).await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "{body}");
}

#[tokio::test]
async fn another_deposit_of_an_address_waits_for_the_first() {
    let Some(chain) = chain().await else {
        return;
    };
    let dir = tempfile::tempdir().unwrap();
    let relayer = chain.relayer(chain.config(dir.path())).await;
    let who = Depositor::new(1);
    chain.mint(who.address(), VALUE).await;
    let valid_before = now() + 3_600;
    let first = chain
        .shield(&relayer, &who, VALUE, fee(), valid_before)
        .await;
    let second = chain
        .shield(&relayer, &who, VALUE, fee(), valid_before + 1)
        .await;
    assert_ne!(first.note, second.note);

    chain.mine_every(0).await;
    let (status, _, body) = post(&relayer, "/v1/deposits/shield", &first).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    let tx = transaction(&body);
    let (status, _, body) = post(&relayer, "/v1/deposits/shield", &second).await;
    assert_eq!(status, StatusCode::CONFLICT, "{body}");
    assert_eq!(body["code"], "conflict");
    assert_eq!(body["transactions"], serde_json::json!([tx]));

    chain.mine().await;
    assert!(chain.mined(tx).await);
    chain.mine_every(1).await;
    let (status, _, body) = post(&relayer, "/v1/deposits/shield", &second).await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "{body}");
}

#[tokio::test]
async fn a_deposit_that_reverted_holds_its_address_back_for_the_cooldown() {
    let Some(chain) = chain().await else {
        return;
    };
    let dir = tempfile::tempdir().unwrap();
    let relayer = chain.relayer(chain.config(dir.path())).await;
    let who = Depositor::new(1);
    chain.mint(who.address(), VALUE).await;
    let request = chain
        .shield(&relayer, &who, VALUE, fee(), now() + 3_600)
        .await;

    chain.mine_every(0).await;
    let (status, _, body) = post(&relayer, "/v1/deposits/shield", &request).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    let tx = transaction(&body);
    // The depositor moves its tokens once the relayer has checked them, tipping more.
    for (method, params) in [
        (
            "anvil_impersonateAccount",
            serde_json::json!([who.address()]),
        ),
        (
            "anvil_setBalance",
            serde_json::json!([who.address(), "0xde0b6b3a7640000"]),
        ),
    ] {
        chain
            .chain
            .raw_request::<_, serde_json::Value>(method.into(), params)
            .await
            .unwrap();
    }
    let away = TransactionRequest::default()
        .with_from(who.address())
        .with_to(chain.token)
        .with_input(
            transferCall {
                to: Address::repeat_byte(9),
                amount: U256::from(VALUE),
            }
            .abi_encode(),
        )
        .with_gas_limit(100_000)
        .with_max_fee_per_gas(2_000_000_000_000)
        .with_max_priority_fee_per_gas(1_000_000_000_000);
    let moved = *chain.chain.send_transaction(away).await.unwrap().tx_hash();
    chain.mine().await;
    assert!(chain.mined(moved).await);
    assert!(!chain.mined(tx).await, "the deposit landed");
    chain.mine_every(1).await;

    let (status, headers, body) = post(&relayer, "/v1/deposits/shield", &request).await;
    assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE, "{body}");
    assert!((3_500..=3_600).contains(&retry_after(&headers)));
    chain.mint(who.address(), VALUE).await;
    let again = chain
        .shield(&relayer, &who, VALUE, fee(), now() + 3_601)
        .await;
    let (status, headers, body) = post(&relayer, "/v1/deposits/shield", &again).await;
    assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE, "{body}");
    assert!(retry_after(&headers) > 3_500);
}

#[tokio::test]
async fn a_full_queue_waits() {
    let Some(chain) = chain().await else {
        return;
    };
    let dir = tempfile::tempdir().unwrap();
    let mut config = chain.config(dir.path());
    config.deposits.as_mut().unwrap().max_pending = 1;
    let relayer = chain.relayer(config).await;
    let (first, second) = (Depositor::new(1), Depositor::new(2));
    for who in [&first, &second] {
        chain.mint(who.address(), VALUE).await;
    }
    let valid_before = now() + 3_600;
    let requests = [
        chain
            .shield(&relayer, &first, VALUE, fee(), valid_before)
            .await,
        chain
            .shield(&relayer, &second, VALUE, fee(), valid_before)
            .await,
    ];
    chain.mine_every(0).await;
    let (status, _, body) = post(&relayer, "/v1/deposits/shield", &requests[0]).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    let (status, headers, body) = post(&relayer, "/v1/deposits/shield", &requests[1]).await;
    assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE, "{body}");
    assert_eq!(retry_after(&headers), 30);
    chain.mine().await;
    chain.mine_every(1).await;
    let (status, _, body) = post(&relayer, "/v1/deposits/shield", &requests[1]).await;
    assert_eq!(status, StatusCode::OK, "{body}");
}

#[tokio::test]
async fn a_rescue_shields_what_came_back_and_each_deposit_is_costed_once() {
    let Some(chain) = chain().await else {
        return;
    };
    let dir = tempfile::tempdir().unwrap();
    let relayer = chain.relayer(chain.config(dir.path())).await;
    let who = Depositor::new(1);
    chain.mint(who.address(), VALUE).await;
    let request = chain
        .shield(&relayer, &who, VALUE, fee(), now() + 3_600)
        .await;
    let (status, _, body) = post(&relayer, "/v1/deposits/shield", &request).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    let shielded = transaction(&body);
    assert!(chain.mined(shielded).await);

    // Nothing came back yet.
    let rescue = chain.rescue(&relayer, &who, 0, FLOOR.into()).await;
    let (status, _, body) = post(&relayer, "/v1/deposits/rescue", &rescue).await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "{body}");
    let vault = chain
        .funder
        .deposit_vault(chain.deposits, who.address())
        .await
        .unwrap();
    let returned = 5_000_000;
    chain.mint(vault, returned).await;
    let (status, _, body) = post(&relayer, "/v1/deposits/rescue", &rescue).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    let rescued = transaction(&body);
    assert!(chain.mined(rescued).await);
    let (status, _, again) = post(&relayer, "/v1/deposits/rescue", &rescue).await;
    assert_eq!((status, &again), (StatusCode::OK, &body));
    let note = who.railgun.note(&who.keys.rescue_entropy(0)).unwrap();
    assert_eq!(chain.funder.shielded(rescued).await.unwrap()[0].note, note);
    assert_eq!(chain.balance(vault).await, 0);
    assert_eq!(
        chain
            .funder
            .deposit_rescue_nonce(chain.deposits, who.address())
            .await
            .unwrap(),
        1
    );

    for _ in 0..2 {
        relayer.cost_pass().await.unwrap();
    }
    let ledger = serde_json::to_value(relayer.deposits_snapshot(Some(0)).unwrap()).unwrap();
    let record = |tx: B256| {
        ledger["deposits"]
            .as_array()
            .unwrap()
            .iter()
            .find(|record| record["transactionHash"] == serde_json::json!(tx))
            .unwrap()
            .clone()
    };
    let (shield, rescue) = (record(shielded), record(rescued));
    assert_eq!(shield["kind"], "shield");
    assert_eq!(shield["value"], VALUE.to_string());
    assert_eq!(shield["fee"], fee().to_string());
    assert_eq!(rescue["kind"], "rescue");
    assert!(rescue["value"].is_null());
    assert_eq!(rescue["fee"], FLOOR.to_string());
    for record in [&shield, &rescue] {
        assert_eq!(record["succeeded"], true);
        assert!(record["gasUsed"].as_u64().unwrap() > 21_000);
    }
    let deposits = relayer.deposits.as_ref().unwrap();
    let texts: Vec<String> = deposits
        .journal()
        .connection()
        .prepare("SELECT text FROM notifications ORDER BY text")
        .unwrap()
        .query_map([], |row| row.get(0))
        .unwrap()
        .collect::<rusqlite::Result<_>>()
        .unwrap();
    assert_eq!(texts.len(), 2, "{texts:?}");
    assert!(
        texts
            .iter()
            .any(|text| text.contains("25 USDC made private"))
    );
    assert!(texts.iter().any(|text| text.contains("deposit rescue")));
    for text in &texts {
        assert!(!text.contains(&who.address().to_string()), "{text}");
    }

    // The fees collected in the deposits account leave with the lanes.
    let to = Address::repeat_byte(0x99);
    let lane = relayer.lane_accounts()["deposits"];
    let collected = chain.balance(lane).await;
    assert_eq!(collected, fee() + u128::from(FLOOR));
    drain_lanes(&relayer.config, &chain.relayer_key, to)
        .await
        .unwrap();
    assert_eq!(chain.balance(to).await, collected);
    assert_eq!(chain.balance(lane).await, 0);
}

#[tokio::test]
async fn a_relayer_refuses_to_start_on_a_deposits_contract_of_another_token_or_railgun() {
    let Some(chain) = chain().await else {
        return;
    };
    let artifacts = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../contracts/out");
    let funder = PrivateKeySigner::from(chain._anvil.keys()[0].clone());
    let other = deploy(
        &chain.url,
        funder.clone(),
        creation(&artifacts, "TestToken"),
    )
    .await
    .unwrap();
    let dir = tempfile::tempdir().unwrap();
    for code in [
        deposits_code(&artifacts, other, chain.railgun),
        deposits_code(&artifacts, chain.token, Address::repeat_byte(0x42)),
    ] {
        let contract = deploy(&chain.url, funder.clone(), code).await.unwrap();
        let mut config = chain.config(dir.path());
        config.deposits.as_mut().unwrap().contract = contract;
        let started = Relayer::new(config, chain.relayer_key.clone(), None).await;
        assert!(started.is_err(), "it started on {contract}");
    }
    let started = Relayer::new(chain.config(dir.path()), chain.relayer_key.clone(), None).await;
    assert!(started.is_ok(), "{:?}", started.err());
}

#[tokio::test]
async fn a_refused_deposit_is_logged_without_its_signature_or_note() {
    let Some(chain) = chain().await else {
        return;
    };
    let captured = crate::logging_tests::Capture::default();
    let writer = captured.clone();
    let subscriber = tracing_subscriber::fmt()
        .without_time()
        .with_ansi(false)
        .with_writer(move || writer.clone())
        .finish();
    let _guard = tracing::subscriber::set_default(subscriber);
    let dir = tempfile::tempdir().unwrap();
    let relayer = chain.relayer(chain.config(dir.path())).await;
    let who = Depositor::new(1);
    let request = chain
        .shield(&relayer, &who, VALUE, fee(), now() + 3_600)
        .await;
    let altered = ShieldDeposit {
        fee: fee() + 1,
        ..request.clone()
    };
    for request in [&altered, &request] {
        let (status, _, body) = post(&relayer, "/v1/deposits/shield", request).await;
        assert_eq!(status, StatusCode::BAD_REQUEST, "{body}");
    }
    let output = String::from_utf8(captured.0.lock().unwrap().clone()).unwrap();
    assert!(output.contains("shield_deposit") && output.contains("not signed by"));
    assert!(output.contains(&format!("from={}", who.address())));
    let note = &request.note;
    for private in [
        request.signature.to_string(),
        note.npk.to_string(),
        note.shield_key.to_string(),
        note.encrypted_bundle[0].to_string(),
    ] {
        assert!(!output.contains(&private[2..]), "request data in the logs");
    }
}

#[test]
fn a_mined_deposit_says_what_it_moved_and_earned_and_names_no_one() {
    use zecswap_chain::evm::deposits::Call;
    let from = Address::repeat_byte(0x5a);
    let hash = B256::repeat_byte(7);
    let alert = |call, succeeded| {
        crate::deposits::deposit_alert(
            zecswap_api::Network::Testnet,
            11_155_111,
            hash,
            call,
            succeeded,
            1_791_720_221,
        )
        .text
    };
    let shield = Call::Shield {
        from,
        value: 25_000_000,
        fee: 125_000,
    };
    assert_eq!(
        alert(Some(shield), true),
        format!(
            "ZecSwap testnet · deposit\n25 USDC made private · fee 0.125 USDC\n11 Oct 12:03 UTC\nhttps://sepolia.etherscan.io/tx/{hash}"
        )
    );
    assert_eq!(
        alert(Some(Call::Rescue { from, fee: 100_000 }), true),
        format!(
            "ZecSwap testnet · deposit rescue\nfee 0.1 USDC\n11 Oct 12:03 UTC\nhttps://sepolia.etherscan.io/tx/{hash}"
        )
    );
    let reverted = alert(Some(shield), false);
    assert!(reverted.starts_with("ZecSwap testnet · deposit reverted\nNothing moved"));
    for text in [reverted, alert(None, true)] {
        assert!(!text.contains(&from.to_string()) && !text.contains("25 USDC"));
    }
}

/// The `[deposits]` tables staged in deploy/ load as the relayer's config, their placeholders
/// filled in.
#[test]
fn the_staged_deposits_configs_load() {
    let deploy = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../deploy");
    let filled = |text: &str| {
        let mut text = text.to_owned();
        while let Some(start) = text.find("\"<") {
            let end = start + text[start..].find(">\"").unwrap() + 2;
            text.replace_range(start..end, "\"0x1111111111111111111111111111111111111111\"");
        }
        text
    };
    let testnet = std::fs::read_to_string(deploy.join("testnet/relayer-deposits.toml")).unwrap();
    let base = r#"
        network = "testnet"
        evm_rpc = "http://127.0.0.1:8545"
        contract = "0x2222222222222222222222222222222222222222"
        token = "0x3333333333333333333333333333333333333333"
        maker = "0x4444444444444444444444444444444444444444"
        listen = "127.0.0.1:8788"
        fee = 100000
        claim_margin = 120
    "#;
    let config: Config = toml::from_str(&filled(&format!("{base}\n{testnet}"))).unwrap();
    let deposits = config.deposits.unwrap();
    assert_eq!(
        (deposits.min_value, deposits.fee_bps, deposits.max_pending),
        (2_000_000, 10, 8)
    );
    assert!(deposits.max_gas_limit >= deposits.fee_gas * 6 / 5);
}
