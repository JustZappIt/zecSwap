//! Spends a Privacy Pass token whenever a service asks for one (the maker, for each accept),
//! fetching a batch from the issuer when none is held. The issuer signs them blind, so neither it nor the services can
//! tell which of this device's tokens paid for which request.

use std::collections::HashMap;
use std::sync::Mutex;

use anyhow::{Context as _, Result, ensure};
use base64::Engine as _;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use zecswap_api::tokens::{TokenKey, TokenRequests, TokenResponses};
use zecswap_tokens::{Pending, Token, read_www_authenticate};

/// Unspent tokens, by the challenge and key they answer.
type Held = HashMap<(Vec<u8>, [u8; 32]), Vec<Token>>;

pub struct Tokens {
    issuer: String,
    attestation: Vec<u8>,
    batch: usize,
    http: reqwest::Client,
    held: Mutex<Held>,
}

impl Tokens {
    /// Fetches `batch` tokens at a time from the issuer API at `issuer`, for the device
    /// `attestation` vouches for.
    pub fn new(issuer: impl Into<String>, attestation: Vec<u8>, batch: usize) -> Result<Self> {
        ensure!(batch > 0, "a token batch of none");
        Ok(Self {
            issuer: issuer.into(),
            attestation,
            batch,
            http: reqwest::Client::builder()
                .timeout(std::time::Duration::from_secs(30))
                .build()?,
            held: Mutex::new(HashMap::new()),
        })
    }

    /// A token for what a service's `WWW-Authenticate` asks.
    pub(crate) async fn take(&self, asked: &str) -> Result<Token> {
        let (challenge, key) = read_www_authenticate(asked)?;
        let held = (challenge.encode(), key.id());
        if let Some(token) = self.held.lock().unwrap().get_mut(&held).and_then(Vec::pop) {
            return Ok(token);
        }
        // A key only some devices were asked to use would mark their tokens: take only the one
        // the issuer publishes to everyone.
        let published: TokenKey = self
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
        let (pending, blinded): (Vec<Pending>, Vec<String>) = (0..self.batch)
            .map(|_| {
                let (pending, blinded) = Pending::new(&key, &challenge)?;
                Ok((pending, URL_SAFE_NO_PAD.encode(blinded)))
            })
            .collect::<Result<Vec<_>>>()?
            .into_iter()
            .unzip();
        let request = TokenRequests {
            attestation: URL_SAFE_NO_PAD.encode(&self.attestation),
            blinded,
        };
        let response = self
            .http
            .post(format!("{}/v1/tokens", self.issuer))
            .json(&request)
            .send()
            .await?;
        let status = response.status();
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
            .map(|(pending, signature)| pending.finalize(&key, &URL_SAFE_NO_PAD.decode(signature)?))
            .collect::<Result<Vec<_>>>()
            .context("the issuer's signatures")?;
        let token = tokens.pop().expect("a batch of at least one");
        // Added to, not replaced: another request may have fetched a batch meanwhile.
        self.held
            .lock()
            .unwrap()
            .entry(held)
            .or_default()
            .extend(tokens);
        Ok(token)
    }
}
