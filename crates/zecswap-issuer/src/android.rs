//! Android hardware key attestation. Each install makes one key in the phone's secure hardware,
//! which certifies it in a chain up to Google's root, along with the app it belongs to and the
//! state the phone booted in. The key signs every request for tokens, and the issuer counts its
//! tokens by the key.

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::time::SystemTime;

use anyhow::{Context as _, Result, ensure};
use base64::Engine as _;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use der::asn1::{AnyRef, ObjectIdentifier};
use der::{Decode, Reader, SliceReader, Tag, TagNumber, Tagged};
use ring::signature::{ECDSA_P256_SHA256_ASN1, UnparsedPublicKey};
use serde::Deserialize;
use sha2::{Digest, Sha256};
use zecswap_api::tokens::Attestation;

use crate::Attester;
use crate::x509::{BASIC_CONSTRAINTS, Cert, KEY_USAGE, Kind, boolean, octets};

/// What sets the issuer's signatures and keys apart from any other use of the same key.
pub(crate) const CONTEXT: &[u8] = b"zecswap-issuer-v1";
const KEY_DESCRIPTION: ObjectIdentifier = ObjectIdentifier::new_unwrap("1.3.6.1.4.1.11129.2.1.17");
const CHALLENGE_LIFETIME: u64 = 5 * 60;
/// Outstanding challenges at most: each takes memory until it is used or expires.
pub(crate) const MAX_CHALLENGES: usize = 10_000;
const MAX_CHAIN: usize = 10;
/// The attestation record's `AuthorizationList` tags this checks.
const ROOT_OF_TRUST: u32 = 704;
const APPLICATION_ID: u32 = 709;
/// `verifiedBootState`'s `Verified`.
const VERIFIED: u64 = 0;

#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AndroidKey {
    /// PEM files of the roots chains must end in: Google's attestation roots.
    pub roots: Vec<PathBuf>,
    /// Google's attestation status list (JSON), which the operator keeps current: certificates
    /// it lists are refused. Read again whenever it changes.
    #[serde(default)]
    pub status_list: Option<PathBuf>,
    /// The app's package names.
    pub packages: Vec<String>,
    /// The SHA-256 digests of the app's signing certificates, hex: the current one, and while
    /// rotating, the next.
    pub signing_digests: Vec<String>,
    #[serde(default)]
    pub min_security_level: SecurityLevel,
}

/// Where a key lives and is attested from, at the least.
#[derive(Clone, Copy, Debug, Default, Deserialize, PartialEq, Eq, PartialOrd, Ord)]
#[serde(rename_all = "kebab-case")]
pub enum SecurityLevel {
    #[default]
    TrustedEnvironment = 1,
    StrongBox = 2,
}

/// What a key's attestation challenge must be: it names this issuer, so a key the app made for
/// anything else counts for nothing here.
pub(crate) fn key_challenge(issuer: &str) -> [u8; 32] {
    Sha256::new()
        .chain_update(CONTEXT)
        .chain_update(issuer)
        .finalize()
        .into()
}

/// What a request's signature covers: the challenge it answers and the blinded messages it
/// carries.
pub(crate) fn signed_message(challenge: &[u8], blinded: &[Vec<u8>]) -> Vec<u8> {
    let blinded = blinded
        .iter()
        .fold(Sha256::new(), |hash, blinded| hash.chain_update(blinded))
        .finalize();
    [CONTEXT, challenge, &blinded].concat()
}

pub(crate) struct AndroidAttester {
    /// Each root, DER.
    roots: Vec<Vec<u8>>,
    status: Option<StatusList>,
    packages: Vec<String>,
    digests: Vec<[u8; 32]>,
    min_level: SecurityLevel,
    key_challenge: [u8; 32],
    /// Each challenge given out and not yet used, with when it expires.
    challenges: Mutex<HashMap<[u8; 32], u64>>,
}

impl AndroidAttester {
    pub fn new(config: &AndroidKey, issuer: &str) -> Result<Self> {
        let mut roots = Vec::new();
        for path in &config.roots {
            let pem = std::fs::read_to_string(path)
                .with_context(|| format!("reading {}", path.display()))?;
            let found = certificates(&pem).with_context(|| format!("in {}", path.display()))?;
            ensure!(!found.is_empty(), "no certificate in {}", path.display());
            roots.extend(found);
        }
        ensure!(!roots.is_empty(), "no attestation roots");
        ensure!(!config.packages.is_empty(), "no app packages");
        let digests = config
            .signing_digests
            .iter()
            .map(|digest| {
                hex::decode(digest)
                    .ok()
                    .and_then(|digest| digest.try_into().ok())
                    .with_context(|| format!("a signing digest that is not SHA-256 hex: {digest}"))
            })
            .collect::<Result<Vec<_>>>()?;
        ensure!(!digests.is_empty(), "no app signing digests");
        Ok(Self {
            roots,
            status: config
                .status_list
                .as_deref()
                .map(StatusList::open)
                .transpose()?,
            packages: config.packages.clone(),
            digests,
            min_level: config.min_security_level,
            key_challenge: key_challenge(issuer),
            challenges: Mutex::default(),
        })
    }

