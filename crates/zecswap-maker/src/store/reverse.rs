use anyhow::{Context, Result, ensure};
use rusqlite::{OptionalExtension, params};
use serde::{Deserialize, Serialize};
use zecswap_api::{Acceptance, reverse::Quote};
use zecswap_chain::evm::B256;
use zecswap_chain::zcash::{AccountUuid, TxId};
use zecswap_core::Terms;

use super::Store;

pub(super) const SCHEMA: &str = "
    CREATE TABLE IF NOT EXISTS reverse_quotes (
        quote_id BLOB PRIMARY KEY REFERENCES quotes(quote_id),
        terms TEXT NOT NULL
    );
    CREATE TABLE IF NOT EXISTS reverse_swaps (
        id BLOB PRIMARY KEY,
        quote_id BLOB NOT NULL UNIQUE REFERENCES reverse_quotes(quote_id),
        data TEXT NOT NULL,
        settled INTEGER NOT NULL DEFAULT 0
    );
";

#[derive(Clone, Serialize, Deserialize)]
pub(crate) struct ReverseSwap {
    pub id: B256,
    pub nonce: u64,
    pub quote: Quote,
    pub acceptance: Acceptance,
    pub account: AccountUuid,
    #[serde(with = "optional_txid")]
    pub deposit: Option<TxId>,
    #[serde(with = "optional_txid")]
    pub sweep: Option<TxId>,
    pub settled: bool,
}

impl ReverseSwap {
    /// What `openReverse` commits the escrow to, which every call on it supplies again.
    pub(crate) fn terms(&self) -> Terms {
        self.quote.open(self.acceptance.user_share).terms()
    }
}

mod optional_txid {
    use serde::{Deserialize, Deserializer, Serialize, Serializer};
    use zecswap_chain::zcash::TxId;

    pub fn serialize<S: Serializer>(
        value: &Option<TxId>,
        serializer: S,
    ) -> Result<S::Ok, S::Error> {
        value.map(|id| *id.as_ref()).serialize(serializer)
    }

    pub fn deserialize<'de, D: Deserializer<'de>>(
        deserializer: D,
    ) -> Result<Option<TxId>, D::Error> {
        Ok(Option::<[u8; 32]>::deserialize(deserializer)?.map(TxId::from_bytes))
    }
}

impl Store {
    pub(crate) fn insert_reverse_quote(
        &self,
        build: impl FnOnce(u64) -> Result<Quote>,
    ) -> Result<Quote> {
        let mut conn = self.conn();
        let tx = conn.transaction()?;
        let nonce = tx.query_row("SELECT IFNULL(MAX(nonce) + 1, 0) FROM quotes", [], |row| {
            row.get(0)
        })?;
        let quote = build(nonce)?;
        tx.execute(
            "INSERT INTO quotes (quote_id, nonce, payout, payout_note, amount, deposit_zat, expires_at)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
            params![quote.terms.quote_id.as_slice(), nonce, quote.user.as_slice(), quote.refund_note.as_slice(),
                quote.terms.amount.to_string(), quote.terms.deposit_zat, quote.terms.expires_at],
        )?;
        tx.execute(
            "INSERT INTO reverse_quotes (quote_id, terms) VALUES (?1, ?2)",
            params![
                quote.terms.quote_id.as_slice(),
                serde_json::to_string(&quote)?
            ],
        )?;
        tx.commit()?;
        Ok(quote)
    }

    pub(crate) fn reverse_quote(&self, id: B256) -> Result<Option<(u64, Quote)>> {
        let row: Option<(u64, String)> = self.conn().query_row(
            "SELECT q.nonce, r.terms FROM quotes q JOIN reverse_quotes r USING (quote_id) WHERE quote_id = ?1",
            params![id.as_slice()], |row| Ok((row.get(0)?, row.get(1)?)),
        ).optional()?;
        row.map(|(nonce, json)| Ok((nonce, serde_json::from_str(&json)?)))
            .transpose()
    }

