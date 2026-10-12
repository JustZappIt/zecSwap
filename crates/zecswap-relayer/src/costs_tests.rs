//! Broadcaster sends on anvil, journaled as `railgun_transact` records them: what each one cost
//! and earned is read once it has mined, a reverted one included, with one Telegram message each,
//! and the notes a reverted one spent go again only after a cooldown.

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

/// Railgun stands at an address with no code, which takes any call, until a test gives it code.
const RAILGUN: Address = Address::repeat_byte(0x42);
const TOKEN: Address = Address::repeat_byte(3);

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
                    tokenAddress: TOKEN,
                    tokenSubID: U256::ZERO,
                },
                value: U120::ZERO,
            },
        }],
    }
    .abi_encode()
}

/// A relayer whose Railgun sends charge `fee`, with its journal in `dir`, and alerts on: they are
/// queued there and never delivered.
fn relayer(
    url: &str,
    contract: Address,
    key: &PrivateKeySigner,
    chain_id: u64,
    fee: u128,
    dir: &Path,
) -> Relayer {
    let policy = SendPolicy {
        railgun: RAILGUN,
        token: TOKEN,
        fee,
        max_gas_limit: 3_000_000,
        max_gas_price_wei: 50_000_000_000,
    };
    Relayer {
        config: Config {
            network: zecswap_api::Network::Testnet,
            evm_rpc: url.into(),
            contract,
            token: TOKEN,
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
        },
        account: key.address(),
        domain: Domain {
            chain_id,
            contract: contract.into(),
        },
        settlement: Settlement::connect(url, contract, key.clone()).unwrap(),
        lanes: crate::lanes::Lanes::connect(url, contract, key).unwrap(),
        lock_duration: 600,
        in_flight: Default::default(),
        monitor: crate::monitor::Monitor::new(MonitorToken::default()),
        sends: Some(
            Sends::open(
                policy,
                Keys::from_seed(&[7; 64], 0),
                &dir.join("railgun-sends.sqlite"),
                Telegram::new(Some("1:test"), Some("1")).unwrap(),
            )
            .unwrap(),
        ),
        funding: None,
        pricing: None,
        history: None,
    }
}

/// Signs `data` to Railgun from `key`'s account, journals it as spending `note` the way
/// `railgun_transact` records a send, and broadcasts it if `broadcast`.
async fn journaled(
    relayer: &Relayer,
    chain: &impl Provider,
    key: &PrivateKeySigner,
    data: Vec<u8>,
    note: u8,
    broadcast: bool,
) -> B256 {
    let chain_id = relayer.domain.chain_id;
    let nonce = chain
        .get_transaction_count(key.address())
        .pending()
        .await
        .unwrap();
    let envelope = TransactionRequest::default()
        .with_to(RAILGUN)
        .with_input(data.clone())
        .with_nonce(nonce)
        .with_chain_id(chain_id)
        .with_gas_limit(200_000)
        .with_gas_price(chain.get_gas_price().await.unwrap())
        .build(&EthereumWallet::from(key.clone()))
        .await
        .unwrap();
    let signed = Signed {
        hash: *envelope.tx_hash(),
        nonce,
        raw: envelope.encoded_2718().into(),
    };
    let sends = relayer.sends.as_ref().unwrap();
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
}

/// Gives Railgun's address `code`.
async fn railgun_code(chain: &impl Provider, code: &str) {
    chain
        .raw_request::<_, ()>("anvil_setCode".into(), (RAILGUN, code))
        .await
        .unwrap();
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
    let mut code =
        alloy_primitives::hex::decode(artifact["bytecode"]["object"].as_str().unwrap()).unwrap();
    code.extend_from_slice(&U256::from(600).to_be_bytes::<32>());
    code.extend_from_slice(&RAILGUN.into_word().0);
    let contract = deploy(&url, key.clone(), code).await.unwrap();
    let chain = ProviderBuilder::new().connect_http(url.parse().unwrap());
    let chain_id = chain.get_chain_id().await.unwrap();
    let dir = tempfile::tempdir().unwrap();
    let relayer = relayer(&url, contract, &key, chain_id, 250_000, dir.path());

    let private = journaled(&relayer, &chain, &key, transact(chain_id, 0, 1), 1, true).await;
    railgun_code(&chain, "0x60006000fd").await;
    let reverted = journaled(&relayer, &chain, &key, transact(chain_id, 1, 2), 2, true).await;
    let lost = journaled(&relayer, &chain, &key, transact(chain_id, 0, 3), 3, false).await;

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

    // One message for each mined send, however often its receipt is read; none for the lost one.
    let alerts: Vec<String> = relayer
        .sends
        .as_ref()
        .unwrap()
        .journal
        .0
        .lock()
        .unwrap()
        .prepare("SELECT text FROM notifications")
        .unwrap()
        .query_map([], |row| row.get(0))
        .unwrap()
        .collect::<rusqlite::Result<_>>()
        .unwrap();
    assert_eq!(alerts.len(), 2);
    for (hash, title) in [(private, "private send"), (reverted, "unshield reverted")] {
        let alert = alerts
            .iter()
            .find(|alert| alert.ends_with(&hash.to_string()))
            .unwrap();
        assert!(alert.starts_with(&format!("ZecSwap testnet · {title}\n")));
    }
}

