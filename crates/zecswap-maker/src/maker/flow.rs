use std::{panic::AssertUnwindSafe, path::Path, sync::Arc, time::Duration};

use anyhow::{Result, ensure};
use futures_util::FutureExt;
use rusqlite::{Connection, OpenFlags, OptionalExtension};
use zecswap_chain::zcash::{AccountUuid, TxId, Wallet};
use zecswap_core::JointAccount;

use super::{Maker, unix_now};
use crate::store::{FlowObservation, ZecTransaction};

fn readonly(path: &Path) -> Result<Connection> {
    let conn = Connection::open_with_flags(
        path,
        OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_NO_MUTEX,
    )?;
    conn.busy_timeout(Duration::from_millis(250))?;
    Ok(conn)
}

fn observe(
    conn: &Connection,
    account: AccountUuid,
    expected: u64,
    required: u32,
) -> Result<FlowObservation> {
    ensure!(required > 0, "confirmation policy must be positive");
    let birthday: Option<u32> =
        conn.query_row("SELECT min(birthday_height) FROM accounts", [], |r| {
            r.get(0)
        })?;
    let scanned: Option<u32> = conn
        .query_row(
            "SELECT block_range_start, block_range_end FROM scan_queue
        WHERE priority = 10 ORDER BY block_range_start LIMIT 1",
            [],
            |r| {
                let start: u32 = r.get(0)?;
                let end: u32 = r.get(1)?;
                Ok(birthday
                    .filter(|birthday| start <= *birthday)
                    .and_then(|_| end.checked_sub(1)))
            },
        )
        .optional()?
        .flatten();
    let mut transactions = Vec::new();
    let mut deposited = 0u64;
    let mut spent = 0u64;
    let mut statement = conn.prepare(
        "SELECT txid, mined_height, COALESCE((SELECT SUM(value) FROM v_tx_outputs outputs
            WHERE outputs.txid = history.txid AND to_account_uuid = history.account_uuid
            AND NOT COALESCE(is_change, 0)), 0), total_spent, block_time, expired_unmined
        FROM v_transactions history WHERE account_uuid = ?1 ORDER BY mined_height, txid",
    )?;
    let records = statement.query_map([account.expose_uuid().as_bytes().as_slice()], |r| {
        Ok((
            r.get::<_, [u8; 32]>(0)?,
            r.get::<_, Option<u32>>(1)?,
            r.get::<_, u64>(2)?,
            r.get::<_, u64>(3)?,
            r.get::<_, Option<u64>>(4)?,
            r.get::<_, bool>(5)?,
        ))
    })?;
    for record in records {
        let (hash, height, received, sent, block_time, expired) = record?;
        let confirmations = height
            .zip(scanned)
            .and_then(|(mined, head)| head.checked_sub(mined))
            .map_or(0, |depth| depth.saturating_add(1));
        let state = if confirmations >= required {
            "confirmed"
        } else if height.is_some() {
            "confirming"
        } else if expired {
            "expired"
        } else {
            "observed"
        };
        if state == "confirmed" {
            deposited = deposited
                .checked_add(received)
                .ok_or_else(|| anyhow::anyhow!("deposit overflow"))?;
            spent = spent
                .checked_add(sent)
                .ok_or_else(|| anyhow::anyhow!("spend overflow"))?;
        }
        for (value, kind) in [(received, "deposit"), (sent, "spend")] {
            if value == 0 {
                continue;
            }
            transactions.push(ZecTransaction {
                txid: TxId::from_bytes(hash).to_string(),
                kind: kind.into(),
                state: state.into(),
                mined_height: height,
                confirmations,
                block_time,
                value_zat: value.to_string(),
            });
        }
    }
    Ok(FlowObservation {
        observed_at: unix_now(),
        scanned_height: scanned,
        required_confirmations: required,
        deposit_verified: deposited >= expected,
        confirmed_deposit_zat: deposited.to_string(),
        confirmed_spent_zat: spent.to_string(),
        transactions,
    })
}

impl Maker {
    pub(super) fn zec_link(&self, hash: &str) -> String {
        format!(
            "https://{}zecblock.com/tx/{hash}",
            if self.alert_network() == "testnet" {
                "testnet."
            } else {
                ""
            }
        )
    }

