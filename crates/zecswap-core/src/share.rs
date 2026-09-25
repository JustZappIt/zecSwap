use core::fmt;

use ff::{Field, PrimeField};
use group::{Curve, GroupEncoding};
use orchard::primitives::redpallas::{self, SpendAuth};
use pasta_curves::pallas;
use rand_core::{CryptoRng, RngCore};
use zeroize::{Zeroize, ZeroizeOnDrop};

use crate::{Error, curve};

/// One half of a joint spend authorizing key: the maker's `e` or the user's `z`.
#[derive(Zeroize, ZeroizeOnDrop)]
pub struct SecretShare([u8; 32]);

impl SecretShare {
    pub fn random<R: RngCore + CryptoRng>(mut rng: R) -> Self {
        loop {
            if let Ok(share) = Self::from_scalar(pallas::Scalar::random(&mut rng)) {
                return share;
            }
        }
    }

    /// Parses the big-endian `uint256` that `claim` or `refund` reveals.
    pub fn from_be_bytes(bytes: &[u8; 32]) -> Result<Self, Error> {
        curve::scalar_from_evm(bytes)
            .ok_or(Error::InvalidScalar)
            .and_then(Self::from_scalar)
    }

    pub fn to_be_bytes(&self) -> [u8; 32] {
        curve::scalar_to_evm(&self.scalar())
    }

    pub fn public(&self) -> PublicShare {
        PublicShare((curve::spend_auth_g() * self.scalar()).to_affine())
    }

    pub(crate) fn from_scalar(scalar: pallas::Scalar) -> Result<Self, Error> {
        if scalar.is_zero().into() {
            Err(Error::InvalidScalar)
        } else {
            Ok(Self(scalar.to_repr()))
        }
    }

    pub(crate) fn scalar(&self) -> pallas::Scalar {
        pallas::Scalar::from_repr(self.0).expect("constructed from a canonical scalar")
    }

    /// A RedPallas signature under this share is a Schnorr proof of knowledge of it.
    pub(crate) fn prove<R: RngCore + CryptoRng>(&self, message: &[u8], rng: R) -> ShareProof {
        let key = redpallas::SigningKey::<SpendAuth>::try_from(self.0)
            .expect("constructed from a canonical scalar");
        ShareProof((&key.sign(rng, message)).into())
    }
}

impl fmt::Debug for SecretShare {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("SecretShare(..)")
    }
}

/// `[share]·SpendAuthG`: the maker's `E` or the user's `Z`. Never the identity.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct PublicShare(pallas::Affine);

impl PublicShare {
    /// Parses big-endian affine `(x, y)` words, the layout the contract stores.
    pub fn from_affine_bytes(bytes: &[u8; 64]) -> Result<Self, Error> {
        curve::point_from_evm(bytes).map(Self)
    }

    pub fn to_affine_bytes(&self) -> [u8; 64] {
        curve::point_to_evm(&self.0)
    }

    pub(crate) fn point(&self) -> pallas::Affine {
        self.0
    }

    pub(crate) fn verify(&self, message: &[u8], proof: &ShareProof) -> Result<(), Error> {
        let key = redpallas::VerificationKey::<SpendAuth>::try_from(self.0.to_bytes())
            .map_err(|_| Error::InvalidPoint)?;
        key.verify(message, &redpallas::Signature::from(proof.0))
            .map_err(|_| Error::InvalidProof)
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ShareProof([u8; 64]);

impl ShareProof {
    pub fn from_bytes(bytes: [u8; 64]) -> Self {
        Self(bytes)
    }

    pub fn to_bytes(&self) -> [u8; 64] {
        self.0
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rejects_zero_and_non_canonical() {
        assert_eq!(
            SecretShare::from_be_bytes(&[0; 32]).unwrap_err(),
            Error::InvalidScalar
        );
        assert_eq!(
            SecretShare::from_be_bytes(&[0xff; 32]).unwrap_err(),
            Error::InvalidScalar
        );
    }
}
