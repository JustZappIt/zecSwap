use anyhow::Result;
use rusqlite::{Connection, OptionalExtension, params};
use serde::Serialize;

use super::Store;

pub(super) const SCHEMA: &str = "
    CREATE TABLE IF NOT EXISTS notifications (
        event_key TEXT PRIMARY KEY,
        text TEXT NOT NULL,
        created_at INTEGER NOT NULL,
        next_attempt_at INTEGER NOT NULL,
        attempts INTEGER NOT NULL DEFAULT 0,
        delivered_at INTEGER,
        message_id INTEGER,
        last_error TEXT
    );
    CREATE INDEX IF NOT EXISTS notifications_due ON notifications (delivered_at, next_attempt_at);
    CREATE TABLE IF NOT EXISTS notification_failures (
        scope TEXT PRIMARY KEY,
        active INTEGER NOT NULL,
        generation INTEGER NOT NULL
    );
    CREATE TABLE IF NOT EXISTS notification_delivery (
        singleton INTEGER PRIMARY KEY CHECK (singleton = 1),
        next_send_at INTEGER NOT NULL DEFAULT 0
    );
    INSERT OR IGNORE INTO notification_delivery (singleton) VALUES (1);
";

pub(crate) struct Notification {
    pub key: String,
    pub text: String,
    pub created_at: u64,
}

pub(crate) struct Delivery {
    pub key: String,
    pub text: String,
    pub attempts: u32,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct NotificationStatus {
    pub enabled: bool,
    pub pending: u64,
    pub oldest_pending_at: Option<u64>,
    pub last_delivered_at: Option<u64>,
    pub last_error: Option<String>,
}

pub(super) fn insert(conn: &Connection, event: Option<&Notification>) -> Result<()> {
    if let Some(event) = event {
        conn.execute(
            "INSERT INTO notifications (event_key, text, created_at, next_attempt_at)
             VALUES (?1, ?2, ?3, ?3) ON CONFLICT(event_key) DO NOTHING",
            params![event.key, event.text, event.created_at],
        )?;
    }
    Ok(())
}

impl Store {
    pub(crate) fn enqueue_notification(&self, event: &Notification) -> Result<()> {
        insert(&self.conn(), Some(event))
    }

    pub(crate) fn notification_failure(
        &self,
        scope: &str,
        event: Option<Notification>,
    ) -> Result<()> {
        let mut conn = self.conn();
        let tx = conn.transaction()?;
        if let Some(mut event) = event {
            let generation: Option<u64> = tx.query_row(
                "INSERT INTO notification_failures (scope, active, generation) VALUES (?1, 1, 1)
                 ON CONFLICT(scope) DO UPDATE SET active = 1, generation = generation + 1
                 WHERE active = 0 RETURNING generation", [scope], |row| row.get(0),
            ).optional()?;
            if let Some(generation) = generation {
                event.key = format!("{scope}:error:{generation}");
                insert(&tx, Some(&event))?;
            }
        } else {
            tx.execute(
                "UPDATE notification_failures SET active = 0 WHERE scope = ?1",
                [scope],
            )?;
        }
        tx.commit()?;
        Ok(())
    }

    /// Persist a low/recovered episode and its message together. Repeated readings
    /// and process restarts do not duplicate alerts; an initial healthy reading is silent.
    pub(crate) fn gas_notification(
        &self,
        scope: &str,
        low: bool,
        mut event: Notification,
    ) -> Result<bool> {
        let mut conn = self.conn();
        let tx = conn.transaction()?;
        let generation: Option<u64> = if low {
            tx.query_row(
                "INSERT INTO notification_failures (scope, active, generation) VALUES (?1, 1, 1)
                 ON CONFLICT(scope) DO UPDATE SET active = 1, generation = generation + 1
                 WHERE active = 0 RETURNING generation",
                [scope],
                |row| row.get(0),
            )
            .optional()?
        } else {
            tx.query_row(
                "UPDATE notification_failures SET active = 0 WHERE scope = ?1 AND active = 1 RETURNING generation",
                [scope], |row| row.get(0),
            ).optional()?
        };
        if let Some(generation) = generation {
            event.key = format!(
                "{scope}:{}:{generation}",
                if low { "low" } else { "recovered" }
            );
            insert(&tx, Some(&event))?;
        }
        tx.commit()?;
        Ok(generation.is_some())
    }

