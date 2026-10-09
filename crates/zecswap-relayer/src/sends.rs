//! Private Railgun sends and withdrawals, sent as the wallet's broadcaster for a fee note to the
//! relayer's own 0zk address. Each is recorded before it is broadcast, so posting the same bytes
//! again never sends twice, and a proof whose notes an earlier send spends is told so. Once it
//! mines, what it cost and earned is recorded beside it, for the monitor.

use std::collections::HashMap;
use std::path::Path;
use std::sync::{Arc, Mutex};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use alloy_primitives::{Address, B256};
use rusqlite::{Connection, OptionalExtension, Row, params};
use serde::Serialize;
use tracing::warn;
use zecswap_api::relayer::{RailgunTransact, Sent};
use zecswap_chain::evm::railgun::{SendError, SendPolicy, Signed, Status};
use zecswap_prices::Asset;
use zecswap_railgun::Keys;

use crate::{Relayer, RelayerError, Result};

/// How deep the block must be that took a recorded transaction's nonce for another, before the
/// recorded one counts as never landing: deeper than any reorg or lagging node.
const SETTLED_DEPTH: u64 = 12;
/// Seconds before a recorded transaction no node knows is broadcast again.
const REBROADCAST_AFTER: u64 = 60;
/// Seconds between looks for the receipt of a send this long unmined: a dropped one never mines.
const UNMINED_RECHECK: u64 = 600;
/// The most sends one export carries; older ones need a later `since`.
const LEDGER_LIMIT: usize = 1000;

pub(crate) struct Sends {
    pub(crate) policy: SendPolicy,
    pub(crate) keys: Keys,
    journal: Journal,
    /// One send at a time, so a post repeated while the first runs finds it recorded.
    one_at_a_time: tokio::sync::Mutex<()>,
    /// When each long-unmined send's receipt was last looked for.
    unmined: Mutex<HashMap<B256, u64>>,
}

/// What a post came to: sent now, or sent before from the same bytes.
pub enum Sending {
    New(Sent),
    Again(Sent),
}

/// What became of a recorded send.
enum Fate {
    /// Pending, or mined and spending its notes.
    Sent,
    /// Mined, and spent nothing.
    Reverted,
    /// Can never land: its nonce went to another transaction.
    Dead,
    /// Not known yet.
    Unknown,
}

impl Sends {
    pub(crate) fn open(policy: SendPolicy, keys: Keys, journal: &Path) -> anyhow::Result<Self> {
        Ok(Self {
            policy,
            keys,
            journal: Journal::open(journal)?,
            one_at_a_time: tokio::sync::Mutex::new(()),
            unmined: Mutex::default(),
        })
    }

    /// What a recorded send was and paid this relayer: a withdrawal or a private send, and the
    /// fee its notes carry, read again from its bytes; none if they no longer pass the policy.
    fn paid(&self, chain_id: u64, signed: &Signed) -> Option<(&'static str, u128)> {
        let (to, value, data) = signed.call()?;
        let transact = self.policy.decode(chain_id, to, value, data).ok()?;
        let kind = if transact.unshields() {
            "unshield"
        } else {
            "send"
        };
        Some((kind, transact.fee_paid(&self.keys, self.policy.token)))
    }
}

/// Whether `paid` covers `gas` at `gas_price` wei, at `rate` token base units per 10^18 wei.
pub(crate) fn covers(paid: u128, rate: Option<u128>, gas: u64, gas_price: u128) -> bool {
    let Some(rate) = rate else {
        return true;
    };
    let cost = u128::from(gas)
        .checked_mul(gas_price)
        .and_then(|wei| wei.checked_mul(rate));
    match (cost, paid.checked_mul(10u128.pow(18))) {
        (Some(cost), Some(paid)) => paid >= cost,
        (Some(_), None) => true,
        (None, _) => false,
    }
}