#[test]
fn a_mined_send_says_what_it_moved_and_earned_and_names_no_one() {
    let hash = B256::repeat_byte(5);
    let text = |paid: Option<Paid>, succeeded| {
        let network = zecswap_api::Network::Testnet;
        send_alert(
            network,
            11155111,
            hash,
            paid.as_ref(),
            succeeded,
            1_791_720_221,
        )
        .text
    };
    let paid = |unshielded| {
        Some(Paid {
            unshielded,
            fee: 420_000,
        })
    };
    let end = format!("11 Oct 12:03 UTC\nhttps://sepolia.etherscan.io/tx/{hash}");
    assert_eq!(
        text(paid(Some(12_500_000)), true),
        format!("ZecSwap testnet · unshield\n12.5 USDC out of Railgun · fee 0.42 USDC\n{end}")
    );
    assert_eq!(
        text(paid(None), true),
        format!("ZecSwap testnet · private send\nAmount private · fee 0.42 USDC\n{end}")
    );
    assert_eq!(
        text(paid(Some(12_500_000)), false),
        format!(
            "ZecSwap testnet · unshield reverted\n\
             Nothing moved; the relayer paid the gas and earned no fee.\n{end}"
        )
    );
    // Bytes that no longer pass the policy say only that it was a Railgun send.
    assert_eq!(
        text(None, true),
        format!("ZecSwap testnet · Railgun send\n{end}")
    );
    let mainnet = send_alert(zecswap_api::Network::Mainnet, 1, hash, None, true, 0);
    assert_eq!(mainnet.key, format!("mainnet:1:railgun:{hash}"));
    assert!(
        mainnet
            .text
            .contains(&format!("https://etherscan.io/tx/{hash}"))
    );
}

#[tokio::test]
async fn the_notes_of_a_send_that_reverted_go_again_only_after_the_cooldown() {
    let Ok(anvil) = alloy::node_bindings::Anvil::new().try_spawn() else {
        eprintln!("skipped: anvil is not installed");
        return;
    };
    let url = anvil.endpoint();
    let key = PrivateKeySigner::from(anvil.keys()[0].clone());
    let chain = ProviderBuilder::new().connect_http(url.parse().unwrap());
    let chain_id = chain.get_chain_id().await.unwrap();
    let dir = tempfile::tempdir().unwrap();
    let relayer = relayer(&url, Address::repeat_byte(1), &key, chain_id, 0, dir.path());
    let lane = relayer.lanes.sends.account().unwrap();
    chain
        .raw_request::<_, ()>(
            "anvil_setBalance".into(),
            (lane, U256::from(10u128.pow(18))),
        )
        .await
        .unwrap();

    // A withdrawal of note 5 passed the simulation and reverted on chain.
    railgun_code(&chain, "0x60006000fd").await;
    journaled(&relayer, &chain, &key, transact(chain_id, 1, 5), 5, true).await;
    // Railgun now takes every call and finds every note unspent.
    railgun_code(&chain, "0x60206000f3").await;
    let send = RailgunTransact {
        chain_id,
        to: RAILGUN,
        data: transact(chain_id, 0, 5).into(),
        value: 0,
    };
    assert!(matches!(
        relayer.railgun_transact(send.clone()).await,
        Err(RelayerError::Reverted(left))
            if (REVERTED_COOLDOWN - 60..=REVERTED_COOLDOWN).contains(&left)
    ));

    // An hour on.
    relayer
        .sends
        .as_ref()
        .unwrap()
        .journal
        .0
        .lock()
        .unwrap()
        .execute(
            "UPDATE sends SET sent_at = sent_at - ?1",
            [REVERTED_COOLDOWN as i64],
        )
        .unwrap();
    let Ok(Sending::New(sent)) = relayer.railgun_transact(send).await else {
        panic!("the notes were not sent once the cooldown was over");
    };
    let receipt = chain
        .get_transaction_receipt(sent.transactions[0])
        .await
        .unwrap()
        .unwrap();
    assert!(receipt.status());
}
