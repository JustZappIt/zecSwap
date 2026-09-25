//! The Base client against a local anvil; skipped where Foundry isn't installed.

use std::time::Duration;

use alloy::node_bindings::Anvil;
use zecswap_chain::base::{Address, PrivateKeySigner, Settlement, U256};

#[tokio::test]
async fn a_send_that_fails_leaves_no_nonce_gap() {
    let Ok(anvil) = Anvil::new().block_time(1).try_spawn() else {
        eprintln!("skipped: anvil is not installed");
        return;
    };
    let key = PrivateKeySigner::from(anvil.keys()[0].clone());
    let chain = Settlement::connect(&anvil.endpoint(), Address::ZERO, key).unwrap();
    let to = Address::repeat_byte(1);

    let too_much = chain.eth_balance(chain.account()).await.unwrap() + U256::from(1);
    assert!(chain.send_eth(to, too_much).await.is_err());
    let next = tokio::time::timeout(Duration::from_secs(20), chain.send_eth(to, U256::from(1)));
    next.await
        .expect("the next send is stuck behind a nonce gap")
        .unwrap();
}