/// What the relayer's sends since a time cost and earned, newest first.
#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SendsSnapshot {
    schema_version: u32,
    generated_at: u64,
    since: u64,
    relayer: Address,
    chain_id: u64,
    /// The token fees are paid in, and what each send must pay now.
    token: Address,
    fee: String,
    /// The gas-based part's rate now and its margin, where sends are priced by gas.
    fee_per_unit_gas: Option<String>,
    fee_margin_bps: Option<u32>,
    max_gas_limit: u64,
    max_gas_price_wei: String,
    limit: usize,
    truncated: bool,
    sends: Vec<SendRecord>,
}

/// A send, and once it mined, what it burned and was paid; USD values at its block's time,
/// from Alchemy's history, once read.
#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SendRecord {
    transaction_hash: B256,
    sent_at: u64,
    /// `send` (0zk to 0zk) or `unshield` (to a public address).
    kind: Option<String>,
    /// Token base units its fee notes pay the relayer: earned only if it succeeded.
    fee: Option<String>,
    succeeded: Option<bool>,
    block_number: Option<u64>,
    block_time: Option<u64>,
    gas_used: Option<u64>,
    gas_price_wei: Option<String>,
    eth_usd: Option<String>,
    token_usd: Option<String>,
}

impl Relayer {
    /// Records each send's receipt once it has mined and values its gas and fee, a bounded
    /// number a pass, for as long as the relayer runs.
    pub async fn run_costs(self: Arc<Self>) {
        if self.sends.is_none() {
            return;
        }
        loop {
            let failure =
                match tokio::time::timeout(Duration::from_secs(60), self.cost_pass()).await {
                    Ok(Ok(())) => None,
                    Ok(Err(error)) => Some(
                        error
                            .downcast_ref()
                            .map_or("internal", crate::monitor::failure_kind),
                    ),
                    Err(_) => Some("timeout"),
                };
            if let Some(failure_kind) = failure {
                // RPC errors can carry the node's URL: log the kind alone.
                warn!(
                    operation = "send_costs",
                    failure_kind, "recording send costs failed; retrying"
                );
            }
            tokio::time::sleep(Duration::from_secs(30)).await;
        }
    }

    pub(crate) async fn cost_pass(&self) -> anyhow::Result<()> {
        let Some(sends) = &self.sends else {
            return Ok(());
        };
        let chain_id = self.domain.chain_id;
        let now = now();
        for entry in sends.journal.unread(chain_id, 20)? {
            let hash = entry.signed.hash;
            let looked = sends.unmined.lock().unwrap().get(&hash).copied();
            if looked.is_some_and(|at| now.saturating_sub(at) < UNMINED_RECHECK) {
                continue;
            }
            let Some(facts) = self
                .settlement
                .transaction_facts(hash, sends.policy.token)
                .await?
            else {
                if now.saturating_sub(entry.sent_at) >= UNMINED_RECHECK {
                    sends.unmined.lock().unwrap().insert(hash, now);
                }
                continue;
            };
            sends.unmined.lock().unwrap().remove(&hash);
            let Some(block_time) = self.settlement.block_time(facts.block_hash).await? else {
                continue;
            };
            let (kind, fee) = sends.paid(chain_id, &entry.signed).unzip();
            sends.journal.save_cost(
                chain_id,
                hash,
                &Cost {
                    kind,
                    fee,
                    succeeded: facts.succeeded,
                    block_number: facts.block_number,
                    block_time,
                    gas_used: facts.gas_used,
                    gas_price: facts.gas_price,
                },
            )?;
        }
        if let Some(history) = &self.history {
            for (hash, at) in sends.journal.unvalued(chain_id, 10)? {
                // The fee token is the relayer's USDC.
                match (
                    history.usd_at(Asset::Eth, at).await,
                    history.usd_at(Asset::Usdc, at).await,
                ) {
                    (Ok(eth), Ok(token)) => sends.journal.value(chain_id, hash, &eth, &token)?,
                    (Err(error), _) | (_, Err(error)) => {
                        warn!(
                            operation = "send_costs",
                            "price history unavailable: {error}"
                        );
                        break;
                    }
                }
            }
        }
        Ok(())
    }

