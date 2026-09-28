use std::time::Duration;

use alloy_primitives::{Address, B256};
use anyhow::{Result, bail};
use serde::Serialize;
use serde::de::DeserializeOwned;
use zecswap_api::relayer::{Claim, LockClaim, Payout, Sent, Terms};
use zecswap_api::{Acceptance, Accepted, Quote, QuoteRequest};

/// Accepting waits for the maker's `open` to land, and a relayed claim for two transactions.
const TIMEOUT: Duration = Duration::from_secs(180);

/// A maker's quote API. Nothing it says is trusted until the contract agrees.
#[derive(Clone)]
pub struct MakerApi(Endpoint);

impl MakerApi {
    pub fn new(url: impl Into<String>) -> Result<Self> {
        Endpoint::new("maker", url).map(Self)
    }

    pub async fn info(&self) -> Result<zecswap_api::service::MakerInfo> {
        self.0.get("/v1/info").await
    }

    pub async fn reverse_quote(
        &self,
        request: &zecswap_api::reverse::QuoteRequest,
    ) -> Result<zecswap_api::reverse::Quote> {
        self.0.post("/v1/reverse/quote", request).await
    }

    pub async fn accept_reverse(
        &self,
        quote_id: B256,
        acceptance: &Acceptance,
    ) -> Result<Accepted> {
        self.0
            .post(&format!("/v1/reverse/quote/{quote_id}/accept"), acceptance)
            .await
    }

    pub async fn reverse_status(&self, swap_id: B256) -> Result<zecswap_api::reverse::Status> {
        self.0.get(&format!("/v1/reverse/swaps/{swap_id}")).await
    }

    pub(crate) async fn quote(
        &self,
        units: u32,
        payout: Address,
        payout_note: Option<B256>,
    ) -> Result<Quote> {
        let request = QuoteRequest {
            units,
            payout,
            payout_note,
        };
        self.0.post("/v1/quote", &request).await
    }

    pub(crate) async fn accept(&self, quote_id: B256, acceptance: &Acceptance) -> Result<Accepted> {
        self.0
            .post(&format!("/v1/quote/{quote_id}/accept"), acceptance)
            .await
    }
}

/// A relayer's API. It can delay a swap but not redirect it: all it sends is signed by the
/// swap's own key or checked by the contract.
#[derive(Clone)]
pub struct RelayerApi(Endpoint);

impl RelayerApi {
    pub fn new(url: impl Into<String>) -> Result<Self> {
        Endpoint::new("relayer", url).map(Self)
    }

    pub async fn ready_reverse(
        &self,
        request: &zecswap_api::reverse::Authorization,
    ) -> Result<Sent> {
        self.0.post("/v1/reverse/ready", request).await
    }

    pub async fn lock_reverse_refund(
        &self,
        request: &zecswap_api::reverse::Authorization,
    ) -> Result<Sent> {
        self.0.post("/v1/reverse/lock-refund", request).await
    }

    pub async fn refund_reverse(&self, request: &zecswap_api::reverse::Refund) -> Result<Sent> {
        self.0.post("/v1/reverse/refund", request).await
    }

    pub async fn reverse_refund_payout(&self, request: &Payout) -> Result<Sent> {
        self.0.post("/v1/reverse/refund-payout", request).await
    }

    pub(crate) async fn terms(&self) -> Result<Terms> {
        self.0.get("/v1/terms").await
    }

    pub(crate) async fn lock_claim(&self, request: &LockClaim) -> Result<Sent> {
        self.0.post("/v1/lock-claim", request).await
    }

    pub(crate) async fn claim(&self, request: &Claim) -> Result<Sent> {
        self.0.post("/v1/claim", request).await
    }

    pub(crate) async fn payout(&self, request: &Payout) -> Result<Sent> {
        self.0.post("/v1/payout", request).await
    }
}

#[derive(Clone)]
struct Endpoint {
    name: &'static str,
    http: reqwest::Client,
    url: String,
}

impl Endpoint {
    fn new(name: &'static str, url: impl Into<String>) -> Result<Self> {
        Ok(Self {
            name,
            http: reqwest::Client::builder().timeout(TIMEOUT).build()?,
            url: url.into(),
        })
    }

    async fn get<T: DeserializeOwned>(&self, path: &str) -> Result<T> {
        self.read(self.http.get(format!("{}{path}", self.url)))
            .await
    }

    async fn post<T: DeserializeOwned>(&self, path: &str, body: &impl Serialize) -> Result<T> {
        self.read(self.http.post(format!("{}{path}", self.url)).json(body))
            .await
    }

    async fn read<T: DeserializeOwned>(&self, request: reqwest::RequestBuilder) -> Result<T> {
        let response = request.send().await?;
        let status = response.status();
        if status.is_success() {
            Ok(response.json().await?)
        } else {
            bail!(
                "{} answered {status}: {}",
                self.name,
                response.text().await?
            )
        }
    }
}
