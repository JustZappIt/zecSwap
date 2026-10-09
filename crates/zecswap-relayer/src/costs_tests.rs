//! What each broadcaster send cost and earned is read once it has mined: its gas from its
//! receipt, what it was from its own journaled bytes, a reverted one included.

use std::path::Path;

use alloy::eips::Encodable2718;
use alloy::network::{EthereumWallet, TransactionBuilder};
use alloy::primitives::aliases::{U72, U120};
use alloy::primitives::keccak256;
use alloy::providers::{Provider, ProviderBuilder};
use alloy::rpc::types::TransactionRequest;
use alloy::sol_types::SolCall;
use zecswap_api::server::MonitorToken;
use zecswap_chain::evm::railgun::{
    BoundParams, G1Point, G2Point, IRailgunSmartWallet, Preimage, SnarkProof, TokenData,
    Transaction,
};
use zecswap_chain::evm::{PrivateKeySigner, Settlement, U256, deploy};
use zecswap_core::Domain;

use super::*;
use crate::Config;

/// A `transact` call of one Railgun transaction in the form wallets prove it, spending `note`;
/// its proof and notes are placeholders, as decoding checks a call's form and not its proof.
fn transact(chain_id: u64, unshield: u8, note: u8) -> Vec<u8> {
    let point = || G1Point {
        x: U256::ZERO,
        y: U256::ZERO,
    };
    IRailgunSmartWallet::transactCall {
        _transactions: vec![Transaction {
            proof: SnarkProof {
                a: point(),
                b: G2Point {
                    x: [U256::ZERO; 2],
                    y: [U256::ZERO; 2],
                },
                c: point(),
            },
            merkleRoot: B256::ZERO,
            nullifiers: vec![B256::repeat_byte(note)],
            commitments: vec![B256::repeat_byte(9)],
            boundParams: BoundParams {
                treeNumber: 0,
                minGasPrice: U72::ZERO,
                unshield,
                chainID: chain_id,
                adaptContract: Address::ZERO,
                adaptParams: B256::ZERO,
                commitmentCiphertext: vec![],
            },
            unshieldPreimage: Preimage {
                npk: B256::ZERO,
                token: TokenData {
                    tokenType: 0,
                    tokenAddress: Address::ZERO,
                    tokenSubID: U256::ZERO,
                },
                value: U120::ZERO,
            },
        }],
    }
    .abi_encode()
}

