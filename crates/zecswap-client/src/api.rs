use std::sync::Arc;
use std::time::Duration;

use alloy_primitives::{Address, B256};
use anyhow::{Result, bail};
use reqwest::StatusCode;
use reqwest::header::{AUTHORIZATION, WWW_AUTHENTICATE};
use serde::Serialize;
use serde::de::DeserializeOwned;
use zecswap_api::relayer::{Claim, LockClaim, Payout, Sent, Terms};
use zecswap_api::{Acceptance, Accepted, Quote, QuoteRequest};

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

    /// Spends a token from `tokens` whenever the maker asks for one.
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

    /// Spends a token from `tokens` whenever the relayer asks for one.
    pub fn with_tokens(mut self, tokens: Arc<Tokens>) -> Self {
        self.0.tokens = Some(tokens);
        self
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

    async fn get<T: DeserializeOwned>(&self, path: &str) -> Result<T> {
        self.read(self.http.get(format!("{}{path}", self.url)))
            .await
    }

    async fn post<T: DeserializeOwned>(&self, path: &str, body: &impl Serialize) -> Result<T> {
        self.read(self.http.post(format!("{}{path}", self.url)).json(body))
            .await
    }

    async fn read<T: DeserializeOwned>(&self, request: reqwest::RequestBuilder) -> Result<T> {
        let again = request.try_clone();
        let mut response = request.send().await?;
        // Asked for a token: spend one and send the request once more.
        if response.status() == StatusCode::UNAUTHORIZED
            && let (Some(tokens), Some(again)) = (&self.tokens, again)
            && let Some(asked) = response.headers().get(WWW_AUTHENTICATE)
        {
            let token = tokens.take(asked.to_str()?).await?;
            response = again
                .header(AUTHORIZATION, token.authorization())
                .send()
                .await?;
        }
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

#[cfg(test)]
mod tests {
    use axum::Router;
    use axum::middleware::from_fn_with_state;
    use axum::routing::post;
    use zecswap_issuer::{Attestation, Issuer};
    use zecswap_tokens::IssuerKey;
    use zecswap_tokens::server::{Config, Gate, require};

    use super::*;

    async fn serve(app: Router) -> String {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("http://{}", listener.local_addr().unwrap());
        tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        url
    }

    /// Asked for a token, the client spends one held or fetches a batch, and sends the request
    /// again, until the issuer's daily allowance for the device runs out, the last batch short.
    /// A service that asks for tokens under a key its issuer does not publish gets none.
    #[tokio::test]
    async fn requests_spend_tokens_from_the_issuer_within_the_devices_allowance() {
        let dir = tempfile::tempdir().unwrap();
        let key = IssuerKey::generate().unwrap();
        let pem = dir.path().join("issuer.pem");
        std::fs::write(&pem, key.to_pem().unwrap()).unwrap();
        let issuer = Issuer::new(zecswap_issuer::Config {
            listen: "127.0.0.1:0".parse().unwrap(),
            name: "issuer.test".into(),
            key: pem,
            data_dir: dir.path().into(),
            tokens_per_day: 3,
            attestation: Attestation::InsecureTest,
        })
        .unwrap();
        let issuer = serve(zecswap_issuer::router(Arc::new(issuer))).await;
        let service = |key: &IssuerKey, spent: &str| {
            let gate = Gate::open(&Config {
                issuer: "issuer.test".into(),
                origin: "maker".into(),
                keys: vec![key.token_key().to_base64()],
                spent: dir.path().join(spent),
            })
            .unwrap();
            Router::new()
                .route("/v1/echo", post(|body: String| async move { body }))
                .route_layer(from_fn_with_state(Arc::new(gate), require))
        };
        let tokens = Arc::new(Tokens::new(issuer, b"phone".to_vec(), 2).unwrap());
        let endpoint = |url: String| Endpoint {
            tokens: Some(tokens.clone()),
            ..Endpoint::new("maker", url).unwrap()
        };

        let maker = endpoint(serve(service(&key, "maker.sqlite")).await);
        for n in [1, 2, 3] {
            assert_eq!(maker.post::<u32>("/v1/echo", &n).await.unwrap(), n);
        }
        let spent = maker.post::<u32>("/v1/echo", &4).await.unwrap_err();
        assert!(spent.to_string().contains("429"), "{spent}");

        let marking = IssuerKey::generate().unwrap();
        let elsewhere = endpoint(serve(service(&marking, "elsewhere.sqlite")).await);
        let refused = elsewhere.post::<u32>("/v1/echo", &1).await;
        let refused = refused.unwrap_err().to_string();
        assert!(refused.contains("does not sign"), "{refused}");
    }
}
