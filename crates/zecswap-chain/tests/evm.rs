//! The settlement client against a local anvil; skipped where Foundry isn't installed, or
//! the contracts aren't built (`forge build` in `contracts/`).

use std::path::Path;
use std::time::Duration;

use alloy::node_bindings::Anvil;
use zecswap_chain::Error;
use zecswap_chain::evm::{
    Address, PrivateKeySigner, Reveal, Settlement, Stage, U256, Window, Windows, deploy,
    reverse_swap_id, swap_id,
};
use zecswap_core::{Domain, NetworkType, Terms, derive_maker_share, derive_user_keys};

/// The windows the tests deploy the contract with: an open must leave `MIN_WINDOW` before `t0`
/// and from `t0` to `t1`, and the maker's refund lasts `LATE_WINDOW` from `t1`.
const MIN_WINDOW: u64 = 600;
const LATE_WINDOW: u64 = 1_200;

/// The contract's creation code with its constructor's arguments.
fn swap_code(artifacts: &Path, railgun: Address) -> Vec<u8> {
    let mut code = creation_code(artifacts, "ZecSwap");
    code.extend_from_slice(&U256::from(MIN_WINDOW).to_be_bytes::<32>());
    code.extend_from_slice(&U256::from(LATE_WINDOW).to_be_bytes::<32>());
    code.extend_from_slice(&railgun.into_word().0);
    code
}

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

/// A send tips what the node suggests, so it pays the gas price that quotes and the relayer's
/// fees charge, not the median of recent blocks' tips that alloy's own estimate tips.
#[tokio::test]
async fn a_send_pays_the_gas_price_quotes_charge() {
    use alloy::consensus::Transaction as _;
    use alloy::network::{EthereumWallet, TransactionBuilder};
    use alloy::providers::{Provider, ProviderBuilder};
    use alloy::rpc::types::TransactionRequest;

    let Ok(anvil) = Anvil::new().try_spawn() else {
        eprintln!("skipped: anvil is not installed");
        return;
    };
    let url = anvil.endpoint();
    let others = ProviderBuilder::new()
        .wallet(EthereumWallet::from(PrivateKeySigner::from(
            anvil.keys()[1].clone(),
        )))
        .connect_http(url.parse().unwrap());
    // Recent blocks tip far over the latest, as Sepolia's bots, tipping a flat gwei, often do.
    for gwei in [100, 100, 100, 2] {
        let tip = gwei * 1_000_000_000;
        let tx = TransactionRequest::default()
            .with_to(Address::repeat_byte(2))
            .with_max_priority_fee_per_gas(tip)
            .with_max_fee_per_gas(tip + 10_000_000_000);
        let pending = others.send_transaction(tx).await.unwrap();
        assert!(pending.get_receipt().await.unwrap().status());
    }
    // anvil suggests the latest block's least tip; alloy's estimate is their median, 100 gwei.
    let tip = others.get_max_priority_fee_per_gas().await.unwrap();
    assert_eq!(tip, 2_000_000_000);

    let key = PrivateKeySigner::from(anvil.keys()[0].clone());
    let chain = Settlement::connect(&url, Address::ZERO, key).unwrap();
    let price = chain.gas_price().await.unwrap();
    let cap = chain.max_fee_per_gas().await.unwrap();
    let sent = chain
        .send_eth(Address::repeat_byte(3), U256::from(1))
        .await
        .unwrap();
    let tx = others.get_transaction_by_hash(sent).await.unwrap().unwrap();
    assert_eq!(tx.max_priority_fee_per_gas(), Some(tip));
    assert_eq!(tx.max_fee_per_gas(), cap);
    let receipt = others.get_transaction_receipt(sent).await.unwrap().unwrap();
    assert_eq!(receipt.effective_gas_price, price);
}

