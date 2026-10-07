//! Privacy Pass tokens: limits per device that never say which device a request came from.
//!
//! An issuer that has checked a device's attestation signs a batch of tokens for it without
//! seeing them (RFC 9578's publicly verifiable type, RSA blind signatures), so it can't recognise
//! them when they are spent. A service spends one per request it can be spammed with, checks it
//! with the issuer's public key alone, and asks for one with RFC 9577's HTTP scheme.

#[cfg(feature = "server")]
pub mod server;

use anyhow::{Result, anyhow, ensure};
use base64::Engine as _;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use blind_rsa_signatures::{
    BlindMessage, BlindSignature, BlindingResult, DefaultRng, KeyPairSha384PSSDeterministic,
    PublicKeySha384PSSDeterministic, Secret, SecretKeySha384PSSDeterministic, Signature,
};
use rand::{Rng, rand_core::UnwrapErr, rngs::SysRng};
use sha2::{Digest, Sha256};

/// RFC 9578's publicly verifiable token type: RSABSSA-SHA384-PSS-Deterministic, 2048-bit keys.
pub const TOKEN_TYPE: u16 = 0x0002;
const KEY_BITS: usize = 2048;
/// An authenticator's length, and a blinded request's: the modulus's.
const NK: usize = KEY_BITS / 8;
/// What the issuer signs: `token_type`, `nonce`, `challenge_digest` and `token_key_id`.
const INPUT: usize = 2 + 32 + 32 + 32;
const DAY: u64 = 24 * 60 * 60;

/// The UTC day `unix` (seconds) falls on, counted from the Unix epoch: what a challenge's
/// redemption context names.
pub fn day(unix: u64) -> u64 {
    unix / DAY
}

/// The UTC day it is now.
pub fn today() -> u64 {
    day(std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .expect("clock is after 1970")
        .as_secs())
}

/// Who issues tokens, where they are spent and on which UTC day: RFC 9577's `TokenChallenge`.
/// Its redemption context is the day, so a token is good only on the day it is for, and the
/// same for everyone that day, so it says nothing of who fetched it.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct Challenge {
    issuer: String,
    origin: String,
    day: u64,
}

impl Challenge {
    pub fn new(issuer: &str, origin: &str, day: u64) -> Result<Self> {
        ensure!(
            (1..=usize::from(u16::MAX)).contains(&issuer.len())
                && origin.len() <= usize::from(u16::MAX),
            "an issuer or origin name of the wrong length"
        );
        Ok(Self {
            issuer: issuer.into(),
            origin: origin.into(),
            day,
        })
    }

    pub fn issuer(&self) -> &str {
        &self.issuer
    }

    pub fn day(&self) -> u64 {
        self.day
    }

    /// The same issuer and service on another day.
    pub fn on(&self, day: u64) -> Self {
        Self {
            day,
            ..self.clone()
        }
    }

    /// The redemption context is 32 bytes: the day as a big-endian `u64` in the last 8, zeros
    /// before it.
    pub fn encode(&self) -> Vec<u8> {
        let mut out = TOKEN_TYPE.to_be_bytes().to_vec();
        out.extend((self.issuer.len() as u16).to_be_bytes());
        out.extend(self.issuer.as_bytes());
        out.push(32);
        out.extend([0; 24]);
        out.extend(self.day.to_be_bytes());
        out.extend((self.origin.len() as u16).to_be_bytes());
        out.extend(self.origin.as_bytes());
        out
    }

    /// Refuses any redemption context but a day: another would let a service tell apart the
    /// clients it gave different ones.
    pub fn decode(bytes: &[u8]) -> Result<Self> {
        let mut rest = bytes;
        ensure!(
            split(&mut rest, 2)? == TOKEN_TYPE.to_be_bytes(),
            "a challenge for another token type"
        );
        let issuer = name(&mut rest)?;
        ensure!(
            split(&mut rest, 1)? == [32] && split(&mut rest, 24)? == [0; 24],
            "a challenge whose redemption context is not a day"
        );
        let day = u64::from_be_bytes(split(&mut rest, 8)?.try_into().expect("8 bytes"));
        let origin = name(&mut rest)?;
        ensure!(rest.is_empty(), "a token challenge with trailing bytes");
        Self::new(&issuer, &origin, day)
    }