    /// Checks the key's attestation record, `KeyDescription`: this issuer's key, in secure
    /// hardware, on a locked phone that booted verified, of this app.
    fn check(&self, description: &[u8]) -> der::Result<Result<(), &'static str>> {
        let record = AnyRef::from_der(description)?.sequence(|r| {
            let _version = uint(AnyRef::decode(r)?, Tag::Integer)?;
            let attestation_level = uint(AnyRef::decode(r)?, Tag::Enumerated)?;
            let _keymint_version = uint(AnyRef::decode(r)?, Tag::Integer)?;
            let keymint_level = uint(AnyRef::decode(r)?, Tag::Enumerated)?;
            let challenge = octets(AnyRef::decode(r)?)?;
            let _unique_id = octets(AnyRef::decode(r)?)?;
            let software = AnyRef::decode(r)?;
            let hardware = AnyRef::decode(r)?;
            [software, hardware]
                .iter()
                .try_for_each(|list| list.tag().assert_eq(Tag::Sequence).map(drop))?;
            Ok::<_, der::Error>((
                [attestation_level, keymint_level],
                challenge,
                software.value(),
                hardware.value(),
            ))
        })?;
        let (levels, challenge, software, hardware) = record;
        if challenge != self.key_challenge {
            return Ok(Err("a key made for another issuer"));
        }
        if levels.iter().any(|level| *level < self.min_level as u64) {
            return Ok(Err("a key outside the required secure hardware"));
        }
        // The phone's secure hardware vouches for how it booted, never the system.
        let Some(root_of_trust) = field(hardware, ROOT_OF_TRUST)? else {
            return Ok(Err("a key with no root of trust"));
        };
        let (locked, boot) = root_of_trust.sequence(|r| {
            let _boot_key = octets(AnyRef::decode(r)?)?;
            let locked = boolean(AnyRef::decode(r)?)?;
            let boot = uint(AnyRef::decode(r)?, Tag::Enumerated)?;
            while !r.is_finished() {
                AnyRef::decode(r)?;
            }
            Ok::<_, der::Error>((locked, boot))
        })?;
        if !locked {
            return Ok(Err("a phone with its bootloader unlocked"));
        }
        if boot != VERIFIED {
            return Ok(Err("a phone that did not boot verified"));
        }
        let application = match field(software, APPLICATION_ID)? {
            Some(application) => Some(application),
            None => field(hardware, APPLICATION_ID)?,
        };
        let Some(application) = application else {
            return Ok(Err("a key with no app"));
        };
        let (packages, digests) = AnyRef::from_der(octets(application)?)?.sequence(|r| {
            let packages = items(AnyRef::decode(r)?, Tag::Set)?
                .into_iter()
                .map(|package| {
                    package.sequence(|r| {
                        let name = octets(AnyRef::decode(r)?)?;
                        AnyRef::decode(r)?;
                        Ok::<_, der::Error>(name)
                    })
                })
                .collect::<der::Result<Vec<_>>>()?;
            let digests = items(AnyRef::decode(r)?, Tag::Set)?
                .into_iter()
                .map(octets)
                .collect::<der::Result<Vec<_>>>()?;
            Ok::<_, der::Error>((packages, digests))
        })?;
        if packages.is_empty()
            || !packages
                .iter()
                .all(|name| self.packages.iter().any(|ours| ours.as_bytes() == *name))
        {
            return Ok(Err("another app's key"));
        }
        if !digests
            .iter()
            .any(|digest| self.digests.iter().any(|ours| ours == digest))
        {
            return Ok(Err("a key of an app signed by someone else"));
        }
        Ok(Ok(()))
    }
}

impl Attester for AndroidAttester {
    fn hold(&self, challenge: [u8; 32], now: u64) -> bool {
        let mut challenges = self.challenges.lock().unwrap();
        if challenges.len() >= MAX_CHALLENGES {
            challenges.retain(|_, expires| *expires > now);
            if challenges.len() >= MAX_CHALLENGES {
                return false;
            }
        }
        challenges.insert(challenge, now + CHALLENGE_LIFETIME);
        true
    }

    /// The key's digest, if `attestation` holds at `now` for a request carrying `blinded`, by
    /// the checks `docs/tokens.md` lists, in its order.
    fn device(
        &self,
        attestation: &Attestation,
        blinded: &[Vec<u8>],
        now: u64,
    ) -> Result<[u8; 32], &'static str> {
        // Spent before anything else is checked: one request per challenge, whatever it holds.
        let challenge: [u8; 32] = decode(&attestation.challenge)
            .and_then(|challenge| challenge.try_into().ok())
            .ok_or("an unknown challenge")?;
        let expires = self
            .challenges
            .lock()
            .unwrap()
            .remove(&challenge)
            .ok_or("an unknown challenge")?;
        if expires <= now {
            return Err("an expired challenge");
        }

        if !(1..=MAX_CHAIN).contains(&attestation.chain.len()) {
            return Err("a chain of the wrong length");
        }
        let ders = attestation
            .chain
            .iter()
            .map(|cert| decode(cert))
            .collect::<Option<Vec<_>>>()
            .ok_or("a certificate that is not base64url")?;
        let chain = ders
            .iter()
            .map(|der| Cert::parse(der))
            .collect::<Result<Vec<_>, _>>()
            .map_err(|_| "an undecodable certificate")?;
        let roots = self
            .roots
            .iter()
            .map(|der| Cert::parse(der).expect("read at startup"))
            .collect::<Vec<_>>();
        let (path, root) = anchor(&chain, &roots)?;
        for (i, cert) in path.iter().enumerate() {
            let parent = path.get(i + 1).unwrap_or(root);
            if cert.issuer != parent.subject || !parent.key.signed(cert) {
                return Err("a certificate its issuer did not sign");
            }
            // An attested key is no authority: one could otherwise vouch for any key it liked.
            if i > 0
                && (!cert.issues(i - 1).unwrap_or(false)
                    || cert.extension(KEY_DESCRIPTION).is_some())
            {
                return Err("a certificate signed by one that is no authority");
            }
            if cert.extensions.iter().any(|extension| {
                extension.critical
                    && ![BASIC_CONSTRAINTS, KEY_USAGE, KEY_DESCRIPTION].contains(&extension.id)
            }) {
                return Err("a certificate with an unknown critical extension");
            }
        }
        let used = || path.iter().chain([root]);
        if used().any(|cert| now < cert.not_before || now > cert.not_after) {
            return Err("a certificate outside its validity");
        }
        if let Some(status) = &self.status
            && status.revoked(used().map(|cert| cert.serial))
        {
            return Err("a revoked certificate");
        }

        let leaf = &path[0];
        let description = leaf
            .extension(KEY_DESCRIPTION)
            .ok_or("a key with no attestation")?;
        self.check(description.value)
            .map_err(|_| "an undecodable key attestation")??;

        if leaf.key.kind != Kind::P256 {
            return Err("a key that is not P-256");
        }
        let signature = decode(&attestation.signature).ok_or("a request its key did not sign")?;
        UnparsedPublicKey::new(&ECDSA_P256_SHA256_ASN1, leaf.key.bytes)
            .verify(&signed_message(&challenge, blinded), &signature)
            .map_err(|_| "a request its key did not sign")?;
        Ok(Sha256::digest(leaf.spki).into())
    }

    fn status_list(&self) -> Option<crate::StatusList> {
        self.status.as_ref().map(StatusList::state)
    }
}

