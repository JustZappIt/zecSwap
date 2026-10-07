use super::super::{PrivateKeySigner, deploy, reverse_funding_calls};
use super::*;
use alloy::node_bindings::Anvil;
use std::path::Path;
use zecswap_core::{NetworkType, derive_maker_share, derive_user_keys};

type Mutation = Box<dyn Fn(&mut IRelayAdapt::relayCall)>;

fn fixture() -> (FundingPolicy, Domain, B256, IRelayAdapt::relayCall) {
    let json: serde_json::Value = serde_json::from_str(include_str!("sdk-vector.json")).unwrap();
    let address = |key: &str| json[key].as_str().unwrap().parse::<Address>().unwrap();
    let policy = FundingPolicy {
        relay_adapt: address("to"),
        token: address("token"),
        maker: address("maker"),
        max_gas_limit: 4_000_000,
        max_gas_price_wei: 20_000_000_000,
        fee: json["fee"].as_str().unwrap().parse().unwrap(),
        fee_recipient: address("relayer"),
    };
    let domain = Domain {
        chain_id: SEPOLIA_CHAIN_ID,
        contract: address("contract").into(),
    };
    let data: Bytes = json["data"].as_str().unwrap().parse().unwrap();
    let id = json["swapId"].as_str().unwrap().parse().unwrap();
    (
        policy,
        domain,
        id,
        IRelayAdapt::relayCall::abi_decode_validate(&data).unwrap(),
    )
}

fn validate(
    p: &FundingPolicy,
    d: Domain,
    id: B256,
    relay: &IRelayAdapt::relayCall,
) -> Result<ValidatedFunding> {
    p.validate(d, id, p.relay_adapt, 0, relay.abi_encode().into())
}

fn bind(relay: &mut IRelayAdapt::relayCall) {
    let nullifiers: Vec<Vec<B256>> = relay
        ._transactions
        .iter()
        .map(|t| t.nullifiers.clone())
        .collect();
    let hash = keccak256(
        (
            nullifiers,
            U256::from(relay._transactions.len()),
            relay._actionData.clone(),
        )
            .abi_encode_params(),
    );
    for tx in &mut relay._transactions {
        tx.boundParams.adaptParams = hash;
    }
}

#[test]
fn accepts_independently_encoded_sdk_vector() {
    let (p, d, id, relay) = fixture();
    let validated = validate(&p, d, id, &relay).unwrap();
    assert_eq!(validated.terms.amount, 1_000_000);
    assert_eq!(validated.min_gas_price, 1);
}

#[test]
fn rejects_wrong_envelope_and_unsafe_configuration() {
    let (p, d, id, relay) = fixture();
    let data: Bytes = relay.abi_encode().into();
    assert!(p.validate(d, id, p.token, 0, data.clone()).is_err());
    assert!(p.validate(d, id, p.relay_adapt, 1, data.clone()).is_err());
    assert!(
        p.validate(
            Domain { chain_id: 1, ..d },
            id,
            p.relay_adapt,
            0,
            data.clone()
        )
        .is_err()
    );
    assert!(
        p.validate(d, B256::ZERO, p.relay_adapt, 0, data.clone())
            .is_err()
    );
    assert!(
        p.validate(
            d,
            id,
            p.relay_adapt,
            0,
            data[..data.len() - 1].to_vec().into()
        )
        .is_err()
    );
    let mut trailing = data.to_vec();
    trailing.push(0);
    assert!(
        p.validate(d, id, p.relay_adapt, 0, trailing.into())
            .is_err()
    );
    assert!(
        p.validate(
            d,
            id,
            p.relay_adapt,
            0,
            vec![0; MAX_CALLDATA_BYTES + 1].into()
        )
        .is_err()
    );
    assert!(p.validate_config(1, Address::repeat_byte(9)).is_err());
    assert!(p.validate_config(SEPOLIA_CHAIN_ID, p.maker).is_err());
    assert!(
        p.validate_config(SEPOLIA_CHAIN_ID, Address::repeat_byte(9))
            .is_err()
    );
    assert!(p.validate_config(SEPOLIA_CHAIN_ID, p.fee_recipient).is_ok());
    let mut empty = p.clone();
    empty.max_gas_price_wei = 0;
    assert!(
        empty
            .validate_config(SEPOLIA_CHAIN_ID, p.fee_recipient)
            .is_err()
    );
}

