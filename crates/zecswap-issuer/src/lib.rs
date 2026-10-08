//! The token issuer: signs each genuine device up to a day's tokens, one per accept, blind
//! (`zecswap-tokens`). It learns which devices fetch tokens and how many, never which swaps they
//! pay for. Run it apart from the maker, and log nothing of who fetched what.

mod android;
mod x509;

use std::collections::BTreeMap;
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
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use zecswap_api::server::{self, Json, MonitorToken};
use zecswap_api::service::ErrorCode;
use zecswap_api::tokens::{self, AttestationChallenge, TokenKey, TokenRequests, TokenResponses};
use zecswap_tokens::{DAY, IssuerKey, day};

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

    /// The certificate status list in force, if the attester checks one.
    fn status_list(&self) -> Option<StatusList> {
        None
    }
}

/// An attestation status list in force: when the file it was read from was written, and how
/// many certificates it lists.
#[derive(Clone, Copy, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct StatusList {
    pub modified_at: u64,
    pub entries: u64,
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
    Refused(&'static str),
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
    monitor: Monitor,
}

struct Monitor {
    token: Arc<MonitorToken>,
    started_at: u64,
    requests: Mutex<Requests>,
}

/// What the issuer exports for monitoring: the day's totals and what it refused, by why. Nothing
/// in it tells one device from another, or says when any one request came.
#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub struct MonitorSnapshot {
    schema_version: u32,
    generated_at: u64,
    uptime_seconds: u64,
    issuer: String,
    tokens_per_day: u32,
    attestation: &'static str,
    status_list: Option<StatusList>,
    today: Today,
    requests: Requests,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct Today {
    day: u64,
    devices: u64,
    tokens_issued: u64,
    devices_at_limit: u64,
}

/// Requests since `since`: the issuer's start or 00:00 UTC, whichever came later.
#[derive(Clone, Default, Serialize)]
#[serde(rename_all = "camelCase")]
struct Requests {
    #[serde(skip)]
    day: u64,
    since: u64,
    challenges: u64,
    busy: u64,
    granted: u64,
    /// Granted fewer tokens than asked: the day's allowance ran out.
    partial: u64,
    exhausted: u64,
    invalid: u64,
    refused: BTreeMap<&'static str, u64>,
}

impl Requests {
    fn starting(now: u64) -> Self {
        Self {
            day: day(now),
            since: now,
            ..Self::default()
        }
    }

    /// Counts afresh from midnight on.
    fn on(&mut self, now: u64) -> &mut Self {
        if day(now) != self.day {
            *self = Self::starting(day(now) * DAY);
        }
        self
    }
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
        let started_at = now();
        Ok(Self {
            config,
            key,
            attester,
            issued: Mutex::new(issued),
            random: SystemRandom::new(),
            monitor: Monitor {
                token: Arc::new(MonitorToken::from_env("ISSUER_MONITOR_TOKEN")?),
                started_at,
                requests: Mutex::new(Requests::starting(started_at)),
            },
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
            self.count(now, |requests| requests.busy += 1);
            return Err(IssueError::Busy);
        }
        self.count(now, |requests| requests.challenges += 1);
        Ok(AttestationChallenge {
            challenge: URL_SAFE_NO_PAD.encode(challenge),
        })
    }

    /// Signs as many of the requests, in order, as the device's allowance for the day at `now`
    /// has left, if its attestation of them is good.
    pub fn issue(&self, request: &TokenRequests, now: u64) -> Result<TokenResponses, IssueError> {
        let issued = self.sign(request, now);
        self.count(now, |requests| match &issued {
            Ok(responses) => {
                requests.granted += 1;
                if responses.blind_signatures.len() < request.blinded.len() {
                    requests.partial += 1;
                }
            }
            Err(IssueError::Invalid(_)) => requests.invalid += 1,
            Err(IssueError::Refused(why)) => *requests.refused.entry(why).or_default() += 1,
            Err(IssueError::Spent) => requests.exhausted += 1,
            Err(IssueError::Busy | IssueError::Internal(_)) => {}
        });
        issued
    }

