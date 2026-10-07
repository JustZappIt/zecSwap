use std::time::Duration;

use anyhow::Result;
use futures_util::{StreamExt, stream};
use zecswap_chain::evm::{B256, SwapEvent};

use super::{Maker, unix_now};
use crate::store::Notification;

pub(super) fn transaction_links(chain_id: u64, hash: B256, uses_railgun: Option<bool>) -> String {
    let (explorer, railgun) = match chain_id {
        1 => ("https://etherscan.io", Some("ethereum")),
        11155111 => ("https://sepolia.etherscan.io", Some("sepolia")),
        8453 => ("https://basescan.org", None),
        84532 => ("https://sepolia.basescan.org", None),
        _ => return format!("Ethereum transaction: {hash}"),
    };
    let mut links = format!("{explorer}/tx/{hash}");
    if let Some(chain) = railgun.filter(|_| uses_railgun == Some(true)) {
        links.push_str(&format!("\nhttps://railscan.io/{chain}/tx/{hash}"));
    }
    links
}

impl Maker {
    fn log_observed_event(&self, scope: &str, event: &SwapEvent) {
        if self.store.swap(&event.id).ok().flatten().is_none()
            && self.store.reverse_swap(event.id).ok().flatten().is_none()
        {
            return;
        }
        // Overlapping index windows are re-read on every pass. Emit once per
        // observed event/block identity, including a changed identity after a reorg.
        let known = self
            .store
            .evm_transactions(scope, event.id)
            .unwrap_or_default();
        if !known.iter().any(|tx| {
            tx.transaction_hash == event.transaction_hash
                && tx.block_hash == event.block_hash
                && tx.log_index == event.log_index
        }) {
            tracing::info!(swap_id = %event.id, transaction_hash = %event.transaction_hash,
                operation = event.kind.as_str(), chain = "evm", outcome = "observed",
                block_number = event.block_number, "settlement event observed at configured confirmation depth");
        }
    }

    pub(super) fn transaction_scope(&self) -> String {
        format!(
            "{}:{}:{}:{}",
            self.alert_network(),
            self.chain_id,
            self.config.contract,
            self.account
        )
    }

    pub(super) fn cached_transaction_links(&self, id: B256) -> String {
        let transactions = self
            .store
            .evm_transactions(&self.transaction_scope(), id)
            .unwrap_or_default();
        if transactions.is_empty() {
            return String::new();
        }
        // Keep messages under Telegram's limit; the dashboard holds the complete history.
        let mut links = String::from("\n\nConfirmed Ethereum transactions (latest 4):");
        for tx in transactions.iter().rev().take(4).rev() {
            links.push_str(&format!(
                "\n{} · block {}\n{}",
                tx.kind.replace('_', " "),
                tx.block_number,
                transaction_links(self.chain_id, tx.transaction_hash, tx.uses_railgun)
            ));
        }
        links
    }

    async fn classify_transaction(&self, scope: &str, event: &SwapEvent) {
        if matches!(self.store.evm_info(scope, event.transaction_hash), Ok(None))
            && let Ok(uses_railgun) = self.settlement.uses_railgun(event.transaction_hash).await
        {
            let _ = self
                .store
                .save_evm_info(scope, event.transaction_hash, uses_railgun);
        }
    }

    fn transaction_alert(&self, event: &SwapEvent) -> Result<Option<Notification>> {
        if !event.kind.notify() {
            return Ok(None);
        }
        let detail = format!(
            "Ethereum transaction confirmed: {}.\nBlock: {}\n{}",
            event.kind.label(),
            event.block_number,
            transaction_links(
                self.chain_id,
                event.transaction_hash,
                self.store
                    .evm_info(&self.transaction_scope(), event.transaction_hash)?
            )
        );
        let key = format!(
            "{}:evm:{}:{}",
            self.transaction_scope(),
            event.transaction_hash,
            event.log_index
        );
        let notification = if let Some(swap) = self.store.swap(&event.id)? {
            self.forward_alert(&swap, "transaction", &detail)
        } else if let Some(swap) = self.store.reverse_swap(event.id)? {
            self.reverse_alert(&swap, "transaction", &detail)
        } else {
            None
        };
        Ok(notification.map(|notification| Notification {
            key,
            ..notification
        }))
    }

    async fn index_window(&self, scope: &str, from: u64, to: u64, advance: bool) -> Result<()> {
        let cursor = self
            .store
            .transaction_cursor(scope)?
            .expect("initialized cursor");
        let mut events = Vec::new();
        for event in self.settlement.swap_events(from, to).await? {
            let alert = if event.block_number >= cursor.notify_from_block {
                self.log_observed_event(scope, &event);
                self.classify_transaction(scope, &event).await;
                self.transaction_alert(&event)?
            } else {
                None
            };
            events.push((event, alert));
        }
        self.store
            .record_evm_window(scope, from, to, &events, advance)
    }

