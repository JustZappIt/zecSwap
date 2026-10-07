//! The token bindings against `zecswap_tokens`' own issuer: a maker's challenge read, a request
//! blinded, signed and finalized through the bytes the Android wrapper carries between calls.

use zecswap::tokens::{authorization, blind, finalize, read_challenge};
use zecswap_tokens::{Challenge, IssuerKey, Token, www_authenticate};

/// RFC 9578's 2048-bit modulus: the blinded message's length.
const BLINDED: usize = 256;

#[test]
fn a_token_round_trips_through_the_bindings_bytes() {
    let issuer = IssuerKey::generate().unwrap();
    let maker = Challenge::new("issuer.test", "maker").unwrap();
    let [challenge, key, name] =
        read_challenge(&www_authenticate(&maker, issuer.token_key())).unwrap();
    assert_eq!(
        (challenge.as_slice(), key.as_slice(), name.as_slice()),
        (
            maker.encode().as_slice(),
            issuer.token_key().spki(),
            b"issuer.test".as_slice()
        )
    );

    let request = blind(&key, &challenge).unwrap();
    let (blinded, pending) = request.split_at(BLINDED);
    let token = finalize(pending, &key, &issuer.sign(blinded).unwrap()).unwrap();
    let spent = Token::from_authorization(&authorization(&token).unwrap()).unwrap();
    assert_eq!(spent.encode(), token);
    spent.verify(issuer.token_key(), &maker).unwrap();

    // Another request's signature, a pending half changed or cut short, never finalize.
    let other = blind(&key, &challenge).unwrap();
    let wrong = issuer.sign(&other[..BLINDED]).unwrap();
    assert!(finalize(pending, &key, &wrong).is_err());
    let mut changed = pending.to_vec();
    changed[100] ^= 1;
    assert!(finalize(&changed, &key, &issuer.sign(blinded).unwrap()).is_err());
    assert!(finalize(&pending[1..], &key, &issuer.sign(blinded).unwrap()).is_err());
    assert!(authorization(&token[1..]).is_err());
}