    /// What the monitor shows at `now`.
    pub fn monitor(&self, now: u64) -> Result<MonitorSnapshot, IssueError> {
        let requests = self.monitor.requests.lock().unwrap().on(now).clone();
        let today = day(now);
        let (devices, tokens_issued, devices_at_limit) = self.issued.lock().unwrap().query_row(
            "SELECT count(*), coalesce(sum(count), 0), coalesce(sum(count >= ?2), 0)
             FROM issued WHERE day = ?1",
            params![today, self.config.tokens_per_day],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
        )?;
        Ok(MonitorSnapshot {
            schema_version: 1,
            generated_at: now,
            uptime_seconds: now.saturating_sub(self.monitor.started_at),
            issuer: self.config.name.clone(),
            tokens_per_day: self.config.tokens_per_day,
            attestation: match self.config.attestation {
                Attestation::InsecureTest => "insecure-test",
                Attestation::AndroidKey(_) => "android-key",
            },
            status_list: self.attester.status_list(),
            today: Today {
                day: today,
                devices,
                tokens_issued,
                devices_at_limit,
            },
            requests,
        })
    }

    fn count(&self, now: u64, record: impl FnOnce(&mut Requests)) {
        record(self.monitor.requests.lock().unwrap().on(now));
    }

    fn sign(&self, request: &TokenRequests, now: u64) -> Result<TokenResponses, IssueError> {
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
            .map_err(IssueError::Refused)?;
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
    let monitor = Router::new()
        .route("/v1/monitor", get(monitor))
        .route_layer(middleware::from_fn_with_state(
            issuer.monitor.token.clone(),
            server::require_monitor,
        ));
    Router::new()
        .route("/v1/token-key", get(token_key))
        .route("/v1/challenge", get(challenge))
        .route("/v1/tokens", post(tokens))
        .merge(monitor)
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

async fn monitor(State(issuer): State<Arc<Issuer>>) -> Result<Json<MonitorSnapshot>, IssueError> {
    Ok(Json(issuer.monitor(now())?))
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

    /// An issuer in `dir` that takes any device's word for who it is, three tokens a day.
    fn insecure(dir: &Path) -> Issuer {
        let key = dir.join("issuer.pem");
        std::fs::write(&key, IssuerKey::generate().unwrap().to_pem().unwrap()).unwrap();
        Issuer::new(Config {
            listen: "127.0.0.1:0".parse().unwrap(),
            name: "issuer.test".into(),
            key,
            data_dir: dir.into(),
            tokens_per_day: 3,
            attestation: Attestation::InsecureTest,
            allow_insecure: true,
        })
        .unwrap()
    }

    /// How many of `count` tokens `device` gets at `now`, each checked to spend at a service.
    fn ask(issuer: &Issuer, device: &str, count: usize, now: u64) -> Result<usize, IssueError> {
        let token_key = Key::from_base64(&issuer.token_key().token_key).unwrap();
        let maker = Challenge::new("issuer.test", "maker", 20_000).unwrap();
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
    }

    /// A device gets its day's allowance and no more, however it splits its requests, and a
    /// request for more than is left gets what is left; another device, and the next day, start
    /// afresh; and every token it gets spends at a service.
    #[test]
    fn each_device_gets_its_days_tokens_and_no_more() {
        let dir = tempfile::tempdir().unwrap();
        let issuer = insecure(dir.path());
        let today = 20_000 * DAY;
        assert_eq!(ask(&issuer, "phone a", 2, today).unwrap(), 2);
        assert_eq!(
            ask(&issuer, "phone a", 2, today).unwrap(),
            1,
            "what the day had left"
        );
        let spent = ask(&issuer, "phone a", 1, today + DAY - 1);
        assert!(matches!(spent, Err(IssueError::Spent)));
        assert_eq!(ask(&issuer, "phone b", 3, today).unwrap(), 3);
        assert_eq!(
            ask(&issuer, "phone a", 3, today + DAY).unwrap(),
            3,
            "the next day"
        );
        assert!(matches!(
            ask(&issuer, "", 1, today),
            Err(IssueError::Refused(_))
        ));
    }

    /// The monitor answers its bearer token alone, and nobody while none is set.
    #[tokio::test]
    async fn the_monitor_takes_only_its_token() {
        use axum::body::Body;
        use axum::http::Request;
        use tower::ServiceExt as _;

        let status = |issuer: Issuer, token: Option<&'static str>| async move {
            let mut request = Request::get("/v1/monitor");
            if let Some(token) = token {
                request = request.header("authorization", format!("Bearer {token}"));
            }
            let response = router(Arc::new(issuer))
                .oneshot(request.body(Body::empty()).unwrap())
                .await
                .unwrap();
            response.status()
        };
        let dir = tempfile::tempdir().unwrap();
        let closed = status(insecure(dir.path()), Some("anything")).await;
        assert_eq!(closed, StatusCode::UNAUTHORIZED);
        for (token, expected) in [
            (None, StatusCode::UNAUTHORIZED),
            (Some("wrong-token"), StatusCode::UNAUTHORIZED),
            (Some("monitor-test-token"), StatusCode::OK),
        ] {
            let dir = tempfile::tempdir().unwrap();
            let mut issuer = insecure(dir.path());
            issuer.monitor.token = Arc::new(MonitorToken::new(Some("monitor-test-token")));
            assert_eq!(status(issuer, token).await, expected, "{token:?}");
        }
    }

    /// The monitor shows the day's totals and what the issuer refused, by why, and nothing that
    /// tells one device from another; its counts start afresh the next day.
    #[test]
    fn the_monitor_shows_the_days_totals_and_no_device() {
        let dir = tempfile::tempdir().unwrap();
        let issuer = insecure(dir.path());
        let today = 20_000 * DAY;
        issuer.challenge(today).unwrap();
        assert_eq!(ask(&issuer, "phone a", 2, today).unwrap(), 2);
        assert_eq!(ask(&issuer, "phone a", 2, today).unwrap(), 1);
        let spent = ask(&issuer, "phone a", 1, today);
        assert!(matches!(spent, Err(IssueError::Spent)));
        assert_eq!(ask(&issuer, "phone b", 1, today).unwrap(), 1);
        assert!(matches!(
            ask(&issuer, "", 1, today),
            Err(IssueError::Refused(_))
        ));
        let empty = ask(&issuer, "phone c", 0, today);
        assert!(matches!(empty, Err(IssueError::Invalid(_))));

        let shown = serde_json::to_value(issuer.monitor(today + 60).unwrap()).unwrap();
        assert_eq!(shown["attestation"], "insecure-test");
        assert!(shown["statusList"].is_null());
        assert_eq!(
            shown["today"],
            serde_json::json!({"day": 20_000, "devices": 2, "tokensIssued": 4, "devicesAtLimit": 1})
        );
        assert_eq!(
            shown["requests"],
            serde_json::json!({
                "since": today, "challenges": 1, "busy": 0, "granted": 3, "partial": 1,
                "exhausted": 1, "invalid": 1, "refused": {"an empty attestation": 1},
            })
        );
        let text = shown.to_string();
        for device in ["phone a", "phone b"] {
            assert!(
                !text.contains(&hex::encode(Sha256::digest(device))),
                "{device}"
            );
            assert!(!text.contains(&URL_SAFE_NO_PAD.encode(device)), "{device}");
        }
        let tomorrow = serde_json::to_value(issuer.monitor(today + DAY).unwrap()).unwrap();
        assert_eq!(tomorrow["today"]["devices"], 0);
        assert_eq!(tomorrow["requests"]["granted"], 0);
        assert_eq!(tomorrow["requests"]["since"], today + DAY);
    }
}