    async fn transaction_pass(&self) -> Result<u64> {
        let confirmations = self
            .config
            .reverse
            .as_ref()
            .map_or(2, |c| u64::from(c.evm_confirmations.get()));
        let head = self.settlement.confirmed_height(confirmations).await?;
        let scope = self.transaction_scope();
        if self.store.transaction_cursor(&scope)?.is_none() {
            let mut start = head.saturating_sub(12);
            if let Some(earliest) = self.store.earliest_swap_quote()? {
                let since = earliest
                    .saturating_sub(self.config.timing.quote_ttl)
                    .saturating_sub(300);
                let (mut low, mut high) = (0, head);
                while low < high {
                    let middle = low + (high - low) / 2;
                    if self.settlement.block_timestamp(middle).await? < since {
                        low = middle + 1;
                    } else {
                        high = middle;
                    }
                }
                start = low.saturating_sub(1);
            }
            self.store.init_transaction_cursor(&scope, start, head)?;
        }
        // New swaps must not wait for the historical backfill.
        let recent = head.saturating_sub(11);
        for from in (recent..=head).step_by(10) {
            self.index_window(&scope, from, (from + 9).min(head), false)
                .await?;
        }
        let cursor = self
            .store
            .transaction_cursor(&scope)?
            .expect("initialized cursor");
        let end = (cursor.next_block.saturating_add(499)).min(head);
        let windows: Vec<_> = (cursor.next_block..=end)
            .step_by(10)
            .map(|from| (from, (from + 9).min(end)))
            .collect();
        // Only reads run concurrently; cursor and outbox writes stay in block order.
        let mut reads = stream::iter(windows)
            .map(
                |(from, to)| async move { (from, to, self.settlement.swap_events(from, to).await) },
            )
            .buffered(4);
        while let Some((from, to, result)) = reads.next().await {
            let mut events = Vec::new();
            for event in result? {
                let alert = if event.block_number >= cursor.notify_from_block {
                    self.log_observed_event(&scope, &event);
                    self.classify_transaction(&scope, &event).await;
                    self.transaction_alert(&event)?
                } else {
                    None
                };
                events.push((event, alert));
            }
            self.store
                .record_evm_window(&scope, from, to, &events, true)?;
        }
        self.store
            .record_transaction_pass(&scope, head, unix_now(), false)?;
        Ok(head)
    }

    pub(super) async fn run_transaction_observer(&self) {
        loop {
            let failure = match tokio::time::timeout(
                Duration::from_secs(45),
                self.transaction_pass(),
            )
            .await
            {
                Ok(Ok(_)) => None,
                Ok(Err(error)) => Some(super::observer_failure(&error)),
                Err(_) => Some("timeout"),
            };
            if let Some(failure_kind) = failure {
                // RPC errors can contain authenticated URLs; export and log a fixed message.
                tracing::warn!(
                    operation = "evm_transaction_observer",
                    failure_kind,
                    "Ethereum transaction indexing failed; retrying"
                );
                let _ = self.store.record_transaction_pass(
                    &self.transaction_scope(),
                    0,
                    unix_now(),
                    true,
                );
            }
            let failure = match tokio::time::timeout(
                Duration::from_secs(20),
                self.enrich_evm_history(),
            )
            .await
            {
                Ok(Ok(())) => None,
                Ok(Err(error)) => Some(super::observer_failure(&error)),
                Err(_) => Some("timeout"),
            };
            if let Some(failure_kind) = failure {
                tracing::warn!(
                    operation = "evm_metadata_observer",
                    failure_kind,
                    "Ethereum explorer metadata lookup delayed; retrying"
                );
            }
            tokio::time::sleep(Duration::from_secs(15)).await;
        }
    }

    async fn enrich_evm_history(&self) -> Result<()> {
        let scope = self.transaction_scope();
        let mut remaining = 25;
        for swap in self.store.monitor_swaps(500)? {
            for transaction in self.store.evm_transactions(&scope, swap.id)? {
                if transaction.uses_railgun.is_none() {
                    let uses_railgun = self
                        .settlement
                        .uses_railgun(transaction.transaction_hash)
                        .await?;
                    self.store
                        .save_evm_info(&scope, transaction.transaction_hash, uses_railgun)?;
                    remaining -= 1;
                    if remaining == 0 {
                        return Ok(());
                    }
                }
            }
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn explorer_links_follow_the_chain_and_only_use_the_transaction_hash() {
        let hash = B256::repeat_byte(3);
        let sepolia = transaction_links(11155111, hash, Some(true));
        assert!(sepolia.contains(&format!("https://sepolia.etherscan.io/tx/{hash}")));
        assert!(sepolia.contains(&format!("https://railscan.io/sepolia/tx/{hash}")));
        let mainnet = transaction_links(1, hash, Some(true));
        assert!(mainnet.contains("https://etherscan.io/tx/"));
        assert!(mainnet.contains("https://railscan.io/ethereum/tx/"));
        assert!(!mainnet.contains("sepolia"));
        assert!(!transaction_links(999, hash, None).contains("https://"));
        assert!(!transaction_links(1, hash, Some(false)).contains("railscan"));
    }
}
