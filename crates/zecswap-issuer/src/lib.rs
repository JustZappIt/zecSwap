//! The token issuer: signs each genuine device up to a day's tokens, one per accept, blind
//! (`zecswap-tokens`). It learns which devices fetch tokens and how many, never which swaps they
//! pay for. Run it apart from the maker, and log nothing of who fetched what.

mod android;
mod x509;

use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::{SystemTime, UNIX_EPOCH};

use anyhow::{Context as _, Result, bail};
use axum::extract::State;
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Router, middleware};
use base64::Engine as _;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use ring::rand::{SecureRandom as _, SystemRandom};
use rusqlite::{Connection, OptionalExtension, params};
use serde::Deserialize;
use sha2::{Digest, Sha256};
use zecswap_api::server::{self, Json};
use zecswap_api::service::ErrorCode;
use zecswap_api::tokens::{self, AttestationChallenge, TokenKey, TokenRequests, TokenResponses};
use zecswap_tokens::IssuerKey;

pub use android::{AndroidKey, SecurityLevel};

/// The most tokens one request signs, whatever the day's allowance.
const MAX_BATCH: usize = 100;

#[derive(Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Config {
    pub listen: SocketAddr,
    /// The issuer every token's challenge names, as the maker's `[tokens]` does.
    pub name: String,
    /// The signing key: the PKCS #8 PEM `zecswap-issuer keygen` writes.
    pub key: PathBuf,
    /// Where each device's tokens for the day are counted.
    pub data_dir: PathBuf,
    pub tokens_per_day: u32,
    /// How a device proves it is a genuine install.
    pub attestation: Attestation,
    /// Lets `insecure-test` run, which believes any caller.
    #[serde(default)]
    pub allow_insecure: bool,
}

impl Config {
    pub fn load(path: &Path) -> Result<Self> {
        let text =
            std::fs::read_to_string(path).with_context(|| format!("reading {}", path.display()))?;
        toml::from_str(&text).with_context(|| format!("parsing {}", path.display()))
    }
}

#[derive(Clone, Debug, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum Attestation {
    /// Takes the first certificate's place in the chain for the device's id, unchecked: anyone
    /// can be any number of devices, so this limits nothing. For tests, and only with
    /// `allow_insecure`.
    InsecureTest,
    /// An Android install's key, attested by the phone's secure hardware up to Google's root.
    AndroidKey(AndroidKey),
}

/// Turns a device's attestation of a request into the id its allowance is counted under, or
/// refuses it.
pub trait Attester: Send + Sync {
    /// Takes note of a challenge given out at `now`, unless too many are outstanding.
    fn hold(&self, challenge: [u8; 32], now: u64) -> bool;
    fn device(
        &self,
        attestation: &tokens::Attestation,
        blinded: &[Vec<u8>],
        now: u64,
    ) -> Result<[u8; 32], &'static str>;
}

struct InsecureTest;

impl Attester for InsecureTest {
    fn hold(&self, _: [u8; 32], _: u64) -> bool {
        true
    }

    fn device(
        &self,
        attestation: &tokens::Attestation,
        _: &[Vec<u8>],
        _: u64,
    ) -> Result<[u8; 32], &'static str> {
        match attestation
            .chain
            .first()
            .map(|id| URL_SAFE_NO_PAD.decode(id))
        {
            Some(Ok(id)) if !id.is_empty() => Ok(Sha256::digest(id).into()),
            _ => Err("an empty attestation"),
        }
    }
}

#[derive(Debug, thiserror::Error)]
pub enum IssueError {
    #[error("{0}")]
    Invalid(String),
    #[error("the attestation is refused: {0}")]
    Refused(String),
    #[error("this device's tokens for today are spent")]
    Spent,
    #[error("too many challenges are outstanding: try again in a few minutes")]
    Busy,
    #[error(transparent)]
    Internal(#[from] anyhow::Error),
}

impl From<rusqlite::Error> for IssueError {
    fn from(e: rusqlite::Error) -> Self {
        Self::Internal(e.into())
    }
}

pub struct Issuer {
    config: Config,
    key: IssuerKey,
    attester: Box<dyn Attester>,
    issued: Mutex<Connection>,
    random: SystemRandom,
}

impl Issuer {
    pub fn new(config: Config) -> Result<Self> {
        let pem = std::fs::read_to_string(&config.key)
            .with_context(|| format!("reading {}", config.key.display()))?;
        let key = IssuerKey::from_pem(&pem).context("the signing key")?;
        std::fs::create_dir_all(&config.data_dir)?;
        let issued = Connection::open(config.data_dir.join("issued.sqlite"))?;
        issued.execute_batch(
            "CREATE TABLE IF NOT EXISTS issued (
                device BLOB NOT NULL, day INTEGER NOT NULL, count INTEGER NOT NULL,
                PRIMARY KEY (device, day)
            )",
        )?;
        let attester: Box<dyn Attester> = match &config.attestation {
            Attestation::InsecureTest if !config.allow_insecure => {
                bail!("insecure-test attestation believes any caller: set allow_insecure to run it")
            }
            Attestation::InsecureTest => {
                tracing::warn!("insecure-test attestation: any caller can claim any device");
                Box::new(InsecureTest)
            }
            Attestation::AndroidKey(android) => {
                Box::new(android::AndroidAttester::new(android, &config.name)?)
            }
        };
        Ok(Self {
            config,
            key,
            attester,
            issued: Mutex::new(issued),
            random: SystemRandom::new(),
        })
    }

