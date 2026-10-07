//! The token issuer's API. A device proves it is a genuine install and gets the day's tokens it
//! asked for, signed blind (`zecswap-tokens`): the issuer never sees them, so no one can tie a
//! spent token to the device that fetched it.

use serde::{Deserialize, Serialize};

/// `GET /v1/token-key`.
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct TokenKey {
    /// The name every token's challenge gives its issuer.
    pub issuer: String,
    /// The key tokens are signed with, base64url SPKI (RFC 9578). Wallets pin it: an issuer
    /// that gave some devices another key would recognise their tokens.
    pub token_key: String,
    /// One per swap: an accept spends one. Resets at 00:00 UTC.
    pub tokens_per_day: u32,
}

/// `POST /v1/tokens`: blinded token requests for the device its attestation vouches for.
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct TokenRequests {
    /// The device's attestation, base64url.
    pub attestation: String,
    /// RFC 9578 `blinded_msg`s, base64url.
    pub blinded: Vec<String>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct TokenResponses {
    /// RFC 9578 `blind_sig`s, base64url, for the first requests in order: as many as the
    /// device's allowance for the day had left.
    pub blind_signatures: Vec<String>,
}
