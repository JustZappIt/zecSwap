//! Deposit notes as `docs/deposits.md` has a wallet build them, through the bindings: the known
//! answers the Android port checks (`android/src/test`).

use zecswap::ops::deposit_note;
use zecswap_core::{NetworkType, derive_deposit_keys};
use zecswap_railgun::{Keys as RailgunKeys, ShieldCiphertext, ShieldNote};

const SEED: [u8; 64] = [7; 64];
const RAILGUN_SEED: [u8; 64] = [8; 64];
const VALID_BEFORE: u64 = 1_790_000_000;

fn hex(bytes: &[u8]) -> String {
    format!("0x{}", hex::encode(bytes))
}

/// The note in the first five words of a binding's answer.
fn note(bytes: &[u8]) -> ShieldNote {
    let words: Vec<[u8; 32]> = bytes
        .chunks(32)
        .map(|word| word.try_into().unwrap())
        .collect();
    assert_eq!(words.len(), 6);
    ShieldNote {
        npk: words[0],
        ciphertext: ShieldCiphertext {
            encrypted_bundle: [words[1], words[2], words[3]],
            shield_key: words[4],
        },
    }
}

fn shield(mainnet: bool, account: u32, index: u32, valid_before: u64) -> Vec<u8> {
    deposit_note(
        &SEED,
        mainnet,
        account,
        index,
        &RAILGUN_SEED,
        false,
        valid_before,
    )
    .unwrap()
}

fn rescue(index: u32, nonce: u64) -> Vec<u8> {
    deposit_note(&SEED, false, 0, index, &RAILGUN_SEED, true, nonce).unwrap()
}

#[test]
fn a_shield_note_is_the_known_answer() {
    let words: Vec<_> = shield(false, 0, 0, VALID_BEFORE)
        .chunks(32)
        .map(hex)
        .collect();
    assert_eq!(
        words,
        [
            "0x15f85d1d25c867703e280aca9580d5e32936dab954250744893bf967f9b86758",
            "0x8da3b2fca02e90a1ff4e8c84d2997579485257500db5935a9d39f1480650a2d5",
            "0x6d9e51f2199703b7ae13b6435d63e1e232ba0c4cb64ac4f7d5cfcd8d1b5e55c7",
            "0x1881c394f5d1926107f78fc402ade1354aa2d2a70021868691fd74d50dcc9298",
            "0xdfd7ff431856fb4cfb9de4047e42f9481270266327a4c11361abe5f8b648a6dd",
            "0x7544c517c31d651b094aa7d6eae02d4265f711a56cc4e0e11533cac7539bfa14",
        ]
    );
}

/// The first rescue of address 0's vault, nonce 0.
#[test]
fn a_rescue_note_is_the_known_answer() {
    let words: Vec<_> = rescue(0, 0).chunks(32).map(hex).collect();
    assert_eq!(
        words,
        [
            "0x2cb930fd875166de6013f1de6c85b7e3c8f92e056e397580a3353ca93c61de45",
            "0xbc4c816d390cea4ce439b2bb5de87e6d2627b2c25f332248750a614e1c4a7ad6",
            "0xbb1a50eaa74ed61b1fa7c0f40cfe709fb4176642e6939bb743d1b35dc8ab920b",
            "0xd7fa1977849a1bf55fa460e415909e3390aaef0c894d8836a693f4629c16ecfa",
            "0x2b19ea4d15e10221fba6ef9f984a38405dfb7affa618116f4bb16b537dd0ba1e",
            "0xc3eee5aa58bc19bf3b51b253422437ef3f85202c73154981583fcd619f0b329d",
        ]
    );
}

#[test]
fn a_note_opens_in_the_railgun_seeds_wallet_alone_and_ends_with_its_commitment() {
    for bytes in [shield(false, 0, 0, VALID_BEFORE), rescue(0, 0)] {
        let note = note(&bytes);
        assert!(
            RailgunKeys::from_seed(&RAILGUN_SEED, 0)
                .open(&note)
                .is_some()
        );
        assert!(RailgunKeys::from_seed(&SEED, 0).open(&note).is_none());
        assert_eq!(bytes[160..], note.commitment());
    }
}

#[test]
fn a_note_is_the_deposit_keys_entropy_fed_to_the_railgun_keys() {
    let railgun = RailgunKeys::from_seed(&RAILGUN_SEED, 0);
    let keys = derive_deposit_keys(&SEED, NetworkType::Test, 0, 3).unwrap();
    assert_eq!(
        note(&shield(false, 0, 3, VALID_BEFORE)),
        railgun.note(&keys.shield_entropy(VALID_BEFORE)).unwrap()
    );
    assert_eq!(
        note(&rescue(3, 7)),
        railgun.note(&keys.rescue_entropy(7)).unwrap()
    );
    let mainnet = derive_deposit_keys(&SEED, NetworkType::Main, 2, 3).unwrap();
    assert_eq!(
        note(&shield(true, 2, 3, VALID_BEFORE)),
        railgun.note(&mainnet.shield_entropy(VALID_BEFORE)).unwrap()
    );
}

#[test]
fn equal_inputs_repeat_a_note_and_number_kind_index_account_and_network_each_change_it() {
    let base = shield(false, 0, 0, VALID_BEFORE);
    assert_eq!(base, shield(false, 0, 0, VALID_BEFORE));
    for other in [
        shield(false, 0, 0, VALID_BEFORE + 1),
        rescue(0, VALID_BEFORE),
        shield(false, 0, 1, VALID_BEFORE),
        shield(false, 1, 0, VALID_BEFORE),
        shield(true, 0, 0, VALID_BEFORE),
    ] {
        assert_ne!(other, base);
    }
}

#[test]
fn seeds_are_64_bytes_and_indices_within_bip32() {
    assert!(deposit_note(&[7; 32], false, 0, 0, &RAILGUN_SEED, false, 0).is_err());
    assert!(deposit_note(&SEED, false, 0, 0, &[8; 32], false, 0).is_err());
    assert!(deposit_note(&SEED, false, 0, 1 << 31, &RAILGUN_SEED, false, 0).is_err());
    assert!(deposit_note(&SEED, false, u32::MAX, 0, &RAILGUN_SEED, false, 0).is_err());
}
