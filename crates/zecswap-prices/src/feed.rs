//! The latest USD prices, from the providers in order: each refresh asks the first, and the next
//! only while the one before fails, so prices stop only while every provider fails.

use std::collections::HashMap;
use std::str::FromStr;
use std::sync::RwLock;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result, ensure};
use reqwest::header::{HeaderMap, HeaderValue};
use rust_decimal::Decimal;
use serde::Deserialize;
use zeroize::Zeroizing;

use crate::{ALCHEMY_PRICES, Asset, CMC_QUOTES, History, Provider, read};

const USD_ID: u64 = 2781;
/// An event this recent is valued at the feed's own price; an older one at the candle around it.
const LIVE_WINDOW: u64 = 300;

/// The providers' keys: `ZCASH_CMC_KEY` for CoinMarketCap and `ALCHEMY_API_KEY` for Alchemy.
#[derive(Default)]
pub struct Keys {
    pub cmc: Option<Zeroizing<String>>,
    pub alchemy: Option<Zeroizing<String>>,
}

impl Keys {
    pub fn from_env() -> Self {
        let key = |name| std::env::var(name).ok().map(Zeroizing::new);
        Self {
            cmc: key("ZCASH_CMC_KEY"),
            alchemy: key("ALCHEMY_API_KEY"),
        }
    }
}

/// One asset's price, as its provider stamped it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Quote {
    pub usd: Decimal,
    pub updated_at: u64,
}

/// One provider's answer: every required asset priced, and the optional ones it could.
#[derive(Clone, Debug)]
pub struct Prices {
    pub provider: Provider,
    quotes: HashMap<Asset, Quote>,
    pub fetched_at: u64,
    fetched: Instant,
}

impl Prices {
    pub fn new(provider: Provider, quotes: &[(Asset, Quote)], fetched_at: u64) -> Self {
        Self {
            provider,
            quotes: quotes.iter().copied().collect(),
            fetched_at,
            fetched: Instant::now(),
        }
    }

    pub fn usd(&self, asset: Asset) -> Option<Decimal> {
        self.quotes.get(&asset).map(|quote| quote.usd)
    }

    pub fn quote(&self, asset: Asset) -> Option<Quote> {
        self.quotes.get(&asset).copied()
    }

    /// Whether it may still be used at `now`: each of `assets` priced, and stamped, like the
    /// answer itself, within `max_age` and not ahead of the clock.
    pub fn fresh(&self, assets: &[Asset], max_age: u64, now: u64) -> bool {
        assets
            .iter()
            .map(|asset| self.quotes.get(asset).map(|quote| quote.updated_at))
            .chain([Some(self.fetched_at)])
            .all(|at| {
                at.is_some_and(|at| {
                    at <= now.saturating_add(30) && now.saturating_sub(at) <= max_age
                })
            })
            && self.fetched.elapsed() <= Duration::from_secs(max_age)
    }
}

/// What the feed holds and how its last refresh went.
pub struct Status {
    pub prices: Option<Prices>,
    pub last_attempt_at: Option<u64>,
    /// Set when no provider answered the last refresh.
    pub last_error: Option<String>,
    /// The providers in order, each with its error if its last attempt failed.
    pub providers: Vec<(Provider, Option<String>)>,
}

pub struct Feed {
    sources: Vec<Source>,
    required: Vec<Asset>,
    optional: Vec<Asset>,
    refresh_seconds: u64,
    max_age_seconds: u64,
    state: RwLock<State>,
    refresh_lock: tokio::sync::Mutex<()>,
    /// Alchemy's, where it is a provider: it values late events.
    history: Option<History>,
}

#[derive(Default)]
struct State {
    prices: Option<Prices>,
    last_attempt_at: Option<u64>,
    last_error: Option<String>,
    failures: HashMap<Provider, String>,
}

struct Source {
    provider: Provider,
    client: reqwest::Client,
    url: String,
}

