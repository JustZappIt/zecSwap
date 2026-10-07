//! A service's side. A request it can be spammed with comes with a token, which it holds while
//! the request runs and keeps spent only if the request goes through: one that is refused leaves
//! its token spendable. A request without one is answered with RFC 9577's challenge, from which
//! a client fetches one. The service also hands tokens back, signed blind under a key of its own.

use std::collections::HashSet;
use std::path::PathBuf;
use std::sync::{Arc, Mutex, MutexGuard};

use anyhow::{Context as _, ensure};
use axum::extract::{Request, State};
use axum::http::{HeaderMap, HeaderValue, StatusCode, header};
use axum::middleware::Next;
use axum::response::Response;
use base64::Engine as _;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use rusqlite::{Connection, OptionalExtension, params};
use serde::Deserialize;
use zecswap_api::server::error;
use zecswap_api::service::ErrorCode;

use crate::{Challenge, IssuerKey, Token, TokenKey, today, www_authenticate};

/// `[tokens]` in a service's config.
#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Config {
    /// The issuer every token names.
    pub issuer: String,
    /// This service as tokens name it, so one meant for another is refused here.
    pub origin: String,
    /// The issuer's keys tokens are taken under, base64url SPKI, the current one first: an old
    /// one stays listed until the UTC day the issuer last signed with it is over.
    pub keys: Vec<String>,
    /// The service's own key for the tokens it hands back, a PKCS #8 PEM as `zecswap-issuer
    /// keygen` writes. Its public half is the same for every client, which pins it.
    pub return_key: PathBuf,
    /// Where spent tokens are recorded.
    pub spent: PathBuf,
}

pub struct Gate {
    challenge: Challenge,
    keys: Vec<TokenKey>,
    returns: IssuerKey,
    state: Mutex<Spent>,
}

struct Spent {
    store: Connection,
    /// The tokens of requests still running.
    held: HashSet<[u8; 32]>,
}

enum Refusal {
    Token(String),
    Store(rusqlite::Error),
}

impl Gate {
    pub fn open(config: &Config) -> anyhow::Result<Self> {
        let challenge = Challenge::new(&config.issuer, &config.origin, today())?;
        let keys = config
            .keys
            .iter()
            .map(|key| TokenKey::from_base64(key))
            .collect::<anyhow::Result<Vec<_>>>()
            .context("a token key")?;
        ensure!(!keys.is_empty(), "no token keys");
        let pem = std::fs::read_to_string(&config.return_key)
            .with_context(|| format!("reading {}", config.return_key.display()))?;
        let returns = IssuerKey::from_pem(&pem).context("the return key")?;
        let store = Connection::open(&config.spent)
            .with_context(|| format!("opening {}", config.spent.display()))?;
        store.execute_batch(
            "CREATE TABLE IF NOT EXISTS spent_tokens (nonce BLOB PRIMARY KEY, day INTEGER NOT NULL)",
        )?;
        Ok(Self {
            challenge,
            keys,
            returns,
            state: Mutex::new(Spent {
                store,
                held: HashSet::new(),
            }),
        })
    }

    /// What the service's tokens answer on `day`.
    pub fn challenge(&self, day: u64) -> Challenge {
        self.challenge.on(day)
    }

    /// The key the service hands tokens back under.
    pub fn return_key(&self) -> &TokenKey {
        self.returns.token_key()
    }

    /// A client's request for its token back, as it arrives (base64url): one the return key
    /// can sign.
    pub fn read_return_request(&self, request: &str) -> anyhow::Result<Vec<u8>> {
        let blinded = URL_SAFE_NO_PAD
            .decode(request)
            .context("a token request that is not base64url")?;
        self.return_key().check_request(&blinded)?;
        Ok(blinded)
    }

    /// Hands a token back, signed blind (base64url): the service never sees the token it
    /// becomes.
    pub fn sign_return(&self, blinded: &[u8]) -> anyhow::Result<String> {
        Ok(URL_SAFE_NO_PAD.encode(self.returns.sign(blinded)?))
    }

    /// Holds the request's token, unless it is missing, forged, meant for another service or
    /// day, spent, or held by another request.
    fn hold(self: &Arc<Self>, headers: &HeaderMap) -> Result<Spend, Refusal> {
        let refuse = |why: &str| Refusal::Token(why.into());
        let header = headers
            .get(header::AUTHORIZATION)
            .ok_or_else(|| refuse("this request takes a token"))?
            .to_str()
            .map_err(|_| refuse("an unreadable authorization"))?;
        let token = Token::from_authorization(header).map_err(|e| refuse(&e.to_string()))?;
        let key = self
            .keys
            .iter()
            .chain([self.return_key()])
            .find(|key| key.id() == token.key_id())
            .ok_or_else(|| refuse("a token under a key not taken here"))?;
        let day = today();
        token
            .verify(key, &self.challenge(day))
            .map_err(|e| refuse(&e.to_string()))?;
        let nonce = token.nonce();
        let mut state = self.state();
        if state.held.contains(&nonce) {
            return Err(refuse("a token another request is spending"));
        }
        let spent = state
            .store
            .query_row(
                "SELECT 1 FROM spent_tokens WHERE nonce = ?1",
                [nonce],
                |_| Ok(()),
            )
            .optional()
            .map_err(Refusal::Store)?;
        if spent.is_some() {
            return Err(refuse("a token already spent"));
        }
        state.held.insert(nonce);
        Ok(Spend(Arc::new(Held {
            gate: self.clone(),
            nonce,
            day,
        })))
    }

