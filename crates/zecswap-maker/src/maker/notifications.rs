use anyhow::{Result, ensure};
use zecswap_chain::evm::{B256, OnChainSwap, Stage, SwapEventKind};

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

/// How long an error has to last before it is news: a failing RPC or lightwalletd usually
/// recovers within a few passes.
const LASTING_ERROR: u64 = 5 * 60;

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

fn time(seconds: u64) -> String {
    i64::try_from(seconds)
        .ok()
        .and_then(|s| chrono::DateTime::from_timestamp(s, 0))
        .map(|t| t.format("%-d %b %H:%M UTC").to_string())
        .unwrap_or_else(|| seconds.to_string())
}

fn duration(seconds: u64) -> String {
    match seconds {
        0..60 => format!("{seconds}s"),
        60..3600 => format!("{}m", seconds / 60),
        _ => format!("{}h {:02}m", seconds / 3600, seconds % 3600 / 60),
    }
}

fn explorer(chain_id: u64, hash: B256) -> Option<String> {
    let explorer = match chain_id {
        1 => "https://etherscan.io",
        11155111 => "https://sepolia.etherscan.io",
        8453 => "https://basescan.org",
        84532 => "https://sepolia.basescan.org",
        _ => return None,
    };
    Some(format!("{explorer}/tx/{hash}"))
}

/// How a swap ended: the word its last alert gives and, unless it simply completed, why.
pub(super) struct Ending {
    word: &'static str,
    why: Option<&'static str>,
}

/// A forward swap's ending, from its confirmed escrow (`None` if it never opened) and whether
/// any ZEC reached the deposit address.
pub(super) fn forward_ending(chain: Option<&OnChainSwap>, deposited: bool) -> Ending {
    let (word, why) = match chain {
        Some(chain) if chain.stage == Stage::Claimed => (
            "completed",
            (chain.payout_note.is_some() && !chain.paid_out)
                .then_some("The user's USDC is claimed but not yet paid into Railgun."),
        ),
        Some(_) if deposited => (
            "refunded",
            Some("The maker has its USDC back; the user can take their ZEC back."),
        ),
        Some(_) => (
            "cancelled",
            Some("No ZEC arrived, so the maker took its USDC back."),
        ),
        None => ("expired", Some("Its escrow never opened.")),
    };
    Ending { word, why }
}

/// A reverse swap's ending, from its confirmed escrow (`None` if the user never funded it) and
/// whether the maker's ZEC deposit landed.
pub(super) fn reverse_ending(chain: Option<&OnChainSwap>, deposited: bool) -> Ending {
    let (word, why) = match chain {
        None => ("expired", Some("The user never funded it.")),
        Some(chain) if chain.stage == Stage::Claimed => ("completed", None),
        Some(chain) if chain.stage == Stage::Refunded => (
            "refunded",
            Some(match (chain.paid_out, deposited) {
                (false, _) => "The user's USDC is still to be paid back to them.",
                (true, true) => "The user has their USDC back and the maker its ZEC.",
                (true, false) => "The maker sent no ZEC, so the user has their USDC back.",
            }),
        ),
        Some(_) => (
            "expired",
            Some("The maker sent no ZEC in time; the user can take their USDC back."),
        ),
    };
    Ending { word, why }
}

/// What an alert about one swap reports.
pub(super) enum Alert {
    Started,
    Ended(Ending),
    /// The watchtower keeps failing to advance it.
    Stuck,
    /// Railgun sent its payout back, and this transaction shielded it again.
    Returned(B256),
}

impl Maker {
    pub(super) fn service_alert(&self, detail: &str) -> Option<Notification> {
        let network = self.alert_network();
        self.telegram.enabled().then(|| Notification {
            key: format!("{}:error", self.alert_scope("service", B256::ZERO)),
            text: format!(
                "ZecSwap {network} · maker error\n{detail}\n{}\n{DASHBOARD}?network={network}",
                time(unix_now())
            ),
            created_at: unix_now(),
        })
    }

