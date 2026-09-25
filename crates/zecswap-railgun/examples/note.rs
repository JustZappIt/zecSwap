//! Shield notes built here, one JSON line each, for Railgun's engine to open:
//!   cargo run -p zecswap-railgun --example note | node crates/zecswap-railgun/engine/check.cjs
//! The first is the contracts' fork-test note (`contracts/test/vectors/railgun_note.json`).

use serde::Deserialize;
use serde_json::json;
use zecswap_railgun::Keys;

#[derive(Deserialize)]
struct Wallet {
    seed: String,
}

#[derive(Deserialize)]
struct Vectors {
    wallets: Vec<Wallet>,
}

fn hex(bytes: &[u8]) -> String {
    format!("0x{}", ::hex::encode(bytes))
}

fn main() {
    let vectors: Vectors =
        serde_json::from_str(include_str!("../tests/engine-vectors.json")).unwrap();
    let mut seeds: Vec<&str> = vectors.wallets.iter().map(|w| w.seed.as_str()).collect();
    seeds.dedup();

    for seed in seeds {
        for index in [0, 1, 7] {
            let keys = Keys::from_seed(&::hex::decode(&seed[2..]).unwrap(), index);
            for entropy in 0..4u8 {
                let note = keys.note(&[entropy; 32]).expect("a note for these keys");
                let random = keys.open(&note).expect("the note opens");
                let bundle = note.ciphertext.encrypted_bundle.map(|word| hex(&word));
                println!(
                    "{}",
                    json!({
                        "seed": seed,
                        "index": index,
                        "address": keys.address(),
                        "random": hex(&random),
                        "npk": hex(&note.npk),
                        "encryptedBundle": bundle,
                        "shieldKey": hex(&note.ciphertext.shield_key),
                        "commitment": hex(&note.commitment()),
                    })
                );
            }
        }
    }
}