    fn ask(&self) -> HeaderValue {
        HeaderValue::from_str(&www_authenticate(&self.challenge(today()), &self.keys[0]))
            .expect("base64url is a header value")
    }

    fn state(&self) -> MutexGuard<'_, Spent> {
        self.state
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }
}

/// The token a request came with, held while it runs: spent for good once the request keeps
/// it, spendable again if the request ends without.
#[derive(Clone)]
pub struct Spend(Arc<Held>);

struct Held {
    gate: Arc<Gate>,
    nonce: [u8; 32],
    day: u64,
}

impl Spend {
    pub fn keep(&self) -> anyhow::Result<()> {
        let Held { gate, nonce, day } = &*self.0;
        let state = gate.state();
        state.store.execute(
            "INSERT OR IGNORE INTO spent_tokens (nonce, day) VALUES (?1, ?2)",
            params![nonce, day],
        )?;
        // A token for an earlier day is refused anyway: keep no record of it.
        state
            .store
            .execute("DELETE FROM spent_tokens WHERE day < ?1", [day])?;
        Ok(())
    }
}

impl Drop for Held {
    fn drop(&mut self) {
        self.gate.state().held.remove(&self.nonce);
    }
}

/// Middleware for the routes a token pays for: the handler finds the token's `Spend` among
/// the request's extensions.
pub async fn require(State(gate): State<Arc<Gate>>, mut request: Request, next: Next) -> Response {
    match gate.hold(request.headers()) {
        Ok(spend) => {
            request.extensions_mut().insert(spend);
            next.run(request).await
        }
        Err(Refusal::Token(why)) => {
            let mut response = error(StatusCode::UNAUTHORIZED, ErrorCode::TokenRequired, why);
            response
                .headers_mut()
                .insert(header::WWW_AUTHENTICATE, gate.ask());
            response
        }
        Err(Refusal::Store(e)) => {
            tracing::error!(error = %e, "spent tokens are unavailable");
            error(
                StatusCode::INTERNAL_SERVER_ERROR,
                ErrorCode::Internal,
                "spent tokens are unavailable",
            )
        }
    }
}

#[cfg(test)]
mod tests {
    use axum::body::Body;
    use axum::routing::post;
    use axum::{Extension, Router};
    use tokio::sync::Notify;
    use tower::ServiceExt;

    use super::*;
    use crate::{Pending, read_www_authenticate};

    fn token(key: &IssuerKey, challenge: &Challenge) -> Token {
        let (pending, blinded) = Pending::new(key.token_key(), challenge).unwrap();
        pending
            .finalize(key.token_key(), &key.sign(&blinded).unwrap())
            .unwrap()
    }

    struct Service {
        _dir: tempfile::TempDir,
        issuer: IssuerKey,
        config: Config,
    }

    impl Service {
        fn new() -> Self {
            let dir = tempfile::tempdir().unwrap();
            let issuer = IssuerKey::generate().unwrap();
            let return_key = dir.path().join("return.pem");
            std::fs::write(
                &return_key,
                IssuerKey::generate().unwrap().to_pem().unwrap(),
            )
            .unwrap();
            let config = Config {
                issuer: "issuer.test".into(),
                origin: "maker".into(),
                keys: vec![issuer.token_key().to_base64()],
                return_key,
                spent: dir.path().join("spent.sqlite"),
            };
            Self {
                _dir: dir,
                issuer,
                config,
            }
        }

        /// `/keep` keeps its token, `/refuse` refuses the request without, and `/wait` keeps
        /// it once `go` is notified.
        fn app(&self, config: &Config, go: Arc<Notify>) -> (Arc<Gate>, Router) {
            let gate = Arc::new(Gate::open(config).unwrap());
            let router = Router::new()
                .route(
                    "/keep",
                    post(|Extension(spend): Extension<Spend>| async move {
                        spend.keep().unwrap();
                        StatusCode::OK
                    }),
                )
                .route("/refuse", post(|| async { StatusCode::BAD_REQUEST }))
                .route(
                    "/wait",
                    post(|Extension(spend): Extension<Spend>| async move {
                        go.notified().await;
                        spend.keep().unwrap();
                        StatusCode::OK
                    }),
                )
                .route_layer(axum::middleware::from_fn_with_state(gate.clone(), require));
            (gate, router)
        }

        fn today(&self) -> Challenge {
            Challenge::new("issuer.test", "maker", today()).unwrap()
        }
    }

