//! What each swap earned and cost: the prices its quote was made at, the gas of every Ethereum
//! transaction on it and the fees of the maker's Zcash ones, each valued in USD when it
//! happened. As in monitoring, never serialize a Swap or ReverseSwap here.
use std::collections::HashSet;

use anyhow::Result;
use rusqlite::{Connection, OptionalExtension, params};
use serde::Serialize;
use zecswap_chain::evm::{Address, B256, TransactionFacts};
use zecswap_chain::zcash::TxId;

use super::Store;
use super::monitoring::stored_txid;
use crate::maker::unix_now;
use crate::market::QuoteMark;

pub(super) const SCHEMA: &str = "
    CREATE TABLE IF NOT EXISTS quote_prices (
        quote_id BLOB PRIMARY KEY, at INTEGER NOT NULL, source TEXT NOT NULL,
        zec_usd TEXT NOT NULL, usdc_usd TEXT NOT NULL, eth_usd TEXT
    );
    -- The maker's own sends, reverted ones included: those emit no event to find them by.
    CREATE TABLE IF NOT EXISTS sent_transactions (
        scope TEXT NOT NULL, transaction_hash BLOB NOT NULL, swap_id BLOB NOT NULL,
        operation TEXT NOT NULL, sent_at INTEGER NOT NULL,
        PRIMARY KEY (scope, transaction_hash)
    );
    CREATE INDEX IF NOT EXISTS sent_transactions_swap ON sent_transactions(scope, swap_id);
    CREATE TABLE IF NOT EXISTS evm_costs (
        scope TEXT NOT NULL, transaction_hash BLOB NOT NULL, sender BLOB NOT NULL,
        succeeded INTEGER NOT NULL, block_number INTEGER NOT NULL, block_hash BLOB NOT NULL,
        gas_used INTEGER NOT NULL, gas_price TEXT NOT NULL, paid_to_sender TEXT NOT NULL,
        eth_usd TEXT, eth_usd_source TEXT,
        PRIMARY KEY (scope, transaction_hash)
    );
    -- A fee is unknown for a transaction the wallet no longer stores.
    CREATE TABLE IF NOT EXISTS zcash_costs (
        scope TEXT NOT NULL, txid BLOB NOT NULL, fee_zat INTEGER, at INTEGER NOT NULL,
        zec_usd TEXT, zec_usd_source TEXT,
        PRIMARY KEY (scope, txid)
    );
";

