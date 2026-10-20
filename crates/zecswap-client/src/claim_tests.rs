//! A relayer request that fails without a refusal, as one meeting a gateway error or its own
//! timeout does, may still land: the user watches the chain for it and posts it again, and the
//! claim goes through, where it used to end on the answer that never came.

use std::path::Path;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

use alloy::network::EthereumWallet;
use alloy::node_bindings::Anvil;
use alloy::providers::{Provider, ProviderBuilder};
use alloy::rpc::types::TransactionRequest;
use alloy_primitives::{Address, B256, U256};
use axum::extract::{Request, State};
use axum::http::StatusCode;
use axum::middleware::{Next, from_fn_with_state};
use axum::response::{IntoResponse, Response};
use rand::{rand_core::UnwrapErr, rngs::SysRng};
use zecswap_api::{Network, Quote};
use zecswap_chain::evm::{PrivateKeySigner, Settlement, Stage, deploy, swap_id};
use zecswap_core::{SwapContext, derive_maker_share};

use crate::tokens::tests::serve;
use crate::{MakerApi, RelayerApi, Route, User, UserSwap};

const SEED: [u8; 64] = [3; 64];
const AMOUNT: u128 = 1_000_000;
const FEE: u64 = 20_000;

/// A gateway that fails the first claim posted to it, before the relayer sees it.
async fn failing_first_claim(
    State(claims): State<Arc<AtomicUsize>>,
    request: Request,
    next: Next,
) -> Response {
    if request.uri().path() == "/v1/claim" && claims.fetch_add(1, Ordering::SeqCst) == 0 {
        return StatusCode::BAD_GATEWAY.into_response();
    }
    next.run(request).await
}

#[tokio::test]
async fn a_claim_whose_answer_never_comes_goes_through_once_the_chain_shows_it() {
    let Ok(anvil) = Anvil::new().block_time(1).try_spawn() else {
        eprintln!("skipped: anvil is not installed");
        return;
    };
    let artifacts = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../contracts/out");
    if !artifacts.join("MockRailgun.sol").exists() {
        eprintln!("skipped: run `forge build` in contracts/");
        return;
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
    code.extend_from_slice(&railgun.into_word().0);
    let contract = deploy(&url, maker_key.clone(), code).await.unwrap();
    let maker = Settlement::connect(&url, contract, maker_key).unwrap();
    maker
        .mint_test_token(token, maker_account, AMOUNT)
        .await
        .unwrap();
    maker.add_inventory(token, AMOUNT).await.unwrap();

    let config = zecswap_relayer::Config {
        network: Network::Testnet,
        evm_rpc: url.clone(),
        contract,
        token,
        maker: maker_account,
        listen: "127.0.0.1:0".parse().unwrap(),
        fee: FEE,
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
    };
    let relayer = zecswap_relayer::Relayer::new(config, relayer_key.clone(), None)
        .await
        .unwrap();
    let snapshot = serde_json::to_value(relayer.monitor_snapshot()).unwrap();
    let lane = |name: &str| {
        snapshot["accounts"][name]
            .as_str()
            .unwrap()
            .parse::<Address>()
    };
    let (claims, locks) = (lane("claims").unwrap(), lane("locks").unwrap());
    let funder = ProviderBuilder::new()
        .wallet(EthereumWallet::from(funder_key))
        .connect_http(url.parse().unwrap());
    for account in [claims, locks] {
        let fund = TransactionRequest::default()
            .to(account)
            .value(U256::from(10u128.pow(18)));
        let funded = funder.send_transaction(fund).await.unwrap();
        assert!(funded.get_receipt().await.unwrap().status());
    }
    let claims_posted = Arc::new(AtomicUsize::new(0));
    let gateway = zecswap_relayer::api::router(Arc::new(relayer)).layer(from_fn_with_state(
        Arc::clone(&claims_posted),
        failing_first_claim,
    ));
    let relayer_api = RelayerApi::new(serve(gateway).await)
        .unwrap()
        .with_timeout(Duration::from_secs(3))
        .unwrap();
    let user = User::new(
        &SEED,
        &SEED,
        Network::Testnet,
        Settlement::read_only(&url, contract).unwrap(),
        MakerApi::new("http://127.0.0.1:1").unwrap(),
        token,
        Route::Railgun {
            relayer: relayer_api,
            max_fee: FEE.into(),
        },
    );

    // The swap as the maker opened it and, once the deposit confirmed, readied it.
    let e = derive_maker_share(&[9; 32], 0).unwrap();
    let chain_id = maker.chain_id().await.unwrap();
    let quote_id = B256::repeat_byte(1);
    let context = SwapContext {
        chain_id,
        contract: contract.into(),
        quote_id: quote_id.0,
    };
    let now = maker.now().await.unwrap();
    let mut swap = UserSwap {
        index: 0,
        quote: Quote {
            quote_id,
            maker: maker_account,
            maker_share: e.public(),
            maker_proof: context.prove_maker(&e, UnwrapErr(SysRng)),
            chain_id,
            contract,
            token,
            amount: AMOUNT,
            deposit_zat: 1,
            expires_at: now + 600,
            network_cost: None,
        },
        swap_id: B256::ZERO,
        t0: now + 3_600,
        t1: now + 7_200,
        ticket: None,
    };
    let terms = user.terms(&swap).unwrap();
    swap.swap_id = swap_id(maker_account, &terms.user_share);
    let id = swap.swap_id;
    maker.open(&terms).await.unwrap();
    maker.ready(id, &terms).await.unwrap();

    let claiming = tokio::spawn(async move { user.claim(&swap).await });
    // Once the lock lands, the gateway fails the claim; the one posted again goes out, and
    // mining stops until its client has given up on the answer.
    let chain = ProviderBuilder::new().connect_http(url.parse().unwrap());
    let mine_every = async |seconds: u64| {
        chain
            .raw_request::<_, serde_json::Value>("evm_setIntervalMining".into(), (seconds,))
            .await
            .unwrap();
    };
    let state = async || maker.swap(id, &terms).await.unwrap().unwrap();
    while state().await.claim_lock_until == 0 {
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    mine_every(0).await;
    while chain.get_transaction_count(claims).pending().await.unwrap() == 0 {
        assert!(!claiming.is_finished(), "{:?}", claiming.await);
        tokio::time::sleep(Duration::from_millis(200)).await;
    }
    tokio::time::sleep(Duration::from_secs(5)).await;
    mine_every(1).await;

    let paid = claiming.await.unwrap().unwrap();
    assert_eq!(paid.amount, AMOUNT - u128::from(FEE));
    assert_eq!(claims_posted.load(Ordering::SeqCst), 2);
    let landed = state().await;
    assert!(landed.stage == Stage::Claimed && landed.paid_out);
    // One lock, one claim and one payout: nothing twice.
    for (account, sent) in [(locks, 1), (claims, 1), (relayer_key.address(), 1)] {
        assert_eq!(
            chain.get_transaction_count(account).await.unwrap(),
            sent,
            "{account}"
        );
    }
}