impl Feed {
    /// A feed of `required` and, where a provider prices them, `optional`, refreshed at most
    /// every `refresh_seconds` and trusted for `max_age_seconds`.
    pub fn new(
        providers: &[Provider],
        keys: &Keys,
        required: &[Asset],
        optional: &[Asset],
        refresh_seconds: u64,
        max_age_seconds: u64,
    ) -> Result<Self> {
        ensure!(
            (60..=3600).contains(&refresh_seconds),
            "price refresh must be 60 to 3600 seconds"
        );
        ensure!(
            max_age_seconds > refresh_seconds && max_age_seconds <= 3600,
            "price max age must exceed refresh interval and be at most one hour"
        );
        ensure!(
            !providers.is_empty(),
            "a price feed needs at least one provider"
        );
        let mut sources: Vec<Source> = Vec::new();
        let mut history = None;
        for &provider in providers {
            ensure!(
                sources.iter().all(|source| source.provider != provider),
                "price provider {} is listed twice",
                provider.name()
            );
            let key = match provider {
                Provider::CoinMarketCap => &keys.cmc,
                Provider::Alchemy => &keys.alchemy,
            };
            let key = key.as_ref().map(|key| key.as_str());
            sources.push(Source::new(provider, key)?);
            if provider == Provider::Alchemy {
                history = key.map(History::new).transpose()?;
            }
        }
        Ok(Self {
            sources,
            required: required.to_vec(),
            optional: optional.to_vec(),
            refresh_seconds,
            max_age_seconds,
            state: RwLock::default(),
            refresh_lock: tokio::sync::Mutex::new(()),
            history,
        })
    }

    pub fn max_age_seconds(&self) -> u64 {
        self.max_age_seconds
    }

    pub fn refresh_seconds(&self) -> u64 {
        self.refresh_seconds
    }

    /// The prices held, fresh or not.
    pub fn prices(&self) -> Option<Prices> {
        self.state.read().unwrap().prices.clone()
    }

    /// The prices held, if they may still be used at `now`.
    pub fn fresh(&self, now: u64) -> Option<Prices> {
        self.prices()
            .filter(|prices| prices.fresh(&self.required, self.max_age_seconds, now))
    }

    pub fn status(&self) -> Status {
        let state = self.state.read().unwrap();
        Status {
            prices: state.prices.clone(),
            last_attempt_at: state.last_attempt_at,
            last_error: state.last_error.clone(),
            providers: self
                .sources
                .iter()
                .map(|source| {
                    (
                        source.provider,
                        state.failures.get(&source.provider).cloned(),
                    )
                })
                .collect(),
        }
    }

    /// Requests share one short-lived cache and one in-flight fetch; there is no polling task.
    pub async fn refresh(&self) {
        let due = |now: u64| {
            let state = self.state.read().unwrap();
            let since_attempt = state.last_attempt_at.map(|at| now.saturating_sub(at));
            // Bound retries after an outage, without accepting a price beyond max_age_seconds.
            let interval = if state.last_error.is_some() {
                10
            } else {
                self.refresh_seconds
            };
            since_attempt.is_none_or(|age| age >= interval)
                || state.prices.as_ref().is_some_and(|p| {
                    p.fetched.elapsed() >= Duration::from_secs(interval)
                        && state.last_error.is_none()
                })
        };
        if !due(unix_now()) {
            return;
        }
        let _guard = self.refresh_lock.lock().await;
        if !due(unix_now()) {
            return;
        }
        let mut failures = HashMap::new();
        let mut prices = None;
        for source in &self.sources {
            match source.latest(self).await {
                Ok(answer) => {
                    prices = Some(answer);
                    break;
                }
                Err(error) => {
                    // Provider response bodies and request objects can contain sensitive data.
                    tracing::warn!(
                        provider = source.provider.name(),
                        "price refresh failed: {error}"
                    );
                    failures.insert(source.provider, error);
                }
            }
        }
        let mut state = self.state.write().unwrap();
        state.last_attempt_at = Some(unix_now());
        state.last_error = prices.is_none().then(|| {
            self.sources
                .iter()
                .filter_map(|source| failures.get(&source.provider).cloned())
                .collect::<Vec<_>>()
                .join("; ")
        });
        if prices.is_some() {
            state.prices = prices;
        }
        state.failures = failures;
    }

    /// What `asset` was worth in USD at `at`, and how that is known: the feed's own price if
    /// `at` is recent, else the five-minute candle around it, where Alchemy is a provider. None
    /// when neither is available.
    pub async fn usd_at(&self, asset: Asset, at: u64) -> Option<(String, &'static str)> {
        let now = unix_now();
        if at <= now.saturating_add(30) && now.saturating_sub(at) <= LIVE_WINDOW {
            self.refresh().await;
            if let Some(price) = self.fresh(now).and_then(|prices| prices.usd(asset)) {
                return Some((price.normalize().to_string(), "live"));
            }
        }
        match self.history.as_ref()?.usd_at(asset, at).await {
            Ok(price) => Some((price, "history")),
            Err(error) => {
                tracing::warn!(provider = "alchemy", "price history unavailable: {error}");
                None
            }
        }
    }