/// A claim or refund carries its share, and one the base fee outruns may land after its window
/// closes, reverted with the share public. Each may pay its account's reveal headroom over the fee
/// cap the account's other sends, a ready among them, keep to.
#[tokio::test]
async fn a_reveal_may_pay_its_headroom_over_the_usual_fee_cap() {
    use alloy::consensus::Transaction as _;
    use alloy::primitives::B256;
    use alloy::providers::{Provider, ProviderBuilder};

    let Ok(anvil) = Anvil::new().try_spawn() else {
        eprintln!("skipped: anvil is not installed");
        return;
    };
    let artifacts = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../contracts/out");
    if !artifacts.join("ZecSwap.sol").exists() {
        eprintln!("skipped: run `forge build` in contracts/");
        return;
    }
    let url = anvil.endpoint();
    let maker_key = PrivateKeySigner::from(anvil.keys()[0].clone());
    let user_key = PrivateKeySigner::from(anvil.keys()[1].clone());
    let (maker_account, user_account) = (maker_key.address(), user_key.address());
    let token = deploy(
        &url,
        maker_key.clone(),
        creation_code(&artifacts, "TestToken"),
    )
    .await
    .unwrap();
    let code = swap_code(&artifacts, Address::ZERO);
    let contract = deploy(&url, maker_key.clone(), code).await.unwrap();

    let headroom = 5_000_000_000;
    let mut maker = Settlement::connect(&url, contract, maker_key).unwrap();
    maker.set_reveal_headroom(headroom);
    let mut user = Settlement::connect(&url, contract, user_key).unwrap();
    user.set_reveal_headroom(headroom);
    maker
        .mint_test_token(token, maker_account, 2)
        .await
        .unwrap();
    maker.add_inventory(token, 2).await.unwrap();
    let open = async |index: u32| {
        let user_share = derive_user_keys(&[7; 64], NetworkType::Test, 0, index)
            .unwrap()
            .share;
        let maker_share = derive_maker_share(&[9; 32], index.into()).unwrap();
        let now = maker.now().await.unwrap();
        let terms = Terms {
            maker: maker_account.into(),
            token: token.into(),
            amount: 1,
            maker_share: maker_share.public(),
            user_share: user_share.public(),
            user: user_account.into(),
            t0: now + 3_600,
            t1: now + 7_200,
            payout_note: [0; 32],
        };
        maker.open(&terms).await.unwrap();
        let id = swap_id(maker_account, &user_share.public());
        (id, terms, maker_share, user_share)
    };
    // Each block mines its one transaction, so the cap read just before a send is the one it gets.
    let reader = ProviderBuilder::new().connect_http(url.parse().unwrap());
    let cap = async |sent: B256| {
        let tx = reader.get_transaction_by_hash(sent).await.unwrap().unwrap();
        tx.max_fee_per_gas()
    };

    let (id, terms, _, user_share) = open(0).await;
    let usual = maker.max_fee_per_gas().await.unwrap();
    assert_eq!(cap(maker.ready(id, &terms).await.unwrap()).await, usual);
    let usual = user.max_fee_per_gas().await.unwrap();
    let claim = user.claim(id, &terms, &user_share).await.unwrap();
    assert_eq!(cap(claim).await, usual + headroom);

    let (id, terms, maker_share, _) = open(1).await;
    let usual = maker.max_fee_per_gas().await.unwrap();
    let cancel = maker.refund(id, &terms, &maker_share).await.unwrap();
    assert_eq!(cap(cancel).await, usual + headroom);
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
    let contract = deploy(&url, maker_key.clone(), swap_code(&artifacts, railgun))
        .await
        .unwrap();

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
    let terms = Terms {
        maker: maker_account.into(),
        token: token.into(),
        amount: 1_000_000,
        maker_share: e.public(),
        user_share: keys.share.public(),
        user: keys.auth.address(),
        t0: now + 3_600,
        t1: now + 7_200,
        payout_note: note.commitment(),
    };
    maker.open(&terms).await.unwrap();
    let id = swap_id(maker_account, &keys.share.public());
    maker.ready(id, &terms).await.unwrap();

    let user = Settlement::read_only(&url, contract).unwrap();
    let swap = user
        .swap(id, &terms)
        .await
        .unwrap()
        .expect("the swap is keyed, and its terms hashed, as in Rust");
    assert_eq!(swap.user, Address::from(keys.auth.address()));
    assert_eq!(swap.payout_note, Some(note.commitment().into()));
    let other_payee = Terms {
        user: [0x77; 20],
        ..terms.clone()
    };
    assert!(matches!(
        user.swap(id, &other_payee).await,
        Err(Error::WrongTerms(wrong)) if wrong == id
    ));
    assert!(user.railgun_accepts(token).await.unwrap());
    assert!(
        user.ready(id, &terms).await.is_err(),
        "a read-only connection sends nothing"
    );

    let relayer = Settlement::connect(&url, contract, relayer_key).unwrap();
    let domain = Domain {
        chain_id: user.chain_id().await.unwrap(),
        contract: contract.into(),
    };
    // Ready, the swap is its user's to claim, whoever sends the claim.
    relayer.claim(id, &terms, &keys.share).await.unwrap();
    let fee = 20_000;
    let payout = keys
        .auth
        .sign(&domain.payout(&id.0, &relayer_account.into(), fee));
    let tx = relayer
        .payout(id, &terms, &note, fee, &payout)
        .await
        .unwrap();

    let swap = user.swap(id, &terms).await.unwrap().unwrap();
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

/// A pass reads any number of swaps, and accounts, in one call where Multicall3 is deployed, as
/// on every chain the services use, and the same answers one at a time where it isn't; the
/// contract's Railgun proxy, which it fixes at deployment, is read once.
#[tokio::test]
async fn many_swaps_and_balances_are_read_in_one_call() {
    use zecswap_chain::evm::BlockId;

    let Ok(anvil) = Anvil::new().try_spawn() else {
        eprintln!("skipped: anvil is not installed");
        return;
    };
    let artifacts = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../contracts/out");
    if !artifacts.join("MockRailgun.sol").exists() {
        eprintln!("skipped: run `forge build` in contracts/");
        return;
    }
    let url = anvil.endpoint();
    let key = PrivateKeySigner::from(anvil.keys()[0].clone());
    let account = key.address();
    let creation = |name: &str| creation_code(&artifacts, name);
    let railgun = deploy(&url, key.clone(), creation("MockRailgun"))
        .await
        .unwrap();
    let token = deploy(&url, key.clone(), creation("TestToken"))
        .await
        .unwrap();
    let contract = deploy(&url, key.clone(), swap_code(&artifacts, railgun))
        .await
        .unwrap();
    let maker = Settlement::connect(&url, contract, key.clone()).unwrap();
    maker.mint_test_token(token, account, 3_000).await.unwrap();
    maker.add_inventory(token, 3_000).await.unwrap();
    let now = maker.now().await.unwrap();
    let mut ids = Vec::new();
    for nonce in 0..3 {
        let user_share = derive_maker_share(&[3; 32], nonce).unwrap().public();
        let terms = Terms {
            maker: account.into(),
            token: token.into(),
            amount: 1_000,
            maker_share: derive_maker_share(&[9; 32], nonce).unwrap().public(),
            user_share,
            user: [7; 20],
            t0: now + 3_600,
            t1: now + 7_200,
            payout_note: [0; 32],
        };
        maker.open(&terms).await.unwrap();
        ids.push(swap_id(account, &user_share));
    }
    ids.push(swap_id(
        Address::repeat_byte(5),
        &derive_maker_share(&[4; 32], 0).unwrap().public(),
    ));
    let owners = [anvil.addresses()[2], Address::repeat_byte(6)];
    let eth_calls = |chain: &Settlement| {
        chain
            .meter()
            .usage()
            .requests
            .get("eth_call")
            .copied()
            .unwrap_or(0)
    };

    let single = Settlement::read_only(&url, contract).unwrap();
    let one_by_one = single.swap_states(&ids, BlockId::latest()).await.unwrap();
    assert_eq!(eth_calls(&single), 4, "no Multicall3: a call a swap");
    assert_eq!(one_by_one.iter().flatten().count(), 3);
    assert!(one_by_one[3].is_none());
    let balances = single.eth_balances(&owners).await.unwrap();

    install_multicall3(&url, &key).await;
    let batched = Settlement::read_only(&url, contract).unwrap();
    assert_eq!(
        batched.swap_states(&ids, BlockId::latest()).await.unwrap(),
        one_by_one
    );
    assert_eq!(batched.eth_balances(&owners).await.unwrap(), balances);
    assert_eq!(
        eth_calls(&batched),
        2,
        "one call each, whatever their number"
    );
    assert_eq!(batched.railgun().await.unwrap(), railgun);
    assert_eq!(batched.railgun().await.unwrap(), railgun);
    assert_eq!(eth_calls(&batched), 3, "the Railgun proxy is read once");
    let usage = batched.meter().usage();
    assert_eq!(usage.requests.get("eth_getCode"), Some(&1));
    assert_eq!(usage.failed, 0);
}

/// Puts Multicall3's code where it is on every public chain, which a fresh anvil lacks.
async fn install_multicall3(url: &str, key: &PrivateKeySigner) {
    use alloy::providers::bindings::IMulticall3;
    use alloy::providers::{MULTICALL3_ADDRESS, Provider, ProviderBuilder};

    let deployed = deploy(url, key.clone(), IMulticall3::BYTECODE.to_vec())
        .await
        .unwrap();
    let provider = ProviderBuilder::new().connect_http(url.parse().unwrap());
    let code = provider.get_code_at(deployed).await.unwrap();
    let () = provider
        .raw_request("anvil_setCode".into(), (MULTICALL3_ADDRESS, code))
        .await
        .unwrap();
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
    let contract = deploy(&url, maker_key.clone(), swap_code(&artifacts, railgun))
        .await
        .unwrap();
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
        let id = reverse_swap_id(keys.auth.address().into(), &e.public());
        let escrow = terms.terms();
        let chain = maker.swap(id, &escrow).await.unwrap().unwrap();
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
        assert_eq!(
            maker.confirmed_swap(id, &escrow, 1).await.unwrap(),
            Some(chain)
        );
        let deadline = maker.now().await.unwrap() + 60;
        if index == 0 {
            let signature = keys.auth.sign(&domain.ready(&id.0, deadline));
            relay
                .ready_with_sig(id, &escrow, deadline, &signature)
                .await
                .unwrap();
            maker.claim(id, &escrow, &e).await.unwrap();
            assert_eq!(
                maker.balance_of(maker_address, token).await.unwrap(),
                terms.amount
            );
            assert_eq!(
                maker
                    .swap(id, &escrow)
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
            // Unready, the escrow is its user's to refund at any time.
            relay.refund(id, &escrow, &keys.share).await.unwrap();
            let fee = 20_000;
            let signature =
                keys.auth
                    .sign(&domain.refund_payout(&id.0, &relay_address.into(), fee));
            let tx = relay
                .refund_payout(id, &escrow, &note, fee, &signature)
                .await
                .unwrap();
            let notes = maker.shielded(tx).await.unwrap();
            assert_eq!(notes.len(), 1);
            assert_eq!(notes[0].value + notes[0].fee, terms.amount - fee);
            assert!(wallet.open(&notes[0].note).is_some());
            assert!(maker.swap(id, &escrow).await.unwrap().unwrap().paid_out);
        }
    }
}

alloy::sol! {
    struct TermsAbi {
        address maker;
        address token;
        uint128 amount;
        uint256[2] makerKey;
        uint256[2] userKey;
        address user;
        uint64 t0;
        uint64 t1;
        bytes32 payoutNote;
    }
    function claim(bytes32 id, TermsAbi terms, uint256 userSecret) external;
    function refund(bytes32 id, TermsAbi terms, uint256 makerSecret) external;
}

fn terms_abi(terms: &Terms) -> TermsAbi {
    let words = |share: &zecswap_core::PublicShare| {
        let bytes = share.to_affine_bytes();
        [
            U256::from_be_slice(&bytes[..32]),
            U256::from_be_slice(&bytes[32..]),
        ]
    };
    TermsAbi {
        maker: terms.maker.into(),
        token: terms.token.into(),
        amount: terms.amount,
        makerKey: words(&terms.maker_share),
        userKey: words(&terms.user_share),
        user: terms.user.into(),
        t0: terms.t0,
        t1: terms.t1,
        payoutNote: terms.payout_note.into(),
    }
}

async fn rpc(url: &str, method: &str, params: serde_json::Value) -> serde_json::Value {
    use alloy::providers::{Provider, ProviderBuilder};
    let provider = ProviderBuilder::new().connect_http(url.parse().unwrap());
    provider
        .raw_request::<_, serde_json::Value>(method.to_owned().into(), params)
        .await
        .unwrap()
}

/// The windows Rust judges reveals by, at every edge, against what the contract itself does with
/// the same reveal in the same block: forward and reverse, open and ready. At each moment one of a
/// live swap's two reveals is open, never both.
#[tokio::test]
async fn the_windows_rust_judges_reveals_by_are_the_contracts() {
    use alloy::eips::BlockId;
    use alloy::network::{EthereumWallet, TransactionBuilder};
    use alloy::providers::{Provider, ProviderBuilder};
    use alloy::rpc::types::TransactionRequest;
    use alloy::sol_types::SolCall;
    use zecswap_chain::evm::reverse_funding_calls;
    use zecswap_core::{ReverseOpen, SecretShare};

    let Ok(anvil) = Anvil::new().try_spawn() else {
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
    let funder_key = PrivateKeySigner::from(anvil.keys()[1].clone());
    let maker_account = maker_key.address();
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
    let contract = deploy(&url, maker_key.clone(), swap_code(&artifacts, railgun))
        .await
        .unwrap();
    let maker = Settlement::connect(&url, contract, maker_key).unwrap();
    maker
        .mint_test_token(token, maker_account, 2)
        .await
        .unwrap();
    maker.add_inventory(token, 2).await.unwrap();
    maker
        .mint_test_token(token, funder_key.address(), 2)
        .await
        .unwrap();
    let funder = ProviderBuilder::new()
        .wallet(EthereumWallet::from(funder_key))
        .connect_http(url.parse().unwrap());
    let domain = Domain {
        chain_id: maker.chain_id().await.unwrap(),
        contract: contract.into(),
    };
    let start = maker.now().await.unwrap();
    let (t0, t1) = (start + 3_600, start + 7_200);
    let seed = [5; 64];
    // Each case: the swap, its terms, whether it is reverse, and the two shares.
    let mut cases: Vec<(&str, _, Terms, bool, SecretShare, SecretShare)> = Vec::new();
    for (index, ready) in [(0u32, false), (1, true)] {
        let user = derive_user_keys(&seed, NetworkType::Test, 0, index).unwrap();
        let e = derive_maker_share(&[9; 32], index.into()).unwrap();
        let terms = Terms {
            maker: maker_account.into(),
            token: token.into(),
            amount: 1,
            maker_share: e.public(),
            user_share: user.share.public(),
            user: [7; 20],
            t0,
            t1,
            payout_note: [0; 32],
        };
        maker.open(&terms).await.unwrap();
        let id = swap_id(maker_account, &user.share.public());
        if ready {
            maker.ready(id, &terms).await.unwrap();
        }
        let name = if ready {
            "forward ready"
        } else {
            "forward open"
        };
        cases.push((name, id, terms, false, user.share, e));
    }
    for (index, ready) in [(2u32, false), (3, true)] {
        let user = derive_user_keys(&seed, NetworkType::Test, 0, index).unwrap();
        let m = derive_maker_share(&[9; 32], index.into()).unwrap();
        let open = ReverseOpen {
            maker: maker_account.into(),
            user: user.auth.address(),
            token: token.into(),
            amount: 1,
            maker_share: m.public(),
            user_share: user.share.public(),
            t0,
            t1,
            refund_note: [index as u8 + 1; 32],
            deadline: start + 600,
        };
        let signature = user.auth.sign(&domain.open_reverse(&open));
        for (to, data) in reverse_funding_calls(contract, &open, &signature) {
            let request = TransactionRequest::default().with_to(to).with_input(data);
            let sent = funder.send_transaction(request).await.unwrap();
            assert!(sent.get_receipt().await.unwrap().status());
        }
        let id = reverse_swap_id(user.auth.address().into(), &m.public());
        if ready {
            let deadline = maker.now().await.unwrap() + 60;
            let signature = user.auth.sign(&domain.ready(&id.0, deadline));
            maker
                .ready_with_sig(id, &open.terms(), deadline, &signature)
                .await
                .unwrap();
        }
        let name = if ready {
            "reverse ready"
        } else {
            "reverse open"
        };
        // The escrow's terms keep the contract's roles: the claim reveals the ZEC maker's share.
        cases.push((name, id, open.terms(), true, m, user.share));
    }

    let reader = Settlement::read_only(&url, contract).unwrap();
    assert_eq!(
        reader.windows().await.unwrap(),
        Windows {
            min: MIN_WINDOW,
            late: LATE_WINDOW
        }
    );
    let late = t1 + LATE_WINDOW;
    for at in [t0 - 1, t0, t1 - 1, t1, late - 1, late] {
        rpc(&url, "evm_setNextBlockTimestamp", serde_json::json!([at])).await;
        rpc(&url, "evm_mine", serde_json::json!([])).await;
        for (name, id, terms, reverse, claim_share, refund_share) in &cases {
            let mut open = 0;
            for (reveal, share) in [(Reveal::Claim, claim_share), (Reveal::Refund, refund_share)] {
                let (window, head) = reader.reveal_window(*id, terms, reveal).await.unwrap();
                assert_eq!(head.timestamp, at);
                let secret = U256::from_be_bytes(share.to_be_bytes());
                let data = match reveal {
                    Reveal::Claim => claimCall {
                        id: *id,
                        terms: terms_abi(terms),
                        userSecret: secret,
                    }
                    .abi_encode(),
                    Reveal::Refund => refundCall {
                        id: *id,
                        terms: terms_abi(terms),
                        makerSecret: secret,
                    }
                    .abi_encode(),
                };
                let request = TransactionRequest::default()
                    .with_to(contract)
                    .with_input(data);
                let lands = funder
                    .call(request)
                    .block(BlockId::hash(head.hash))
                    .await
                    .is_ok();
                let what = format!("{name}, {} at t1{:+}", reveal.name(), at as i64 - t1 as i64);
                assert_eq!(window != Window::Shut, lands, "{what}: {window:?}");
                open += usize::from(lands);
                let expected = match (*reverse, reveal, *name, at) {
                    (false, Reveal::Refund, "forward open", at) if at < t0 => Window::Until(t0),
                    (false, Reveal::Claim, _, at) if at < t1 && window != Window::Shut => {
                        Window::Until(t1)
                    }
                    (false, Reveal::Refund, _, at) if (t1..late).contains(&at) => {
                        Window::Until(late)
                    }
                    (false, Reveal::Claim, _, at) if at >= late => Window::Lasting,
                    (true, Reveal::Claim, "reverse ready", at) if at < t1 => Window::Until(t1),
                    (true, Reveal::Refund, "reverse open", _) => Window::Lasting,
                    (true, Reveal::Refund, "reverse ready", at) if at >= t1 => Window::Lasting,
                    _ => Window::Shut,
                };
                assert_eq!(window, expected, "{what}");
            }
            assert_eq!(open, 1, "{name} at t1{:+}", at as i64 - t1 as i64);
        }
    }
}

/// A reveal whose window is shut, or ends sooner than its account's margin, is refused before
/// anything goes out: its share stays secret. One with the margin to spare lands.
#[tokio::test]
async fn a_reveal_outside_its_window_is_never_sent() {
    use alloy::providers::{Provider, ProviderBuilder};

    let Ok(anvil) = Anvil::new().try_spawn() else {
        eprintln!("skipped: anvil is not installed");
        return;
    };
    let artifacts = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../contracts/out");
    if !artifacts.join("ZecSwap.sol").exists() {
        eprintln!("skipped: run `forge build` in contracts/");
        return;
    }
    let url = anvil.endpoint();
    let maker_key = PrivateKeySigner::from(anvil.keys()[0].clone());
    let user_key = PrivateKeySigner::from(anvil.keys()[1].clone());
    let (maker_account, user_account) = (maker_key.address(), user_key.address());
    let token = deploy(
        &url,
        maker_key.clone(),
        creation_code(&artifacts, "TestToken"),
    )
    .await
    .unwrap();
    let contract = deploy(
        &url,
        maker_key.clone(),
        swap_code(&artifacts, Address::ZERO),
    )
    .await
    .unwrap();
    let mut maker = Settlement::connect(&url, contract, maker_key).unwrap();
    let user = Settlement::connect(&url, contract, user_key).unwrap();
    maker
        .mint_test_token(token, maker_account, 1)
        .await
        .unwrap();
    maker.add_inventory(token, 1).await.unwrap();
    let user_share = derive_user_keys(&[7; 64], NetworkType::Test, 0, 0)
        .unwrap()
        .share;
    let maker_share = derive_maker_share(&[9; 32], 0).unwrap();
    let now = maker.now().await.unwrap();
    let terms = Terms {
        maker: maker_account.into(),
        token: token.into(),
        amount: 1,
        maker_share: maker_share.public(),
        user_share: user_share.public(),
        user: user_account.into(),
        t0: now + 3_600,
        t1: now + 7_200,
        payout_note: [0; 32],
    };
    maker.open(&terms).await.unwrap();
    let id = swap_id(maker_account, &user_share.public());
    let chain = ProviderBuilder::new().connect_http(url.parse().unwrap());
    let sent = async |account| chain.get_transaction_count(account).await.unwrap();
    let (user_sent, maker_sent) = (sent(user_account).await, sent(maker_account).await);

    // Open and before t0, the swap is the maker's to cancel: no claim goes out.
    let claimed = user.claim(id, &terms, &user_share).await;
    assert!(matches!(claimed, Err(Error::Late("claim"))), "{claimed:?}");
    // The cancel's window ends at t0, an hour off: under a margin of an hour, it never goes out.
    maker.reveal_with_margin(3_600);
    let cancelled = maker.refund(id, &terms, &maker_share).await;
    assert!(
        matches!(cancelled, Err(Error::Late("refund"))),
        "{cancelled:?}"
    );
    assert_eq!(sent(user_account).await, user_sent);
    assert_eq!(sent(maker_account).await, maker_sent);

    maker.reveal_with_margin(600);
    maker.refund(id, &terms, &maker_share).await.unwrap();
    let swap = maker.swap(id, &terms).await.unwrap().unwrap();
    assert_eq!(swap.stage, Stage::Refunded);
}

/// A send priced at the base fee then, which a rising base fee leaves out of the blocks after it,
/// goes again at the same nonce at the fees then, and lands: one transaction, not two.
#[tokio::test]
async fn a_public_send_left_unmined_goes_again_at_the_fees_then() {
    use alloy::consensus::Transaction as _;
    use alloy::providers::{Provider, ProviderBuilder};

    let Ok(anvil) = Anvil::new().try_spawn() else {
        eprintln!("skipped: anvil is not installed");
        return;
    };
    let artifacts = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../contracts/out");
    if !artifacts.join("ZecSwap.sol").exists() {
        eprintln!("skipped: run `forge build` in contracts/");
        return;
    }
    let url = anvil.endpoint();
    let key = PrivateKeySigner::from(anvil.keys()[0].clone());
    let account = key.address();
    let token = deploy(&url, key.clone(), creation_code(&artifacts, "TestToken"))
        .await
        .unwrap();
    let contract = deploy(&url, key.clone(), swap_code(&artifacts, Address::ZERO))
        .await
        .unwrap();
    let maker = std::sync::Arc::new(Settlement::connect(&url, contract, key).unwrap());
    maker.mint_test_token(token, account, 1).await.unwrap();
    maker.add_inventory(token, 1).await.unwrap();
    let now = maker.now().await.unwrap();
    let terms = Terms {
        maker: account.into(),
        token: token.into(),
        amount: 1,
        maker_share: derive_maker_share(&[9; 32], 0).unwrap().public(),
        user_share: derive_maker_share(&[8; 32], 0).unwrap().public(),
        user: [7; 20],
        t0: now + 3_600,
        t1: now + 7_200,
        payout_note: [0; 32],
    };
    let chain = ProviderBuilder::new().connect_http(url.parse().unwrap());
    let before = chain.get_transaction_count(account).await.unwrap();

    rpc(&url, "evm_setAutomine", serde_json::json!([false])).await;
    let opening = tokio::spawn({
        let (maker, terms) = (maker.clone(), terms.clone());
        async move { maker.open(&terms).await }
    });
    while chain
        .get_transaction_count(account)
        .pending()
        .await
        .unwrap()
        == before
    {
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    // The base fee leaps a hundredfold over what the open was priced at.
    let spike = 100_000_000_000u128;
    rpc(
        &url,
        "anvil_setNextBlockBaseFeePerGas",
        serde_json::json!([U256::from(spike)]),
    )
    .await;
    for _ in 0..100 {
        if opening.is_finished() {
            break;
        }
        rpc(&url, "evm_mine", serde_json::json!([])).await;
        tokio::time::sleep(Duration::from_millis(300)).await;
    }
    let opened = opening.await.unwrap().unwrap();
    let tx = chain
        .get_transaction_by_hash(opened)
        .await
        .unwrap()
        .unwrap();
    assert!(
        tx.max_fee_per_gas() > spike / 2,
        "it landed at a cap of {}",
        tx.max_fee_per_gas()
    );
    assert_eq!(
        chain.get_transaction_count(account).await.unwrap(),
        before + 1
    );
    let swap = maker
        .swap(swap_id(account, &terms.user_share), &terms)
        .await
        .unwrap();
    assert_eq!(swap.unwrap().stage, Stage::Open);
}

/// A windows client reads its windows from the contract, and finds none on the lock contract
/// deployed before, whose swap records and rules are another's.
#[tokio::test]
async fn the_lock_contract_has_no_windows_to_read() {
    let Ok(anvil) = Anvil::new().try_spawn() else {
        eprintln!("skipped: anvil is not installed");
        return;
    };
    let artifacts = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../contracts/out");
    if !artifacts.join("ZecSwapLocks.sol").exists() {
        eprintln!("skipped: run `forge build` in contracts/");
        return;
    }
    let url = anvil.endpoint();
    let key = PrivateKeySigner::from(anvil.keys()[0].clone());
    let mut code = creation_code(&artifacts, "ZecSwapLocks");
    code.extend_from_slice(&U256::from(1_800).to_be_bytes::<32>());
    code.extend_from_slice(&[0; 32]);
    let locks = deploy(&url, key.clone(), code).await.unwrap();
    let windows = deploy(&url, key, swap_code(&artifacts, Address::ZERO))
        .await
        .unwrap();
    let refused = Settlement::read_only(&url, locks).unwrap().windows().await;
    assert!(
        matches!(&refused, Err(Error::Contract(why)) if why.contains("MIN_WINDOW")),
        "{refused:?}"
    );
    let read = Settlement::read_only(&url, windows)
        .unwrap()
        .windows()
        .await
        .unwrap();
    assert_eq!((read.min, read.late), (MIN_WINDOW, LATE_WINDOW));
}