/// One of the maker's Zcash sends on a swap, as the swap records its current one.
pub(crate) struct ZcashSend {
    pub swap: B256,
    pub kind: &'static str,
    pub txid: TxId,
    pub settled_at: Option<u64>,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct SwapEconomics {
    pub id: B256,
    pub direction: String,
    pub amount: String,
    pub deposit_zat: String,
    pub accepted_at: Option<u64>,
    pub settled: bool,
    pub settled_at: Option<u64>,
    /// The market prices the quote was made at; none before they were recorded and valued.
    pub quote: Option<QuotePrices>,
    pub evm: Vec<EvmCost>,
    pub zcash: Vec<ZcashCost>,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct QuotePrices {
    pub at: u64,
    /// The provider that priced the quote, or `history` for a candle read afterwards.
    pub source: String,
    pub zec_usd: String,
    pub usdc_usd: String,
    pub eth_usd: Option<String>,
}

/// A transaction on the swap. Everything its receipt says is missing until that is read.
#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct EvmCost {
    pub transaction_hash: B256,
    /// The swap's contract events in it, in order: none for a send that reverted.
    pub events: Vec<String>,
    /// What the maker sent it for, where the maker sent it.
    pub operation: Option<String>,
    pub sender: Option<Address>,
    pub succeeded: Option<bool>,
    pub block_number: Option<u64>,
    pub block_time: Option<u64>,
    pub gas_used: Option<u64>,
    pub gas_price_wei: Option<String>,
    /// The escrow token it moved to its sender: a relayer's fee.
    pub paid_to_sender: Option<String>,
    pub eth_usd: Option<String>,
    /// `live` for the maker's own price at the time, `history` for a candle read afterwards.
    pub eth_usd_source: Option<String>,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct ZcashCost {
    pub txid: String,
    /// `sweep` (a forward swap's deposit into inventory), `deposit` (the maker paying a reverse
    /// swap) or `recovery` (the maker taking that deposit back after a refund).
    pub kind: &'static str,
    pub fee_zat: Option<String>,
    pub at: Option<u64>,
    pub zec_usd: Option<String>,
    pub zec_usd_source: Option<String>,
}

impl Store {
    pub(crate) fn record_quote_price(
        &self,
        quote: &[u8; 32],
        at: u64,
        mark: &QuoteMark,
    ) -> Result<()> {
        self.conn().execute(
            "INSERT OR IGNORE INTO quote_prices VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
            params![
                quote,
                at,
                mark.source,
                mark.zec_usd,
                mark.usdc_usd,
                mark.eth_usd
            ],
        )?;
        Ok(())
    }

    /// Quotes of swaps with no prices recorded, newest first, with when each was accepted.
    pub(crate) fn unpriced_quotes(&self, limit: usize) -> Result<Vec<([u8; 32], u64)>> {
        let conn = self.conn();
        let mut statement = conn.prepare(
            "SELECT quote_id, at FROM (
                SELECT q.quote_id, coalesce(a.at, s.opened_at) AS at
                FROM swaps s JOIN quotes q USING (quote_id) LEFT JOIN swap_accepted a ON a.id = s.id
                UNION ALL
                SELECT q.quote_id, a.at
                FROM reverse_swaps s JOIN quotes q USING (quote_id) JOIN swap_accepted a ON a.id = s.id
            ) u WHERE NOT EXISTS (SELECT 1 FROM quote_prices p WHERE p.quote_id = u.quote_id)
            ORDER BY at DESC LIMIT ?1",
        )?;
        Ok(statement
            .query_map([limit as u64], |row| Ok((row.get(0)?, row.get(1)?)))?
            .collect::<rusqlite::Result<_>>()?)
    }

    pub(crate) fn record_sent(
        &self,
        scope: &str,
        swap: B256,
        operation: &str,
        hash: B256,
        at: u64,
    ) -> Result<()> {
        self.conn().execute(
            "INSERT OR IGNORE INTO sent_transactions VALUES (?1, ?2, ?3, ?4, ?5)",
            params![scope, hash.as_slice(), swap.as_slice(), operation, at],
        )?;
        Ok(())
    }

    /// Transactions on swaps whose receipts are unread: the maker's own sends first, then
    /// events', newest first. A send unmined a day on was dropped, and has no receipt to read.
    pub(crate) fn unread_transactions(&self, scope: &str, limit: usize) -> Result<Vec<B256>> {
        let conn = self.conn();
        let mut statement = conn.prepare(
            "SELECT hash FROM (
                SELECT transaction_hash AS hash, 0 AS rank, sent_at AS ord
                FROM sent_transactions WHERE scope = ?1 AND sent_at > ?3
                UNION ALL
                SELECT transaction_hash, 1, block_number FROM evm_transactions WHERE scope = ?1
            ) t WHERE NOT EXISTS (
                SELECT 1 FROM evm_costs c WHERE c.scope = ?1 AND c.transaction_hash = t.hash
            ) GROUP BY hash ORDER BY min(rank), max(ord) DESC LIMIT ?2",
        )?;
        Ok(statement
            .query_map(
                params![scope, limit as u64, unix_now().saturating_sub(86_400)],
                |row| Ok(B256::from(row.get::<_, [u8; 32]>(0)?)),
            )?
            .collect::<rusqlite::Result<_>>()?)
    }

    pub(crate) fn save_evm_cost(
        &self,
        scope: &str,
        hash: B256,
        facts: &TransactionFacts,
        eth_usd: Option<(&str, &str)>,
    ) -> Result<()> {
        self.conn().execute(
            "INSERT OR IGNORE INTO evm_costs VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11)",
            params![
                scope,
                hash.as_slice(),
                facts.sender.as_slice(),
                facts.succeeded,
                facts.block_number,
                facts.block_hash.as_slice(),
                facts.gas_used,
                facts.gas_price.to_string(),
                facts.paid_to_sender.to_string(),
                eth_usd.map(|(usd, _)| usd),
                eth_usd.map(|(_, source)| source),
            ],
        )?;
        Ok(())
    }