    /// The sends since `since` (30 days ago if none), with what each cost and earned; none
    /// where the relayer sends no Railgun transactions.
    pub fn sends_snapshot(&self, since: Option<u64>) -> anyhow::Result<Option<SendsSnapshot>> {
        let Some(sends) = &self.sends else {
            return Ok(None);
        };
        let generated_at = now();
        let since = since.unwrap_or(generated_at.saturating_sub(30 * 86_400));
        let (records, truncated) =
            sends
                .journal
                .ledger(self.domain.chain_id, since, LEDGER_LIMIT)?;
        Ok(Some(SendsSnapshot {
            schema_version: 1,
            generated_at,
            since,
            relayer: self.account,
            chain_id: self.domain.chain_id,
            token: sends.policy.token,
            fee: sends.policy.fee.to_string(),
            fee_per_unit_gas: self
                .pricing
                .as_ref()
                .and_then(|pricing| pricing.rate(generated_at))
                .map(|rate| rate.to_string()),
            fee_margin_bps: self.pricing.as_ref().map(|pricing| pricing.margin_bps),
            max_gas_limit: sends.policy.max_gas_limit,
            max_gas_price_wei: sends.policy.max_gas_price_wei.to_string(),
            limit: LEDGER_LIMIT,
            truncated,
            sends: records,
        }))
    }
}

impl Relayer {
    #[tracing::instrument(skip_all, fields(operation = "railgun_transact"), err(level = "warn"))]
    pub async fn railgun_transact(&self, request: RailgunTransact) -> Result<Sending> {
        let sends = self.sends.as_ref().ok_or_else(|| {
            RelayerError::Rejected("this relayer sends no Railgun transactions".into())
        })?;
        let chain_id = self.domain.chain_id;
        if request.chain_id != chain_id {
            return Err(RelayerError::Rejected("wrong chain".into()));
        }
        let transact = sends
            .policy
            .decode(chain_id, request.to, request.value, request.data)
            .map_err(refused)?;
        let rate = match &self.pricing {
            Some(pricing) => Some(
                pricing
                    .honored_rate(now())
                    .await
                    .ok_or(RelayerError::Unpriced)?,
            ),
            None => None,
        };
        let _one = sends.one_at_a_time.lock().await;
        let id = transact.id();
        if let Some(entry) = sends.journal.find(chain_id, id)? {
            match self.fate(&entry).await? {
                Fate::Sent | Fate::Reverted => {
                    return Ok(Sending::Again(Sent {
                        transactions: vec![entry.signed.hash],
                    }));
                }
                Fate::Dead => sends.journal.forget(chain_id, id)?,
                Fate::Unknown => return Err(unsettled()),
            }
        }
        let nullifiers = transact.nullifiers();
        let (mut spending, mut unknown) = (Vec::new(), false);
        for entry in sends.journal.spending(chain_id, &nullifiers)? {
            match self.fate(&entry).await? {
                Fate::Sent => spending.push(entry.signed.hash),
                Fate::Reverted => {}
                Fate::Dead => sends.journal.forget(chain_id, entry.id)?,
                Fate::Unknown => unknown = true,
            }
        }
        if !spending.is_empty() {
            return Err(RelayerError::Spent(spending));
        }
        if unknown {
            return Err(unsettled());
        }
        if self.settlement.spends_spent_notes(&transact).await? {
            return Err(RelayerError::Spent(Vec::new()));
        }
        let paid = sends
            .policy
            .check(&transact, &sends.keys)
            .map_err(refused)?;
        let tx = self
            .settlement
            .send_transact(
                &sends.policy,
                &transact,
                |gas, gas_price| covers(paid, rate, gas, gas_price),
                |signed| {
                    sends
                        .journal
                        .record(chain_id, id, &nullifiers, signed)
                        .map_err(|e| zecswap_chain::Error::Journal(e.to_string()))
                },
            )
            .await
            .map_err(refused)?;
        Ok(Sending::New(Sent {
            transactions: vec![tx],
        }))
    }

