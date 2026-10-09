//! A relayer refuses, before sending anything, what it shouldn't pay for: a swap of another
//! token or maker, and terms that are not the swap's. The contract would revert such a call
//! anyway, but a claim whose payout carries other terms, or a fee that leaves nothing to shield,
//! or that Railgun won't take, would land and reveal the user's share before the payout failed.

use std::path::Path;

use alloy::network::EthereumWallet;
use alloy::node_bindings::Anvil;
use alloy::providers::{Provider, ProviderBuilder};
use alloy::rpc::types::TransactionRequest;
use alloy::sol_types::SolCall;
use zecswap_api::relayer::Rescue;
use zecswap_api::reverse::{Authorization, Refund};
use zecswap_chain::evm::{U256, deploy, reverse_funding_calls, reverse_swap_id, swap_id};
use zecswap_core::{NetworkType, ReverseOpen, derive_maker_share, derive_user_keys};

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
        evm_rpc: url.clone(),
        contract,
        token,
        maker: maker_account,
        listen: "127.0.0.1:0".parse().unwrap(),
        fee: 20_000,
        claim_margin: 30,
        reverse_funding: None,
        railgun_sends: None,
    };
    let relayer_for = |config: Config| Relayer {
        config,
        account: relayer_account,
        domain,
        settlement: Settlement::connect(&url, contract, relayer_key.clone()).unwrap(),
        monitor: crate::monitor::Monitor::new(MonitorToken::default()),
        sends: None,
        history: None,
    };
    let relayer = relayer_for(config.clone());
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
    let sent = || async { chain.get_transaction_count(relayer_account).await.unwrap() };
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
    maker.ready(id, &terms).await.unwrap();
    let deadline = now + 60;
    let lock = |terms: &zecswap_core::Terms| LockClaim {
        swap_id: id,
        terms: terms.into(),
        deadline,
        signature: keys.auth.sign(&domain.lock_claim(&id, deadline)).into(),
    };
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
    relayer.lock_claim(lock(&terms)).await.unwrap();
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
    relayer.payout(payout(&terms)).await.unwrap();
    assert_eq!(sent().await, 2);
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
    relayer.observe("ready_reverse", readied).unwrap();
    assert_eq!(sent().await, 3);
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
}