    fn digest(&self) -> [u8; 32] {
        Sha256::digest(self.encode()).into()
    }
}

/// An issuer's public key, which tokens name by the SHA-256 of its encoding.
#[derive(Clone, Debug)]
pub struct TokenKey {
    key: PublicKeySha384PSSDeterministic,
    spki: Vec<u8>,
}

impl TokenKey {
    /// From RFC 9578's encoding, an RSASSA-PSS SubjectPublicKeyInfo.
    pub fn from_spki(spki: &[u8]) -> Result<Self> {
        let key = PublicKeySha384PSSDeterministic::from_spki(spki).map_err(|e| anyhow!("{e}"))?;
        ensure!(
            key.components().n().len() == NK,
            "a token key of another size"
        );
        Ok(Self {
            key,
            spki: spki.to_vec(),
        })
    }

    pub fn from_base64(encoded: &str) -> Result<Self> {
        Self::from_spki(&URL_SAFE_NO_PAD.decode(encoded)?)
    }

    pub fn spki(&self) -> &[u8] {
        &self.spki
    }

    pub fn to_base64(&self) -> String {
        URL_SAFE_NO_PAD.encode(&self.spki)
    }

    /// `token_key_id`.
    pub fn id(&self) -> [u8; 32] {
        Sha256::digest(&self.spki).into()
    }

    /// Refuses a blinded request this key can't sign: one of another length, or not below the
    /// modulus.
    pub fn check_request(&self, blinded: &[u8]) -> Result<()> {
        ensure!(
            blinded.len() == NK && blinded < self.key.components().n().as_slice(),
            "a blinded token request this key cannot sign"
        );
        Ok(())
    }
}

/// The issuer's signing key.
pub struct IssuerKey {
    secret: SecretKeySha384PSSDeterministic,
    public: TokenKey,
}

impl IssuerKey {
    pub fn generate() -> Result<Self> {
        let pair = KeyPairSha384PSSDeterministic::generate(&mut DefaultRng, KEY_BITS)
            .map_err(|e| anyhow!("{e}"))?;
        Self::from_secret(pair.sk)
    }

    /// From a PKCS #8 PEM, as RFC 9578's test vectors give keys.
    pub fn from_pem(pem: &str) -> Result<Self> {
        Self::from_secret(
            SecretKeySha384PSSDeterministic::from_pem(pem).map_err(|e| anyhow!("{e}"))?,
        )
    }

    pub fn to_pem(&self) -> Result<String> {
        self.secret.to_pem().map_err(|e| anyhow!("{e}"))
    }

    fn from_secret(secret: SecretKeySha384PSSDeterministic) -> Result<Self> {
        let key = secret.public_key().map_err(|e| anyhow!("{e}"))?;
        let public = TokenKey::from_spki(&key.to_spki().map_err(|e| anyhow!("{e}"))?)?;
        Ok(Self { secret, public })
    }

    pub fn token_key(&self) -> &TokenKey {
        &self.public
    }

    /// Signs a blinded token request without learning the token it will become.
    pub fn sign(&self, blinded: &[u8]) -> Result<Vec<u8>> {
        Ok(self
            .secret
            .blind_sign(blinded)
            .map_err(|e| anyhow!("{e}"))?
            .0)
    }
}

/// A token on its way: the client's half, kept while the issuer signs the blinded half.
pub struct Pending {
    input: [u8; INPUT],
    blinding: BlindingResult,
}

impl Pending {
    /// Starts a token for `challenge` under `key`, and the blinded request for the issuer.
    pub fn new(key: &TokenKey, challenge: &Challenge) -> Result<(Self, Vec<u8>)> {
        let mut nonce = [0; 32];
        UnwrapErr(SysRng).fill_bytes(&mut nonce);
        let input = input(&nonce, &challenge.digest(), &key.id());
        let blinding = key
            .key
            .blind(&mut DefaultRng, input)
            .map_err(|e| anyhow!("{e}"))?;
        let blinded = blinding.blind_message.0.clone();
        Ok((Self { input, blinding }, blinded))
    }

    /// `token_input ‖ blinding secret ‖ blinded message`, for a client that keeps no state between
    /// calls. Whoever holds them can tie the token to its request, so they never leave the device.
    pub fn to_bytes(&self) -> Vec<u8> {
        [
            &self.input[..],
            &self.blinding.secret.0,
            &self.blinding.blind_message.0,
        ]
        .concat()
    }

