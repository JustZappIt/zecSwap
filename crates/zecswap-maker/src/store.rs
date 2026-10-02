mod flow;
mod monitoring;
pub(crate) use flow::{FlowObservation, FlowStatus, ZecTransaction};
mod notifications;
mod reverse;
mod transactions;
pub(crate) use monitoring::{MonitorCounts, MonitorSwap};
pub(crate) use notifications::{Notification, NotificationStatus};
pub(crate) use reverse::ReverseSwap;
pub(crate) use transactions::{EvmTransaction, TransactionStatus};

use std::path::Path;
use std::sync::{Mutex, MutexGuard};

use anyhow::Result;
use rusqlite::types::Type;
use rusqlite::{Connection, OptionalExtension, Row, params};
use uuid::Uuid;
use zecswap_chain::evm::{Address, B256};
use zecswap_chain::zcash::{AccountUuid, TxId};
use zecswap_core::{PublicShare, ViewingKeys};

const SCHEMA: &str = "
    CREATE TABLE IF NOT EXISTS quotes (
        quote_id BLOB PRIMARY KEY,
        nonce INTEGER NOT NULL UNIQUE,
        payout BLOB NOT NULL,
        payout_note BLOB,
        amount TEXT NOT NULL,
        deposit_zat INTEGER NOT NULL,
        expires_at INTEGER NOT NULL,
        accepted INTEGER NOT NULL DEFAULT 0
    );
    CREATE TABLE IF NOT EXISTS swaps (
        id BLOB PRIMARY KEY,
        quote_id BLOB NOT NULL UNIQUE REFERENCES quotes (quote_id),
        user_share BLOB NOT NULL,
        viewing_keys BLOB NOT NULL,
        zcash_account TEXT NOT NULL,
        opened_at INTEGER NOT NULL,
        t1 INTEGER NOT NULL,
        sweep_txid BLOB,
        settled INTEGER NOT NULL DEFAULT 0,
        refund_started INTEGER NOT NULL DEFAULT 0
    );
";

const SWAP_COLUMNS: &str = "
    s.id, s.user_share, s.viewing_keys, s.zcash_account, s.opened_at, s.t1, s.sweep_txid, s.settled,
    q.quote_id, q.nonce, q.payout, q.payout_note, q.amount, q.deposit_zat, s.refund_started
";

#[derive(Clone, Debug)]
pub struct Quote {
    pub id: [u8; 32],
    /// Index of the maker share, derived from the root secret rather than stored.
    pub nonce: u64,
    pub payout: Address,
    /// For a payout into Railgun, the commitment to the note it pays.
    pub payout_note: Option<B256>,
    pub amount: u128,
    pub deposit_zat: u64,
}

/// A swap the maker has sent `open` for, whether or not it has landed.
pub struct Swap {
    pub id: B256,
    pub quote: Quote,
    pub user_share: PublicShare,
    pub viewing: ViewingKeys,
    pub zcash_account: AccountUuid,
    pub opened_at: u64,
    /// The `t1` sent with `open`. `open` fails from `t0` on, so by `t1` the swap is on-chain if
    /// it ever will be.
    pub t1: u64,
    pub sweep: Option<TxId>,
    pub settled: bool,
    /// Persisted before cancellation sends; a reorg must not make a revealed share safe again.
    pub refund_started: bool,
}

pub struct Store(Mutex<Connection>);

impl Store {
    pub fn open(path: &Path) -> Result<Self> {
        let conn = Connection::open(path)?;
        conn.execute_batch(SCHEMA)?;
        let has_refund_started = conn
            .prepare("PRAGMA table_info(swaps)")?
            .query_map([], |row| row.get::<_, String>(1))?
            .collect::<rusqlite::Result<Vec<_>>>()?
            .iter()
            .any(|column| column == "refund_started");
        if !has_refund_started {
            conn.execute_batch(
                "ALTER TABLE swaps ADD COLUMN refund_started INTEGER NOT NULL DEFAULT 0",
            )?;
        }
        conn.execute_batch(reverse::SCHEMA)?;
        conn.execute_batch(notifications::SCHEMA)?;
        conn.execute_batch(transactions::SCHEMA)?;
        conn.execute_batch(flow::SCHEMA)?;
        Ok(Self(Mutex::new(conn)))
    }

    /// Records a quote under the next unused nonce and returns that nonce.
    pub fn insert_quote(
        &self,
        id: [u8; 32],
        payout: Address,
        payout_note: Option<B256>,
        amount: u128,
        deposit_zat: u64,
        expires_at: u64,
    ) -> Result<u64> {
        Ok(self.conn().query_row(
            "INSERT INTO quotes (quote_id, nonce, payout, payout_note, amount, deposit_zat, expires_at)
             VALUES (?1, (SELECT IFNULL(MAX(nonce) + 1, 0) FROM quotes), ?2, ?3, ?4, ?5, ?6)
             RETURNING nonce",
            params![
                id,
                payout.as_slice(),
                payout_note.as_ref().map(B256::as_slice),
                amount.to_string(),
                deposit_zat,
                expires_at
            ],
            |row| row.get(0),
        )?)
    }

    /// Claims a live quote; each can be accepted once.
    pub fn take_quote(&self, id: &[u8; 32], now: u64) -> Result<Option<Quote>> {
        Ok(self
            .conn()
            .query_row(
                "UPDATE quotes SET accepted = 1
                 WHERE quote_id = ?1 AND accepted = 0 AND expires_at > ?2
                   AND NOT EXISTS (SELECT 1 FROM reverse_quotes r WHERE r.quote_id = quotes.quote_id)
                 RETURNING quote_id, nonce, payout, payout_note, amount, deposit_zat",
                params![id, now],
                |row| quote_at(row, 0),
            )
            .optional()?)
    }