    /// Costs not yet valued, newest first, with their block's time.
    pub(crate) fn unvalued_evm_costs(&self, scope: &str, limit: usize) -> Result<Vec<(B256, u64)>> {
        let conn = self.conn();
        let mut statement = conn.prepare(
            "SELECT c.transaction_hash, b.time FROM evm_costs c
             JOIN evm_block_times b ON b.scope = c.scope AND b.block_hash = c.block_hash
             WHERE c.scope = ?1 AND c.eth_usd IS NULL ORDER BY c.block_number DESC LIMIT ?2",
        )?;
        Ok(statement
            .query_map(params![scope, limit as u64], |row| {
                Ok((B256::from(row.get::<_, [u8; 32]>(0)?), row.get(1)?))
            })?
            .collect::<rusqlite::Result<_>>()?)
    }

    pub(crate) fn value_evm_cost(
        &self,
        scope: &str,
        hash: B256,
        usd: &str,
        source: &str,
    ) -> Result<()> {
        self.conn().execute(
            "UPDATE evm_costs SET eth_usd = ?3, eth_usd_source = ?4
             WHERE scope = ?1 AND transaction_hash = ?2 AND eth_usd IS NULL",
            params![scope, hash.as_slice(), usd, source],
        )?;
        Ok(())
    }

    /// The maker's Zcash sends with no fee recorded, up to `limit`.
    pub(crate) fn uncosted_zcash_sends(&self, scope: &str, limit: usize) -> Result<Vec<ZcashSend>> {
        let conn = self.conn();
        let costed = conn
            .prepare("SELECT txid FROM zcash_costs WHERE scope = ?1")?
            .query_map([scope], |row| row.get::<_, [u8; 32]>(0))?
            .collect::<rusqlite::Result<HashSet<_>>>()?;
        Ok(zcash_sends(&conn, None)?
            .into_iter()
            .filter(|send| !costed.contains(send.txid.as_ref()))
            .take(limit)
            .collect())
    }

    pub(crate) fn save_zcash_cost(
        &self,
        scope: &str,
        txid: TxId,
        fee_zat: Option<u64>,
        at: u64,
    ) -> Result<()> {
        self.conn().execute(
            "INSERT OR IGNORE INTO zcash_costs (scope, txid, fee_zat, at) VALUES (?1, ?2, ?3, ?4)",
            params![scope, txid.as_ref(), fee_zat, at],
        )?;
        Ok(())
    }

    pub(crate) fn unvalued_zcash_costs(
        &self,
        scope: &str,
        limit: usize,
    ) -> Result<Vec<(TxId, u64)>> {
        let conn = self.conn();
        let mut statement = conn.prepare(
            "SELECT txid, at FROM zcash_costs WHERE scope = ?1 AND zec_usd IS NULL
             ORDER BY at DESC LIMIT ?2",
        )?;
        Ok(statement
            .query_map(params![scope, limit as u64], |row| {
                Ok((TxId::from_bytes(row.get(0)?), row.get(1)?))
            })?
            .collect::<rusqlite::Result<_>>()?)
    }

    pub(crate) fn value_zcash_cost(
        &self,
        scope: &str,
        txid: TxId,
        usd: &str,
        source: &str,
    ) -> Result<()> {
        self.conn().execute(
            "UPDATE zcash_costs SET zec_usd = ?3, zec_usd_source = ?4
             WHERE scope = ?1 AND txid = ?2 AND zec_usd IS NULL",
            params![scope, txid.as_ref(), usd, source],
        )?;
        Ok(())
    }

