//! Privacy Pass tokens for the maker's accepts (RFC 9578, type `0x0002`) over plain bytes: the
//! challenge a maker sends, the blinded requests the issuer signs, and the tokens they become.
//! Stateless: a request's client half travels as `Pending::to_bytes` between calls.

use zecswap_tokens::{Challenge, Pending, Token, TokenKey, read_www_authenticate};
use zeroize::Zeroizing;

use crate::ops::Result;

/// What a `WWW-Authenticate: PrivateToken` header asks for: the encoded challenge, the token key
/// (SPKI) it names, and the issuer it names.
pub fn read_challenge(header: &str) -> Result<[Vec<u8>; 3]> {
    let (challenge, key) = read_www_authenticate(header).map_err(error)?;
    Ok([
        challenge.encode(),
        key.spki().to_vec(),
        challenge.issuer().as_bytes().to_vec(),
    ])
}

/// A fresh token request for `challenge` under `token_key`: the 256-byte blinded message for the
/// issuer, then the client's half that finalizes its answer.
pub fn blind(token_key: &[u8], challenge: &[u8]) -> Result<Zeroizing<Vec<u8>>> {
    let key = TokenKey::from_spki(token_key).map_err(error)?;
    let challenge = Challenge::decode(challenge).map_err(error)?;
    let (pending, blinded) = Pending::new(&key, &challenge).map_err(error)?;
    let pending = Zeroizing::new(pending.to_bytes());
    Ok(Zeroizing::new([&blinded[..], &pending[..]].concat()))
}

/// The 354-byte token `pending` becomes, once `blind_signature` unblinds to a valid signature
/// under `token_key`.
pub fn finalize(pending: &[u8], token_key: &[u8], blind_signature: &[u8]) -> Result<Vec<u8>> {
    let key = TokenKey::from_spki(token_key).map_err(error)?;
    let token = Pending::from_bytes(pending)
        .map_err(error)?
        .finalize(&key, blind_signature)
        .map_err(error)?;
    Ok(token.encode())
}

/// The `Authorization` header value that spends `token`.
pub fn authorization(token: &[u8]) -> Result<String> {
    Ok(Token::decode(token).map_err(error)?.authorization())
}

fn error(e: impl core::fmt::Display) -> String {
    e.to_string()
}
