//! Pays for each accept with a Privacy Pass token when the maker asks for one, fetching a batch
//! from the issuer when none is held, and asks for it back: a swap the user pays into hands its
//! token back, signed blind under the maker's return key. Neither the issuer nor the maker can
//! tell which of this device's tokens paid for which swap.

use std::collections::HashMap;
use std::sync::{Mutex, MutexGuard};
use std::time::{SystemTime, UNIX_EPOCH};

use alloy_primitives::B256;
use anyhow::{Context as _, Result, anyhow, bail, ensure};
use base64::Engine as _;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use reqwest::StatusCode;
use zecswap_api::tokens::{
    Attestation, AttestationChallenge, TokenKey as Published, TokenRequests, TokenResponses,
};
use zecswap_tokens::{Challenge, Pending, Token, TokenKey, day, read_www_authenticate};

/// How far the maker's clock may be from ours around midnight, when the day it asks tokens
/// for turns.
const CLOCK_SKEW: u64 = 10 * 60;

/// How this install proves to the issuer that it is genuine: its attestation of a request
/// carrying `blinded`, for the issuer's `challenge` (base64url, as given).
pub trait Attest: Send + Sync {
    fn attest(&self, challenge: &str, blinded: &[Vec<u8>]) -> Result<Attestation>;
}

/// A device id the issuer's `insecure-test` mode takes on trust: for tests.
pub struct Unattested(pub Vec<u8>);

impl Attest for Unattested {
    fn attest(&self, challenge: &str, _: &[Vec<u8>]) -> Result<Attestation> {
        Ok(Attestation {
            challenge: challenge.into(),
            chain: vec![URL_SAFE_NO_PAD.encode(&self.0)],
            signature: String::new(),
        })
    }
}

pub struct Tokens {
    issuer: String,
    attest: Box<dyn Attest>,
    batch: usize,
    /// The key the maker hands tokens back under, pinned: one a maker gave only some devices
    /// would mark their returned tokens.
    return_key: TokenKey,
    http: reqwest::Client,
    state: Mutex<State>,
}

#[derive(Default)]
struct State {
    /// Unspent tokens, by the challenge they answer, its day included, and the key they are
    /// under.
    held: HashMap<(Challenge, [u8; 32]), Vec<Token>>,
    /// Each accept's request for its token back, by swap: more than one if it was retried.
    returns: HashMap<B256, Vec<(Challenge, Pending)>>,
    /// The day the issuer said this device's allowance was spent.
    exhausted: Option<u64>,
}

/// What an accept pays with: a token, and the request for it back that the accept carries.
pub(crate) struct Payment {
    pub(crate) token: Token,
    pub(crate) request: String,
    challenge: Challenge,
    pending: Pending,
}

impl Tokens {
    /// Fetches `batch` tokens at a time from the issuer API at `issuer`, for the install
    /// `attest` speaks for, and asks for tokens back under `return_key` (base64url SPKI), the
    /// maker's published one.
    pub fn new(
        issuer: impl Into<String>,
        attest: impl Attest + 'static,
        batch: usize,
        return_key: &str,
    ) -> Result<Self> {
        ensure!(batch > 0, "a token batch of none");
        Ok(Self {
            issuer: issuer.into(),
            attest: Box::new(attest),
            batch,
            return_key: TokenKey::from_base64(return_key).context("the return key")?,
            http: reqwest::Client::builder()
                .timeout(std::time::Duration::from_secs(30))
                .build()?,
            state: Mutex::default(),
        })
    }

    /// A token for what the maker's `WWW-Authenticate` asks, and a request for it back. The
    /// issuer's tokens go first: a returned one spent soon after it came back could be tied
    /// to the swap that returned it.
    pub(crate) async fn pay(&self, asked: &str) -> Result<Payment> {
        let (challenge, key) = read_www_authenticate(asked)?;
        let now = SystemTime::now().duration_since(UNIX_EPOCH)?.as_secs();
        // A day only some devices were asked for would mark their tokens.
        ensure!(
            (day(now.saturating_sub(CLOCK_SKEW))..=day(now + CLOCK_SKEW))
                .contains(&challenge.day()),
            "the maker asks for tokens for another day"
        );
        let today = challenge.day();
        {
            let mut state = self.state();
            // Earlier days' tokens are refused, and so would be the tokens their requests
            // become.
            state.held.retain(|(held, _), _| held.day() >= today);
            state.returns.retain(|_, requests| {
                requests.retain(|(challenge, _)| challenge.day() >= today);
                !requests.is_empty()
            });
        }
        let exhausted = self.state().exhausted == Some(today);
        let token = match self.take(&challenge, key.id()) {
            Some(token) => token,
            None if exhausted => self.returned(&challenge)?,
            None => match self.fetch(&challenge, &key).await? {
                Some(token) => token,
                None => {
                    self.state().exhausted = Some(today);
                    self.returned(&challenge)?
                }
            },
        };
        let (pending, blinded) = Pending::new(&self.return_key, &challenge)?;
        Ok(Payment {
            token,
            request: URL_SAFE_NO_PAD.encode(blinded),
            challenge,
            pending,
        })
    }