    /// Swaps accepted at or after `since`, newest first, at most `limit`, and whether more
    /// were left out.
    pub(crate) fn economics(
        &self,
        scope: &str,
        since: u64,
        limit: usize,
    ) -> Result<(Vec<SwapEconomics>, bool)> {
        let conn = self.conn();
        let mut statement = conn.prepare(
            "SELECT * FROM (
                SELECT s.id, 'forward' AS direction, q.amount, q.deposit_zat, s.settled,
                    s.settled_at, coalesce(a.at, s.opened_at) AS accepted_at, q.nonce,
                    p.at, p.source, p.zec_usd, p.usdc_usd, p.eth_usd
                FROM swaps s JOIN quotes q USING (quote_id) LEFT JOIN swap_accepted a ON a.id = s.id
                    LEFT JOIN quote_prices p ON p.quote_id = q.quote_id
                UNION ALL
                SELECT s.id, 'reverse', q.amount, q.deposit_zat, s.settled, s.settled_at, a.at,
                    q.nonce, p.at, p.source, p.zec_usd, p.usdc_usd, p.eth_usd
                FROM reverse_swaps s JOIN quotes q USING (quote_id)
                    LEFT JOIN swap_accepted a ON a.id = s.id
                    LEFT JOIN quote_prices p ON p.quote_id = q.quote_id
            ) WHERE coalesce(accepted_at, ?1) >= ?1 ORDER BY nonce DESC LIMIT ?2",
        )?;
        let mut swaps = statement
            .query_map(params![since, limit as u64 + 1], |row| {
                Ok(SwapEconomics {
                    id: B256::from(row.get::<_, [u8; 32]>(0)?),
                    direction: row.get(1)?,
                    amount: row.get(2)?,
                    deposit_zat: row.get::<_, u64>(3)?.to_string(),
                    settled: row.get(4)?,
                    settled_at: row.get(5)?,
                    accepted_at: row.get(6)?,
                    quote: match row.get::<_, Option<u64>>(8)? {
                        Some(at) => Some(QuotePrices {
                            at,
                            source: row.get(9)?,
                            zec_usd: row.get(10)?,
                            usdc_usd: row.get(11)?,
                            eth_usd: row.get(12)?,
                        }),
                        None => None,
                    },
                    evm: Vec::new(),
                    zcash: Vec::new(),
                })
            })?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        let truncated = swaps.len() > limit;
        swaps.truncate(limit);
        for swap in &mut swaps {
            swap.evm = evm_costs(&conn, scope, swap.id)?;
            swap.zcash = zcash_sends(&conn, Some(swap.id))?
                .into_iter()
                .map(|send| zcash_cost(&conn, scope, send))
                .collect::<Result<_>>()?;
        }
        Ok((swaps, truncated))
    }
}

/// Every transaction on swap `id`: those its events were in and those the maker sent for it,
/// in chain order, the unmined last.
fn evm_costs(conn: &Connection, scope: &str, id: B256) -> Result<Vec<EvmCost>> {
    let mut costs: Vec<EvmCost> = Vec::new();
    let mut events = conn.prepare(
        "SELECT transaction_hash, kind FROM evm_transactions
         WHERE scope = ?1 AND swap_id = ?2 ORDER BY block_number, log_index",
    )?;
    let mut sent = conn.prepare(
        "SELECT transaction_hash, operation FROM sent_transactions
         WHERE scope = ?1 AND swap_id = ?2 ORDER BY sent_at",
    )?;
    for row in events.query_map(params![scope, id.as_slice()], |row| {
        Ok((
            B256::from(row.get::<_, [u8; 32]>(0)?),
            row.get::<_, String>(1)?,
        ))
    })? {
        let (hash, kind) = row?;
        entry(&mut costs, hash).events.push(kind);
    }
    for row in sent.query_map(params![scope, id.as_slice()], |row| {
        Ok((
            B256::from(row.get::<_, [u8; 32]>(0)?),
            row.get::<_, String>(1)?,
        ))
    })? {
        let (hash, operation) = row?;
        entry(&mut costs, hash).operation = Some(operation);
    }
    let mut read = conn.prepare(
        "SELECT sender, succeeded, block_number, gas_used, gas_price, paid_to_sender, eth_usd,
            eth_usd_source,
            (SELECT time FROM evm_block_times b WHERE b.scope = c.scope AND b.block_hash = c.block_hash)
         FROM evm_costs c WHERE scope = ?1 AND transaction_hash = ?2",
    )?;
    for cost in &mut costs {
        read.query_row(params![scope, cost.transaction_hash.as_slice()], |row| {
            cost.sender = Some(Address::from(row.get::<_, [u8; 20]>(0)?));
            cost.succeeded = Some(row.get(1)?);
            cost.block_number = Some(row.get(2)?);
            cost.gas_used = Some(row.get(3)?);
            cost.gas_price_wei = Some(row.get(4)?);
            cost.paid_to_sender = Some(row.get(5)?);
            cost.eth_usd = row.get(6)?;
            cost.eth_usd_source = row.get(7)?;
            cost.block_time = row.get(8)?;
            Ok(())
        })
        .optional()?;
    }
    costs.sort_by_key(|cost| cost.block_number.unwrap_or(u64::MAX));
    Ok(costs)
}

/// Transaction `hash` among `costs`, added unread if it isn't yet.
fn entry(costs: &mut Vec<EvmCost>, hash: B256) -> &mut EvmCost {
    let index = match costs.iter().position(|cost| cost.transaction_hash == hash) {
        Some(index) => index,
        None => {
            costs.push(EvmCost {
                transaction_hash: hash,
                events: Vec::new(),
                operation: None,
                sender: None,
                succeeded: None,
                block_number: None,
                block_time: None,
                gas_used: None,
                gas_price_wei: None,
                paid_to_sender: None,
                eth_usd: None,
                eth_usd_source: None,
            });
            costs.len() - 1
        }
    };
    &mut costs[index]
}