    async fn send(app: &Router, path: &str, token: Option<&Token>) -> StatusCode {
        let mut request = Request::post(path);
        if let Some(token) = token {
            request = request.header(header::AUTHORIZATION, token.authorization());
        }
        app.clone()
            .oneshot(request.body(Body::empty()).unwrap())
            .await
            .unwrap()
            .status()
    }

    /// A request without a token is asked for today's; a token kept by its request buys
    /// nothing more, here or after a restart, while one whose request was refused buys the
    /// next. Tokens for another service or day, or under a retired key, buy nothing; the
    /// service's own returned tokens are taken like the issuer's.
    #[tokio::test]
    async fn a_token_is_spent_only_by_a_request_that_keeps_it() {
        let service = Service::new();
        let (gate, maker) = service.app(&service.config, Arc::default());
        let asked = maker
            .clone()
            .oneshot(Request::post("/keep").body(Body::empty()).unwrap())
            .await
            .unwrap();
        assert_eq!(asked.status(), StatusCode::UNAUTHORIZED);
        let asked = asked.headers()[header::WWW_AUTHENTICATE].to_str().unwrap();
        let (challenge, key) = read_www_authenticate(asked).unwrap();
        assert_eq!(
            (&challenge, key.id()),
            (&service.today(), service.issuer.token_key().id())
        );

        let paid = token(&service.issuer, &challenge);
        assert_eq!(
            send(&maker, "/refuse", Some(&paid)).await,
            StatusCode::BAD_REQUEST
        );
        assert_eq!(
            send(&maker, "/refuse", Some(&paid)).await,
            StatusCode::BAD_REQUEST
        );
        assert_eq!(send(&maker, "/keep", Some(&paid)).await, StatusCode::OK);
        assert_eq!(
            send(&maker, "/keep", Some(&paid)).await,
            StatusCode::UNAUTHORIZED
        );
        let (_, restarted) = service.app(&service.config, Arc::default());
        let again = send(&restarted, "/refuse", Some(&paid)).await;
        assert_eq!(again, StatusCode::UNAUTHORIZED);

        let yesterday = token(&service.issuer, &challenge.on(challenge.day() - 1));
        let stale = send(&restarted, "/keep", Some(&yesterday)).await;
        assert_eq!(stale, StatusCode::UNAUTHORIZED);
        let relayer = Challenge::new("issuer.test", "relayer", today()).unwrap();
        let elsewhere = send(&restarted, "/keep", Some(&token(&service.issuer, &relayer))).await;
        assert_eq!(elsewhere, StatusCode::UNAUTHORIZED);

        let returned_key = gate.return_key().clone();
        let (pending, blinded) = Pending::new(&returned_key, &challenge).unwrap();
        let request = gate
            .read_return_request(&URL_SAFE_NO_PAD.encode(&blinded))
            .unwrap();
        let signature = URL_SAFE_NO_PAD
            .decode(gate.sign_return(&request).unwrap())
            .unwrap();
        let returned = pending.finalize(&returned_key, &signature).unwrap();
        assert_eq!(
            send(&restarted, "/keep", Some(&returned)).await,
            StatusCode::OK
        );

        let unspent = token(&service.issuer, &challenge);
        let next = IssuerKey::generate().unwrap();
        let (_, rotated) = service.app(
            &Config {
                keys: vec![next.token_key().to_base64()],
                ..service.config.clone()
            },
            Arc::default(),
        );
        let retired = send(&rotated, "/keep", Some(&unspent)).await;
        assert_eq!(retired, StatusCode::UNAUTHORIZED);
        let current = send(&rotated, "/keep", Some(&token(&next, &challenge))).await;
        assert_eq!(current, StatusCode::OK);
    }

    /// Two requests at once with one token: the second is refused while the first holds it,
    /// and once the first keeps it, so is every later one.
    #[tokio::test]
    async fn one_token_sent_twice_at_once_buys_one_request() {
        let service = Service::new();
        let go = Arc::new(Notify::new());
        let (_, maker) = service.app(&service.config, go.clone());
        let paid = token(&service.issuer, &service.today());
        let first = tokio::spawn({
            let (maker, paid) = (maker.clone(), paid.clone());
            async move { send(&maker, "/wait", Some(&paid)).await }
        });
        let held = tokio::time::timeout(std::time::Duration::from_secs(5), async {
            while send(&maker, "/refuse", Some(&paid)).await != StatusCode::UNAUTHORIZED {
                tokio::task::yield_now().await;
            }
        });
        held.await
            .expect("a second request went through while the first held the token");
        assert_eq!(
            send(&maker, "/keep", Some(&paid)).await,
            StatusCode::UNAUTHORIZED
        );
        go.notify_one();
        assert_eq!(first.await.unwrap(), StatusCode::OK);
        assert_eq!(
            send(&maker, "/keep", Some(&paid)).await,
            StatusCode::UNAUTHORIZED
        );
    }
}