    /// Points every provider, and the history, at a stand-in, under its name: for tests.
    #[doc(hidden)]
    pub fn served_by(mut self, base: &str) -> Self {
        for source in &mut self.sources {
            source.url = format!("{base}/{}", source.provider.name());
        }
        self.history = self
            .history
            .map(|history| history.served_by(&format!("{base}/alchemy")));
        self
    }
}

impl Source {
    fn new(provider: Provider, key: Option<&str>) -> Result<Self> {
        let (url, header, variable) = match provider {
            Provider::CoinMarketCap => (CMC_QUOTES, "X-CMC_PRO_API_KEY", "ZCASH_CMC_KEY"),
            Provider::Alchemy => (ALCHEMY_PRICES, "Authorization", "ALCHEMY_API_KEY"),
        };
        let key = key
            .filter(|v| !v.trim().is_empty())
            .with_context(|| format!("{variable} is required for {} pricing", provider.name()))?;
        let value = Zeroizing::new(match provider {
            Provider::CoinMarketCap => key.to_owned(),
            Provider::Alchemy => format!("Bearer {key}"),
        });
        let mut value =
            HeaderValue::from_str(&value).with_context(|| format!("invalid {variable} header"))?;
        value.set_sensitive(true);
        let mut headers = HeaderMap::new();
        headers.insert(header, value);
        Ok(Self {
            provider,
            client: reqwest::Client::builder()
                .default_headers(headers)
                .timeout(Duration::from_secs(8))
                .redirect(reqwest::redirect::Policy::none())
                .build()?,
            url: url.to_owned(),
        })
    }

    async fn latest(&self, feed: &Feed) -> Result<Prices, String> {
        let assets: Vec<Asset> = feed
            .required
            .iter()
            .chain(&feed.optional)
            .copied()
            .collect();
        let request = match self.provider {
            Provider::CoinMarketCap => {
                let ids: Vec<String> = assets.iter().map(|a| a.cmc_id().to_string()).collect();
                self.client
                    .get(&self.url)
                    .query(&[("id", ids.join(",").as_str()), ("convert", "USD")])
            }
            Provider::Alchemy => {
                let symbols: Vec<_> = assets.iter().map(|a| ("symbols", a.symbol())).collect();
                self.client
                    .get(format!("{}/by-symbol", self.url))
                    .query(&symbols)
            }
        };
        let body = read(request, self.provider.label()).await?;
        let now = unix_now();
        let listed = match self.provider {
            Provider::CoinMarketCap => cmc_listings(&body)?,
            Provider::Alchemy => alchemy_listings(&body)?,
        };
        let label = self.provider.label();
        let mut quotes = Vec::new();
        for (asset, required) in feed
            .required
            .iter()
            .map(|a| (*a, true))
            .chain(feed.optional.iter().map(|a| (*a, false)))
        {
            // An optional asset that doesn't read is left out: what it values never waits on it.
            match price(label, &listed, asset, now, feed.max_age_seconds) {
                Ok(quote) => quotes.push((asset, quote)),
                Err(error) if required => return Err(error),
                Err(_) => {}
            }
        }
        Ok(Prices::new(self.provider, &quotes, now))
    }
}

fn unix_now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("clock is after 1970")
        .as_secs()
}

/// What an answer lists of an asset: its USD price and when it was stamped, as raw text; an
/// error where the answer names it other than once.
type Listings = Vec<(Asset, Result<(String, String), String>)>;