/// The maker's Zcash sends, of every swap or of `swap` alone.
fn zcash_sends(conn: &Connection, swap: Option<B256>) -> Result<Vec<ZcashSend>> {
    let mut statement = conn.prepare(
        "SELECT id, 'sweep', lower(hex(sweep_txid)), 0, settled_at FROM swaps
         WHERE sweep_txid IS NOT NULL AND (?1 IS NULL OR id = ?1)
         UNION ALL
         SELECT id, 'deposit', json_extract(data, '$.deposit'), 1, settled_at FROM reverse_swaps
         WHERE json_extract(data, '$.deposit') IS NOT NULL AND (?1 IS NULL OR id = ?1)
         UNION ALL
         SELECT id, 'recovery', json_extract(data, '$.sweep'), 1, settled_at FROM reverse_swaps
         WHERE json_extract(data, '$.sweep') IS NOT NULL AND (?1 IS NULL OR id = ?1)",
    )?;
    Ok(statement
        .query_map([swap.as_ref().map(B256::as_slice)], |row| {
            let kind = match row.get::<_, String>(1)?.as_str() {
                "sweep" => "sweep",
                "deposit" => "deposit",
                _ => "recovery",
            };
            Ok(ZcashSend {
                swap: B256::from(row.get::<_, [u8; 32]>(0)?),
                kind,
                txid: stored_txid(row.get(2)?, row.get(3)?, 2)?,
                settled_at: row.get(4)?,
            })
        })?
        .collect::<rusqlite::Result<_>>()?)
}

