use std::sync::Arc;
use std::time::Duration;

use alloy_primitives::{Address, B256};
use anyhow::{Context as _, Result};
use reqwest::StatusCode;
use reqwest::header::{AUTHORIZATION, RETRY_AFTER, WWW_AUTHENTICATE};
use serde::Serialize;
use serde::de::DeserializeOwned;
use zecswap_api::relayer::{
    Claim, Conflict, LockClaim, Payout, RailgunTransact, RescueDeposit, Sent, ShieldDeposit, Terms,
};
use zecswap_api::service::{ErrorCode, ErrorResponse};
use zecswap_api::{Acceptance, Accepted, Quote, QuoteRequest, Status, TICKET_HEADER};

use crate::tokens::Tokens;

/// Accepting waits for the maker's `open` to land, and a relayed claim for two transactions.
const TIMEOUT: Duration = Duration::from_secs(180);
/// The most a request waits in all on `503`s asking it to come back: the gateway's, past an
/// allowance, before the request reaches the service.
const BUSY_PATIENCE: Duration = Duration::from_secs(90);
/// The longest `Retry-After` waited for: one asking longer is an outage, not a queue.
const BUSY_WAIT: Duration = Duration::from_secs(30);

/// A service's answer other than success.
#[derive(Debug)]
pub struct ServiceError {
    pub service: &'static str,
    pub status: StatusCode,
    pub body: String,
}

impl ServiceError {
    /// Whether the service turned the request down, sending nothing: a `5xx`, or no answer at
    /// all, may follow a send.
    pub fn refused(&self) -> bool {
        self.status.is_client_error()
    }
}

impl std::fmt::Display for ServiceError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "{} answered {}: {}",
            self.service, self.status, self.body
        )
    }
}

impl std::error::Error for ServiceError {}

/// Whether `error` is a service's refusal, after which nothing it was asked for was sent.
pub(crate) fn refused(error: &anyhow::Error) -> bool {
    error
        .downcast_ref::<ServiceError>()
        .is_some_and(ServiceError::refused)
}

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
/// swap's own key or checked by the contract. Each request for a swap carries the ticket its
/// accept answered with, where the wallet has one: the gateway gives the swap an allowance of
/// its own.
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

/// What a relayer made of a deposit posted to it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Deposited {
    /// Submitted, in these transactions; the same request posted again names the same ones.
    Sent(Vec<B256>),
    /// Refused: nothing was sent, and nothing will be. A fee or validity gone stale is signed
    /// again from the terms.
    Refused(String),
    /// The authorization is used, or another of the same address is on its way: follow the
    /// relayer's transactions named, or with none, the chain.
    Conflict(Vec<B256>),
    /// Not now, or not known yet: post the same request again later, no sooner than `after`
    /// where the relayer said.
    Retry {
        reason: String,
        after: Option<Duration>,
    },
}

impl RelayerApi {
    pub fn new(url: impl Into<String>) -> Result<Self> {
        Endpoint::new("relayer", url).map(Self)
    }

    /// Posts a deposit address's signed shield. Persist `request` before posting it, and post the
    /// same bytes again while the answer is `Retry`.
    pub async fn shield_deposit(&self, request: &ShieldDeposit) -> Deposited {
        self.deposit("/v1/deposits/shield", request).await
    }

    /// Posts a deposit address's signed rescue, as `shield_deposit`.
    pub async fn rescue_deposit(&self, request: &RescueDeposit) -> Deposited {
        self.deposit("/v1/deposits/rescue", request).await
    }

    /// A `503` asking a short wait is waited out and posted again as the gateway's are, for up to
    /// `BUSY_PATIENCE` in all; one asking longer comes back as `Retry`.
    async fn deposit(&self, path: &str, request: &impl Serialize) -> Deposited {
        let url = format!("{}{path}", self.0.url);
        let response = match self.0.send(|| self.0.http.post(&url).json(request)).await {
            Ok(response) => response,
            Err(e) => {
                return Deposited::Retry {
                    reason: e.to_string(),
                    after: None,
                };
            }
        };
        let (status, after) = (response.status(), retry_after(&response));
        let body = response.text().await.unwrap_or_default();
        if status.is_success() {
            return match serde_json::from_str::<Sent>(&body) {
                Ok(sent) => Deposited::Sent(sent.transactions),
                Err(e) => Deposited::Retry {
                    reason: format!("an unreadable answer: {e}"),
                    after: None,
                },
            };
        }
        if status == StatusCode::CONFLICT
            && let Ok(conflict) = serde_json::from_str::<Conflict>(&body)
        {
            return Deposited::Conflict(conflict.transactions);
        }
        let error = serde_json::from_str::<ErrorResponse>(&body).map_or(body, |e| e.error);
        if status.is_client_error() {
            return Deposited::Refused(error);
        }
        Deposited::Retry {
            reason: format!("relayer answered {status}: {error}"),
            after,
        }
    }

