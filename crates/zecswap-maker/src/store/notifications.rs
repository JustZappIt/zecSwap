use std::sync::Mutex;

use anyhow::Result;
use rusqlite::{Connection, OptionalExtension};
pub(super) use zecswap_alerts::insert;
use zecswap_alerts::{Notification, NotificationStatus};

use super::Store;

/// The maker's failure and gas episodes; the queue itself is `zecswap_alerts::SCHEMA`.
pub(super) const SCHEMA: &str = "
    CREATE TABLE IF NOT EXISTS notification_failures (
        scope TEXT PRIMARY KEY,
        active INTEGER NOT NULL,
        generation INTEGER NOT NULL
    );
";

impl Store {
    /// The database the alert queue is in, for its delivery.
    pub(crate) fn outbox(&self) -> &Mutex<Connection> {
        &self.0
    }

    pub(crate) fn enqueue_notification(&self, event: &Notification) -> Result<()> {
        Ok(insert(&self.conn(), Some(event))?)
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

    pub(crate) fn notification_delivered(&self, key: &str) -> Result<bool> {
        Ok(zecswap_alerts::delivered(&self.conn(), key)?)
    }

    pub(crate) fn notification_status(&self, enabled: bool) -> Result<NotificationStatus> {
        Ok(zecswap_alerts::status(&self.conn(), enabled)?)
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
            token: Address::repeat_byte(3),
            t0: 200,
            t1: 300,
            sweep: None,
            settled: false,
            refund_started: false,
            token_request: None,
            token_return: None,
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
