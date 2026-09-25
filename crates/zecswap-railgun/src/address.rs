use bech32::{Bech32m, Hrp};

use crate::keys::Receiver;

const VERSION: u8 = 1;
/// Railgun's network id for "all chains", `ff…ff`, XORed with `"railgun\0"` as its addresses
/// store every network id.
const ALL_CHAINS: [u8; 8] = {
    let mut id = [0xff; 8];
    let railgun = b"railgun";
    let mut i = 0;
    while i < railgun.len() {
        id[i] ^= railgun[i];
        i += 1;
    }
    id
};

/// The bech32m `0zk` address: version, master public key, network id, viewing public key.
pub(crate) fn encode(receiver: &Receiver) -> String {
    let mut data = Vec::with_capacity(73);
    data.push(VERSION);
    data.extend_from_slice(&receiver.master_public_key);
    data.extend_from_slice(&ALL_CHAINS);
    data.extend_from_slice(&receiver.viewing_public_key);
    bech32::encode::<Bech32m>(Hrp::parse_unchecked("0zk"), &data)
        .expect("a 73-byte address is within bech32m's length limit")
}