    /// Gives each request `timeout` to answer, rather than three minutes.
    pub fn with_timeout(self, timeout: Duration) -> Result<Self> {
        Ok(Self(Endpoint {
            http: reqwest::Client::builder().timeout(timeout).build()?,
            ..self.0
        }))
    }

    /// Submit a persisted, locally proved funding transaction. Returned hashes are pending;
    /// reconcile the escrow independently, including after any transport failure.
    pub async fn fund_reverse(
        &self,
        request: &zecswap_api::reverse::Funding,
        ticket: Option<&str>,
    ) -> Result<Sent> {
        self.0
            .post_for_swap("/v1/reverse/fund", request, ticket)
            .await
    }

    pub async fn ready_reverse(
        &self,
        request: &zecswap_api::reverse::Authorization,
        ticket: Option<&str>,
    ) -> Result<Sent> {
        self.0
            .post_for_swap("/v1/reverse/ready", request, ticket)
            .await
    }

    pub async fn lock_reverse_refund(
        &self,
        request: &zecswap_api::reverse::Authorization,
        ticket: Option<&str>,
    ) -> Result<Sent> {
        self.0
            .post_for_swap("/v1/reverse/lock-refund", request, ticket)
            .await
    }

    pub async fn refund_reverse(
        &self,
        request: &zecswap_api::reverse::Refund,
        ticket: Option<&str>,
    ) -> Result<Sent> {
        self.0
            .post_for_swap("/v1/reverse/refund", request, ticket)
            .await
    }

    pub async fn reverse_refund_payout(
        &self,
        request: &Payout,
        ticket: Option<&str>,
    ) -> Result<Sent> {
        self.0
            .post_for_swap("/v1/reverse/refund-payout", request, ticket)
            .await
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
                    serde_json::from_str::<Conflict>(&body)
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

    pub(crate) async fn lock_claim(
        &self,
        request: &LockClaim,
        ticket: Option<&str>,
    ) -> Result<Sent> {
        self.0
            .post_for_swap("/v1/lock-claim", request, ticket)
            .await
    }

    pub(crate) async fn claim(&self, request: &Claim, ticket: Option<&str>) -> Result<Sent> {
        self.0.post_for_swap("/v1/claim", request, ticket).await
    }

    pub(crate) async fn payout(&self, request: &Payout, ticket: Option<&str>) -> Result<Sent> {
        self.0.post_for_swap("/v1/payout", request, ticket).await
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
        let mut response = self.send(|| self.http.post(&url).json(acceptance)).await?;
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
                let authorization = payment.token.authorization();
                let sent = self
                    .send(|| {
                        self.http
                            .post(&url)
                            .header(AUTHORIZATION, &authorization)
                            .json(&paid)
                    })
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
        let url = format!("{}{path}", self.url);
        self.read(self.send(|| self.http.get(&url)).await?).await
    }

    async fn post<T: DeserializeOwned>(&self, path: &str, body: &impl Serialize) -> Result<T> {
        self.post_for_swap(path, body, None).await
    }

    /// As `post`, with the ticket of the swap the request is for.
    async fn post_for_swap<T: DeserializeOwned>(
        &self,
        path: &str,
        body: &impl Serialize,
        ticket: Option<&str>,
    ) -> Result<T> {
        let url = format!("{}{path}", self.url);
        let request = || {
            let request = self.http.post(&url).json(body);
            match ticket {
                Some(ticket) => request.header(TICKET_HEADER, ticket),
                None => request,
            }
        };
        self.read(self.send(request).await?).await
    }

    /// Sends what `request` builds, and again while the answer is a `503` asking to come back
    /// shortly: the gateway's, given before the request reaches the service, or the relayer's,
    /// whose requests send nothing twice.
    async fn send(
        &self,
        request: impl Fn() -> reqwest::RequestBuilder,
    ) -> reqwest::Result<reqwest::Response> {
        let mut waited = Duration::ZERO;
        loop {
            let response = request().send().await?;
            let wait = (response.status() == StatusCode::SERVICE_UNAVAILABLE)
                .then(|| retry_after(&response))
                .flatten()
                .filter(|wait| *wait <= BUSY_WAIT && waited + *wait <= BUSY_PATIENCE);
            let Some(wait) = wait else {
                return Ok(response);
            };
            waited += wait;
            tokio::time::sleep(wait).await;
        }
    }

    async fn read<T: DeserializeOwned>(&self, response: reqwest::Response) -> Result<T> {
        let status = response.status();
        if status.is_success() {
            return Ok(response.json().await?);
        }
        Err(ServiceError {
            service: self.name,
            status,
            body: response.text().await?,
        }
        .into())
    }
}

/// The wait a `Retry-After` of delay-seconds asks for; an HTTP date asks none this client keeps.
fn retry_after(response: &reqwest::Response) -> Option<Duration> {
    let seconds = response.headers().get(RETRY_AFTER)?.to_str().ok()?;
    seconds.trim().parse().ok().map(Duration::from_secs)
}

#[cfg(test)]
mod tests {
    use std::collections::VecDeque;
    use std::sync::Mutex;