/// The chain below the root, and the pinned root it ends in. The app may send the root along,
/// or leave it out; either way only a pinned root's key counts.
fn anchor<'a>(
    chain: &'a [Cert<'a>],
    roots: &'a [Cert<'a>],
) -> Result<(&'a [Cert<'a>], &'a Cert<'a>), &'static str> {
    let last = chain.last().expect("a chain of one or more");
    if let Some(root) = roots.iter().find(|root| root.spki == last.spki) {
        return match &chain[..chain.len() - 1] {
            [] => Err("a chain of the root alone"),
            path => Ok((path, root)),
        };
    }
    roots
        .iter()
        .find(|root| root.subject == last.issuer && root.key.signed(last))
        .map(|root| (chain, root))
        .ok_or("a chain to an unknown root")
}

fn decode(base64: &str) -> Option<Vec<u8>> {
    URL_SAFE_NO_PAD.decode(base64).ok()
}

/// Each certificate a PEM file holds, DER.
fn certificates(pem: &str) -> Result<Vec<Vec<u8>>> {
    const END: &str = "-----END CERTIFICATE-----";
    pem.split_inclusive(END)
        .filter_map(|block| block.find("-----BEGIN").map(|begin| &block[begin..]))
        .map(|block| {
            let (label, der) = pem_rfc7468::decode_vec(block.as_bytes())
                .map_err(|e| anyhow::anyhow!("a PEM block that does not decode: {e}"))?;
            ensure!(
                label == "CERTIFICATE",
                "a PEM block that is not a certificate"
            );
            Cert::parse(&der).context("a certificate that does not decode")?;
            Ok(der)
        })
        .collect()
}

/// The `AuthorizationList` field tagged `number`, if `list` has it.
fn field(list: &[u8], number: u32) -> der::Result<Option<AnyRef<'_>>> {
    let mut r = SliceReader::new(list)?;
    while !r.is_finished() {
        let field = AnyRef::decode(&mut r)?;
        if field.tag()
            == (Tag::ContextSpecific {
                constructed: true,
                number: TagNumber(number),
            })
        {
            return AnyRef::from_der(field.value()).map(Some);
        }
    }
    Ok(None)
}

fn items(set: AnyRef<'_>, tag: Tag) -> der::Result<Vec<AnyRef<'_>>> {
    set.tag().assert_eq(tag)?;
    let mut r = SliceReader::new(set.value())?;
    let mut items = Vec::new();
    while !r.is_finished() {
        items.push(AnyRef::decode(&mut r)?);
    }
    Ok(items)
}

/// A small non-negative `INTEGER` or `ENUMERATED`.
fn uint(value: AnyRef<'_>, tag: Tag) -> der::Result<u64> {
    value.tag().assert_eq(tag)?;
    match value.value() {
        bytes @ [first, ..] if bytes.len() <= 8 && first & 0x80 == 0 => {
            Ok(bytes.iter().fold(0, |n, byte| n << 8 | u64::from(*byte)))
        }
        _ => Err(tag.value_error().into()),
    }
}

/// Google's attestation status list (`https://android.googleapis.com/attestation/status`):
/// `{"entries": {"<serial>": {"status": "REVOKED", ...}, ...}}`, each serial in lowercase hex
/// or, for over half of them, decimal.
struct StatusList {
    path: PathBuf,
    listed: Mutex<Listed>,
}

struct Listed {
    /// When the file was written that the serials were read from.
    written: SystemTime,
    /// When the file was written as last seen, read or not.
    seen: SystemTime,
    entries: u64,
    serials: HashSet<String>,
}

impl StatusList {
    fn open(path: &Path) -> Result<Self> {
        let written = modified(path)?;
        let (entries, serials) = Self::read(path)?;
        Ok(Self {
            path: path.into(),
            listed: Mutex::new(Listed {
                written,
                seen: written,
                entries,
                serials,
            }),
        })
    }

    /// The number of entries, and the serials they list.
    fn read(path: &Path) -> Result<(u64, HashSet<String>)> {
        #[derive(Deserialize)]
        struct List {
            entries: HashMap<String, serde_json::Value>,
        }
        let text =
            std::fs::read_to_string(path).with_context(|| format!("reading {}", path.display()))?;
        let list: List =
            serde_json::from_str(&text).with_context(|| format!("parsing {}", path.display()))?;
        let entries = list.entries.len() as u64;
        let serials = list
            .entries
            .into_keys()
            .flat_map(|serial| {
                // Digits alone could be either reading, so both are kept: a real serial
                // matching the other one by chance is out of reach.
                let decimal = serial
                    .bytes()
                    .all(|byte| byte.is_ascii_digit())
                    .then(|| serial.parse::<u128>().ok())
                    .flatten()
                    .map(|serial| format!("{serial:x}"));
                [Some(serial_hex(&serial.to_ascii_lowercase())), decimal]
            })
            .flatten()
            .collect();
        Ok((entries, serials))
    }

    /// The list as the file now has it. A file that no longer reads leaves the last list in
    /// force.
    fn current(&self) -> std::sync::MutexGuard<'_, Listed> {
        let mut listed = self.listed.lock().unwrap();
        if let Ok(modified) = modified(&self.path)
            && modified != listed.seen
        {
            listed.seen = modified;
            match Self::read(&self.path) {
                Ok((entries, serials)) => {
                    listed.written = modified;
                    listed.entries = entries;
                    listed.serials = serials;
                }
                Err(e) => tracing::error!("keeping the last attestation status list: {e:#}"),
            }
        }
        listed
    }

    /// Whether any of `serials` is listed.
    fn revoked<'a>(&self, serials: impl IntoIterator<Item = &'a [u8]>) -> bool {
        let listed = self.current();
        serials
            .into_iter()
            .any(|serial| listed.serials.contains(&serial_hex(&hex::encode(serial))))
    }

    fn state(&self) -> crate::StatusList {
        let listed = self.current();
        crate::StatusList {
            modified_at: listed
                .written
                .duration_since(std::time::UNIX_EPOCH)
                .map_or(0, |since| since.as_secs()),
            entries: listed.entries,
        }
    }
}

