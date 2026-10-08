//! Private Railgun sends and withdrawals, sent as the wallet's broadcaster for a fee note to the
//! relayer's own 0zk address. Each is recorded before it is broadcast, so posting the same bytes
//! again never sends twice, and a proof whose notes an earlier send spends is told so.

use std::path::Path;
use std::sync::Mutex;
use std::time::{SystemTime, UNIX_EPOCH};

use alloy_primitives::B256;
use rusqlite::{Connection, OptionalExtension, Row, params};
use zecswap_api::relayer::{RailgunTransact, Sent};
use zecswap_chain::evm::railgun::{SendError, SendPolicy, Signed, Status};
use zecswap_railgun::Keys;

use crate::{Relayer, RelayerError, Result};

/// How deep the block must be that took a recorded transaction's nonce for another, before the
/// recorded one counts as never landing: deeper than any reorg or lagging node.
const SETTLED_DEPTH: u64 = 12;
/// Seconds before a recorded transaction no node knows is broadcast again.
const REBROADCAST_AFTER: u64 = 60;

pub(crate) struct Sends {
    pub(crate) policy: SendPolicy,
    pub(crate) keys: Keys,
    journal: Journal,
    /// One send at a time, so a post repeated while the first runs finds it recorded.
    one_at_a_time: tokio::sync::Mutex<()>,
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
        })
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
        sends
            .policy
            .check(&transact, &sends.keys)
            .map_err(refused)?;
        let tx = self
            .settlement
            .send_transact(&sends.policy, &transact, |signed| {
                sends
                    .journal
                    .record(chain_id, id, &nullifiers, signed)
                    .map_err(|e| zecswap_chain::Error::Journal(e.to_string()))
            })
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

/// Every send, recorded before its broadcast: the signed transaction, and the notes it spends.
struct Journal(Mutex<Connection>);

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
             CREATE INDEX IF NOT EXISTS spends_by_note ON spends (chain_id, tree, nullifier);",
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

    fn forget(&self, chain_id: u64, id: B256) -> rusqlite::Result<()> {
        self.0.lock().unwrap().execute(
            "DELETE FROM sends WHERE chain_id = ?1 AND id = ?2",
            params![chain_id as i64, id.as_slice()],
        )?;
        Ok(())
    }
}

fn entry(row: &Row<'_>) -> rusqlite::Result<Entry> {
    let word = |index| -> rusqlite::Result<B256> {
        let bytes: Vec<u8> = row.get(index)?;
        B256::try_from(bytes.as_slice()).map_err(|e| {
            rusqlite::Error::FromSqlConversionFailure(index, rusqlite::types::Type::Blob, e.into())
        })
    };
    Ok(Entry {
        id: word(0)?,
        signed: Signed {
            hash: word(1)?,
            nonce: row.get::<_, i64>(2)? as u64,
            raw: row.get::<_, Vec<u8>>(3)?.into(),
        },
        sent_at: row.get::<_, i64>(4)? as u64,
    })
}

fn now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("clock is after 1970")
        .as_secs()
}
