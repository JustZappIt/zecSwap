use bip32::{ChildNumber, ExtendedPrivateKey};
use blake2b_simd::Params;
use k256::ecdsa::SigningKey;
use zcash_protocol::consensus::NetworkType;
use zeroize::Zeroizing;

use crate::Error;
use crate::auth::{address_of, sign};

/// Deposit addresses take BIP-44 account numbers from here on, above any a wallet opens by
/// default: MetaMask's `m/44'/60'/0'/0/i` and Ledger Live's `m/44'/60'/x'/0/0` count up from 0.
pub const DEPOSIT_ACCOUNTS: u32 = 1000;

const NOTES_PERSONALIZATION: &[u8; 16] = b"ZecSwap_Deposits";
const NOTE_PERSONALIZATION: &[u8; 16] = b"ZecSwap_DepoNote";

/// Deposit address `index` of a wallet's account: its key, and what the notes its deposits pay
/// are built from.
pub struct DepositKeys {
    pub key: DepositKey,
    notes: Zeroizing<[u8; 32]>,
}

/// A deposit address's Ethereum key, at BIP-44's `m/44'/60'/(1000 + account)'/0/index` on
/// mainnet and `m/44'/1'/(1000 + account)'/0/index` on testnet, so any wallet that takes a
/// derivation path finds it from the seed. It signs only what the Deposits contract takes: never
/// delegate it (EIP-7702) or use the address for anything else.
pub struct DepositKey(SigningKey);

impl DepositKey {
    pub fn address(&self) -> [u8; 20] {
        address_of(self.0.verifying_key())
    }

    /// `r ‖ s ‖ v` over a `TokenDomain` or `DepositsDomain` digest, as they are verified.
    pub fn sign(&self, digest: &[u8; 32]) -> [u8; 65] {
        sign(&self.0, digest)
    }
}

/// Derives deposit address `index` of `account`, the wallet's ZIP 32 account, from the wallet's
/// BIP-39 seed. Testnet addresses are under another coin type, unlinked to mainnet's.
pub fn derive_deposit_keys(
    seed: &[u8],
    network: NetworkType,
    account: u32,
    index: u32,
) -> Result<DepositKeys, Error> {
    let coin_type: u32 = match network {
        NetworkType::Main => 60,
        NetworkType::Test | NetworkType::Regtest => 1,
    };
    let bip44_account = account
        .checked_add(DEPOSIT_ACCOUNTS)
        .ok_or(Error::InvalidDerivation)?;
    let path = [
        (44, true),
        (coin_type, true),
        (bip44_account, true),
        (0, false),
        (index, false),
    ];
    let mut node = ExtendedPrivateKey::<secp256k1::SecretKey>::new(seed)
        .map_err(|_| Error::InvalidDerivation)?;
    for (number, hardened) in path {
        let child = ChildNumber::new(number, hardened).map_err(|_| Error::InvalidDerivation)?;
        node = node
            .derive_child(child)
            .map_err(|_| Error::InvalidDerivation)?;
    }
    let secret = Zeroizing::new(node.to_bytes());
    let key = SigningKey::from_bytes(&(*secret).into()).map_err(|_| Error::InvalidScalar)?;

    let mut notes = Zeroizing::new([0; 32]);
    notes.copy_from_slice(
        Params::new()
            .hash_length(32)
            .personal(NOTES_PERSONALIZATION)
            .to_state()
            .update(seed)
            .update(&coin_type.to_le_bytes())
            .update(&account.to_le_bytes())
            .update(&index.to_le_bytes())
            .finalize()
            .as_bytes(),
    );
    Ok(DepositKeys {
        key: DepositKey(key),
        notes,
    })
}

impl DepositKeys {
    /// What the note a shield of this address pays is built from (`zecswap_railgun::Keys::note`),
    /// for an authorization valid before `valid_before`. Each signing has its own, so no two
    /// shields of one address share a note or a nonce, and the persisted request rebuilds it.
    pub fn shield_entropy(&self, valid_before: u64) -> Zeroizing<[u8; 32]> {
        self.entropy(0, valid_before)
    }

