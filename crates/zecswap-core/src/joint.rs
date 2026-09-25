use ff::{Field, FromUniformBytes, PrimeField};
use group::{Group, GroupEncoding};
use orchard::keys::{FullViewingKey, Scope};
use orchard::primitives::redpallas::{self, SpendAuth};
use pasta_curves::pallas;
use rand_core::{CryptoRng, RngCore};
use zcash_address::unified::{self, Encoding};
use zcash_protocol::consensus::NetworkType;
use zeroize::{Zeroize, ZeroizeOnDrop};

use crate::{Error, PublicShare, SecretShare};

/// The user's `nk` and `rivk`, which complete the joint full viewing key. Shared with the
/// maker so both can see the deposit.
#[derive(Clone, PartialEq, Eq)]
pub struct ViewingKeys {
    nk: [u8; 32],
    rivk: [u8; 32],
}

impl ViewingKeys {
    pub fn random<R: RngCore + CryptoRng>(mut rng: R) -> Self {
        Self {
            nk: pallas::Base::random(&mut rng).to_repr(),
            rivk: pallas::Scalar::random(&mut rng).to_repr(),
        }
    }

    /// Parses `nk ‖ rivk` in their raw Orchard FVK encodings.
    pub fn from_bytes(bytes: &[u8; 64]) -> Result<Self, Error> {
        let (nk, rivk) = bytes.split_at(32);
        let nk: [u8; 32] = nk.try_into().expect("32 bytes");
        let rivk: [u8; 32] = rivk.try_into().expect("32 bytes");
        let canonical = bool::from(pallas::Base::from_repr(nk).is_some())
            && bool::from(pallas::Scalar::from_repr(rivk).is_some());
        canonical
            .then_some(Self { nk, rivk })
            .ok_or(Error::InvalidViewingKey)
    }

    pub fn to_bytes(&self) -> [u8; 64] {
        let mut bytes = [0; 64];
        bytes[..32].copy_from_slice(&self.nk);
        bytes[32..].copy_from_slice(&self.rivk);
        bytes
    }

    pub(crate) fn from_uniform(nk: &[u8; 64], rivk: &[u8; 64]) -> Self {
        Self {
            nk: pallas::Base::from_uniform_bytes(nk).to_repr(),
            rivk: pallas::Scalar::from_uniform_bytes(rivk).to_repr(),
        }
    }
}

/// The Orchard account whose `ak` is `±(E + Z)`: both parties can view it, and only the
/// holder of both secret shares can spend from it.
#[derive(Clone, Debug)]
pub struct JointAccount {
    fvk: FullViewingKey,
    negated: bool,
}

impl JointAccount {
    /// Derive from the shares as recorded on-chain, never from what a quote API claims.
    pub fn derive(
        maker: &PublicShare,
        user: &PublicShare,
        viewing: &ViewingKeys,
    ) -> Result<Self, Error> {
        let sum = pallas::Point::from(maker.point()) + user.point();
        if sum.is_identity().into() {
            return Err(Error::DegenerateJointKey);
        }
        // Orchard requires `ak` to have an even `y`; `spend_key` negates `e + z` to match.
        let negated = sum.to_bytes()[31] >> 7 == 1;
        let ak = if negated { -sum } else { sum };

        let mut raw = [0; 96];
        raw[..32].copy_from_slice(&ak.to_bytes());
        raw[32..].copy_from_slice(&viewing.to_bytes());
        let fvk = FullViewingKey::from_bytes(&raw).ok_or(Error::InvalidViewingKey)?;
        Ok(Self { fvk, negated })
    }

    pub fn fvk(&self) -> &FullViewingKey {
        &self.fvk
    }

    /// An Orchard-only unified full viewing key, for importing as a view-only account.
    pub fn ufvk(&self, network: NetworkType) -> String {
        unified::Ufvk::try_from_items(vec![unified::Fvk::Orchard(self.fvk.to_bytes())])
            .expect("a lone Orchard FVK is a valid UFVK")
            .encode(&network)
    }

    pub fn deposit_address(&self) -> orchard::Address {
        self.fvk.address_at(0u32, Scope::External)
    }

    /// The deposit address as an Orchard-only unified address.
    pub fn unified_address(&self, network: NetworkType) -> String {
        let receiver = unified::Receiver::Orchard(self.deposit_address().to_raw_address_bytes());
        unified::Address::try_from_items(vec![receiver])
            .expect("a lone Orchard receiver is a valid unified address")
            .encode(&network)
    }

    /// Combines both halves into the key that authorizes spends from this account.
    pub fn spend_key(&self, maker: &SecretShare, user: &SecretShare) -> Result<SpendKey, Error> {
        let sum = maker.scalar() + user.scalar();
        let key = SpendKey::from_scalar(if self.negated { -sum } else { sum })
            .ok_or(Error::ShareMismatch)?;
        let ak: [u8; 32] = (&redpallas::VerificationKey::from(&key.signing_key())).into();
        (ak[..] == self.fvk.to_bytes()[..32])
            .then_some(key)
            .ok_or(Error::ShareMismatch)
    }
}

/// A combined spend authorizing key, `±(e + z)`, for one joint account.
#[derive(Zeroize, ZeroizeOnDrop)]
pub struct SpendKey([u8; 32]);

impl SpendKey {
    fn from_scalar(ask: pallas::Scalar) -> Option<Self> {
        (!bool::from(ask.is_zero())).then(|| Self(ask.to_repr()))
    }

    pub(crate) fn signing_key(&self) -> redpallas::SigningKey<SpendAuth> {
        redpallas::SigningKey::try_from(self.0).expect("constructed from a canonical scalar")
    }
}

#[cfg(test)]
mod tests {
    use rand_core::OsRng;

    use super::*;

    fn account(e: &SecretShare, z: &SecretShare) -> JointAccount {
        JointAccount::derive(&e.public(), &z.public(), &ViewingKeys::random(OsRng)).unwrap()
    }

    #[test]
    fn both_halves_combine_under_either_parity() {
        let (mut even, mut odd) = (0, 0);
        while even < 4 || odd < 4 {
            let (e, z) = (SecretShare::random(OsRng), SecretShare::random(OsRng));
            let joint = account(&e, &z);
            if joint.negated {
                odd += 1
            } else {
                even += 1
            }
            assert!(joint.spend_key(&e, &z).is_ok());
            assert_eq!(joint.fvk.to_bytes()[31] >> 7, 0);
        }
    }

    #[test]
    fn either_half_alone_or_a_wrong_half_is_rejected() {
        let (e, z) = (SecretShare::random(OsRng), SecretShare::random(OsRng));
        let joint = account(&e, &z);
        let stranger = SecretShare::random(OsRng);
        assert_eq!(
            joint.spend_key(&e, &stranger).err(),
            Some(Error::ShareMismatch)
        );
        assert_eq!(
            joint.spend_key(&stranger, &z).err(),
            Some(Error::ShareMismatch)
        );
    }

    #[test]
    fn opposite_shares_are_degenerate() {
        let z = SecretShare::random(OsRng);
        let minus_z = SecretShare::from_scalar(-z.scalar()).unwrap();
        let result =
            JointAccount::derive(&minus_z.public(), &z.public(), &ViewingKeys::random(OsRng));
        assert_eq!(result.err(), Some(Error::DegenerateJointKey));
    }

    #[test]
    fn rejects_non_canonical_viewing_keys() {
        assert_eq!(
            ViewingKeys::from_bytes(&[0xff; 64]).err(),
            Some(Error::InvalidViewingKey)
        );
    }
}
