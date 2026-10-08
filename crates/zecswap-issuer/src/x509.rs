//! What an attestation chain is checked by in each X.509 certificate (RFC 5280), read by DER's
//! strict rules but for booleans, and the signatures binding them.

use der::asn1::{AnyRef, BitStringRef, GeneralizedTime, ObjectIdentifier, UtcTime};
use der::{Decode, Reader, SliceReader, Tag, TagNumber, Tagged};
use ring::signature::{self, UnparsedPublicKey, VerificationAlgorithm};

const EC_PUBLIC_KEY: ObjectIdentifier = ObjectIdentifier::new_unwrap("1.2.840.10045.2.1");
const P256: ObjectIdentifier = ObjectIdentifier::new_unwrap("1.2.840.10045.3.1.7");
const P384: ObjectIdentifier = ObjectIdentifier::new_unwrap("1.3.132.0.34");
const RSA: ObjectIdentifier = ObjectIdentifier::new_unwrap("1.2.840.113549.1.1.1");
pub(crate) const BASIC_CONSTRAINTS: ObjectIdentifier = ObjectIdentifier::new_unwrap("2.5.29.19");
pub(crate) const KEY_USAGE: ObjectIdentifier = ObjectIdentifier::new_unwrap("2.5.29.15");

/// The signature algorithms attestation chains use, by the kind of key that checks them.
static ALGORITHMS: [(Kind, ObjectIdentifier, &dyn VerificationAlgorithm); 7] = [
    (
        Kind::P256,
        ObjectIdentifier::new_unwrap("1.2.840.10045.4.3.2"),
        &signature::ECDSA_P256_SHA256_ASN1,
    ),
    (
        Kind::P256,
        ObjectIdentifier::new_unwrap("1.2.840.10045.4.3.3"),
        &signature::ECDSA_P256_SHA384_ASN1,
    ),
    (
        Kind::P384,
        ObjectIdentifier::new_unwrap("1.2.840.10045.4.3.2"),
        &signature::ECDSA_P384_SHA256_ASN1,
    ),
    (
        Kind::P384,
        ObjectIdentifier::new_unwrap("1.2.840.10045.4.3.3"),
        &signature::ECDSA_P384_SHA384_ASN1,
    ),
    (
        Kind::Rsa,
        ObjectIdentifier::new_unwrap("1.2.840.113549.1.1.11"),
        &signature::RSA_PKCS1_2048_8192_SHA256,
    ),
    (
        Kind::Rsa,
        ObjectIdentifier::new_unwrap("1.2.840.113549.1.1.12"),
        &signature::RSA_PKCS1_2048_8192_SHA384,
    ),
    (
        Kind::Rsa,
        ObjectIdentifier::new_unwrap("1.2.840.113549.1.1.13"),
        &signature::RSA_PKCS1_2048_8192_SHA512,
    ),
];

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Kind {
    P256,
    P384,
    Rsa,
}

/// A public key: an uncompressed EC point, or an RSA key as PKCS #1 `RSAPublicKey`.
pub(crate) struct Key<'a> {
    pub kind: Kind,
    pub bytes: &'a [u8],
}

impl Key<'_> {
    /// Whether this key signed `cert`.
    pub fn signed(&self, cert: &Cert) -> bool {
        ALGORITHMS
            .iter()
            .find(|(kind, id, _)| *kind == self.kind && *id == cert.algorithm)
            .is_some_and(|(_, _, algorithm)| {
                UnparsedPublicKey::new(*algorithm, self.bytes)
                    .verify(cert.tbs, cert.signature)
                    .is_ok()
            })
    }
}

pub(crate) struct Extension<'a> {
    pub id: ObjectIdentifier,
    pub critical: bool,
    pub value: &'a [u8],
}

pub(crate) struct Cert<'a> {
    tbs: &'a [u8],
    algorithm: ObjectIdentifier,
    signature: &'a [u8],
    /// The serial number's DER contents: big-endian, two's complement.
    pub serial: &'a [u8],
    /// Each name whole, as DER: issuers and subjects match byte for byte.
    pub issuer: &'a [u8],
    pub subject: &'a [u8],
    /// Unix time.
    pub not_before: u64,
    pub not_after: u64,
    /// The `SubjectPublicKeyInfo` whole.
    pub spki: &'a [u8],
    pub key: Key<'a>,
    pub extensions: Vec<Extension<'a>>,
}

impl<'a> Cert<'a> {
    pub fn parse(der: &'a [u8]) -> der::Result<Self> {
        let (tbs, algorithm, signature) = whole(der, |r| {
            r.sequence(|r| {
                let tbs = r.tlv_bytes()?;
                let algorithm = read_algorithm(r)?;
                let signature = bits(BitStringRef::decode(r)?)?;
                Ok((tbs, algorithm, signature))
            })
        })?;
        whole(tbs, |r| {
            r.sequence(|r| {
                if Tag::peek(r)? == explicit(0) {
                    AnyRef::decode(r)?;
                }
                let serial = AnyRef::decode(r)?;
                serial.tag().assert_eq(Tag::Integer)?;
                // RFC 5280 4.1.1.2: the signed algorithm is the one the signature uses.
                if read_algorithm(r)? != algorithm {
                    return Err(Tag::Sequence.value_error().into());
                }
                let issuer = r.tlv_bytes()?;
                let (not_before, not_after) = r.sequence(|r| {
                    Ok::<_, der::Error>((time(AnyRef::decode(r)?)?, time(AnyRef::decode(r)?)?))
                })?;
                let subject = r.tlv_bytes()?;
                let spki = r.tlv_bytes()?;
                let mut extensions = Vec::new();
                while !r.is_finished() {
                    let field = AnyRef::decode(r)?;
                    match field.tag() {
                        Tag::ContextSpecific {
                            constructed: false,
                            number: TagNumber(1 | 2),
                        } => {}
                        tag if tag == explicit(3) => {
                            extensions = self::extensions(field.value())?;
                        }
                        tag => return Err(tag.unexpected_error(None).into()),
                    }
                }
                Ok(Self {
                    tbs,
                    algorithm,
                    signature,
                    serial: serial.value(),
                    issuer,
                    subject,
                    not_before,
                    not_after,
                    spki,
                    key: key(spki)?,
                    extensions,
                })
            })
        })
    }

