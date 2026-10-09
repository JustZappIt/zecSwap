//! The relayer's fees, priced by gas: ETH in the relayer's token from live prices, with its
//! margin on top. Each quote is honored for a while, so what a wallet was quoted is what it
//! pays though prices moved since.

use std::collections::VecDeque;
use std::sync::Mutex;

use rust_decimal::{Decimal, prelude::ToPrimitive};
use zecswap_prices::{Asset, Feed, Keys, Provider};

/// How long a rate quoted for sends is honored: time enough to prove a send with it and post.
pub(crate) const SEND_FEE_VALIDITY: u64 = 600;
/// How long a quoted swap fee is honored: the fee a wallet shows when a swap starts is the fee
/// its claim or funding pays, while the swap runs.
pub(crate) const SWAP_FEE_VALIDITY: u64 = 3600;
/// How long a gas price read is current.
const GAS_PRICE_MAX_AGE: u64 = 15;

/// What a swap fee pays the relayer back for.
#[derive(Clone, Copy, Debug)]
pub(crate) enum SwapFee {
    /// Kept from a payout: the claim lock, the claim and the payout, or on a refund its lock,
    /// the refund and its payout.
    Payout,
    /// Paid by a reverse swap's funding: the funding and its ready.
    Funding,
}

pub(crate) struct GasPricing {
    feed: Feed,
    pub(crate) margin_bps: u32,
    pub(crate) rates: Quoted,
    pub(crate) payouts: Quoted,
    pub(crate) fundings: Quoted,
    gas_price: Mutex<Option<(u64, u128)>>,
}

/// Values quoted, oldest first, each with when it last was.
#[derive(Default)]
pub(crate) struct Quoted(pub(crate) Mutex<VecDeque<(u64, u128)>>);

impl Quoted {
    fn record(&self, now: u64, validity: u64, value: u128) {
        let mut quoted = self.0.lock().unwrap();
        quoted.retain(|(at, _)| now.saturating_sub(*at) < validity);
        match quoted.iter_mut().find(|(_, quote)| *quote == value) {
            Some(entry) => entry.0 = now,
            None => quoted.push_back((now, value)),
        }
    }

    /// The lowest quoted within `validity`, `current` included.
    fn lowest(&self, now: u64, validity: u64, current: Option<u128>) -> Option<u128> {
        self.0
            .lock()
            .unwrap()
            .iter()
            .filter(|(at, _)| now.saturating_sub(*at) < validity)
            .map(|(_, value)| *value)
            .chain(current)
            .min()
    }
}

impl GasPricing {
    pub(crate) fn new(
        providers: &[Provider],
        keys: &Keys,
        margin_bps: u32,
    ) -> anyhow::Result<Self> {
        Ok(Self {
            feed: Feed::new(providers, keys, &[Asset::Eth, Asset::Usdc], &[], 60, 300)?,
            margin_bps,
            rates: Quoted::default(),
            payouts: Quoted::default(),
            fundings: Quoted::default(),
            gas_price: Mutex::default(),
        })
    }

    /// The rate now, unquoted: token base units per 10^18 wei of gas (per ETH), the margin
    /// included.
    pub(crate) fn rate(&self, now: u64) -> Option<u128> {
        let prices = self.feed.fresh(now)?;
        let usdc_per_eth = prices
            .usd(Asset::Eth)?
            .checked_div(prices.usd(Asset::Usdc)?)?;
        let margin = Decimal::from(10_000 + self.margin_bps) / Decimal::from(10_000);
        (usdc_per_eth * Decimal::from(1_000_000) * margin)
            .trunc()
            .to_u128()
            .filter(|rate| *rate > 0)
    }

    /// The rate now, quoted: a send proved with it is held to it for `SEND_FEE_VALIDITY`.
    pub(crate) async fn quote_rate(&self, now: u64) -> Option<u128> {
        self.feed.refresh().await;
        let rate = self.rate(now)?;
        self.rates.record(now, SEND_FEE_VALIDITY, rate);
        Some(rate)
    }

    /// The rate a send posted now is held to: the lowest quoted within `SEND_FEE_VALIDITY`,
    /// now's included, so a proof made from any of those terms is taken. None if gas can't be
    /// priced now and nothing was quoted since.
    pub(crate) async fn honored_rate(&self, now: u64) -> Option<u128> {
        let current = self.quote_rate(now).await;
        self.rates.lowest(now, SEND_FEE_VALIDITY, current)
    }

    /// A swap fee now, quoted: `gas` at `gas_price` and the rate now, never under `floor`, and
    /// whether gas was priced; just `floor` where it can't be.
    pub(crate) async fn quote_fee(
        &self,
        fee: SwapFee,
        now: u64,
        floor: u128,
        gas: u64,
        gas_price: Option<u128>,
    ) -> (u128, bool) {
        let (Some(gas_price), true) = (gas_price, gas > 0) else {
            return (floor, false);
        };
        let Some(rate) = self.quote_rate(now).await else {
            return (floor, false);
        };
        let quote = gas_fee(gas, gas_price, rate).max(floor);
        self.quoted(fee).record(now, SWAP_FEE_VALIDITY, quote);
        (quote, true)
    }

    /// The least a swap fee signed or proved now must pay: the lowest quoted within
    /// `SWAP_FEE_VALIDITY`, now's included. With nothing priced or quoted it is `floor`: a claim
    /// is never held up for want of a price.
    pub(crate) async fn honored_fee(
        &self,
        fee: SwapFee,
        now: u64,
        floor: u128,
        gas: u64,
        gas_price: Option<u128>,
    ) -> u128 {
        let (current, priced) = self.quote_fee(fee, now, floor, gas, gas_price).await;
        self.quoted(fee)
            .lowest(now, SWAP_FEE_VALIDITY, priced.then_some(current))
            .map_or(floor, |least| least.max(floor))
    }