    pub fn from_bytes(bytes: &[u8]) -> Result<Self> {
        ensure!(
            bytes.len() == INPUT + 2 * NK,
            "a pending token of {} bytes",
            bytes.len()
        );
        ensure!(
            bytes[..2] == TOKEN_TYPE.to_be_bytes(),
            "a pending token of another type"
        );
        let (input, rest) = bytes.split_at(INPUT);
        let (secret, blinded) = rest.split_at(NK);
        Ok(Self {
            input: input.try_into().expect("the input's length"),
            // The deterministic variant has no message randomizer.
            blinding: BlindingResult {
                blind_message: BlindMessage(blinded.to_vec()),
                secret: Secret(secret.to_vec()),
                msg_randomizer: None,
            },
        })
    }

    /// The token, once the issuer's blind signature unblinds to a valid one under `key`.
    pub fn finalize(self, key: &TokenKey, blind_signature: &[u8]) -> Result<Token> {
        let signature = key
            .key
            .finalize(
                &BlindSignature(blind_signature.to_vec()),
                &self.blinding,
                self.input,
            )
            .map_err(|_| anyhow!("the issuer's signature does not finalize"))?;
        Ok(Token {
            input: self.input,
            authenticator: signature.0,
        })
    }
}

/// A token to spend: what the issuer signed, and the signature.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Token {
    input: [u8; INPUT],
    authenticator: Vec<u8>,
}

impl Token {
    pub fn decode(bytes: &[u8]) -> Result<Self> {
        ensure!(
            bytes.len() == INPUT + NK,
            "a token of {} bytes",
            bytes.len()
        );
        ensure!(
            bytes[..2] == TOKEN_TYPE.to_be_bytes(),
            "a token of another type"
        );
        let (input, authenticator) = bytes.split_at(INPUT);
        Ok(Self {
            input: input.try_into().expect("the input's length"),
            authenticator: authenticator.to_vec(),
        })
    }

    pub fn encode(&self) -> Vec<u8> {
        [&self.input[..], &self.authenticator].concat()
    }

    /// What a service records to accept the token once.
    pub fn nonce(&self) -> [u8; 32] {
        self.input[2..34].try_into().expect("32 bytes")
    }

    pub fn key_id(&self) -> [u8; 32] {
        self.input[66..].try_into().expect("32 bytes")
    }

    /// Whether `key` signed this token for `challenge`.
    pub fn verify(&self, key: &TokenKey, challenge: &Challenge) -> Result<()> {
        self.verify_digest(key, &challenge.digest())
    }

    fn verify_digest(&self, key: &TokenKey, challenge: &[u8; 32]) -> Result<()> {
        ensure!(
            self.input[34..66] == challenge[..],
            "a token for another service or day"
        );
        ensure!(self.key_id() == key.id(), "a token under another key");
        key.key
            .verify(&Signature(self.authenticator.clone()), None, self.input)
            .map_err(|_| anyhow!("a token its key did not sign"))
    }

    /// The `Authorization` header that spends it.
    pub fn authorization(&self) -> String {
        format!(
            "PrivateToken token=\"{}\"",
            URL_SAFE_NO_PAD.encode(self.encode())
        )
    }

    pub fn from_authorization(header: &str) -> Result<Self> {
        let params = params(header)?;
        let token = params
            .iter()
            .find(|(name, _)| name.eq_ignore_ascii_case("token"))
            .ok_or_else(|| anyhow!("no token in the authorization"))?;
        Self::decode(&URL_SAFE_NO_PAD.decode(token.1)?)
    }
}

/// The `WWW-Authenticate` header asking for a token: the challenge, and the key it must be
/// signed with.
pub fn www_authenticate(challenge: &Challenge, key: &TokenKey) -> String {
    format!(
        "PrivateToken challenge=\"{}\", token-key=\"{}\"",
        URL_SAFE_NO_PAD.encode(challenge.encode()),
        key.to_base64()
    )
}

