use k256::ecdsa::{RecoveryId, Signature, SigningKey, VerifyingKey};
use k256::elliptic_curve::bigint::U512;
use k256::elliptic_curve::generic_array::GenericArray;
use k256::elliptic_curve::ops::Reduce;
use k256::{NonZeroScalar, Scalar};
use sha3::{Digest, Keccak256};

use crate::Error;

const DOMAIN_TYPE: &[u8] =
    b"EIP712Domain(string name,string version,uint256 chainId,address verifyingContract)";
const LOCK_CLAIM_TYPE: &[u8] = b"LockClaim(bytes32 id,uint64 deadline)";
const PAYOUT_TYPE: &[u8] = b"Payout(bytes32 id,address relayer,uint128 fee)";
const RESCUE_TYPE: &[u8] = b"Rescue(bytes32 id,bytes32 note,address relayer,uint128 fee)";

/// The EIP-712 domain of one ZecSwap deployment, and the digests of what a swap's `user` signs
/// there. A signature under one domain never moves another.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Domain {
    pub chain_id: u64,
    pub contract: [u8; 20],
}

impl Domain {
    /// One claim lock, sent by `deadline`, which must fall within the contract's lock duration
    /// of the time it lands.
    pub fn lock_claim(&self, id: &[u8; 32], deadline: u64) -> [u8; 32] {
        self.digest(&[&keccak(&[LOCK_CLAIM_TYPE]), id, &uint(deadline.into())])
    }

    /// The swap's payout into Railgun, sent by `relayer`, which keeps `fee` of it.
    pub fn payout(&self, id: &[u8; 32], relayer: &[u8; 20], fee: u128) -> [u8; 32] {
        self.digest(&[&keccak(&[PAYOUT_TYPE]), id, &address(relayer), &uint(fee)])
    }

    /// Shielding what came back to the swap's vault to `note`, the commitment
    /// `zecswap_railgun::ShieldNote::commitment` computes, sent by `relayer` for `fee`.
    pub fn rescue(
        &self,
        id: &[u8; 32],
        note: &[u8; 32],
        relayer: &[u8; 20],
        fee: u128,
    ) -> [u8; 32] {
        self.digest(&[
            &keccak(&[RESCUE_TYPE]),
            id,
            note,
            &address(relayer),
            &uint(fee),
        ])
    }

    /// The EIP-712 digest of the struct whose encoded words are `fields`.
    fn digest(&self, fields: &[&[u8]]) -> [u8; 32] {
        let separator = keccak(&[
            &keccak(&[DOMAIN_TYPE]),
            &keccak(&[b"ZecSwap"]),
            &keccak(&[b"1"]),
            &uint(self.chain_id.into()),
            &address(&self.contract),
        ]);
        keccak(&[b"\x19\x01", &separator, &keccak(fields)])
    }
}

/// A swap's own Ethereum key. A swap that pays into Railgun names its address as `user`: the
/// user has no account on the chain, so this key signs for the claim lock and the payout and
/// relayers send them. It is never funded.
pub struct AuthKey(SigningKey);

impl AuthKey {
    pub(crate) fn from_uniform(bytes: &[u8; 64]) -> Result<Self, Error> {
        let scalar = <Scalar as Reduce<U512>>::reduce_bytes(GenericArray::from_slice(bytes));
        Option::from(NonZeroScalar::new(scalar))
            .map(|scalar: NonZeroScalar| Self(SigningKey::from(scalar)))
            .ok_or(Error::InvalidScalar)
    }

    /// The swap's `user`.
    pub fn address(&self) -> [u8; 20] {
        address_of(self.0.verifying_key())
    }

    /// `r ‖ s ‖ v` over one of `Domain`'s digests, with a low `s` and `v` in {27, 28}, as the
    /// contract takes it.
    pub fn sign(&self, digest: &[u8; 32]) -> [u8; 65] {
        let (signature, recovery) = self
            .0
            .sign_prehash_recoverable(digest)
            .expect("a 32-byte prehash signs");
        let mut bytes = [0; 65];
        bytes[..64].copy_from_slice(&signature.to_bytes());
        bytes[64] = 27 + recovery.to_byte();
        bytes
    }
}

