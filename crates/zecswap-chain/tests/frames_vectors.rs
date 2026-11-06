//! The frame transaction encoder against @forshtat/hegota-support's answers for the frames devnet
//! (frames/scripts/vectors.mjs), and its signatures against the signer they name.

use alloy::primitives::{Address, Bytes, U256};
use serde_json::Value;
use zecswap_chain::evm::frames::{Frame, FrameTx, Mode};
use zecswap_core::{NetworkType, derive_user_keys, signer};

fn vectors() -> Vec<Value> {
    let json: Value = serde_json::from_str(include_str!("frame-vectors.json")).unwrap();
    assert_eq!(json["profile"], "ethrex-eip8141-tracer");
    json["vectors"].as_array().unwrap().clone()
}

fn number(v: &Value) -> u128 {
    u128::from_str_radix(v.as_str().unwrap().trim_start_matches("0x"), 16).unwrap()
}

fn unsigned(v: &Value) -> FrameTx {
    let frames = v["frames"].as_array().unwrap().iter().map(|f| Frame {
        mode: match f["mode"].as_u64().unwrap() {
            0 => Mode::Default,
            1 => Mode::Verify,
            2 => Mode::Sender,
            mode => panic!("mode {mode}"),
        },
        flags: f["flags"].as_u64().unwrap() as u8,
        target: f["target"].as_str().map(|t| t.parse().unwrap()),
        execution: number(&f["execution"]) as u64,
        state: number(&f["state"]) as u64,
        value: U256::from(number(&f["value"])),
        data: f["data"].as_str().unwrap().parse().unwrap(),
    });
    FrameTx {
        chain_id: v["chainId"].as_u64().unwrap(),
        nonce: number(&v["nonce"]) as u64,
        sender: v["sender"].as_str().unwrap().parse().unwrap(),
        frames: frames.collect(),
        max_priority_fee_per_gas: number(&v["maxPriorityFeePerGas"]),
        max_fee_per_gas: number(&v["maxFeePerGas"]),
        signature: Bytes::new(),
    }
}

#[test]
fn encodes_hashes_and_wires_as_the_devnet_library_does() {
    for v in vectors() {
        let name = v["name"].as_str().unwrap();
        let mut tx = unsigned(&v);
        assert_eq!(
            tx.sig_hash().to_string(),
            v["sigHash"],
            "{name}: signature hash"
        );
        tx.signature = v["signature"].as_str().unwrap().parse().unwrap();
        assert_eq!(
            Bytes::from(tx.encode()).to_string(),
            v["raw"],
            "{name}: encoding"
        );
        assert_eq!(tx.hash().to_string(), v["hash"], "{name}: hash");
        let ours = serde_json::to_string(&tx.to_wire()).unwrap().to_lowercase();
        let theirs = serde_json::to_string(&v["wire"]).unwrap().to_lowercase();
        assert_eq!(
            serde_json::from_str::<Value>(&ours).unwrap(),
            serde_json::from_str::<Value>(&theirs).unwrap(),
            "{name}: wire JSON"
        );
    }
}

#[test]
fn signs_as_the_swaps_key() {
    let keys = derive_user_keys(&[7; 32], NetworkType::Test, 0, 3).unwrap();
    let mut tx = unsigned(&vectors()[0]);
    tx.sender = Address::from(keys.auth.address());
    let tx = tx.sign(&keys.auth);
    let sig = &tx.signature;
    assert_eq!(sig.len(), 65);
    // `y_parity ‖ r ‖ s` here; `r ‖ s ‖ v` for the core's recovery.
    let mut rsv = [0; 65];
    rsv[..64].copy_from_slice(&sig[1..]);
    rsv[64] = sig[0] + 27;
    assert_eq!(signer(&tx.sig_hash().0, &rsv), Some(keys.auth.address()));
}