    pub fn listen(&self) -> SocketAddr {
        self.config.listen
    }

    pub fn token_key(&self) -> TokenKey {
        TokenKey {
            issuer: self.config.name.clone(),
            token_key: self.key.token_key().to_base64(),
            tokens_per_day: self.config.tokens_per_day,
        }
    }

    /// A challenge for the next request to sign, good once for five minutes from `now`.
    pub fn challenge(&self, now: u64) -> Result<AttestationChallenge, IssueError> {
        let mut challenge = [0; 32];
        self.random
            .fill(&mut challenge)
            .map_err(|_| anyhow::anyhow!("the system's random source failed"))?;
        if !self.attester.hold(challenge, now) {
            return Err(IssueError::Busy);
        }
        Ok(AttestationChallenge {
            challenge: URL_SAFE_NO_PAD.encode(challenge),
        })
    }

    /// Signs as many of the requests, in order, as the device's allowance for the day at `now`
    /// has left, if its attestation of them is good.
    pub fn issue(&self, request: &TokenRequests, now: u64) -> Result<TokenResponses, IssueError> {
        let invalid = |what: &str| IssueError::Invalid(what.into());
        let mut blinded = request
            .blinded
            .iter()
            .map(|blinded| URL_SAFE_NO_PAD.decode(blinded))
            .collect::<Result<Vec<_>, _>>()
            .map_err(|_| invalid("a blinded request that is not base64url"))?;
        // Checked before counting: one the key can't sign would cost the device its allowance.
        if !(1..=MAX_BATCH).contains(&blinded.len())
            || blinded
                .iter()
                .any(|blinded| self.key.token_key().check_request(blinded).is_err())
        {
            return Err(invalid(
                "between 1 and 100 blinded requests of 256 bytes each",
            ));
        }
        let device = self
            .attester
            .device(&request.attestation, &blinded, now)
            .map_err(|refused| IssueError::Refused(refused.into()))?;
        let day = zecswap_tokens::day(now);
        {
            let mut issued = self.issued.lock().unwrap();
            let tx = issued.transaction()?;
            let before: u32 = tx
                .query_row(
                    "SELECT count FROM issued WHERE device = ?1 AND day = ?2",
                    params![device, day],
                    |row| row.get(0),
                )
                .optional()?
                .unwrap_or(0);
            let left = self.config.tokens_per_day.saturating_sub(before) as usize;
            if left == 0 {
                return Err(IssueError::Spent);
            }
            blinded.truncate(left);
            let count = before + blinded.len() as u32;
            tx.execute(
                "INSERT INTO issued (device, day, count) VALUES (?1, ?2, ?3)
                 ON CONFLICT (device, day) DO UPDATE SET count = ?3",
                params![device, day, count],
            )?;
            // Earlier days limit nothing any more: keep no record of them.
            tx.execute("DELETE FROM issued WHERE day < ?1", [day])?;
            tx.commit()?;
        }
        let blind_signatures = blinded
            .iter()
            .map(|blinded| Ok(URL_SAFE_NO_PAD.encode(self.key.sign(blinded)?)))
            .collect::<Result<_>>()
            .map_err(|e| IssueError::Invalid(e.to_string()))?;
        Ok(TokenResponses { blind_signatures })
    }
}

impl IntoResponse for IssueError {
    fn into_response(self) -> Response {
        let (status, code) = match &self {
            IssueError::Invalid(_) => (StatusCode::BAD_REQUEST, ErrorCode::InvalidRequest),
            IssueError::Refused(_) => (StatusCode::FORBIDDEN, ErrorCode::Rejected),
            IssueError::Spent => (StatusCode::TOO_MANY_REQUESTS, ErrorCode::Unavailable),
            IssueError::Busy => (StatusCode::SERVICE_UNAVAILABLE, ErrorCode::Unavailable),
            IssueError::Internal(e) => {
                tracing::error!("{e:#}");
                (StatusCode::INTERNAL_SERVER_ERROR, ErrorCode::Internal)
            }
        };
        server::error(status, code, self.to_string())
    }
}