fn zcash_cost(conn: &Connection, scope: &str, send: ZcashSend) -> Result<ZcashCost> {
    let recorded = conn
        .query_row(
            "SELECT fee_zat, at, zec_usd, zec_usd_source FROM zcash_costs
             WHERE scope = ?1 AND txid = ?2",
            params![scope, send.txid.as_ref()],
            |row| {
                Ok((
                    row.get::<_, Option<u64>>(0)?,
                    row.get::<_, u64>(1)?,
                    row.get::<_, Option<String>>(2)?,
                    row.get::<_, Option<String>>(3)?,
                ))
            },
        )
        .optional()?;
    let (fee, at, usd, source) = match recorded {
        Some((fee, at, usd, source)) => (fee.map(|fee| fee.to_string()), Some(at), usd, source),
        None => (None, None, None, None),
    };
    Ok(ZcashCost {
        txid: send.txid.to_string(),
        kind: send.kind,
        fee_zat: fee,
        at,
        zec_usd: usd,
        zec_usd_source: source,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use zecswap_chain::evm::U256;

    const SCOPE: &str = "testnet:1:contract:maker";

    fn store() -> (tempfile::TempDir, Store) {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::open(&dir.path().join("maker.sqlite")).unwrap();
        (dir, store)
    }

    /// A forward swap `[id; 32]` accepted at `accepted_at`, as raw rows.
    fn forward(store: &Store, id: u8, accepted_at: u64) {
        let conn = store.conn();
        conn.execute(
            "INSERT INTO quotes VALUES (?1, ?2, X'00', NULL, '5000000', 4200, 9999999999, 1)",
            params![[id; 32], id],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO swaps (id, quote_id, user_share, viewing_keys, zcash_account, opened_at,
                token, t0, t1) VALUES (?1, ?1, X'12', X'34', 'account', ?2, zeroblob(20), 0, 0)",
            params![[id; 32], accepted_at],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO swap_accepted VALUES (?1, ?2)",
            params![[id; 32], accepted_at],
        )
        .unwrap();
    }

    fn facts(block: u64, succeeded: bool) -> TransactionFacts {
        TransactionFacts {
            sender: Address::repeat_byte(0xaa),
            succeeded,
            block_number: block,
            block_hash: B256::repeat_byte(block as u8),
            gas_used: 50_000,
            gas_price: 2_000_000_000,
            uses_railgun: false,
            paid_to_sender: U256::ZERO,
        }
    }

    /// A sweep that never reached the network expires and the maker sweeps again: only the
    /// sweep the swap now records is its cost, never both fees.
    #[test]
    fn a_replaced_sweep_is_not_costed_twice() {
        let (_dir, store) = store();
        forward(&store, 1, 1_000);
        let (expired, mined) = (TxId::from_bytes([7; 32]), TxId::from_bytes([8; 32]));
        store.record_sweep(&B256::repeat_byte(1), expired).unwrap();
        let send = store.uncosted_zcash_sends(SCOPE, 10).unwrap();
        assert_eq!((send.len(), send[0].kind), (1, "sweep"));
        assert_eq!(send[0].txid, expired);
        store
            .save_zcash_cost(SCOPE, expired, Some(10_000), 1_100)
            .unwrap();
        assert!(store.uncosted_zcash_sends(SCOPE, 10).unwrap().is_empty());

        store.record_sweep(&B256::repeat_byte(1), mined).unwrap();
        assert_eq!(
            store.uncosted_zcash_sends(SCOPE, 10).unwrap()[0].txid,
            mined
        );
        store
            .save_zcash_cost(SCOPE, mined, Some(15_000), 1_200)
            .unwrap();
        let (swaps, truncated) = store.economics(SCOPE, 0, 10).unwrap();
        assert!(!truncated);
        let zcash = &swaps[0].zcash;
        assert_eq!(zcash.len(), 1);
        assert_eq!(zcash[0].txid, mined.to_string());
        assert_eq!(
            (zcash[0].fee_zat.as_deref(), zcash[0].at),
            (Some("15000"), Some(1_200))
        );
    }

    /// Each pass asks only for what is still missing: a quote already priced isn't priced
    /// again, a receipt already read isn't read again, and a send that never mined stops being
    /// asked for a day on, so none of them crowds out the work behind it.
    #[test]
    fn worklists_name_only_what_is_missing() {
        let (_dir, store) = store();
        forward(&store, 1, 1_000);
        forward(&store, 2, 2_000);
        let mark = QuoteMark {
            source: "coinmarketcap",
            zec_usd: "1185.19".into(),
            usdc_usd: "1.0006".into(),
            eth_usd: None,
        };
        store.record_quote_price(&[2; 32], 2_000, &mark).unwrap();
        assert_eq!(store.unpriced_quotes(10).unwrap(), [([1; 32], 1_000)]);

        let swap = B256::repeat_byte(1);
        let (read, dropped, fresh) = (
            B256::repeat_byte(3),
            B256::repeat_byte(4),
            B256::repeat_byte(5),
        );
        let now = unix_now();
        store.record_sent(SCOPE, swap, "open", read, now).unwrap();
        store
            .record_sent(SCOPE, swap, "ready", dropped, now - 86_401)
            .unwrap();
        store
            .record_sent(SCOPE, swap, "refund", fresh, now - 60)
            .unwrap();
        store
            .save_evm_cost(SCOPE, read, &facts(10, true), None)
            .unwrap();
        assert_eq!(store.unread_transactions(SCOPE, 10).unwrap(), [fresh]);

        // Valued once a block time is known, and only once.
        assert!(store.unvalued_evm_costs(SCOPE, 10).unwrap().is_empty());
        store
            .save_block_time(SCOPE, B256::repeat_byte(10), 1_500)
            .unwrap();
        assert_eq!(
            store.unvalued_evm_costs(SCOPE, 10).unwrap(),
            [(read, 1_500)]
        );
        store
            .value_evm_cost(SCOPE, read, "2473.68", "live")
            .unwrap();
        store.value_evm_cost(SCOPE, read, "1", "history").unwrap();
        assert!(store.unvalued_evm_costs(SCOPE, 10).unwrap().is_empty());
        let (swaps, _) = store.economics(SCOPE, 1_500, 10).unwrap();
        assert_eq!(
            swaps.len(),
            1,
            "accepted before `since`, swap 1 is left out"
        );
        let (swaps, _) = store.economics(SCOPE, 0, 10).unwrap();
        let opened = &swaps.iter().find(|s| s.id == swap).unwrap().evm[0];
        assert_eq!(opened.transaction_hash, read);
        assert_eq!(
            (opened.eth_usd.as_deref(), opened.eth_usd_source.as_deref()),
            (Some("2473.68"), Some("live"))
        );
        assert_eq!(
            (
                opened.gas_used,
                opened.gas_price_wei.as_deref(),
                opened.block_time
            ),
            (Some(50_000), Some("2000000000"), Some(1_500))
        );
    }
}