#[test]
fn rejects_tampered_actions_even_when_rebound() {
    let (p, d, id, relay) = fixture();
    let mutations: Vec<Mutation> = vec![
        Box::new(|r| r._actionData.requireSuccess = false),
        Box::new(|r| {
            r._actionData.calls.remove(2);
        }),
        Box::new(|r| {
            r._actionData.calls.push(r._actionData.calls[0].clone());
        }),
        Box::new(|r| r._actionData.calls.swap(0, 1)),
        Box::new(|r| r._actionData.calls[1].to = Address::repeat_byte(9)),
        Box::new(|r| r._actionData.calls[0].value = U256::from(1)),
        Box::new(|r| r._actionData.minGasLimit = U256::from(4_000_001)),
        Box::new(|r| {
            let c = &mut r._actionData.calls[0];
            let mut a = IErc20::approveCall::abi_decode(&c.data).unwrap();
            a.amount = U256::MAX;
            c.data = a.abi_encode().into();
        }),
        Box::new(|r| {
            let c = &mut r._actionData.calls[1];
            let mut o = IZecSwap::openReverseCall::abi_decode(&c.data).unwrap();
            o.terms.refundNote = B256::repeat_byte(8);
            c.data = o.abi_encode().into();
        }),
        Box::new(|r| {
            let c = &mut r._actionData.calls[1];
            let mut o = IZecSwap::openReverseCall::abi_decode(&c.data).unwrap();
            o.signature = vec![0; 65].into();
            c.data = o.abi_encode().into();
        }),
        Box::new(|r| {
            r._actionData.calls.remove(2);
        }),
        Box::new(|r| r._actionData.calls.swap(2, 3)),
        Box::new(|r| r._actionData.calls[2].to = Address::repeat_byte(9)),
        Box::new(|r| {
            let c = &mut r._actionData.calls[2];
            let mut t = IErc20::transferCall::abi_decode(&c.data).unwrap();
            t.to = Address::repeat_byte(9);
            c.data = t.abi_encode().into();
        }),
        Box::new(|r| {
            let c = &mut r._actionData.calls[2];
            let mut t = IErc20::transferCall::abi_decode(&c.data).unwrap();
            t.amount -= U256::from(1);
            c.data = t.abi_encode().into();
        }),
        Box::new(|r| r._actionData.calls[2].data = r._actionData.calls[0].data.clone()),
        Box::new(|r| {
            let c = &mut r._actionData.calls[3];
            let mut s = IRelayAdapt::shieldCall::abi_decode(&c.data).unwrap();
            s._shieldRequests[0].preimage.value = alloy::primitives::aliases::U120::from(1);
            c.data = s.abi_encode().into();
        }),
        Box::new(|r| {
            let c = &mut r._actionData.calls[3];
            let mut s = IRelayAdapt::shieldCall::abi_decode(&c.data).unwrap();
            s._shieldRequests[0].preimage.token.tokenAddress = Address::ZERO;
            c.data = s.abi_encode().into();
        }),
    ];
    for (index, mutate) in mutations.iter().enumerate() {
        let mut bad = relay.clone();
        mutate(&mut bad);
        bind(&mut bad);
        assert!(validate(&p, d, id, &bad).is_err(), "mutation {index}");
    }
}

#[test]
fn rejects_unbound_proofs_and_wrong_unshields() {
    let (p, d, id, relay) = fixture();
    let mutations: Vec<Mutation> = vec![
        Box::new(|r| r._transactions.clear()),
        Box::new(|r| r._transactions[0].nullifiers.clear()),
        Box::new(|r| r._transactions[0].boundParams.chainID = 1),
        Box::new(|r| r._transactions[0].boundParams.adaptContract = Address::ZERO),
        Box::new(|r| r._transactions[0].boundParams.adaptParams = B256::ZERO),
        Box::new(|r| r._transactions[0].boundParams.unshield = 0),
        Box::new(|r| r._transactions[0].boundParams.unshield = 2),
        Box::new(|r| {
            r._transactions[0].boundParams.minGasPrice =
                alloy::primitives::aliases::U72::from(20_000_000_001u128)
        }),
        Box::new(|r| r._transactions[0].unshieldPreimage.npk = B256::ZERO),
        Box::new(|r| r._transactions[0].unshieldPreimage.token.tokenType = 1),
        Box::new(|r| r._transactions[0].unshieldPreimage.token.tokenAddress = Address::ZERO),
    ];
    for (index, mutate) in mutations.iter().enumerate() {
        let mut bad = relay.clone();
        mutate(&mut bad);
        assert!(validate(&p, d, id, &bad).is_err(), "mutation {index}");
    }
}

