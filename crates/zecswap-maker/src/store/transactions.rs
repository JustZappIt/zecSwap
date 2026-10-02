use anyhow::{Result, ensure};
use rusqlite::{OptionalExtension, params};
use serde::Serialize;
use zecswap_chain::evm::{B256, SwapEvent};

use super::{Notification, Store, notifications};

pub(super) const SCHEMA: &str = "
    CREATE TABLE IF NOT EXISTS evm_transactions (
        scope TEXT NOT NULL, swap_id BLOB NOT NULL, kind TEXT NOT NULL,
        transaction_hash BLOB NOT NULL, block_number INTEGER NOT NULL,
        block_hash BLOB NOT NULL, log_index INTEGER NOT NULL,
        PRIMARY KEY (scope, transaction_hash, log_index)
    );
    CREATE INDEX IF NOT EXISTS evm_transactions_swap ON evm_transactions(scope, swap_id, block_number);
    CREATE TABLE IF NOT EXISTS evm_transaction_cursors (
        scope TEXT PRIMARY KEY, next_block INTEGER NOT NULL, notify_from_block INTEGER NOT NULL,
        confirmed_head INTEGER NOT NULL, updated_at INTEGER, last_error TEXT
    );
";

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct EvmTransaction {
    pub kind: String,
    pub transaction_hash: B256,
    pub block_number: u64,
    pub block_hash: B256,
    pub log_index: u64,
}

pub(crate) struct TransactionCursor {
    pub next_block: u64,
    pub notify_from_block: u64,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct TransactionStatus {
    next_block: Option<u64>,
    confirmed_head: Option<u64>,
    updated_at: Option<u64>,
    last_error: Option<String>,
}

impl Store {
    pub(crate) fn earliest_swap_quote(&self) -> Result<Option<u64>> {
        Ok(self.conn().query_row(
            "SELECT min(expires_at) FROM quotes WHERE quote_id IN
             (SELECT quote_id FROM swaps UNION ALL SELECT quote_id FROM reverse_swaps)",
            [],
            |row| row.get(0),
        )?)
    }

    pub(crate) fn transaction_cursor(&self, scope: &str) -> Result<Option<TransactionCursor>> {
        Ok(self.conn().query_row(
            "SELECT next_block, notify_from_block FROM evm_transaction_cursors WHERE scope = ?1",
            [scope], |row| Ok(TransactionCursor { next_block: row.get(0)?, notify_from_block: row.get(1)? }),
        ).optional()?)
    }

    pub(crate) fn init_transaction_cursor(&self, scope: &str, start: u64, head: u64) -> Result<()> {
        self.conn().execute(
            "INSERT OR IGNORE INTO evm_transaction_cursors(scope, next_block, notify_from_block, confirmed_head)
             VALUES (?1, ?2, ?3, ?4)", params![scope, start, head + 1, head],
        )?;
        Ok(())
    }

    pub(crate) fn evm_transactions(&self, scope: &str, id: B256) -> Result<Vec<EvmTransaction>> {
        let conn = self.conn();
        Ok(conn.prepare(
            "SELECT kind, transaction_hash, block_number, block_hash, log_index FROM evm_transactions
             WHERE scope = ?1 AND swap_id = ?2 ORDER BY block_number, log_index",
        )?.query_map(params![scope, id.as_slice()], |row| Ok(EvmTransaction {
            kind: row.get(0)?, transaction_hash: B256::from(row.get::<_, [u8; 32]>(1)?),
            block_number: row.get(2)?, block_hash: B256::from(row.get::<_, [u8; 32]>(3)?), log_index: row.get(4)?,
        }))?.collect::<rusqlite::Result<Vec<_>>>()?)
    }

