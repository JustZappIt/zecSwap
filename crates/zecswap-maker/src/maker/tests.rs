use axum::body::Body;
use axum::http::{Request, StatusCode};
use tempfile::TempDir;
use tonic::transport::Endpoint;
use tower::ServiceExt;
use zecswap_chain::evm::PrivateKeySigner;
use zecswap_core::{ShareProof, ViewingKeys};

use super::*;

fn maker() -> (TempDir, Arc<Maker>) {
    let dir = tempfile::tempdir().unwrap();
    let mut config: Config = toml::from_str(include_str!("../../maker.example.toml")).unwrap();
    config.data_dir = dir.path().join("wallet");
    config.evm_rpc = "http://127.0.0.1:1".into();
    std::fs::create_dir(&config.data_dir).unwrap();
    let joint = JointAccount::derive(
        &SecretShare::random(OsRng).public(),
        &SecretShare::random(OsRng).public(),
        &ViewingKeys::random(OsRng),
    )
    .unwrap();
    let key = PrivateKeySigner::random();
    let maker = Maker {
        telegram: crate::telegram::Telegram::new(None, None).unwrap(),
        prices: crate::market::PriceBook::from_env(&config.pricing).unwrap(),
        monitoring: monitoring::Monitoring::new(None),
        inventory: None,
        account: key.address(),
        lock_duration: 7200,
        root: Zeroizing::new([7; 32]),
        chain_id: 1,
        sweep_to: joint
            .unified_address(zecswap_core::NetworkType::Test)
            .parse()
            .unwrap(),
        store: Store::open(&dir.path().join("maker.sqlite")).unwrap(),
        settlement: Settlement::connect(&config.evm_rpc, config.contract, key).unwrap(),
        zcash: Mutex::new(Zcash {
            wallet: Some(Zcash::open_wallet(&config).unwrap()),
            client: Lightwalletd::new(
                Endpoint::from_static("http://127.0.0.1:1")
                    .connect_lazy()
                    .into(),
            ),
        }),
        prover: Prover::default(),
        health: Health::new(Duration::from_secs(config.timing.tick)),
        zcash_health: Health::new(Duration::from_secs(config.timing.tick)),
        wallet_snapshots: std::sync::Mutex::new(HashMap::new()),
        config,
    };
    (dir, Arc::new(maker))
}

fn acceptance() -> Acceptance {
    Acceptance {
        user_share: SecretShare::random(OsRng).public(),
        user_proof: ShareProof::from_bytes([0; 64]),
        viewing_keys: ViewingKeys::random(OsRng),
    }
}

