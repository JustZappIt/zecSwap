use std::time::Duration;

use alloy_primitives::{Address, B256};
use anyhow::{Result, bail};
use serde::Serialize;
use serde::de::DeserializeOwned;
use zecswap_api::{Acceptance, Accepted, Quote, QuoteRequest};

/// Accepting waits for the maker's `open` to land on Base.
const TIMEOUT: Duration = Duration::from_secs(180);

/// A maker's quote API. Nothing it says is trusted until the contract agrees.
#[derive(Clone)]
pub struct MakerApi {
    http: reqwest::Client,
    url: String,
}

impl MakerApi {
    pub fn new(url: impl Into<String>) -> Result<Self> {
        Ok(Self {
            http: reqwest::Client::builder().timeout(TIMEOUT).build()?,
            url: url.into(),
        })
    }

    pub(crate) async fn quote(&self, units: u32, payout: Address) -> Result<Quote> {
        self.post("/v1/quote", &QuoteRequest { units, payout })
            .await
    }

    pub(crate) async fn accept(&self, quote_id: B256, acceptance: &Acceptance) -> Result<Accepted> {
        self.post(&format!("/v1/quote/{quote_id}/accept"), acceptance)
            .await
    }

    async fn post<T: DeserializeOwned>(&self, path: &str, body: &impl Serialize) -> Result<T> {
        let response = self
            .http
            .post(format!("{}{path}", self.url))
            .json(body)
            .send()
            .await?;
        let status = response.status();
        if status.is_success() {
            Ok(response.json().await?)
        } else {
            bail!("maker answered {status}: {}", response.text().await?)
        }
    }
}
