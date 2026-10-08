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
    /// An accept spends one, which the maker hands back once the user pays in: how many swaps
    /// a device may walk away from a day. Resets at 00:00 UTC.
    pub tokens_per_day: u32,
}

/// `GET /v1/challenge`: what the next `POST /v1/tokens` signs. Good for one request, within
/// five minutes.
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct AttestationChallenge {
    /// 32 random bytes, base64url.
    pub challenge: String,
}

/// `POST /v1/tokens`: blinded token requests for the device its attestation vouches for.
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct TokenRequests {
    pub attestation: Attestation,
    /// RFC 9578 `blinded_msg`s, base64url.
    pub blinded: Vec<String>,
}

/// An install's key, attested by the phone's secure hardware, signing the request it comes
/// with. The app makes the key once, an EC P-256 key in the Android Keystore whose
/// attestation challenge is `SHA-256("zecswap-issuer-v1" ‖ issuer)`, `issuer` being the
/// name `GET /v1/token-key` gives (UTF-8), and keeps it: the issuer counts tokens by key.
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct Attestation {
    /// The challenge from `GET /v1/challenge`, base64url, as given.
    pub challenge: String,
    /// The key's certificate chain from the Keystore, leaf first, each DER, base64url.
    pub chain: Vec<String>,
    /// `SHA256withECDSA` by the key, DER, base64url, over
    /// `"zecswap-issuer-v1" ‖ challenge ‖ SHA-256(blinded)`: the challenge's 32 bytes, and
    /// the request's blinded messages decoded and concatenated in order.
    pub signature: String,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct TokenResponses {
    /// RFC 9578 `blind_sig`s, base64url, for the first requests in order: as many as the
    /// device's allowance for the day had left.
    pub blind_signatures: Vec<String>,
}