    pub fn insert_swap(&self, swap: &Swap, event: Option<&Notification>) -> Result<()> {
        let mut conn = self.conn();
        let tx = conn.transaction()?;
        tx.execute(
            "INSERT INTO swaps (id, quote_id, user_share, viewing_keys, zcash_account, opened_at, t1)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
            params![
                swap.id.as_slice(),
                swap.quote.id,
                swap.user_share.to_affine_bytes(),
                swap.viewing.to_bytes(),
                swap.zcash_account.expose_uuid().to_string(),
                swap.opened_at,
                swap.t1,
            ],
        )?;
        notifications::insert(&tx, event)?;
        tx.commit()?;
        Ok(())
    }

    pub fn swap(&self, id: &B256) -> Result<Option<Swap>> {
        let sql = format!(
            "SELECT {SWAP_COLUMNS} FROM swaps s JOIN quotes q USING (quote_id) WHERE s.id = ?1"
        );
        Ok(self
            .conn()
            .query_row(&sql, params![id.as_slice()], swap_from_row)
            .optional()?)
    }

    pub fn unsettled_swaps(&self) -> Result<Vec<Swap>> {
        self.swaps(false)
    }

    /// Retain and revisit completed swaps so a reorg can resume settlement.
    pub fn watched_swaps(&self) -> Result<Vec<Swap>> {
        self.swaps(true)
    }

    fn swaps(&self, include_settled: bool) -> Result<Vec<Swap>> {
        let sql = format!(
            "SELECT {SWAP_COLUMNS} FROM swaps s JOIN quotes q USING (quote_id)
             WHERE s.settled = 0 OR ?1 ORDER BY s.settled, s.opened_at"
        );
        let conn = self.conn();
        let mut statement = conn.prepare(&sql)?;
        let swaps = statement
            .query_map([include_settled], swap_from_row)?
            .collect::<Result<_, _>>()?;
        Ok(swaps)
    }

    /// Moves a swap's `opened_at` to when its `open` landed.
    pub fn set_opened_at(&self, id: &B256, opened_at: u64) -> Result<()> {
        self.conn().execute(
            "UPDATE swaps SET opened_at = ?2 WHERE id = ?1",
            params![id.as_slice(), opened_at],
        )?;
        Ok(())
    }

    pub fn record_sweep(&self, id: &B256, txid: TxId) -> Result<()> {
        self.conn().execute(
            "UPDATE swaps SET sweep_txid = ?2 WHERE id = ?1",
            params![id.as_slice(), txid.as_ref()],
        )?;
        Ok(())
    }

    pub fn settle(&self, id: &B256, event: Option<&Notification>) -> Result<()> {
        let mut conn = self.conn();
        let tx = conn.transaction()?;
        tx.execute(
            "UPDATE swaps SET settled = 1 WHERE id = ?1",
            params![id.as_slice()],
        )?;
        notifications::insert(&tx, event)?;
        tx.commit()?;
        Ok(())
    }

    pub fn resume(&self, id: &B256) -> Result<()> {
        self.conn().execute(
            "UPDATE swaps SET settled = 0 WHERE id = ?1",
            params![id.as_slice()],
        )?;
        Ok(())
    }

    pub fn start_refund(&self, id: &B256) -> Result<()> {
        self.conn().execute(
            "UPDATE swaps SET refund_started = 1 WHERE id = ?1",
            params![id.as_slice()],
        )?;
        Ok(())
    }

    fn conn(&self) -> MutexGuard<'_, Connection> {
        self.0
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }
}

fn swap_from_row(row: &Row<'_>) -> rusqlite::Result<Swap> {
    let account: String = row.get(3)?;
    let account = Uuid::parse_str(&account).map_err(|e| invalid(3, Type::Text, e))?;
    Ok(Swap {
        id: B256::from(row.get::<_, [u8; 32]>(0)?),
        user_share: PublicShare::from_affine_bytes(&row.get(1)?)
            .map_err(|e| invalid(1, Type::Blob, e))?,
        viewing: ViewingKeys::from_bytes(&row.get(2)?).map_err(|e| invalid(2, Type::Blob, e))?,
        zcash_account: AccountUuid::from_uuid(account),
        opened_at: row.get(4)?,
        t1: row.get(5)?,
        sweep: row.get::<_, Option<[u8; 32]>>(6)?.map(TxId::from_bytes),
        settled: row.get(7)?,
        refund_started: row.get(14)?,
        quote: quote_at(row, 8)?,
    })
}

/// Reads the quote columns starting at `first`, in `quote_id, nonce, payout, payout_note,
/// amount, deposit_zat` order.
fn quote_at(row: &Row<'_>, first: usize) -> rusqlite::Result<Quote> {
    let amount: String = row.get(first + 4)?;
    Ok(Quote {
        id: row.get(first)?,
        nonce: row.get(first + 1)?,
        payout: Address::from(row.get::<_, [u8; 20]>(first + 2)?),
        payout_note: row.get::<_, Option<[u8; 32]>>(first + 3)?.map(B256::from),
        amount: amount
            .parse()
            .map_err(|e| invalid(first + 4, Type::Text, e))?,
        deposit_zat: row.get(first + 5)?,
    })
}

fn invalid(
    column: usize,
    kind: Type,
    e: impl std::error::Error + Send + Sync + 'static,
) -> rusqlite::Error {
    rusqlite::Error::FromSqlConversionFailure(column, kind, Box::new(e))
}
