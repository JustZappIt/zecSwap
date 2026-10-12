//! Read-only, bounded operations export. Never serialize a Swap or ReverseSwap here:
//! those types contain viewing keys and authorization material.
use std::collections::HashMap;

use anyhow::Result;
use rusqlite::params;
use serde::Serialize;
use zecswap_chain::evm::B256;

use super::Store;

/// A Zcash txid as a swap stores it: a forward swap's as hex, a reverse swap's in its JSON.
pub(super) fn stored_txid(
    value: String,
    reverse: bool,
    column: usize,
) -> rusqlite::Result<zecswap_chain::zcash::TxId> {
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
    Ok(zecswap_chain::zcash::TxId::from_bytes(bytes))
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

/// A swap as the monitor exports it: what the maker records, never what the chain says, which
/// the dashboard reads itself.
#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct MonitorSwap {
    pub id: B256,
    pub quote_id: B256,
    pub direction: String,
    pub amount: String,
    pub deposit_zat: String,
    pub settled: bool,
    pub archived: bool,
    pub refund_started: bool,
    pub accepted_at: Option<u64>,
    pub opened_at: Option<u64>,
    pub settled_at: Option<u64>,
    pub funding_deadline: Option<u64>,
    pub ready_deadline: u64,
    pub refund_after: u64,
    pub deposit_txid: Option<String>,
    pub sweep_txid: Option<String>,
    /// None where the accept asked nothing back.
    pub token_returned: Option<bool>,
    pub last_pass_failed: bool,
    pub zec_total_zat: Option<String>,
    pub zec_spendable_zat: Option<String>,
    pub evm_transactions: Vec<super::EvmTransaction>,
    pub zcash_flow: Option<super::FlowObservation>,
    #[serde(skip)]
    pub account: String,
}