    pub(super) fn queue_alert(&self, event: Option<Notification>) {
        if let Some(event) = event
            && self.store.enqueue_notification(&event).is_err()
        {
            tracing::warn!("could not queue bridge notification");
        }
    }
    pub(super) fn alert_network(&self) -> &'static str {
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

    pub(super) fn forward_alert(&self, swap: &Swap, alert: Alert) -> Option<Notification> {
        let amounts = (swap.quote.amount, swap.quote.deposit_zat);
        self.swap_alert("forward", swap.id, amounts, Some(swap.opened_at), alert)
    }

    pub(super) fn reverse_alert(&self, swap: &ReverseSwap, alert: Alert) -> Option<Notification> {
        let amounts = (swap.quote.terms.amount, swap.quote.terms.deposit_zat);
        self.swap_alert("reverse", swap.id, amounts, None, alert)
    }

    /// A few lines: which way the swap goes and what happened to it, its amounts, why if it
    /// didn't simply start or complete, when, a short id the dashboard's search finds, and one
    /// link: the transaction that settled it, or the dashboard.
    fn swap_alert(
        &self,
        direction: &str,
        id: B256,
        (amount, zat): (u128, u64),
        opened_at: Option<u64>,
        alert: Alert,
    ) -> Option<Notification> {
        if !self.telegram.enabled() {
            return None;
        }
        let now = unix_now();
        let decimals = self
            .config
            .pricing
            .market
            .as_ref()
            .map_or(6, |p| p.token_decimals);
        let usdc = format!("{} USDC", units(amount, decimals));
        let zec = format!("{} ZEC", units(zat.into(), 8));
        let (trade, amounts) = match direction {
            "forward" => ("ZEC → USDC", format!("{zec} for {usdc}")),
            _ => ("USDC → ZEC", format!("{usdc} for {zec}")),
        };
        let (phase, what, why, transaction) = match alert {
            Alert::Started => ("accepted", "started".to_owned(), None, None),
            Alert::Ended(ending) => {
                let took = self
                    .store
                    .accepted_at(id)
                    .ok()
                    .flatten()
                    .or(opened_at)
                    .map(|at| format!(" after {}", duration(now.saturating_sub(at))))
                    .unwrap_or_default();
                let what = format!("{}{took}", ending.word);
                ("finished", what, ending.why, self.settling_transaction(id))
            }
            Alert::Stuck => (
                "error",
                "stuck".to_owned(),
                Some("The maker keeps failing to move it on; see the maker's logs."),
                None,
            ),
            Alert::Returned(hash) => (
                "returned",
                "payout returned".to_owned(),
                Some("Railgun sent the payout back, and it has been shielded again."),
                Some(hash),
            ),
        };
        let network = self.alert_network();
        let link = transaction
            .and_then(|hash| explorer(self.chain_id, hash))
            .unwrap_or_else(|| format!("{DASHBOARD}?network={network}"));
        let why = why.map(|why| format!("{why}\n")).unwrap_or_default();
        Some(Notification {
            key: format!("{}:{phase}", self.alert_scope(direction, id)),
            text: format!(
                "ZecSwap {network} · {trade} {what}\n{amounts}\n{why}{} · swap {}\n{link}",
                time(now),
                &id.to_string()[..10]
            ),
            created_at: now,
        })
    }

    /// The last claim, refund or payout on a swap.
    fn settling_transaction(&self, id: B256) -> Option<B256> {
        let settling = [
            SwapEventKind::Claimed,
            SwapEventKind::Refunded,
            SwapEventKind::PaidOut,
        ]
        .map(SwapEventKind::as_str);
        self.store
            .evm_transactions(&self.transaction_scope(), id)
            .ok()?
            .into_iter()
            .rev()
            .find(|tx| settling.contains(&tx.kind.as_str()))
            .map(|tx| tx.transaction_hash)
    }