    pub fn extension(&self, id: ObjectIdentifier) -> Option<&Extension<'a>> {
        self.extensions.iter().find(|extension| extension.id == id)
    }

    /// Whether this certificate may sign certificates for others with `below` certificate
    /// authorities between them and the leaf.
    pub fn issues(&self, below: usize) -> der::Result<bool> {
        let Some(constraints) = self.extension(BASIC_CONSTRAINTS) else {
            return Ok(false);
        };
        let (authority, depth) = whole(constraints.value, |r| {
            r.sequence(|r| {
                let authority =
                    Tag::peek(r).ok() == Some(Tag::Boolean) && boolean(AnyRef::decode(r)?)?;
                let depth = if r.is_finished() {
                    None
                } else {
                    Some(u32::decode(r)?)
                };
                Ok((authority, depth))
            })
        })?;
        let signs = match self.extension(KEY_USAGE) {
            // keyCertSign
            Some(usage) => whole(usage.value, BitStringRef::decode)?
                .get(5)
                .unwrap_or(false),
            None => true,
        };
        Ok(authority && signs && depth.is_none_or(|depth| depth as usize >= below))
    }
}

/// Decodes all of `der`, and nothing after it.
fn whole<'a, T>(
    der: &'a [u8],
    f: impl FnOnce(&mut SliceReader<'a>) -> der::Result<T>,
) -> der::Result<T> {
    let mut reader = SliceReader::new(der)?;
    let value = f(&mut reader)?;
    reader.finish()?;
    Ok(value)
}

fn explicit(number: u32) -> Tag {
    Tag::ContextSpecific {
        constructed: true,
        number: TagNumber(number),
    }
}

/// An `AlgorithmIdentifier`'s algorithm; its parameters are the algorithm's to check.
fn read_algorithm(r: &mut SliceReader<'_>) -> der::Result<ObjectIdentifier> {
    r.sequence(|r| {
        let id = ObjectIdentifier::decode(r)?;
        if !r.is_finished() {
            AnyRef::decode(r)?;
        }
        Ok(id)
    })
}

pub(crate) fn octets(value: AnyRef<'_>) -> der::Result<&[u8]> {
    value.tag().assert_eq(Tag::OctetString)?;
    Ok(value.value())
}

/// Any non-zero octet is true: some secure hardware writes `TRUE` as 1, not DER's 0xff. The
/// signatures cover the bytes as written, so reading them this way trusts nothing more.
pub(crate) fn boolean(value: AnyRef<'_>) -> der::Result<bool> {
    value.tag().assert_eq(Tag::Boolean)?;
    match value.value() {
        [byte] => Ok(*byte != 0),
        _ => Err(Tag::Boolean.value_error().into()),
    }
}

fn bits(bits: BitStringRef<'_>) -> der::Result<&[u8]> {
    bits.as_bytes()
        .ok_or_else(|| Tag::BitString.value_error().into())
}

fn time(time: AnyRef<'_>) -> der::Result<u64> {
    let since_epoch = match time.tag() {
        Tag::UtcTime => time.decode_as::<UtcTime>()?.to_unix_duration(),
        Tag::GeneralizedTime => time.decode_as::<GeneralizedTime>()?.to_unix_duration(),
        tag => return Err(tag.unexpected_error(None).into()),
    };
    Ok(since_epoch.as_secs())
}

fn key(spki: &[u8]) -> der::Result<Key<'_>> {
    whole(spki, |r| {
        r.sequence(|r| {
            let (id, curve) = r.sequence(|r| {
                let id = ObjectIdentifier::decode(r)?;
                let parameters = if r.is_finished() {
                    None
                } else {
                    Some(AnyRef::decode(r)?)
                };
                Ok::<_, der::Error>((id, parameters))
            })?;
            let curve = curve.and_then(|curve| curve.decode_as::<ObjectIdentifier>().ok());
            let kind = match curve {
                Some(curve) if id == EC_PUBLIC_KEY && curve == P256 => Kind::P256,
                Some(curve) if id == EC_PUBLIC_KEY && curve == P384 => Kind::P384,
                _ if id == RSA => Kind::Rsa,
                _ => return Err(Tag::ObjectIdentifier.value_error().into()),
            };
            let bytes = bits(BitStringRef::decode(r)?)?;
            Ok(Key { kind, bytes })
        })
    })
}

fn extensions(der: &[u8]) -> der::Result<Vec<Extension<'_>>> {
    whole(der, |r| {
        r.sequence(|r| {
            let mut extensions = Vec::new();
            while !r.is_finished() {
                extensions.push(r.sequence(|r| {
                    let id = ObjectIdentifier::decode(r)?;
                    let critical = Tag::peek(r)? == Tag::Boolean && boolean(AnyRef::decode(r)?)?;
                    let value = octets(AnyRef::decode(r)?)?;
                    Ok::<_, der::Error>(Extension {
                        id,
                        critical,
                        value,
                    })
                })?);
            }
            Ok(extensions)
        })
    })
}
