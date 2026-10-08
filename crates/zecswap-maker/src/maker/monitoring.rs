use std::sync::{Arc, Mutex};

use anyhow::Result;
use serde::Serialize;
use zecswap_api::server::MonitorToken;
use zecswap_api::service::MakerInfo;
use zecswap_chain::evm::{Address, B256};
use zecswap_chain::zcash::{AccountUuid, Funds, Wallet};

use super::{Maker, unix_now};
use crate::store::{MonitorCounts, MonitorSwap, TokenDay};

const SCHEMA_VERSION: u32 = 2;

pub(super) struct Monitoring {
    token: Arc<MonitorToken>,
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
    pub(super) fn from_env() -> Result<Self> {
        Ok(Self::new(MonitorToken::from_env("MAKER_MONITOR_TOKEN")?))
    }

    pub(super) fn new(token: MonitorToken) -> Self {
        Self {
            token: Arc::new(token),
            started_at: unix_now(),
            state: Mutex::new(Runtime::default()),
        }
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
    counts: Counts,
    inventory: Inventory,
    pricing: crate::market::PriceSnapshot,
    notifications: crate::store::NotificationStatus,
    transactions: crate::store::TransactionStatus,
    zcash_flow: crate::store::FlowStatus,
    gas_alerts: Option<GasAlerts>,
    tokens: Option<Tokens>,
    policy: Policy,
    swaps: Vec<MonitorSwap>,
    swap_limit: usize,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct MonitorSwapSnapshot {
    schema_version: u32,
    generated_at: u64,
    swap: MonitorSwap,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct Counts {
    #[serde(flatten)]
    stored: MonitorCounts,
    awaiting_deposit: usize,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct Inventory {
    zec_total_zat: Option<String>,
    zec_spendable_zat: Option<String>,
    zec_reserved_zat: String,
    zec_available_zat: Option<String>,
    zec_in_flight_zat: Option<String>,
    wallet_busy: bool,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct GasAlerts {
    enabled: bool,
    interval_seconds: u64,
    accounts: Vec<GasAlertAccount>,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct GasAlertAccount {
    label: String,
    address: Address,
    low_wei: String,
    recovery_wei: String,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct Tokens {
    #[serde(flatten)]
    gate: zecswap_tokens::server::Today,
    days: Vec<TokenDay>,
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
    forward_evm_confirmations: u32,
    reverse_fee_reserve_zat: Option<String>,
    max_awaiting_deposit: Option<usize>,
}

impl Maker {
    pub(crate) fn monitor_token(&self) -> Arc<MonitorToken> {
        self.monitoring.token.clone()
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

    /// No chain reads, syncing, proving, broadcasts or inventory writes: the dashboard reads
    /// the chain itself, and a busy wallet leaves its readings missing, so monitoring never
    /// queues behind a financial operation or spends the watchtower's RPC.
    pub(crate) async fn monitor_snapshot(&self) -> Result<MonitorSnapshot> {
        const LIMIT: usize = 50;
        let stored = self.store.monitor_counts()?;
        let mut swaps = self.store.monitor_swaps(LIMIT, None)?;
        let runtime = self.monitoring.state.lock().unwrap().clone();
        let reserved = self.reverse_reserved()?;
        let (funds, in_flight, wallet_busy) = match self.zcash.try_lock() {
            Ok(zcash) => match zcash.wallet() {
                Ok(wallet) => {
                    self.wallet_funds(&mut swaps, wallet);
                    (
                        self.inventory
                            .as_ref()
                            .and_then(|(account, _)| wallet.funds(*account).ok()),
                        self.in_flight(wallet)?,
                        false,
                    )
                }
                Err(_) => (None, None, false),
            },
            Err(_) => (runtime.cached_funds, None, true),
        };
        self.records(&mut swaps, &runtime.failed_swap_ids)?;
        self.prices.refresh().await;
        let pricing = self.prices.snapshot(unix_now());
        let tokens = match &self.tokens {
            Some(gate) => {
                let gate = gate.today()?;
                Some(Tokens {
                    days: self.store.token_days(gate.day)?,
                    gate,
                })
            }
            None => None,
        };
        Ok(MonitorSnapshot {
            transactions: self.store.transaction_status(&self.transaction_scope())?,
            zcash_flow: self.store.flow_status(&self.transaction_scope())?,
            notifications: self.store.notification_status(self.telegram.enabled())?,
            schema_version: SCHEMA_VERSION,
            generated_at: unix_now(),
            deployment: self.info(),
            uptime_seconds: unix_now().saturating_sub(self.monitoring.started_at),
            watchtower_healthy: self.check_watchtower().is_ok(),
            last_pass_age_seconds: self.health.completed_age_seconds(),
            runtime,
            counts: Counts {
                stored,
                awaiting_deposit: self.awaiting_deposit()?,
            },
            inventory: Inventory {
                zec_total_zat: funds.map(|value| value.total.to_string()),
                zec_spendable_zat: funds.map(|value| value.spendable.to_string()),
                zec_reserved_zat: reserved.to_string(),
                zec_available_zat: funds
                    .map(|value| value.spendable.saturating_sub(reserved).to_string()),
                zec_in_flight_zat: in_flight.map(|value| value.to_string()),
                wallet_busy,
            },
            gas_alerts: self.config.gas_alerts.as_ref().map(|alerts| GasAlerts {
                enabled: self.telegram.enabled(),
                interval_seconds: alerts.interval_seconds,
                accounts: alerts
                    .accounts
                    .iter()
                    .map(|account| GasAlertAccount {
                        label: account.label.clone(),
                        address: account.address,
                        low_wei: account.low_wei.to_string(),
                        recovery_wei: account.recovery_wei.to_string(),
                    })
                    .collect(),
            }),
            tokens,
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
                forward_evm_confirmations: self.config.evm_confirmations.get(),
                reverse_fee_reserve_zat: self
                    .config
                    .reverse
                    .as_ref()
                    .map(|value| value.fee_reserve_zat.to_string()),
                max_awaiting_deposit: self.config.max_awaiting_deposit,
            },
            pricing,
            swaps,
            swap_limit: LIMIT,
        })
    }

    /// One swap as the monitor exports it, whether or not it is among the latest.
    pub(crate) fn monitor_swap(&self, id: B256) -> Result<Option<MonitorSwapSnapshot>> {
        let mut swaps = self.store.monitor_swaps(1, Some(id))?;
        if swaps.is_empty() {
            return Ok(None);
        }
        if let Ok(zcash) = self.zcash.try_lock()
            && let Ok(wallet) = zcash.wallet()
        {
            self.wallet_funds(&mut swaps, wallet);
        }
        let failed = self
            .monitoring
            .state
            .lock()
            .unwrap()
            .failed_swap_ids
            .clone();
        self.records(&mut swaps, &failed)?;
        Ok(swaps.pop().map(|swap| MonitorSwapSnapshot {
            schema_version: SCHEMA_VERSION,
            generated_at: unix_now(),
            swap,
        }))
    }

    fn wallet_funds(&self, swaps: &mut [MonitorSwap], wallet: &Wallet) {
        for swap in swaps {
            if let Some(funds) = uuid::Uuid::parse_str(&swap.account)
                .ok()
                .and_then(|account| wallet.funds(AccountUuid::from_uuid(account)).ok())
            {
                swap.zec_total_zat = Some(funds.total.to_string());
                swap.zec_spendable_zat = Some(funds.spendable.to_string());
            }
        }
    }

    /// ZEC in the joint accounts of every unsettled swap: users' deposits the maker has yet to
    /// sweep, and its own deposits users have yet to claim. Unknown if any account can't be read.
    fn in_flight(&self, wallet: &Wallet) -> Result<Option<u64>> {
        let forward = self.store.unsettled_swaps()?;
        let reverse = self.store.pending_reverse_swaps()?;
        Ok(forward
            .iter()
            .map(|swap| swap.zcash_account)
            .chain(reverse.iter().map(|swap| swap.account))
            .map(|account| wallet.funds(account).map(|funds| funds.total))
            .sum::<Result<u64, _>>()
            .ok())
    }

    fn records(&self, swaps: &mut [MonitorSwap], failed: &[B256]) -> Result<()> {
        let scope = self.transaction_scope();
        for swap in swaps {
            swap.last_pass_failed = failed.contains(&swap.id);
            swap.evm_transactions = self.store.evm_transactions(&scope, swap.id)?;
            swap.zcash_flow = self.store.flow_observation(&scope, swap.id)?;
        }
        Ok(())
    }
}