fn modified(path: &Path) -> Result<SystemTime> {
    Ok(std::fs::metadata(path)
        .with_context(|| format!("reading {}", path.display()))?
        .modified()?)
}

/// A serial in hex as the status list keys it: lowercase, no leading zeros.
fn serial_hex(hex: &str) -> String {
    match hex.trim_start_matches('0') {
        "" => "0".into(),
        hex => hex.into(),
    }
}

#[cfg(test)]
mod tests {
    use std::time::{Duration, UNIX_EPOCH};

    use der::Encode as _;
    use der::asn1::{GeneralizedTime, UtcTime};
    use pem_rfc7468::LineEnding;
    use ring::rand::SystemRandom;
    use ring::signature::{
        ECDSA_P256_SHA256_ASN1_SIGNING, ECDSA_P384_SHA384_ASN1_SIGNING, EcdsaKeyPair,
        EcdsaSigningAlgorithm, KeyPair as _, RSA_PKCS1_SHA256, RsaKeyPair,
    };
    use zecswap_api::tokens::TokenRequests;
    use zecswap_tokens::{Challenge, IssuerKey, Pending, TokenKey};

    use super::*;
    use crate::{Config, IssueError, Issuer};

    /// 2026-09-21, when the chains are checked.
    const NOW: u64 = 1_790_000_000;
    const DAY: u64 = 24 * 60 * 60;
    const YEAR: u64 = 365 * DAY;
    const ISSUER: &str = "issuer.test";
    const PACKAGE: &str = "xyz.justzappit.zapp";
    const DIGEST: [u8; 32] = [0x5a; 32];
    /// The batch certificate's serial, high bit set: DER gives it a leading zero byte.
    const BATCH_SERIAL: u64 = 0xc35d_0e1f_2ab9;

    fn tlv(tag: &[u8], value: &[u8]) -> Vec<u8> {
        let length = match value.len() {
            len @ 0..0x80 => vec![len as u8],
            len @ 0x80..0x100 => vec![0x81, len as u8],
            len => vec![0x82, (len >> 8) as u8, len as u8],
        };
        [tag, &length, value].concat()
    }

    fn seq(items: &[&[u8]]) -> Vec<u8> {
        tlv(&[0x30], &items.concat())
    }

    fn set(items: &[&[u8]]) -> Vec<u8> {
        tlv(&[0x31], &items.concat())
    }

    fn int(n: u64) -> Vec<u8> {
        let bytes = n.to_be_bytes();
        let start = bytes.iter().position(|byte| *byte != 0).unwrap_or(7);
        let padding: &[u8] = if bytes[start] & 0x80 == 0 { &[] } else { &[0] };
        tlv(&[0x02], &[padding, &bytes[start..]].concat())
    }

    fn enumerated(n: u8) -> Vec<u8> {
        tlv(&[0x0a], &[n])
    }

    fn boolean(value: bool) -> Vec<u8> {
        tlv(&[0x01], &[if value { 0xff } else { 0 }])
    }

    fn octet_string(bytes: &[u8]) -> Vec<u8> {
        tlv(&[0x04], bytes)
    }

    fn bit_string(bytes: &[u8]) -> Vec<u8> {
        tlv(&[0x03], &[&[0], bytes].concat())
    }

    fn oid(id: &str) -> Vec<u8> {
        ObjectIdentifier::new_unwrap(id).to_der().unwrap()
    }

    /// `[number] EXPLICIT`: tags above 30 take DER's long form, as the attestation record's do.
    fn tagged(number: u32, inner: &[u8]) -> Vec<u8> {
        let tag = if number < 31 {
            vec![0xa0 | number as u8]
        } else {
            let mut digits = vec![(number & 0x7f) as u8];
            let mut rest = number >> 7;
            while rest > 0 {
                digits.insert(0, 0x80 | (rest & 0x7f) as u8);
                rest >>= 7;
            }
            [vec![0xbf], digits].concat()
        };
        tlv(&tag, inner)
    }

    fn time(unix: u64) -> Vec<u8> {
        let at = Duration::from_secs(unix);
        match UtcTime::from_unix_duration(at) {
            Ok(time) => time.to_der().unwrap(),
            Err(_) => GeneralizedTime::from_unix_duration(at)
                .unwrap()
                .to_der()
                .unwrap(),
        }
    }

    fn name(common_name: &str) -> Vec<u8> {
        let common_name = tlv(&[0x0c], common_name.as_bytes());
        seq(&[&set(&[&seq(&[&oid("2.5.4.3"), &common_name])])])
    }

    fn extension(id: &str, critical: bool, value: &[u8]) -> Vec<u8> {
        let critical = if critical { boolean(true) } else { vec![] };
        seq(&[&oid(id), &critical, &octet_string(value)])
    }

    /// What a certificate authority's certificate carries.
    fn authority() -> Vec<Vec<u8>> {
        vec![
            extension("2.5.29.19", true, &seq(&[&boolean(true)])),
            // keyCertSign and cRLSign
            extension("2.5.29.15", true, &tlv(&[0x03], &[0x01, 0x06])),
        ]
    }

    enum Signer {
        P256(EcdsaKeyPair),
        P384(EcdsaKeyPair),
        Rsa(RsaKeyPair),
    }

    impl Signer {
        fn ec(algorithm: &'static EcdsaSigningAlgorithm) -> EcdsaKeyPair {
            let random = SystemRandom::new();
            let pkcs8 = EcdsaKeyPair::generate_pkcs8(algorithm, &random).unwrap();
            EcdsaKeyPair::from_pkcs8(algorithm, pkcs8.as_ref(), &random).unwrap()
        }

        fn p256() -> Self {
            Self::P256(Self::ec(&ECDSA_P256_SHA256_ASN1_SIGNING))
        }

        fn p384() -> Self {
            Self::P384(Self::ec(&ECDSA_P384_SHA384_ASN1_SIGNING))
        }

        fn rsa() -> Self {
            let pem = IssuerKey::generate().unwrap().to_pem().unwrap();
            let (_, pkcs8) = pem_rfc7468::decode_vec(pem.as_bytes()).unwrap();
            Self::Rsa(RsaKeyPair::from_pkcs8(&pkcs8).unwrap())
        }

