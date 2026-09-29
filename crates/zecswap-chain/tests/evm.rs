//! The settlement client against a local anvil; skipped where Foundry isn't installed, or
//! the contracts aren't built (`forge build` in `contracts/`).

use std::path::Path;
use std::time::Duration;

use alloy::node_bindings::Anvil;
use zecswap_chain::evm::{
    Address, OpenRequest, PrivateKeySigner, Settlement, Stage, U256, deploy, swap_id,
};
use zecswap_core::{Domain, NetworkType, derive_maker_share, derive_user_keys};

#[tokio::test]
async fn a_send_that_fails_leaves_no_nonce_gap() {
    let Ok(anvil) = Anvil::new().block_time(1).try_spawn() else {
        eprintln!("skipped: anvil is not installed");
        return;
    };
    let key = PrivateKeySigner::from(anvil.keys()[0].clone());
    let account = key.address();
    let chain = Settlement::connect(&anvil.endpoint(), Address::ZERO, key).unwrap();
    let to = Address::repeat_byte(1);

    let too_much = chain.eth_balance(account).await.unwrap() + U256::from(1);
    assert!(chain.send_eth(to, too_much).await.is_err());
    let next = tokio::time::timeout(Duration::from_secs(20), chain.send_eth(to, U256::from(1)));
    next.await
        .expect("the next send is stuck behind a nonce gap")
        .unwrap();
}

/// A swap paid into Railgun, settled by a relayer on the user's signatures: the Rust signer,
/// swap id, note commitment and `Shield` decoding all agree with the contract.
#[tokio::test]
async fn a_railgun_swap_settles_on_the_users_signatures() {
    let Ok(anvil) = Anvil::new().block_time(1).try_spawn() else {
        eprintln!("skipped: anvil is not installed");
        return;
    };
    let artifacts = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../contracts/out");
    if !artifacts.join("MockRailgun.sol").exists() {
        eprintln!("skipped: run `forge build` in contracts/");
        return;
    }
    let url = anvil.endpoint();
    let maker_key = PrivateKeySigner::from(anvil.keys()[0].clone());
    let relayer_key = PrivateKeySigner::from(anvil.keys()[1].clone());
    let (maker_account, relayer_account) = (maker_key.address(), relayer_key.address());
    let creation = |name: &str| creation_code(&artifacts, name);
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

    let seed = [7; 64];
    let keys = derive_user_keys(&seed, NetworkType::Test, 0, 0).unwrap();
    let wallet = zecswap_railgun::Keys::from_seed(&seed, 0);
    let note = wallet.note(&keys.note_entropy).unwrap();
    let e = derive_maker_share(&[9; 32], 0).unwrap();
    let now = maker.now().await.unwrap();
    maker
        .open(&OpenRequest {
            token,
            amount: 1_000_000,
            maker_share: &e.public(),
            user_share: &keys.share.public(),
            user: keys.auth.address().into(),
            t0: now + 3_600,
            t1: now + 7_200,
            payout_note: Some(note.commitment().into()),
        })
        .await
        .unwrap();
    let id = swap_id(maker_account, &keys.share.public());
    maker.ready(id).await.unwrap();

    let user = Settlement::read_only(&url, contract).unwrap();
    let swap = user
        .swap(id)
        .await
        .unwrap()
        .expect("the swap is keyed as in Rust");
    assert_eq!(swap.user, Address::from(keys.auth.address()));
    assert_eq!(swap.payout_note, Some(note.commitment().into()));
    assert!(user.railgun_accepts(token).await.unwrap());
    assert!(
        user.ready(id).await.is_err(),
        "a read-only connection sends nothing"
    );

    let relayer = Settlement::connect(&url, contract, relayer_key).unwrap();
    let domain = Domain {
        chain_id: user.chain_id().await.unwrap(),
        contract: contract.into(),
    };
    let deadline = user.now().await.unwrap() + 60;
    let lock = keys.auth.sign(&domain.lock_claim(&id.0, deadline));
    relayer
        .lock_claim_with_sig(id, deadline, &lock)
        .await
        .unwrap();
    relayer.claim(id, &keys.share).await.unwrap();
    let fee = 20_000;
    let payout = keys
        .auth
        .sign(&domain.payout(&id.0, &relayer_account.into(), fee));
    let tx = relayer.payout(id, &note, fee, &payout).await.unwrap();

    let swap = user.swap(id).await.unwrap().unwrap();
    assert_eq!(swap.stage, Stage::Claimed);
    assert!(swap.paid_out);
    let shielded = user.shielded(tx).await.unwrap();
    assert_eq!(shielded.len(), 1);
    assert_eq!(shielded[0].note, note);
    assert_eq!(shielded[0].token, token);
    assert_eq!(shielded[0].value + shielded[0].fee, 1_000_000 - fee);
    assert!(wallet.open(&shielded[0].note).is_some());
    assert_eq!(
        user.token_balance(token, relayer_account).await.unwrap(),
        fee
    );
}

fn creation_code(artifacts: &Path, name: &str) -> Vec<u8> {
    let path = artifacts.join(format!("{name}.sol/{name}.json"));
    let artifact: serde_json::Value =
        serde_json::from_slice(&std::fs::read(path).unwrap()).unwrap();
    let code = artifact["bytecode"]["object"].as_str().unwrap();
    hex::decode(code.trim_start_matches("0x")).unwrap()
}

