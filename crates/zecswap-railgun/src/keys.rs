use ark_bn254::Fr;
use ark_ff::{BigInteger, PrimeField};
use curve25519_dalek::edwards::{CompressedEdwardsY, EdwardsPoint};
use curve25519_dalek::scalar::Scalar;
use hmac::{Hmac, Mac};
use light_poseidon::{Poseidon, PoseidonHasher};
use sha2::{Digest, Sha256, Sha512};
use zeroize::Zeroizing;

use crate::note::{OutputCiphertext, Received, ShieldNote};
use crate::{Error, address, babyjubjub};

const SPENDING_PATH: [u32; 4] = [44, 1984, 0, 0];
const VIEWING_PATH: [u32; 4] = [420, 1984, 0, 0];

/// A Railgun wallet's keys, derived from a BIP-39 seed along Railgun's own paths, so the same
/// mnemonic opens the balance in any Railgun wallet.
pub struct Keys {
    viewing_key: Zeroizing<[u8; 32]>,
    receiver: Receiver,
}

/// What a payer needs to reach a wallet, as its 0zk address carries it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Receiver {
    /// `Poseidon(spending public key, nullifying key)`, big-endian.
    pub master_public_key: [u8; 32],
    /// The ed25519 public key that shield notes to this wallet are encrypted to.
    pub viewing_public_key: [u8; 32],
}

impl Keys {
    /// The wallet at `index` of `seed`, the 64-byte BIP-39 seed of the mnemonic. Railgun's
    /// wallets open index 0 by default.
    pub fn from_seed(seed: &[u8], index: u32) -> Self {
        let spending_key = derive(seed, &SPENDING_PATH, index);
        let viewing_key = derive(seed, &VIEWING_PATH, index);

        let spending_public_key = babyjubjub::public_key(&spending_key);
        let nullifying_key = poseidon(&[Fr::from_be_bytes_mod_order(&*viewing_key)]);
        let master_public_key =
            poseidon(&[spending_public_key.x, spending_public_key.y, nullifying_key]);
        let receiver = Receiver {
            master_public_key: to_bytes(&master_public_key),
            viewing_public_key: ed25519_public_key(&viewing_key),
        };
        Self {
            viewing_key,
            receiver,
        }
    }

    pub fn receiver(&self) -> Receiver {
        self.receiver
    }

    /// The wallet's `0zk` address, valid on every chain.
    pub fn address(&self) -> String {
        address::encode(&self.receiver)
    }

    /// A note paying this wallet, built from `entropy` and opened with the wallet's own viewing
    /// key before anyone commits to it: Railgun takes any ciphertext, and a note its receiver
    /// cannot open is lost.
    pub fn note(&self, entropy: &[u8; 32]) -> Result<ShieldNote, Error> {
        let note = ShieldNote::new(&self.receiver, entropy);
        match self.open(&note) {
            Some(_) => Ok(note),
            None => Err(Error::UnopenableNote),
        }
    }

    /// The `random` of a note paying this wallet, or `None` if the note is not for it. Unlike
    /// Railgun's wallets, this also checks the note public key, without which the note cannot
    /// be spent.
    pub fn open(&self, note: &ShieldNote) -> Option<[u8; 16]> {
        let random = note.decrypt_random(&self.viewing_key)?;
        (note.npk == note_public_key(&self.receiver.master_public_key, &random)).then_some(random)
    }

    /// What a transaction's output pays this wallet, if it is the note `commitment` and the
    /// wallet can open it, as Railgun's broadcasters read their fees.
    pub fn receive(&self, commitment: &[u8; 32], output: &OutputCiphertext) -> Option<Received> {
        output.open(
            &self.viewing_key,
            &self.receiver.master_public_key,
            commitment,
        )
    }
}

impl Receiver {
    pub fn address(&self) -> String {
        address::encode(self)
    }
}

/// `Poseidon(master public key, random)`: the note public key a shield commits to.
pub(crate) fn note_public_key(master_public_key: &[u8; 32], random: &[u8; 16]) -> [u8; 32] {
    to_bytes(&poseidon(&[
        Fr::from_be_bytes_mod_order(master_public_key),
        Fr::from_be_bytes_mod_order(random),
    ]))
}

/// `Poseidon(note public key, token, value)`: the commitment a transaction output adds to the
/// tree.
pub(crate) fn note_hash(npk: &[u8; 32], token: &[u8; 32], value: u128) -> [u8; 32] {
    to_bytes(&poseidon(&[
        Fr::from_be_bytes_mod_order(npk),
        Fr::from_be_bytes_mod_order(token),
        Fr::from(value),
    ]))
}

pub(crate) fn ed25519_public_key(private_key: &[u8; 32]) -> [u8; 32] {
    EdwardsPoint::mul_base(&ed25519_scalar(private_key))
        .compress()
        .to_bytes()
}

