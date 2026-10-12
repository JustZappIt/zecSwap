use std::sync::Mutex;
use std::time::Duration;

use alloy::node_bindings::Anvil;
use rusqlite::Connection;
use zecswap_chain::evm::{Address, PrivateKeySigner, Settlement, U256};

use super::*;

const MILLI: u128 = 1_000_000_000_000_000;

fn alerts() -> GasAlerts {
    GasAlerts {
        low_wei: 500 * MILLI as u64,
        recovery_wei: 800 * MILLI as u64,
        interval_seconds: 300,
    }
}

fn queue() -> Mutex<Connection> {
    let conn = Connection::open_in_memory().unwrap();
    conn.execute_batch(crate::SCHEMA).unwrap();
    Mutex::new(conn)
}

/// The queued messages' keys and texts, oldest first.
fn queued(db: &Mutex<Connection>) -> Vec<(String, String)> {
    let conn = db.lock().unwrap();
    let mut statement = conn
        .prepare("SELECT event_key, text FROM notifications ORDER BY rowid")
        .unwrap();
    statement
        .query_map([], |row| Ok((row.get(0)?, row.get(1)?)))
        .unwrap()
        .collect::<rusqlite::Result<_>>()
        .unwrap()
}

#[test]
fn a_band_between_low_and_topped_up_and_balances_never_read_higher() {
    let alerts = alerts();
    assert_eq!(alerts.low(U256::from(500 * MILLI - 1)), Some(true));
    assert_eq!(alerts.low(U256::from(500 * MILLI)), None);
    assert_eq!(alerts.low(U256::from(800 * MILLI - 1)), None);
    assert_eq!(alerts.low(U256::from(800 * MILLI)), Some(false));
    assert_eq!(alerts.low(U256::MAX), Some(false));
    assert_eq!(eth(U256::from(1)), "0");
    assert_eq!(eth(U256::from(7_925_991_455_494_601u64)), "0.007925");
    assert_eq!(eth(U256::from(1_000_000_000_000_000_000u64)), "1");
    assert_eq!(eth(U256::from(20_500_000_000_000_000_000u128)), "20.5");
}

#[test]
fn checks_its_thresholds_and_shows_the_account_it_watches() {
    let valid = alerts();
    assert!(valid.check().is_ok());
    let mut bad = valid.clone();
    bad.interval_seconds = 59;
    assert!(bad.check().is_err());
    bad = valid.clone();
    bad.low_wei = 0;
    assert!(bad.check().is_err());
    bad = valid.clone();
    bad.recovery_wei = bad.low_wei;
    assert!(bad.check().is_err());
    let address = Address::repeat_byte(1);
    assert_eq!(
        serde_json::to_value(valid.status(true, "Maker", address)).unwrap(),
        serde_json::json!({
            "enabled": true,
            "intervalSeconds": 300,
            "accounts": [{
                "label": "Maker",
                "address": address,
                "lowWei": "500000000000000000",
                "recoveryWei": "800000000000000000",
            }],
        })
    );
}

#[test]
fn each_low_spell_says_so_once_when_it_starts_and_once_when_it_ends() {
    let db = queue();
    let event = || Notification {
        key: String::new(),
        text: "gas".into(),
        created_at: 1,
    };
    let mut conn = db.lock().unwrap();
    // A first reading that is fine says nothing.
    assert!(!episode(&mut conn, "fine", false, event()).unwrap());
    assert!(episode(&mut conn, "a", true, event()).unwrap());
    assert!(!episode(&mut conn, "a", true, event()).unwrap());
    assert!(is_low(&conn, "a").unwrap());
    assert!(episode(&mut conn, "a", false, event()).unwrap());
    assert!(!episode(&mut conn, "a", false, event()).unwrap());
    assert!(!is_low(&conn, "a").unwrap());
    assert!(episode(&mut conn, "a", true, event()).unwrap());
    drop(conn);
    let keys: Vec<_> = queued(&db).into_iter().map(|(key, _)| key).collect();
    assert_eq!(keys, ["a:low:1", "a:recovered:1", "a:low:2"]);
}

#[tokio::test]
async fn reads_the_account_when_it_starts_after_its_sends_and_while_it_is_low() {
    let Ok(anvil) = Anvil::new().block_time(1).try_spawn() else {
        eprintln!("skipped: anvil is not installed");
        return;
    };
    let url = anvil.endpoint();
    let settlement = Settlement::connect(
        &url,
        Address::ZERO,
        PrivateKeySigner::from(anvil.keys()[0].clone()),
    )
    .unwrap();
    let account = settlement.account().unwrap();
    // As a top-up, or anything but the account's own send, changes it.
    let set_balance = async |wei: u128| {
        let request = serde_json::json!({
            "jsonrpc": "2.0",
            "id": 1,
            "method": "anvil_setBalance",
            "params": [account, format!("{wei:#x}")],
        });
        let response = reqwest::Client::new()
            .post(url.as_str())
            .header("content-type", "application/json")
            .body(request.to_string())
            .send()
            .await
            .unwrap();
        assert!(response.status().is_success());
    };
    let db = queue();
    let queued_at_least = async |count: usize| {
        for _ in 0..100 {
            let messages = queued(&db);
            if messages.len() >= count {
                return messages;
            }
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
        panic!("{:?}", queued(&db));
    };
    let alerts = GasAlerts {
        interval_seconds: 1,
        ..alerts()
    };
    set_balance(100 * MILLI).await;
    let scope = format!("testnet:31337:gas:{account}");
    let steps = async {
        let low = queued_at_least(1).await;
        assert_eq!(
            low[0],
            (
                format!("{scope}:low:1"),
                format!("ZecSwap testnet · gas low: Relayer\n0.1 ETH, under 0.5 ETH\n{account}")
            ),
            "read when it starts"
        );
        set_balance(1000 * MILLI).await;
        let topped_up = queued_at_least(2).await;
        assert_eq!(
            topped_up[1],
            (
                format!("{scope}:recovered:1"),
                "ZecSwap testnet · gas topped up: Relayer\n1 ETH".into()
            ),
            "read while low"
        );
        set_balance(100 * MILLI).await;
        tokio::time::sleep(Duration::from_secs(3)).await;
        assert_eq!(queued(&db).len(), 2, "fine, it is read only after a send");
        settlement
            .send_eth(Address::repeat_byte(7), U256::from(1))
            .await
            .unwrap();
        let low = queued_at_least(3).await;
        assert_eq!(low[2].0, format!("{scope}:low:2"));
        assert!(
            low[2]
                .1
                .starts_with("ZecSwap testnet · gas low: Relayer\n0.0999")
                && low[2]
                    .1
                    .ends_with(&format!(" ETH, under 0.5 ETH\n{account}")),
            "{low:?}"
        );
    };
    tokio::select! {
        () = watch(&db, &settlement, &alerts, "Relayer", "testnet", 31337) => unreachable!(),
        () = steps => {}
    }
}
