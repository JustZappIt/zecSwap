//! What each swap earned and cost, recorded as it happens: the prices its quote was made at,
//! the gas of every Ethereum transaction on it, the maker's reverted sends included, and the
//! fees of the maker's Zcash sends, each valued in USD when it happened.
use anyhow::Result;
use serde::Serialize;
use tracing::warn;
use zecswap_chain::evm::{Address, B256};

use super::{Maker, unix_now};
use crate::market::{Asset, QuoteMark};
use crate::store::SwapEconomics;

const SCHEMA_VERSION: u32 = 1;
/// The most swaps one export carries; older ones need a later `since`.
const LIMIT: usize = 1000;
/// What one pass values, the network lookups it may take bounded.
const VALUED_PER_PASS: usize = 10;

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct EconomicsSnapshot {
    schema_version: u32,
    generated_at: u64,
    since: u64,
    /// The maker's own address, telling its transactions from the relayer's and others'.
    maker: Address,
    limit: usize,
    truncated: bool,
    swaps: Vec<SwapEconomics>,
}

impl Maker {
    /// Records a transaction the maker sent for a swap, mined, reverted or of unknown outcome,
    /// so its gas counts against the swap even when it emitted no event.
    pub(super) fn journal(
        &self,
        swap: B256,
        operation: &str,
        sent: Result<B256, zecswap_chain::Error>,
    ) -> Result<B256, zecswap_chain::Error> {
        use zecswap_chain::Error;
        if let Ok(hash) | Err(Error::Reverted(hash) | Error::Unconfirmed(hash)) = &sent
            && let Err(e) = self.store.record_sent(
                &self.transaction_scope(),
                swap,
                operation,
                *hash,
                unix_now(),
            )
        {
            warn!(swap_id = %swap, operation, "could not journal a sent transaction: {e:#}");
        }
        sent
    }

    /// Records the market prices a quote was made at; a failure costs only its valuation.
    pub(super) fn record_quote_mark(&self, quote: &[u8; 32], mark: Option<QuoteMark>) {
        if let Some(mark) = mark
            && let Err(e) = self.store.record_quote_price(quote, unix_now(), &mark)
        {
            warn!("could not record a quote's prices: {e:#}");
        }
    }

    /// Reads a transaction's receipt once: whether Railgun took part, for explorer links, and
    /// what it cost, which `value_costs` prices. Nothing while it is unmined.
    pub(super) async fn read_transaction(&self, scope: &str, hash: B256) -> Result<()> {
        let Some(facts) = self
            .settlement
            .transaction_facts(hash, self.config.token)
            .await?
        else {
            return Ok(());
        };
        self.store.save_evm_info(scope, hash, facts.uses_railgun)?;
        if self.store.block_time(scope, facts.block_hash)?.is_none()
            && let Some(time) = self.settlement.block_time(facts.block_hash).await?
        {
            self.store.save_block_time(scope, facts.block_hash, time)?;
        }
        self.store.save_evm_cost(scope, hash, &facts, None)
    }

    /// Records the fees of the maker's new Zcash sends, then values in USD what is recorded but
    /// not yet valued: gas at its block's time, Zcash fees when sent, and the quotes of swaps
    /// accepted before quotes carried their prices.
    pub(super) async fn cost_pass(&self) -> Result<()> {
        let scope = self.transaction_scope();
        self.cost_zcash_sends(&scope)?;
        for (hash, at) in self.store.unvalued_evm_costs(&scope, VALUED_PER_PASS)? {
            if let Some((usd, source)) = self.prices.usd_at(Asset::Eth, at).await {
                self.store.value_evm_cost(&scope, hash, &usd, source)?;
            }
        }
        for (txid, at) in self.store.unvalued_zcash_costs(&scope, VALUED_PER_PASS)? {
            if let Some((usd, source)) = self.prices.usd_at(Asset::Zec, at).await {
                self.store.value_zcash_cost(&scope, txid, &usd, source)?;
            }
        }
        for (quote, at) in self.store.unpriced_quotes(VALUED_PER_PASS / 3)? {
            let (Some((zec, source)), Some((usdc, _))) = (
                self.prices.usd_at(Asset::Zec, at).await,
                self.prices.usd_at(Asset::Usdc, at).await,
            ) else {
                continue;
            };
            let eth = self.prices.usd_at(Asset::Eth, at).await;
            let mark = QuoteMark {
                source,
                zec_usd: zec,
                usdc_usd: usdc,
                eth_usd: eth.map(|(usd, _)| usd),
            };
            self.store.record_quote_price(&quote, at, &mark)?;
        }
        Ok(())
    }

    /// A send is costed when it was made: now if it hasn't mined, else when its block was, as
    /// the viewing wallet saw it, or failing that when its swap settled.
    fn cost_zcash_sends(&self, scope: &str) -> Result<()> {
        let sends = self.store.uncosted_zcash_sends(scope, VALUED_PER_PASS)?;
        if sends.is_empty() {
            return Ok(());
        }
        // Never queue behind a sync, proof or broadcast: the next pass costs what this can't.
        let Ok(zcash) = self.zcash.try_lock() else {
            return Ok(());
        };
        let Ok(wallet) = zcash.wallet() else {
            return Ok(());
        };
        for send in sends {
            let txid = send.txid.to_string();
            let observed = self
                .store
                .flow_observation(scope, send.swap)?
                .and_then(|flow| flow.transactions.into_iter().find(|tx| tx.txid == txid))
                .and_then(|tx| tx.block_time);
            let at = match observed {
                Some(at) => at,
                None if !wallet.is_mined(send.txid)? => unix_now(),
                None => send.settled_at.unwrap_or_else(unix_now),
            };
            self.store
                .save_zcash_cost(scope, send.txid, wallet.fee(send.txid)?, at)?;
        }
        Ok(())
    }

    /// Swaps accepted at or after `since` (30 days ago if none), newest first, with what each
    /// earned and cost.
    pub(crate) fn economics(&self, since: Option<u64>) -> Result<EconomicsSnapshot> {
        let now = unix_now();
        let since = since.unwrap_or(now.saturating_sub(30 * 86_400));
        let (swaps, truncated) = self
            .store
            .economics(&self.transaction_scope(), since, LIMIT)?;
        Ok(EconomicsSnapshot {
            schema_version: SCHEMA_VERSION,
            generated_at: now,
            since,
            maker: self.account,
            limit: LIMIT,
            truncated,
            swaps,
        })
    }
}