#[tokio::test]
async fn bridge_alerts_use_exact_public_details_and_isolate_both_networks() {
    let (_dir, maker) = maker();
    let mut maker = Arc::try_unwrap(maker).ok().unwrap();
    let swap = crate::store::Swap {
        id: B256::repeat_byte(5),
        quote: crate::store::Quote {
            id: [1; 32],
            nonce: 0,
            payout: Address::repeat_byte(2),
            payout_note: None,
            amount: 1234567,
            deposit_zat: 100001,
        },
        user_share: SecretShare::random(OsRng).public(),
        viewing: ViewingKeys::random(OsRng),
        zcash_account: AccountUuid::from_uuid(uuid::Uuid::nil()),
        opened_at: 1000,
        t1: 7300,
        sweep: None,
        settled: false,
        refund_started: false,
    };
    assert!(
        maker
            .forward_alert(&swap, "accepted", "Bridge accepted")
            .is_none()
    );
    maker.telegram =
        crate::telegram::Telegram::new(Some("123:private-token"), Some("123456789")).unwrap();
    maker
        .store
        .insert_quote(
            swap.quote.id,
            swap.quote.payout,
            None,
            swap.quote.amount,
            swap.quote.deposit_zat,
            2000,
        )
        .unwrap();
    maker.store.insert_swap(&swap, None).unwrap();
    let share = maker.maker_share(0).unwrap();
    let reverse = crate::store::ReverseSwap {
        id: B256::repeat_byte(6),
        nonce: 0,
        quote: zecswap_api::reverse::Quote {
            terms: Quote {
                quote_id: B256::repeat_byte(1),
                maker: maker.account,
                maker_share: share.public(),
                maker_proof: maker.context([1; 32]).prove_maker(&share, OsRng),
                chain_id: maker.chain_id,
                contract: maker.config.contract,
                token: maker.config.token,
                amount: 1234567,
                deposit_zat: 100001,
                expires_at: 2000,
            },
            user: Address::repeat_byte(2),
            refund_note: B256::repeat_byte(3),
            funding_deadline: 3000,
            ready_deadline: 4000,
            refund_after: 5000,
        },
        acceptance: acceptance(),
        account: swap.zcash_account,
        deposit: Some(TxId::from_bytes([8; 32])),
        sweep: None,
        settled: false,
    };
    let mut keys = Vec::new();
    for (network, name) in [
        (crate::Chain::Testnet, "testnet"),
        (crate::Chain::Mainnet, "mainnet"),
    ] {
        maker.config.network = network;
        maker.chain_id = if name == "testnet" { 11155111 } else { 1 };
        let scope = maker.transaction_scope();
        maker.store.init_transaction_cursor(&scope, 90, 99).unwrap();
        let events: Vec<_> = (0..6)
            .map(|index| {
                (
                    zecswap_chain::evm::SwapEvent {
                        id: swap.id,
                        kind: zecswap_chain::evm::SwapEventKind::Claimed,
                        transaction_hash: B256::repeat_byte(20 + index),
                        block_number: 100,
                        block_hash: B256::repeat_byte(4),
                        log_index: u64::from(index),
                    },
                    None,
                )
            })
            .collect();
        maker
            .store
            .record_evm_window(&scope, 100, 109, &events, false)
            .unwrap();
        for (event, _) in &events {
            maker
                .store
                .save_evm_info(&scope, event.transaction_hash, true)
                .unwrap();
        }
        for (event, direction) in [
            (
                maker
                    .forward_alert(&swap, "accepted", "Bridge accepted")
                    .unwrap(),
                "ZEC → USDC",
            ),
            (
                maker
                    .reverse_alert(&reverse, "accepted", "Bridge accepted")
                    .unwrap(),
                "USDC → ZEC",
            ),
        ] {
            assert!(event.text.contains(&name.to_uppercase()));
            assert!(event.text.contains(&format!("?network={name}")));
            assert!(event.text.contains(direction));
            assert!(event.text.contains("1.234567 USDC ↔ 0.00100001 ZEC"));
            assert!(!event.text.contains("private-token"));
            assert!(!event.text.contains("123456789"));
            assert!(!event.text.contains(&hex::encode(swap.viewing.to_bytes())));
            assert!(!event.text.contains(&hex::encode(*maker.root)));
            assert!(event.text.chars().count() < 4096);
            assert!(event.text.contains("Swap ID (bridge reference):"));
            assert!(!event.text.contains(&format!("/tx/{}", swap.id)));
            if direction == "ZEC → USDC" {
                assert!(
                    event
                        .text
                        .contains(&format!("/tx/{}", B256::repeat_byte(25)))
                );
                assert!(
                    !event
                        .text
                        .contains(&format!("/tx/{}", B256::repeat_byte(20)))
                );
                let path = if name == "testnet" {
                    "sepolia"
                } else {
                    "ethereum"
                };
                assert!(
                    event
                        .text
                        .contains(&format!("https://railscan.io/{path}/tx/"))
                );
            }
            assert!(!keys.contains(&event.key));
            keys.push(event.key);
        }
    }
    let summary = serde_json::to_string(&maker.store.notification_status(true).unwrap()).unwrap();
    assert!(!summary.contains("private-token"));
    assert!(!summary.contains("123456789"));
}

fn request(path: &str, body: impl serde::Serialize) -> Request<Body> {
    Request::post(path)
        .header("content-type", "application/json")
        .body(Body::from(serde_json::to_vec(&body).unwrap()))
        .unwrap()
}

