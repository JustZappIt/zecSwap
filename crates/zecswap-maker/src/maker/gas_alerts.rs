use std::time::Duration;

use zecswap_chain::evm::U256;

use super::{Maker, unix_now};
use crate::config::GasAlertAccount;
use crate::store::Notification;

fn transition(account: &GasAlertAccount, balance: U256) -> Option<bool> {
    if balance < U256::from(account.low_wei) {
        Some(true)
    } else if balance >= U256::from(account.recovery_wei) {
        Some(false)
    } else {
        None
    }
}

fn eth(wei: U256) -> String {
    let base = U256::from(1_000_000_000_000_000_000u64);
    let fraction = format!("{:018}", wei % base);
    let fraction = fraction.trim_end_matches('0');
    if fraction.is_empty() {
        (wei / base).to_string()
    } else {
        format!("{}.{fraction}", wei / base)
    }
}

impl Maker {
    pub(super) async fn run_gas_alerts(&self) {
        let Some(config) = self
            .config
            .gas_alerts
            .as_ref()
            .filter(|_| self.telegram.enabled())
        else {
            std::future::pending::<()>().await;
            return;
        };
        let mut ticks = tokio::time::interval(Duration::from_secs(config.interval_seconds));
        ticks.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
        loop {
            ticks.tick().await;
            for account in &config.accounts {
                // Read-only RPCs, independent of the watchtower, wallet scanner,
                // notification delivery and request-driven market pricing.
                let result = tokio::time::timeout(
                    Duration::from_secs(5),
                    self.settlement.eth_balance(account.address),
                )
                .await;
                let Ok(Ok(balance)) = result else {
                    tracing::warn!(address = %account.address, operation = "gas_balance_check", "Gas balance unavailable; preserving previous alert state");
                    continue;
                };
                let Some(low) = transition(account, balance) else {
                    continue;
                };
                let scope = format!(
                    "{}:{}:gas:{}",
                    self.alert_network(),
                    self.chain_id,
                    account.address
                );
                let state = if low { "LOW GAS" } else { "GAS RECOVERED" };
                let text = format!(
                    "Zapp bridge · {}\n{state}: {}\nAccount: {}\nBalance: {} ETH\nLow threshold: {} ETH\nRecovery threshold: {} ETH\nEVM chain: {}\n{}\nhttps://zapp-dashboard-seven.vercel.app/bridge?network={}",
                    self.alert_network().to_uppercase(),
                    account.label,
                    account.address,
                    eth(balance),
                    eth(U256::from(account.low_wei)),
                    eth(U256::from(account.recovery_wei)),
                    self.chain_id,
                    if low {
                        "Top up this address with native ETH on the listed chain. Gas is separate from token/private USD balances."
                    } else {
                        "The configured gas reserve has been restored. This does not guarantee every transaction's fee."
                    },
                    self.alert_network()
                );
                match self.store.gas_notification(
                    &scope,
                    low,
                    Notification {
                        key: String::new(),
                        text,
                        created_at: unix_now(),
                    },
                ) {
                    Ok(true) => {
                        tracing::info!(address = %account.address, balance_wei = %balance, low, operation = "gas_balance_check", "Queued gas balance notification")
                    }
                    Ok(false) => {}
                    Err(_) => {
                        tracing::warn!(address = %account.address, operation = "gas_balance_check", "Could not queue gas balance notification; will retry")
                    }
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::GasAlerts;
    use zecswap_chain::evm::Address;

    fn account() -> GasAlertAccount {
        GasAlertAccount {
            label: "Android test gas".into(),
            address: Address::repeat_byte(1),
            low_wei: 10,
            recovery_wei: 15,
        }
    }

    #[test]
    fn uses_hysteresis_and_exact_wei_without_rounding() {
        let account = account();
        assert_eq!(transition(&account, U256::from(9)), Some(true));
        assert_eq!(transition(&account, U256::from(10)), None);
        assert_eq!(transition(&account, U256::from(14)), None);
        assert_eq!(transition(&account, U256::from(15)), Some(false));
        assert_eq!(transition(&account, U256::MAX), Some(false));
        assert_eq!(eth(U256::from(1)), "0.000000000000000001");
        assert_eq!(
            eth(U256::from(7_925_991_455_494_601u64)),
            "0.007925991455494601"
        );
        assert_eq!(eth(U256::from(1_000_000_000_000_000_000u64)), "1");
    }

    #[test]
    fn bounds_monitor_work_and_validates_accounts() {
        let valid = GasAlerts {
            interval_seconds: 300,
            accounts: vec![account()],
        };
        assert!(valid.check().is_ok());
        let mut bad = valid.clone();
        bad.interval_seconds = 1;
        assert!(bad.check().is_err());
        bad = valid.clone();
        bad.accounts.clear();
        assert!(bad.check().is_err());
        bad = valid.clone();
        bad.accounts.push(account());
        assert!(bad.check().is_err());
        bad = valid.clone();
        bad.accounts[0].address = Address::ZERO;
        assert!(bad.check().is_err());
        bad = valid.clone();
        bad.accounts[0].recovery_wei = 10;
        assert!(bad.check().is_err());
    }
}