    pub(crate) fn claim_notification(&self, now: u64) -> Result<Option<Delivery>> {
        let mut conn = self.conn();
        let tx = conn.transaction()?;
        let delivery = tx.query_row(
            "UPDATE notifications SET next_attempt_at = ?1 + 60, attempts = attempts + 1
             WHERE event_key = (
                 SELECT event_key FROM notifications WHERE delivered_at IS NULL AND next_attempt_at <= ?1
                     AND (SELECT next_send_at FROM notification_delivery WHERE singleton = 1) <= ?1
                 ORDER BY created_at, rowid LIMIT 1
             ) RETURNING event_key, text, attempts", [now],
            |row| Ok(Delivery { key: row.get(0)?, text: row.get(1)?, attempts: row.get(2)? }),
        ).optional()?;
        if delivery.is_some() {
            tx.execute(
                "UPDATE notification_delivery SET next_send_at = ?1 + 3 WHERE singleton = 1",
                [now],
            )?;
        }
        tx.commit()?;
        Ok(delivery)
    }

    pub(crate) fn acknowledge_notification(
        &self,
        delivery: &Delivery,
        now: u64,
        message_id: i64,
    ) -> Result<()> {
        self.conn().execute(
            "UPDATE notifications SET delivered_at = ?2, message_id = ?3, last_error = NULL
             WHERE event_key = ?1 AND delivered_at IS NULL AND attempts = ?4",
            params![delivery.key, now, message_id, delivery.attempts],
        )?;
        Ok(())
    }

    pub(crate) fn retry_notification(
        &self,
        delivery: &Delivery,
        now: u64,
        delay: u64,
        error: &str,
    ) -> Result<()> {
        let mut conn = self.conn();
        let tx = conn.transaction()?;
        tx.execute(
            "UPDATE notifications SET next_attempt_at = ?2, last_error = ?3
             WHERE event_key = ?1 AND delivered_at IS NULL AND attempts = ?4",
            params![
                delivery.key,
                now.saturating_add(delay),
                error,
                delivery.attempts
            ],
        )?;
        tx.execute("UPDATE notification_delivery SET next_send_at = MAX(next_send_at, ?1) WHERE singleton = 1", [now.saturating_add(delay)])?;
        tx.commit()?;
        Ok(())
    }

    pub(crate) fn notification_delivered(&self, key: &str) -> Result<bool> {
        Ok(self.conn().query_row(
            "SELECT delivered_at IS NOT NULL FROM notifications WHERE event_key = ?1",
            [key],
            |row| row.get(0),
        )?)
    }