fn price(
    label: &str,
    listed: &Listings,
    asset: Asset,
    now: u64,
    max_age: u64,
) -> Result<Quote, String> {
    let symbol = asset.symbol();
    let (raw, updated) = match listed.iter().find(|(listed, _)| *listed == asset) {
        Some((_, entry)) => entry.clone()?,
        None => return Err(format!("{label} {symbol} asset missing or duplicated")),
    };
    let usd = Decimal::from_str(&raw)
        .or_else(|_| Decimal::from_scientific(&raw))
        .map_err(|_| format!("{label} {symbol}/USD price invalid"))?;
    if usd <= Decimal::ZERO {
        return Err(format!("{label} {symbol}/USD price must be positive"));
    }
    let updated_at = chrono::DateTime::parse_from_rfc3339(&updated)
        .ok()
        .and_then(|value| u64::try_from(value.timestamp()).ok())
        .ok_or_else(|| format!("{label} {symbol} timestamp invalid"))?;
    if updated_at > now.saturating_add(30) || now.saturating_sub(updated_at) > max_age {
        return Err(format!(
            "{label} {symbol}/USD quote is stale or ahead of clock"
        ));
    }
    Ok(Quote { usd, updated_at })
}

#[derive(Deserialize)]
struct CmcResponse {
    status: CmcStatus,
    /// Each asset read on its own, so one that doesn't read fails only what needs it.
    data: Vec<serde_json::Value>,
}
#[derive(Deserialize)]
struct CmcStatus {
    #[serde(deserialize_with = "status_code")]
    error_code: u32,
}

fn status_code<'de, D: serde::Deserializer<'de>>(deserializer: D) -> Result<u32, D::Error> {
    #[derive(Deserialize)]
    #[serde(untagged)]
    enum Code {
        Number(u32),
        Text(String),
    }
    match Code::deserialize(deserializer)? {
        Code::Number(code) => Ok(code),
        Code::Text(code) => code
            .parse()
            .map_err(|_| serde::de::Error::custom("invalid CMC status code")),
    }
}
#[derive(Deserialize)]
struct CmcAsset {
    symbol: String,
    quote: Vec<CmcQuote>,
}
#[derive(Deserialize)]
struct CmcQuote {
    id: u64,
    symbol: String,
    price: serde_json::Number,
    last_updated: String,
}

fn cmc_listings(body: &[u8]) -> Result<Listings, String> {
    let data: CmcResponse =
        serde_json::from_slice(body).map_err(|_| "CMC response invalid".to_string())?;
    if data.status.error_code != 0 {
        return Err(format!("CMC error {}", data.status.error_code));
    }
    Ok([Asset::Zec, Asset::Usdc, Asset::Eth]
        .into_iter()
        .map(|asset| {
            let (id, symbol) = (asset.cmc_id(), asset.symbol());
            let entries: Vec<_> = data
                .data
                .iter()
                .filter(|entry| entry.get("id").and_then(serde_json::Value::as_u64) == Some(id))
                .collect();
            let listing = match entries.as_slice() {
                [entry] => serde_json::from_value::<CmcAsset>((*entry).clone())
                    .map_err(|_| "CMC response invalid".to_string())
                    .and_then(|listing| {
                        let quotes: Vec<_> = listing
                            .quote
                            .iter()
                            .filter(|q| q.id == USD_ID && q.symbol == "USD")
                            .collect();
                        match (listing.symbol == symbol, quotes.as_slice()) {
                            (true, [quote]) => {
                                Ok((quote.price.to_string(), quote.last_updated.clone()))
                            }
                            (true, _) => {
                                Err(format!("CMC {symbol}/USD quote missing or duplicated"))
                            }
                            (false, _) => Err(format!("CMC {symbol} asset missing or duplicated")),
                        }
                    }),
                _ => Err(format!("CMC {symbol} asset missing or duplicated")),
            };
            (asset, listing)
        })
        .collect())
}

#[derive(Deserialize)]
struct AlchemyLatest {
    /// Read one by one, as CMC's are.
    data: Vec<serde_json::Value>,
}
#[derive(Deserialize)]
struct AlchemySymbol {
    prices: Vec<AlchemyPrice>,
}
#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct AlchemyPrice {
    currency: String,
    value: String,
    last_updated_at: String,
}