    async fn flow_pass(&self) -> Result<()> {
        let scope = self.transaction_scope();
        let notify_after = self.store.init_flow_observer(&scope, unix_now())?;
        let path = self.config.data_dir.join("flow-wallet.sqlite");
        let mut wallet = Wallet::open(&path, self.config.network())?;
        let mut client = zecswap_chain::zcash::connect(&self.config.lightwalletd).await?;
        let original = readonly(&self.config.data_dir.join("wallet.sqlite"))?;
        let mut accounts = Vec::new();
        // Include completed swaps: final recovery often happens after the maker's job ends.
        for record in self.store.monitor_swaps(self.config.timing.t0_after, 500)? {
            let after = if record.direction == "reverse" {
                self.config
                    .reverse
                    .as_ref()
                    .map_or(self.config.timing.t0_after, |config| config.ready_after)
            } else {
                self.config.timing.t0_after
            };
            let since = record
                .ready_deadline
                .saturating_sub(after)
                .saturating_sub(300);
            let height: Option<u32> = original
                .query_row(
                    "SELECT height FROM blocks WHERE time <= ?1 ORDER BY height DESC LIMIT 1",
                    [since],
                    |row| row.get(0),
                )
                .optional()?
                .or(original.query_row("SELECT min(height) FROM blocks", [], |row| row.get(0))?);
            let joint = if let Some(swap) = self.store.swap(&record.id)? {
                JointAccount::derive(
                    &self.maker_share(swap.quote.nonce)?.public(),
                    &swap.user_share,
                    &swap.viewing,
                )?
            } else if let Some(swap) = self.store.reverse_swap(record.id)? {
                JointAccount::derive(
                    &swap.quote.terms.maker_share,
                    &swap.acceptance.user_share,
                    &swap.acceptance.viewing_keys,
                )?
            } else {
                continue;
            };
            let account = wallet
                .import_joint_view(
                    &mut client,
                    &joint,
                    &format!("bridge {}", record.id),
                    height,
                )
                .await?;
            accounts.push((record.id, account, record.deposit_zat.parse::<u64>()?));
        }
        drop(original);
        wallet.sync(&mut client).await?;
        drop(wallet);
        let mut conn = readonly(&path)?;
        let tx = conn.transaction()?;
        let required = self.config.confirmations.map_or(10, |c| c.get());
        let observations: Vec<_> = accounts
            .into_iter()
            .map(|(id, account, expected)| {
                observe(&tx, account, expected, required).map(|observation| (id, observation))
            })
            .collect::<Result<_>>()?;
        tx.commit()?;
        let mut alerts = Vec::new();
        for (id, observation) in &observations {
            let previous = self.store.flow_observation(&scope, *id).ok().flatten();
            for transaction in &observation.transactions {
                if !previous.as_ref().is_some_and(|previous| {
                    previous.transactions.iter().any(|old| {
                        old.txid == transaction.txid
                            && old.kind == transaction.kind
                            && old.state == transaction.state
                            && old.mined_height == transaction.mined_height
                    })
                }) {
                    tracing::info!(swap_id = %id, transaction_hash = %transaction.txid,
                        operation = %transaction.kind, chain = "zcash", outcome = %transaction.state,
                        confirmations = transaction.confirmations, "Zcash flow observation changed");
                }
                if transaction.state != "confirmed"
                    || transaction
                        .block_time
                        .is_none_or(|time| time < notify_after)
                {
                    continue;
                }
                let phase = format!("zec:{}:{}", transaction.kind, transaction.txid);
                let detail = format!(
                    "ZEC {} confirmed by the server viewing wallet ({} confirmations).{}\n{}",
                    if transaction.kind == "deposit" {
                        "deposit"
                    } else {
                        "escrow spend"
                    },
                    transaction.confirmations,
                    if observation.deposit_verified {
                        " Quoted deposit amount verified."
                    } else {
                        " Deposit amount is not yet fully verified."
                    },
                    self.zec_link(&transaction.txid)
                );
                let alert = if let Some(swap) = self.store.swap(id)? {
                    self.forward_alert(&swap, &phase, &detail)
                } else if let Some(swap) = self.store.reverse_swap(*id)? {
                    self.reverse_alert(&swap, &phase, &detail)
                } else {
                    None
                };
                if let Some(alert) = alert {
                    alerts.push(alert);
                }
            }
        }
        self.store
            .save_flow_observations(&scope, &observations, &alerts, unix_now())?;
        Ok(())
    }