    async fn fate(&self, entry: &Entry) -> Result<Fate> {
        let chain = &self.settlement;
        Ok(match chain.transaction_status(entry.signed.hash).await? {
            Status::Pending | Status::Mined { succeeded: true } => Fate::Sent,
            Status::Mined { succeeded: false } => Fate::Reverted,
            Status::Unknown if chain.nonce_taken(entry.signed.nonce, SETTLED_DEPTH).await? => {
                Fate::Dead
            }
            Status::Unknown
                if now().saturating_sub(entry.sent_at) >= REBROADCAST_AFTER
                    && chain.rebroadcast(&entry.signed).await? =>
            {
                Fate::Sent
            }
            Status::Unknown => Fate::Unknown,
        })
    }
}

fn refused(error: SendError) -> RelayerError {
    match error {
        SendError::Rejected(reason) => RelayerError::Rejected(reason.into()),
        SendError::Spent => RelayerError::Spent(Vec::new()),
        SendError::Chain(error) => error.into(),
        SendError::Unknown(_) => unsettled(),
    }
}

fn unsettled() -> RelayerError {
    RelayerError::Unsettled(
        "a send of these notes has no known outcome yet; post the same request again later",
    )
}

/// Every send, recorded before its broadcast: the signed transaction, and the notes it spends;
/// and once it mined, what it cost and earned.
struct Journal(Mutex<Connection>);

/// A mined send, as its receipt and its own bytes tell it.
struct Cost {
    kind: Option<&'static str>,
    fee: Option<u128>,
    succeeded: bool,
    block_number: u64,
    block_time: u64,
    gas_used: u64,
    gas_price: u128,
}

struct Entry {
    id: B256,
    signed: Signed,
    sent_at: u64,
}

impl Journal {
    fn open(path: &Path) -> rusqlite::Result<Self> {
        let db = Connection::open(path)?;
        db.execute_batch(
            "PRAGMA foreign_keys = ON;
             CREATE TABLE IF NOT EXISTS sends (
                 chain_id INTEGER NOT NULL,
                 id BLOB NOT NULL,
                 tx_hash BLOB NOT NULL,
                 nonce INTEGER NOT NULL,
                 raw BLOB NOT NULL,
                 sent_at INTEGER NOT NULL,
                 PRIMARY KEY (chain_id, id)
             );
             CREATE TABLE IF NOT EXISTS spends (
                 chain_id INTEGER NOT NULL,
                 id BLOB NOT NULL,
                 tree INTEGER NOT NULL,
                 nullifier BLOB NOT NULL,
                 PRIMARY KEY (chain_id, id, tree, nullifier),
                 FOREIGN KEY (chain_id, id) REFERENCES sends (chain_id, id) ON DELETE CASCADE
             );
             CREATE INDEX IF NOT EXISTS spends_by_note ON spends (chain_id, tree, nullifier);
             CREATE TABLE IF NOT EXISTS send_costs (
                 chain_id INTEGER NOT NULL,
                 tx_hash BLOB NOT NULL,
                 kind TEXT,
                 fee TEXT,
                 succeeded INTEGER NOT NULL,
                 block_number INTEGER NOT NULL,
                 block_time INTEGER NOT NULL,
                 gas_used INTEGER NOT NULL,
                 gas_price TEXT NOT NULL,
                 eth_usd TEXT,
                 token_usd TEXT,
                 PRIMARY KEY (chain_id, tx_hash)
             );",
        )?;
        Ok(Self(Mutex::new(db)))
    }