    pub(crate) fn insert_reverse_swap(
        &self,
        swap: &ReverseSwap,
        now: u64,
        event: Option<&super::Notification>,
    ) -> Result<()> {
        let mut conn = self.conn();
        let tx = conn.transaction()?;
        let taken = tx.execute("UPDATE quotes SET accepted = 1 WHERE quote_id = ?1 AND accepted = 0 AND expires_at > ?2",
            params![swap.quote.terms.quote_id.as_slice(), now])?;
        ensure!(taken == 1, "reverse quote is expired or already accepted");
        tx.execute(
            "INSERT INTO reverse_swaps (id, quote_id, data) VALUES (?1, ?2, ?3)",
            params![
                swap.id.as_slice(),
                swap.quote.terms.quote_id.as_slice(),
                serde_json::to_string(swap)?
            ],
        )?;
        super::notifications::insert(&tx, event)?;
        tx.commit()?;
        Ok(())
    }

    pub(crate) fn reverse_swap(&self, id: B256) -> Result<Option<ReverseSwap>> {
        let data: Option<String> = self
            .conn()
            .query_row(
                "SELECT data FROM reverse_swaps WHERE id = ?1",
                params![id.as_slice()],
                |row| row.get(0),
            )
            .optional()?;
        data.map(|json| serde_json::from_str(&json).context("reading reverse swap"))
            .transpose()
    }

    pub(crate) fn pending_reverse_swaps(&self) -> Result<Vec<ReverseSwap>> {
        self.reverse_swaps(false)
    }

    pub(crate) fn watched_reverse_swaps(&self) -> Result<Vec<ReverseSwap>> {
        self.reverse_swaps(true)
    }

    fn reverse_swaps(&self, include_settled: bool) -> Result<Vec<ReverseSwap>> {
        let conn = self.conn();
        let mut statement = conn.prepare(
            "SELECT data FROM reverse_swaps WHERE settled = 0 OR ?1 ORDER BY settled, rowid",
        )?;
        statement
            .query_map([include_settled], |row| row.get::<_, String>(0))?
            .map(|row| Ok(serde_json::from_str(&row?)?))
            .collect()
    }

    pub(crate) fn save_reverse_swap(&self, swap: &ReverseSwap) -> Result<()> {
        self.save_reverse_swap_with_notification(swap, None)
    }