/// What a `WWW-Authenticate` header asks for.
pub fn read_www_authenticate(header: &str) -> Result<(Challenge, TokenKey)> {
    let params = params(header)?;
    let param = |wanted: &str| {
        params
            .iter()
            .find(|(name, _)| name.eq_ignore_ascii_case(wanted))
            .map(|(_, value)| *value)
            .ok_or_else(|| anyhow!("no {wanted} in the token challenge"))
    };
    Ok((
        Challenge::decode(&URL_SAFE_NO_PAD.decode(param("challenge")?)?)?,
        TokenKey::from_base64(param("token-key")?)?,
    ))
}

/// The parameters of a `PrivateToken` header. Their values are base64url, so they hold no
/// commas or quotes.
fn params(header: &str) -> Result<Vec<(&str, &str)>> {
    let (scheme, params) = header.trim().split_once(' ').unwrap_or((header, ""));
    ensure!(
        scheme.eq_ignore_ascii_case("PrivateToken"),
        "not a PrivateToken header"
    );
    params
        .split(',')
        .filter(|param| !param.trim().is_empty())
        .map(|param| {
            let (name, value) = param
                .split_once('=')
                .ok_or_else(|| anyhow!("a malformed token parameter"))?;
            Ok((name.trim(), value.trim().trim_matches('"')))
        })
        .collect()
}

fn split<'a>(rest: &mut &'a [u8], n: usize) -> Result<&'a [u8]> {
    ensure!(rest.len() >= n, "a truncated token challenge");
    let (head, tail) = rest.split_at(n);
    *rest = tail;
    Ok(head)
}

fn name(rest: &mut &[u8]) -> Result<String> {
    let len = u16::from_be_bytes(split(rest, 2)?.try_into().expect("two bytes"));
    Ok(String::from_utf8(split(rest, len.into())?.to_vec())?)
}

fn input(nonce: &[u8; 32], challenge: &[u8; 32], key: &[u8; 32]) -> [u8; INPUT] {
    let mut input = [0; INPUT];
    input[..2].copy_from_slice(&TOKEN_TYPE.to_be_bytes());
    input[2..34].copy_from_slice(nonce);
    input[34..66].copy_from_slice(challenge);
    input[66..].copy_from_slice(key);
    input
}

#[cfg(test)]
mod tests {
    use super::*;

    /// RFC 9578, appendix A.2, test vector 2: a token another implementation issued verifies
    /// here. Its challenge has no redemption context, which ours always do, so it is checked by
    /// its digest.
    #[test]
    fn verifies_the_rfcs_token() {
        let key = TokenKey::from_spki(
            &hex::decode(concat!(
                "30820152303d06092a864886f70d01010a3030a00d300b0609608648016503040202a11a30180609",
                "2a864886f70d010108300b0609608648016503040202a2030201300382010f003082010a02820101",
                "00cb1aed6b6a95f5b1ce013a4cfcab25b94b2e64a23034e4250a7eab43c0df3a8c12993af12b1119",
                "08d4b471bec31d4b6c9ad9cdda90612a2ee903523e6de5a224d6b02f09e5c374d0cfe01d8f529c50",
                "0a78a2f67908fa682b5a2b430c81eaf1af72d7b5e794fc98a3139276879757ce453b526ef9bf6ceb",
                "99979b8423b90f4461a22af37aab0cf5733f7597abe44d31c732db68a181c6cbbe607d8c0e52e065",
                "5fd9996dc584eca0be87afbcd78a337d17b1dba9e828bbd81e291317144e7ff89f55619709b096cb",
                "b9ea474cead264c2073fe49740c01f00e109106066983d21e5f83f086e2e823c879cd43cef700d2a",
                "352a9babd612d03cad02db134b7e225a5f0203010001",
            ))
            .unwrap(),
        )
        .unwrap();
        let challenge = Sha256::digest(
            hex::decode("0002000e6973737565722e6578616d706c6500000e6f726967696e2e6578616d706c65")
                .unwrap(),
        )
        .into();
        let token = Token::decode(
            &hex::decode(concat!(
                "000298c1345ff38a554b429b428b0f206cfe4f3892f8041995f2c24873d90e84488d11e15c91a7c2",
                "ad02abd66645802373db1d823bea80f08d452541fb2b62b5898bca572f8982a9ca248a3056186322",
                "d93ca147266121ddeb5632c07f1f71cd27083350a206c5e9b7c0898f97611ce0bb8d74d310bb194a",
                "b67e094e32ff6da90886924b1b9e7b569402c1101d896d2fc3a7371ef77f02310db1dc9f81c85358",
                "28c2d0e9d9051720d182cd54e1c2c3bf417da2fc7aa72bb70ccc834ef274a2e809c9821b3d395d65",
                "35423f7428b3f29175d6eb840b4b7685336e57e2b6afeaabc0c17ea4f557e8a9cc2f624e245c6ccd",
                "7cbdd6c32c97c5c6974e802f688e2d25f0aba4215f609f692244517d5d3407e0172273982c001c15",
                "8f5fcbe1b5d2447c26a87e89f5a9e72b498b0c59ce749823d2cf253d3cf6cd4e64fa0e434d95e488",
                "789247a9ceed756ff4ff33a8d2402c0db381236d331092838b608a42002552092897",
            ))
            .unwrap(),
        )
        .unwrap();
        token.verify_digest(&key, &challenge).unwrap();
        assert_eq!(
            hex::encode(token.nonce()),
            "98c1345ff38a554b429b428b0f206cfe4f3892f8041995f2c24873d90e84488d"
        );
        let elsewhere = Challenge::new("issuer.example", "origin.example", 0).unwrap();
        assert!(token.verify(&key, &elsewhere).is_err());
    }