    /// Replace an overlapping block window to discard orphaned logs. Cursor and alert inserts
    /// commit together, so a restart cannot skip a notification after advancing the cursor.
    pub(crate) fn record_evm_window(
        &self,
        scope: &str,
        from: u64,
        to: u64,
        events: &[(SwapEvent, Option<Notification>)],
        advance: bool,
    ) -> Result<()> {
        ensure!(from <= to && to - from < 10, "invalid event window");
        let mut conn = self.conn();
        let tx = conn.transaction()?;
        tx.execute(
            "DELETE FROM evm_transactions WHERE scope = ?1 AND block_number BETWEEN ?2 AND ?3",
            params![scope, from, to],
        )?;
        for (event, notification) in events {
            ensure!(
                (from..=to).contains(&event.block_number),
                "event outside window"
            );
            let known: bool = tx.query_row(
                "SELECT EXISTS(SELECT 1 FROM swaps WHERE id = ?1 UNION ALL SELECT 1 FROM reverse_swaps WHERE id = ?1)",
                [event.id.as_slice()], |row| row.get(0),
            )?;
            if !known {
                continue;
            }
            tx.execute(
                "INSERT OR IGNORE INTO evm_transactions VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
                params![
                    scope,
                    event.id.as_slice(),
                    event.kind.as_str(),
                    event.transaction_hash.as_slice(),
                    event.block_number,
                    event.block_hash.as_slice(),
                    event.log_index
                ],
            )?;
            let notify_from: u64 = tx.query_row(
                "SELECT notify_from_block FROM evm_transaction_cursors WHERE scope = ?1",
                [scope],
                |row| row.get(0),
            )?;
            if event.block_number >= notify_from {
                notifications::insert(&tx, notification.as_ref())?;
            }
        }
        if advance {
            tx.execute("UPDATE evm_transaction_cursors SET next_block = ?2 WHERE scope = ?1 AND next_block = ?3",
                params![scope, to + 1, from])?;
        }
        tx.commit()?;
        Ok(())
    }

    pub(crate) fn record_transaction_pass(
        &self,
        scope: &str,
        head: u64,
        now: u64,
        failed: bool,
    ) -> Result<()> {
        self.conn().execute(
            "UPDATE evm_transaction_cursors SET confirmed_head = CASE WHEN ?2 = 0 THEN confirmed_head ELSE ?2 END, updated_at = ?3, last_error = ?4 WHERE scope = ?1",
            params![scope, head, now, failed.then_some("Ethereum transaction indexing failed; retrying")],
        )?;
        Ok(())
    }