/// One UTC day's accepts that spent a token, by what became of the token: handed back, walked
/// away from, or neither yet.
#[derive(Debug, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct TokenDay {
    pub day: u64,
    pub accepted: u64,
    pub returned: u64,
    pub walked_away: u64,
    pub open: u64,
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

    /// Up to `limit` swaps, unsettled first, or the one `id` names.
    pub(crate) fn monitor_swaps(&self, limit: usize, id: Option<B256>) -> Result<Vec<MonitorSwap>> {
        let conn = self.conn();
        let mut statement = conn.prepare(
            "SELECT * FROM (
                SELECT s.id, 'forward' AS direction, q.amount, cast(q.deposit_zat AS TEXT),
                    s.settled, NULL, s.t0, s.t1, NULL,
                    CASE WHEN s.sweep_txid IS NULL THEN NULL ELSE lower(hex(s.sweep_txid)) END,
                    q.nonce, s.zcash_account, q.quote_id, s.archived, s.refund_started, a.at,
                    s.opened_at, s.settled_at,
                    CASE WHEN s.token_request IS NULL THEN NULL ELSE s.token_return IS NOT NULL END
                FROM swaps s JOIN quotes q USING (quote_id) LEFT JOIN swap_accepted a ON a.id = s.id
                UNION ALL
                SELECT s.id, 'reverse', q.amount, cast(q.deposit_zat AS TEXT),
                    s.settled, json_extract(r.terms, '$.fundingDeadline'),
                    json_extract(r.terms, '$.readyDeadline'), json_extract(r.terms, '$.refundAfter'),
                    json_extract(s.data, '$.deposit'), json_extract(s.data, '$.sweep'), q.nonce,
                    json_extract(s.data, '$.account'), q.quote_id, s.archived, 0, a.at, NULL,
                    s.settled_at,
                    CASE WHEN json_extract(s.data, '$.acceptance.tokenRequest') IS NULL THEN NULL
                        ELSE json_extract(s.data, '$.token_return') IS NOT NULL END
                FROM reverse_swaps s JOIN quotes q USING (quote_id)
                    JOIN reverse_quotes r USING (quote_id) LEFT JOIN swap_accepted a ON a.id = s.id
            ) WHERE ?2 IS NULL OR id = ?2 ORDER BY settled ASC, nonce DESC LIMIT ?1",
        )?;
        Ok(statement
            .query_map(
                params![limit as u64, id.as_ref().map(B256::as_slice)],
                |row| {
                    Ok(MonitorSwap {
                        id: B256::from(row.get::<_, [u8; 32]>(0)?),
                        quote_id: B256::from(row.get::<_, [u8; 32]>(12)?),
                        direction: row.get(1)?,
                        amount: row.get(2)?,
                        deposit_zat: row.get(3)?,
                        settled: row.get(4)?,
                        archived: row.get(13)?,
                        refund_started: row.get(14)?,
                        accepted_at: row.get(15)?,
                        opened_at: row.get(16)?,
                        settled_at: row.get(17)?,
                        funding_deadline: row.get(5)?,
                        ready_deadline: row.get(6)?,
                        refund_after: row.get(7)?,
                        deposit_txid: row
                            .get::<_, Option<String>>(8)?
                            .map(|value| stored_txid(value, true, 8).map(|txid| txid.to_string()))
                            .transpose()?,
                        sweep_txid: {
                            let txid: Option<String> = row.get(9)?;
                            let reverse = row.get::<_, String>(1)? == "reverse";
                            txid.map(|value| {
                                stored_txid(value, reverse, 9).map(|txid| txid.to_string())
                            })
                            .transpose()?
                        },
                        token_returned: row.get(18)?,
                        last_pass_failed: false,
                        zec_total_zat: None,
                        zec_spendable_zat: None,
                        evm_transactions: Vec::new(),
                        zcash_flow: None,
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
                },
            )?
            .collect::<Result<_, _>>()?)
    }

    /// The last seven UTC days up to `today`, newest first, by the day each accept that spent
    /// a token was made on: before accept times were recorded, its quote's expiry, which is at
    /// most a quote's lifetime later.
    pub(crate) fn token_days(&self, today: u64) -> Result<Vec<TokenDay>> {
        const DAYS: u64 = 7;
        let conn = self.conn();
        let mut statement = conn.prepare(
            "SELECT day, count(*), sum(returned), sum(settled AND NOT returned),
                sum(NOT settled AND NOT returned)
             FROM (
                SELECT coalesce(a.at, q.expires_at) / 86400 AS day, s.token_return IS NOT NULL AS returned,
                    s.settled
                FROM swaps s JOIN quotes q USING (quote_id) LEFT JOIN swap_accepted a ON a.id = s.id
                WHERE s.token_request IS NOT NULL
                UNION ALL
                SELECT coalesce(a.at, q.expires_at) / 86400,
                    json_extract(s.data, '$.token_return') IS NOT NULL, s.settled
                FROM reverse_swaps s JOIN quotes q USING (quote_id) LEFT JOIN swap_accepted a ON a.id = s.id
                WHERE json_extract(s.data, '$.acceptance.tokenRequest') IS NOT NULL
             ) WHERE day > ?1 - ?2 AND day <= ?1 GROUP BY day",
        )?;
        let mut counted: HashMap<u64, TokenDay> = statement
            .query_map(params![today, DAYS], |row| {
                Ok(TokenDay {
                    day: row.get(0)?,
                    accepted: row.get(1)?,
                    returned: row.get(2)?,
                    walked_away: row.get(3)?,
                    open: row.get(4)?,
                })
            })?
            .map(|day| day.map(|day| (day.day, day)))
            .collect::<rusqlite::Result<_>>()?;
        Ok((0..DAYS.min(today + 1))
            .map(|ago| today - ago)
            .map(|day| {
                counted.remove(&day).unwrap_or(TokenDay {
                    day,
                    accepted: 0,
                    returned: 0,
                    walked_away: 0,
                    open: 0,
                })
            })
            .collect())
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
        assert!(store.monitor_swaps(50, None).unwrap().is_empty());
    }

    /// Records a forward swap as raw rows: its quote expiring at `expires_at`, accepted at
    /// `accepted_at` if that was recorded, with a token request and its return as given.
    fn forward(
        store: &Store,
        id: u8,
        settled: bool,
        expires_at: u64,
        accepted_at: Option<u64>,
        token: (bool, bool),
    ) {
        let conn = store.conn();
        conn.execute(
            "INSERT INTO quotes VALUES (?1, ?2, X'00', NULL, ?3, 123, ?4, 1)",
            params![[id; 32], id, u128::MAX.to_string(), expires_at],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO swaps (id, quote_id, user_share, viewing_keys, zcash_account, opened_at,
                token, t0, t1, sweep_txid, settled, token_request, token_return)
             VALUES (?1, ?1, X'1234', X'5678', 'private-account', 100, zeroblob(20), 300, 700,
                NULL, ?2, ?3, ?4)",
            params![
                [id; 32],
                settled,
                token.0.then_some(&b"request"[..]),
                token.1.then_some("signature")
            ],
        )
        .unwrap();
        if let Some(at) = accepted_at {
            conn.execute(
                "INSERT INTO swap_accepted (id, at) VALUES (?1, ?2)",
                params![[id; 32], at],
            )
            .unwrap();
        }
    }

    #[test]
    fn export_is_bounded_prioritizes_active_and_preserves_large_amounts() {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::open(&dir.path().join("maker.sqlite")).unwrap();
        for (id, settled) in [(1u8, false), (2, true), (3, true)] {
            forward(&store, id, settled, 999, None, (false, false));
        }
        let rows = store.monitor_swaps(2, None).unwrap();
        assert_eq!(rows.len(), 2);
        assert_eq!(rows[0].id, B256::repeat_byte(1));
        assert_eq!(rows[1].id, B256::repeat_byte(3));
        assert_eq!(rows[0].quote_id, B256::repeat_byte(1));
        assert_eq!(rows[0].amount, u128::MAX.to_string());
        assert_eq!(rows[0].ready_deadline, 300);
        assert_eq!(rows[0].opened_at, Some(100));
        assert_eq!((rows[0].accepted_at, rows[0].token_returned), (None, None));
        // Any one swap, settled or not, by its id alone.
        let one = store.monitor_swaps(1, Some(B256::repeat_byte(2))).unwrap();
        assert_eq!(one.len(), 1);
        assert_eq!(one[0].id, B256::repeat_byte(2));
        assert!(one[0].settled);
        assert!(
            store
                .monitor_swaps(1, Some(B256::repeat_byte(9)))
                .unwrap()
                .is_empty()
        );
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

    /// Each day's accepts that spent a token, told apart by what became of the token: handed
    /// back, walked away from, or neither yet. An accept that spent none is left out, a reverse
    /// swap counts like a forward one, and a swap from before accept times were recorded counts
    /// on its quote's expiry.
    #[test]
    fn token_days_tell_walk_aways_from_returns() {
        const DAY: u64 = 86_400;
        let today = 20_000;
        let dir = tempfile::tempdir().unwrap();
        let store = Store::open(&dir.path().join("maker.sqlite")).unwrap();
        let at = |day: u64| Some(day * DAY + 3_600);
        forward(&store, 1, true, 0, at(today), (true, true));
        forward(&store, 2, true, 0, at(today), (true, false));
        forward(&store, 3, false, 0, at(today), (true, false));
        forward(&store, 4, true, 0, at(today), (false, false));
        forward(&store, 5, true, 0, at(today - 1), (true, false));
        forward(&store, 6, true, 0, at(today - 7), (true, false));
        forward(&store, 7, false, today * DAY + 60, None, (true, true));
        {
            let conn = store.conn();
            conn.execute(
                "INSERT INTO quotes VALUES (?1, 8, X'00', NULL, '1', 1, 0, 1)",
                [[8u8; 32]],
            )
            .unwrap();
            conn.execute(
                r#"INSERT INTO reverse_quotes (quote_id, terms)
                   VALUES (?1, '{"fundingDeadline": 1, "readyDeadline": 2, "refundAfter": 3}')"#,
                [[8u8; 32]],
            )
            .unwrap();
            conn.execute(
                "INSERT INTO reverse_swaps (id, quote_id, data, settled) VALUES (?1, ?1, ?2, 1)",
                params![
                    [8u8; 32],
                    r#"{"account": [0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0],
                        "acceptance": {"tokenRequest": "request"}, "token_return": null}"#
                ],
            )
            .unwrap();
            conn.execute(
                "INSERT INTO swap_accepted (id, at) VALUES (?1, ?2)",
                params![[8u8; 32], at(today - 1)],
            )
            .unwrap();
        }
        let days = store.token_days(today).unwrap();
        assert_eq!(days.len(), 7);
        let day = |day, accepted, returned, walked_away, open| TokenDay {
            day,
            accepted,
            returned,
            walked_away,
            open,
        };
        assert_eq!(days[0], day(today, 4, 2, 1, 1));
        assert_eq!(days[1], day(today - 1, 2, 0, 2, 0));
        assert_eq!(
            &days[2..],
            (2..7)
                .map(|ago| day(today - ago, 0, 0, 0, 0))
                .collect::<Vec<_>>()
        );
        let rows = store.monitor_swaps(50, None).unwrap();
        let returned = |id: u8| {
            rows.iter()
                .find(|row| row.id == B256::repeat_byte(id))
                .unwrap()
                .token_returned
        };
        assert_eq!(
            [1, 2, 4, 8].map(returned),
            [Some(true), Some(false), None, Some(false)]
        );
    }
}