fn creation(artifacts: &Path, name: &str) -> Vec<u8> {
    let json: serde_json::Value = serde_json::from_slice(
        &std::fs::read(artifacts.join(format!("{name}.sol/{name}.json"))).unwrap(),
    )
    .unwrap();
    hex::decode(
        json["bytecode"]["object"]
            .as_str()
            .unwrap()
            .trim_start_matches("0x"),
    )
    .unwrap()
}

#[tokio::test]
async fn sponsors_atomic_escrow_and_reconciles_retries_on_anvil() {
    let Ok(anvil) = Anvil::new().chain_id(SEPOLIA_CHAIN_ID).try_spawn() else {
        eprintln!("skipped: anvil is not installed");
        return;
    };
    let artifacts = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../contracts/out");
    if !artifacts.join("MockRelayAdapt.sol").exists() {
        eprintln!("skipped: run forge build in contracts/");
        return;
    }
    let url = anvil.endpoint();
    let key = PrivateKeySigner::from(anvil.keys()[0].clone());
    let sponsor = key.address();
    let railgun = deploy(&url, key.clone(), creation(&artifacts, "MockRailgun"))
        .await
        .unwrap();
    let token = deploy(&url, key.clone(), creation(&artifacts, "TestToken"))
        .await
        .unwrap();
    let mut code = creation(&artifacts, "ZecSwap");
    code.extend_from_slice(&U256::from(600).to_be_bytes::<32>());
    code.extend_from_slice(railgun.into_word().as_slice());
    let contract = deploy(&url, key.clone(), code).await.unwrap();
    let mut code = creation(&artifacts, "MockRelayAdapt");
    code.extend_from_slice(railgun.into_word().as_slice());
    let adapter = deploy(&url, key.clone(), code).await.unwrap();
    let chain = Settlement::connect(&url, contract, key).unwrap();
    chain.check_funding_adapter(adapter).await.unwrap();
    assert!(chain.check_funding_adapter(token).await.is_err());
    let (mut p, _, _, mut relay) = fixture();
    p.token = token;
    p.relay_adapt = adapter;
    p.fee_recipient = sponsor;
    p.validate_config(SEPOLIA_CHAIN_ID, sponsor).unwrap();
    let d = Domain {
        chain_id: SEPOLIA_CHAIN_ID,
        contract: contract.into(),
    };
    let keys = derive_user_keys(&[7; 64], NetworkType::Test, 0, 0).unwrap();
    let now = chain.now().await.unwrap();
    let terms = ReverseOpen {
        maker: p.maker.into(),
        user: keys.auth.address(),
        token: token.into(),
        amount: 1_000_000,
        maker_share: derive_maker_share(&[9; 32], 0).unwrap().public(),
        user_share: keys.share.public(),
        t0: now + 3600,
        t1: now + 7200,
        deadline: now + 600,
        refund_note: [6; 32],
    };
    let id = swap_id(terms.user.into(), &terms.maker_share);
    let calls = reverse_funding_calls(contract, &terms, &keys.auth.sign(&d.open_reverse(&terms)));
    for (i, (to, data)) in calls.into_iter().enumerate() {
        relay._actionData.calls[i].to = to;
        relay._actionData.calls[i].data = data.into();
    }
    relay._actionData.calls[2].to = token;
    relay._actionData.calls[2].data = IErc20::transferCall {
        to: sponsor,
        amount: U256::from(p.fee),
    }
    .abi_encode()
    .into();
    relay._actionData.calls[3].to = adapter;
    let mut dust = IRelayAdapt::shieldCall::abi_decode(&relay._actionData.calls[3].data).unwrap();
    dust._shieldRequests[0].preimage.token.tokenAddress = token;
    relay._actionData.calls[3].data = dust.abi_encode().into();
    relay._transactions[0].boundParams.adaptContract = adapter;
    relay._transactions[0].unshieldPreimage.npk = adapter.into_word();
    relay._transactions[0].unshieldPreimage.token.tokenAddress = token;
    bind(&mut relay);
    let request = validate(&p, d, id, &relay).unwrap();
    let nonce = chain.provider.get_transaction_count(sponsor).await.unwrap();
    let mut expired = relay.clone();
    let expired_terms = ReverseOpen {
        deadline: now - 1,
        ..terms.clone()
    };
    expired._actionData.calls[1].data = reverse_funding_calls(
        contract,
        &expired_terms,
        &keys.auth.sign(&d.open_reverse(&expired_terms)),
    )[1]
    .1
    .clone()
    .into();
    bind(&mut expired);
    let expired = validate(&p, d, id, &expired).unwrap();
    assert!(matches!(
        chain.sponsor_reverse_funding(&p, &expired).await,
        Err(FundingError::Rejected("funding deadline passed"))
    ));
    let mut invalid = relay.clone();
    invalid._transactions[0].proof.a.x = U256::ZERO;
    let invalid = validate(&p, d, id, &invalid).unwrap();
    assert!(chain.sponsor_reverse_funding(&p, &invalid).await.is_err());
    assert_eq!(
        chain.provider.get_transaction_count(sponsor).await.unwrap(),
        nonce
    );
    assert!(chain.swap(id, &terms.terms()).await.unwrap().is_none());
    // A call that fails after the mocked unshield still rolls everything back in simulation.
    let mut short = relay.clone();
    short._transactions[0].unshieldPreimage.value = alloy::primitives::aliases::U120::from(1);
    let short = validate(&p, d, id, &short).unwrap();
    assert!(chain.sponsor_reverse_funding(&p, &short).await.is_err());
    assert_eq!(chain.token_balance(token, adapter).await.unwrap(), 0);
    assert!(
        !IRelayAdapt::new(adapter, &chain.provider)
            .railgun()
            .call()
            .await
            .unwrap()
            .is_zero()
    );
    let mut expensive = p.clone();
    expensive.max_gas_price_wei = 1;
    let expensive_request = validate(&expensive, d, id, &relay).unwrap();
    assert!(
        chain
            .sponsor_reverse_funding(&expensive, &expensive_request)
            .await
            .is_err()
    );
    let mut limited = p.clone();
    limited.max_gas_limit = 21_000;
    let mut low_gas = relay.clone();
    low_gas._actionData.minGasLimit = U256::ZERO;
    bind(&mut low_gas);
    let low_gas = validate(&limited, d, id, &low_gas).unwrap();
    assert!(
        chain
            .sponsor_reverse_funding(&limited, &low_gas)
            .await
            .is_err()
    );
    assert_eq!(
        chain.provider.get_transaction_count(sponsor).await.unwrap(),
        nonce
    );
    assert_eq!(
        chain.eth_balance(terms.user.into()).await.unwrap(),
        U256::ZERO
    );
    let before = chain.eth_balance(sponsor).await.unwrap();
    let hash = chain
        .sponsor_reverse_funding(&p, &request)
        .await
        .unwrap()
        .unwrap();
    chain
        .provider
        .watch_pending_transaction(hash.into())
        .await
        .unwrap()
        .await
        .unwrap();
    let swap = chain.swap(id, &terms.terms()).await.unwrap().unwrap();
    assert_eq!(swap.maker, Address::from(terms.user));
    assert_eq!(swap.user, p.maker);
    assert_eq!(swap.amount, terms.amount);
    assert_eq!(
        chain.token_balance(token, contract).await.unwrap(),
        terms.amount
    );
    assert_eq!(chain.token_balance(token, sponsor).await.unwrap(), p.fee);
    assert!(chain.eth_balance(sponsor).await.unwrap() < before);
    assert_eq!(
        chain.sponsor_reverse_funding(&p, &request).await.unwrap(),
        None
    );
    // A restarted service reconciles from escrow rather than paying to submit again.
    let restarted = Settlement::connect(
        &url,
        contract,
        PrivateKeySigner::from(anvil.keys()[0].clone()),
    )
    .unwrap();
    assert_eq!(
        restarted
            .sponsor_reverse_funding(&p, &request)
            .await
            .unwrap(),
        None
    );
    assert_eq!(
        chain.provider.get_transaction_count(sponsor).await.unwrap(),
        nonce + 1
    );
}