/// Railgun's ECDH: the SHA-256 of `[a]·B`, where `a` is the private key's ed25519 scalar. It is
/// not X25519. `None` if `public_key` is not a point.
pub(crate) fn shared_key(private_key: &[u8; 32], public_key: &[u8; 32]) -> Option<[u8; 32]> {
    let point = CompressedEdwardsY(*public_key).decompress()?;
    let shared = (point * ed25519_scalar(private_key)).compress();
    Some(Sha256::digest(shared.as_bytes()).into())
}

/// The ed25519 scalar of a private key: its clamped SHA-512 head, reduced.
fn ed25519_scalar(private_key: &[u8; 32]) -> Scalar {
    let mut head = Zeroizing::new([0u8; 32]);
    head.copy_from_slice(&Sha512::digest(private_key)[..32]);
    head[0] &= 0xf8;
    head[31] &= 0x7f;
    head[31] |= 0x40;
    Scalar::from_bytes_mod_order(*head)
}

/// Railgun's hardened-only BIP-32 variant, keyed "babyjubjub seed", to `path/index'`. The key
/// is the raw left half of each HMAC output, never reduced.
fn derive(seed: &[u8], path: &[u32; 4], index: u32) -> Zeroizing<[u8; 32]> {
    let (mut key, mut chain_code) = split(hmac(b"babyjubjub seed", &[seed]));
    for segment in path.iter().chain([&index]) {
        let hardened = (segment | 0x8000_0000).to_be_bytes();
        (key, chain_code) = split(hmac(&*chain_code, &[&[0], &*key, &hardened]));
    }
    key
}

type Node = (Zeroizing<[u8; 32]>, Zeroizing<[u8; 32]>);

fn split(output: Zeroizing<[u8; 64]>) -> Node {
    let (mut key, mut chain_code) = (Zeroizing::new([0; 32]), Zeroizing::new([0; 32]));
    key.copy_from_slice(&output[..32]);
    chain_code.copy_from_slice(&output[32..]);
    (key, chain_code)
}

fn hmac(key: &[u8], parts: &[&[u8]]) -> Zeroizing<[u8; 64]> {
    let mut mac = Hmac::<Sha512>::new_from_slice(key).expect("HMAC takes keys of any length");
    for part in parts {
        mac.update(part);
    }
    let mut output = Zeroizing::new([0; 64]);
    output.copy_from_slice(&mac.finalize().into_bytes());
    output
}

fn poseidon(inputs: &[Fr]) -> Fr {
    Poseidon::<Fr>::new_circom(inputs.len())
        .and_then(|mut hasher| hasher.hash(inputs))
        .expect("circom parameters exist for one to three inputs")
}

pub(crate) fn to_bytes(value: &Fr) -> [u8; 32] {
    value
        .into_bigint()
        .to_bytes_be()
        .try_into()
        .expect("a BN254 scalar is 32 bytes")
}

#[cfg(test)]
mod tests {
    use serde::Deserialize;

    use super::*;

    #[derive(Deserialize)]
    #[serde(rename_all = "camelCase")]
    struct Wallet {
        seed: String,
        index: u32,
        spending_private_key: String,
        spending_public_key: [String; 2],
        viewing_private_key: String,
        nullifying_key: String,
    }

    #[derive(Deserialize)]
    struct Vectors {
        wallets: Vec<Wallet>,
    }

    fn bytes(value: &str) -> Vec<u8> {
        hex::decode(value.trim_start_matches("0x")).unwrap()
    }

    /// Intermediate keys, which the integration tests only see through the address.
    #[test]
    fn derivation_steps_match_railgun_engine() {
        let vectors: Vectors =
            serde_json::from_str(include_str!("../tests/engine-vectors.json")).unwrap();
        for wallet in &vectors.wallets {
            let seed = bytes(&wallet.seed);
            let spending_key = derive(&seed, &SPENDING_PATH, wallet.index);
            let viewing_key = derive(&seed, &VIEWING_PATH, wallet.index);
            assert_eq!(spending_key.to_vec(), bytes(&wallet.spending_private_key));
            assert_eq!(viewing_key.to_vec(), bytes(&wallet.viewing_private_key));

            let public_key = babyjubjub::public_key(&spending_key);
            assert_eq!(
                [
                    to_bytes(&public_key.x).to_vec(),
                    to_bytes(&public_key.y).to_vec()
                ],
                wallet
                    .spending_public_key
                    .clone()
                    .map(|coordinate| bytes(&coordinate))
            );
            let nullifying_key = poseidon(&[Fr::from_be_bytes_mod_order(&*viewing_key)]);
            assert_eq!(
                to_bytes(&nullifying_key).to_vec(),
                bytes(&wallet.nullifying_key)
            );
        }
    }
}