#[tokio::test]
async fn monitor_route_is_authenticated_read_only_and_survives_upstream_outages() {
    let (_dir, maker) = maker();
    let app = crate::api::router(maker.clone());
    let response = app
        .oneshot(Request::get("/v1/monitor").body(Body::empty()).unwrap())
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
    assert_eq!(response.headers()["cache-control"], "no-store");
    let mut maker = Arc::try_unwrap(maker).ok().unwrap();
    maker.monitoring = monitoring::Monitoring::new(Some("monitor-test-token"));
    let maker = Arc::new(maker);
    let app = crate::api::router(maker.clone());
    for (token, status) in [
        ("wrong-token", StatusCode::UNAUTHORIZED),
        ("monitor-test-token", StatusCode::OK),
    ] {
        let response = app
            .clone()
            .oneshot(
                Request::get("/v1/monitor")
                    .header("authorization", format!("Bearer {token}"))
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), status);
        if status == StatusCode::OK {
            let body = axum::body::to_bytes(response.into_body(), 100_000)
                .await
                .unwrap();
            let json: serde_json::Value = serde_json::from_slice(&body).unwrap();
            assert_eq!(json["counts"]["quotes"], 0);
            assert!(json["inventory"]["usdcAvailable"].is_null());
            assert_eq!(json["watchtowerHealthy"], false);
            assert!(json["swaps"].as_array().unwrap().is_empty());
            assert!(
                !String::from_utf8(body.to_vec())
                    .unwrap()
                    .contains("monitor-test-token")
            );
        }
    }
    assert_eq!(maker.store.monitor_counts().unwrap().quotes, 0);
    assert!(maker.store.unsettled_swaps().unwrap().is_empty());
}

#[tokio::test(start_paused = true)]
async fn readiness_tracks_watchtower_and_info_reports_the_configured_deployment() {
    let (_dir, maker) = maker();
    let app = crate::api::router(maker.clone());
    for expected in [
        StatusCode::SERVICE_UNAVAILABLE,
        StatusCode::NO_CONTENT,
        StatusCode::SERVICE_UNAVAILABLE,
    ] {
        let response = app
            .clone()
            .oneshot(Request::get("/healthz").body(Body::empty()).unwrap())
            .await
            .unwrap();
        assert_eq!(response.status(), expected);
        assert_eq!(response.headers()["cache-control"], "no-store");
        if expected == StatusCode::NO_CONTENT {
            tokio::time::advance(Duration::from_secs(45)).await;
        } else {
            maker.health.completed();
            maker.zcash_health.completed();
        }
    }
    let response = app
        .oneshot(Request::get("/v1/info").body(Body::empty()).unwrap())
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    let body = axum::body::to_bytes(response.into_body(), 4096)
        .await
        .unwrap();
    let info: zecswap_api::service::MakerInfo = serde_json::from_slice(&body).unwrap();
    assert_eq!(info.contract, maker.config.contract);
    assert_eq!(info.chain_id, maker.chain_id);
    assert_eq!(info.token, maker.config.token);
    assert!(!info.reverse_enabled);
}

#[tokio::test]
async fn invalid_requests_and_unknown_routes_have_machine_readable_errors() {
    use zecswap_api::service::{ErrorCode, ErrorResponse};

    let (_dir, maker) = maker();
    maker.health.completed();
    maker.zcash_health.completed();
    let app = crate::api::router(maker);
    let cases = [
        (
            request("/v1/reverse/quote", serde_json::json!({"units": "1"})),
            StatusCode::UNPROCESSABLE_ENTITY,
            ErrorCode::InvalidRequest,
        ),
        (
            Request::get("/v1/reverse/swaps/not-a-hash")
                .body(Body::empty())
                .unwrap(),
            StatusCode::BAD_REQUEST,
            ErrorCode::InvalidRequest,
        ),
        (
            Request::get(format!("/v1/reverse/swaps/{}", B256::ZERO))
                .body(Body::empty())
                .unwrap(),
            StatusCode::NOT_FOUND,
            ErrorCode::UnknownSwap,
        ),
        (
            Request::get("/missing").body(Body::empty()).unwrap(),
            StatusCode::NOT_FOUND,
            ErrorCode::NotFound,
        ),
        (
            Request::post("/v1/info").body(Body::empty()).unwrap(),
            StatusCode::METHOD_NOT_ALLOWED,
            ErrorCode::MethodNotAllowed,
        ),
    ];
    for (request, status, code) in cases {
        let response = app.clone().oneshot(request).await.unwrap();
        assert_eq!(response.status(), status);
        assert_eq!(response.headers()["cache-control"], "no-store");
        let body = axum::body::to_bytes(response.into_body(), 4096)
            .await
            .unwrap();
        let error: ErrorResponse = serde_json::from_slice(&body).unwrap();
        assert_eq!(error.code, code);
    }
}