fn alchemy_listings(body: &[u8]) -> Result<Listings, String> {
    let data: AlchemyLatest =
        serde_json::from_slice(body).map_err(|_| "Alchemy response invalid".to_string())?;
    Ok([Asset::Zec, Asset::Usdc, Asset::Eth]
        .into_iter()
        .map(|asset| {
            let symbol = asset.symbol();
            let entries: Vec<_> = data
                .data
                .iter()
                .filter(|entry| {
                    entry.get("symbol").and_then(serde_json::Value::as_str) == Some(symbol)
                })
                .collect();
            let listing = match entries.as_slice() {
                [entry] => serde_json::from_value::<AlchemySymbol>((*entry).clone())
                    .map_err(|_| "Alchemy response invalid".to_string())
                    .and_then(|listing| {
                        let usd: Vec<_> = listing
                            .prices
                            .iter()
                            .filter(|p| p.currency.eq_ignore_ascii_case("usd"))
                            .collect();
                        match usd.as_slice() {
                            [price] => Ok((price.value.clone(), price.last_updated_at.clone())),
                            _ => Err(format!("Alchemy {symbol}/USD quote missing or duplicated")),
                        }
                    }),
                _ => Err(format!("Alchemy {symbol} asset missing or duplicated")),
            };
            (asset, listing)
        })
        .collect())
}

#[cfg(test)]
mod tests {
    use axum::http::StatusCode;
    use serde_json::json;

    use super::*;
    use crate::stand_in::{self, StandIn, alchemy_body, cmc_body, listed};

    const NOW: u64 = 1_767_225_600; // 2026-01-01
    const AT: &str = "2026-01-01T00:00:00.000Z";
    const BOTH: [Provider; 2] = [Provider::CoinMarketCap, Provider::Alchemy];

    fn feed(providers: &[Provider]) -> Feed {
        Feed::new(
            providers,
            &stand_in::keys(),
            &[Asset::Zec, Asset::Usdc],
            &[Asset::Eth],
            60,
            300,
        )
        .unwrap()
    }

    /// ZEC and USDC as a provider's answer prices them at `now`, ETH where it can.
    fn read(provider: Provider, body: &[u8], now: u64) -> Result<Vec<(Asset, Quote)>, String> {
        let listed = match provider {
            Provider::CoinMarketCap => cmc_listings(body)?,
            Provider::Alchemy => alchemy_listings(body)?,
        };
        let mut quotes = Vec::new();
        for asset in [Asset::Zec, Asset::Usdc] {
            quotes.push((asset, price(provider.label(), &listed, asset, now, 300)?));
        }
        quotes.extend(
            price(provider.label(), &listed, Asset::Eth, now, 300)
                .ok()
                .map(|quote| (Asset::Eth, quote)),
        );
        Ok(quotes)
    }

    #[test]
    fn supports_live_v3_text_status_codes_and_documented_numeric_status_codes() {
        let mut body: serde_json::Value = serde_json::from_slice(&cmc_body("40", "1", AT)).unwrap();
        for code in [json!("0"), json!(0)] {
            body["status"]["error_code"] = code;
            assert!(
                read(
                    Provider::CoinMarketCap,
                    &serde_json::to_vec(&body).unwrap(),
                    NOW
                )
                .is_ok()
            );
        }
        for code in [
            json!("1001"),
            json!(1001),
            json!("invalid"),
            json!(-1),
            json!(null),
        ] {
            body["status"]["error_code"] = code;
            assert!(
                read(
                    Provider::CoinMarketCap,
                    &serde_json::to_vec(&body).unwrap(),
                    NOW
                )
                .is_err()
            );
        }
    }

    #[test]
    fn stale_or_invalid_provider_values_are_rejected() {
        for (zec, usdc, at, now) in [
            ("0", "1", AT, NOW),
            ("-1", "1", AT, NOW),
            ("40", "0", AT, NOW),
            ("40", "1", AT, NOW + 301),
            ("40", "1", AT, NOW - 31),
            ("40", "1", "bad date", NOW),
            ("1e100", "1", AT, NOW),
        ] {
            assert!(read(Provider::CoinMarketCap, &cmc_body(zec, usdc, at), now).is_err());
            let answer = alchemy_body(json!([listed("ZEC", zec, at), listed("USDC", usdc, at)]));
            assert!(read(Provider::Alchemy, &answer, now).is_err());
        }
        let mut data: serde_json::Value = serde_json::from_slice(&cmc_body("40", "1", AT)).unwrap();
        data["data"][0]["id"] = json!(1);
        assert!(
            read(
                Provider::CoinMarketCap,
                &serde_json::to_vec(&data).unwrap(),
                NOW
            )
            .is_err()
        );
        assert!(read(Provider::CoinMarketCap, b"invalid json", NOW).is_err());
        let scientific = read(Provider::CoinMarketCap, &cmc_body("4e1", "1e0", AT), NOW).unwrap();
        assert_eq!(scientific[0].1.usd, Decimal::from(40));
    }

