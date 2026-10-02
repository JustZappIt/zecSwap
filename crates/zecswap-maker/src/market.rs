use std::str::FromStr;
use std::sync::RwLock;
use std::time::{Duration, Instant};

use anyhow::{Context, Result, ensure};
use reqwest::header::{HeaderMap, HeaderValue};
use rust_decimal::{Decimal, prelude::ToPrimitive};
use serde::{Deserialize, Serialize};

use crate::pricing::{MarketConfig, Pricing};

const CMC_QUOTES: &str = "https://pro-api.coinmarketcap.com/v3/cryptocurrency/quotes/latest";
const ZEC_ID: u64 = 1437;
const USDC_ID: u64 = 3408;
const USD_ID: u64 = 2781;

pub(crate) struct PriceBook {
    policy: Pricing,
    client: Option<reqwest::Client>,
    state: RwLock<State>,
    refresh_lock: tokio::sync::Mutex<()>,
}

#[derive(Default)]
struct State {
    price: Option<MarketPrice>,
    last_attempt_at: Option<u64>,
    last_error: Option<String>,
}

#[derive(Clone)]
struct MarketPrice {
    price_per_zec: u128,
    zec_usd: String,
    usdc_usd: String,
    zec_updated_at: u64,
    usdc_updated_at: u64,
    fetched_at: u64,
    fetched: Instant,
}

pub(crate) struct QuotePricing {
    pub policy: Pricing,
    observed: Option<MarketPrice>,
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
    pub fetched_at: Option<u64>,
    pub last_attempt_at: Option<u64>,
    pub last_error: Option<String>,
    pub refresh_seconds: Option<u64>,
    pub max_age_seconds: Option<u64>,
    pub token_decimals: Option<u8>,
}

impl PriceBook {
    pub fn from_env(policy: &Pricing) -> Result<Self> {
        let key = std::env::var("ZCASH_CMC_KEY")
            .ok()
            .map(zeroize::Zeroizing::new);
        Self::new(policy, key.as_ref().map(|v| v.as_str()))
    }

    fn new(policy: &Pricing, key: Option<&str>) -> Result<Self> {
        ensure!(
            policy.unit > 0 && policy.max_units > 0,
            "pricing denominations must be positive"
        );
        ensure!(
            policy.spread_bps < 10_000,
            "pricing spread must be below 100%"
        );
        let client = if let Some(config) = &policy.market {
            // USDC uses six decimals. Test tokens must explicitly use that same precision.
            ensure!(
                config.token_decimals == 6,
                "CMC pricing requires a six-decimal USDC token"
            );
            ensure!(
                (60..=3600).contains(&config.refresh_seconds),
                "price refresh must be 60 to 3600 seconds"
            );
            ensure!(
                config.max_age_seconds > config.refresh_seconds && config.max_age_seconds <= 3600,
                "price max age must exceed refresh interval and be at most one hour"
            );
            let mut value = HeaderValue::from_str(
                key.filter(|v| !v.trim().is_empty())
                    .context("ZCASH_CMC_KEY is required for market pricing")?,
            )
            .context("invalid ZCASH_CMC_KEY header")?;
            value.set_sensitive(true);
            let mut headers = HeaderMap::new();
            headers.insert("X-CMC_PRO_API_KEY", value);
            Some(
                reqwest::Client::builder()
                    .default_headers(headers)
                    .timeout(Duration::from_secs(8))
                    .redirect(reqwest::redirect::Policy::none())
                    .build()?,
            )
        } else {
            ensure!(policy.price_per_zec > 0, "fixed price must be positive");
            None
        };
        Ok(Self {
            policy: policy.clone(),
            client,
            state: RwLock::new(State::default()),
            refresh_lock: tokio::sync::Mutex::new(()),
        })
    }

