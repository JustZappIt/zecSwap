use aes::Aes256;
use aes_gcm::AesGcm;
use aes_gcm::aead::consts::U16;
use aes_gcm::aead::generic_array::GenericArray;
use aes_gcm::aead::{AeadInPlace, KeyInit};
use ctr::cipher::{KeyIvInit, StreamCipher};
use sha2::{Digest, Sha512};
use sha3::Keccak256;
use zeroize::Zeroizing;

use crate::keys::{Receiver, ed25519_public_key, note_hash, note_public_key, shared_key};

/// Railgun encrypts notes with AES-256-GCM under a 16-byte IV.
type Gcm = AesGcm<Aes256, U16>;
type Ctr = ctr::Ctr128BE<Aes256>;

/// The part of a Railgun `ShieldRequest` that decides who can spend it: the note public key,
/// and the ciphertext its receiver finds it by. The token and value go in when it is shielded,
/// since the ciphertext binds neither.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ShieldNote {
    pub npk: [u8; 32],
    pub ciphertext: ShieldCiphertext,
}

/// Railgun's `ShieldCiphertext`.
///
/// The bundle holds the GCM IV and tag, then `random` encrypted to the receiver's viewing key
/// with the CTR IV, then the receiver's viewing key encrypted to the shielder. `shield_key` is
/// the ed25519 public key the receiver's side of the key agreement uses.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ShieldCiphertext {
    pub encrypted_bundle: [[u8; 32]; 3],
    pub shield_key: [u8; 32],
}

impl ShieldNote {
    /// A note paying `receiver`, every secret of it drawn from `entropy`: the same entropy
    /// always builds the same note. Use fresh entropy for each note, since a reused shield key
    /// links the shields that share it.
    pub(crate) fn new(receiver: &Receiver, entropy: &[u8; 32]) -> Self {
        let random: [u8; 16] = *expand(entropy, b"random").first_chunk().expect("64 bytes");
        let mut shield_private_key = Zeroizing::new([0u8; 32]);
        shield_private_key.copy_from_slice(&expand(entropy, b"shield key")[..32]);
        let ivs = expand(entropy, b"iv");
        let (gcm_iv, ctr_iv) = (&ivs[..16], &ivs[16..32]);

        let key = shared_key(&shield_private_key, &receiver.viewing_public_key)
            .map(Zeroizing::new)
            .expect("a derived viewing public key is a point");
        let mut encrypted_random = random;
        let tag = Gcm::new(GenericArray::from_slice(&*key))
            .encrypt_in_place_detached(GenericArray::from_slice(gcm_iv), &[], &mut encrypted_random)
            .expect("16 bytes are within GCM's limit");

        let mut encrypted_receiver = receiver.viewing_public_key;
        Ctr::new(
            GenericArray::from_slice(&*shield_private_key),
            GenericArray::from_slice(ctr_iv),
        )
        .apply_keystream(&mut encrypted_receiver);

        Self {
            npk: note_public_key(&receiver.master_public_key, &random),
            ciphertext: ShieldCiphertext {
                encrypted_bundle: [
                    concat(gcm_iv, &tag),
                    concat(&encrypted_random, ctr_iv),
                    encrypted_receiver,
                ],
                shield_key: ed25519_public_key(&shield_private_key),
            },
        }
    }

    /// `keccak256(abi.encode(npk, encryptedBundle, shieldKey))`: what a swap commits its payout
    /// to when it opens.
    pub fn commitment(&self) -> [u8; 32] {
        let mut hasher = Keccak256::new();
        hasher.update(self.npk);
        for word in &self.ciphertext.encrypted_bundle {
            hasher.update(word);
        }
        hasher.update(self.ciphertext.shield_key);
        hasher.finalize().into()
    }

    /// The note's `random`, if the holder of `viewing_key` can decrypt it.
    pub(crate) fn decrypt_random(&self, viewing_key: &[u8; 32]) -> Option<[u8; 16]> {
        let key = Zeroizing::new(shared_key(viewing_key, &self.ciphertext.shield_key)?);
        let [iv_and_tag, random_and_ctr_iv, _] = &self.ciphertext.encrypted_bundle;
        let mut random: [u8; 16] = *random_and_ctr_iv.first_chunk().expect("32 bytes");
        Gcm::new(GenericArray::from_slice(&*key))
            .decrypt_in_place_detached(
                GenericArray::from_slice(&iv_and_tag[..16]),
                &[],
                &mut random,
                GenericArray::from_slice(&iv_and_tag[16..]),
            )
            .ok()?;
        Some(random)
    }
}

/// One output of a Railgun transaction as its `CommitmentCiphertext` carries it (V2): the note
/// encrypted to its receiver under a key agreed with the sender's blinded viewing key.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct OutputCiphertext {
    /// The GCM IV and tag, then the note: its encoded master public key, its token, and its
    /// random and value.
    pub ciphertext: [[u8; 32]; 4],
    pub blinded_sender_viewing_key: [u8; 32],
    /// The encrypted memo, which continues the GCM stream and is covered by its tag.
    pub memo: Vec<u8>,
}

/// What a transaction output pays the wallet that opens it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Received {
    /// The token's hash: for an ERC-20, its address left-padded to 32 bytes.
    pub token: [u8; 32],
    pub value: u128,
}

impl OutputCiphertext {
    /// The output, if the holder of `viewing_key` can decrypt it and it is the note
    /// `commitment` paying `master_public_key`, so that wallet can find and spend it.
    pub(crate) fn open(
        &self,
        viewing_key: &[u8; 32],
        master_public_key: &[u8; 32],
        commitment: &[u8; 32],
    ) -> Option<Received> {
        let key = Zeroizing::new(shared_key(viewing_key, &self.blinded_sender_viewing_key)?);
        let [iv_and_tag, note @ ..] = &self.ciphertext;
        let mut plaintext = Zeroizing::new(note.concat());
        plaintext.extend_from_slice(&self.memo);
        Gcm::new(GenericArray::from_slice(&*key))
            .decrypt_in_place_detached(
                GenericArray::from_slice(&iv_and_tag[..16]),
                &[],
                &mut plaintext[..],
                GenericArray::from_slice(&iv_and_tag[16..]),
            )
            .ok()?;
        let token: [u8; 32] = plaintext[32..64].try_into().expect("32 bytes");
        let random: [u8; 16] = plaintext[64..80].try_into().expect("16 bytes");
        let value = u128::from_be_bytes(plaintext[80..96].try_into().expect("16 bytes"));
        let npk = note_public_key(master_public_key, &random);
        (note_hash(&npk, &token, value) == *commitment).then_some(Received { token, value })
    }
}

fn expand(entropy: &[u8; 32], label: &[u8]) -> Zeroizing<[u8; 64]> {
    let mut output = Zeroizing::new([0; 64]);
    output.copy_from_slice(
        &Sha512::new()
            .chain_update(b"ZecSwap/railgun/")
            .chain_update(label)
            .chain_update(entropy)
            .finalize(),
    );
    output
}

fn concat(left: &[u8], right: &[u8]) -> [u8; 32] {
    let mut word = [0; 32];
    word[..16].copy_from_slice(left);
    word[16..].copy_from_slice(right);
    word
}
