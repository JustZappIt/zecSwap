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
use std::time::{SystemTime, UNIX_EPOCH};

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
        token BLOB NOT NULL,
        t0 INTEGER NOT NULL,
        t1 INTEGER NOT NULL,
        sweep_txid BLOB,
        settled INTEGER NOT NULL DEFAULT 0,
        refund_started INTEGER NOT NULL DEFAULT 0,
        settled_at INTEGER,
        archived INTEGER NOT NULL DEFAULT 0
    );
";

/// Seconds a settled swap is still re-read, in case a reorganisation undoes its settlement: far
/// longer than either chain takes to make a block final. After that it is archived.
pub(crate) const FINAL_AFTER: u64 = 24 * 60 * 60;
/// SQLite's clock, which records and ages settlements.
pub(crate) const NOW: &str = "CAST(strftime('%s', 'now') AS INTEGER)";

/// What `quote_at` reads, in its order.
const QUOTE_COLUMNS: &str = "quote_id, nonce, payout, payout_note, amount, deposit_zat";
/// Quote `?1`, unexpired at `?2`, not yet accepted, and not a reverse quote.
const LIVE_QUOTE: &str = "quote_id = ?1 AND accepted = 0 AND expires_at > ?2
    AND NOT EXISTS (SELECT 1 FROM reverse_quotes r WHERE r.quote_id = quotes.quote_id)";

const SWAP_COLUMNS: &str = "
    s.id, s.user_share, s.viewing_keys, s.zcash_account, s.opened_at, s.t1, s.sweep_txid, s.settled,
    q.quote_id, q.nonce, q.payout, q.payout_note, q.amount, q.deposit_zat, s.refund_started, s.t0,
    s.token
";

#[derive(Clone, Debug)]
pub struct Quote {
    pub id: [u8; 32],
    /// Index of the maker share, derived from the root secret rather than stored. Never
    /// reused: see `next_share_index`.
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
    /// The token and deadlines sent with `open`, part of the terms every later call supplies;
    /// kept per swap, since the configured token may change while one runs. `open` fails from
    /// `t0` on, so by `t1` the swap is on-chain if it ever will be.
    pub token: Address,
    pub t0: u64,
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
        let columns = conn
            .prepare("PRAGMA table_info(swaps)")?
            .query_map([], |row| row.get::<_, String>(1))?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        anyhow::ensure!(
            columns.iter().any(|column| column == "archived"),
            "{} was made by an older maker; a new deployment needs a new store",
            path.display()
        );
        if !columns.iter().any(|column| column == "refund_started") {
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
        let mut conn = self.conn();
        let tx = conn.transaction()?;
        let nonce = next_share_index(&tx)?;
        tx.execute(
            "INSERT INTO quotes (quote_id, nonce, payout, payout_note, amount, deposit_zat, expires_at)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
            params![
                id,
                nonce,
                payout.as_slice(),
                payout_note.as_ref().map(B256::as_slice),
                amount.to_string(),
                deposit_zat,
                expires_at
            ],
        )?;
        tx.commit()?;
        Ok(nonce)
    }

    /// A live forward quote, left live for `take_quote`.
    pub fn quote(&self, id: &[u8; 32], now: u64) -> Result<Option<Quote>> {
        let sql = format!("SELECT {QUOTE_COLUMNS} FROM quotes WHERE {LIVE_QUOTE}");
        Ok(self
            .conn()
            .query_row(&sql, params![id, now], |row| quote_at(row, 0))
            .optional()?)
    }

    /// Claims a live forward quote; each can be accepted once.
    pub fn take_quote(&self, id: &[u8; 32], now: u64) -> Result<Option<Quote>> {
        let sql =
            format!("UPDATE quotes SET accepted = 1 WHERE {LIVE_QUOTE} RETURNING {QUOTE_COLUMNS}");
        Ok(self
            .conn()
            .query_row(&sql, params![id, now], |row| quote_at(row, 0))
            .optional()?)
    }