    pub fn quote(&self, now: u64) -> Option<QuotePricing> {
        let mut policy = self.policy.clone();
        let observed = if let Some(config) = &policy.market {
            let price = self.state.read().unwrap().price.clone()?;
            if !price.fresh(config, now) {
                return None;
            }
            policy.price_per_zec = price.price_per_zec;
            Some(price)
        } else {
            None
        };
        Some(QuotePricing { policy, observed })
    }

    pub fn snapshot(&self, now: u64) -> PriceSnapshot {
        let state = self.state.read().unwrap();
        let config = self.policy.market.as_ref();
        let price = state.price.as_ref();
        let status = match config {
            None => "fixed",
            Some(config) => match price {
                Some(price) if price.fresh(config, now) => "fresh",
                Some(_) => "stale",
                None => "unavailable",
            },
        };
        PriceSnapshot {
            source: if config.is_some() {
                "coinmarketcap"
            } else {
                "fixed"
            },
            status,
            quotes_available: matches!(status, "fresh" | "fixed"),
            price_per_zec: if config.is_some() {
                price.map(|p| p.price_per_zec.to_string())
            } else {
                Some(self.policy.price_per_zec.to_string())
            },
            zec_usd: price.map(|p| p.zec_usd.clone()),
            usdc_usd: price.map(|p| p.usdc_usd.clone()),
            zec_updated_at: price.map(|p| p.zec_updated_at),
            usdc_updated_at: price.map(|p| p.usdc_updated_at),
            fetched_at: price.map(|p| p.fetched_at),
            last_attempt_at: state.last_attempt_at,
            last_error: state.last_error.clone(),
            refresh_seconds: config.map(|c| c.refresh_seconds),
            max_age_seconds: config.map(|c| c.max_age_seconds),
            token_decimals: config.map(|c| c.token_decimals),
        }
    }

    /// Requests share one short-lived cache and one in-flight fetch; there is no polling task.
    pub async fn refresh(&self) {
        self.refresh_from(CMC_QUOTES).await;
    }

    async fn refresh_from(&self, url: &str) {
        let Some(config) = &self.policy.market else {
            return;
        };
        let due = |now: u64| {
            let state = self.state.read().unwrap();
            let since_attempt = state.last_attempt_at.map(|at| now.saturating_sub(at));
            // Bound retries after an outage, without accepting a price beyond max_age_seconds.
            let interval = if state.last_error.is_some() {
                10
            } else {
                config.refresh_seconds
            };
            since_attempt.is_none_or(|age| age >= interval)
                || state.price.as_ref().is_some_and(|p| {
                    p.fetched.elapsed() >= Duration::from_secs(interval)
                        && state.last_error.is_none()
                })
        };
        if !due(crate::maker::unix_now()) {
            return;
        }
        let _guard = self.refresh_lock.lock().await;
        if !due(crate::maker::unix_now()) {
            return;
        }
        let result = self.fetch(url).await;
        self.record(result, crate::maker::unix_now());
    }

    fn record(&self, result: Result<MarketPrice, String>, now: u64) {
        let mut state = self.state.write().unwrap();
        state.last_attempt_at = Some(now);
        match result {
            Ok(price) => {
                state.price = Some(price);
                state.last_error = None;
            }
            Err(error) => {
                // Provider response bodies and request objects can contain sensitive data.
                tracing::warn!("market price refresh failed: {error}");
                state.last_error = Some(error);
            }
        }
    }

    async fn fetch(&self, url: &str) -> Result<MarketPrice, String> {
        let client = self.client.as_ref().expect("market client configured");
        let mut response = client
            .get(url)
            .query(&[("id", "1437,3408"), ("convert", "USD")])
            .send()
            .await
            .map_err(|_| "CMC request failed or timed out".to_string())?;
        if !response.status().is_success() {
            return Err(format!("CMC HTTP {}", response.status().as_u16()));
        }
        let mut body = Vec::new();
        while let Some(chunk) = response
            .chunk()
            .await
            .map_err(|_| "CMC response body unavailable".to_string())?
        {
            if body.len().saturating_add(chunk.len()) > 256 * 1024 {
                return Err("CMC response too large".into());
            }
            body.extend_from_slice(&chunk);
        }
        parse_price(
            &body,
            crate::maker::unix_now(),
            self.policy.market.as_ref().unwrap(),
        )
    }
}