    pub(crate) fn transaction_status(&self, scope: &str) -> Result<TransactionStatus> {
        Ok(self.conn().query_row(
            "SELECT next_block, confirmed_head, updated_at, last_error FROM evm_transaction_cursors WHERE scope = ?1",
            [scope], |row| Ok(TransactionStatus { next_block: Some(row.get(0)?), confirmed_head: Some(row.get(1)?),
                updated_at: row.get(2)?, last_error: row.get(3)? }),
        ).optional()?.unwrap_or(TransactionStatus { next_block: None, confirmed_head: None, updated_at: None, last_error: None }))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use zecswap_chain::{
        evm::{Address, SwapEventKind},
        zcash::AccountUuid,
    };
    use zecswap_core::{SecretShare, ViewingKeys};

    fn fixture(path: &std::path::Path) -> Store {
        let store = Store::open(path).unwrap();
        store
            .insert_quote([1; 32], Address::repeat_byte(2), None, 1234567, 100000, 200)
            .unwrap();
        let quote = store.take_quote(&[1; 32], 100).unwrap().unwrap();
        store
            .insert_swap(
                &super::super::Swap {
                    id: B256::repeat_byte(1),
                    quote,
                    user_share: SecretShare::random(rand_core::OsRng).public(),
                    viewing: ViewingKeys::random(rand_core::OsRng),
                    zcash_account: AccountUuid::from_uuid(uuid::Uuid::nil()),
                    opened_at: 100,
                    t1: 300,
                    sweep: None,
                    settled: false,
                },
                None,
            )
            .unwrap();
        store.init_transaction_cursor("testnet", 90, 99).unwrap();
        store
    }

    fn event(block: u64) -> SwapEvent {
        SwapEvent {
            id: B256::repeat_byte(1),
            kind: SwapEventKind::Claimed,
            transaction_hash: B256::repeat_byte(3),
            block_number: block,
            block_hash: B256::repeat_byte(4),
            log_index: 1,
        }
    }

    fn notification() -> Notification {
        Notification {
            key: "transaction:3:1".into(),
            text: "Confirmed transaction links".into(),
            created_at: 100,
        }
    }

    #[test]
    fn historical_backfill_is_silent_and_resumes_after_restart() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("maker.sqlite");
        let store = fixture(&path);
        store
            .record_evm_window(
                "testnet",
                90,
                99,
                &[(event(95), Some(notification()))],
                true,
            )
            .unwrap();
        assert_eq!(store.notification_status(true).unwrap().pending, 0);
        drop(store);
        let store = Store::open(&path).unwrap();
        let cursor = store.transaction_cursor("testnet").unwrap().unwrap();
        assert_eq!(cursor.next_block, 100);
        assert_eq!(cursor.notify_from_block, 100);
        assert_eq!(
            store
                .evm_transactions("testnet", B256::repeat_byte(1))
                .unwrap()
                .len(),
            1
        );
        assert!(
            store
                .evm_transactions("mainnet", B256::repeat_byte(1))
                .unwrap()
                .is_empty()
        );
    }

    #[test]
    fn overlapping_windows_deduplicate_alerts_and_remove_orphaned_transactions() {
        let store = fixture(std::path::Path::new(":memory:"));
        let events = [(event(101), Some(notification()))];
        for _ in 0..2 {
            store
                .record_evm_window("testnet", 100, 109, &events, false)
                .unwrap();
        }
        assert_eq!(store.notification_status(true).unwrap().pending, 1);
        let recorded = store
            .evm_transactions("testnet", B256::repeat_byte(1))
            .unwrap();
        assert_eq!(recorded.len(), 1);
        assert_ne!(recorded[0].transaction_hash, B256::repeat_byte(1));
        store
            .record_evm_window("testnet", 100, 109, &[], false)
            .unwrap();
        assert!(
            store
                .evm_transactions("testnet", B256::repeat_byte(1))
                .unwrap()
                .is_empty()
        );
        let foreign = SwapEvent {
            id: B256::repeat_byte(9),
            ..event(101)
        };
        store
            .record_evm_window(
                "testnet",
                100,
                109,
                &[(
                    foreign,
                    Some(Notification {
                        key: "foreign".into(),
                        ..notification()
                    }),
                )],
                false,
            )
            .unwrap();
        assert!(
            store
                .evm_transactions("testnet", B256::repeat_byte(9))
                .unwrap()
                .is_empty()
        );
        assert_eq!(store.notification_status(true).unwrap().pending, 1);
    }

    #[test]
    fn cursor_transaction_and_notification_rollback_together_on_queue_failure() {
        let store = fixture(std::path::Path::new(":memory:"));
        store
            .record_evm_window("testnet", 90, 99, &[], true)
            .unwrap();
        store.conn().execute_batch("CREATE TRIGGER reject_alert BEFORE INSERT ON notifications BEGIN SELECT RAISE(FAIL, 'injected queue failure'); END;").unwrap();
        assert!(
            store
                .record_evm_window(
                    "testnet",
                    100,
                    109,
                    &[(event(101), Some(notification()))],
                    true
                )
                .is_err()
        );
        assert_eq!(
            store
                .transaction_cursor("testnet")
                .unwrap()
                .unwrap()
                .next_block,
            100
        );
        assert!(
            store
                .evm_transactions("testnet", B256::repeat_byte(1))
                .unwrap()
                .is_empty()
        );
    }
}