        fn spki(&self) -> Vec<u8> {
            const EC: &str = "1.2.840.10045.2.1";
            let (algorithm, key) = match self {
                Self::P256(key) => (
                    seq(&[&oid(EC), &oid("1.2.840.10045.3.1.7")]),
                    key.public_key().as_ref(),
                ),
                Self::P384(key) => (
                    seq(&[&oid(EC), &oid("1.3.132.0.34")]),
                    key.public_key().as_ref(),
                ),
                Self::Rsa(key) => (
                    seq(&[&oid("1.2.840.113549.1.1.1"), &[0x05, 0x00]]),
                    key.public().as_ref(),
                ),
            };
            seq(&[&algorithm, &bit_string(key)])
        }

        /// The algorithm this key signs certificates with.
        fn algorithm(&self) -> Vec<u8> {
            match self {
                Self::P256(_) => seq(&[&oid("1.2.840.10045.4.3.2")]),
                Self::P384(_) => seq(&[&oid("1.2.840.10045.4.3.3")]),
                Self::Rsa(_) => seq(&[&oid("1.2.840.113549.1.1.11"), &[0x05, 0x00]]),
            }
        }

        fn sign(&self, message: &[u8]) -> Vec<u8> {
            let random = SystemRandom::new();
            match self {
                Self::P256(key) | Self::P384(key) => {
                    key.sign(&random, message).unwrap().as_ref().to_vec()
                }
                Self::Rsa(key) => {
                    let mut signature = vec![0; key.public().modulus_len()];
                    key.sign(&RSA_PKCS1_SHA256, &random, message, &mut signature)
                        .unwrap();
                    signature
                }
            }
        }
    }

    /// A certificate for the key `spki`, signed by `issuer`.
    fn certificate(
        issuer: (&Signer, &str),
        subject: (&[u8], &str),
        serial: u64,
        (not_before, not_after): (u64, u64),
        extensions: &[Vec<u8>],
    ) -> Vec<u8> {
        let (signer, issuer) = issuer;
        let (spki, subject) = subject;
        let extensions = extensions.iter().map(Vec::as_slice).collect::<Vec<_>>();
        let tbs = seq(&[
            &tagged(0, &int(2)),
            &int(serial),
            &signer.algorithm(),
            &name(issuer),
            &seq(&[&time(not_before), &time(not_after)]),
            &name(subject),
            spki,
            &tagged(3, &seq(&extensions)),
        ]);
        seq(&[&tbs, &signer.algorithm(), &bit_string(&signer.sign(&tbs))])
    }

    /// What a key's attestation record says.
    struct Record {
        /// `attestationSecurityLevel` and `keyMintSecurityLevel`.
        levels: [u8; 2],
        challenge: [u8; 32],
        locked: bool,
        boot: u8,
        package: &'static str,
        digest: [u8; 32],
    }

    impl Default for Record {
        fn default() -> Self {
            Self {
                levels: [1, 1],
                challenge: key_challenge(ISSUER),
                locked: true,
                boot: 0,
                package: PACKAGE,
                digest: DIGEST,
            }
        }
    }

    impl Record {
        /// A `KeyDescription` with the fields Android's carry around the ones checked.
        fn der(&self) -> Vec<u8> {
            let application = seq(&[
                &set(&[&seq(&[&octet_string(self.package.as_bytes()), &int(1)])]),
                &set(&[&octet_string(&self.digest)]),
            ]);
            let software = seq(&[
                &tagged(701, &int(NOW * 1000)),
                &tagged(709, &octet_string(&application)),
            ]);
            let root_of_trust = seq(&[
                &octet_string(&[1; 32]),
                &boolean(self.locked),
                &enumerated(self.boot),
                &octet_string(&[2; 32]),
            ]);
            let hardware = seq(&[
                &tagged(1, &set(&[&int(2)])),
                &tagged(702, &int(0)),
                &tagged(704, &root_of_trust),
            ]);
            seq(&[
                &int(300),
                &enumerated(self.levels[0]),
                &int(300),
                &enumerated(self.levels[1]),
                &octet_string(&self.challenge),
                &octet_string(&[]),
                &software,
                &hardware,
            ])
        }
    }

    /// An install: its key, and the key's chain as the app sends it.
    struct Device {
        key: Signer,
        chain: Vec<Vec<u8>>,
    }

    impl Device {
        /// A new key attested as `record` says by the attestation key `issuer`, whose chain
        /// is `above`.
        fn new(issuer: (&Signer, &str), above: &[Vec<u8>], record: &Record) -> Self {
            Self::with(issuer, above, record, &[])
        }

        /// As `new`, its key's certificate carrying `extensions` besides the record.
        fn with(
            issuer: (&Signer, &str),
            above: &[Vec<u8>],
            record: &Record,
            extensions: &[Vec<u8>],
        ) -> Self {
            let key = Signer::p256();
            let description = extension("1.3.6.1.4.1.11129.2.1.17", false, &record.der());
            let leaf = certificate(
                issuer,
                (&key.spki(), "Android Keystore Key"),
                1,
                (NOW - 60, NOW + 10 * YEAR),
                &[&[description], extensions].concat(),
            );
            Self {
                key,
                chain: [&[leaf], above].concat(),
            }
        }

        fn attest(&self, challenge: &str, blinded: &[Vec<u8>]) -> Attestation {
            let message = signed_message(&URL_SAFE_NO_PAD.decode(challenge).unwrap(), blinded);
            Attestation {
                challenge: challenge.into(),
                chain: self
                    .chain
                    .iter()
                    .map(|cert| URL_SAFE_NO_PAD.encode(cert))
                    .collect(),
                signature: URL_SAFE_NO_PAD.encode(self.key.sign(&message)),
            }
        }
    }

    /// An issuer taking Android keys under a pinned root, and the root's batch key.
    struct Fixture {
        dir: tempfile::TempDir,
        issuer: Issuer,
        token_key: TokenKey,
        root: Signer,
        root_cert: Vec<u8>,
        batch: Signer,
        batch_cert: Vec<u8>,
    }

    impl Fixture {
        fn new(root: Signer) -> Self {
            Self::with(root, "")
        }

