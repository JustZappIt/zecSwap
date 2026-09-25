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
