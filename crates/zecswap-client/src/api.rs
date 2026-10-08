use std::sync::Arc;
use std::time::Duration;

use alloy_primitives::{Address, B256};
use anyhow::{Context as _, Result, bail};
use reqwest::StatusCode;
use reqwest::header::{AUTHORIZATION, WWW_AUTHENTICATE};
use serde::Serialize;
use serde::de::DeserializeOwned;
use zecswap_api::relayer::{AlreadySpent, Claim, LockClaim, Payout, RailgunTransact, Sent, Terms};
use zecswap_api::service::{ErrorCode, ErrorResponse};
use zecswap_api::{Acceptance, Accepted, Quote, QuoteRequest, Status};

use crate::tokens::Tokens;

/// Accepting waits for the maker's `open` to land, and a relayed claim for two transactions.
const TIMEOUT: Duration = Duration::from_secs(180);

/// A maker's quote API. Nothing it says is trusted until the contract agrees.
#[derive(Clone)]
pub struct MakerApi(Endpoint);

impl MakerApi {
    pub fn new(url: impl Into<String>) -> Result<Self> {
        Endpoint::new("maker", url).map(Self)
    }

    /// Pays for each accept with a token from `tokens` when the maker asks for one.
    pub fn with_tokens(mut self, tokens: Arc<Tokens>) -> Self {
        self.0.tokens = Some(tokens);
        self
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

    /// Accepts the reverse quote that opens `swap_id`.
    pub async fn accept_reverse(
        &self,
        quote_id: B256,
        swap_id: B256,
        acceptance: &Acceptance,
    ) -> Result<Accepted> {
        let path = format!("/v1/reverse/quote/{quote_id}/accept");
        self.0.accept(&path, swap_id, acceptance).await
    }

    pub async fn reverse_status(&self, swap_id: B256) -> Result<zecswap_api::reverse::Status> {
        self.0.get(&format!("/v1/reverse/swaps/{swap_id}")).await
    }

    /// Holds the token a forward swap hands back once paid into: whether it now holds it.
    pub async fn collect_token(&self, swap_id: B256) -> Result<bool> {
        let status: Status = self.0.get(&format!("/v1/swaps/{swap_id}")).await?;
        self.0
            .tokens()?
            .collect(swap_id, status.token_return.as_deref())
    }

    /// As `collect_token`, for a reverse swap.
    pub async fn collect_reverse_token(&self, swap_id: B256) -> Result<bool> {
        let status = self.reverse_status(swap_id).await?;
        self.0
            .tokens()?
            .collect(swap_id, status.token_return.as_deref())
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

    /// Accepts the quote that opens `swap_id`.
    pub(crate) async fn accept(
        &self,
        quote_id: B256,
        swap_id: B256,
        acceptance: &Acceptance,
    ) -> Result<Accepted> {
        let path = format!("/v1/quote/{quote_id}/accept");
        self.0.accept(&path, swap_id, acceptance).await
    }
}

/// A relayer's API. It can delay a swap but not redirect it: all it sends is signed by the
/// swap's own key or checked by the contract.
#[derive(Clone)]
pub struct RelayerApi(Endpoint);

/// What a relayer made of a Railgun transaction posted to it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Broadcast {
    /// Submitted, in these transactions; the same bytes posted again name the same ones.
    Sent(Vec<B256>),
    /// Refused: nothing from this proof was sent or will be. Drop it and free its notes.
    Refused(String),
    /// A note it spends is spent, or a transaction already sent spends it: settle from the chain.
    /// Names the relayer's own transactions that spend them, when it knows them.
    Spent(Vec<B256>),
    /// Not known yet, or never answered: post the same bytes again later.
    Retry(String),
}

impl RelayerApi {
    pub fn new(url: impl Into<String>) -> Result<Self> {
        Endpoint::new("relayer", url).map(Self)
    }

