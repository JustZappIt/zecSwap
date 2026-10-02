//! Read-only, bounded operations export. Never serialize a Swap or ReverseSwap here:
//! those types contain viewing keys and authorization material.
use anyhow::Result;
use rusqlite::params;
use serde::Serialize;
use zecswap_chain::evm::B256;

use super::Store;

fn stored_txid(value: String, reverse: bool, column: usize) -> rusqlite::Result<String> {
    let bytes: [u8; 32] = if reverse {
        serde_json::from_str(&value).map_err(|error| {
            rusqlite::Error::FromSqlConversionFailure(
                column,
                rusqlite::types::Type::Text,
                Box::new(error),
            )
        })?
    } else {
        hex::decode(value)
            .map_err(|error| {
                rusqlite::Error::FromSqlConversionFailure(
                    column,
                    rusqlite::types::Type::Text,
                    Box::new(error),
                )
            })?
            .try_into()
            .map_err(|_| {
                rusqlite::Error::InvalidColumnType(
                    column,
                    "txid".into(),
                    rusqlite::types::Type::Text,
                )
            })?
    };
    Ok(zecswap_chain::zcash::TxId::from_bytes(bytes).to_string())
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct MonitorCounts {
    pub quotes: u64,
    pub accepted_quotes: u64,
    pub forward_swaps: u64,
    pub reverse_swaps: u64,
    pub active_forward: u64,
    pub active_reverse: u64,
    pub settled: u64,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct MonitorSwap {
    pub id: B256,
    pub direction: String,
    pub amount: String,
    pub deposit_zat: String,
    pub settled: bool,
    pub funding_deadline: Option<u64>,
    pub ready_deadline: u64,
    pub refund_after: u64,
    pub deposit_txid: Option<String>,
    pub sweep_txid: Option<String>,
    // Unknown is different from an escrow that does not exist.
    pub chain_observed: bool,
    pub stage: Option<String>,
    pub paid_out: Option<bool>,
    pub claim_lock_until: Option<u64>,
    pub refund_lock_until: Option<u64>,
    pub last_pass_failed: bool,
    pub zec_total_zat: Option<String>,
    pub zec_spendable_zat: Option<String>,
    pub evm_transactions: Vec<super::EvmTransaction>,
    #[serde(skip)]
    pub account: String,
}

impl Store {
    pub(crate) fn monitor_counts(&self) -> Result<MonitorCounts> {
        Ok(self.conn().query_row(
            "SELECT
                (SELECT count(*) FROM quotes),
                (SELECT count(*) FROM quotes WHERE accepted = 1),
                (SELECT count(*) FROM swaps),
                (SELECT count(*) FROM reverse_swaps),
                (SELECT count(*) FROM swaps WHERE settled = 0),
                (SELECT count(*) FROM reverse_swaps WHERE settled = 0),
                (SELECT count(*) FROM swaps WHERE settled = 1) +
                (SELECT count(*) FROM reverse_swaps WHERE settled = 1)",
            [],
            |row| {
                Ok(MonitorCounts {
                    quotes: row.get(0)?,
                    accepted_quotes: row.get(1)?,
                    forward_swaps: row.get(2)?,
                    reverse_swaps: row.get(3)?,
                    active_forward: row.get(4)?,
                    active_reverse: row.get(5)?,
                    settled: row.get(6)?,
                })
            },
        )?)
    }

    pub(crate) fn monitor_swaps(&self, t0_after: u64, limit: usize) -> Result<Vec<MonitorSwap>> {
        let conn = self.conn();
        let mut statement = conn.prepare(
            "SELECT * FROM (
                SELECT s.id, 'forward' AS direction, q.amount, cast(q.deposit_zat AS TEXT),
                    s.settled, NULL, s.opened_at + ?1, s.t1, NULL,
                    CASE WHEN s.sweep_txid IS NULL THEN NULL ELSE lower(hex(s.sweep_txid)) END,
                    q.nonce, s.zcash_account
                FROM swaps s JOIN quotes q USING (quote_id)
                UNION ALL
                SELECT s.id, 'reverse', q.amount, cast(q.deposit_zat AS TEXT),
                    s.settled, json_extract(r.terms, '$.fundingDeadline'),
                    json_extract(r.terms, '$.readyDeadline'), json_extract(r.terms, '$.refundAfter'),
                    json_extract(s.data, '$.deposit'), json_extract(s.data, '$.sweep'), q.nonce,
                    json_extract(s.data, '$.account')
                FROM reverse_swaps s JOIN quotes q USING (quote_id)
                    JOIN reverse_quotes r USING (quote_id)
            ) ORDER BY settled ASC, nonce DESC LIMIT ?2",
        )?;
        Ok(statement
            .query_map(params![t0_after, limit as u64], |row| {
                Ok(MonitorSwap {
                    id: B256::from(row.get::<_, [u8; 32]>(0)?),
                    direction: row.get(1)?,
                    amount: row.get(2)?,
                    deposit_zat: row.get(3)?,
                    settled: row.get(4)?,
                    funding_deadline: row.get(5)?,
                    ready_deadline: row.get(6)?,
                    refund_after: row.get(7)?,
                    deposit_txid: row
                        .get::<_, Option<String>>(8)?
                        .map(|value| stored_txid(value, true, 8))
                        .transpose()?,
                    sweep_txid: {
                        let txid: Option<String> = row.get(9)?;
                        let reverse = row.get::<_, String>(1)? == "reverse";
                        txid.map(|value| stored_txid(value, reverse, 9))
                            .transpose()?
                    },
                    chain_observed: false,
                    stage: None,
                    paid_out: None,
                    claim_lock_until: None,
                    refund_lock_until: None,
                    last_pass_failed: false,
                    zec_total_zat: None,
                    zec_spendable_zat: None,
                    evm_transactions: Vec::new(),
                    account: {
                        let account: String = row.get(11)?;
                        if row.get::<_, String>(1)? == "reverse" {
                            let bytes: [u8; 16] =
                                serde_json::from_str(&account).map_err(|error| {
                                    rusqlite::Error::FromSqlConversionFailure(
                                        11,
                                        rusqlite::types::Type::Text,
                                        Box::new(error),
                                    )
                                })?;
                            uuid::Uuid::from_bytes(bytes).to_string()
                        } else {
                            account
                        }
                    },
                })
            })?
            .collect::<Result<_, _>>()?)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn empty_database_has_real_zero_counts() {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::open(&dir.path().join("maker.sqlite")).unwrap();
        let counts = store.monitor_counts().unwrap();
        assert_eq!(counts.quotes, 0);
        assert_eq!(counts.active_forward + counts.active_reverse, 0);
        assert!(store.monitor_swaps(2700, 50).unwrap().is_empty());
    }

    #[test]
    fn export_is_bounded_prioritizes_active_and_preserves_large_amounts() {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::open(&dir.path().join("maker.sqlite")).unwrap();
        let amount = u128::MAX.to_string();
        for (id, settled) in [(1u8, false), (2, true), (3, true)] {
            store
                .conn()
                .execute(
                    "INSERT INTO quotes VALUES (?1, ?2, X'00', NULL, ?3, 123, 999, 1)",
                    params![[id; 32], id, amount],
                )
                .unwrap();
            store.conn().execute(
                "INSERT INTO swaps VALUES (?1, ?1, X'1234', X'5678', 'private-account', 100, 700, NULL, ?2)",
                params![[id; 32], settled],
            ).unwrap();
        }
        let rows = store.monitor_swaps(200, 2).unwrap();
        assert_eq!(rows.len(), 2);
        assert_eq!(rows[0].id, B256::repeat_byte(1));
        assert_eq!(rows[1].id, B256::repeat_byte(3));
        assert_eq!(rows[0].amount, amount);
        assert_eq!(rows[0].ready_deadline, 300);
        let counts = store.monitor_counts().unwrap();
        assert_eq!(
            (
                counts.quotes,
                counts.accepted_quotes,
                counts.active_forward,
                counts.settled
            ),
            (3, 3, 1, 2)
        );
        let json = serde_json::to_string(&rows).unwrap();
        for private in ["viewing", "userShare", "nonce", "private-account", "5678"] {
            assert!(!json.contains(private));
        }
    }
}
