use anyhow::{Result, ensure};
use zecswap_chain::evm::{B256, OnChainSwap, Stage};

use super::{Maker, unix_now};
use crate::store::{Notification, ReverseSwap, Store, Swap};

async fn deliver_notification(store: &Store, telegram: &crate::telegram::Telegram) -> Result<()> {
    if let Some(delivery) = store.claim_notification(unix_now())? {
        match telegram.send(&delivery.text).await {
            Ok(message_id) => store.acknowledge_notification(&delivery, unix_now(), message_id)?,
            Err(error) => {
                let delay = error
                    .retry_after
                    .unwrap_or(
                        5u64.saturating_mul(1u64 << delivery.attempts.min(6))
                            .min(300),
                    )
                    .max(3);
                store.retry_notification(&delivery, unix_now(), delay, &error.message)?;
                tracing::warn!("{}; bridge alert queued for retry", error.message);
            }
        }
    }
    Ok(())
}

const DASHBOARD: &str = "https://zapp-dashboard-seven.vercel.app/bridge";

fn units(amount: u128, decimals: u8) -> String {
    let base = 10u128.pow(u32::from(decimals));
    let fraction = format!("{:0width$}", amount % base, width = usize::from(decimals));
    let fraction = fraction.trim_end_matches('0');
    if fraction.is_empty() {
        (amount / base).to_string()
    } else {
        format!("{}.{fraction}", amount / base)
    }
}

fn timestamp(seconds: u64) -> String {
    i64::try_from(seconds)
        .ok()
        .and_then(|s| chrono::DateTime::from_timestamp(s, 0))
        .map(|t| t.format("%Y-%m-%d %H:%M:%S UTC").to_string())
        .unwrap_or_else(|| seconds.to_string())
}

pub(super) fn outcome(chain: Option<&OnChainSwap>, reverse: bool) -> &'static str {
    match chain {
        Some(chain) if chain.stage == Stage::Claimed => match (reverse, chain.paid_out) {
            (false, true) => "Bridge settled: USDC paid out; maker ZEC sweep confirmed.",
            (false, false) => {
                "Bridge settled: USDC claimed; payout withdrawal pending; maker ZEC sweep confirmed."
            }
            (true, true) => {
                "Bridge settled: USDC paid to maker; ZEC available for the user to recover."
            }
            (true, false) => {
                "Bridge claimed: maker payout pending; ZEC available for the user to recover."
            }
        },
        Some(chain) if chain.stage == Stage::Refunded => {
            if chain.paid_out {
                if reverse {
                    "Bridge refunded: USDC returned; maker ZEC recovery complete."
                } else {
                    "Bridge refunded: escrow closed; user can recover their ZEC."
                }
            } else {
                "Bridge refund recorded: escrow payout withdrawal pending; ZEC recovery remains subject to wallet state."
            }
        }
        Some(_) => "Bridge expired without maker funding. Escrow refund may still be pending.",
        None => "Bridge expired: escrow was never opened or funded before its deadline.",
    }
}

impl Maker {
    pub(super) fn service_alert(&self, detail: &str) -> Option<Notification> {
        self.alert("service", B256::ZERO, "error", detail)
    }

