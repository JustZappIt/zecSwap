use std::sync::Mutex;
use std::time::Duration;

use anyhow::{Result, ensure};
use futures_util::{StreamExt, stream};
use serde::Serialize;
use tokio::time::{Instant, timeout, timeout_at};
use zecswap_api::service::MakerInfo;
use zecswap_chain::evm::{B256, Stage};
use zecswap_chain::zcash::Funds;

use super::{Maker, unix_now};
use crate::store::{MonitorCounts, MonitorSwap};

pub(super) struct Monitoring {
    token_hash: Option<blake2b_simd::Hash>,
    started_at: u64,
    state: Mutex<Runtime>,
}

#[derive(Clone, Default, Serialize)]
#[serde(rename_all = "camelCase")]
struct Runtime {
    last_sync_at: Option<u64>,
    last_sync_ok: Option<bool>,
    swap_errors_since_start: u64,
    failed_swap_ids: Vec<B256>,
    #[serde(skip)]
    cached_funds: Option<Funds>,
}

impl Monitoring {
    pub(super) fn new(token: Option<&str>) -> Self {
        Self {
            token_hash: token.map(|value| blake2b_simd::blake2b(value.as_bytes())),
            started_at: unix_now(),
            state: Mutex::new(Runtime::default()),
        }
    }

    pub(super) fn from_env() -> Result<Self> {
        let token = match std::env::var("MAKER_MONITOR_TOKEN") {
            Ok(value) => Some(zeroize::Zeroizing::new(value)),
            Err(std::env::VarError::NotPresent) => None,
            Err(error) => return Err(error.into()),
        };
        if let Some(token) = &token {
            ensure!(
                token.len() >= 32,
                "MAKER_MONITOR_TOKEN must be at least 32 characters"
            );
        }
        Ok(Self::new(token.as_ref().map(|value| value.as_str())))
    }