pub fn router(issuer: Arc<Issuer>) -> Router {
    Router::new()
        .route("/v1/token-key", get(token_key))
        .route("/v1/challenge", get(challenge))
        .route("/v1/tokens", post(tokens))
        .fallback(server::not_found)
        .method_not_allowed_fallback(server::method_not_allowed)
        .layer(middleware::from_fn(server::no_store))
        .with_state(issuer)
}

async fn token_key(State(issuer): State<Arc<Issuer>>) -> Json<TokenKey> {
    Json(issuer.token_key())
}

async fn challenge(
    State(issuer): State<Arc<Issuer>>,
) -> Result<Json<AttestationChallenge>, IssueError> {
    Ok(Json(issuer.challenge(now())?))
}

async fn tokens(
    State(issuer): State<Arc<Issuer>>,
    Json(request): Json<TokenRequests>,
) -> Result<Json<TokenResponses>, IssueError> {
    Ok(Json(issuer.issue(&request, now())?))
}

fn now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("clock is after 1970")
        .as_secs()
}

#[cfg(test)]
mod tests {
    use zecswap_tokens::{Challenge, Pending, TokenKey as Key};

    use super::*;

    /// `insecure-test` believes any caller, so it runs only when its config says so outright.
    #[test]
    fn insecure_test_runs_only_when_allowed() {
        let dir = tempfile::tempdir().unwrap();
        let key = IssuerKey::generate().unwrap().to_pem().unwrap();
        std::fs::write(dir.path().join("issuer.pem"), key).unwrap();
        let config = |allow: &str| -> Config {
            let text = format!(
                r#"
                listen = "127.0.0.1:0"
                name = "issuer.test"
                key = "{dir}/issuer.pem"
                data_dir = "{dir}"
                tokens_per_day = 3
                attestation = "insecure-test"
                {allow}
                "#,
                dir = dir.path().display(),
            );
            toml::from_str(&text).unwrap()
        };
        let refused = Issuer::new(config("")).err().unwrap();
        assert!(refused.to_string().contains("allow_insecure"), "{refused}");
        assert!(Issuer::new(config("allow_insecure = true")).is_ok());
    }

    /// A device gets its day's allowance and no more, however it splits its requests, and a
    /// request for more than is left gets what is left; another device, and the next day, start
    /// afresh; and every token it gets spends at a service.
    #[test]
    fn each_device_gets_its_days_tokens_and_no_more() {
        let dir = tempfile::tempdir().unwrap();
        let key = dir.path().join("issuer.pem");
        std::fs::write(&key, IssuerKey::generate().unwrap().to_pem().unwrap()).unwrap();
        let issuer = Issuer::new(Config {
            listen: "127.0.0.1:0".parse().unwrap(),
            name: "issuer.test".into(),
            key,
            data_dir: dir.path().into(),
            tokens_per_day: 3,
            attestation: Attestation::InsecureTest,
            allow_insecure: true,
        })
        .unwrap();
        let token_key = Key::from_base64(&issuer.token_key().token_key).unwrap();
        let maker = Challenge::new("issuer.test", "maker", 20_000).unwrap();
        // How many of `count` tokens the device gets, each checked to spend at a service.
        let ask = |device: &str, count: usize, now: u64| {
            let (pending, blinded): (Vec<Pending>, Vec<String>) = (0..count)
                .map(|_| {
                    let (pending, blinded) = Pending::new(&token_key, &maker).unwrap();
                    (pending, URL_SAFE_NO_PAD.encode(blinded))
                })
                .unzip();
            let request = TokenRequests {
                attestation: tokens::Attestation {
                    challenge: String::new(),
                    chain: vec![URL_SAFE_NO_PAD.encode(device)],
                    signature: String::new(),
                },
                blinded,
            };
            issuer.issue(&request, now).map(|issued| {
                let granted = issued.blind_signatures.len();
                for (pending, signature) in pending.into_iter().zip(issued.blind_signatures) {
                    let signature = URL_SAFE_NO_PAD.decode(signature).unwrap();
                    let token = pending.finalize(&token_key, &signature).unwrap();
                    token.verify(&token_key, &maker).unwrap();
                }
                granted
            })
        };
        const DAY: u64 = 24 * 60 * 60;
        let today = 20_000 * DAY;
        assert_eq!(ask("phone a", 2, today).unwrap(), 2);
        assert_eq!(
            ask("phone a", 2, today).unwrap(),
            1,
            "what the day had left"
        );
        let spent = ask("phone a", 1, today + DAY - 1);
        assert!(matches!(spent, Err(IssueError::Spent)));
        assert_eq!(ask("phone b", 3, today).unwrap(), 3);
        assert_eq!(ask("phone a", 3, today + DAY).unwrap(), 3, "the next day");
        assert!(matches!(ask("", 1, today), Err(IssueError::Refused(_))));
    }
}