    /// Settles an accept paid with `payment` once the maker answers `status`, or none if the
    /// answer was lost. Refused before the maker took its quote, the accept left the token
    /// spendable: it is kept, to go first next time; if the maker took the quote after all, it
    /// refuses the token then and the next goes. Unless the maker refused the token itself,
    /// the swap may still hand one back.
    pub(crate) fn settle(&self, swap_id: B256, payment: Payment, status: Option<StatusCode>) {
        let Payment {
            token,
            challenge,
            pending,
            ..
        } = payment;
        if status == Some(StatusCode::UNAUTHORIZED) {
            return;
        }
        let mut state = self.state();
        if !status.is_some_and(|status| status.is_success()) {
            let held = (challenge.clone(), token.key_id());
            state.held.entry(held).or_default().push(token);
        }
        state
            .returns
            .entry(swap_id)
            .or_default()
            .push((challenge, pending));
    }

    /// Holds the token a swap's status hands back, if it does: whether one is now held.
    pub(crate) fn collect(&self, swap_id: B256, token_return: Option<&str>) -> Result<bool> {
        let Some(token_return) = token_return else {
            return Ok(false);
        };
        let signature = URL_SAFE_NO_PAD.decode(token_return)?;
        let mut state = self.state();
        let requests = state
            .returns
            .remove(&swap_id)
            .context("no request for this swap's token is pending")?;
        // The maker signed one of the swap's requests, the one it took the quote with.
        for (challenge, pending) in requests {
            if let Ok(token) = pending.finalize(&self.return_key, &signature) {
                let held = (challenge, self.return_key.id());
                state.held.entry(held).or_default().push(token);
                return Ok(true);
            }
        }
        bail!("the maker's signature finalizes none of the swap's requests")
    }

    fn take(&self, challenge: &Challenge, key: [u8; 32]) -> Option<Token> {
        self.state()
            .held
            .get_mut(&(challenge.clone(), key))
            .and_then(Vec::pop)
    }

    fn returned(&self, challenge: &Challenge) -> Result<Token> {
        self.take(challenge, self.return_key.id())
            .ok_or_else(|| anyhow!("this device's tokens for today are spent"))
    }

    /// A token from a new batch, the rest held; none if the device's allowance for the day is
    /// spent.
    async fn fetch(&self, challenge: &Challenge, key: &TokenKey) -> Result<Option<Token>> {
        // A key only some devices were asked to use would mark their tokens: take only the one
        // the issuer publishes to everyone.
        let published: Published = self
            .http
            .get(format!("{}/v1/token-key", self.issuer))
            .send()
            .await?
            .error_for_status()?
            .json()
            .await?;
        ensure!(
            published.token_key == key.to_base64() && published.issuer == challenge.issuer(),
            "the service asks for tokens its issuer does not sign"
        );
        let (pending, blinded): (Vec<Pending>, Vec<Vec<u8>>) = (0..self.batch)
            .map(|_| Pending::new(key, challenge))
            .collect::<Result<Vec<_>>>()?
            .into_iter()
            .unzip();
        let attesting: AttestationChallenge = self
            .http
            .get(format!("{}/v1/challenge", self.issuer))
            .send()
            .await?
            .error_for_status()?
            .json()
            .await?;
        let request = TokenRequests {
            attestation: self.attest.attest(&attesting.challenge, &blinded)?,
            blinded: blinded.iter().map(|b| URL_SAFE_NO_PAD.encode(b)).collect(),
        };
        let response = self
            .http
            .post(format!("{}/v1/tokens", self.issuer))
            .json(&request)
            .send()
            .await?;
        let status = response.status();
        if status == StatusCode::TOO_MANY_REQUESTS {
            return Ok(None);
        }
        ensure!(
            status.is_success(),
            "the issuer answered {status}: {}",
            response.text().await?
        );
        let issued: TokenResponses = response.json().await?;
        // The issuer signs no more than the day has left.
        ensure!(
            (1..=pending.len()).contains(&issued.blind_signatures.len()),
            "the issuer signed another number of tokens"
        );
        let mut tokens = pending
            .into_iter()
            .zip(issued.blind_signatures)
            .map(|(pending, signature)| pending.finalize(key, &URL_SAFE_NO_PAD.decode(signature)?))
            .collect::<Result<Vec<_>>>()
            .context("the issuer's signatures")?;
        let token = tokens.pop().expect("a batch of at least one");
        // Added to, not replaced: another accept may have fetched a batch meanwhile.
        self.state()
            .held
            .entry((challenge.clone(), key.id()))
            .or_default()
            .extend(tokens);
        Ok(Some(token))
    }

