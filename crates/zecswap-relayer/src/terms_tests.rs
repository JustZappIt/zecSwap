//! A relayer refuses, before sending anything, what it shouldn't pay for: a swap of another
//! token or maker, terms that are not the swap's, and, where it prices gas, a fee under its
//! quote. The contract would revert such a call anyway, but a claim whose payout carries other
//! terms, or a fee that leaves nothing to shield, or that Railgun won't take, would land and
//! reveal the user's share before the payout failed.

use std::path::Path;

use alloy::network::EthereumWallet;
use alloy::node_bindings::Anvil;
use alloy::providers::{Provider, ProviderBuilder};
use alloy::rpc::types::TransactionRequest;
use alloy::sol_types::SolCall;
use zecswap_api::relayer::Rescue;
use zecswap_api::reverse::{Authorization, Refund};
use zecswap_chain::evm::{U256, deploy, reverse_funding_calls, reverse_swap_id, swap_id};
use zecswap_core::{NetworkType, ReverseOpen, UserSwapKeys, derive_maker_share, derive_user_keys};

use super::*;

alloy::sol! {
    function setTokenBlocked(address token, bool blocked);
}

#[tokio::test]
async fn requests_it_should_not_send_are_refused_before_anything_is_sent() {
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
    let (maker_account, relayer_account) = (maker_key.address(), relayer_key.address());
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
        .mint_test_token(token, maker_account, 1_000_000)
        .await
        .unwrap();
    maker.add_inventory(token, 1_000_000).await.unwrap();
    let domain = Domain {
        chain_id: maker.chain_id().await.unwrap(),
        contract: contract.into(),
    };
    let config = Config {
        network: zecswap_api::Network::Testnet,
        evm_rpc: url.clone(),
        contract,
        token,
        maker: maker_account,
        listen: "127.0.0.1:0".parse().unwrap(),
        fee: 20_000,
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
    let relayer_for = |config: Config| Relayer {
        config,
        account: relayer_account,
        domain,
        settlement: Settlement::connect(&url, contract, relayer_key.clone()).unwrap(),
        lanes: crate::lanes::Lanes::connect(&url, contract, &relayer_key).unwrap(),
        lock_duration: 600,
        turns: Default::default(),
        steps: Default::default(),
        monitor: crate::monitor::Monitor::new(MonitorToken::default()),
        sends: None,
        funding: None,

        deposits: None,
        pricing: None,
        history: None,
    };
    let relayer = relayer_for(config.clone());
    relayer.top_up().await.unwrap();
    let lanes = relayer.lane_accounts();
    let strangers = || {
        let other = Address::repeat_byte(0x66);
        [
            relayer_for(Config {
                token: other,
                ..config.clone()
            }),
            relayer_for(Config {
                maker: other,
                ..config.clone()
            }),
        ]
    };
    let funder = ProviderBuilder::new()
        .disable_recommended_fillers()
        .with_gas_estimation()
        .with_simple_nonce_management()
        .fetch_chain_id()
        .wallet(EthereumWallet::from(funder_key.clone()))
        .connect_http(url.parse().unwrap());
    let chain = ProviderBuilder::new().connect_http(url.parse().unwrap());
    let nonces = async || {
        let mut nonces = 0;
        for account in lanes.values().chain([&relayer_account]) {
            nonces += chain.get_transaction_count(*account).await.unwrap();
        }
        nonces
    };
    let before = nonces().await;
    let sent = async || nonces().await - before;
    let sender = async |tx: B256| {
        let receipt = chain.get_transaction_receipt(tx).await.unwrap();
        receipt.unwrap().from
    };
    let refused = |result: Result<Sent>, why: &str| match result {
        Err(RelayerError::Rejected(reason)) => assert!(reason.contains(why), "{reason}"),
        Err(e) => panic!("expected a refusal for {why}, got {e:#}"),
        Ok(_) => panic!("expected a refusal for {why}, but it sent"),
    };

    // A swap paid into the user's Railgun wallet, and the same terms but for its amount.
    let seed = [7; 64];
    let wallet = zecswap_railgun::Keys::from_seed(&seed, 0);
    let keys = derive_user_keys(&seed, NetworkType::Test, 0, 0).unwrap();
    let note = wallet.note(&keys.note_entropy).unwrap();
    let now = maker.now().await.unwrap();
    let terms = zecswap_core::Terms {
        maker: maker_account.into(),
        token: token.into(),
        amount: 1_000_000,
        maker_share: derive_maker_share(&[9; 32], 0).unwrap().public(),
        user_share: keys.share.public(),
        user: keys.auth.address(),
        t0: now + 3_600,
        t1: now + 7_200,
        payout_note: note.commitment(),
    };
    let other = zecswap_core::Terms {
        amount: terms.amount - 1,
        ..terms.clone()
    };
    maker.open(&terms).await.unwrap();
    let id = swap_id(maker_account, &keys.share.public());
    let lock_until = |terms: &zecswap_core::Terms, deadline: u64| LockClaim {
        swap_id: id,
        terms: terms.into(),
        deadline,
        signature: keys.auth.sign(&domain.lock_claim(&id, deadline)).into(),
    };
    let deadline = now + 60;
    let lock = |terms: &zecswap_core::Terms| lock_until(terms, deadline);
    refused(
        relayer.lock_claim(lock(&terms)).await,
        "once ready, or from t0",
    );
    maker.ready(id, &terms).await.unwrap();
    refused(
        relayer.lock_claim(lock_until(&terms, now - 1)).await,
        "expired",
    );
    refused(
        relayer.lock_claim(lock_until(&terms, now + 3_600)).await,
        "too far off",
    );
    let payout = |terms: &zecswap_core::Terms| Payout {
        swap_id: id,
        terms: terms.into(),
        note: (&note).into(),
        fee: 20_000,
        signature: keys
            .auth
            .sign(&domain.payout(&id, &relayer_account.into(), 20_000))
            .into(),
    };
    let claim = |claimed: &zecswap_core::Terms, paid: &zecswap_core::Terms| Claim {
        swap_id: id,
        terms: claimed.into(),
        secret: keys.share.to_be_bytes().into(),
        payout: payout(paid),
    };

    refused(relayer.lock_claim(lock(&other)).await, "other terms");
    for stranger in strangers() {
        refused(
            stranger.lock_claim(lock(&terms)).await,
            "another token or maker",
        );
    }
    assert_eq!(sent().await, 0);
    let locked = relayer.lock_claim(lock(&terms)).await.unwrap();
    assert_eq!(sent().await, 1);
    assert_eq!(sender(locked.transactions[0]).await, lanes["locks"]);
    let again = relayer.lock_claim(lock(&terms)).await.unwrap();
    assert!(again.transactions.is_empty());
    assert_eq!(sent().await, 1);

    refused(relayer.claim(claim(&other, &other)).await, "other terms");
    // The claim alone is right: sending it would reveal the share for a payout that can't land.
    refused(
        relayer.claim(claim(&terms, &other)).await,
        "different swaps or terms",
    );
    let greedy = Claim {
        payout: Payout {
            fee: terms.amount,
            signature: keys
                .auth
                .sign(&domain.payout(&id, &relayer_account.into(), terms.amount))
                .into(),
            ..payout(&terms)
        },
        ..claim(&terms, &terms)
    };
    refused(relayer.claim(greedy).await, "leaves nothing to shield");
    let block = |blocked| {
        let funder = &funder;
        async move {
            let call = setTokenBlockedCall { token, blocked };
            let request = TransactionRequest::default()
                .to(railgun)
                .input(call.abi_encode().into());
            let receipt = funder.send_transaction(request).await.unwrap();
            assert!(receipt.get_receipt().await.unwrap().status());
        }
    };
    block(true).await;
    refused(
        relayer.claim(claim(&terms, &terms)).await,
        "Railgun is not accepting",
    );
    block(false).await;
    assert_eq!(sent().await, 1);
    let swap = maker.swap(id, &terms).await.unwrap().unwrap();
    assert_eq!((swap.stage, swap.secret), (Stage::Ready, [0; 32]));

    maker.claim(id, &terms, &keys.share).await.unwrap();
    refused(relayer.payout(payout(&other)).await, "other terms");
    assert_eq!(sent().await, 1);
    // Priced by gas: 100,000 gas at 1 gwei is 266,666 at 2,666.67 USDC an ETH, over the floor.
    let prices = zecswap_prices::stand_in::start(axum::http::StatusCode::OK).await;
    let pricing = crate::pricing::GasPricing::new(
        &[zecswap_prices::Provider::CoinMarketCap],
        &zecswap_prices::stand_in::keys(),
        1_000,
    )
    .unwrap()
    .served_by(&prices.url);
    pricing.cache_gas_price(crate::sends::now(), 1_000_000_000);
    let priced = Relayer {
        pricing: Some(pricing),
        ..relayer_for(Config {
            fee_gas: 100_000,
            ..config.clone()
        })
    };
    let quoted = priced.terms().await;
    assert_eq!(
        (quoted.fee, quoted.fee_expires_at.is_some()),
        (266_666, true)
    );
    refused(
        priced.payout(payout(&terms)).await,
        "less than the relayer's 266666",
    );
    assert_eq!(sent().await, 1);
    let paid = Payout {
        fee: quoted.fee,
        signature: keys
            .auth
            .sign(&domain.payout(&id, &relayer_account.into(), quoted.fee))
            .into(),
        ..payout(&terms)
    };
    let paid = priced.payout(paid).await.unwrap();
    assert_eq!(sent().await, 2);
    assert_eq!(sender(paid.transactions[0]).await, relayer_account);
    let rescue = Rescue {
        swap_id: id,
        terms: (&other).into(),
        note: (&note).into(),
        fee: 20_000,
        nonce: 0,
        deadline,
        signature: [0; 65].into(),
    };
    refused(relayer.rescue(rescue).await, "other terms");
    assert_eq!(sent().await, 2);

    // A reverse escrow, opened by the user's funds through a stand-in for Relay Adapt.
    let keys = derive_user_keys(&seed, NetworkType::Test, 0, 1).unwrap();
    let note = wallet.note(&keys.note_entropy).unwrap();
    let e = derive_maker_share(&[9; 32], 1).unwrap();
    let open = ReverseOpen {
        maker: maker_account.into(),
        user: keys.auth.address(),
        token: token.into(),
        amount: 500_000,
        maker_share: e.public(),
        user_share: keys.share.public(),
        t0: now + 3_600,
        t1: now + 7_200,
        refund_note: note.commitment(),
        deadline: now + 300,
    };
    maker
        .mint_test_token(token, funder_key.address(), open.amount)
        .await
        .unwrap();
    let signature = keys.auth.sign(&domain.open_reverse(&open));
    for (to, data) in reverse_funding_calls(contract, &open, &signature) {
        let request = TransactionRequest::default().to(to).input(data.into());
        let receipt = funder
            .send_transaction(request)
            .await
            .unwrap()
            .get_receipt()
            .await
            .unwrap();
        assert!(receipt.status());
    }
    let id = reverse_swap_id(keys.auth.address().into(), &e.public());
    let deadline = maker.now().await.unwrap() + 60;
    let escrow = open.terms();
    let other = zecswap_core::Terms {
        t1: escrow.t1 + 1,
        ..escrow.clone()
    };
    let ready = |terms: &zecswap_core::Terms| Authorization {
        swap_id: id,
        terms: terms.into(),
        deadline,
        signature: keys.auth.sign(&domain.ready(&id, deadline)).into(),
    };
    let refund_payout = |terms: &zecswap_core::Terms| Payout {
        swap_id: id,
        terms: terms.into(),
        note: (&note).into(),
        fee: 20_000,
        signature: keys
            .auth
            .sign(&domain.refund_payout(&id, &relayer_account.into(), 20_000))
            .into(),
    };

    refused(relayer.ready_reverse(ready(&other)).await, "other terms");
    for stranger in strangers() {
        refused(
            stranger.ready_reverse(ready(&escrow)).await,
            "another token or maker",
        );
    }
    let refund = Refund {
        swap_id: id,
        terms: (&escrow).into(),
        secret: keys.share.to_be_bytes().into(),
        payout: refund_payout(&other),
    };
    refused(
        relayer.refund_reverse(refund).await,
        "different swaps or terms",
    );
    refused(
        relayer.reverse_refund_payout(refund_payout(&other)).await,
        "other terms",
    );
    assert_eq!(sent().await, 2);
    let relayer = Arc::new(relayer);
    let readied = relayer.ready_reverse(ready(&escrow)).await;
    let readied = relayer.observe("ready_reverse", readied).unwrap();
    assert_eq!(sent().await, 3);
    assert_eq!(sender(readied.transactions[0]).await, lanes["locks"]);
    // What it sent is counted at once, and the gas its receipt shows burned once that is read.
    let snapshot = || {
        serde_json::to_value(relayer.monitor_snapshot()).unwrap()["operations"]["ready_reverse"]
            .clone()
    };
    assert_eq!(snapshot()["sent"], 1);
    let mut readied = snapshot();
    for _ in 0..50 {
        if readied["gasUsed"] != "0" {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
        readied = snapshot();
    }
    let gas: u64 = readied["gasUsed"].as_str().unwrap().parse().unwrap();
    let wei: u128 = readied["gasCostWei"].as_str().unwrap().parse().unwrap();
    assert!(gas > 21_000 && wei >= u128::from(gas), "{readied}");
    assert_eq!(
        maker.swap(id, &escrow).await.unwrap().unwrap().stage,
        Stage::Ready
    );
    let lock_refund = Authorization {
        swap_id: id,
        terms: (&escrow).into(),
        deadline,
        signature: keys.auth.sign(&domain.lock_refund(&id, deadline)).into(),
    };
    refused(relayer.lock_reverse_refund(lock_refund).await, "from t1");
    assert_eq!(sent().await, 3);

    maker
        .mint_test_token(token, maker_account, 2 * terms.amount)
        .await
        .unwrap();
    maker.add_inventory(token, 2 * terms.amount).await.unwrap();
    let open = async |index| {
        let keys = derive_user_keys(&seed, NetworkType::Test, 0, index).unwrap();
        let now = maker.now().await.unwrap();
        let opened = zecswap_core::Terms {
            maker_share: derive_maker_share(&[9; 32], index.into()).unwrap().public(),
            user_share: keys.share.public(),
            user: keys.auth.address(),
            t0: now + 3_600,
            t1: now + 7_200,
            payout_note: wallet.note(&keys.note_entropy).unwrap().commitment(),
            ..terms.clone()
        };
        maker.open(&opened).await.unwrap();
        (swap_id(maker_account, &keys.share.public()), opened, keys)
    };
    let lock = async |id: B256, terms: &zecswap_core::Terms, keys: &UserSwapKeys| {
        let deadline = maker.now().await.unwrap() + 60;
        relayer
            .lock_claim(LockClaim {
                swap_id: id,
                terms: terms.into(),
                deadline,
                signature: keys.auth.sign(&domain.lock_claim(&id, deadline)).into(),
            })
            .await
    };
    let pass = async |seconds: u64| {
        chain
            .raw_request::<_, serde_json::Value>("evm_increaseTime".into(), (seconds,))
            .await
            .unwrap();
        chain
            .raw_request::<_, serde_json::Value>("evm_mine".into(), ())
            .await
            .unwrap();
    };
    let (lapsing, opened, keys) = open(2).await;
    maker.ready(lapsing, &opened).await.unwrap();
    lock(lapsing, &opened, &keys).await.unwrap();
    assert_eq!(sent().await, 4);
    pass(600).await;
    refused(
        lock(lapsing, &opened, &keys).await,
        "the maker has the next turn",
    );
    pass(600).await;
    lock(lapsing, &opened, &keys).await.unwrap();
    assert_eq!(sent().await, 5);
    let note = wallet.note(&keys.note_entropy).unwrap();
    let signature = domain.payout(&lapsing, &relayer_account.into(), 20_000);
    let payout = Payout {
        swap_id: lapsing,
        terms: (&opened).into(),
        note: (&note).into(),
        fee: 20_000,
        signature: keys.auth.sign(&signature).into(),
    };
    let claimed = relayer
        .claim(Claim {
            swap_id: lapsing,
            terms: (&opened).into(),
            secret: keys.share.to_be_bytes().into(),
            payout,
        })
        .await
        .unwrap();
    assert_eq!(sent().await, 7);
    assert_eq!(sender(claimed.transactions[0]).await, lanes["claims"]);
    assert_eq!(sender(claimed.transactions[1]).await, relayer_account);
    let (refunded, opened, keys) = open(3).await;
    maker.lock_refund(refunded, &opened).await.unwrap();
    maker
        .refund(refunded, &opened, &derive_maker_share(&[9; 32], 3).unwrap())
        .await
        .unwrap();
    refused(lock(refunded, &opened, &keys).await, "refunded");
    assert_eq!(sent().await, 7);
}