    pub(super) fn queue_alert(&self, event: Option<Notification>) {
        if let Some(event) = event
            && self.store.enqueue_notification(&event).is_err()
        {
            tracing::warn!("could not queue bridge notification");
        }
    }
    fn alert_network(&self) -> &'static str {
        match self.config.network {
            crate::Chain::Mainnet => "mainnet",
            crate::Chain::Testnet => "testnet",
        }
    }

    fn alert_scope(&self, direction: &str, id: B256) -> String {
        format!(
            "{}:{}:{}:{}:{direction}:{id}",
            self.alert_network(),
            self.chain_id,
            self.config.contract,
            self.account
        )
    }

    fn alert(&self, direction: &str, id: B256, phase: &str, details: &str) -> Option<Notification> {
        let reference = if id.is_zero() {
            "Service notification".into()
        } else {
            format!("Swap: {id}")
        };
        self.telegram.enabled().then(|| Notification {
            key: format!("{}:{phase}", self.alert_scope(direction, id)),
            text: format!("Zapp bridge · {}\n{details}\n\n{reference}\nEVM chain: {}\nContract: {}\nMaker: {}\nEvent: {}\n{DASHBOARD}?network={}",
                self.alert_network().to_uppercase(), self.chain_id, self.config.contract, self.account, timestamp(unix_now()), self.alert_network()),
            created_at: unix_now(),
        })
    }

    fn alert_amounts(&self, amount: u128, zat: u64) -> String {
        let decimals = self
            .config
            .pricing
            .market
            .as_ref()
            .map_or(6, |p| p.token_decimals);
        format!(
            "Locked quote: {} USDC ↔ {} ZEC",
            units(amount, decimals),
            units(zat.into(), 8)
        )
    }

    pub(super) fn forward_alert(
        &self,
        swap: &Swap,
        phase: &str,
        detail: &str,
    ) -> Option<Notification> {
        self.alert(
            "forward",
            swap.id,
            phase,
            &format!(
                "{detail}\nDirection: ZEC → USDC\n{}\nClaim fallback after: {}\nRefund after: {}{}",
                self.alert_amounts(swap.quote.amount, swap.quote.deposit_zat),
                timestamp(
                    swap.t1
                        .saturating_sub(self.config.timing.t1_after)
                        .saturating_add(self.config.timing.t0_after)
                ),
                timestamp(swap.t1),
                swap.sweep
                    .map(|id| format!("\nMaker ZEC sweep: {id}"))
                    .unwrap_or_default()
            ),
        )
    }

    pub(super) fn reverse_alert(
        &self,
        swap: &ReverseSwap,
        phase: &str,
        detail: &str,
    ) -> Option<Notification> {
        self.alert("reverse", swap.id, phase, &format!("{detail}\nDirection: USDC → ZEC\n{}\nFund by: {}\nReady by: {}\nRefund after: {}{}{}",
            self.alert_amounts(swap.quote.terms.amount, swap.quote.terms.deposit_zat), timestamp(swap.quote.funding_deadline),
            timestamp(swap.quote.ready_deadline), timestamp(swap.quote.refund_after),
            swap.deposit.map(|id| format!("\nMaker ZEC deposit: {id}")).unwrap_or_default(),
            swap.sweep.map(|id| format!("\nMaker ZEC recovery: {id}")).unwrap_or_default()))
    }

    pub(super) fn record_alert_failure(
        &self,
        direction: &str,
        id: B256,
        event: Option<Notification>,
    ) {
        if self.telegram.enabled()
            && self
                .store
                .notification_failure(&self.alert_scope(direction, id), event)
                .is_err()
        {
            tracing::warn!("could not record Telegram failure notification");
        }
    }

    pub(super) async fn run_notifications(&self) {
        if !self.telegram.enabled() {
            std::future::pending::<()>().await;
        }
        loop {
            if deliver_notification(&self.store, &self.telegram)
                .await
                .is_err()
            {
                tracing::warn!("Telegram delivery queue unavailable; retrying");
            }
            // Also stays below Telegram's per-chat flood limit.
            tokio::time::sleep(std::time::Duration::from_secs(3)).await;
        }
    }

    /// Sends a clearly labeled setup message without creating or funding a swap.
    pub async fn telegram_test(config: &crate::Config) -> Result<()> {
        let telegram = crate::telegram::Telegram::from_env()?;
        let store = Store::open(&config.data_dir.join("maker.sqlite"))?;
        ensure!(telegram.enabled(), "Telegram alerts are not configured");
        let network = match config.network {
            crate::Chain::Mainnet => "mainnet",
            crate::Chain::Testnet => "testnet",
        };
        let event = Notification {
            key: format!(
                "{network}:{}:setup:{}",
                config.contract,
                uuid::Uuid::new_v4()
            ),
            text: format!(
                "Zapp bridge · {}\nSETUP TEST — Telegram bridge alerts enabled.\nThis is a connection test, not a bridge transaction.\nNotifications: accepted bridges, settlement, refunds, expiry and processing errors.\nContract: {}\n{DASHBOARD}?network={network}",
                network.to_uppercase(),
                config.contract
            ),
            created_at: unix_now(),
        };
        store.enqueue_notification(&event)?;
        tokio::time::timeout(std::time::Duration::from_secs(45), async {
            while !store.notification_delivered(&event.key)? {
                deliver_notification(&store, &telegram).await?;
                tokio::time::sleep(std::time::Duration::from_secs(3)).await;
            }
            Ok::<_, anyhow::Error>(())
        })
        .await
        .map_err(|_| {
            anyhow::anyhow!("Telegram test remains queued; inspect notification telemetry")
        })??;
        tracing::info!("Telegram accepted the setup test message");
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn exact_amounts_do_not_use_floating_point() {
        assert_eq!(units(123456789, 8), "1.23456789");
        assert_eq!(units(1, 6), "0.000001");
        assert_eq!(
            units(u128::MAX, 6),
            "340282366920938463463374607431768.211455"
        );
    }

    #[test]
    fn outcomes_only_announce_observed_payouts_and_do_not_infer_user_zec_recovery() {
        use zecswap_chain::evm::Address;
        use zecswap_core::SecretShare;
        let mut chain = OnChainSwap {
            stage: Stage::Claimed,
            maker: Address::ZERO,
            user: Address::ZERO,
            token: Address::ZERO,
            amount: 1,
            t0: 100,
            t1: 200,
            claim_lock_until: 0,
            refund_lock_until: 0,
            maker_share: SecretShare::random(rand_core::OsRng).public(),
            user_share: SecretShare::random(rand_core::OsRng).public(),
            secret: [0; 32],
            payout_note: None,
            paid_out: false,
        };
        assert!(outcome(Some(&chain), false).contains("payout withdrawal pending"));
        assert!(outcome(Some(&chain), true).contains("ZEC available for the user to recover"));
        chain.paid_out = true;
        assert!(outcome(Some(&chain), false).contains("USDC paid out"));
        chain.stage = Stage::Refunded;
        assert!(outcome(Some(&chain), true).contains("Bridge refunded"));
        assert!(outcome(Some(&chain), false).contains("user can recover"));
        chain.paid_out = false;
        assert!(outcome(Some(&chain), true).contains("payout withdrawal pending"));
        assert!(outcome(None, true).contains("Bridge expired"));
    }
}