    use axum::Router;
    use axum::http::HeaderMap;
    use axum::response::IntoResponse;
    use axum::routing::{get, post};
    use zecswap_api::service::MakerInfo;

    use super::*;
    use crate::tokens::tests::serve;

    /// A maker behind a gateway that answers `busy` first, each a `503` with its `Retry-After`
    /// if any: the requests it saw, and how one for its info ended.
    async fn info_through(busy: &[Option<&'static str>]) -> (usize, Result<MakerInfo>) {
        let busy = Arc::new(Mutex::new(busy.iter().copied().collect::<VecDeque<_>>()));
        let seen = Arc::new(Mutex::new(0));
        let answer = {
            let seen = seen.clone();
            move || async move {
                *seen.lock().unwrap() += 1;
                let Some(retry_after) = busy.lock().unwrap().pop_front() else {
                    return axum::Json(MakerInfo {
                        api_version: 1,
                        maker: Address::repeat_byte(1),
                        chain_id: 11_155_111,
                        contract: Address::repeat_byte(2),
                        token: Address::repeat_byte(3),
                        zcash_network: zecswap_api::Network::Testnet,
                        reverse_enabled: false,
                        token_return_key: None,
                    })
                    .into_response();
                };
                let mut headers = HeaderMap::new();
                if let Some(seconds) = retry_after {
                    headers.insert(RETRY_AFTER, seconds.parse().unwrap());
                }
                (StatusCode::SERVICE_UNAVAILABLE, headers, "busy").into_response()
            }
        };
        let maker =
            MakerApi::new(serve(Router::new().route("/v1/info", get(answer))).await).unwrap();
        let info = maker.info().await;
        let seen = *seen.lock().unwrap();
        (seen, info)
    }

    #[tokio::test]
    async fn a_busy_gateway_is_waited_out_as_it_asks() {
        let started = std::time::Instant::now();
        let (seen, info) = info_through(&[Some("1"), Some("1")]).await;
        assert_eq!(info.unwrap().chain_id, 11_155_111);
        assert_eq!(seen, 3);
        assert!(started.elapsed() >= Duration::from_secs(2));

        // A 503 asking no wait, or one too long, is the service's answer.
        for busy in [None, Some("3600"), Some("Wed, 21 Oct 2026 07:28:00 GMT")] {
            let started = std::time::Instant::now();
            let (seen, info) = info_through(&[busy]).await;
            let error = info.unwrap_err();
            let answer = error.downcast_ref::<ServiceError>().unwrap();
            assert_eq!(answer.status, StatusCode::SERVICE_UNAVAILABLE);
            assert!(!answer.refused(), "a 503 may follow a send");
            assert_eq!(seen, 1, "{busy:?}");
            assert!(started.elapsed() < Duration::from_secs(1));
        }
    }

    #[tokio::test]
    async fn a_request_for_a_swap_carries_its_ticket() {
        let seen = Arc::new(Mutex::new(Vec::new()));
        let record = {
            let seen = seen.clone();
            move |headers: HeaderMap| async move {
                let ticket = headers
                    .get(TICKET_HEADER)
                    .map(|value| value.to_str().unwrap());
                seen.lock().unwrap().push(ticket.map(str::to_owned));
                axum::Json(Sent {
                    transactions: vec![],
                })
            }
        };
        let relayer =
            RelayerApi::new(serve(Router::new().route("/v1/reverse/fund", post(record))).await)
                .unwrap();
        let funding = zecswap_api::reverse::Funding {
            swap_id: B256::repeat_byte(1),
            chain_id: 1,
            to: Address::ZERO,
            data: Default::default(),
            value: 0,
        };
        let ticket = format!("{}.{}", B256::repeat_byte(1), "ab".repeat(32));
        relayer.fund_reverse(&funding, Some(&ticket)).await.unwrap();
        relayer.fund_reverse(&funding, None).await.unwrap();
        assert_eq!(*seen.lock().unwrap(), [Some(ticket), None]);
    }
}