    /// Alchemy lists by symbol: an answer missing ZEC or USDC, naming one twice, or pricing it in
    /// another currency prices nothing; ETH, which only values gas, may be missing or unreadable.
    #[test]
    fn alchemy_answers_must_name_each_asset_once_in_usd() {
        let read = |entries| read(Provider::Alchemy, &alchemy_body(entries), NOW);
        let priced = read(json!([
            listed("ZEC", "1185.19", AT),
            listed("USDC", "1.00062", AT),
            listed("ETH", "2473.68", AT)
        ]))
        .unwrap();
        assert_eq!(priced.len(), 3);
        for entries in [
            json!([listed("USDC", "1", AT)]),
            json!([
                listed("ZEC", "40", AT),
                listed("ZEC", "41", AT),
                listed("USDC", "1", AT)
            ]),
            json!([{"symbol": "ZEC", "prices": [], "error": {"message": "Price not found"}}, listed("USDC", "1", AT)]),
            json!([{"symbol": "ZEC", "prices": [{"currency": "eur", "value": "40", "lastUpdatedAt": AT}]}, listed("USDC", "1", AT)]),
            json!([{"symbol": "ZEC", "prices": [{"currency": "usd", "value": 40, "lastUpdatedAt": AT}]}, listed("USDC", "1", AT)]),
        ] {
            assert!(read(entries).is_err());
        }
        let without_eth = read(json!([listed("ZEC", "40", AT), listed("USDC", "1", AT), {"symbol": "ETH", "prices": "unreadable"}])).unwrap();
        assert_eq!(without_eth.len(), 2);
    }

    /// ETH rides along: missing, invalid or stale, it is left out and ZEC and USDC are as they
    /// would be without it; an entry that doesn't read fails the answer unless it is ETH's.
    #[test]
    fn eth_never_holds_up_what_is_required() {
        let with_eth = |price: serde_json::Value, at: &str| {
            let mut body: serde_json::Value =
                serde_json::from_slice(&cmc_body("40", "1", AT)).unwrap();
            body["data"].as_array_mut().unwrap().push(json!({
                "id": 1027, "symbol": "ETH",
                "quote": [{"id": 2781, "symbol": "USD", "price": price, "last_updated": at}],
            }));
            read(
                Provider::CoinMarketCap,
                &serde_json::to_vec(&body).unwrap(),
                NOW,
            )
            .unwrap()
        };
        assert_eq!(
            with_eth(json!(2500.5), AT)[2],
            (
                Asset::Eth,
                Quote {
                    usd: Decimal::from_str("2500.5").unwrap(),
                    updated_at: NOW
                }
            )
        );
        for (price, at) in [
            (json!(-1), AT),
            (json!(0), AT),
            (json!("2500"), AT),
            (json!(2500), "2025-12-31T23:00:00.000Z"),
        ] {
            assert_eq!(with_eth(price, at).len(), 2);
        }
        let mut body: serde_json::Value = serde_json::from_slice(&cmc_body("40", "1", AT)).unwrap();
        body["data"][0]["quote"][0]["price"] = json!("40");
        assert_eq!(
            read(
                Provider::CoinMarketCap,
                &serde_json::to_vec(&body).unwrap(),
                NOW
            )
            .err()
            .as_deref(),
            Some("CMC response invalid")
        );
    }

    #[tokio::test]
    async fn concurrent_requests_share_one_fetch_and_provider_errors_never_expose_the_key() {
        let up = stand_in::start(StatusCode::OK).await;
        let feed = feed(&BOTH).served_by(&up.url);
        tokio::join!(feed.refresh(), feed.refresh(), feed.refresh());
        feed.refresh().await;
        assert_eq!((StandIn::hits(&up.cmc), StandIn::hits(&up.alchemy)), (1, 0));
        assert!(feed.fresh(unix_now()).is_some());

        let down = stand_in::start(StatusCode::TOO_MANY_REQUESTS).await;
        let cmc_only = self::feed(&[Provider::CoinMarketCap]).served_by(&down.url);
        cmc_only.refresh().await;
        let status = cmc_only.status();
        assert_eq!(status.last_error.as_deref(), Some("CMC HTTP 429"));
        assert!(!format!("{:?}", status.providers).contains(stand_in::CMC_KEY));
        assert!(status.prices.is_none());
    }