    pub(super) fn record_alert_failure(
        &self,
        direction: &str,
        id: B256,
        event: Option<Notification>,
    ) {
        if !self.telegram.enabled() {
            return;
        }
        let scope = self.alert_scope(direction, id);
        let event = match event {
            Some(event) => {
                let since = *self
                    .failing
                    .lock()
                    .unwrap()
                    .entry(scope.clone())
                    .or_insert(event.created_at);
                if event.created_at < since.saturating_add(LASTING_ERROR) {
                    return;
                }
                Some(event)
            }
            None => {
                self.failing.lock().unwrap().remove(&scope);
                None
            }
        };
        if self.store.notification_failure(&scope, event).is_err() {
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
                "ZecSwap {network} · alerts test\nNot a swap. Alerts come when a swap starts and ends, when Railgun returns a payout, when gas runs low, and when an error lasts five minutes.\n{DASHBOARD}?network={network}"
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
    use zecswap_chain::evm::Address;
    use zecswap_core::SecretShare;

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
    fn times_read_to_the_minute() {
        assert_eq!(time(1_791_720_221), "11 Oct 12:03 UTC");
        assert_eq!(duration(59), "59s");
        assert_eq!(duration(14 * 60 + 59), "14m");
        assert_eq!(duration(3600 + 5 * 60), "1h 05m");
    }

    #[test]
    fn explorer_links_follow_the_chain() {
        let hash = B256::repeat_byte(3);
        assert_eq!(
            explorer(11155111, hash).unwrap(),
            format!("https://sepolia.etherscan.io/tx/{hash}")
        );
        assert_eq!(
            explorer(1, hash).unwrap(),
            format!("https://etherscan.io/tx/{hash}")
        );
        assert!(explorer(31337, hash).is_none());
    }

    fn chain(stage: Stage, payout_note: Option<B256>, paid_out: bool) -> OnChainSwap {
        let share = || SecretShare::random(rand::rand_core::UnwrapErr(rand::rngs::SysRng)).public();
        OnChainSwap {
            stage,
            maker: Address::ZERO,
            user: Address::ZERO,
            token: Address::ZERO,
            amount: 1,
            t0: 100,
            t1: 200,
            claim_lock_until: 0,
            refund_lock_until: 0,
            maker_share: share(),
            user_share: share(),
            secret: [0; 32],
            payout_note,
            paid_out,
        }
    }

    #[test]
    fn endings_tell_completions_from_refunds_and_walk_aways() {
        let note = Some(B256::repeat_byte(1));
        let forward = |stage, payout_note, paid_out, deposited| {
            let ending = forward_ending(Some(&chain(stage, payout_note, paid_out)), deposited);
            (ending.word, ending.why.unwrap_or_default())
        };
        assert_eq!(forward(Stage::Claimed, note, true, true), ("completed", ""));
        // Only a Railgun payout can be pending: an account's is the user's to withdraw.
        assert!(
            forward(Stage::Claimed, note, false, true)
                .1
                .contains("not yet paid")
        );
        assert_eq!(
            forward(Stage::Claimed, None, false, true),
            ("completed", "")
        );
        // A forward refund credits the maker's balance and never pays out.
        assert_eq!(forward(Stage::Refunded, note, false, true).0, "refunded");
        assert_eq!(forward(Stage::Refunded, note, false, false).0, "cancelled");
        assert_eq!(forward_ending(None, false).word, "expired");

        let reverse = |stage, paid_out, deposited| {
            let ending = reverse_ending(Some(&chain(stage, note, paid_out)), deposited);
            (ending.word, ending.why.unwrap_or_default())
        };
        assert_eq!(reverse(Stage::Claimed, false, true), ("completed", ""));
        assert!(
            reverse(Stage::Refunded, false, true)
                .1
                .contains("still to be paid")
        );
        assert!(
            reverse(Stage::Refunded, true, true)
                .1
                .contains("the maker its ZEC")
        );
        assert!(
            reverse(Stage::Refunded, true, false)
                .1
                .contains("sent no ZEC,")
        );
        let late = reverse(Stage::Open, false, false);
        assert_eq!(late.0, "expired");
        assert!(late.1.contains("sent no ZEC in time"));
        assert_eq!(
            reverse_ending(None, false).why,
            Some("The user never funded it.")
        );
    }
}
