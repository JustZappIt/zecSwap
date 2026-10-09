//! What an asset was worth at a past time, by Alchemy's five-minute candles.

use std::collections::HashMap;
use std::str::FromStr;
use std::sync::Mutex;
use std::time::Duration;

use anyhow::{Context, Result};
use reqwest::header::{AUTHORIZATION, CONTENT_TYPE, HeaderMap, HeaderValue};
use rust_decimal::Decimal;
use serde::Deserialize;
use zeroize::Zeroizing;

use crate::{ALCHEMY_PRICES, Asset, read};

/// Alchemy's finest history: one price every five minutes.
pub const CANDLE: u64 = 300;
const CACHED: usize = 4096;

/// Alchemy's price history, each candle asked for once.
pub struct History {
    client: reqwest::Client,
    url: String,
    candles: Mutex<HashMap<(Asset, u64), String>>,
}

impl History {
    /// `key` is an Alchemy app's API key, sent as a bearer header, never in a URL.
    pub fn new(key: &str) -> Result<Self> {
        let mut value = HeaderValue::from_str(&Zeroizing::new(format!("Bearer {}", key.trim())))
            .context("invalid ALCHEMY_API_KEY header")?;
        value.set_sensitive(true);
        let mut headers = HeaderMap::new();
        headers.insert(AUTHORIZATION, value);
        Ok(Self {
            client: reqwest::Client::builder()
                .default_headers(headers)
                .timeout(Duration::from_secs(8))
                .redirect(reqwest::redirect::Policy::none())
                .build()?,
            url: ALCHEMY_PRICES.to_owned(),
            candles: Mutex::default(),
        })
    }

    /// Asks `url` in Alchemy's place: a stand-in, in tests.
    pub fn served_by(mut self, url: &str) -> Self {
        self.url = url.to_owned();
        self
    }

    /// What `asset` was worth in USD at `at`, by the five-minute candle nearest it: an error
    /// rather than another time's price when none is within five minutes.
    pub async fn usd_at(&self, asset: Asset, at: u64) -> Result<String, String> {
        // Keyed by the candle nearest `at`, which is the one it takes.
        let key = (asset, (at + CANDLE / 2) / CANDLE);
        if let Some(price) = self.candles.lock().unwrap().get(&key) {
            return Ok(price.clone());
        }
        let body = serde_json::json!({
            "symbol": asset.symbol(),
            "startTime": at.saturating_sub(CANDLE),
            "endTime": at.saturating_add(CANDLE),
            "interval": "5m",
        });
        let request = self
            .client
            .post(format!("{}/historical", self.url))
            .header(CONTENT_TYPE, "application/json")
            .body(body.to_string());
        let price = nearest(&read(request, "Alchemy").await?, asset, at)?;
        let mut candles = self.candles.lock().unwrap();
        if candles.len() >= CACHED {
            candles.clear();
        }
        candles.insert(key, price.clone());
        Ok(price)
    }
}

#[derive(Deserialize)]
struct Candles {
    symbol: String,
    currency: String,
    data: Vec<Candle>,
}
#[derive(Deserialize)]
struct Candle {
    value: String,
    timestamp: String,
}

fn nearest(body: &[u8], asset: Asset, at: u64) -> Result<String, String> {
    let candles: Candles =
        serde_json::from_slice(body).map_err(|_| "Alchemy history invalid".to_string())?;
    let symbol = asset.symbol();
    if !candles.symbol.eq_ignore_ascii_case(symbol) || !candles.currency.eq_ignore_ascii_case("usd")
    {
        return Err(format!("Alchemy history is not {symbol}/USD"));
    }
    let mut nearest: Option<(u64, Decimal)> = None;
    for candle in candles.data {
        let price = Decimal::from_str(&candle.value)
            .map_err(|_| format!("Alchemy {symbol} history price invalid"))?;
        let time = chrono::DateTime::parse_from_rfc3339(&candle.timestamp)
            .ok()
            .and_then(|value| u64::try_from(value.timestamp()).ok())
            .ok_or_else(|| format!("Alchemy {symbol} history timestamp invalid"))?;
        let distance = time.abs_diff(at);
        if price > Decimal::ZERO
            && distance <= CANDLE
            && nearest.is_none_or(|(closest, _)| distance < closest)
        {
            nearest = Some((distance, price));
        }
    }
    nearest
        .map(|(_, price)| price.normalize().to_string())
        .ok_or_else(|| format!("Alchemy has no {symbol} price within five minutes"))
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;
    use std::sync::atomic::{AtomicUsize, Ordering};

    use axum::{Router, http::HeaderMap, routing::post};
    use serde_json::json;

    use super::*;

    /// A stand-in for Alchemy's history: a price a dollar higher each candle, asked for counted.
    async fn alchemy() -> (String, Arc<AtomicUsize>, tokio::task::JoinHandle<()>) {
        let asked = Arc::new(AtomicUsize::new(0));
        let counter = asked.clone();
        let server = Router::new().route(
            "/historical",
            post(move |headers: HeaderMap, body: String| async move {
                assert_eq!(headers["authorization"], "Bearer key");
                counter.fetch_add(1, Ordering::SeqCst);
                let request: serde_json::Value = serde_json::from_str(&body).unwrap();
                let (start, end) = (request["startTime"].as_u64().unwrap(), request["endTime"].as_u64().unwrap());
                let data: Vec<_> = (start.div_ceil(CANDLE)..=end / CANDLE)
                    .map(|candle| json!({
                        "value": format!("{}.5", 2000 + candle % 1000),
                        "timestamp": chrono::DateTime::from_timestamp((candle * CANDLE) as i64, 0).unwrap().to_rfc3339(),
                    }))
                    .collect();
                json!({"symbol": request["symbol"], "currency": "usd", "data": data}).to_string()
            }),
        );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("http://{}", listener.local_addr().unwrap());
        (
            url,
            asked,
            tokio::spawn(async move { axum::serve(listener, server).await.unwrap() }),
        )
    }

    /// The nearest candle prices a time, each candle is asked for once, and a time with no
    /// candle within five minutes has no price rather than a neighbour's.
    #[tokio::test]
    async fn a_time_takes_its_nearest_candle_and_none_further_than_five_minutes() {
        let (url, asked, server) = alchemy().await;
        let history = History::new("key").unwrap().served_by(&url);
        // Twenty seconds into candle 6,000,000, then 160 seconds in: nearer the next.
        let at = 6_000_000 * CANDLE + 20;
        assert_eq!(
            history.usd_at(Asset::Eth, at).await.as_deref(),
            Ok("2000.5")
        );
        assert_eq!(
            history.usd_at(Asset::Eth, at + 1).await.as_deref(),
            Ok("2000.5")
        );
        assert_eq!(asked.load(Ordering::SeqCst), 1);
        assert_eq!(
            history.usd_at(Asset::Eth, at + 140).await.as_deref(),
            Ok("2001.5")
        );
        let far = json!({"symbol": "ETH", "currency": "usd", "data": [
            {"value": "2400", "timestamp": "2026-01-01T00:10:00Z"},
        ]});
        assert_eq!(
            nearest(far.to_string().as_bytes(), Asset::Eth, 1_767_225_600)
                .err()
                .as_deref(),
            Some("Alchemy has no ETH price within five minutes")
        );
        let other = json!({"symbol": "ZEC", "currency": "usd", "data": []});
        assert!(nearest(other.to_string().as_bytes(), Asset::Eth, 0).is_err());
        server.abort();
    }
}