/// Who signed `digest`, if `signature` is one the contract accepts.
pub fn signer(digest: &[u8; 32], signature: &[u8; 65]) -> Option<[u8; 20]> {
    let parsed = Signature::from_slice(&signature[..64]).ok()?;
    if parsed.normalize_s().is_some() {
        return None;
    }
    let recovery = RecoveryId::from_byte(signature[64].checked_sub(27)?)?;
    VerifyingKey::recover_from_prehash(digest, &parsed, recovery)
        .ok()
        .map(|key| address_of(&key))
}

fn address_of(key: &VerifyingKey) -> [u8; 20] {
    let point = key.to_encoded_point(false);
    Keccak256::digest(&point.as_bytes()[1..])[12..]
        .try_into()
        .expect("20 bytes")
}

fn keccak(parts: &[&[u8]]) -> [u8; 32] {
    let mut hasher = Keccak256::new();
    for part in parts {
        hasher.update(part);
    }
    hasher.finalize().into()
}

fn uint(value: u128) -> [u8; 32] {
    let mut word = [0; 32];
    word[16..].copy_from_slice(&value.to_be_bytes());
    word
}

fn address(value: &[u8; 20]) -> [u8; 32] {
    let mut word = [0; 32];
    word[12..].copy_from_slice(value);
    word
}

#[cfg(test)]
mod tests {
    use super::*;

    fn key(byte: u8) -> AuthKey {
        let mut scalar = [0; 32];
        scalar[31] = byte;
        AuthKey(SigningKey::from_bytes(&scalar.into()).unwrap())
    }

    const DOMAIN: Domain = Domain {
        chain_id: 11155111,
        contract: [9; 20],
    };

    #[test]
    fn address_is_ethereums() {
        // The well-known address of private key 1.
        assert_eq!(
            key(1).address(),
            *b"\x7e\x5f\x45\x52\x09\x1a\x69\x12\x5d\x5d\xfc\xb7\xb8\xc2\x65\x90\x29\x39\x5b\xdf"
        );
    }

    #[test]
    fn signatures_recover_to_their_signer_under_their_digest_only() {
        let key = key(7);
        let digest = DOMAIN.payout(&[3; 32], &[4; 20], 1_000_000);
        let signature = key.sign(&digest);
        assert_eq!(signer(&digest, &signature), Some(key.address()));

        let other_fee = DOMAIN.payout(&[3; 32], &[4; 20], 1_000_001);
        let other_chain = Domain {
            chain_id: 1,
            ..DOMAIN
        }
        .payout(&[3; 32], &[4; 20], 1_000_000);
        for digest in [other_fee, other_chain] {
            assert_ne!(signer(&digest, &signature), Some(key.address()));
        }
    }

    #[test]
    fn high_s_twins_are_rejected() {
        const N: [u8; 32] = [
            0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff,
            0xff, 0xfe, 0xba, 0xae, 0xdc, 0xe6, 0xaf, 0x48, 0xa0, 0x3b, 0xbf, 0xd2, 0x5e, 0x8c,
            0xd0, 0x36, 0x41, 0x41,
        ];
        let digest = DOMAIN.lock_claim(&[3; 32], 1_790_000_000);
        let mut signature = key(7).sign(&digest);
        // s' = n - s, with v flipped, is the same signature malleated.
        let mut borrow = 0i16;
        for i in (0..32).rev() {
            let difference = i16::from(N[i]) - i16::from(signature[32 + i]) - borrow;
            signature[32 + i] = difference.rem_euclid(256) as u8;
            borrow = i16::from(difference < 0);
        }
        signature[64] ^= 1;
        assert_eq!(signer(&digest, &signature), None);
    }
}
