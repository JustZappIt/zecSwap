//! Messages wait in the service's own SQLite database, queued in the same transaction as what
//! they report, under a key that never queues one twice. Delivery leases the oldest due message,
//! sends at most one every three seconds, and retries outages with backoff and rate limits after
//! the delay Telegram asks for.

use std::sync::{Mutex, PoisonError};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use rusqlite::{Connection, OptionalExtension, params};
use serde::Serialize;

use crate::Telegram;

pub const SCHEMA: &str = "
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
    CREATE TABLE IF NOT EXISTS notification_delivery (
        singleton INTEGER PRIMARY KEY CHECK (singleton = 1),
        next_send_at INTEGER NOT NULL DEFAULT 0
    );
    INSERT OR IGNORE INTO notification_delivery (singleton) VALUES (1);
";

pub struct Notification {
    pub key: String,
    pub text: String,
    pub created_at: u64,
}

struct Delivery {
    key: String,
    text: String,
    attempts: u32,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub struct NotificationStatus {
    pub enabled: bool,
    pub pending: u64,
    pub oldest_pending_at: Option<u64>,
    pub last_delivered_at: Option<u64>,
    pub last_error: Option<String>,
}

pub fn insert(conn: &Connection, event: Option<&Notification>) -> rusqlite::Result<()> {
    if let Some(event) = event {
        conn.execute(
            "INSERT INTO notifications (event_key, text, created_at, next_attempt_at)
             VALUES (?1, ?2, ?3, ?3) ON CONFLICT(event_key) DO NOTHING",
            params![event.key, event.text, event.created_at],
        )?;
    }
    Ok(())
}

pub fn delivered(conn: &Connection, key: &str) -> rusqlite::Result<bool> {
    conn.query_row(
        "SELECT delivered_at IS NOT NULL FROM notifications WHERE event_key = ?1",
        [key],
        |row| row.get(0),
    )
}

pub fn status(conn: &Connection, enabled: bool) -> rusqlite::Result<NotificationStatus> {
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

/// Sends the oldest due message, if there is one. The database is never locked while Telegram
/// is asked.
pub async fn deliver(db: &Mutex<Connection>, telegram: &Telegram) -> anyhow::Result<()> {
    let lock = || db.lock().unwrap_or_else(PoisonError::into_inner);
    let Some(delivery) = claim(&mut lock(), now())? else {
        return Ok(());
    };
    match telegram.send(&delivery.text).await {
        Ok(message_id) => acknowledge(&lock(), &delivery, now(), message_id)?,
        Err(error) => {
            let delay = error
                .retry_after
                .unwrap_or(
                    5u64.saturating_mul(1u64 << delivery.attempts.min(6))
                        .min(300),
                )
                .max(3);
            retry(&mut lock(), &delivery, now(), delay, &error.message)?;
            tracing::warn!("{}; alert queued for retry", error.message);
        }
    }
    Ok(())
}

/// Delivers the queue for as long as the service runs; with alerts off, it waits forever.
pub async fn run(db: &Mutex<Connection>, telegram: &Telegram) {
    if !telegram.enabled() {
        std::future::pending::<()>().await;
    }
    loop {
        if deliver(db, telegram).await.is_err() {
            tracing::warn!("Telegram delivery queue unavailable; retrying");
        }
        // Also stays below Telegram's per-chat flood limit.
        tokio::time::sleep(Duration::from_secs(3)).await;
    }
}

fn claim(conn: &mut Connection, now: u64) -> rusqlite::Result<Option<Delivery>> {
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

fn acknowledge(
    conn: &Connection,
    delivery: &Delivery,
    now: u64,
    message_id: i64,
) -> rusqlite::Result<()> {
    conn.execute(
        "UPDATE notifications SET delivered_at = ?2, message_id = ?3, last_error = NULL
         WHERE event_key = ?1 AND delivered_at IS NULL AND attempts = ?4",
        params![delivery.key, now, message_id, delivery.attempts],
    )?;
    Ok(())
}

fn retry(
    conn: &mut Connection,
    delivery: &Delivery,
    now: u64,
    delay: u64,
    error: &str,
) -> rusqlite::Result<()> {
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
    tx.execute(
        "UPDATE notification_delivery SET next_send_at = MAX(next_send_at, ?1) WHERE singleton = 1",
        [now.saturating_add(delay)],
    )?;
    tx.commit()
}

fn now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("clock is after 1970")
        .as_secs()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn open(path: &std::path::Path) -> Connection {
        let conn = Connection::open(path).unwrap();
        conn.execute_batch(SCHEMA).unwrap();
        conn
    }

    fn event() -> Notification {
        Notification {
            key: "testnet:swap:accepted".into(),
            text: "Bridge accepted".into(),
            created_at: 100,
        }
    }

    fn pending(conn: &Connection) -> u64 {
        status(conn, true).unwrap().pending
    }

    #[test]
    fn deduplicates_persists_leases_retries_and_acknowledges() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("alerts.sqlite");
        let conn = open(&path);
        insert(&conn, Some(&event())).unwrap();
        insert(&conn, Some(&event())).unwrap();
        assert_eq!(pending(&conn), 1);
        drop(conn);
        let mut conn = open(&path);
        let mut other = open(&path);
        let first = claim(&mut conn, 100).unwrap().unwrap();
        assert!(claim(&mut other, 100).unwrap().is_none());
        retry(&mut conn, &first, 100, 20, "Telegram rate limited").unwrap();
        assert!(claim(&mut other, 119).unwrap().is_none());
        let next = claim(&mut other, 120).unwrap().unwrap();
        assert_eq!(next.attempts, 2);
        // A stale lease cannot acknowledge another worker's attempt.
        acknowledge(&conn, &first, 120, 3).unwrap();
        assert!(!delivered(&conn, &first.key).unwrap());
        acknowledge(&other, &next, 121, 4).unwrap();
        insert(&conn, Some(&event())).unwrap();
        let status = status(&conn, true).unwrap();
        assert_eq!(status.pending, 0);
        assert_eq!(status.last_delivered_at, Some(121));
        assert!(status.last_error.is_none());
    }

    #[test]
    fn crashed_delivery_is_reclaimed_and_rate_limits_cover_the_whole_queue() {
        let mut conn = open(std::path::Path::new(":memory:"));
        insert(&conn, Some(&event())).unwrap();
        let second = Notification {
            key: "second".into(),
            ..event()
        };
        insert(&conn, Some(&second)).unwrap();
        let first = claim(&mut conn, 100).unwrap().unwrap();
        assert!(claim(&mut conn, 102).unwrap().is_none());
        let next = claim(&mut conn, 103).unwrap().unwrap();
        retry(
            &mut conn,
            &next,
            103,
            100,
            "Telegram rejected request (HTTP 429)",
        )
        .unwrap();
        assert!(claim(&mut conn, 160).unwrap().is_none());
        let reclaimed = claim(&mut conn, 203).unwrap().unwrap();
        assert_eq!(reclaimed.key, first.key);
        assert_eq!(reclaimed.attempts, 2);
    }

    #[test]
    fn nothing_is_queued_while_alerts_are_off() {
        let conn = open(std::path::Path::new(":memory:"));
        insert(&conn, None).unwrap();
        assert_eq!(pending(&conn), 0);
    }
}
