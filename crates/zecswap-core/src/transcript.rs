use rand_core::{CryptoRng, RngCore};

use crate::{Error, PublicShare, SecretShare, ShareProof};

const MAKER_TAG: &[u8; 16] = b"ZecSwap/v1/maker";
const USER_TAG: &[u8; 16] = b"ZecSwap/v1/user_";

/// The quote a share's proof of knowledge is bound to, so it cannot be replayed into
/// another swap, chain or contract.
///
/// The maker proves `E` before it sees `Z`, so its proof binds only `E`; the user's
/// binds both shares and the payout address.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct SwapContext {
    pub chain_id: u64,
    pub contract: [u8; 20],
    pub quote_id: [u8; 32],
}

impl SwapContext {
    pub fn prove_maker<R: RngCore + CryptoRng>(&self, e: &SecretShare, rng: R) -> ShareProof {
        e.prove(&self.maker_message(&e.public()), rng)
    }

    pub fn verify_maker(&self, maker: &PublicShare, proof: &ShareProof) -> Result<(), Error> {
        maker.verify(&self.maker_message(maker), proof)
    }

    pub fn prove_user<R: RngCore + CryptoRng>(
        &self,
        maker: &PublicShare,
        z: &SecretShare,
        payout: &[u8; 20],
        rng: R,
    ) -> ShareProof {
        z.prove(&self.user_message(maker, &z.public(), payout), rng)
    }

    pub fn verify_user(
        &self,
        maker: &PublicShare,
        user: &PublicShare,
        payout: &[u8; 20],
        proof: &ShareProof,
    ) -> Result<(), Error> {
        user.verify(&self.user_message(maker, user, payout), proof)
    }

    fn maker_message(&self, maker: &PublicShare) -> Vec<u8> {
        let mut message = self.header(MAKER_TAG);
        message.extend_from_slice(&maker.to_affine_bytes());
        message
    }

    fn user_message(&self, maker: &PublicShare, user: &PublicShare, payout: &[u8; 20]) -> Vec<u8> {
        let mut message = self.header(USER_TAG);
        message.extend_from_slice(&maker.to_affine_bytes());
        message.extend_from_slice(&user.to_affine_bytes());
        message.extend_from_slice(payout);
        message
    }

    fn header(&self, tag: &[u8; 16]) -> Vec<u8> {
        let mut message = Vec::with_capacity(16 + 8 + 20 + 32 + 64 + 64 + 20);
        message.extend_from_slice(tag);
        message.extend_from_slice(&self.chain_id.to_be_bytes());
        message.extend_from_slice(&self.contract);
        message.extend_from_slice(&self.quote_id);
        message
    }
}

#[cfg(test)]
mod tests {
    use rand_core::OsRng;

    use super::*;

    fn context() -> SwapContext {
        SwapContext {
            chain_id: 8453,
            contract: [7; 20],
            quote_id: [9; 32],
        }
    }

    #[test]
    fn maker_proof_verifies_only_in_its_context() {
        let e = SecretShare::random(OsRng);
        let proof = context().prove_maker(&e, OsRng);
        assert_eq!(context().verify_maker(&e.public(), &proof), Ok(()));

        let replayed = SwapContext {
            quote_id: [8; 32],
            ..context()
        };
        assert_eq!(
            replayed.verify_maker(&e.public(), &proof),
            Err(Error::InvalidProof)
        );
        let other_chain = SwapContext {
            chain_id: 84532,
            ..context()
        };
        assert_eq!(
            other_chain.verify_maker(&e.public(), &proof),
            Err(Error::InvalidProof)
        );
    }

    #[test]
    fn user_proof_binds_maker_share_and_payout() {
        let (e, z) = (SecretShare::random(OsRng), SecretShare::random(OsRng));
        let payout = [3; 20];
        let proof = context().prove_user(&e.public(), &z, &payout, OsRng);
        assert_eq!(
            context().verify_user(&e.public(), &z.public(), &payout, &proof),
            Ok(())
        );

        let other_maker = SecretShare::random(OsRng).public();
        assert_eq!(
            context().verify_user(&other_maker, &z.public(), &payout, &proof),
            Err(Error::InvalidProof)
        );
        assert_eq!(
            context().verify_user(&e.public(), &z.public(), &[4; 20], &proof),
            Err(Error::InvalidProof)
        );
    }

    #[test]
    fn roles_are_domain_separated() {
        let e = SecretShare::random(OsRng);
        let proof = context().prove_maker(&e, OsRng);
        let maker = SecretShare::random(OsRng).public();
        assert_eq!(
            context().verify_user(&maker, &e.public(), &[0; 20], &proof),
            Err(Error::InvalidProof)
        );
    }
}
