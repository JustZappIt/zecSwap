//! A message when the account a service sends from runs low on ETH, and another once it is
//! topped up. Only the account's own sends spend its ETH, so it is read once each of its turns
//! to send ends, and when the service starts. A top-up shows in no send of its own: while the
//! account is low it is read every `interval_seconds` as well, and never otherwise.

use std::sync::{Mutex, PoisonError};
use std::time::Duration;

use rusqlite::{Connection, OptionalExtension};
use serde::{Deserialize, Serialize};
use tracing::{info, warn};
use zecswap_chain::evm::{Address, Settlement, U256};

use crate::outbox::now;
use crate::{Notification, insert};

/// `[gas_alerts]` in a service's config, for the account it sends from.
#[derive(Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct GasAlerts {
    /// Under this many wei the account is low.
    pub low_wei: u64,
    /// From this many wei a low account is topped up; in between, it stays as it was.
    pub recovery_wei: u64,
    /// How often a low account is read until it is topped up.
    pub interval_seconds: u64,
}

impl GasAlerts {
    pub fn check(&self) -> anyhow::Result<()> {
        anyhow::ensure!(
            (60..=3600).contains(&self.interval_seconds),
            "gas alert interval must be 60..3600 seconds"
        );
        anyhow::ensure!(
            self.low_wei > 0 && self.recovery_wei > self.low_wei,
            "gas recovery threshold must exceed a positive low threshold"
        );
        Ok(())
    }

    /// What a monitor says of them, for the account named `label`.
    pub fn status(&self, enabled: bool, label: &str, address: Address) -> GasAlertsStatus {
        GasAlertsStatus {
            enabled,
            interval_seconds: self.interval_seconds,
            accounts: vec![WatchedAccount {
                label: label.into(),
                address,
                low_wei: self.low_wei.to_string(),
                recovery_wei: self.recovery_wei.to_string(),
            }],
        }
    }

    fn low(&self, balance: U256) -> Option<bool> {
        if balance < U256::from(self.low_wei) {
            Some(true)
        } else if balance >= U256::from(self.recovery_wei) {
            Some(false)
        } else {
            None
        }
    }
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub struct GasAlertsStatus {
    enabled: bool,
    interval_seconds: u64,
    accounts: Vec<WatchedAccount>,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct WatchedAccount {
    label: String,
    address: Address,
    low_wei: String,
    recovery_wei: String,
}

/// Watches the ETH of the account `settlement` sends as for as long as the service runs, its
/// messages queued in `db` and naming it `label`.
pub async fn watch(
    db: &Mutex<Connection>,
    settlement: &Settlement,
    alerts: &GasAlerts,
    label: &str,
    network: &str,
    chain_id: u64,
) {
    let Some(account) = settlement.account() else {
        return std::future::pending().await;
    };
    let scope = format!("{network}:{chain_id}:gas:{account}");
    let lock = || db.lock().unwrap_or_else(PoisonError::into_inner);
    loop {
        let read = tokio::time::timeout(Duration::from_secs(5), settlement.eth_balance(account))
            .await
            .ok()
            .and_then(Result::ok);
        let Some(balance) = read else {
            warn!(address = %account, operation = "gas_balance_check", "Gas balance unavailable; preserving previous alert state");
            wait(settlement, alerts, true).await;
            continue;
        };
        let mut settled = true;
        if let Some(low) = alerts.low(balance) {
            let text = if low {
                format!(
                    "ZecSwap {network} · gas low: {label}\n{} ETH, under {} ETH\n{account}",
                    eth(balance),
                    eth(U256::from(alerts.low_wei))
                )
            } else {
                format!(
                    "ZecSwap {network} · gas topped up: {label}\n{} ETH",
                    eth(balance)
                )
            };
            let event = Notification {
                key: String::new(),
                text,
                created_at: now(),
            };
            let queued = episode(&mut lock(), &scope, low, event);
            match queued {
                Ok(true) => {
                    info!(address = %account, balance_wei = %balance, low, operation = "gas_balance_check", "Queued gas balance notification")
                }
                Ok(false) => {}
                Err(_) => {
                    warn!(address = %account, operation = "gas_balance_check", "Could not queue gas balance notification; will retry");
                    settled = false;
                }
            }
        }
        let again = !settled || is_low(&lock(), &scope).unwrap_or(true);
        wait(settlement, alerts, again).await;
    }
}

/// Until the account's next turn to send ends; `again`, for a low account or a failed
/// reading, `interval_seconds` at most.
async fn wait(settlement: &Settlement, alerts: &GasAlerts, again: bool) {
    if again {
        tokio::select! {
            () = settlement.sent() => {}
            () = tokio::time::sleep(Duration::from_secs(alerts.interval_seconds)) => {}
        }
    } else {
        settlement.sent().await;
    }
}

/// Starts or ends the account's low episode with its message, together: the same reading
/// again, or after a restart, queues nothing, and a first reading that is fine says nothing.
/// Whether it queued the message.
fn episode(
    conn: &mut Connection,
    scope: &str,
    low: bool,
    mut event: Notification,
) -> rusqlite::Result<bool> {
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
            "UPDATE notification_failures SET active = 0 WHERE scope = ?1 AND active = 1
             RETURNING generation",
            [scope],
            |row| row.get(0),
        )
        .optional()?
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

fn is_low(conn: &Connection, scope: &str) -> rusqlite::Result<bool> {
    Ok(conn
        .query_row(
            "SELECT active FROM notification_failures WHERE scope = ?1",
            [scope],
            |row| row.get(0),
        )
        .optional()?
        .unwrap_or(false))
}

/// Ether to six decimals, cut rather than rounded so a balance never reads higher than it is.
fn eth(wei: U256) -> String {
    let micro = wei / U256::from(1_000_000_000_000u64);
    let base = U256::from(1_000_000u64);
    let fraction = format!("{:06}", micro % base);
    let fraction = fraction.trim_end_matches('0');
    if fraction.is_empty() {
        (micro / base).to_string()
    } else {
        format!("{}.{fraction}", micro / base)
    }
}

#[cfg(test)]
mod tests;
