use anyhow::{Result, ensure};
use rust_decimal::{Decimal, prelude::ToPrimitive};
use serde::Serialize;
use zecswap_prices::{Feed, Keys, Prices};

pub(crate) use zecswap_prices::Asset;

use crate::pricing::{Costs, Pricing, Terms};

/// What every quote's price needs; ETH rides along for gas values only, unless the maker
/// charges its network cost.
const QUOTED: [Asset; 2] = [Asset::Zec, Asset::Usdc];
const QUOTED_WITH_GAS: [Asset; 3] = [Asset::Zec, Asset::Usdc, Asset::Eth];

pub(crate) struct PriceBook {
    policy: Pricing,
    /// With `[pricing.market]`; a fixed price needs none.
    feed: Option<Feed>,
}

pub(crate) struct QuotePricing {
    pub policy: Pricing,
    observed: Option<Prices>,
    max_age: u64,
    /// With `[pricing.costs]`.
    costs: Option<NetworkCosts>,
}

/// What a quote charges for the maker's own gas and Zcash fee, in token base units, and the gas
/// price it was charged at.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct NetworkCosts {
    pub forward: u128,
    pub reverse: u128,
    pub gas_price: u128,
}

/// The market prices a quote was made at, and where they came from.
pub(crate) struct QuoteMark {
    pub source: &'static str,
    pub zec_usd: String,
    pub usdc_usd: String,
    pub eth_usd: Option<String>,
    /// What it charged for the maker's network cost, and the gas price that was charged at.
    pub network_cost: Option<(u128, u128)>,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct PriceSnapshot {
    pub source: &'static str,
    pub status: &'static str,
    pub quotes_available: bool,
    pub price_per_zec: Option<String>,
    pub zec_usd: Option<String>,
    pub usdc_usd: Option<String>,
    pub zec_updated_at: Option<u64>,
    pub usdc_updated_at: Option<u64>,
    pub eth_usd: Option<String>,
    pub eth_updated_at: Option<u64>,
    pub fetched_at: Option<u64>,
    pub last_attempt_at: Option<u64>,
    pub last_error: Option<String>,
    pub refresh_seconds: Option<u64>,
    pub max_age_seconds: Option<u64>,
    pub token_decimals: Option<u8>,
    pub providers: Vec<ProviderStatus>,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct ProviderStatus {
    pub provider: &'static str,
    pub last_error: Option<String>,
}

impl PriceBook {
    pub fn from_env(policy: &Pricing) -> Result<Self> {
        Self::new(policy, &Keys::from_env())
    }

    pub(crate) fn new(policy: &Pricing, keys: &Keys) -> Result<Self> {
        ensure!(
            policy.unit > 0 && policy.max_units > 0,
            "pricing denominations must be positive"
        );
        ensure!(
            policy.spread_bps < 10_000,
            "pricing spread must be below 100%"
        );
        ensure!(
            policy.costs.is_none() || policy.market.is_some(),
            "network costs need market prices: [pricing.market]"
        );
        let feed = match &policy.market {
            Some(config) => {
                // USDC uses six decimals. Test tokens must explicitly use that same precision.
                ensure!(
                    config.token_decimals == 6,
                    "market pricing requires a six-decimal USDC token"
                );
                let (required, optional): (&[Asset], &[Asset]) = match policy.costs {
                    Some(_) => (&QUOTED_WITH_GAS, &[]),
                    None => (&QUOTED, &[Asset::Eth]),
                };
                Some(Feed::new(
                    &config.providers,
                    keys,
                    required,
                    optional,
                    config.refresh_seconds,
                    config.max_age_seconds,
                )?)
            }
            None => {
                ensure!(policy.price_per_zec > 0, "fixed price must be positive");
                None
            }
        };
        Ok(Self {
            policy: policy.clone(),
            feed,
        })
    }

    /// A quote's pricing now: none while the market prices are stale, or, where the maker
    /// charges its network cost, without a `gas_price` to charge it at.
    pub fn quote(&self, now: u64, gas_price: Option<u128>) -> Option<QuotePricing> {
        let mut policy = self.policy.clone();
        let Some(feed) = &self.feed else {
            return Some(QuotePricing {
                policy,
                observed: None,
                max_age: 0,
                costs: None,
            });
        };
        let prices = feed.fresh(now)?;
        policy.price_per_zec = price_per_zec(&prices, self.decimals())?;
        let costs = match &self.policy.costs {
            Some(costs) => Some(NetworkCosts::at(
                costs,
                gas_price?,
                &prices,
                self.decimals(),
            )?),
            None => None,
        };
        Some(QuotePricing {
            policy,
            observed: Some(prices),
            max_age: feed.max_age_seconds(),
            costs,
        })
    }

    /// Whether quotes charge the maker's network cost, and so need a gas price.
    pub fn charges_costs(&self) -> bool {
        self.policy.costs.is_some()
    }

    pub fn snapshot(&self, now: u64) -> PriceSnapshot {
        let Some(feed) = &self.feed else {
            return PriceSnapshot {
                source: "fixed",
                status: "fixed",
                quotes_available: true,
                price_per_zec: Some(self.policy.price_per_zec.to_string()),
                zec_usd: None,
                usdc_usd: None,
                zec_updated_at: None,
                usdc_updated_at: None,
                eth_usd: None,
                eth_updated_at: None,
                fetched_at: None,
                last_attempt_at: None,
                last_error: None,
                refresh_seconds: None,
                max_age_seconds: None,
                token_decimals: None,
                providers: Vec::new(),
            };
        };
        let status = feed.status();
        let prices = status.prices.as_ref();
        let rate = prices.and_then(|prices| price_per_zec(prices, self.decimals()));
        let fresh = prices.is_some_and(|p| p.fresh(&QUOTED, feed.max_age_seconds(), now));
        let usd = |asset| {
            prices
                .and_then(|p| p.usd(asset))
                .map(|usd: Decimal| usd.normalize().to_string())
        };
        let at = |asset| {
            prices
                .and_then(|p| p.quote(asset))
                .map(|quote| quote.updated_at)
        };
        PriceSnapshot {
            source: prices.map_or(status.providers[0].0, |p| p.provider).name(),
            status: match (prices, fresh) {
                (None, _) => "unavailable",
                (Some(_), true) => "fresh",
                (Some(_), false) => "stale",
            },
            quotes_available: fresh && rate.is_some(),
            price_per_zec: rate.map(|rate| rate.to_string()),
            zec_usd: usd(Asset::Zec),
            usdc_usd: usd(Asset::Usdc),
            zec_updated_at: at(Asset::Zec),
            usdc_updated_at: at(Asset::Usdc),
            eth_usd: usd(Asset::Eth),
            eth_updated_at: at(Asset::Eth),
            fetched_at: prices.map(|p| p.fetched_at),
            last_attempt_at: status.last_attempt_at,
            last_error: status.last_error,
            refresh_seconds: Some(feed.refresh_seconds()),
            max_age_seconds: Some(feed.max_age_seconds()),
            token_decimals: Some(self.decimals()),
            providers: status
                .providers
                .into_iter()
                .map(|(provider, last_error)| ProviderStatus {
                    provider: provider.name(),
                    last_error,
                })
                .collect(),
        }
    }

    /// Requests share one short-lived cache and one in-flight fetch; there is no polling task.
    pub async fn refresh(&self) {
        if let Some(feed) = &self.feed {
            feed.refresh().await;
        }
    }

    /// What `asset` was worth in USD at `at`, and how that is known: `live` from the maker's
    /// own price if recent, `history` from Alchemy's candle around it. None when neither is.
    pub async fn usd_at(&self, asset: Asset, at: u64) -> Option<(String, &'static str)> {
        self.feed.as_ref()?.usd_at(asset, at).await
    }

    fn decimals(&self) -> u8 {
        self.policy.market.as_ref().map_or(6, |c| c.token_decimals)
    }

    /// Points the feed at a local stand-in.
    #[cfg(test)]
    pub(crate) fn served_by(mut self, base: &str) -> Self {
        self.feed = self.feed.map(|feed| feed.served_by(base));
        self
    }
}

/// ZEC/USDC: ZEC/USD over USDC/USD, in token units per whole ZEC.
fn price_per_zec(prices: &Prices, decimals: u8) -> Option<u128> {
    let scale = Decimal::from(10u64.pow(decimals.into()));
    prices
        .usd(Asset::Zec)?
        .checked_div(prices.usd(Asset::Usdc)?)?
        .checked_mul(scale)?
        .trunc()
        .to_u128()
        .filter(|rate| *rate > 0)
}

impl QuotePricing {
    // Recheck the captured price after RPCs or a wait for the wallet, before issuing a quote.
    pub fn fresh(&self, now: u64) -> bool {
        let quoted: &[Asset] = match self.costs {
            Some(_) => &QUOTED_WITH_GAS,
            None => &QUOTED,
        };
        match &self.observed {
            Some(prices) => prices.fresh(quoted, self.max_age, now),
            None => self.policy.market.is_none(),
        }
    }

    /// A swap from ZEC to USDC, its network cost in the deposit.
    pub fn terms(&self, units: u32) -> Option<Terms> {
        self.policy.terms(units, self.forward_cost().unwrap_or(0))
    }

    /// A swap from USDC to ZEC, its network cost kept from the ZEC paid.
    pub fn reverse_terms(&self, units: u32) -> Option<Terms> {
        self.policy
            .reverse_terms(units, self.reverse_cost().unwrap_or(0))
    }

    pub fn forward_cost(&self) -> Option<u128> {
        self.costs.map(|costs| costs.forward)
    }

    pub fn reverse_cost(&self) -> Option<u128> {
        self.costs.map(|costs| costs.reverse)
    }

    /// The market prices behind a quote that charged `cost` for the maker's network cost; none
    /// for a fixed price.
    pub fn mark(&self, cost: Option<u128>) -> Option<QuoteMark> {
        let prices = self.observed.as_ref()?;
        let usd = |asset| prices.usd(asset).map(|usd| usd.normalize().to_string());
        Some(QuoteMark {
            source: prices.provider.name(),
            zec_usd: usd(Asset::Zec)?,
            usdc_usd: usd(Asset::Usdc)?,
            eth_usd: usd(Asset::Eth),
            network_cost: self
                .costs
                .zip(cost)
                .map(|(costs, cost)| (cost, costs.gas_price)),
        })
    }
}

impl NetworkCosts {
    fn at(costs: &Costs, gas_price: u128, prices: &Prices, decimals: u8) -> Option<Self> {
        let cost = |gas| network_cost(gas, gas_price, costs, prices, decimals);
        Some(Self {
            forward: cost(costs.forward_gas)?,
            reverse: cost(costs.reverse_gas)?,
            gas_price,
        })
    }
}

/// `gas` at `gas_price` and the Zcash fee, at `prices` and with the margin on top, in token
/// base units rounded up.
fn network_cost(
    gas: u64,
    gas_price: u128,
    costs: &Costs,
    prices: &Prices,
    decimals: u8,
) -> Option<u128> {
    let wei = i128::try_from(u128::from(gas).checked_mul(gas_price)?).ok()?;
    let eth = Decimal::try_from_i128_with_scale(wei, 18).ok()?;
    let zec = Decimal::try_from_i128_with_scale(costs.zcash_fee_zat.into(), 8).ok()?;
    let usd = eth
        .checked_mul(prices.usd(Asset::Eth)?)?
        .checked_add(zec.checked_mul(prices.usd(Asset::Zec)?)?)?;
    let margin = Decimal::from(10_000 + costs.margin_bps) / Decimal::from(10_000);
    usd.checked_mul(margin)?
        .checked_div(prices.usd(Asset::Usdc)?)?
        .checked_mul(Decimal::from(10u64.pow(decimals.into())))?
        .ceil()
        .to_u128()
}

#[cfg(test)]
mod tests {
    use std::str::FromStr;

    use axum::http::StatusCode;
    use zecswap_prices::stand_in::{self, StandIn};
    use zecswap_prices::{Provider, Quote};

    use super::*;
    use crate::pricing::MarketConfig;

    fn policy() -> Pricing {
        Pricing {
            price_per_zec: 500_000_000,
            spread_bps: 100,
            unit: 1_000_000,
            max_units: 20,
            market: Some(MarketConfig {
                token_decimals: 6,
                refresh_seconds: 60,
                max_age_seconds: 300,
                providers: vec![Provider::CoinMarketCap, Provider::Alchemy],
            }),
            costs: None,
        }
    }

    fn book() -> PriceBook {
        PriceBook::new(&policy(), &stand_in::keys()).unwrap()
    }

    #[test]
    fn prices_both_directions_with_exact_decimals_and_actual_usdc_usd() {
        let quote = |usd: &str| Quote {
            usd: Decimal::from_str(usd).unwrap(),
            updated_at: 0,
        };
        let prices = Prices::new(
            Provider::CoinMarketCap,
            &[
                (Asset::Zec, quote("1334.6563794177857")),
                (Asset::Usdc, quote("0.9999")),
            ],
            0,
        );
        let rate = price_per_zec(&prices, 6).unwrap();
        assert_eq!(rate, 1_334_789_858);
        let pricing = Pricing {
            price_per_zec: rate,
            ..policy()
        };
        let forward = pricing.terms(20, 0).unwrap();
        let reverse = pricing.reverse_terms(20, 0).unwrap();
        assert_eq!(forward.amount, 20_000_000);
        assert_eq!(forward.deposit_zat, 1_513_499);
        assert_eq!(reverse.deposit_zat, 1_483_379);
        assert!(forward.deposit_zat > reverse.deposit_zat);
    }

    /// A quote is priced only while the market price is fresh, never at the fixed price, and a
    /// quote's captured price goes stale on its own however the market moves after.
    #[tokio::test]
    async fn never_falls_back_to_fixed_price_and_holds_a_captured_rate_until_it_expires() {
        let cmc_only = || {
            PriceBook::new(
                &Pricing {
                    market: Some(MarketConfig {
                        providers: vec![Provider::CoinMarketCap],
                        ..policy().market.unwrap()
                    }),
                    ..policy()
                },
                &stand_in::keys(),
            )
            .unwrap()
        };
        let down = stand_in::start(StatusCode::SERVICE_UNAVAILABLE).await;
        let book = cmc_only().served_by(&down.url);
        let now = crate::maker::unix_now();
        book.refresh().await;
        assert!(book.quote(now, None).is_none());
        let snapshot = book.snapshot(now);
        assert_eq!(
            (snapshot.status, snapshot.quotes_available),
            ("unavailable", false)
        );
        assert_eq!(snapshot.last_error.as_deref(), Some("CMC HTTP 503"));

        let up = stand_in::start(StatusCode::OK).await;
        let book = cmc_only().served_by(&up.url);
        book.refresh().await;
        let now = crate::maker::unix_now();
        let locked = book.quote(now, None).unwrap();
        // ZEC at $40 over USDC at $0.99.
        assert_eq!(locked.policy.price_per_zec, 40_404_040);
        assert!(locked.fresh(now + 300) && !locked.fresh(now + 301));
        assert!(book.quote(now + 301, None).is_none());
        assert_eq!(book.snapshot(now + 301).status, "stale");
    }

    /// While CoinMarketCap fails, Alchemy prices quotes, and the quote and the monitor say so.
    #[tokio::test]
    async fn a_failing_provider_hands_quotes_to_the_next() {
        let down = stand_in::start(StatusCode::SERVICE_UNAVAILABLE).await;
        let book = book().served_by(&down.url);
        book.refresh().await;
        let now = crate::maker::unix_now();
        let quote = book.quote(now, None).unwrap();
        assert_eq!(quote.policy.price_per_zec, 50_000_000);
        let mark = quote.mark(None).unwrap();
        assert_eq!(
            (mark.source, mark.zec_usd.as_str(), mark.eth_usd.as_deref()),
            ("alchemy", "50", Some("2500"))
        );
        let snapshot = book.snapshot(now);
        assert_eq!((snapshot.source, snapshot.status), ("alchemy", "fresh"));
        let errors: Vec<_> = snapshot
            .providers
            .iter()
            .map(|p| (p.provider, p.last_error.as_deref()))
            .collect();
        assert_eq!(
            errors,
            [("coinmarketcap", Some("CMC HTTP 503")), ("alchemy", None)]
        );
        assert_eq!(StandIn::hits(&down.alchemy), 1);
    }

    #[test]
    fn validates_market_configuration_and_does_not_require_a_key_for_fixed_mode() {
        let mut p = policy();
        p.market.as_mut().unwrap().token_decimals = 18;
        assert!(PriceBook::new(&p, &stand_in::keys()).is_err());
        assert!(PriceBook::new(&policy(), &Keys::default()).is_err());
        p = policy();
        p.market = None;
        let fixed = PriceBook::new(&p, &Keys::default()).unwrap();
        assert_eq!(
            fixed.quote(0, None).unwrap().policy.price_per_zec,
            500_000_000
        );
        let snapshot = fixed.snapshot(0);
        assert_eq!(snapshot.status, "fixed");
        assert!(snapshot.providers.is_empty());
        assert!(fixed.quote(0, None).unwrap().mark(None).is_none());
        p.price_per_zec = 0;
        assert!(PriceBook::new(&p, &Keys::default()).is_err());
    }

    /// Where the maker charges its network cost, each quote carries its gas at the gas price of
    /// the moment and its Zcash fee, at the quote's prices: in the deposit one way, kept from
    /// the ZEC the other. Without a gas price, or ETH's, there is no quote.
    #[tokio::test]
    async fn a_quote_charges_the_makers_gas_and_zcash_fee_at_the_prices_of_the_moment() {
        let costs = Pricing {
            costs: Some(Costs {
                forward_gas: 400_000,
                reverse_gas: 350_000,
                zcash_fee_zat: 10_000,
                margin_bps: 0,
            }),
            market: Some(MarketConfig {
                providers: vec![Provider::CoinMarketCap],
                ..policy().market.unwrap()
            }),
            ..policy()
        };
        let up = stand_in::start(StatusCode::OK).await;
        let book = PriceBook::new(&costs, &stand_in::keys())
            .unwrap()
            .served_by(&up.url);
        book.refresh().await;
        let now = crate::maker::unix_now();
        assert!(book.quote(now, None).is_none());
        let quote = book.quote(now, Some(1_000_000_000)).unwrap();
        // 0.0004 ETH at $2,400 and 0.0001 ZEC at $40 is $0.964: 0.973738 USDC at $0.99.
        assert_eq!(
            (quote.forward_cost(), quote.reverse_cost()),
            (Some(973_738), Some(852_526))
        );
        let charged = quote.terms(1).unwrap();
        let plain = quote.policy.terms(1, 0).unwrap();
        assert_eq!(charged.amount, plain.amount);
        assert!(charged.deposit_zat > plain.deposit_zat);
        assert!(
            quote.reverse_terms(1).unwrap().deposit_zat
                < quote.policy.reverse_terms(1, 0).unwrap().deposit_zat
        );
        let mark = quote.mark(quote.forward_cost()).unwrap();
        assert_eq!(mark.network_cost, Some((973_738, 1_000_000_000)));

        let fixed = Pricing {
            market: None,
            ..costs
        };
        assert!(PriceBook::new(&fixed, &Keys::default()).is_err());
    }
}
