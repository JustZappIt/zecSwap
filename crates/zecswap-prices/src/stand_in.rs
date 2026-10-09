//! A local stand-in for both providers, for tests. CoinMarketCap answers with the status it is
//! started with, and when that is OK with ZEC at $40, USDC at $0.99 and ETH at $2,400; Alchemy
//! with ZEC at $50, USDC at $1 and ETH at $2,500, and a history a dollar higher each candle from
//! $2,000.50. Each counts the requests it serves, and checks the key it is sent.

use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use axum::Router;
use axum::http::{HeaderMap, StatusCode};
use axum::routing::{get, post};
use serde_json::json;
use zeroize::Zeroizing;

use crate::{CANDLE, Keys};

pub const CMC_KEY: &str = "cmc-key";
pub const ALCHEMY_KEY: &str = "alchemy-key";

pub fn keys() -> Keys {
    Keys {
        cmc: Some(Zeroizing::new(CMC_KEY.into())),
        alchemy: Some(Zeroizing::new(ALCHEMY_KEY.into())),
    }
}

pub struct StandIn {
    /// Serve a feed from here with `Feed::served_by`.
    pub url: String,
    pub cmc: Arc<AtomicUsize>,
    pub alchemy: Arc<AtomicUsize>,
    pub history: Arc<AtomicUsize>,
    task: tokio::task::JoinHandle<()>,
}

impl Drop for StandIn {
    fn drop(&mut self) {
        self.task.abort();
    }
}

impl StandIn {
    pub fn hits(counter: &AtomicUsize) -> usize {
        counter.load(Ordering::SeqCst)
    }
}

fn stamp() -> String {
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_secs();
    chrono::DateTime::from_timestamp(now as i64, 0)
        .unwrap()
        .to_rfc3339()
}

/// CoinMarketCap's answer pricing ZEC and USDC, stamped `at`.
pub fn cmc_body(zec: &str, usdc: &str, at: &str) -> Vec<u8> {
    format!(r#"{{"status":{{"error_code":"0"}},"data":[{{"id":1437,"symbol":"ZEC","quote":[{{"id":2781,"symbol":"USD","price":{zec},"last_updated":"{at}"}}]}},{{"id":3408,"symbol":"USDC","quote":[{{"id":2781,"symbol":"USD","price":{usdc},"last_updated":"{at}"}}]}}]}}"#).into_bytes()
}

/// Alchemy's by-symbol answer listing `entries`.
pub fn alchemy_body(entries: serde_json::Value) -> Vec<u8> {
    serde_json::to_vec(&json!({ "data": entries })).unwrap()
}

/// One symbol as Alchemy lists it, priced in USD and stamped `at`.
pub fn listed(symbol: &str, value: &str, at: &str) -> serde_json::Value {
    json!({"symbol": symbol, "prices": [{"currency": "usd", "value": value, "lastUpdatedAt": at}]})
}

pub async fn start(cmc: StatusCode) -> StandIn {
    let counters = [(); 3].map(|_| Arc::new(AtomicUsize::new(0)));
    let [cmc_hits, alchemy_hits, history_hits] = counters.clone();
    let server = Router::new()
        .route(
            "/coinmarketcap",
            get(move |headers: HeaderMap| async move {
                assert_eq!(headers["X-CMC_PRO_API_KEY"], CMC_KEY);
                cmc_hits.fetch_add(1, Ordering::SeqCst);
                tokio::time::sleep(std::time::Duration::from_millis(30)).await;
                if cmc != StatusCode::OK {
                    return (cmc, CMC_KEY.as_bytes().to_vec());
                }
                let at = stamp();
                let mut body: serde_json::Value =
                    serde_json::from_slice(&cmc_body("40", "0.99", &at)).unwrap();
                body["data"].as_array_mut().unwrap().push(json!({
                    "id": 1027, "symbol": "ETH",
                    "quote": [{"id": 2781, "symbol": "USD", "price": 2400, "last_updated": at}],
                }));
                (cmc, serde_json::to_vec(&body).unwrap())
            }),
        )
        .route(
            "/alchemy/by-symbol",
            get(move |headers: HeaderMap| async move {
                assert_eq!(headers["authorization"], format!("Bearer {ALCHEMY_KEY}"));
                alchemy_hits.fetch_add(1, Ordering::SeqCst);
                let at = stamp();
                alchemy_body(json!([
                    listed("ZEC", "50", &at),
                    listed("USDC", "1", &at),
                    listed("ETH", "2500", &at)
                ]))
            }),
        )
        .route(
            "/alchemy/historical",
            post(move |body: String| async move {
                history_hits.fetch_add(1, Ordering::SeqCst);
                let request: serde_json::Value = serde_json::from_str(&body).unwrap();
                assert_eq!(request["interval"], "5m");
                let (start, end) = (
                    request["startTime"].as_u64().unwrap(),
                    request["endTime"].as_u64().unwrap(),
                );
                let candles: Vec<_> = (start.div_ceil(CANDLE)..=end / CANDLE)
                    .map(|candle| {
                        json!({
                            "value": format!("{}.5", 2000 + candle % 1000),
                            "timestamp": chrono::DateTime::from_timestamp((candle * CANDLE) as i64, 0).unwrap().to_rfc3339(),
                        })
                    })
                    .collect();
                json!({"symbol": request["symbol"], "currency": "usd", "data": candles}).to_string()
            }),
        );
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://{}", listener.local_addr().unwrap());
    let task = tokio::spawn(async move {
        axum::serve(listener, server).await.unwrap();
    });
    let [cmc, alchemy, history] = counters;
    StandIn {
        url,
        cmc,
        alchemy,
        history,
        task,
    }
}