    fn authorized(&self, header: Option<&str>) -> bool {
        let Some(expected) = self.token_hash else {
            return false;
        };
        let Some(value) = header.and_then(|value| value.strip_prefix("Bearer ")) else {
            return false;
        };
        // Hash equality is constant time; both hashes have the same fixed length.
        blake2b_simd::blake2b(value.as_bytes()) == expected
    }
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct MonitorSnapshot {
    schema_version: u32,
    generated_at: u64,
    deployment: MakerInfo,
    uptime_seconds: u64,
    watchtower_healthy: bool,
    last_pass_age_seconds: Option<u64>,
    runtime: Runtime,
    counts: MonitorCounts,
    inventory: Inventory,
    pricing: crate::market::PriceSnapshot,
    notifications: crate::store::NotificationStatus,
    transactions: crate::store::TransactionStatus,
    policy: Policy,
    swaps: Vec<MonitorSwap>,
    swap_limit: usize,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct Inventory {
    usdc_available: Option<String>,
    usdc_wallet: Option<String>,
    maker_gas_wei: Option<String>,
    zec_total_zat: Option<String>,
    zec_spendable_zat: Option<String>,
    zec_reserved_zat: String,
    zec_available_zat: Option<String>,
    wallet_busy: bool,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct Policy {
    price_per_zec: String,
    spread_bps: u16,
    unit: String,
    max_units: u32,
    quote_ttl_seconds: u64,
    tick_seconds: u64,
    stale_after_seconds: u64,
    lock_seconds: u64,
    t0_after_seconds: u64,
    t1_after_seconds: u64,
    cancel_after_seconds: u64,
    deposit_confirmations: u32,
    evm_confirmations: Option<u32>,
    reverse_fee_reserve_zat: Option<String>,
}

impl Maker {
    pub(crate) fn monitor_authorized(&self, header: Option<&str>) -> bool {
        self.monitoring.authorized(header)
    }

    pub(super) fn record_monitor_sync(&self, synced: bool) {
        let funds = self.zcash.try_lock().ok().and_then(|zcash| {
            self.inventory
                .as_ref()
                .and_then(|(account, _)| zcash.wallet().ok()?.funds(*account).ok())
        });
        let mut runtime = self.monitoring.state.lock().unwrap();
        runtime.last_sync_ok = Some(synced);
        if synced {
            runtime.last_sync_at = Some(unix_now());
        }
        if let Some(funds) = funds {
            runtime.cached_funds = Some(funds);
        }
    }

    pub(super) fn record_monitor_errors(&self, failed: Vec<B256>) {
        let mut runtime = self.monitoring.state.lock().unwrap();
        runtime.swap_errors_since_start = runtime
            .swap_errors_since_start
            .saturating_add(failed.len() as u64);
        runtime.failed_swap_ids = failed;
    }

    /// No syncing, proving, broadcasts or inventory writes. Slow RPCs and a busy wallet
    /// produce missing readings, so monitoring cannot queue behind a financial operation.
    pub(crate) async fn monitor_snapshot(&self) -> Result<MonitorSnapshot> {
        const LIMIT: usize = 50;
        let counts = self.store.monitor_counts()?;
        let mut swaps = self
            .store
            .monitor_swaps(self.config.timing.t0_after, LIMIT)?;
        let runtime = self.monitoring.state.lock().unwrap().clone();
        for swap in &mut swaps {
            swap.last_pass_failed = runtime.failed_swap_ids.contains(&swap.id);
        }
        let reserved = self.reverse_reserved()?;
        let (funds, wallet_busy) = match self.zcash.try_lock() {
            Ok(zcash) => {
                for swap in &mut swaps {
                    if let Some(funds) =
                        uuid::Uuid::parse_str(&swap.account)
                            .ok()
                            .and_then(|account| {
                                zcash
                                    .wallet()
                                    .ok()?
                                    .funds(zecswap_chain::zcash::AccountUuid::from_uuid(account))
                                    .ok()
                            })
                    {
                        swap.zec_total_zat = Some(funds.total.to_string());
                        swap.zec_spendable_zat = Some(funds.spendable.to_string());
                    }
                }
                (
                    self.inventory
                        .as_ref()
                        .and_then(|(account, _)| zcash.wallet().ok()?.funds(*account).ok()),
                    false,
                )
            }
            Err(_) => (runtime.cached_funds, true),
        };
        let readings = async {
            let duration = Duration::from_secs(3);
            let (available, wallet, gas) = tokio::join!(
                timeout(
                    duration,
                    self.settlement.balance_of(self.account, self.config.token)
                ),
                timeout(
                    duration,
                    self.settlement
                        .token_balance(self.config.token, self.account)
                ),
                timeout(duration, self.settlement.eth_balance(self.account)),
            );
            (
                available.ok().and_then(Result::ok),
                wallet.ok().and_then(Result::ok),
                gas.ok().and_then(Result::ok),
            )
        };
        let observations = async {
            let ids: Vec<_> = swaps.iter().map(|swap| swap.id).collect();
            let mut pending = stream::iter(ids)
                .map(|id| async move {
                    (
                        id,
                        timeout(Duration::from_secs(2), self.settlement.swap(id)).await,
                    )
                })
                .buffer_unordered(4);
            let deadline = Instant::now() + Duration::from_secs(6);
            let mut observed = Vec::new();
            while let Ok(Some((id, result))) = timeout_at(deadline, pending.next()).await {
                if let Ok(Ok(chain)) = result {
                    observed.push((id, chain));
                }
            }
            observed
        };
        let ((available, wallet, gas), observed, ()) =
            tokio::join!(readings, observations, self.prices.refresh());
        for (id, chain) in observed {
            let swap = swaps.iter_mut().find(|swap| swap.id == id).unwrap();
            swap.chain_observed = true;
            if let Some(chain) = chain {
                swap.stage = Some(
                    match chain.stage {
                        Stage::Open => "open",
                        Stage::Ready => "ready",
                        Stage::Claimed => "claimed",
                        Stage::Refunded => "refunded",
                    }
                    .into(),
                );
                swap.paid_out = Some(chain.paid_out);
                swap.ready_deadline = chain.t0;
                swap.refund_after = chain.t1;
                swap.claim_lock_until = Some(chain.claim_lock_until);
                swap.refund_lock_until = Some(chain.refund_lock_until);
            }
        }
        let pricing = self.prices.snapshot(unix_now());
        for swap in &mut swaps {
            swap.evm_transactions = self
                .store
                .evm_transactions(&self.transaction_scope(), swap.id)?;
        }
        Ok(MonitorSnapshot {
            transactions: self.store.transaction_status(&self.transaction_scope())?,
            notifications: self.store.notification_status(self.telegram.enabled())?,
            schema_version: 1,
            generated_at: unix_now(),
            deployment: self.info(),
            uptime_seconds: unix_now().saturating_sub(self.monitoring.started_at),
            watchtower_healthy: self.health.check().is_ok(),
            last_pass_age_seconds: self.health.completed_age_seconds(),
            runtime,
            counts,
            inventory: Inventory {
                usdc_available: available.map(|value| value.to_string()),
                usdc_wallet: wallet.map(|value| value.to_string()),
                maker_gas_wei: gas.map(|value| value.to_string()),
                zec_total_zat: funds.map(|value| value.total.to_string()),
                zec_spendable_zat: funds.map(|value| value.spendable.to_string()),
                zec_reserved_zat: reserved.to_string(),
                zec_available_zat: funds
                    .map(|value| value.spendable.saturating_sub(reserved).to_string()),
                wallet_busy,
            },
            policy: Policy {
                price_per_zec: pricing.price_per_zec.clone().unwrap_or_else(|| "0".into()),
                spread_bps: self.config.pricing.spread_bps,
                unit: self.config.pricing.unit.to_string(),
                max_units: self.config.pricing.max_units,
                quote_ttl_seconds: self.config.timing.quote_ttl,
                tick_seconds: self.config.timing.tick,
                stale_after_seconds: self.config.timing.tick.saturating_mul(3),
                lock_seconds: self.lock_duration,
                t0_after_seconds: self.config.timing.t0_after,
                t1_after_seconds: self.config.timing.t1_after,
                cancel_after_seconds: self.config.timing.cancel_after,
                deposit_confirmations: self.config.confirmations.map_or(10, |value| value.get()),
                evm_confirmations: self
                    .config
                    .reverse
                    .as_ref()
                    .map(|value| value.evm_confirmations.get()),
                reverse_fee_reserve_zat: self
                    .config
                    .reverse
                    .as_ref()
                    .map(|value| value.fee_reserve_zat.to_string()),
            },
            pricing,
            swaps,
            swap_limit: LIMIT,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn monitoring_is_closed_by_default_and_requires_the_exact_bearer_token() {
        assert!(!Monitoring::new(None).authorized(Some("Bearer anything")));
        let monitor = Monitoring::new(Some("test-token"));
        assert!(monitor.authorized(Some("Bearer test-token")));
        for header in [
            None,
            Some("test-token"),
            Some("Bearer test-tokeN"),
            Some("Bearer "),
        ] {
            assert!(!monitor.authorized(header));
        }
    }
}
