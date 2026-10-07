//! A service's side: it takes a fresh token for each request it can be spammed with, and
//! answers any other with RFC 9577's challenge, from which a client fetches one.

use std::path::PathBuf;
use std::sync::{Arc, Mutex};

use anyhow::{Context as _, ensure};
use axum::extract::{Request, State};
use axum::http::{HeaderMap, HeaderValue, StatusCode, header};
use axum::middleware::Next;
use axum::response::Response;
use rusqlite::{Connection, params};
use serde::Deserialize;
use zecswap_api::server::error;
use zecswap_api::service::ErrorCode;

use crate::{Challenge, Token, TokenKey, www_authenticate};

/// `[tokens]` in a service's config.
#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Config {
    /// The issuer every token names.
    pub issuer: String,
    /// This service as tokens name it, so one meant for another is refused here.
    pub origin: String,
    /// The issuer's keys tokens are taken under, base64url SPKI, the current one first: an old
    /// one stays listed until tokens it signed have been spent.
    pub keys: Vec<String>,
    /// Where spent tokens are recorded.
    pub spent: PathBuf,
}

pub struct Gate {
    challenge: Challenge,
    keys: Vec<TokenKey>,
    spent: Mutex<Connection>,
    ask: HeaderValue,
}

enum Refusal {
    Token(String),
    Store(rusqlite::Error),
}

impl Gate {
    pub fn open(config: &Config) -> anyhow::Result<Self> {
        let challenge = Challenge::new(&config.issuer, &config.origin)?;
        let keys = config
            .keys
            .iter()
            .map(|key| TokenKey::from_base64(key))
            .collect::<anyhow::Result<Vec<_>>>()
            .context("a token key")?;
        ensure!(!keys.is_empty(), "no token keys");
        let spent = Connection::open(&config.spent)
            .with_context(|| format!("opening {}", config.spent.display()))?;
        spent.execute_batch(
            "CREATE TABLE IF NOT EXISTS spent (nonce BLOB PRIMARY KEY, key_id BLOB NOT NULL)",
        )?;
        // Tokens under a key no longer taken can't come back: forget them.
        let ids: Vec<[u8; 32]> = keys.iter().map(TokenKey::id).collect();
        spent.execute(
            &format!(
                "DELETE FROM spent WHERE key_id NOT IN ({})",
                vec!["?"; ids.len()].join(", ")
            ),
            rusqlite::params_from_iter(ids),
        )?;
        let ask = HeaderValue::from_str(&www_authenticate(&challenge, &keys[0]))?;
        Ok(Self {
            challenge,
            keys,
            spent: Mutex::new(spent),
            ask,
        })
    }

    /// Spends the request's token, unless it is missing, forged, meant elsewhere or spent.
    fn spend(&self, headers: &HeaderMap) -> Result<(), Refusal> {
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
            .find(|key| key.id() == token.key_id())
            .ok_or_else(|| refuse("a token under a key not taken here"))?;
        token
            .verify(key, &self.challenge)
            .map_err(|e| refuse(&e.to_string()))?;
        let fresh = self
            .spent
            .lock()
            .unwrap()
            .execute(
                "INSERT OR IGNORE INTO spent (nonce, key_id) VALUES (?1, ?2)",
                params![token.nonce(), token.key_id()],
            )
            .map_err(Refusal::Store)?;
        if fresh == 1 {
            Ok(())
        } else {
            Err(refuse("a token already spent"))
        }
    }
}

/// Middleware for the routes a token pays for.
pub async fn require(State(gate): State<Arc<Gate>>, request: Request, next: Next) -> Response {
    match gate.spend(request.headers()) {
        Ok(()) => next.run(request).await,
        Err(Refusal::Token(why)) => {
            let mut response = error(StatusCode::UNAUTHORIZED, ErrorCode::TokenRequired, why);
            response
                .headers_mut()
                .insert(header::WWW_AUTHENTICATE, gate.ask.clone());
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
    use axum::Router;
    use axum::body::Body;
    use axum::routing::post;
    use tower::ServiceExt;

    use super::*;
    use crate::{IssuerKey, Pending, read_www_authenticate};

    fn token(issuer: &IssuerKey, challenge: &Challenge) -> Token {
        let key = issuer.token_key();
        let (pending, blinded) = Pending::new(key, challenge).unwrap();
        pending
            .finalize(key, &issuer.sign(&blinded).unwrap())
            .unwrap()
    }

    /// A request without a token is asked for one; a token buys exactly one request, here and
    /// across a restart, and only at the service it names; a retired key's tokens stop
    /// buying anything.
    #[tokio::test]
    async fn each_token_buys_one_request_where_it_was_meant() {
        let dir = tempfile::tempdir().unwrap();
        let issuer = IssuerKey::generate().unwrap();
        let config = Config {
            issuer: "issuer.test".into(),
            origin: "maker".into(),
            keys: vec![issuer.token_key().to_base64()],
            spent: dir.path().join("spent.sqlite"),
        };
        let app = |config: &Config| {
            let gate = Arc::new(Gate::open(config).unwrap());
            Router::new()
                .route("/v1/quote", post(|| async { "quoted" }))
                .route_layer(axum::middleware::from_fn_with_state(gate, require))
        };
        let send = |app: &Router, token: Option<&Token>| {
            let mut request = Request::post("/v1/quote");
            if let Some(token) = token {
                request = request.header(header::AUTHORIZATION, token.authorization());
            }
            app.clone().oneshot(request.body(Body::empty()).unwrap())
        };

        let maker = app(&config);
        let asked = send(&maker, None).await.unwrap();
        assert_eq!(asked.status(), StatusCode::UNAUTHORIZED);
        let challenge = asked.headers()[header::WWW_AUTHENTICATE].to_str().unwrap();
        let (challenge, key) = read_www_authenticate(challenge).unwrap();
        assert_eq!(key.id(), issuer.token_key().id());

        let paid = token(&issuer, &challenge);
        assert_eq!(
            send(&maker, Some(&paid)).await.unwrap().status(),
            StatusCode::OK
        );
        let again = send(&maker, Some(&paid)).await.unwrap().status();
        assert_eq!(again, StatusCode::UNAUTHORIZED);
        let restarted = app(&config);
        let again = send(&restarted, Some(&paid)).await.unwrap().status();
        assert_eq!(again, StatusCode::UNAUTHORIZED);

        let relayer = Challenge::new("issuer.test", "relayer").unwrap();
        let elsewhere = send(&restarted, Some(&token(&issuer, &relayer))).await;
        assert_eq!(elsewhere.unwrap().status(), StatusCode::UNAUTHORIZED);

        let unspent = token(&issuer, &challenge);
        let next = IssuerKey::generate().unwrap();
        let rotated = app(&Config {
            keys: vec![next.token_key().to_base64()],
            ..config.clone()
        });
        let retired = send(&rotated, Some(&unspent)).await.unwrap().status();
        assert_eq!(retired, StatusCode::UNAUTHORIZED);
        let current = send(&rotated, Some(&token(&next, &challenge))).await;
        assert_eq!(current.unwrap().status(), StatusCode::OK);
    }
}