    /// What the note the rescue with `nonce` of this address's vault pays is built from.
    pub fn rescue_entropy(&self, nonce: u64) -> Zeroizing<[u8; 32]> {
        self.entropy(1, nonce)
    }

    fn entropy(&self, purpose: u8, number: u64) -> Zeroizing<[u8; 32]> {
        let mut entropy = Zeroizing::new([0; 32]);
        entropy.copy_from_slice(
            Params::new()
                .hash_length(32)
                .personal(NOTE_PERSONALIZATION)
                .to_state()
                .update(&*self.notes)
                .update(&[purpose])
                .update(&number.to_le_bytes())
                .finalize()
                .as_bytes(),
        );
        entropy
    }
}

#[cfg(test)]
mod tests {
    use serde::Deserialize;

    use super::*;

    #[derive(Deserialize)]
    struct Vector {
        seed: String,
        network: String,
        account: u32,
        index: u32,
        path: String,
        address: String,
    }

    fn hex(text: &str) -> Vec<u8> {
        (2..text.len())
            .step_by(2)
            .map(|i| u8::from_str_radix(&text[i..i + 2], 16).unwrap())
            .collect()
    }

    /// The addresses ethers derives from the same seeds and paths (`tests/deposit-vectors.json`,
    /// from crates/zecswap-railgun/engine/deposit-vectors.cjs): other wallets find these.
    #[test]
    fn deposit_addresses_are_the_ones_ethers_derives() {
        let vectors: Vec<Vector> =
            serde_json::from_str(include_str!("../tests/deposit-vectors.json")).unwrap();
        assert!(vectors.len() >= 8);
        for vector in &vectors {
            let network = match vector.network.as_str() {
                "mainnet" => NetworkType::Main,
                _ => NetworkType::Test,
            };
            let coin = if network == NetworkType::Main { 60 } else { 1 };
            assert_eq!(
                vector.path,
                format!(
                    "m/44'/{coin}'/{}'/0/{}",
                    DEPOSIT_ACCOUNTS + vector.account,
                    vector.index
                )
            );
            let keys =
                derive_deposit_keys(&hex(&vector.seed), network, vector.account, vector.index)
                    .unwrap();
            assert_eq!(
                keys.key.address().to_vec(),
                hex(&vector.address.to_lowercase()),
                "{}",
                vector.path
            );
        }
    }

    #[test]
    fn index_account_network_and_seed_separate_addresses_and_notes() {
        let seed = [42; 64];
        let base = derive_deposit_keys(&seed, NetworkType::Main, 0, 0).unwrap();
        for other in [
            derive_deposit_keys(&seed, NetworkType::Main, 0, 1),
            derive_deposit_keys(&seed, NetworkType::Main, 1, 0),
            derive_deposit_keys(&seed, NetworkType::Test, 0, 0),
            derive_deposit_keys(&[43; 64], NetworkType::Main, 0, 0),
        ] {
            let other = other.unwrap();
            assert_ne!(other.key.address(), base.key.address());
            assert_ne!(*other.shield_entropy(5), *base.shield_entropy(5));
        }
        assert_ne!(*base.shield_entropy(5), *base.shield_entropy(6));
        assert_ne!(*base.shield_entropy(5), *base.rescue_entropy(5));
        let again = derive_deposit_keys(&seed, NetworkType::Main, 0, 0).unwrap();
        assert_eq!(*again.shield_entropy(5), *base.shield_entropy(5));
    }

    #[test]
    fn indices_bip32_cant_reach_are_refused() {
        let seed = [42; 64];
        for (account, index) in [
            (u32::MAX, 0),
            ((1 << 31) - DEPOSIT_ACCOUNTS, 0),
            (0, 1 << 31),
        ] {
            assert!(matches!(
                derive_deposit_keys(&seed, NetworkType::Main, account, index),
                Err(Error::InvalidDerivation)
            ));
        }
        assert!(derive_deposit_keys(&[1; 20], NetworkType::Main, 0, 0).is_err());
    }
}