#[tokio::test]
async fn each_mined_send_is_costed_from_its_receipt_and_its_own_bytes() {
    let Ok(anvil) = alloy::node_bindings::Anvil::new().try_spawn() else {
        eprintln!("skipped: anvil is not installed");
        return;
    };
    let artifact =
        Path::new(env!("CARGO_MANIFEST_DIR")).join("../../contracts/out/ZecSwap.sol/ZecSwap.json");
    let Ok(artifact) = std::fs::read(artifact) else {
        eprintln!("skipped: run `forge build` in contracts/");
        return;
    };
    let artifact: serde_json::Value = serde_json::from_slice(&artifact).unwrap();
    let url = anvil.endpoint();
    let key = PrivateKeySigner::from(anvil.keys()[0].clone());
    // Railgun stands at an address with no code, which takes any call, until it is given code
    // that refuses every call.
    let railgun = Address::repeat_byte(0x42);
    let mut code =
        alloy_primitives::hex::decode(artifact["bytecode"]["object"].as_str().unwrap()).unwrap();
    code.extend_from_slice(&U256::from(600).to_be_bytes::<32>());
    code.extend_from_slice(&railgun.into_word().0);
    let contract = deploy(&url, key.clone(), code).await.unwrap();
    let settlement = Settlement::connect(&url, contract, key.clone()).unwrap();
    let chain_id = settlement.chain_id().await.unwrap();
    let dir = tempfile::tempdir().unwrap();
    let policy = SendPolicy {
        railgun,
        token: Address::repeat_byte(3),
        fee: 250_000,
        max_gas_limit: 3_000_000,
        max_gas_price_wei: 50_000_000_000,
    };
    let sends = Sends::open(
        policy,
        Keys::from_seed(&[7; 64], 0),
        None,
        &dir.path().join("railgun-sends.sqlite"),
    )
    .unwrap();

    let chain = ProviderBuilder::new().connect_http(url.parse().unwrap());
    let wallet = EthereumWallet::from(key.clone());
    let gas_price = chain.get_gas_price().await.unwrap();
    let first = chain.get_transaction_count(key.address()).await.unwrap();
    // Signed and journaled as `railgun_transact` records a send, then broadcast or not.
    let send = async |nonce: u64, unshield: u8, note: u8, broadcast: bool| {
        let data = transact(chain_id, unshield, note);
        let envelope = TransactionRequest::default()
            .with_to(railgun)
            .with_input(data.clone())
            .with_nonce(nonce)
            .with_chain_id(chain_id)
            .with_gas_limit(200_000)
            .with_gas_price(gas_price)
            .build(&wallet)
            .await
            .unwrap();
        let signed = Signed {
            hash: *envelope.tx_hash(),
            nonce,
            raw: envelope.encoded_2718().into(),
        };
        sends
            .journal
            .record(
                chain_id,
                keccak256(&data),
                &[(0, B256::repeat_byte(note))],
                &signed,
            )
            .unwrap();
        if broadcast {
            chain
                .send_raw_transaction(&signed.raw)
                .await
                .unwrap()
                .get_receipt()
                .await
                .unwrap();
        }
        signed.hash
    };
    let private = send(first, 0, 1, true).await;
    chain
        .raw_request::<_, ()>("anvil_setCode".into(), (railgun, "0x60006000fd"))
        .await
        .unwrap();
    let reverted = send(first + 1, 1, 2, true).await;
    let lost = send(first + 2, 0, 3, false).await;

    let relayer = Relayer {
        config: Config {
            evm_rpc: url.clone(),
            contract,
            token: Address::repeat_byte(3),
            maker: Address::repeat_byte(4),
            listen: "127.0.0.1:0".parse().unwrap(),
            fee: 1,
            claim_margin: 30,
            reverse_funding: None,
            railgun_sends: None,
        },
        account: key.address(),
        domain: Domain {
            chain_id,
            contract: contract.into(),
        },
        settlement,
        monitor: crate::monitor::Monitor::new(MonitorToken::default()),
        sends: Some(sends),
        history: None,
    };
    relayer.cost_pass().await.unwrap();
    // Read once: a second pass reads nothing new.
    relayer.cost_pass().await.unwrap();
    let ledger = serde_json::to_value(relayer.sends_snapshot(Some(0)).unwrap().unwrap()).unwrap();
    let records = ledger["sends"].as_array().unwrap();
    assert_eq!(records.len(), 3);
    let record = |hash: B256| {
        records
            .iter()
            .find(|record| record["transactionHash"] == serde_json::json!(hash))
            .unwrap()
    };
    let number = |value: &serde_json::Value| {
        u128::from_str_radix(value.as_str().unwrap().trim_start_matches("0x"), 16).unwrap()
    };
    for (hash, kind, succeeded) in [(private, "send", true), (reverted, "unshield", false)] {
        let receipt = chain
            .raw_request::<_, serde_json::Value>("eth_getTransactionReceipt".into(), (hash,))
            .await
            .unwrap();
        let record = record(hash);
        assert_eq!(
            (record["kind"].as_str(), record["succeeded"].as_bool()),
            (Some(kind), Some(succeeded))
        );
        // Its notes open to nothing of this relayer's: it paid no fee.
        assert_eq!(record["fee"], "0");
        assert_eq!(
            record["gasUsed"].as_u64().map(u128::from),
            Some(number(&receipt["gasUsed"]))
        );
        assert_eq!(
            record["gasPriceWei"],
            number(&receipt["effectiveGasPrice"]).to_string()
        );
        assert!(record["blockTime"].as_u64().is_some());
        assert!(record["ethUsd"].is_null());
    }
    assert!(
        number(
            &chain
                .raw_request::<_, serde_json::Value>(
                    "eth_getTransactionReceipt".into(),
                    (reverted,)
                )
                .await
                .unwrap()["gasUsed"]
        ) > 21_000
    );
    let unread = record(lost);
    assert!(
        unread["succeeded"].is_null() && unread["gasUsed"].is_null() && unread["kind"].is_null()
    );
    assert_eq!(ledger["fee"], "250000");
}

/// A send's fee covers its gas at the rate the relayer quoted: a rate stays good for ten minutes
/// after it was last quoted, so a proof made with it is taken though the price rose since; past
/// that the current rate holds; and with gas unpriceable and nothing quoted, nothing is sent.
#[tokio::test]
async fn a_quoted_rate_is_honored_for_ten_minutes_and_gas_is_never_priced_blind() {
    use axum::http::StatusCode;
    use zecswap_prices::stand_in;

    let pricing = |url: &str, providers: &[zecswap_prices::Provider]| GasPricing {
        feed: Feed::new(
            providers,
            &stand_in::keys(),
            &[Asset::Eth, Asset::Usdc],
            &[],
            60,
            300,
        )
        .unwrap()
        .served_by(url),
        margin_bps: 1_000,
        quoted: Mutex::default(),
    };
    let up = stand_in::start(StatusCode::OK).await;
    let priced = pricing(&up.url, &[zecswap_prices::Provider::CoinMarketCap]);
    let now = now();
    // ETH at $2,400 over USDC at $0.99, and ten percent: 2,666.67 USDC per ETH of gas.
    let current = 2_666_666_666;
    assert_eq!(priced.quote(now).await, Some(current));
    let lower = 2_000_000_000;
    priced.quoted.lock().unwrap().push_front((now - 540, lower));
    assert_eq!(priced.honored(now).await, Some(lower));
    priced.quoted.lock().unwrap().front_mut().unwrap().0 = now - 660;
    assert_eq!(priced.honored(now).await, Some(current));

    let down = stand_in::start(StatusCode::SERVICE_UNAVAILABLE).await;
    let blind = pricing(&down.url, &[zecswap_prices::Provider::CoinMarketCap]);
    assert_eq!(blind.honored(now).await, None);
    blind.quoted.lock().unwrap().push_back((now - 120, current));
    assert_eq!(blind.honored(now).await, Some(current));

    // 1,000,000 gas at 2 gwei is 0.002 ETH: 5,333,333 base units at that rate.
    let (gas, price) = (1_000_000, 2_000_000_000);
    assert!(covers(5_333_334, Some(current), gas, price));
    assert!(!covers(5_333_332, Some(current), gas, price));
    assert!(covers(1, None, gas, price));
}