#[tokio::test]
async fn reverse_routes_are_disabled_until_configured_and_unknown_status_is_404() {
    let (_dir, maker) = maker();
    maker.health.completed();
    maker.zcash_health.completed();
    let app = crate::api::router(maker);
    let quote = request(
        "/v1/reverse/quote",
        zecswap_api::reverse::QuoteRequest {
            units: 1,
            user: Address::repeat_byte(1),
            refund_note: B256::repeat_byte(2),
        },
    );
    let accept = request(
        &format!("/v1/reverse/quote/{}/accept", B256::ZERO),
        acceptance(),
    );
    for request in [quote, accept] {
        assert_eq!(
            app.clone().oneshot(request).await.unwrap().status(),
            StatusCode::SERVICE_UNAVAILABLE
        );
    }
    let status = Request::get(format!("/v1/reverse/swaps/{}", B256::ZERO))
        .body(Body::empty())
        .unwrap();
    assert_eq!(
        app.oneshot(status).await.unwrap().status(),
        StatusCode::NOT_FOUND
    );
}

#[tokio::test(start_paused = true)]
async fn both_endpoints_return_503_at_startup_and_when_stale() {
    let (_dir, maker) = maker();
    let app = crate::api::router(maker.clone());
    for state in 0..3 {
        let expected = match state {
            1 => {
                maker.health.completed();
                maker.zcash_health.completed();
                (StatusCode::BAD_REQUEST, StatusCode::NOT_FOUND)
            }
            2 => {
                tokio::time::advance(Duration::from_secs(45)).await;
                (
                    StatusCode::SERVICE_UNAVAILABLE,
                    StatusCode::SERVICE_UNAVAILABLE,
                )
            }
            _ => (
                StatusCode::SERVICE_UNAVAILABLE,
                StatusCode::SERVICE_UNAVAILABLE,
            ),
        };
        let quote = request(
            "/v1/quote",
            QuoteRequest {
                units: 0,
                payout: Address::repeat_byte(1),
                payout_note: None,
            },
        );
        let accept = request(&format!("/v1/quote/{}/accept", B256::ZERO), acceptance());
        assert_eq!(
            app.clone().oneshot(quote).await.unwrap().status(),
            expected.0
        );
        assert_eq!(
            app.clone().oneshot(accept).await.unwrap().status(),
            expected.1
        );
    }
}

#[tokio::test(start_paused = true)]
async fn accept_rechecks_health_after_waiting_without_consuming_quote() {
    let (_dir, maker) = maker();
    let id = [1; 32];
    maker
        .store
        .insert_quote(id, Address::repeat_byte(1), None, 1, 1, unix_now() + 120)
        .unwrap();
    maker.health.completed();
    maker.zcash_health.completed();
    let wallet_lock = maker.zcash.lock().await;
    let response = tokio::spawn(crate::api::router(maker.clone()).oneshot(request(
        &format!("/v1/quote/{}/accept", B256::from(id)),
        acceptance(),
    )));
    tokio::task::yield_now().await;
    tokio::time::advance(Duration::from_secs(45)).await;
    drop(wallet_lock);
    assert_eq!(
        response.await.unwrap().unwrap().status(),
        StatusCode::SERVICE_UNAVAILABLE
    );
    assert!(maker.store.take_quote(&id, unix_now()).unwrap().is_some());
}

#[tokio::test]
async fn sync_panic_reopens_wallet_and_next_sync_runs() {
    let (_dir, maker) = maker();
    let mut zcash = maker.zcash.lock().await;
    let path = maker.config.data_dir.join("wallet.sqlite");
    let old = maker.config.data_dir.join("old.sqlite");
    let synced = zcash
        .sync_with(&maker.config, |wallet, _| async {
            // Reopening must create a new file at the original path, not keep this connection.
            std::fs::rename(&path, &old).unwrap();
            let _owned = wallet;
            panic!("injected sync panic");
        })
        .await
        .unwrap();
    assert!(!synced, "a panic must take the failed-sync policy path");
    assert!(old.exists() && path.exists());
    assert!(
        zcash
            .sync_with(&maker.config, |mut wallet, mut client| async move {
                let result = wallet.sync(&mut client).await;
                (wallet, result)
            })
            .await
            .unwrap()
    );
}

