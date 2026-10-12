use alloy::eips::Encodable2718;
use alloy::network::{EthereumWallet, TransactionBuilder};
use alloy::node_bindings::Anvil;
use alloy::rpc::types::TransactionRequest;
use alloy_primitives::{Bytes, U256};
use zecswap_chain::evm::railgun::Signed;

use super::*;
use crate::lanes::{Lanes, derive};

const CONTRACT: Address = Address::repeat_byte(1);

fn relayer_on(url: &str, key: &PrivateKeySigner) -> Relayer {
    Relayer {
        config: Config {
            network: zecswap_api::Network::Testnet,
            evm_rpc: url.into(),
            contract: CONTRACT,
            token: Address::repeat_byte(3),
            maker: Address::repeat_byte(4),
            listen: "127.0.0.1:0".parse().unwrap(),
            fee: 1,
            fee_gas: 0,
            providers: vec![],
            fee_margin_bps: 0,
            claim_margin: 30,
            top_up_transactions: 8,
            reverse_funding: None,
            railgun_sends: Some(RailgunSendsConfig {
                fee: 1,
                max_gas_limit: 3_000_000,
                max_gas_price_wei: 50_000_000_000,
                journal: "unused.sqlite".into(),
            }),
        },
        account: key.address(),
        domain: Domain {
            chain_id: 1,
            contract: CONTRACT.into(),
        },
        settlement: Settlement::connect(url, CONTRACT, key.clone()).unwrap(),
        lanes: Lanes::connect(url, CONTRACT, key).unwrap(),
        lock_duration: 600,
        in_flight: Default::default(),
        monitor: crate::monitor::Monitor::new(MonitorToken::default()),
        sends: None,
        funding: None,
        pricing: None,
        history: None,
    }
}

#[test]
fn each_lane_has_its_own_account_derived_from_the_relayers_key() {
    let key = PrivateKeySigner::random();
    let accounts = |key: &PrivateKeySigner| {
        let lanes = Lanes::connect("http://127.0.0.1:1", CONTRACT, key).unwrap();
        [lanes.claims, lanes.locks, lanes.sends, lanes.funding].map(|lane| lane.account().unwrap())
    };
    let lanes = accounts(&key);
    assert_eq!(lanes, accounts(&key));
    let mut all = lanes.to_vec();
    all.push(key.address());
    all.sort();
    all.dedup();
    assert_eq!(all.len(), 5);
    assert!(
        accounts(&PrivateKeySigner::random())
            .iter()
            .all(|account| !lanes.contains(account))
    );
}

#[tokio::test]
async fn a_recorded_send_is_followed_up_on_the_account_that_signed_it() {
    let key = PrivateKeySigner::random();
    let relayer = relayer_on("http://127.0.0.1:1", &key);
    let signed = async |signer: PrivateKeySigner| {
        let envelope = TransactionRequest::default()
            .with_to(Address::repeat_byte(0x42))
            .with_nonce(0)
            .with_chain_id(1)
            .with_gas_limit(21_000)
            .with_gas_price(1)
            .build(&EthereumWallet::from(signer))
            .await
            .unwrap();
        Signed {
            hash: *envelope.tx_hash(),
            nonce: 0,
            raw: envelope.encoded_2718().into(),
        }
    };
    let sends = derive(&key, "sends").unwrap();
    let lane = signed(sends.clone()).await;
    let before = signed(key.clone()).await;
    let unreadable = Signed {
        raw: Bytes::from_static(b"not a transaction"),
        ..before.clone()
    };
    assert_eq!(relayer.sender_of(&lane).account(), Some(sends.address()));
    for signed in [before, unreadable] {
        assert_eq!(relayer.sender_of(&signed).account(), Some(key.address()));
    }
}

#[tokio::test]
async fn lanes_are_kept_in_eth_from_the_relayers_own_account() {
    let Ok(anvil) = Anvil::new().try_spawn() else {
        eprintln!("skipped: anvil is not installed");
        return;
    };
    let url = anvil.endpoint();
    let relayer = relayer_on(&url, &PrivateKeySigner::from(anvil.keys()[0].clone()));
    let balance = async |lane: &Settlement| {
        let account = lane.account().unwrap();
        relayer.settlement.eth_balance(account).await.unwrap()
    };

    // Each send holds its gas at the fee cap it is filled with, which on Sepolia runs to hundreds
    // of times the gas price.
    let fee = relayer.settlement.max_fee_per_gas().await.unwrap();
    let sent = relayer.top_up().await.unwrap();
    let lanes = sent.iter().map(|(lane, _)| *lane).collect::<Vec<_>>();
    assert_eq!(lanes, ["claims", "locks", "sends"]);
    assert_eq!(
        sent[0].1,
        U256::from(fee) * U256::from(crate::lanes::SETTLEMENT_GAS) * U256::from(8)
    );
    assert_eq!(sent[2].1, sent[0].1 * U256::from(3));
    assert_eq!(balance(&relayer.lanes.claims).await, sent[0].1);
    assert_eq!(balance(&relayer.lanes.funding).await, U256::ZERO);
    assert!(relayer.top_up().await.unwrap().is_empty());

    let spent = balance(&relayer.lanes.claims).await * U256::from(3) / U256::from(4);
    relayer
        .lanes
        .claims
        .send_eth(Address::repeat_byte(9), spent)
        .await
        .unwrap();
    let sent = relayer.top_up().await.unwrap();
    assert_eq!(sent.len(), 1);
    assert_eq!(sent[0].0, "claims");

    let poor = relayer_on(&url, &PrivateKeySigner::random());
    assert!(poor.top_up().await.unwrap().is_empty());
}