    fn quoted(&self, fee: SwapFee) -> &Quoted {
        match fee {
            SwapFee::Payout => &self.payouts,
            SwapFee::Funding => &self.fundings,
        }
    }

    /// The gas price read within `GAS_PRICE_MAX_AGE`, if any.
    pub(crate) fn cached_gas_price(&self, now: u64) -> Option<u128> {
        self.gas_price
            .lock()
            .unwrap()
            .filter(|(at, _)| now.saturating_sub(*at) < GAS_PRICE_MAX_AGE)
            .map(|(_, price)| price)
    }

    pub(crate) fn cache_gas_price(&self, now: u64, price: u128) {
        *self.gas_price.lock().unwrap() = Some((now, price));
    }

    /// Points the feed at a stand-in of the providers.
    #[cfg(test)]
    pub(crate) fn served_by(mut self, base: &str) -> Self {
        self.feed = self.feed.served_by(base);
        self
    }
}

/// Token base units `gas` costs at `gas_price` wei and `rate` token base units per 10^18 wei.
pub(crate) fn gas_fee(gas: u64, gas_price: u128, rate: u128) -> u128 {
    u128::from(gas)
        .saturating_mul(gas_price)
        .saturating_mul(rate)
        / 10u128.pow(18)
}

#[cfg(test)]
mod tests {
    use axum::http::StatusCode;
    use zecswap_prices::stand_in;

    use super::*;
    use crate::sends::{covers, now};

    fn pricing(url: &str) -> GasPricing {
        GasPricing::new(&[Provider::CoinMarketCap], &stand_in::keys(), 1_000)
            .unwrap()
            .served_by(url)
    }

    /// A send's fee covers its gas at the rate the relayer quoted: a rate stays good for ten
    /// minutes after it was last quoted, so a proof made with it is taken though the price rose
    /// since; past that the current rate holds; and with gas unpriceable and nothing quoted,
    /// nothing is sent.
    #[tokio::test]
    async fn a_quoted_rate_is_honored_for_ten_minutes_and_gas_is_never_priced_blind() {
        let up = stand_in::start(StatusCode::OK).await;
        let priced = pricing(&up.url);
        let now = now();
        // ETH at $2,400 over USDC at $0.99, and ten percent: 2,666.67 USDC per ETH of gas.
        let current = 2_666_666_666;
        assert_eq!(priced.quote_rate(now).await, Some(current));
        let lower = 2_000_000_000;
        priced
            .rates
            .0
            .lock()
            .unwrap()
            .push_front((now - 540, lower));
        assert_eq!(priced.honored_rate(now).await, Some(lower));
        priced.rates.0.lock().unwrap().front_mut().unwrap().0 = now - 660;
        assert_eq!(priced.honored_rate(now).await, Some(current));

        let down = stand_in::start(StatusCode::SERVICE_UNAVAILABLE).await;
        let blind = pricing(&down.url);
        assert_eq!(blind.honored_rate(now).await, None);
        blind
            .rates
            .0
            .lock()
            .unwrap()
            .push_back((now - 120, current));
        assert_eq!(blind.honored_rate(now).await, Some(current));

        // 1,000,000 gas at 2 gwei is 0.002 ETH: 5,333,333 base units at that rate.
        let (gas, price) = (1_000_000, 2_000_000_000);
        assert!(covers(5_333_334, Some(current), gas, price));
        assert!(!covers(5_333_332, Some(current), gas, price));
        assert!(covers(1, None, gas, price));
    }

    /// A swap's fee is its gas at the gas price and rate of its quote, never under its floor;
    /// a claim signed with a quote is taken for an hour though gas rose since; and with nothing
    /// priced the floor holds, so no claim waits on a price.
    #[tokio::test]
    async fn a_swap_fee_follows_gas_is_held_to_for_an_hour_and_never_holds_a_claim_up() {
        let up = stand_in::start(StatusCode::OK).await;
        let priced = pricing(&up.url);
        let now = now();
        let (floor, gas) = (100_000, 1_800_000);
        let (gwei, doubled) = (1_000_000_000, 2_000_000_000);
        // 1.8M gas at 1 gwei is 0.0018 ETH: 4.8 USDC at 2,666.67 USDC an ETH.
        assert_eq!(
            priced
                .quote_fee(SwapFee::Payout, now, floor, gas, Some(gwei))
                .await,
            (4_799_999, true)
        );
        // Quoted half an hour ago: a claim signed with it goes, though gas doubled since.
        priced.payouts.0.lock().unwrap().front_mut().unwrap().0 = now - 1_800;
        let honored = |gas_price| priced.honored_fee(SwapFee::Payout, now, floor, gas, gas_price);
        assert_eq!(honored(Some(doubled)).await, 4_799_999);
        // An hour on it no longer holds: the fee at today's gas does.
        priced.payouts.0.lock().unwrap().front_mut().unwrap().0 = now - 3_601;
        assert_eq!(honored(Some(doubled)).await, 9_599_999);
        // The funding fee is quoted apart, and cheap gas leaves it at its floor.
        assert_eq!(
            priced
                .quote_fee(SwapFee::Funding, now, floor, gas, Some(gwei / 100))
                .await,
            (floor, true)
        );

        let down = stand_in::start(StatusCode::SERVICE_UNAVAILABLE).await;
        let blind = pricing(&down.url);
        for gas_price in [Some(gwei), None] {
            assert_eq!(
                blind
                    .honored_fee(SwapFee::Payout, now, floor, gas, gas_price)
                    .await,
                floor
            );
        }
    }
}
