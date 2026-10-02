use anyhow::Result;
use rusqlite::{OptionalExtension, params};
use serde::{Deserialize, Serialize};
use zecswap_chain::evm::B256;

use super::{Notification, Store, notifications};

pub(super) const SCHEMA: &str = "
    CREATE TABLE IF NOT EXISTS evm_transaction_info (
        scope TEXT NOT NULL, transaction_hash BLOB NOT NULL, uses_railgun INTEGER NOT NULL,
        PRIMARY KEY(scope, transaction_hash)
    );
    CREATE TABLE IF NOT EXISTS bridge_flow (
        scope TEXT NOT NULL, swap_id BLOB NOT NULL, observation TEXT NOT NULL,
        PRIMARY KEY(scope, swap_id)
    );
    CREATE TABLE IF NOT EXISTS bridge_flow_status (
        scope TEXT PRIMARY KEY, updated_at INTEGER, last_error TEXT
    );
    CREATE TABLE IF NOT EXISTS bridge_flow_start (scope TEXT PRIMARY KEY, started_at INTEGER NOT NULL);
";

#[derive(Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct ZecTransaction {
    pub txid: String,
    pub kind: String,
    pub state: String,
    pub mined_height: Option<u32>,
    pub confirmations: u32,
    pub block_time: Option<u64>,
    pub value_zat: String,
}

#[derive(Clone, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct FlowObservation {
    pub observed_at: u64,
    pub scanned_height: Option<u32>,
    pub required_confirmations: u32,
    pub deposit_verified: bool,
    pub confirmed_deposit_zat: String,
    pub confirmed_spent_zat: String,
    pub transactions: Vec<ZecTransaction>,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct FlowStatus {
    pub updated_at: Option<u64>,
    pub last_error: Option<String>,
}

impl Store {
    pub(crate) fn init_flow_observer(&self, scope: &str, now: u64) -> Result<u64> {
        let conn = self.conn();
        conn.execute(
            "INSERT OR IGNORE INTO bridge_flow_start VALUES (?1,?2)",
            params![scope, now],
        )?;
        Ok(conn.query_row(
            "SELECT started_at FROM bridge_flow_start WHERE scope = ?1",
            [scope],
            |row| row.get(0),
        )?)
    }
    pub(crate) fn save_evm_info(&self, scope: &str, hash: B256, uses_railgun: bool) -> Result<()> {
        self.conn().execute(
            "INSERT INTO evm_transaction_info VALUES (?1,?2,?3)
            ON CONFLICT(scope,transaction_hash) DO UPDATE SET uses_railgun = excluded.uses_railgun",
            params![scope, hash.as_slice(), uses_railgun],
        )?;
        Ok(())
    }

    pub(crate) fn evm_info(&self, scope: &str, hash: B256) -> Result<Option<bool>> {
        Ok(self.conn().query_row("SELECT uses_railgun FROM evm_transaction_info WHERE scope = ?1 AND transaction_hash = ?2",
            params![scope,hash.as_slice()], |row| row.get(0)).optional()?)
    }
    pub(crate) fn save_flow_observations(
        &self,
        scope: &str,
        observations: &[(B256, FlowObservation)],
        alerts: &[Notification],
        now: u64,
    ) -> Result<()> {
        let mut conn = self.conn();
        let tx = conn.transaction()?;
        for (id, observation) in observations {
            tx.execute(
                "INSERT INTO bridge_flow VALUES (?1, ?2, ?3)
                ON CONFLICT(scope,swap_id) DO UPDATE SET observation = excluded.observation",
                params![scope, id.as_slice(), serde_json::to_string(observation)?],
            )?;
        }
        for alert in alerts {
            notifications::insert(&tx, Some(alert))?;
        }
        tx.execute(
            "INSERT INTO bridge_flow_status VALUES (?1,?2,NULL)
            ON CONFLICT(scope) DO UPDATE SET updated_at = excluded.updated_at, last_error = NULL",
            params![scope, now],
        )?;
        tx.commit()?;
        Ok(())
    }

    pub(crate) fn flow_observation(
        &self,
        scope: &str,
        id: B256,
    ) -> Result<Option<FlowObservation>> {
        self.conn()
            .query_row(
                "SELECT observation FROM bridge_flow WHERE scope = ?1 AND swap_id = ?2",
                params![scope, id.as_slice()],
                |row| row.get::<_, String>(0),
            )
            .optional()?
            .map(|value| serde_json::from_str(&value).map_err(Into::into))
            .transpose()
    }

    pub(crate) fn flow_error(&self, scope: &str) -> Result<()> {
        self.conn().execute(
            "INSERT INTO bridge_flow_status VALUES (?1,NULL,?2)
            ON CONFLICT(scope) DO UPDATE SET last_error = excluded.last_error",
            params![scope, "Zcash flow observation unavailable; retrying"],
        )?;
        Ok(())
    }

    pub(crate) fn flow_status(&self, scope: &str) -> Result<FlowStatus> {
        Ok(self
            .conn()
            .query_row(
                "SELECT updated_at,last_error FROM bridge_flow_status WHERE scope = ?1",
                [scope],
                |row| {
                    Ok(FlowStatus {
                        updated_at: row.get(0)?,
                        last_error: row.get(1)?,
                    })
                },
            )
            .optional()?
            .unwrap_or(FlowStatus {
                updated_at: None,
                last_error: None,
            }))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn observations_are_scoped_durable_and_replace_reorged_evidence() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("maker.sqlite");
        let store = Store::open(&path).unwrap();
        let id = B256::repeat_byte(1);
        let observation = FlowObservation {
            observed_at: 1000,
            deposit_verified: true,
            confirmed_deposit_zat: "100".into(),
            ..Default::default()
        };
        assert_eq!(store.init_flow_observer("testnet", 1000).unwrap(), 1000);
        assert_eq!(store.init_flow_observer("testnet", 2000).unwrap(), 1000);
        store
            .save_flow_observations("testnet", &[(id, observation)], &[], 1000)
            .unwrap();
        drop(store);
        let store = Store::open(&path).unwrap();
        assert!(
            store
                .flow_observation("testnet", id)
                .unwrap()
                .unwrap()
                .deposit_verified
        );
        assert!(store.flow_observation("mainnet", id).unwrap().is_none());
        store.flow_error("testnet").unwrap();
        assert!(store.flow_status("testnet").unwrap().last_error.is_some());
        store
            .save_flow_observations("testnet", &[(id, FlowObservation::default())], &[], 2000)
            .unwrap();
        assert!(
            !store
                .flow_observation("testnet", id)
                .unwrap()
                .unwrap()
                .deposit_verified
        );
        assert_eq!(store.flow_status("testnet").unwrap().updated_at, Some(2000));
        assert!(store.flow_status("testnet").unwrap().last_error.is_none());
        store.save_evm_info("testnet", id, false).unwrap();
        assert_eq!(store.evm_info("testnet", id).unwrap(), Some(false));
        assert_eq!(store.evm_info("mainnet", id).unwrap(), None);
    }
}