    /// The day sits in RFC 9577's 32-byte redemption context, big-endian in its last 8 bytes,
    /// as ports must encode it; a challenge with any other context, or none, is refused.
    #[test]
    fn challenges_name_their_day_and_nothing_else() {
        let challenge = Challenge::new("issuer.example", "origin.example", 20_000).unwrap();
        let encoded = challenge.encode();
        assert_eq!(
            hex::encode(&encoded),
            concat!(
                "0002000e6973737565722e6578616d706c6520",
                "0000000000000000000000000000000000000000000000000000000000004e20",
                "000e6f726967696e2e6578616d706c65"
            )
        );
        assert_eq!(Challenge::decode(&encoded).unwrap(), challenge);
        let mut marked = encoded.clone();
        marked[25] = 1;
        assert!(Challenge::decode(&marked).is_err());
        let none =
            hex::decode("0002000e6973737565722e6578616d706c6500000e6f726967696e2e6578616d706c65");
        assert!(Challenge::decode(&none.unwrap()).is_err());
        assert_eq!(day(20_000 * 86_400 + 86_399), 20_000);
    }

    /// A token issued blind verifies for its own service, day and key, and nowhere else; the
    /// headers carry it intact, and a client takes no signature but its own request's.
    #[test]
    fn issued_tokens_verify_only_where_they_were_meant() {
        let issuer = IssuerKey::generate().unwrap();
        let key = issuer.token_key();
        let maker = Challenge::new("issuer.test", "maker", 20_000).unwrap();
        let (pending, blinded) = Pending::new(key, &maker).unwrap();
        key.check_request(&blinded).unwrap();
        let token = pending
            .finalize(key, &issuer.sign(&blinded).unwrap())
            .unwrap();
        let spent = Token::from_authorization(&token.authorization()).unwrap();
        spent.verify(key, &maker).unwrap();

        let relayer = Challenge::new("issuer.test", "relayer", 20_000).unwrap();
        assert!(spent.verify(key, &relayer).is_err());
        let tomorrow = Challenge::new("issuer.test", "maker", 20_001).unwrap();
        assert!(spent.verify(key, &tomorrow).is_err());
        let other = IssuerKey::generate().unwrap();
        assert!(spent.verify(other.token_key(), &maker).is_err());
        let mut forged = spent.encode();
        forged[2] ^= 1;
        assert!(Token::decode(&forged).unwrap().verify(key, &maker).is_err());

        let (asked, asked_key) = read_www_authenticate(&www_authenticate(&maker, key)).unwrap();
        assert_eq!((asked, asked_key.id()), (maker, key.id()));
        let (pending, _) = Pending::new(key, &relayer).unwrap();
        let wrong = issuer
            .sign(&Pending::new(key, &relayer).unwrap().1)
            .unwrap();
        assert!(pending.finalize(key, &wrong).is_err());
        assert!(key.check_request(&[0xff; NK]).is_err());
        assert!(key.check_request(&blinded[1..]).is_err());
    }
}
