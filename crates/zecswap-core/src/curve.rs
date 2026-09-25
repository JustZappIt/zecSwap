//! Pallas encodings shared with the settlement contract.
//!
//! The contract works in big-endian EVM words: points as affine `(x, y)`, scalars as
//! `uint256`. Zcash encodings are little-endian, so every crossing goes through here.

use std::sync::OnceLock;

use ff::PrimeField;
use group::{GroupEncoding, prime::PrimeCurveAffine};
use pasta_curves::{arithmetic::CurveAffine, pallas};

use crate::Error;

/// `GroupHash^P("z.cash:Orchard", "G")`, the Orchard spend authorization base, compressed.
const SPEND_AUTH_G: [u8; 32] = [
    99, 201, 117, 184, 132, 114, 26, 141, 12, 161, 112, 123, 227, 12, 127, 12, 95, 68, 95, 62, 124,
    24, 141, 59, 6, 214, 241, 40, 179, 35, 85, 183,
];

pub(crate) fn spend_auth_g() -> pallas::Point {
    static G: OnceLock<pallas::Point> = OnceLock::new();
    *G.get_or_init(|| pallas::Point::from_bytes(&SPEND_AUTH_G).unwrap())
}

/// Affine big-endian `(x, y)`. The caller guarantees `p` is not the identity.
pub(crate) fn point_to_evm(p: &pallas::Affine) -> [u8; 64] {
    let c = p.coordinates().expect("identity has no affine coordinates");
    let mut out = [0; 64];
    out[..32].copy_from_slice(&reversed(c.x().to_repr()));
    out[32..].copy_from_slice(&reversed(c.y().to_repr()));
    out
}

pub(crate) fn point_from_evm(bytes: &[u8; 64]) -> Result<pallas::Affine, Error> {
    let (x, y) = bytes.split_at(32);
    let (x, y) = base_from_be(x)
        .zip(base_from_be(y))
        .ok_or(Error::InvalidPoint)?;
    // `from_xy` accepts (0, 0) as the identity, which is never a valid share.
    Option::<pallas::Affine>::from(pallas::Affine::from_xy(x, y))
        .filter(|p| !bool::from(p.is_identity()))
        .ok_or(Error::InvalidPoint)
}

pub(crate) fn scalar_to_evm(s: &pallas::Scalar) -> [u8; 32] {
    reversed(s.to_repr())
}

pub(crate) fn scalar_from_evm(bytes: &[u8; 32]) -> Option<pallas::Scalar> {
    pallas::Scalar::from_repr(reversed(*bytes)).into()
}

fn base_from_be(bytes: &[u8]) -> Option<pallas::Base> {
    let repr = reversed(bytes.try_into().ok()?);
    pallas::Base::from_repr(repr).into()
}

fn reversed(mut bytes: [u8; 32]) -> [u8; 32] {
    bytes.reverse();
    bytes
}

#[cfg(test)]
mod tests {
    use ff::Field;
    use group::{Curve, Group};
    use orchard::primitives::redpallas::{SigningKey, SpendAuth, VerificationKey};
    use pasta_curves::arithmetic::CurveExt;
    use rand_core::OsRng;

    use super::*;

    #[test]
    fn spend_auth_g_is_the_orchard_group_hash_redpallas_signs_with() {
        let hashed = pallas::Point::hash_to_curve("z.cash:Orchard")(b"G");
        assert_eq!(spend_auth_g(), hashed);
        assert_ne!(spend_auth_g(), pallas::Point::generator());

        let k = pallas::Scalar::random(OsRng);
        let sk = SigningKey::<SpendAuth>::try_from(k.to_repr()).unwrap();
        let vk: [u8; 32] = (&VerificationKey::from(&sk)).into();
        assert_eq!(vk, (spend_auth_g() * k).to_bytes());
    }

    #[test]
    fn evm_point_rejects_identity_off_curve_and_non_canonical() {
        assert_eq!(point_from_evm(&[0; 64]), Err(Error::InvalidPoint));

        let mut off_curve = point_to_evm(&spend_auth_g().to_affine());
        off_curve[63] ^= 1;
        assert_eq!(point_from_evm(&off_curve), Err(Error::InvalidPoint));

        let mut non_canonical = [0xff; 64];
        non_canonical[..32].copy_from_slice(&point_to_evm(&spend_auth_g().to_affine())[..32]);
        assert_eq!(point_from_evm(&non_canonical), Err(Error::InvalidPoint));
    }
}