    pub(super) async fn run_flow_observer(self: &Arc<Self>) {
        loop {
            let maker = self.clone();
            // SDK scanning is synchronous; keep observation off the settlement executor.
            let result = tokio::task::spawn_blocking(move || {
                tokio::runtime::Handle::current().block_on(async {
                    let pass = AssertUnwindSafe(maker.flow_pass()).catch_unwind();
                    match tokio::time::timeout(Duration::from_secs(90), pass).await {
                        Ok(Ok(Ok(()))) => None,
                        Ok(Ok(Err(error))) => Some(super::observer_failure(&error)),
                        Ok(Err(_)) => Some("panic"),
                        Err(_) => Some("timeout"),
                    }
                })
            })
            .await;
            if let Some(failure_kind) = result.unwrap_or(Some("worker_failed")) {
                let _ = self.store.flow_error(&self.transaction_scope());
                tracing::warn!(
                    operation = "zcash_flow_observer",
                    failure_kind,
                    "Zcash flow observation unavailable; retrying"
                );
            }
            tokio::time::sleep(Duration::from_secs(30)).await;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use rusqlite::params;

    fn fixture() -> (Connection, AccountUuid) {
        let conn = Connection::open_in_memory().unwrap();
        let account = AccountUuid::from_uuid(uuid::Uuid::nil());
        conn.execute_batch("CREATE TABLE accounts (birthday_height INTEGER);
            INSERT INTO accounts VALUES (100);
            CREATE TABLE scan_queue (block_range_start INTEGER, block_range_end INTEGER, priority INTEGER);
            INSERT INTO scan_queue VALUES (100,111,10);
            CREATE TABLE v_transactions (account_uuid BLOB, txid BLOB, mined_height INTEGER,
                total_spent INTEGER, block_time INTEGER, expired_unmined INTEGER);
            CREATE TABLE v_tx_outputs (txid BLOB, to_account_uuid BLOB, value INTEGER, is_change INTEGER);").unwrap();
        (conn, account)
    }
    fn transaction(
        conn: &Connection,
        account: AccountUuid,
        byte: u8,
        mined: Option<u32>,
        received: u64,
        spent: u64,
        change: u64,
    ) {
        let hash = [byte; 32];
        let uuid = account.expose_uuid();
        conn.execute(
            "INSERT INTO v_transactions VALUES (?1,?2,?3,?4,1000,0)",
            params![uuid.as_bytes().as_slice(), hash.as_slice(), mined, spent],
        )
        .unwrap();
        for (value, is_change) in [(received, false), (change, true)] {
            conn.execute(
                "INSERT INTO v_tx_outputs VALUES (?1,?2,?3,?4)",
                params![
                    hash.as_slice(),
                    uuid.as_bytes().as_slice(),
                    value,
                    is_change
                ],
            )
            .unwrap();
        }
    }
    #[test]
    fn deposits_require_scanned_confirmations_and_exclude_change() {
        let (conn, account) = fixture();
        transaction(&conn, account, 1, Some(109), 40, 0, 0);
        transaction(&conn, account, 2, Some(110), 60, 0, 0);
        transaction(&conn, account, 3, Some(108), 0, 40, 900);
        let observation = observe(&conn, account, 100, 2).unwrap();
        assert!(!observation.deposit_verified);
        assert_eq!(observation.confirmed_deposit_zat, "40");
        assert_eq!(observation.confirmed_spent_zat, "40");
        assert_eq!(observation.transactions.len(), 3);
        assert_eq!(
            observation
                .transactions
                .iter()
                .find(|t| t.txid == TxId::from_bytes([2; 32]).to_string())
                .unwrap()
                .state,
            "confirming"
        );
        conn.execute("UPDATE scan_queue SET block_range_end=112", [])
            .unwrap();
        assert!(observe(&conn, account, 100, 2).unwrap().deposit_verified);
        // A gap/reorg removes verification even if the server advertised a higher tip.
        conn.execute("UPDATE scan_queue SET block_range_end=110", [])
            .unwrap();
        assert!(!observe(&conn, account, 100, 2).unwrap().deposit_verified);
        conn.execute("UPDATE scan_queue SET block_range_start=101", [])
            .unwrap();
        let observation = observe(&conn, account, 100, 2).unwrap();
        assert_eq!(observation.scanned_height, None);
        assert_eq!(observation.confirmed_spent_zat, "0");
    }
    #[test]
    fn public_flow_contains_only_allowlisted_evidence() {
        let (conn, account) = fixture();
        transaction(&conn, account, 4, None, 100, 0, 0);
        let observation = observe(&conn, account, 100, 2).unwrap();
        assert!(!observation.deposit_verified);
        assert_eq!(observation.transactions[0].state, "observed");
        let public = serde_json::to_string(&observation).unwrap();
        for private in [
            "accountUuid",
            "memo",
            "address",
            "ufvk",
            "viewing",
            "secret",
        ] {
            assert!(!public.contains(private));
        }
        assert!(observe(&conn, account, 100, 0).is_err());
    }
}