    pub(crate) fn notification_status(&self, enabled: bool) -> Result<NotificationStatus> {
        let conn = self.conn();
        let (pending, oldest_pending_at, last_delivered_at) = conn.query_row(
            "SELECT COUNT(CASE WHEN delivered_at IS NULL THEN 1 END),
                    MIN(CASE WHEN delivered_at IS NULL THEN created_at END), MAX(delivered_at)
             FROM notifications",
            [],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
        )?;
        let last_error = conn.query_row(
            "SELECT last_error FROM notifications WHERE delivered_at IS NULL AND last_error IS NOT NULL
             ORDER BY next_attempt_at DESC LIMIT 1", [], |row| row.get(0),
        ).optional()?.flatten();
        Ok(NotificationStatus {
            enabled,
            pending,
            oldest_pending_at,
            last_delivered_at,
            last_error,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn event() -> Notification {
        Notification {
            key: "testnet:swap:accepted".into(),
            text: "Bridge accepted".into(),
            created_at: 100,
        }
    }

    #[test]
    fn deduplicates_persists_leases_retries_and_acknowledges() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("maker.sqlite");
        let store = Store::open(&path).unwrap();
        store.enqueue_notification(&event()).unwrap();
        store.enqueue_notification(&event()).unwrap();
        assert_eq!(store.notification_status(true).unwrap().pending, 1);
        drop(store);
        let store = Store::open(&path).unwrap();
        let other = Store::open(&path).unwrap();
        let first = store.claim_notification(100).unwrap().unwrap();
        assert!(other.claim_notification(100).unwrap().is_none());
        store
            .retry_notification(&first, 100, 20, "Telegram rate limited")
            .unwrap();
        assert!(other.claim_notification(119).unwrap().is_none());
        let next = other.claim_notification(120).unwrap().unwrap();
        assert_eq!(next.attempts, 2);
        // A stale lease cannot acknowledge another worker's attempt.
        store.acknowledge_notification(&first, 120, 3).unwrap();
        assert!(!store.notification_delivered(&first.key).unwrap());
        other.acknowledge_notification(&next, 121, 4).unwrap();
        store.enqueue_notification(&event()).unwrap();
        let status = store.notification_status(true).unwrap();
        assert_eq!(status.pending, 0);
        assert_eq!(status.last_delivered_at, Some(121));
        assert!(status.last_error.is_none());
    }

    #[test]
    fn errors_notify_once_per_failure_episode_and_transactions_roll_back() {
        let store = Store::open(std::path::Path::new(":memory:")).unwrap();
        store.notification_failure("swap", Some(event())).unwrap();
        store.notification_failure("swap", Some(event())).unwrap();
        assert_eq!(store.notification_status(true).unwrap().pending, 1);
        store.notification_failure("swap", None).unwrap();
        store.notification_failure("swap", Some(event())).unwrap();
        assert_eq!(store.notification_status(true).unwrap().pending, 2);
        let mut conn = store.conn();
        let tx = conn.transaction().unwrap();
        insert(&tx, Some(&event())).unwrap();
        tx.execute("UPDATE notification_failures SET active = 0", [])
            .unwrap();
        tx.rollback().unwrap();
        drop(conn);
        assert_eq!(store.notification_status(true).unwrap().pending, 2);
    }

    #[test]
    fn crashed_delivery_is_reclaimed_and_rate_limits_cover_the_whole_queue() {
        let store = Store::open(std::path::Path::new(":memory:")).unwrap();
        store.enqueue_notification(&event()).unwrap();
        let mut second = event();
        second.key = "second".into();
        store.enqueue_notification(&second).unwrap();
        let first = store.claim_notification(100).unwrap().unwrap();
        assert!(store.claim_notification(102).unwrap().is_none());
        let next = store.claim_notification(103).unwrap().unwrap();
        store
            .retry_notification(&next, 103, 100, "Telegram rejected request (HTTP 429)")
            .unwrap();
        assert!(store.claim_notification(160).unwrap().is_none());
        let reclaimed = store.claim_notification(203).unwrap().unwrap();
        assert_eq!(reclaimed.key, first.key);
        assert_eq!(reclaimed.attempts, 2);
    }

    #[test]
    fn forward_swap_and_outbox_changes_roll_back_together() {
        use zecswap_chain::{
            evm::{Address, B256},
            zcash::AccountUuid,
        };
        use zecswap_core::{SecretShare, ViewingKeys};
        let store = Store::open(std::path::Path::new(":memory:")).unwrap();
        store
            .insert_quote([1; 32], Address::repeat_byte(2), None, 1234567, 100000, 200)
            .unwrap();
        let quote = store.take_quote(&[1; 32], 100).unwrap().unwrap();
        let swap = super::super::Swap {
            id: B256::repeat_byte(3),
            quote,
            user_share: SecretShare::random(rand::rand_core::UnwrapErr(rand::rngs::SysRng))
                .public(),
            viewing: ViewingKeys::random(rand::rand_core::UnwrapErr(rand::rngs::SysRng)),
            zcash_account: AccountUuid::from_uuid(uuid::Uuid::nil()),
            opened_at: 100,
            t1: 300,
            sweep: None,
            settled: false,
            refund_started: false,
        };
        store.conn().execute_batch("CREATE TRIGGER reject_alert BEFORE INSERT ON notifications WHEN NEW.event_key = 'reject' BEGIN SELECT RAISE(FAIL, 'injected queue failure'); END;").unwrap();
        let bad = Notification {
            key: "reject".into(),
            ..event()
        };
        assert!(store.insert_swap(&swap, Some(&bad)).is_err());
        assert!(store.swap(&swap.id).unwrap().is_none());
        store.insert_swap(&swap, Some(&event())).unwrap();
        assert!(store.settle(&swap.id, Some(&bad)).is_err());
        assert!(!store.swap(&swap.id).unwrap().unwrap().settled);
        let finished = Notification {
            key: "finished".into(),
            ..event()
        };
        store.settle(&swap.id, Some(&finished)).unwrap();
        assert!(store.swap(&swap.id).unwrap().unwrap().settled);
        assert_eq!(store.notification_status(true).unwrap().pending, 2);
    }
}