#[tokio::test]
async fn failed_reopen_leaves_no_wallet_and_retries_on_the_next_pass() {
    let (dir, maker) = maker();
    let mut zcash = maker.zcash.lock().await;
    let saved = dir.path().join("saved-wallet");
    let result = zcash
        .sync_with(&maker.config, |wallet, _| async {
            std::fs::rename(&maker.config.data_dir, &saved).unwrap();
            std::fs::write(&maker.config.data_dir, b"blocks reopening").unwrap();
            let _owned = wallet;
            panic!("injected sync panic");
        })
        .await;
    assert!(result.is_err());
    assert!(zcash.wallet.is_none());
    std::fs::remove_file(&maker.config.data_dir).unwrap();
    std::fs::rename(&saved, &maker.config.data_dir).unwrap();
    assert!(
        zcash
            .sync_with(&maker.config, |mut wallet, mut client| async move {
                let result = wallet.sync(&mut client).await;
                (wallet, result)
            })
            .await
            .unwrap()
    );
}

#[tokio::test(start_paused = true)]
async fn evm_health_alone_never_admits_new_swaps_and_snapshots_expire() {
    let (_dir, maker) = maker();
    maker.health.completed();
    assert!(maker.check_watchtower().is_err());
    maker.zcash_health.completed();
    assert!(maker.check_watchtower().is_ok());
    let account = AccountUuid::from_uuid(uuid::Uuid::nil());
    maker.wallet_snapshots.lock().unwrap().insert(
        account,
        WalletSnapshot {
            funds: Funds {
                total: 123,
                spendable: 123,
            },
            sweep: None,
        },
    );
    let _busy = maker.zcash.lock().await;
    assert_eq!(maker.wallet_snapshot(account).unwrap().funds.spendable, 123);
    tokio::time::advance(Duration::from_secs(maker.config.timing.tick * 3)).await;
    maker.health.completed();
    assert!(maker.wallet_snapshot(account).is_none());
    assert!(maker.check_watchtower().is_err());
}

async fn anvil_rpc(url: &str, method: &str, params: serde_json::Value) -> serde_json::Value {
    let reply: serde_json::Value = reqwest::Client::new()
        .post(url)
        .json(&serde_json::json!({"jsonrpc": "2.0", "id": 1, "method": method, "params": params}))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert!(reply.get("error").is_none(), "{reply}");
    reply["result"].clone()
}