    pub fn insert_swap(&self, swap: &Swap, event: Option<&Notification>) -> Result<()> {
        let mut conn = self.conn();
        let tx = conn.transaction()?;
        tx.execute(
            "INSERT INTO swaps (id, quote_id, user_share, viewing_keys, zcash_account, opened_at, token, t0, t1)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)",
            params![
                swap.id.as_slice(),
                swap.quote.id,
                swap.user_share.to_affine_bytes(),
                swap.viewing.to_bytes(),
                swap.zcash_account.expose_uuid().to_string(),
                swap.opened_at,
                swap.token.as_slice(),
                swap.t0,
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

    /// Also revisits swaps settled within `FINAL_AFTER`, so a reorg can resume settlement.
    pub fn watched_swaps(&self) -> Result<Vec<Swap>> {
        self.swaps(true)
    }

    fn swaps(&self, recently_settled: bool) -> Result<Vec<Swap>> {
        let sql = format!(
            "SELECT {SWAP_COLUMNS} FROM swaps s JOIN quotes q USING (quote_id)
             WHERE s.settled = 0 OR (?1 AND s.settled_at > {NOW} - ?2)
             ORDER BY s.settled, s.opened_at"
        );
        let conn = self.conn();
        let mut statement = conn.prepare(&sql)?;
        let swaps = statement
            .query_map(params![recently_settled, FINAL_AFTER], swap_from_row)?
            .collect::<Result<_, _>>()?;
        Ok(swaps)
    }

    /// Swaps of either direction settled over `FINAL_AFTER` ago and not yet archived, with the
    /// deposit accounts the wallet no longer needs to scan.
    pub fn final_swaps(&self) -> Result<Vec<(B256, AccountUuid)>> {
        let conn = self.conn();
        let mut forward = conn.prepare(&format!(
            "SELECT id, zcash_account FROM swaps
             WHERE settled = 1 AND archived = 0 AND settled_at <= {NOW} - ?1"
        ))?;
        let mut swaps = forward
            .query_map([FINAL_AFTER], |row| {
                let account: String = row.get(1)?;
                let account = Uuid::parse_str(&account).map_err(|e| invalid(1, Type::Text, e))?;
                Ok((
                    B256::from(row.get::<_, [u8; 32]>(0)?),
                    AccountUuid::from_uuid(account),
                ))
            })?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        let mut reverse = conn.prepare(&format!(
            "SELECT data FROM reverse_swaps
             WHERE settled = 1 AND archived = 0 AND settled_at <= {NOW} - ?1"
        ))?;
        for data in reverse.query_map([FINAL_AFTER], |row| row.get::<_, String>(0))? {
            let swap: ReverseSwap = serde_json::from_str(&data?)?;
            swaps.push((swap.id, swap.account));
        }
        Ok(swaps)
    }

    /// Leaves a final swap, of either direction, out of `final_swaps` from now on.
    pub fn archive(&self, id: &B256) -> Result<()> {
        let conn = self.conn();
        for table in ["swaps", "reverse_swaps"] {
            conn.execute(
                &format!("UPDATE {table} SET archived = 1 WHERE id = ?1"),
                params![id.as_slice()],
            )?;
        }
        Ok(())
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
            &format!("UPDATE swaps SET settled = 1, settled_at = {NOW} WHERE id = ?1"),
            params![id.as_slice()],
        )?;
        notifications::insert(&tx, event)?;
        tx.commit()?;
        Ok(())
    }

    pub fn resume(&self, id: &B256) -> Result<()> {
        self.conn().execute(
            "UPDATE swaps SET settled = 0, settled_at = NULL WHERE id = ?1",
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
        token: Address::from(row.get::<_, [u8; 20]>(16)?),
        t0: row.get(15)?,
        t1: row.get(5)?,
        sweep: row.get::<_, Option<[u8; 32]>>(6)?.map(TxId::from_bytes),
        settled: row.get(7)?,
        refund_started: row.get(14)?,
        quote: quote_at(row, 8)?,
    })
}

/// Reads the quote columns starting at `first`, in `quote_id, nonce, payout, payout_note,
/// amount, deposit_zat` order.
/// The next maker share index, forward or reverse: one past the highest, and never below the
/// time in milliseconds. A database restored from a backup can't wind the clock back, so it
/// never reissues an index whose share a later swap may since have revealed.
fn next_share_index(conn: &Connection) -> rusqlite::Result<u64> {
    let millis = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("clock is after 1970")
        .as_millis() as u64;
    conn.query_row(
        "SELECT MAX(IFNULL(MAX(nonce) + 1, 0), ?1) FROM quotes",
        [millis],
        |row| row.get(0),
    )
}

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

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use super::*;

    /// A day after settling, a swap is final: no longer re-read in case a reorganisation undoes
    /// it, and its deposit account offered once for the wallet to stop scanning. One settled
    /// since is still re-read.
    #[test]
    fn settled_swaps_turn_final_after_a_day() {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::open(&dir.path().join("maker.sqlite")).unwrap();
        let account = |id: u8| AccountUuid::from_uuid(Uuid::from_bytes([id; 16]));
        for id in [1, 2] {
            store
                .insert_quote([id; 32], Address::repeat_byte(1), None, 1, 1, 4_000_000_000)
                .unwrap();
            let quote = store.take_quote(&[id; 32], 0).unwrap().unwrap();
            let random = || rand::rand_core::UnwrapErr(rand::rngs::SysRng);
            let swap = Swap {
                id: B256::repeat_byte(id),
                quote,
                user_share: zecswap_core::SecretShare::random(random()).public(),
                viewing: ViewingKeys::random(random()),
                zcash_account: account(id),
                opened_at: 0,
                token: Address::repeat_byte(2),
                t0: 1,
                t1: 2,
                sweep: None,
                settled: false,
                refund_started: false,
            };
            store.insert_swap(&swap, None).unwrap();
            store.settle(&swap.id, None).unwrap();
        }
        store
            .conn()
            .execute(
                "UPDATE swaps SET settled_at = settled_at - ?1 WHERE id = ?2",
                params![FINAL_AFTER, [1u8; 32]],
            )
            .unwrap();
        let watched: Vec<B256> = store
            .watched_swaps()
            .unwrap()
            .iter()
            .map(|swap| swap.id)
            .collect();
        assert_eq!(watched, [B256::repeat_byte(2)]);
        let first = (B256::repeat_byte(1), account(1));
        assert_eq!(store.final_swaps().unwrap(), [first]);
        store.archive(&first.0).unwrap();
        assert!(store.final_swaps().unwrap().is_empty());
    }

    /// A maker share revealed by a swap the backup never saw must not come back: a database
    /// restored from that backup still issues indices above every one handed out before.
    #[test]
    fn a_restored_backup_never_reissues_a_share_index() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("maker.sqlite");
        let backup = dir.path().join("backup.sqlite");
        let quote = |store: &Store, id: u8| {
            store
                .insert_quote([id; 32], Address::repeat_byte(1), None, 1, 1, 4_000_000_000)
                .unwrap()
        };
        let store = Store::open(&path).unwrap();
        quote(&store, 1);
        drop(store);
        std::fs::copy(&path, &backup).unwrap();
        let store = Store::open(&path).unwrap();
        let lost: Vec<u64> = (2..5).map(|id| quote(&store, id)).collect();
        drop(store);
        // A restore comes after the quotes it loses.
        std::thread::sleep(Duration::from_millis(20));
        std::fs::copy(&backup, &path).unwrap();
        let store = Store::open(&path).unwrap();
        let next = quote(&store, 5);
        assert!(
            lost.iter().all(|&index| next > index),
            "{next} reissues one of {lost:?}"
        );
    }

    /// A store from before the terms hash lacks terms every call on its swaps now supplies:
    /// refused at startup, rather than failing every watchtower pass, cancellations included.
    #[test]
    fn a_store_from_an_older_maker_is_refused() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("maker.sqlite");
        Connection::open(&path)
            .unwrap()
            .execute_batch(
                "CREATE TABLE swaps (
                    id BLOB PRIMARY KEY,
                    quote_id BLOB NOT NULL UNIQUE,
                    user_share BLOB NOT NULL,
                    viewing_keys BLOB NOT NULL,
                    zcash_account TEXT NOT NULL,
                    opened_at INTEGER NOT NULL,
                    t1 INTEGER NOT NULL,
                    sweep_txid BLOB,
                    settled INTEGER NOT NULL DEFAULT 0,
                    refund_started INTEGER NOT NULL DEFAULT 0
                )",
            )
            .unwrap();
        let error = Store::open(&path).err().expect("the old store opened");
        assert!(error.to_string().contains("older maker"), "{error}");
    }
}