    fn state(&self) -> MutexGuard<'_, State> {
        self.state
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use axum::extract::{Path, Request, State};
    use axum::http::header::AUTHORIZATION;
    use axum::middleware::{Next, from_fn_with_state};
    use axum::response::Response;
    use axum::routing::{get, post};
    use axum::{Extension, Json, Router};
    use rand::{rand_core::UnwrapErr, rngs::SysRng};
    use zecswap_api::{Acceptance, Accepted, Status};
    use zecswap_core::{SecretShare, ShareProof, ViewingKeys};
    use zecswap_issuer::{Attestation, Issuer};
    use zecswap_tokens::server::{Config, Gate, Spend, require};
    use zecswap_tokens::{IssuerKey, today, www_authenticate};

    use super::*;
    use crate::MakerApi;

    /// Every accept of it is refused.
    const REFUSED: B256 = B256::repeat_byte(0xff);

    /// A maker's accepts and forward statuses: an accept keeps its token and opens the swap its
    /// quote id names, unless it is `REFUSED`; a swap marked paid hands its token back.
    struct Maker {
        gate: Arc<Gate>,
        /// Every token sent, whether or not the gate took it.
        sent: Mutex<Vec<Token>>,
        requests: Mutex<Vec<(B256, String)>>,
        paid: Mutex<Vec<B256>>,
    }

    async fn serve(app: Router) -> String {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("http://{}", listener.local_addr().unwrap());
        tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        url
    }

    fn issuer(dir: &std::path::Path, key: &IssuerKey) -> Issuer {
        let pem = dir.join("issuer.pem");
        std::fs::write(&pem, key.to_pem().unwrap()).unwrap();
        Issuer::new(zecswap_issuer::Config {
            listen: "127.0.0.1:0".parse().unwrap(),
            name: "issuer.test".into(),
            key: pem,
            data_dir: dir.into(),
            tokens_per_day: 2,
            attestation: Attestation::InsecureTest,
            allow_insecure: true,
        })
        .unwrap()
    }

    async fn maker(dir: &std::path::Path, key: &IssuerKey) -> (Arc<Maker>, String) {
        let return_key = dir.join("return.pem");
        std::fs::write(
            &return_key,
            IssuerKey::generate().unwrap().to_pem().unwrap(),
        )
        .unwrap();
        let gate = Gate::open(&Config {
            issuer: "issuer.test".into(),
            origin: "maker".into(),
            keys: vec![key.token_key().to_base64()],
            return_key,
            spent: dir.join(format!("{}.sqlite", key.token_key().to_base64().len())),
        })
        .unwrap();
        let maker = Arc::new(Maker {
            gate: Arc::new(gate),
            sent: Mutex::default(),
            requests: Mutex::default(),
            paid: Mutex::default(),
        });
        async fn accept(
            State(maker): State<Arc<Maker>>,
            Path(quote): Path<B256>,
            Extension(spend): Extension<Spend>,
            Json(acceptance): Json<Acceptance>,
        ) -> Result<Json<Accepted>, StatusCode> {
            if quote == REFUSED {
                return Err(StatusCode::BAD_REQUEST);
            }
            spend.keep().unwrap();
            let request = acceptance.token_request.unwrap();
            maker.requests.lock().unwrap().push((quote, request));
            Ok(Json(Accepted {
                swap_id: quote,
                t0: 0,
                t1: 0,
            }))
        }
        async fn status(
            State(maker): State<Arc<Maker>>,
            Path(swap_id): Path<B256>,
        ) -> Json<Status> {
            let paid = maker.paid.lock().unwrap().contains(&swap_id);
            let requests = maker.requests.lock().unwrap();
            let request = requests.iter().find(|(id, _)| *id == swap_id);
            let token_return = request.filter(|_| paid).map(|(_, request)| {
                let blinded = maker.gate.read_return_request(request).unwrap();
                maker.gate.sign_return(&blinded).unwrap()
            });
            Json(Status {
                swap_id,
                token_return,
            })
        }
        async fn record(State(maker): State<Arc<Maker>>, request: Request, next: Next) -> Response {
            if let Some(header) = request.headers().get(AUTHORIZATION) {
                let token = Token::from_authorization(header.to_str().unwrap()).unwrap();
                maker.sent.lock().unwrap().push(token);
            }
            next.run(request).await
        }
        let app = Router::new()
            .route("/v1/quote/{quote}/accept", post(accept))
            .route_layer(from_fn_with_state(maker.gate.clone(), require))
            .route("/v1/swaps/{swap_id}", get(status))
            .layer(from_fn_with_state(maker.clone(), record))
            .with_state(maker.clone());
        let url = serve(app).await;
        (maker, url)
    }