        /// With `extra` added to the `android-key` table.
        fn with(root: Signer, extra: &str) -> Self {
            let dir = tempfile::tempdir().unwrap();
            let root_cert = certificate(
                (&root, "root"),
                (&root.spki(), "root"),
                1,
                (NOW - YEAR, NOW + 50 * YEAR),
                &authority(),
            );
            let pem = pem_rfc7468::encode_string("CERTIFICATE", LineEnding::LF, &root_cert);
            std::fs::write(dir.path().join("root.pem"), pem.unwrap()).unwrap();
            std::fs::write(dir.path().join("status.json"), r#"{"entries": {}}"#).unwrap();
            let batch = Signer::p256();
            let batch_cert = certificate(
                (&root, "root"),
                (&batch.spki(), "batch"),
                BATCH_SERIAL,
                (NOW - YEAR, NOW + YEAR),
                &authority(),
            );
            let issuer = Issuer::new(config(dir.path(), extra)).unwrap();
            let token_key = TokenKey::from_base64(&issuer.token_key().token_key).unwrap();
            Self {
                dir,
                issuer,
                token_key,
                root,
                root_cert,
                batch,
                batch_cert,
            }
        }

        /// A new install whose key is attested as `record` says, sending its whole chain.
        fn device(&self, record: &Record) -> Device {
            let above = [self.batch_cert.clone(), self.root_cert.clone()];
            Device::new((&self.batch, "batch"), &above, record)
        }

        fn blinded(&self, count: usize) -> Vec<Vec<u8>> {
            let maker = Challenge::new(ISSUER, "maker", NOW / DAY).unwrap();
            (0..count)
                .map(|_| Pending::new(&self.token_key, &maker).unwrap().1)
                .collect()
        }

        /// How many of `count` tokens `device` gets at `now`.
        fn ask(&self, device: &Device, count: usize, now: u64) -> Result<usize, IssueError> {
            let challenge = self.issuer.challenge(now)?.challenge;
            let blinded = self.blinded(count);
            self.send(device.attest(&challenge, &blinded), &blinded, now)
        }

        fn send(
            &self,
            attestation: Attestation,
            blinded: &[Vec<u8>],
            now: u64,
        ) -> Result<usize, IssueError> {
            let blinded = blinded.iter().map(|b| URL_SAFE_NO_PAD.encode(b)).collect();
            let request = TokenRequests {
                attestation,
                blinded,
            };
            let issued = self.issuer.issue(&request, now)?;
            Ok(issued.blind_signatures.len())
        }
    }

    /// An issuer's config in `dir`, as an operator writes it, with `extra` in its
    /// `android-key` table.
    fn config(dir: &Path, extra: &str) -> Config {
        let key = dir.join("issuer.pem");
        if !key.exists() {
            std::fs::write(&key, IssuerKey::generate().unwrap().to_pem().unwrap()).unwrap();
        }
        let dir = dir.display();
        let text = format!(
            r#"
            listen = "127.0.0.1:0"
            name = "{ISSUER}"
            key = "{dir}/issuer.pem"
            data_dir = "{dir}"
            tokens_per_day = 3

            [attestation.android-key]
            roots = ["{dir}/root.pem"]
            status_list = "{dir}/status.json"
            packages = ["{PACKAGE}"]
            signing_digests = ["{}"]
            {extra}
            "#,
            hex::encode(DIGEST),
        );
        toml::from_str(&text).unwrap()
    }

    fn refused(result: Result<usize, IssueError>) -> &'static str {
        match result {
            Err(IssueError::Refused(why)) => why,
            other => panic!("not refused: {other:?}"),
        }
    }

    /// An install's key gets the day's allowance and no more, whether the app sends the root
    /// along or not, under RSA and EC roots alike; another key gets its own, and the next day
    /// starts afresh.
    #[test]
    fn an_installs_key_gets_its_days_tokens() {
        for root in [Signer::p384(), Signer::rsa()] {
            let fixture = Fixture::new(root);
            let mut phone = fixture.device(&Record::default());
            assert_eq!(fixture.ask(&phone, 2, NOW).unwrap(), 2);
            phone.chain.pop();
            assert_eq!(
                fixture.ask(&phone, 2, NOW).unwrap(),
                1,
                "what the day had left"
            );
            assert!(matches!(
                fixture.ask(&phone, 1, NOW),
                Err(IssueError::Spent)
            ));
            let tablet = fixture.device(&Record::default());
            assert_eq!(fixture.ask(&tablet, 3, NOW).unwrap(), 3, "another key");
            assert_eq!(
                fixture.ask(&phone, 3, NOW + DAY).unwrap(),
                3,
                "the next day"
            );
        }
    }

    /// A challenge counts once, within five minutes of being given out, and only one the
    /// issuer gave out counts at all.
    #[test]
    fn a_challenge_counts_once_and_briefly() {
        let fixture = Fixture::new(Signer::p384());
        let phone = fixture.device(&Record::default());
        let blinded = fixture.blinded(1);
        let made_up = phone.attest(&URL_SAFE_NO_PAD.encode([9; 32]), &blinded);
        let made_up = fixture.send(made_up, &blinded, NOW);
        assert_eq!(refused(made_up), "an unknown challenge");

        let challenge = fixture.issuer.challenge(NOW).unwrap().challenge;
        let attestation = phone.attest(&challenge, &blinded);
        assert_eq!(fixture.send(attestation.clone(), &blinded, NOW).unwrap(), 1);
        let again = fixture.send(attestation, &blinded, NOW);
        assert_eq!(refused(again), "an unknown challenge");

        let challenge = fixture.issuer.challenge(NOW).unwrap().challenge;
        let late = fixture.send(phone.attest(&challenge, &blinded), &blinded, NOW + 5 * 60);
        assert_eq!(refused(late), "an expired challenge");
    }