#[tokio::test]
async fn reverse_signatures_fund_claim_and_refund_the_committed_note() {
    use alloy::network::EthereumWallet;
    use alloy::providers::{Provider, ProviderBuilder};
    use alloy::rpc::types::TransactionRequest;
    use zecswap_chain::evm::reverse_funding_calls;
    use zecswap_core::ReverseOpen;

    let Ok(anvil) = Anvil::new().block_time(1).try_spawn() else {
        eprintln!("skipped: anvil is not installed");
        return;
    };
    let artifacts = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../contracts/out");
    if !artifacts.join("MockRailgun.sol").exists() {
        eprintln!("skipped: run `forge build` in contracts/");
        return;
    }
    let url = anvil.endpoint();
    let maker_key = PrivateKeySigner::from(anvil.keys()[0].clone());
    let relay_key = PrivateKeySigner::from(anvil.keys()[1].clone());
    let maker_address = maker_key.address();
    let relay_address = relay_key.address();
    let railgun = deploy(
        &url,
        maker_key.clone(),
        creation_code(&artifacts, "MockRailgun"),
    )
    .await
    .unwrap();
    let token = deploy(
        &url,
        maker_key.clone(),
        creation_code(&artifacts, "TestToken"),
    )
    .await
    .unwrap();
    let mut code = creation_code(&artifacts, "ZecSwap");
    code.extend_from_slice(&U256::from(600).to_be_bytes::<32>());
    code.extend_from_slice(&railgun.into_word().0);
    let contract = deploy(&url, maker_key.clone(), code).await.unwrap();
    let maker = Settlement::connect(&url, contract, maker_key).unwrap();
    let relay = Settlement::connect(&url, contract, relay_key.clone()).unwrap();
    let provider = ProviderBuilder::new()
        .disable_recommended_fillers()
        .with_gas_estimation()
        .with_simple_nonce_management()
        .fetch_chain_id()
        .wallet(EthereumWallet::from(relay_key))
        .connect_http(url.parse().unwrap());
    let domain = Domain {
        chain_id: maker.chain_id().await.unwrap(),
        contract: contract.into(),
    };
    let seed = [7; 64];
    let wallet = zecswap_railgun::Keys::from_seed(&seed, 0);
    maker
        .mint_test_token(token, relay_address, 2_000_000)
        .await
        .unwrap();

    for index in 0..2 {
        let keys = derive_user_keys(&seed, NetworkType::Test, 0, index).unwrap();
        let e = derive_maker_share(&[9; 32], index.into()).unwrap();
        let note = wallet.note(&keys.note_entropy).unwrap();
        let now = maker.now().await.unwrap();
        let terms = ReverseOpen {
            maker: maker_address.into(),
            user: keys.auth.address(),
            token: token.into(),
            amount: 1_000_000,
            maker_share: e.public(),
            user_share: keys.share.public(),
            t0: now + 3600,
            t1: now + 7200,
            refund_note: note.commitment(),
            deadline: now + 300,
        };
        let signature = keys.auth.sign(&domain.open_reverse(&terms));
        for (to, data) in reverse_funding_calls(contract, &terms, &signature) {
            let receipt = provider
                .send_transaction(TransactionRequest::default().to(to).input(data.into()))
                .await
                .unwrap()
                .get_receipt()
                .await
                .unwrap();
            assert!(receipt.status());
        }
        let id = swap_id(keys.auth.address().into(), &e.public());
        let chain = maker.swap(id).await.unwrap().unwrap();
        assert_eq!(chain.maker_share, keys.share.public());
        assert_eq!(chain.user_share, e.public());
        assert_eq!(
            maker
                .confirmed_reverse_funding(id, 1)
                .await
                .unwrap()
                .unwrap()
                .refund_note,
            zecswap_chain::evm::B256::from(note.commitment())
        );
        assert!(
            maker
                .confirmed_reverse_funding(id, 100)
                .await
                .unwrap()
                .is_none()
        );
        assert_eq!(maker.confirmed_swap(id, 1).await.unwrap(), Some(chain));
        let deadline = maker.now().await.unwrap() + 60;
        if index == 0 {
            let signature = keys.auth.sign(&domain.ready(&id.0, deadline));
            relay
                .ready_with_sig(id, deadline, &signature)
                .await
                .unwrap();
            maker.lock_claim(id).await.unwrap();
            maker.claim(id, &e).await.unwrap();
            assert_eq!(
                maker.balance_of(maker_address, token).await.unwrap(),
                terms.amount
            );
            assert_eq!(
                maker
                    .swap(id)
                    .await
                    .unwrap()
                    .unwrap()
                    .revealed()
                    .unwrap()
                    .unwrap()
                    .public(),
                e.public()
            );
        } else {
            let signature = keys.auth.sign(&domain.lock_refund(&id.0, deadline));
            relay
                .lock_refund_with_sig(id, deadline, &signature)
                .await
                .unwrap();
            relay.refund(id, &keys.share).await.unwrap();
            let fee = 20_000;
            let signature =
                keys.auth
                    .sign(&domain.refund_payout(&id.0, &relay_address.into(), fee));
            let tx = relay
                .refund_payout(id, &note, fee, &signature)
                .await
                .unwrap();
            let notes = maker.shielded(tx).await.unwrap();
            assert_eq!(notes.len(), 1);
            assert_eq!(notes[0].value + notes[0].fee, terms.amount - fee);
            assert!(wallet.open(&notes[0].note).is_some());
            assert!(maker.swap(id).await.unwrap().unwrap().paid_out);
        }
    }
}