    fn acceptance() -> Acceptance {
        Acceptance {
            user_share: SecretShare::random(UnwrapErr(SysRng)).public(),
            user_proof: ShareProof::from_bytes([0; 64]),
            viewing_keys: ViewingKeys::random(UnwrapErr(SysRng)),
            token_request: None,
        }
    }

    /// Asked for a token, the client pays with one of the issuer's, fetched in a batch, and
    /// asks for it back; a refused accept's token pays for the next. A swap paid into hands its
    /// token back, which the client spends only once the issuer's allowance for the day is
    /// gone; one for an earlier day is dropped unsent, and a challenge for another day or under
    /// a key the issuer does not publish gets nothing.
    #[tokio::test]
    async fn accepts_spend_the_issuers_tokens_then_those_handed_back() {
        let dir = tempfile::tempdir().unwrap();
        let key = IssuerKey::generate().unwrap();
        let issuer = serve(zecswap_issuer::router(Arc::new(issuer(dir.path(), &key)))).await;
        let (fake, url) = maker(dir.path(), &key).await;
        let return_key = fake.gate.return_key().clone();
        let phone = Unattested(b"phone".to_vec());
        let tokens = Tokens::new(&issuer, phone, 2, &return_key.to_base64()).unwrap();
        let tokens = Arc::new(tokens);
        let api = MakerApi::new(url.clone())
            .unwrap()
            .with_tokens(tokens.clone());
        let accept = |id: u8| {
            let api = api.clone();
            let id = B256::repeat_byte(id);
            async move { api.accept(id, id, &acceptance()).await }
        };
        let sent = || fake.sent.lock().unwrap().clone();

        accept(1).await.unwrap();
        let refused = api.accept(REFUSED, REFUSED, &acceptance()).await;
        assert!(refused.unwrap_err().to_string().contains("400"));
        assert!(
            !api.collect_token(B256::repeat_byte(1)).await.unwrap(),
            "not paid yet"
        );
        fake.paid.lock().unwrap().push(B256::repeat_byte(1));
        assert!(api.collect_token(B256::repeat_byte(1)).await.unwrap());
        accept(2).await.unwrap();
        let [first, refused, again] = <[Token; 3]>::try_from(sent()).unwrap();
        assert_eq!(
            again, refused,
            "the refused accept's token paid for the next"
        );
        assert!(
            [first, again]
                .iter()
                .all(|t| t.key_id() == key.token_key().id())
        );

        accept(3).await.unwrap();
        assert_eq!(
            sent()[3].key_id(),
            return_key.id(),
            "the issuer had none left"
        );

        // A swap accepted yesterday hands back a token for yesterday.
        let challenge = fake.gate.challenge(today());
        let (pending, blinded) = Pending::new(&return_key, &challenge.on(today() - 1)).unwrap();
        let stale = B256::repeat_byte(4);
        let request = (stale, URL_SAFE_NO_PAD.encode(blinded));
        fake.requests.lock().unwrap().push(request);
        fake.paid.lock().unwrap().push(stale);
        let returns = (challenge.on(today() - 1), pending);
        tokens.state().returns.insert(stale, vec![returns]);
        assert!(api.collect_token(stale).await.unwrap());
        let spent = accept(5).await.unwrap_err().to_string();
        assert!(spent.contains("spent"), "{spent}");
        assert_eq!(sent().len(), 4, "yesterday's token was sent");

        let tomorrow = www_authenticate(&challenge.on(today() + 2), key.token_key());
        let refused = tokens.pay(&tomorrow).await.err().unwrap().to_string();
        assert!(refused.contains("another day"), "{refused}");
        let marking = IssuerKey::generate().unwrap();
        let (elsewhere, url) = maker(dir.path(), &marking).await;
        let tablet = Unattested(b"tablet".to_vec());
        let fresh = Tokens::new(&issuer, tablet, 2, &return_key.to_base64());
        let api = MakerApi::new(url)
            .unwrap()
            .with_tokens(Arc::new(fresh.unwrap()));
        let refused = api.accept(B256::ZERO, B256::ZERO, &acceptance()).await;
        let refused = refused.unwrap_err().to_string();
        assert!(refused.contains("does not sign"), "{refused}");
        assert!(elsewhere.sent.lock().unwrap().is_empty());
    }
}
