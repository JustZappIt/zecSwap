//! Known answers from Railgun's engine (`engine/vectors.cjs`): the same seed gives the same
//! address, and notes and transaction outputs the engine builds open here.

use serde::Deserialize;
use zecswap_railgun::{Keys, OutputCiphertext, Received, ShieldCiphertext, ShieldNote};

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct Wallet {
    seed: String,
    index: u32,
    viewing_public_key: String,
    master_public_key: String,
    address: String,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct Note {
    receiver: usize,
    random: String,
    npk: String,
    encrypted_bundle: [String; 3],
    shield_key: String,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct Output {
    receiver: usize,
    commitment: String,
    token: String,
    value: String,
    ciphertext: [String; 4],
    blinded_sender_viewing_key: String,
    memo: String,
}

#[derive(Deserialize)]
struct Vectors {
    wallets: Vec<Wallet>,
    notes: Vec<Note>,
    outputs: Vec<Output>,
}

fn vectors() -> Vectors {
    serde_json::from_str(include_str!("engine-vectors.json")).unwrap()
}

fn bytes<const N: usize>(value: &str) -> [u8; N] {
    hex::decode(value.trim_start_matches("0x"))
        .unwrap()
        .try_into()
        .unwrap()
}

fn keys(wallet: &Wallet) -> Keys {
    Keys::from_seed(&hex::decode(&wallet.seed[2..]).unwrap(), wallet.index)
}

#[test]
fn keys_and_addresses_match_the_engine() {
    for wallet in &vectors().wallets {
        let keys = keys(wallet);
        let receiver = keys.receiver();
        assert_eq!(receiver.master_public_key, bytes(&wallet.master_public_key));
        assert_eq!(
            receiver.viewing_public_key,
            bytes(&wallet.viewing_public_key)
        );
        assert_eq!(keys.address(), wallet.address);
    }
}

#[test]
fn opens_notes_the_engine_built_for_it_only() {
    let vectors = vectors();
    for note in &vectors.notes {
        let shield = ShieldNote {
            npk: bytes(&note.npk),
            ciphertext: ShieldCiphertext {
                encrypted_bundle: note.encrypted_bundle.each_ref().map(|word| bytes(word)),
                shield_key: bytes(&note.shield_key),
            },
        };
        for (index, wallet) in vectors.wallets.iter().enumerate() {
            let expected = (index == note.receiver).then(|| bytes(&note.random));
            assert_eq!(keys(wallet).open(&shield), expected);
        }
    }
}

/// Outputs the engine encrypted, a hidden sender's fee and a visible sender's transfer with a
/// memo: only the receiver reads them, only as the note committed to, and a changed memo or
/// note word fails the GCM tag.
#[test]
fn receives_outputs_the_engine_encrypted_for_it_only() {
    let vectors = vectors();
    for output in &vectors.outputs {
        let ciphertext = OutputCiphertext {
            ciphertext: output.ciphertext.each_ref().map(|word| bytes(word)),
            blinded_sender_viewing_key: bytes(&output.blinded_sender_viewing_key),
            memo: hex::decode(output.memo.trim_start_matches("0x")).unwrap(),
        };
        let commitment = bytes(&output.commitment);
        for (index, wallet) in vectors.wallets.iter().enumerate() {
            let expected = (index == output.receiver).then(|| Received {
                token: bytes(&output.token),
                value: output.value.parse().unwrap(),
            });
            assert_eq!(keys(wallet).receive(&commitment, &ciphertext), expected);
        }

        let receiver = keys(&vectors.wallets[output.receiver]);
        let mut other_note = commitment;
        other_note[31] ^= 1;
        assert_eq!(receiver.receive(&other_note, &ciphertext), None);
        let mut memo = ciphertext.clone();
        memo.memo.push(0);
        assert_eq!(receiver.receive(&commitment, &memo), None);
        for word in 0..4 {
            let mut tampered = ciphertext.clone();
            tampered.ciphertext[word][7] ^= 1;
            assert_eq!(receiver.receive(&commitment, &tampered), None);
        }
    }
}

#[test]
fn notes_built_here_open_for_their_receiver_only() {
    let vectors = vectors();
    let (receiver, other) = (keys(&vectors.wallets[0]), keys(&vectors.wallets[1]));
    let note = receiver.note(&[1; 32]).unwrap();
    assert!(receiver.open(&note).is_some());
    assert_eq!(other.open(&note), None);

    assert_eq!(
        receiver.note(&[1; 32]).unwrap(),
        note,
        "the same entropy builds the same note"
    );
    let fresh = receiver.note(&[2; 32]).unwrap();
    assert_ne!(fresh.npk, note.npk);
    assert_ne!(fresh.ciphertext.shield_key, note.ciphertext.shield_key);
    assert_ne!(fresh.commitment(), note.commitment());
}

#[test]
fn tampered_notes_do_not_open() {
    let keys = keys(&vectors().wallets[0]);
    let note = keys.note(&[3; 32]).unwrap();
    // The GCM IV and tag, then the encrypted random; the rest of the bundle is for the shielder.
    for (word, byte) in [(0, 0), (0, 15), (0, 16), (0, 31), (1, 0), (1, 15)] {
        let mut tampered = note;
        tampered.ciphertext.encrypted_bundle[word][byte] ^= 1;
        assert_eq!(keys.open(&tampered), None);
    }
    let mut wrong_npk = note;
    wrong_npk.npk[31] ^= 1;
    assert_eq!(keys.open(&wrong_npk), None);
}