    /// While CoinMarketCap fails, Alchemy answers and the status says which did and why the first
    /// didn't; the next refresh asks CoinMarketCap first again.
    #[tokio::test]
    async fn a_failing_provider_hands_over_to_the_next() {
        let down = stand_in::start(StatusCode::SERVICE_UNAVAILABLE).await;
        let feed = feed(&BOTH).served_by(&down.url);
        feed.refresh().await;
        let prices = feed.fresh(unix_now()).unwrap();
        assert_eq!(
            (prices.provider, prices.usd(Asset::Zec)),
            (Provider::Alchemy, Some(Decimal::from(50)))
        );
        let status = feed.status();
        assert_eq!(status.last_error, None);
        assert_eq!(
            status.providers,
            [
                (Provider::CoinMarketCap, Some("CMC HTTP 503".into())),
                (Provider::Alchemy, None)
            ]
        );

        let up = stand_in::start(StatusCode::OK).await;
        let feed = feed.served_by(&up.url);
        feed.state.write().unwrap().last_attempt_at = None;
        feed.refresh().await;
        let status = feed.status();
        assert_eq!(status.prices.unwrap().provider, Provider::CoinMarketCap);
        assert!(status.providers.iter().all(|(_, error)| error.is_none()));
        assert_eq!((StandIn::hits(&up.cmc), StandIn::hits(&up.alchemy)), (1, 0));
    }

    /// An event seen as it happens is valued at the feed's own price; one seen later, after a
    /// restart, at Alchemy's candle around it, each candle asked for once; without Alchemy among
    /// the providers, not at all rather than at today's price.
    #[tokio::test]
    async fn events_are_valued_when_they_happened() {
        let up = stand_in::start(StatusCode::OK).await;
        let feed = feed(&BOTH).served_by(&up.url);
        let now = unix_now();
        assert_eq!(
            feed.usd_at(Asset::Eth, now - 60).await,
            Some(("2400".into(), "live"))
        );
        // Twenty seconds into a candle six hours back: that candle is the nearest.
        let at = now / crate::CANDLE * crate::CANDLE - 6 * 3600 + 20;
        let expected = format!("{}.5", 2000 + (at / crate::CANDLE) % 1000);
        assert_eq!(
            feed.usd_at(Asset::Eth, at).await,
            Some((expected.clone(), "history"))
        );
        assert_eq!(
            feed.usd_at(Asset::Eth, at + 1).await,
            Some((expected, "history"))
        );
        assert_eq!(StandIn::hits(&up.history), 1);
        let cmc_only = self::feed(&[Provider::CoinMarketCap]).served_by(&up.url);
        assert_eq!(cmc_only.usd_at(Asset::Eth, at).await, None);
    }

    #[test]
    fn validates_its_configuration() {
        let needs = |providers: &[Provider], cmc: Option<&str>, alchemy: Option<&str>| {
            let keys = Keys {
                cmc: cmc.map(|k| Zeroizing::new(k.into())),
                alchemy: alchemy.map(|k| Zeroizing::new(k.into())),
            };
            Feed::new(providers, &keys, &[Asset::Eth], &[], 60, 300)
                .err()
                .map(|e| e.to_string())
        };
        assert_eq!(
            needs(&BOTH, Some("k"), None).as_deref(),
            Some("ALCHEMY_API_KEY is required for alchemy pricing")
        );
        assert_eq!(
            needs(&[Provider::Alchemy], None, Some(" ")).as_deref(),
            Some("ALCHEMY_API_KEY is required for alchemy pricing")
        );
        assert_eq!(
            needs(&[Provider::Alchemy, Provider::Alchemy], None, Some("k")).as_deref(),
            Some("price provider alchemy is listed twice")
        );
        assert!(needs(&[], Some("k"), Some("k")).is_some());
        assert_eq!(needs(&[Provider::Alchemy], None, Some("k")), None);
        assert!(Feed::new(&BOTH, &stand_in::keys(), &[Asset::Eth], &[], 0, 300).is_err());
        assert!(Feed::new(&BOTH, &stand_in::keys(), &[Asset::Eth], &[], 60, 60).is_err());
    }
}