    /// A request its key did not sign, whether it was signed over other blinded messages or
    /// by another key, is refused, and costs the device nothing.
    #[test]
    fn a_refused_request_spends_no_allowance() {
        let fixture = Fixture::new(Signer::p384());
        let phone = fixture.device(&Record::default());
        let blinded = fixture.blinded(3);
        let challenge = fixture.issuer.challenge(NOW).unwrap().challenge;
        let swapped = phone.attest(&challenge, &fixture.blinded(3));
        let swapped = fixture.send(swapped, &blinded, NOW);
        assert_eq!(refused(swapped), "a request its key did not sign");

        let thief = Device {
            key: Signer::p256(),
            chain: phone.chain.clone(),
        };
        let stolen = fixture.ask(&thief, 3, NOW);
        assert_eq!(refused(stolen), "a request its key did not sign");
        assert_eq!(fixture.ask(&phone, 3, NOW).unwrap(), 3, "nothing spent");
    }

    /// Only a pinned root anchors a chain: one that merely comes with it, named like the
    /// pinned one, does not.
    #[test]
    fn a_chain_must_end_in_a_pinned_root() {
        let fixture = Fixture::new(Signer::p384());
        let stranger = Signer::p384();
        let validity = (NOW - YEAR, NOW + YEAR);
        let stranger_cert = certificate(
            (&stranger, "root"),
            (&stranger.spki(), "root"),
            1,
            validity,
            &authority(),
        );
        let batch = Signer::p256();
        let batch_cert = certificate(
            (&stranger, "root"),
            (&batch.spki(), "batch"),
            2,
            validity,
            &authority(),
        );
        let above = [batch_cert, stranger_cert];
        let mut phone = Device::new((&batch, "batch"), &above, &Record::default());
        assert_eq!(
            refused(fixture.ask(&phone, 1, NOW)),
            "a chain to an unknown root"
        );
        phone.chain.pop();
        assert_eq!(
            refused(fixture.ask(&phone, 1, NOW)),
            "a chain to an unknown root"
        );
    }

    /// A OnePlus StrongBox marks its keys' `keyUsage` critical with 1, not DER's 0xff: a chain
    /// whose flags are written so still counts, its certificate authority's included.
    #[test]
    fn a_true_written_as_one_still_reads() {
        let fixture = Fixture::new(Signer::p384());
        let one = tlv(&[0x01], &[0x01]);
        let flagged = |id: &str, value: &[u8]| seq(&[&oid(id), &one, &octet_string(value)]);
        let batch = Signer::p256();
        let batch_cert = certificate(
            (&fixture.root, "root"),
            (&batch.spki(), "batch"),
            BATCH_SERIAL,
            (NOW - YEAR, NOW + YEAR),
            &[
                flagged("2.5.29.19", &seq(&[&one])),
                flagged("2.5.29.15", &tlv(&[0x03], &[0x01, 0x06])),
            ],
        );
        // digitalSignature
        let usage = flagged("2.5.29.15", &tlv(&[0x03], &[0x07, 0x80]));
        let above = [batch_cert, fixture.root_cert.clone()];
        let phone = Device::with((&batch, "batch"), &above, &Record::default(), &[usage]);
        assert_eq!(fixture.ask(&phone, 3, NOW).unwrap(), 3);
    }

    /// A certificate counts only signed by the key of the one it names as its issuer.
    #[test]
    fn a_forged_certificate_is_refused() {
        let fixture = Fixture::new(Signer::p384());
        let above = [fixture.batch_cert.clone(), fixture.root_cert.clone()];
        let forgery = Device::new((&Signer::p256(), "batch"), &above, &Record::default());
        let refusal = refused(fixture.ask(&forgery, 1, NOW));
        assert_eq!(refusal, "a certificate its issuer did not sign");
    }

    /// A key attested for any app can sign what it likes, a certificate too: one it signs for
    /// a key of its owner's choosing, claiming to be this app's, vouches for nothing. Nor does
    /// any certificate not marked an authority, nor an attested key's even if it were.
    #[test]
    fn an_attested_key_vouches_for_no_other() {
        let fixture = Fixture::new(Signer::p384());
        let other_app = Record {
            package: "com.example.other",
            ..Record::default()
        };
        let description = || {
            let record = other_app.der();
            extension("1.3.6.1.4.1.11129.2.1.17", false, &record)
        };
        let validity = (NOW - 60, NOW + YEAR);
        let attested = Signer::p256();
        let above = [fixture.batch_cert.clone(), fixture.root_cert.clone()];
        let unmarked = certificate(
            (&fixture.batch, "batch"),
            (&attested.spki(), "Android Keystore Key"),
            1,
            validity,
            &[description()],
        );
        let marked = certificate(
            (&fixture.batch, "batch"),
            (&attested.spki(), "Android Keystore Key"),
            1,
            validity,
            &[authority(), vec![description()]].concat(),
        );
        let no_record = certificate(
            (&fixture.batch, "batch"),
            (&attested.spki(), "Android Keystore Key"),
            1,
            validity,
            &[],
        );
        for vouching in [unmarked, marked, no_record] {
            let chain = [&[vouching], &above[..]].concat();
            let forgery = Device::new(
                (&attested, "Android Keystore Key"),
                &chain,
                &Record::default(),
            );
            let refusal = refused(fixture.ask(&forgery, 1, NOW));
            assert_eq!(refusal, "a certificate signed by one that is no authority");
        }
    }

    #[test]
    fn an_expired_certificate_is_refused() {
        let fixture = Fixture::new(Signer::p384());
        let expired = certificate(
            (&fixture.root, "root"),
            (&fixture.batch.spki(), "batch"),
            BATCH_SERIAL,
            (NOW - 2 * YEAR, NOW - 1),
            &authority(),
        );
        let above = [expired, fixture.root_cert.clone()];
        let phone = Device::new((&fixture.batch, "batch"), &above, &Record::default());
        assert_eq!(
            refused(fixture.ask(&phone, 1, NOW)),
            "a certificate outside its validity"
        );
    }