    pub(crate) fn save_reverse_swap_with_notification(
        &self,
        swap: &ReverseSwap,
        event: Option<&super::Notification>,
    ) -> Result<()> {
        let mut conn = self.conn();
        let tx = conn.transaction()?;
        ensure!(
            tx.execute(
                "UPDATE reverse_swaps SET data = ?2, settled = ?3 WHERE id = ?1",
                params![
                    swap.id.as_slice(),
                    serde_json::to_string(swap)?,
                    swap.settled
                ]
            )? == 1,
            "reverse swap is missing"
        );
        super::notifications::insert(&tx, event)?;
        tx.commit()?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use rand::{rand_core::UnwrapErr, rngs::SysRng};
    use zecswap_chain::evm::{Address, swap_id};
    use zecswap_core::{
        NetworkType, Payout, SwapContext, ViewingKeys, derive_maker_share, derive_user_keys,
    };

    #[test]
    fn reverse_reservations_are_isolated_and_resume_after_reopening() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("maker.sqlite");
        let store = Store::open(&path).unwrap();
        let user = derive_user_keys(&[7; 64], NetworkType::Test, 0, 0).unwrap();
        let context = SwapContext {
            chain_id: 1,
            contract: [1; 20],
            quote_id: [2; 32],
        };
        let quote = store
            .insert_reverse_quote(|nonce| {
                assert_eq!(nonce, 0);
                let maker = derive_maker_share(&[9; 32], nonce).unwrap();
                Ok(Quote {
                    terms: zecswap_api::Quote {
                        quote_id: context.quote_id.into(),
                        maker: Address::repeat_byte(3),
                        maker_share: maker.public(),
                        maker_proof: context.prove_maker(&maker, UnwrapErr(SysRng)),
                        chain_id: 1,
                        contract: context.contract.into(),
                        token: Address::repeat_byte(4),
                        amount: u128::from(u64::MAX) + 1,
                        deposit_zat: 100_000,
                        expires_at: 200,
                    },
                    user: user.auth.address().into(),
                    refund_note: B256::repeat_byte(5),
                    funding_deadline: 300,
                    ready_deadline: 500,
                    refund_after: 700,
                })
            })
            .unwrap();
        assert!(store.take_quote(&context.quote_id, 100).unwrap().is_none());
        assert_eq!(
            store
                .insert_quote([3; 32], Address::repeat_byte(4), None, 1, 1, 200)
                .unwrap(),
            1
        );
        let acceptance = Acceptance {
            user_share: user.share.public(),
            user_proof: context.prove_user(
                &quote.terms.maker_share,
                &user.share,
                &Payout {
                    user: user.auth.address(),
                    note: Some(quote.refund_note.0),
                },
                UnwrapErr(SysRng),
            ),
            viewing_keys: ViewingKeys::random(UnwrapErr(SysRng)),
        };
        let mut swap = ReverseSwap {
            id: swap_id(quote.user, &quote.terms.maker_share),
            nonce: 0,
            quote,
            acceptance,
            account: AccountUuid::from_uuid(uuid::Uuid::nil()),
            deposit: None,
            sweep: None,
            settled: false,
        };
        assert!(store.insert_reverse_swap(&swap, 200, None).is_err());
        store.conn().execute_batch("CREATE TRIGGER reject_alert BEFORE INSERT ON notifications WHEN NEW.event_key = 'reject' BEGIN SELECT RAISE(FAIL, 'injected queue failure'); END;").unwrap();
        let bad = super::super::Notification {
            key: "reject".into(),
            text: "test".into(),
            created_at: 100,
        };
        assert!(store.insert_reverse_swap(&swap, 100, Some(&bad)).is_err());
        assert!(store.pending_reverse_swaps().unwrap().is_empty());
        let accepted = super::super::Notification {
            key: "accepted".into(),
            ..bad
        };
        store
            .insert_reverse_swap(&swap, 100, Some(&accepted))
            .unwrap();
        assert!(store.insert_reverse_swap(&swap, 100, None).is_err());
        swap.deposit = Some(TxId::from_bytes([8; 32]));
        store.save_reverse_swap(&swap).unwrap();
        drop(store);

        let store = Store::open(&path).unwrap();
        let restored = store.reverse_swap(swap.id).unwrap().unwrap();
        assert_eq!(restored.deposit, swap.deposit);
        assert_eq!(restored.quote.terms.amount, u128::from(u64::MAX) + 1);
        assert_eq!(
            restored.acceptance.viewing_keys.to_bytes(),
            swap.acceptance.viewing_keys.to_bytes()
        );
        assert_eq!(store.pending_reverse_swaps().unwrap().len(), 1);
        let rows = store.monitor_swaps(50).unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].direction, "reverse");
        assert_eq!(rows[0].funding_deadline, Some(300));
        assert_eq!(rows[0].ready_deadline, 500);
        assert_eq!(rows[0].refund_after, 700);
        assert_eq!(rows[0].deposit_txid, swap.deposit.map(|id| id.to_string()));
        assert_eq!(rows[0].account, swap.account.expose_uuid().to_string());
        let exported = serde_json::to_string(&rows).unwrap();
        assert!(!exported.contains("viewingKeys"));
        assert!(!exported.contains("acceptance"));
        assert!(!exported.contains(&swap.account.expose_uuid().to_string()));
        swap.settled = true;
        let bad = super::super::Notification {
            key: "reject".into(),
            text: "test".into(),
            created_at: 101,
        };
        assert!(
            store
                .save_reverse_swap_with_notification(&swap, Some(&bad))
                .is_err()
        );
        assert!(!store.reverse_swap(swap.id).unwrap().unwrap().settled);
        let finished = super::super::Notification {
            key: "finished".into(),
            ..bad
        };
        store
            .save_reverse_swap_with_notification(&swap, Some(&finished))
            .unwrap();
        assert_eq!(store.notification_status(true).unwrap().pending, 2);
        assert!(store.pending_reverse_swaps().unwrap().is_empty());
        assert!(store.reverse_swap(swap.id).unwrap().unwrap().settled);
    }
}