    fn find(&self, chain_id: u64, id: B256) -> rusqlite::Result<Option<Entry>> {
        self.0
            .lock()
            .unwrap()
            .query_row(
                "SELECT id, tx_hash, nonce, raw, sent_at FROM sends WHERE chain_id = ?1 AND id = ?2",
                params![chain_id as i64, id.as_slice()],
                entry,
            )
            .optional()
    }

    /// The recorded sends spending any of `nullifiers`.
    fn spending(&self, chain_id: u64, nullifiers: &[(u16, B256)]) -> rusqlite::Result<Vec<Entry>> {
        let db = self.0.lock().unwrap();
        let mut query = db.prepare(
            "SELECT DISTINCT s.id, s.tx_hash, s.nonce, s.raw, s.sent_at FROM spends p
             JOIN sends s ON s.chain_id = p.chain_id AND s.id = p.id
             WHERE p.chain_id = ?1 AND p.tree = ?2 AND p.nullifier = ?3",
        )?;
        let mut entries: Vec<Entry> = Vec::new();
        for (tree, nullifier) in nullifiers {
            for found in
                query.query_map(params![chain_id as i64, tree, nullifier.as_slice()], entry)?
            {
                let found = found?;
                if entries.iter().all(|known| known.id != found.id) {
                    entries.push(found);
                }
            }
        }
        Ok(entries)
    }

    fn record(
        &self,
        chain_id: u64,
        id: B256,
        nullifiers: &[(u16, B256)],
        signed: &Signed,
    ) -> rusqlite::Result<()> {
        let mut db = self.0.lock().unwrap();
        let tx = db.transaction()?;
        tx.execute(
            "INSERT INTO sends (chain_id, id, tx_hash, nonce, raw, sent_at)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
            params![
                chain_id as i64,
                id.as_slice(),
                signed.hash.as_slice(),
                signed.nonce as i64,
                signed.raw.as_ref(),
                now() as i64,
            ],
        )?;
        for (tree, nullifier) in nullifiers {
            tx.execute(
                "INSERT OR IGNORE INTO spends (chain_id, id, tree, nullifier) VALUES (?1, ?2, ?3, ?4)",
                params![chain_id as i64, id.as_slice(), tree, nullifier.as_slice()],
            )?;
        }
        tx.commit()
    }

    /// Sends whose receipt hasn't been read, newest first.
    fn unread(&self, chain_id: u64, limit: usize) -> rusqlite::Result<Vec<Entry>> {
        let db = self.0.lock().unwrap();
        let mut query = db.prepare(
            "SELECT s.id, s.tx_hash, s.nonce, s.raw, s.sent_at FROM sends s
             WHERE s.chain_id = ?1 AND NOT EXISTS (
                 SELECT 1 FROM send_costs c WHERE c.chain_id = s.chain_id AND c.tx_hash = s.tx_hash
             ) ORDER BY s.sent_at DESC LIMIT ?2",
        )?;
        query
            .query_map(params![chain_id as i64, limit as i64], entry)?
            .collect()
    }

