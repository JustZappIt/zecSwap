//! A wallet deposits from its seed's deposit addresses through a relayer on anvil, as Veylor will:
//! nothing to sign while an address holds under the minimum; a shield lands to the wallet's own
//! note, each request kept before it is posted; the used addresses are found again from the seed
//! alone; and what Railgun sent back is rescued.

use std::path::Path;
use std::sync::Arc;

use alloy::node_bindings::Anvil;
use alloy::providers::bindings::IMulticall3;
use alloy::providers::{MULTICALL3_ADDRESS, Provider, ProviderBuilder};
use alloy_primitives::{Address, U256};
use zecswap_api::Network;
use zecswap_api::relayer::deposit_fee;
use zecswap_chain::evm::{PrivateKeySigner, Settlement, deploy};

use crate::tokens::tests::serve;
use crate::{DepositDeployment, Depositor, FeeCap, RelayerApi};

const SEED: [u8; 64] = [5; 64];
const VALUE: u128 = 25_000_000;
const FLOOR: u64 = 100_000;
const FEE_BPS: u32 = 10;

#[tokio::test]
async fn a_wallet_shields_its_deposit_addresses_and_finds_them_again_from_its_seed() {
    let Ok(anvil) = Anvil::new().block_time(1).try_spawn() else {
        eprintln!("skipped: anvil is not installed");
        return;
    };
    let artifacts = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../contracts/out");
    if !artifacts.join("Deposits.sol").exists() {
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
    let funder_key = PrivateKeySigner::from(anvil.keys()[0].clone());
    let relayer_key = PrivateKeySigner::from(anvil.keys()[1].clone());
    let deployed = |code: Vec<u8>| deploy(&url, funder_key.clone(), code);
    let railgun = deployed(creation("MockRailgun")).await.unwrap();
    let token = deployed(creation("TestToken")).await.unwrap();
    let mut code = creation("ZecSwap");
    code.extend_from_slice(&U256::from(600).to_be_bytes::<32>());
    code.extend_from_slice(&railgun.into_word().0);
    let contract = deployed(code).await.unwrap();
    let mut code = creation("Deposits");
    code.extend_from_slice(&token.into_word().0);
    code.extend_from_slice(&railgun.into_word().0);
    code.extend_from_slice(&[0; 32]);
    let deposits = deployed(code).await.unwrap();
    // Multicall3, where it is on every public chain, which a fresh anvil lacks.
    let multicall = deployed(IMulticall3::BYTECODE.to_vec()).await.unwrap();
    let chain = ProviderBuilder::new().connect_http(url.parse().unwrap());
    let multicall = chain.get_code_at(multicall).await.unwrap();
    let () = chain
        .raw_request("anvil_setCode".into(), (MULTICALL3_ADDRESS, multicall))
        .await
        .unwrap();
    let funder = Settlement::connect(&url, contract, funder_key).unwrap();
    let chain_id = funder.chain_id().await.unwrap();

    let dir = tempfile::tempdir().unwrap();
    let config = zecswap_relayer::Config {
        network: Network::Testnet,
        evm_rpc: url.clone(),
        contract,
        token,
        maker: Address::repeat_byte(4),
        listen: "127.0.0.1:0".parse().unwrap(),
        fee: 1,
        fee_gas: 0,
        providers: vec![],
        fee_margin_bps: 0,
        claim_margin: 30,
        reveals: None,
        top_up_transactions: 8,
        reverse_funding: None,
        railgun_sends: None,
        gas_alerts: None,
        deposits: Some(zecswap_relayer::DepositsConfig {
            contract: deposits,
            fee: FLOOR,
            fee_gas: 0,
            fee_bps: FEE_BPS,
            max_gas_limit: 2_000_000,
            max_gas_price_wei: 50_000_000_000,
            min_value: 2_000_000,
            // Under the hour wallets sign for where they can.
            max_validity: 1_500,
            max_pending: 8,
            journal: dir.path().join("deposits.sqlite"),
            private_rpc: None,
        }),
    };
    let relayer = Arc::new(
        zecswap_relayer::Relayer::new(config, relayer_key, None)
            .await
            .unwrap(),
    );
    tokio::spawn(relayer.clone().run_top_ups());
    // Until its first top-up, the account deposits go from would answer them to come back later.
    let snapshot = serde_json::to_value(relayer.monitor_snapshot()).unwrap();
    let lane: Address = snapshot["accounts"]["deposits"]
        .as_str()
        .unwrap()
        .parse()
        .unwrap();
    while funder.eth_balance(lane).await.unwrap().is_zero() {
        tokio::time::sleep(std::time::Duration::from_millis(200)).await;
    }
    let relayer_url = serve(zecswap_relayer::api::router(relayer)).await;

    let wallet = Depositor::new(
        &SEED,
        &SEED,
        Network::Testnet,
        Settlement::read_only(&url, deposits).unwrap(),
        RelayerApi::new(relayer_url.clone()).unwrap(),
        DepositDeployment {
            chain_id,
            contract: deposits,
            token,
        },
        FeeCap {
            fee: 1_000_000,
            fee_bps: 50,
        },
    );
    assert!(wallet.recover().await.unwrap().is_empty());
    assert!(wallet.sign_shield(0).await.unwrap().is_none());
    let (first, later) = (wallet.address(0).unwrap(), wallet.address(3).unwrap());
    funder
        .mint_test_token(token, first, 1_000_000)
        .await
        .unwrap();
    assert!(
        wallet.sign_shield(0).await.unwrap().is_none(),
        "under the minimum"
    );
    funder
        .mint_test_token(token, first, VALUE - 1_000_000)
        .await
        .unwrap();

    let mut kept = Vec::new();
    let deposit = wallet
        .shield(0, |request| {
            kept.push(request.clone());
            Ok(())
        })
        .await
        .unwrap()
        .unwrap();
    assert_eq!(kept.len(), 1);
    assert_eq!(kept[0].value, VALUE);
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_secs();
    assert!((now + 1_300..=now + 1_500).contains(&kept[0].valid_before));
    let fee = deposit_fee(FLOOR.into(), FEE_BPS, VALUE);
    assert_eq!(kept[0].fee, fee);
    let shielded = VALUE - fee;
    assert_eq!(deposit.shielded.value, shielded - shielded * 25 / 10_000);
    assert!(wallet.railgun().open(&deposit.shielded.note).is_some());

    funder.mint_test_token(token, later, VALUE).await.unwrap();
    let used = wallet.recover().await.unwrap();
    let indices: Vec<u32> = used.iter().map(|(index, _)| *index).collect();
    assert_eq!(indices, [0, 3]);
    assert!(used[0].1.deposited && used[0].1.balance == 0);
    assert!(!used[1].1.deposited && used[1].1.balance == VALUE);

    // Railgun sends a note back to the vault it came from.
    assert!(wallet.rescue(0, |_| Ok(())).await.unwrap().is_none());
    let vault = funder.deposit_vault(deposits, first).await.unwrap();
    funder
        .mint_test_token(token, vault, 5_000_000)
        .await
        .unwrap();
    let rescued = wallet.rescue(0, |_| Ok(())).await.unwrap().unwrap();
    assert_eq!(
        rescued.shielded.value,
        (5_000_000 - u128::from(FLOOR)) * 9_975 / 10_000
    );
    assert!(wallet.railgun().open(&rescued.shielded.note).is_some());

    // A relayer that answers a new deposit with an earlier one of the address is not believed.
    let terms = RelayerApi::new(relayer_url).unwrap().terms().await.unwrap();
    let earlier = deposit.tx;
    let liar = axum::Router::new()
        .route(
            "/v1/terms",
            axum::routing::get(move || {
                let terms = terms.clone();
                async move { axum::Json(terms) }
            }),
        )
        .route(
            "/v1/deposits/shield",
            axum::routing::post(move || async move {
                axum::Json(zecswap_api::relayer::Sent {
                    transactions: vec![earlier],
                })
            }),
        );
    let fooled = Depositor::new(
        &SEED,
        &SEED,
        Network::Testnet,
        Settlement::read_only(&url, deposits).unwrap(),
        RelayerApi::new(serve(liar).await).unwrap(),
        DepositDeployment {
            chain_id,
            contract: deposits,
            token,
        },
        FeeCap {
            fee: 1_000_000,
            fee_bps: 50,
        },
    );
    funder.mint_test_token(token, first, VALUE).await.unwrap();
    let error = fooled.shield(0, |_| Ok(())).await.unwrap_err();
    assert!(
        error
            .to_string()
            .contains("no deposit of this address to its note"),
        "{error:#}"
    );
}