#[tokio::test]
async fn a_locked_wallet_cannot_block_refunds_and_reorgs_resume_after_restart() {
    use alloy::node_bindings::Anvil;
    use zecswap_chain::evm::{U256, deploy};

    let Ok(anvil) = Anvil::new().try_spawn() else {
        eprintln!("skipped: anvil is not installed");
        return;
    };
    let artifacts = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../contracts/out");
    if !artifacts.join("ZecSwap.sol/ZecSwap.json").exists() {
        eprintln!("skipped: run forge build in contracts/");
        return;
    }
    let creation = |name: &str| {
        let artifact: serde_json::Value = serde_json::from_slice(
            &std::fs::read(artifacts.join(format!("{name}.sol/{name}.json"))).unwrap(),
        )
        .unwrap();
        hex::decode(
            artifact["bytecode"]["object"]
                .as_str()
                .unwrap()
                .trim_start_matches("0x"),
        )
        .unwrap()
    };
    let (_dir, maker) = maker();
    let mut maker = Arc::try_unwrap(maker).ok().unwrap();
    let key = PrivateKeySigner::from(anvil.keys()[0].clone());
    let url = anvil.endpoint();
    let token = deploy(&url, key.clone(), creation("TestToken"))
        .await
        .unwrap();
    let mut code = creation("ZecSwap");
    code.extend_from_slice(&U256::from(maker.lock_duration).to_be_bytes::<32>());
    code.extend_from_slice(&Address::ZERO.into_word().0);
    let contract = deploy(&url, key.clone(), code).await.unwrap();
    maker.account = key.address();
    maker.config.evm_rpc = url.clone();
    maker.config.contract = contract;
    maker.config.token = token;
    maker.config.evm_confirmations = std::num::NonZeroU32::new(3).unwrap();
    maker.settlement = Settlement::connect(&url, contract, key).unwrap();
    maker
        .settlement
        .mint_test_token(token, maker.account, 1_000_000)
        .await
        .unwrap();
    maker
        .settlement
        .add_inventory(token, 1_000_000)
        .await
        .unwrap();
    maker
        .store
        .insert_quote(
            [1; 32],
            Address::repeat_byte(7),
            None,
            1_000_000,
            100_000,
            unix_now() + 120,
        )
        .unwrap();
    let quote = maker
        .store
        .take_quote(&[1; 32], unix_now())
        .unwrap()
        .unwrap();
    let user_share = SecretShare::random(OsRng).public();
    let now = maker.settlement.now().await.unwrap();
    let swap = Swap {
        id: swap_id(maker.account, &user_share),
        quote,
        user_share,
        viewing: ViewingKeys::random(OsRng),
        zcash_account: AccountUuid::from_uuid(uuid::Uuid::nil()),
        opened_at: now,
        t1: now + 7200,
        sweep: None,
        settled: false,
        refund_started: false,
    };
    maker.store.insert_swap(&swap, None).unwrap();
    maker
        .settlement
        .open(&OpenRequest {
            token,
            amount: swap.quote.amount,
            maker_share: &maker.maker_share(swap.quote.nonce).unwrap().public(),
            user_share: &swap.user_share,
            user: swap.quote.payout,
            t0: now + 3600,
            t1: swap.t1,
            payout_note: None,
        })
        .await
        .unwrap();
    anvil_rpc(
        &url,
        "evm_setNextBlockTimestamp",
        serde_json::json!([now + 3300]),
    )
    .await;
    anvil_rpc(&url, "evm_mine", serde_json::json!([])).await;
    let before_cancel = anvil_rpc(&url, "evm_snapshot", serde_json::json!([])).await;

    // Simulates either a stalled sync or a proof that holds the wallet throughout.
    // This is a current-thread runtime, so EVM progress cannot rely on spare async workers.
    let busy = maker.zcash.lock().await;
    for _ in 0..2 {
        tokio::time::timeout(Duration::from_secs(10), maker.tick())
            .await
            .expect("EVM cancellation waited for the wallet lock")
            .unwrap();
    }
    assert_eq!(
        maker.settlement.swap(swap.id).await.unwrap().unwrap().stage,
        Stage::Refunded
    );
    maker.tick().await.unwrap();
    assert!(
        !maker.status(swap.id).unwrap().unwrap().settled,
        "one confirmation is not final"
    );
    assert!(maker.store.swap(&swap.id).unwrap().unwrap().refund_started);
    anvil_rpc(&url, "anvil_mine", serde_json::json!(["0x2"])).await;
    maker.tick().await.unwrap();
    assert!(maker.status(swap.id).unwrap().unwrap().settled);
    drop(busy);

    // Reopen persisted state, then remove both the lock and the refund from the chain.
    let store_path = _dir.path().join("maker.sqlite");
    maker.store = Store::open(&store_path).unwrap();
    assert_eq!(maker.store.watched_swaps().unwrap().len(), 1);
    assert!(maker.store.unsettled_swaps().unwrap().is_empty());
    assert_eq!(
        anvil_rpc(&url, "evm_revert", serde_json::json!([before_cancel])).await,
        true
    );
    let busy = maker.zcash.lock().await;
    tokio::time::timeout(Duration::from_secs(10), maker.tick())
        .await
        .unwrap()
        .unwrap();
    assert!(!maker.status(swap.id).unwrap().unwrap().settled);
    assert!(
        maker
            .settlement
            .swap(swap.id)
            .await
            .unwrap()
            .unwrap()
            .refund_lock_until
            > 0
    );
    tokio::time::timeout(Duration::from_secs(10), maker.tick())
        .await
        .unwrap()
        .unwrap();
    assert_eq!(
        maker.settlement.swap(swap.id).await.unwrap().unwrap().stage,
        Stage::Refunded
    );
    drop(busy);
}
