//! What each binding computes, over plain bytes, so it runs and is tested without a JVM. Every
//! call derives the swap's keys from the seed again: nothing secret outlives it.

use rand_core::OsRng;
use zecswap_core::{
    Domain, JointAccount, NetworkType, Payout, PublicShare, SecretShare, ShareProof, SwapContext,
    UserSwapKeys, derive_user_keys, sign_pczt_bytes,
};
use zecswap_railgun::{Keys as RailgunKeys, ShieldNote};

/// The 64-byte BIP-39 seed of the wallet's mnemonic, the bytes the Zcash SDK derives from.
pub const SEED_BYTES: usize = 64;
/// Railgun's wallets open the first wallet of a seed, and swaps pay into it.
const RAILGUN_WALLET: u32 = 0;

pub type Result<T> = core::result::Result<T, String>;

/// Swap number `index` of the wallet whose seed is `seed`.
pub struct Swap<'a> {
    seed: &'a [u8],
    network: NetworkType,
    index: u32,
}

impl<'a> Swap<'a> {
    pub fn new(seed: &'a [u8], mainnet: bool, index: i32) -> Result<Self> {
        check_seed(seed)?;
        let index = u32::try_from(index).map_err(|_| format!("a swap index can't be {index}"))?;
        let network = if mainnet {
            NetworkType::Main
        } else {
            NetworkType::Test
        };
        Ok(Self {
            seed,
            network,
            index,
        })
    }

    /// `Z`, the public share a quote is accepted with.
    pub fn user_share(&self) -> Result<[u8; 64]> {
        Ok(self.keys()?.share.public().to_affine_bytes())
    }

    /// The swap's `user` on the contract: a key of its own that signs for it and is never funded.
    pub fn auth_address(&self) -> Result<[u8; 20]> {
        Ok(self.keys()?.auth.address())
    }

    /// `z`, which a claim reveals.
    pub fn claim_secret(&self) -> Result<[u8; 32]> {
        Ok(self.keys()?.share.to_be_bytes())
    }

    /// The Railgun note the payout is shielded to, as `npk ‖ encryptedBundle ‖ shieldKey ‖
    /// commitment`: the quote names the commitment and the relayer sends the rest.
    pub fn payout_note(&self) -> Result<Vec<u8>> {
        let note = self.note(&self.keys()?)?;
        let mut bytes = Vec::with_capacity(6 * 32);
        bytes.extend_from_slice(&note.npk);
        note.ciphertext
            .encrypted_bundle
            .iter()
            .for_each(|word| bytes.extend_from_slice(word));
        bytes.extend_from_slice(&note.ciphertext.shield_key);
        bytes.extend_from_slice(&note.commitment());
        Ok(bytes)
    }

    /// Checks the maker's proof of `E`, then proves `Z` bound to both shares and the payout.
    /// Returns `userShare ‖ userProof ‖ viewingKeys`, what the quote's accept call takes.
    pub fn accept(
        &self,
        context: SwapContext,
        maker_share: &[u8; 64],
        maker_proof: &[u8; 64],
    ) -> Result<Vec<u8>> {
        let keys = self.keys()?;
        let maker = PublicShare::from_affine_bytes(maker_share).map_err(error)?;
        context
            .verify_maker(&maker, &ShareProof::from_bytes(*maker_proof))
            .map_err(|e| format!("the maker's share proof: {e}"))?;
        let payout = Payout {
            user: keys.auth.address(),
            note: Some(self.note(&keys)?.commitment()),
        };
        let proof = context.prove_user(&maker, &keys.share, &payout, OsRng);
        let mut bytes = Vec::with_capacity(3 * 64);
        bytes.extend_from_slice(&keys.share.public().to_affine_bytes());
        bytes.extend_from_slice(&proof.to_bytes());
        bytes.extend_from_slice(&keys.viewing.to_bytes());
        Ok(bytes)
    }

    /// The deposit account as `[unified address, UFVK]`, from the maker's share as the contract
    /// records it, never as the quote API reports it.
    pub fn deposit_account(&self, maker_share: &[u8; 64]) -> Result<[String; 2]> {
        let joint = self.joint(&self.keys()?, maker_share)?;
        Ok([
            joint.unified_address(self.network),
            joint.ufvk(self.network),
        ])
    }

    /// `r ‖ s ‖ v` over the EIP-712 `LockClaim(id, deadline)`.
    pub fn sign_lock_claim(
        &self,
        domain: Domain,
        swap_id: &[u8; 32],
        deadline: u64,
    ) -> Result<[u8; 65]> {
        Ok(self
            .keys()?
            .auth
            .sign(&domain.lock_claim(swap_id, deadline)))
    }

    /// `r ‖ s ‖ v` over the EIP-712 `Payout(id, relayer, fee)`.
    pub fn sign_payout(
        &self,
        domain: Domain,
        swap_id: &[u8; 32],
        relayer: &[u8; 20],
        fee: u128,
    ) -> Result<[u8; 65]> {
        Ok(self
            .keys()?
            .auth
            .sign(&domain.payout(swap_id, relayer, fee)))
    }

    /// Signs the refund sweep in `pczt` with `e + z`, once the contract has revealed the maker's
    /// `e`. Fails unless `e` matches the recorded maker share and the PCZT spends nothing but the
    /// deposit account.
    pub fn sign_refund(
        &self,
        maker_share: &[u8; 64],
        maker_secret: &[u8; 32],
        pczt: &[u8],
    ) -> Result<Vec<u8>> {
        let keys = self.keys()?;
        let joint = self.joint(&keys, maker_share)?;
        let e = SecretShare::from_be_bytes(maker_secret).map_err(error)?;
        let key = joint.spend_key(&e, &keys.share).map_err(error)?;
        sign_pczt_bytes(pczt, &[key]).map_err(error)
    }

    fn keys(&self) -> Result<UserSwapKeys> {
        derive_user_keys(self.seed, self.network, 0, self.index).map_err(error)
    }

    fn note(&self, keys: &UserSwapKeys) -> Result<ShieldNote> {
        railgun(self.seed).note(&keys.note_entropy).map_err(error)
    }

    fn joint(&self, keys: &UserSwapKeys, maker_share: &[u8; 64]) -> Result<JointAccount> {
        let maker = PublicShare::from_affine_bytes(maker_share).map_err(error)?;
        JointAccount::derive(&maker, &keys.share.public(), &keys.viewing).map_err(error)
    }
}

/// The `0zk` address payouts go to: the Railgun wallet the same mnemonic opens in Railgun's own
/// wallets.
pub fn railgun_address(seed: &[u8]) -> Result<String> {
    check_seed(seed)?;
    Ok(railgun(seed).address())
}

fn railgun(seed: &[u8]) -> RailgunKeys {
    RailgunKeys::from_seed(seed, RAILGUN_WALLET)
}

fn check_seed(seed: &[u8]) -> Result<()> {
    match seed.len() {
        SEED_BYTES => Ok(()),
        length => Err(format!("the seed must be {SEED_BYTES} bytes, not {length}")),
    }
}

fn error(e: impl core::fmt::Display) -> String {
    e.to_string()
}