    /// The operator refreshes the status list in place, and a certificate it lists from
    /// then on is refused, keyed as Google keys it: lowercase hex with no leading zeros, or for
    /// over half of the list's entries, decimal.
    #[test]
    fn a_certificate_is_refused_once_the_status_list_names_it() {
        for serial in [format!("{BATCH_SERIAL:x}"), BATCH_SERIAL.to_string()] {
            let fixture = Fixture::new(Signer::p384());
            let phone = fixture.device(&Record::default());
            assert_eq!(fixture.ask(&phone, 1, NOW).unwrap(), 1);
            let status = fixture.dir.path().join("status.json");
            let listed = format!(r#"{{"entries": {{"{serial}": {{"status": "REVOKED"}}}}}}"#);
            std::fs::write(&status, listed).unwrap();
            let later = SystemTime::now() + Duration::from_secs(60);
            std::fs::File::options()
                .write(true)
                .open(&status)
                .unwrap()
                .set_modified(later)
                .unwrap();
            assert_eq!(
                refused(fixture.ask(&phone, 1, NOW)),
                "a revoked certificate",
                "listed as {serial}"
            );
            let written = later.duration_since(UNIX_EPOCH).unwrap().as_secs();
            let shown = serde_json::to_value(fixture.issuer.monitor(NOW).unwrap()).unwrap();
            assert_eq!(
                shown["statusList"],
                serde_json::json!({"modifiedAt": written, "entries": 1})
            );
            assert_eq!(shown["requests"]["refused"]["a revoked certificate"], 1);

            // A refresh that doesn't read leaves the last list in force, and says so.
            std::fs::write(&status, "not a list").unwrap();
            std::fs::File::options()
                .write(true)
                .open(&status)
                .unwrap()
                .set_modified(later + Duration::from_secs(60))
                .unwrap();
            assert_eq!(
                refused(fixture.ask(&phone, 1, NOW)),
                "a revoked certificate"
            );
            let shown = serde_json::to_value(fixture.issuer.monitor(NOW).unwrap()).unwrap();
            assert_eq!(shown["statusList"]["modifiedAt"], written);
        }
    }

    /// The attestation record must speak for this issuer's key, in the phone's secure hardware,
    /// on a phone that is locked and booted verified, and for this app, signed by its owner;
    /// the operator may require StrongBox.
    #[test]
    fn the_record_must_vouch_for_this_app_on_a_locked_phone() {
        let fixture = Fixture::new(Signer::p384());
        let cases = [
            (
                Record {
                    levels: [0, 1],
                    ..Record::default()
                },
                "a key outside the required secure hardware",
            ),
            (
                Record {
                    levels: [1, 0],
                    ..Record::default()
                },
                "a key outside the required secure hardware",
            ),
            (
                Record {
                    locked: false,
                    ..Record::default()
                },
                "a phone with its bootloader unlocked",
            ),
            (
                Record {
                    boot: 2,
                    ..Record::default()
                },
                "a phone that did not boot verified",
            ),
            (
                Record {
                    package: "com.example.other",
                    ..Record::default()
                },
                "another app's key",
            ),
            (
                Record {
                    digest: [1; 32],
                    ..Record::default()
                },
                "a key of an app signed by someone else",
            ),
            (
                Record {
                    challenge: key_challenge("another"),
                    ..Record::default()
                },
                "a key made for another issuer",
            ),
        ];
        for (record, why) in cases {
            let phone = fixture.device(&record);
            assert_eq!(refused(fixture.ask(&phone, 1, NOW)), why);
        }

        let strict = Fixture::with(Signer::p384(), r#"min_security_level = "strong-box""#);
        let trusted_environment = strict.device(&Record::default());
        let refusal = refused(strict.ask(&trusted_environment, 1, NOW));
        assert_eq!(refusal, "a key outside the required secure hardware");
        let strong_box = Record {
            levels: [2, 2],
            ..Record::default()
        };
        assert_eq!(strict.ask(&strict.device(&strong_box), 1, NOW).unwrap(), 1);
    }

    /// Challenges given out and not yet used take memory, so there is a ceiling on them, and
    /// each makes room again once it expires.
    #[test]
    fn outstanding_challenges_are_capped() {
        let fixture = Fixture::new(Signer::p384());
        for _ in 0..MAX_CHALLENGES {
            fixture.issuer.challenge(NOW).unwrap();
        }
        let full = fixture.issuer.challenge(NOW + 5 * 60 - 1);
        assert!(matches!(full, Err(IssueError::Busy)), "{full:?}");
        assert!(fixture.issuer.challenge(NOW + 5 * 60).is_ok());
    }

    #[test]
    fn a_status_list_that_does_not_read_stops_startup() {
        let fixture = Fixture::new(Signer::p384());
        std::fs::remove_file(fixture.dir.path().join("status.json")).unwrap();
        let error = Issuer::new(config(fixture.dir.path(), "")).err().unwrap();
        assert!(format!("{error:#}").contains("status.json"), "{error:#}");
    }

    /// A chain captured from a real phone passes every check: the fixture slot
    /// `fixtures/android-key/` holds it (see its README) once one is captured.
    #[test]
    #[ignore = "needs a captured chain and Google's roots in fixtures/android-key"]
    fn a_real_phones_chain_passes() {
        #[derive(Deserialize)]
        #[serde(rename_all = "camelCase")]
        struct Captured {
            issuer: String,
            package: String,
            signing_digest: String,
            at: u64,
            blinded: Vec<String>,
            attestation: Attestation,
        }
        let slot = Path::new(env!("CARGO_MANIFEST_DIR")).join("fixtures/android-key");
        let captured = std::fs::read_to_string(slot.join("captured.json")).unwrap();
        let captured: Captured = serde_json::from_str(&captured).unwrap();
        let roots = std::fs::read_dir(slot.join("roots"))
            .unwrap()
            .map(|entry| entry.unwrap().path())
            .collect();
        let config = AndroidKey {
            roots,
            status_list: None,
            packages: vec![captured.package],
            signing_digests: vec![captured.signing_digest],
            min_security_level: SecurityLevel::TrustedEnvironment,
        };
        let attester = AndroidAttester::new(&config, &captured.issuer).unwrap();
        let challenge = decode(&captured.attestation.challenge).unwrap();
        assert!(attester.hold(challenge.try_into().unwrap(), captured.at));
        let blinded = captured
            .blinded
            .iter()
            .map(|blinded| decode(blinded).unwrap())
            .collect::<Vec<_>>();
        let leaf = decode(&captured.attestation.chain[0]).unwrap();
        let spki = Cert::parse(&leaf).unwrap().spki.to_vec();
        let device = attester.device(&captured.attestation, &blinded, captured.at);
        assert_eq!(device, Ok(Sha256::digest(spki).into()));
    }
}