    /// Submit a persisted, locally proved funding transaction. Returned hashes are pending;
    /// reconcile the escrow independently, including after any transport failure.
    pub async fn fund_reverse(&self, request: &zecswap_api::reverse::Funding) -> Result<Sent> {
        self.0.post("/v1/reverse/fund", request).await
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

    pub async fn terms(&self) -> Result<Terms> {
        self.0.get("/v1/terms").await
    }

    /// Posts a wallet's own proved Railgun transaction for the relayer to send as its
    /// broadcaster. Persist `request` before posting it, and post the same bytes again until the
    /// answer is not `Retry`.
    pub async fn railgun_transact(&self, request: &RailgunTransact) -> Broadcast {
        let url = format!("{}/v1/railgun/transact", self.0.url);
        let response = match self.0.http.post(url).json(request).send().await {
            Ok(response) => response,
            Err(e) => return Broadcast::Retry(e.to_string()),
        };
        let status = response.status();
        let body = response.text().await.unwrap_or_default();
        if status.is_success() {
            return match serde_json::from_str::<Sent>(&body) {
                Ok(sent) => Broadcast::Sent(sent.transactions),
                Err(e) => Broadcast::Retry(format!("an unreadable answer: {e}")),
            };
        }
        let error = serde_json::from_str::<ErrorResponse>(&body).ok();
        match (status, error) {
            (StatusCode::CONFLICT, Some(e)) if e.code == ErrorCode::AlreadySpent => {
                Broadcast::Spent(
                    serde_json::from_str::<AlreadySpent>(&body)
                        .map(|spent| spent.transactions)
                        .unwrap_or_default(),
                )
            }
            (status, error) if status.is_client_error() && status != StatusCode::CONFLICT => {
                Broadcast::Refused(error.map_or(body, |e| e.error))
            }
            (status, error) => Broadcast::Retry(format!(
                "relayer answered {status}: {}",
                error.map_or(body, |e| e.error)
            )),
        }
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
    tokens: Option<Arc<Tokens>>,
}

impl Endpoint {
    fn new(name: &'static str, url: impl Into<String>) -> Result<Self> {
        Ok(Self {
            name,
            http: reqwest::Client::builder().timeout(TIMEOUT).build()?,
            url: url.into(),
            tokens: None,
        })
    }

    /// Posts an accept: asked for a token, it pays with one and asks for it back.
    async fn accept(&self, path: &str, swap_id: B256, acceptance: &Acceptance) -> Result<Accepted> {
        let url = format!("{}{path}", self.url);
        let mut response = self.http.post(&url).json(acceptance).send().await?;
        if let Some(tokens) = &self.tokens {
            // A token the maker refuses, spent after all or no longer good, gives way to the
            // next.
            for _ in 0..3 {
                if response.status() != StatusCode::UNAUTHORIZED {
                    break;
                }
                let Some(asked) = response.headers().get(WWW_AUTHENTICATE) else {
                    break;
                };
                let payment = tokens.pay(asked.to_str()?).await?;
                let paid = Acceptance {
                    token_request: Some(payment.request.clone()),
                    ..acceptance.clone()
                };
                let sent = self
                    .http
                    .post(&url)
                    .header(AUTHORIZATION, payment.token.authorization())
                    .json(&paid)
                    .send()
                    .await;
                let status = sent.as_ref().ok().map(reqwest::Response::status);
                tokens.settle(swap_id, payment, status);
                response = sent?;
            }
        }
        self.read(response).await
    }

    fn tokens(&self) -> Result<&Tokens> {
        self.tokens
            .as_deref()
            .context("this client spends no tokens")
    }

    async fn get<T: DeserializeOwned>(&self, path: &str) -> Result<T> {
        self.read(self.http.get(format!("{}{path}", self.url)).send().await?)
            .await
    }

    async fn post<T: DeserializeOwned>(&self, path: &str, body: &impl Serialize) -> Result<T> {
        self.read(
            self.http
                .post(format!("{}{path}", self.url))
                .json(body)
                .send()
                .await?,
        )
        .await
    }

    async fn read<T: DeserializeOwned>(&self, response: reqwest::Response) -> Result<T> {
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