impl QuotePricing {
    // Recheck the captured price after RPCs or a wait for the wallet, before issuing a quote.
    pub fn fresh(&self, now: u64) -> bool {
        match (&self.observed, &self.policy.market) {
            (Some(price), Some(config)) => price.fresh(config, now),
            (None, None) => true,
            _ => false,
        }
    }
}

impl MarketPrice {
    fn fresh(&self, config: &MarketConfig, now: u64) -> bool {
        [self.zec_updated_at, self.usdc_updated_at, self.fetched_at]
            .iter()
            .all(|at| {
                *at <= now.saturating_add(30) && now.saturating_sub(*at) <= config.max_age_seconds
            })
            && self.fetched.elapsed() <= Duration::from_secs(config.max_age_seconds)
    }
}

#[derive(Deserialize)]
struct CmcResponse {
    status: CmcStatus,
    data: Vec<CmcAsset>,
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
    id: u64,
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

fn parse_price(body: &[u8], now: u64, config: &MarketConfig) -> Result<MarketPrice, String> {
    let data: CmcResponse =
        serde_json::from_slice(body).map_err(|_| "CMC response invalid".to_string())?;
    if data.status.error_code != 0 {
        return Err(format!("CMC error {}", data.status.error_code));
    }
    let asset = |id, symbol: &str| -> Result<(Decimal, u64), String> {
        let assets: Vec<_> = data
            .data
            .iter()
            .filter(|a| a.id == id && a.symbol == symbol)
            .collect();
        if assets.len() != 1 {
            return Err(format!("CMC {symbol} asset missing or duplicated"));
        }
        let quotes: Vec<_> = assets[0]
            .quote
            .iter()
            .filter(|q| q.id == USD_ID && q.symbol == "USD")
            .collect();
        if quotes.len() != 1 {
            return Err(format!("CMC {symbol}/USD quote missing or duplicated"));
        }
        let quote = quotes[0];
        let raw = quote.price.to_string();
        let price = Decimal::from_str(&raw)
            .or_else(|_| Decimal::from_scientific(&raw))
            .map_err(|_| format!("CMC {symbol}/USD price invalid"))?;
        if price <= Decimal::ZERO {
            return Err(format!("CMC {symbol}/USD price must be positive"));
        }
        let at = chrono::DateTime::parse_from_rfc3339(&quote.last_updated)
            .ok()
            .and_then(|value| u64::try_from(value.timestamp()).ok())
            .ok_or_else(|| format!("CMC {symbol} timestamp invalid"))?;
        if at > now.saturating_add(30) || now.saturating_sub(at) > config.max_age_seconds {
            return Err(format!("CMC {symbol}/USD quote is stale or ahead of clock"));
        }
        Ok((price, at))
    };
    let (zec, zec_updated_at) = asset(ZEC_ID, "ZEC")?;
    let (usdc, usdc_updated_at) = asset(USDC_ID, "USDC")?;
    let scale = Decimal::from(10u64.pow(config.token_decimals.into()));
    let price_per_zec = zec
        .checked_div(usdc)
        .and_then(|v| v.checked_mul(scale))
        .and_then(|v| v.trunc().to_u128())
        .filter(|v| *v > 0)
        .ok_or_else(|| "CMC ZEC/USDC rate is outside the supported range".to_string())?;
    Ok(MarketPrice {
        price_per_zec,
        zec_usd: zec.normalize().to_string(),
        usdc_usd: usdc.normalize().to_string(),
        zec_updated_at,
        usdc_updated_at,
        fetched_at: now,
        fetched: Instant::now(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

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
            }),
        }
    }
    fn sample(zec: &str, usdc: &str, at: &str) -> Vec<u8> {
        format!(r#"{{"status":{{"error_code":"0"}},"data":[{{"id":1437,"symbol":"ZEC","quote":[{{"id":2781,"symbol":"USD","price":{zec},"last_updated":"{at}"}}]}},{{"id":3408,"symbol":"USDC","quote":[{{"id":2781,"symbol":"USD","price":{usdc},"last_updated":"{at}"}}]}}]}}"#).into_bytes()
    }
    const NOW: u64 = 1_767_225_600; // 2026-01-01
    const AT: &str = "2026-01-01T00:00:00.000Z";

    #[test]
    fn prices_both_directions_with_exact_decimals_and_actual_usdc_usd() {
        let p = parse_price(
            &sample("1334.6563794177857", "0.9999", AT),
            NOW,
            policy().market.as_ref().unwrap(),
        )
        .unwrap();
        assert_eq!(p.zec_usd, "1334.6563794177857");
        assert_eq!(p.price_per_zec, 1_334_789_858);
        let mut pricing = policy();
        pricing.price_per_zec = p.price_per_zec;
        let forward = pricing.terms(20).unwrap();
        let reverse = pricing.reverse_terms(20).unwrap();
        assert_eq!(forward.amount, 20_000_000);
        assert_eq!(forward.deposit_zat, 1_513_499);
        assert_eq!(reverse.deposit_zat, 1_483_379);
        assert!(forward.deposit_zat > reverse.deposit_zat);
    }

    #[test]
    fn supports_live_v3_text_status_codes_and_documented_numeric_status_codes() {
        let mut body: serde_json::Value = serde_json::from_slice(&sample("40", "1", AT)).unwrap();
        for code in [json!("0"), json!(0)] {
            body["status"]["error_code"] = code;
            assert!(
                parse_price(
                    &serde_json::to_vec(&body).unwrap(),
                    NOW,
                    policy().market.as_ref().unwrap()
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
                parse_price(
                    &serde_json::to_vec(&body).unwrap(),
                    NOW,
                    policy().market.as_ref().unwrap()
                )
                .is_err()
            );
        }
    }

    #[test]
    fn stale_or_invalid_provider_values_are_rejected() {
        let config = policy().market.unwrap();
        for (zec, usdc, at, now) in [
            ("0", "1", AT, NOW),
            ("-1", "1", AT, NOW),
            ("40", "0", AT, NOW),
            ("40", "1", AT, NOW + 301),
            ("40", "1", AT, NOW - 31),
            ("40", "1", "bad date", NOW),
            ("1e100", "1", AT, NOW),
        ] {
            assert!(parse_price(&sample(zec, usdc, at), now, &config).is_err());
        }
        let body = sample("40", "1", AT);
        let mut data: serde_json::Value = serde_json::from_slice(&body).unwrap();
        data["data"][0]["id"] = json!(1);
        assert!(parse_price(&serde_json::to_vec(&data).unwrap(), NOW, &config).is_err());
        assert!(parse_price(b"invalid json", NOW, &config).is_err());
        let scientific = parse_price(&sample("4e1", "1e0", AT), NOW, &config).unwrap();
        assert_eq!(scientific.price_per_zec, 40_000_000);
    }

    #[test]
    fn never_falls_back_to_fixed_price_and_honors_captured_rate_until_expiry() {
        let book = PriceBook::new(&policy(), Some("test-key")).unwrap();
        assert!(book.quote(NOW).is_none());
        assert_eq!(book.snapshot(NOW).status, "unavailable");
        let price = parse_price(
            &sample("40", "1", AT),
            NOW,
            policy().market.as_ref().unwrap(),
        )
        .unwrap();
        book.record(Ok(price), NOW);
        let locked = book.quote(NOW).unwrap();
        assert_eq!(locked.policy.price_per_zec, 40_000_000);
        let repriced = parse_price(
            &sample("80", "1", AT),
            NOW,
            policy().market.as_ref().unwrap(),
        )
        .unwrap();
        book.record(Ok(repriced), NOW);
        assert_eq!(book.quote(NOW).unwrap().policy.price_per_zec, 80_000_000);
        assert_eq!(locked.policy.price_per_zec, 40_000_000);
        book.record(Err("CMC HTTP 429".into()), NOW + 60);
        assert_eq!(
            book.quote(NOW + 60).unwrap().policy.price_per_zec,
            80_000_000
        );
        assert!(book.quote(NOW + 301).is_none());
        assert!(!locked.fresh(NOW + 301));
        assert_eq!(book.snapshot(NOW + 301).status, "stale");
        assert_eq!(
            book.snapshot(NOW + 301).last_error.as_deref(),
            Some("CMC HTTP 429")
        );
    }

    #[tokio::test]
    async fn concurrent_requests_share_one_fetch_and_provider_errors_never_expose_the_key() {
        use axum::{
            Router,
            http::{HeaderMap, StatusCode},
            routing::get,
        };
        use std::sync::{
            Arc,
            atomic::{AtomicUsize, Ordering},
        };
        let count = Arc::new(AtomicUsize::new(0));
        let hits = count.clone();
        let server = Router::new()
            .route(
                "/quotes",
                get(move |headers: HeaderMap| {
                    let hits = hits.clone();
                    async move {
                        assert_eq!(headers["X-CMC_PRO_API_KEY"], "private-test-key");
                        hits.fetch_add(1, Ordering::SeqCst);
                        tokio::time::sleep(Duration::from_millis(30)).await;
                        let at =
                            chrono::DateTime::from_timestamp(crate::maker::unix_now() as i64, 0)
                                .unwrap()
                                .to_rfc3339();
                        (StatusCode::OK, sample("40", "0.99", &at))
                    }
                }),
            )
            .route(
                "/error",
                get(|| async { (StatusCode::TOO_MANY_REQUESTS, "private-test-key") }),
            );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let task = tokio::spawn(async move {
            axum::serve(listener, server).await.unwrap();
        });
        let book = PriceBook::new(&policy(), Some("private-test-key")).unwrap();
        let url = format!("http://{addr}/quotes");
        tokio::join!(
            book.refresh_from(&url),
            book.refresh_from(&url),
            book.refresh_from(&url)
        );
        book.refresh_from(&url).await;
        assert_eq!(count.load(Ordering::SeqCst), 1);
        assert!(book.quote(crate::maker::unix_now()).is_some());
        book.state.write().unwrap().last_attempt_at = None;
        book.refresh_from(&format!("http://{addr}/error")).await;
        let snapshot = book.snapshot(crate::maker::unix_now());
        assert_eq!(snapshot.last_error.as_deref(), Some("CMC HTTP 429"));
        assert!(
            !serde_json::to_string(&snapshot)
                .unwrap()
                .contains("private-test-key")
        );
        task.abort();
    }

    #[test]
    fn validates_market_configuration_and_does_not_require_a_key_for_fixed_mode() {
        assert!(PriceBook::new(&policy(), None).is_err());
        let mut p = policy();
        p.market.as_mut().unwrap().token_decimals = 18;
        assert!(PriceBook::new(&p, Some("test-key")).is_err());
        p = policy();
        p.market.as_mut().unwrap().refresh_seconds = 0;
        assert!(PriceBook::new(&p, Some("test-key")).is_err());
        p = policy();
        p.market = None;
        let fixed = PriceBook::new(&p, None).unwrap();
        assert_eq!(fixed.quote(NOW).unwrap().policy.price_per_zec, 500_000_000);
        assert_eq!(fixed.snapshot(NOW).status, "fixed");
    }
}