    fn save_cost(&self, chain_id: u64, hash: B256, cost: &Cost) -> rusqlite::Result<()> {
        self.0.lock().unwrap().execute(
            "INSERT OR IGNORE INTO send_costs (chain_id, tx_hash, kind, fee, succeeded, block_number,
                 block_time, gas_used, gas_price) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)",
            params![
                chain_id as i64,
                hash.as_slice(),
                cost.kind,
                cost.fee.map(|fee| fee.to_string()),
                cost.succeeded,
                cost.block_number as i64,
                cost.block_time as i64,
                cost.gas_used as i64,
                cost.gas_price.to_string(),
            ],
        )?;
        Ok(())
    }

    /// Costs not yet valued, newest first, with their block's time.
    fn unvalued(&self, chain_id: u64, limit: usize) -> rusqlite::Result<Vec<(B256, u64)>> {
        let db = self.0.lock().unwrap();
        let mut query = db.prepare(
            "SELECT tx_hash, block_time FROM send_costs WHERE chain_id = ?1 AND eth_usd IS NULL
             ORDER BY block_time DESC LIMIT ?2",
        )?;
        query
            .query_map(params![chain_id as i64, limit as i64], |row| {
                Ok((word(row, 0)?, row.get::<_, i64>(1)? as u64))
            })?
            .collect()
    }

    fn value(
        &self,
        chain_id: u64,
        hash: B256,
        eth_usd: &str,
        token_usd: &str,
    ) -> rusqlite::Result<()> {
        self.0.lock().unwrap().execute(
            "UPDATE send_costs SET eth_usd = ?3, token_usd = ?4
             WHERE chain_id = ?1 AND tx_hash = ?2 AND eth_usd IS NULL",
            params![chain_id as i64, hash.as_slice(), eth_usd, token_usd],
        )?;
        Ok(())
    }

    /// Sends recorded since `since`, newest first, at most `limit`, and whether more were left out.
    fn ledger(
        &self,
        chain_id: u64,
        since: u64,
        limit: usize,
    ) -> rusqlite::Result<(Vec<SendRecord>, bool)> {
        let db = self.0.lock().unwrap();
        let mut query = db.prepare(
            "SELECT s.tx_hash, s.sent_at, c.kind, c.fee, c.succeeded, c.block_number, c.block_time,
                 c.gas_used, c.gas_price, c.eth_usd, c.token_usd
             FROM sends s LEFT JOIN send_costs c ON c.chain_id = s.chain_id AND c.tx_hash = s.tx_hash
             WHERE s.chain_id = ?1 AND s.sent_at >= ?2 ORDER BY s.sent_at DESC LIMIT ?3",
        )?;
        let unsigned = |row: &Row<'_>, index| -> rusqlite::Result<Option<u64>> {
            Ok(row.get::<_, Option<i64>>(index)?.map(|value| value as u64))
        };
        let mut records = query
            .query_map(
                params![chain_id as i64, since as i64, limit as i64 + 1],
                |row| {
                    Ok(SendRecord {
                        transaction_hash: word(row, 0)?,
                        sent_at: row.get::<_, i64>(1)? as u64,
                        kind: row.get(2)?,
                        fee: row.get(3)?,
                        succeeded: row.get(4)?,
                        block_number: unsigned(row, 5)?,
                        block_time: unsigned(row, 6)?,
                        gas_used: unsigned(row, 7)?,
                        gas_price_wei: row.get(8)?,
                        eth_usd: row.get(9)?,
                        token_usd: row.get(10)?,
                    })
                },
            )?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        let truncated = records.len() > limit;
        records.truncate(limit);
        Ok((records, truncated))
    }

    fn forget(&self, chain_id: u64, id: B256) -> rusqlite::Result<()> {
        self.0.lock().unwrap().execute(
            "DELETE FROM sends WHERE chain_id = ?1 AND id = ?2",
            params![chain_id as i64, id.as_slice()],
        )?;
        Ok(())
    }
}

fn word(row: &Row<'_>, index: usize) -> rusqlite::Result<B256> {
    let bytes: Vec<u8> = row.get(index)?;
    B256::try_from(bytes.as_slice()).map_err(|e| {
        rusqlite::Error::FromSqlConversionFailure(index, rusqlite::types::Type::Blob, e.into())
    })
}

fn entry(row: &Row<'_>) -> rusqlite::Result<Entry> {
    Ok(Entry {
        id: word(row, 0)?,
        signed: Signed {
            hash: word(row, 1)?,
            nonce: row.get::<_, i64>(2)? as u64,
            raw: row.get::<_, Vec<u8>>(3)?.into(),
        },
        sent_at: row.get::<_, i64>(4)? as u64,
    })
}

pub(crate) fn now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("clock is after 1970")
        .as_secs()
}

#[cfg(test)]
#[path = "costs_tests.rs"]
mod costs_tests;
