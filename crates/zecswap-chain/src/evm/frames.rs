//! EIP-8141 frame transactions as the EF's alt-mempool devnet takes them, and the sponsored claim
//! built from them: the swap's own key sends its claim, and a sponsor contract pays the gas
//! (docs/frames.md). Frame transactions exist on that devnet alone (chain 1337, ethrex
//! `eip8141-tracer`), which runs an older draft than the EIP text: this is the layout of
//! `@forshtat/hegota-support`'s `ethrex-eip8141-tracer` profile, which `tests/frame-vectors.json`
//! pins byte for byte.

use std::time::Duration;

use alloy::primitives::{Address, B256, Bytes, U256, keccak256};
use alloy::providers::{DynProvider, Provider};
use alloy::rlp::{Encodable, Header};
use alloy::sol_types::SolCall;
use serde_json::{Value, json};
use zecswap_core::{AuthKey, SecretShare, Terms};

use super::{IClaimSponsor, IZecSwap, RpcMeter, Settlement, signing_provider, terms_words};
use crate::Error;

const TX_TYPE: u8 = 0x06;
const SECP256K1: u8 = 1;
const EMPTY: &[u8] = &[];
/// What a frame transaction pays per gas, priority fee included: the devnet's workshop's price.
pub const MAX_FEE_PER_GAS: u128 = 2_000_000_000;
/// How long a mempool node keeps a transaction it hasn't included (`useropsTTL`): after that the
/// claim goes out again, the same bytes.
pub const RESEND_AFTER: Duration = Duration::from_secs(300);

/// What a frame runs as: called by the entry point, a check that must APPROVE, or the sender.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Mode {
    Default = 0,
    Verify = 1,
    Sender = 2,
}

/// What a VERIFY frame's APPROVE grants, in its flags.
pub const APPROVE_PAYMENT: u8 = 1;
pub const APPROVE_EXECUTION: u8 = 2;

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Frame {
    pub mode: Mode,
    pub flags: u8,
    /// None calls the transaction's sender.
    pub target: Option<Address>,
    /// Its execution and state gas limits, which the devnet keeps apart.
    pub execution: u64,
    pub state: u64,
    pub value: U256,
    pub data: Bytes,
}

/// A frame transaction from an account with no code, on its linear nonce: one secp256k1
/// signature by the sender over the signature hash, no blobs and no recent roots.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct FrameTx {
    pub chain_id: u64,
    pub nonce: u64,
    pub sender: Address,
    pub frames: Vec<Frame>,
    pub max_priority_fee_per_gas: u128,
    pub max_fee_per_gas: u128,
    /// `y_parity ‖ r ‖ s`; empty until signed.
    pub signature: Bytes,
}

impl FrameTx {
    /// `0x06 ‖ rlp([chain_id, nonce_keys, nonce_seq, sender, frames, signatures, fees,
    /// blob_versioned_hashes, recent_root_references])`, its nonce key the linear nonce's.
    pub fn encode(&self) -> Vec<u8> {
        self.encode_signed_as(&self.signature)
    }

    /// What the sender signs: the encoding with the signature's bytes left empty, as an entry
    /// with no `msg` signs it.
    pub fn sig_hash(&self) -> B256 {
        keccak256(self.encode_signed_as(EMPTY))
    }

    pub fn hash(&self) -> B256 {
        keccak256(self.encode())
    }

    /// Signed by `key`, which must be the sender's.
    pub fn sign(mut self, key: &AuthKey) -> Self {
        let rsv = key.sign(&self.sig_hash().0);
        let mut signature = Vec::with_capacity(65);
        signature.push(rsv[64] - 27);
        signature.extend_from_slice(&rsv[..64]);
        self.signature = signature.into();
        self
    }

    /// The JSON a mempool node's `eth_sendFrameTransaction` takes.
    pub fn to_wire(&self) -> Value {
        json!({
            "chainId": quantity(self.chain_id),
            "nonceKeys": ["0x0"],
            "nonceSeq": quantity(self.nonce),
            "sender": self.sender,
            "frames": self.frames.iter().map(Frame::to_wire).collect::<Vec<_>>(),
            "signatures": [{ "scheme": SECP256K1, "signer": "0x", "msg": "0x", "signature": self.signature }],
            "fees": {
                "maxPriorityFeePerGas": quantity(self.max_priority_fee_per_gas),
                "maxFeePerGas": quantity(self.max_fee_per_gas),
                "maxFeePerBlobGas": "0x0",
            },
            "blobVersionedHashes": [],
            "recentRootReferences": [],
        })
    }

    fn encode_signed_as(&self, signature: &[u8]) -> Vec<u8> {
        let mut payload = Vec::new();
        self.chain_id.encode(&mut payload);
        list(&mut payload, |out| 0u8.encode(out));
        self.nonce.encode(&mut payload);
        self.sender.encode(&mut payload);
        list(&mut payload, |out| {
            self.frames.iter().for_each(|frame| frame.encode(out))
        });
        list(&mut payload, |out| {
            list(out, |out| {
                SECP256K1.encode(out);
                // The sender signs, over the signature hash.
                EMPTY.encode(out);
                EMPTY.encode(out);
                signature.encode(out);
            })
        });
        list(&mut payload, |out| {
            self.max_priority_fee_per_gas.encode(out);
            self.max_fee_per_gas.encode(out);
            0u8.encode(out);
        });
        list(&mut payload, |_| {});
        list(&mut payload, |_| {});
        let mut tx = vec![TX_TYPE];
        Header {
            list: true,
            payload_length: payload.len(),
        }
        .encode(&mut tx);
        tx.extend(payload);
        tx
    }
}

impl Frame {
    fn encode(&self, out: &mut Vec<u8>) {
        list(out, |out| {
            (self.mode as u8).encode(out);
            self.flags.encode(out);
            match &self.target {
                Some(target) => target.encode(out),
                None => EMPTY.encode(out),
            }
            list(out, |out| {
                self.execution.encode(out);
                self.state.encode(out);
            });
            self.value.encode(out);
            self.data.encode(out);
        });
    }

    fn to_wire(&self) -> Value {
        json!({
            "mode": self.mode as u8,
            "flags": self.flags,
            "target": self.target,
            "limits": { "execution": quantity(self.execution), "state": quantity(self.state) },
            "value": format!("{:#x}", self.value),
            "data": self.data,
        })
    }
}

fn list(out: &mut Vec<u8>, items: impl FnOnce(&mut Vec<u8>)) {
    let mut payload = Vec::new();
    items(&mut payload);
    Header {
        list: true,
        payload_length: payload.len(),
    }
    .encode(out);
    out.extend(payload);
}

fn quantity(n: impl Into<u128>) -> String {
    format!("{:#x}", n.into())
}

/// The claim's gas limits, fixed: an estimate would show a node the share before the claim goes
/// out. The devnet probe of 2026-11-06 measured 100, 29,891, 194,468 and 36,247 execution gas.
/// The claim creates two storage slots (the share, the credit) at 97,920 state gas each, though
/// its receipt shows one: the withdrawal empties the credit, and the node takes that refund off
/// the claim's receipt. The two checks, with the signature's 2,800, stay under the 100,000 a
/// mempool lets a transaction's checks use.
const VERIFY_GAS: u64 = 20_000;
const PAY_GAS: u64 = 60_000;
const CLAIM_GAS: u64 = 400_000;
const CLAIM_STATE_GAS: u64 = 300_000;
const WITHDRAW_GAS: u64 = 100_000;
const WITHDRAW_STATE_GAS: u64 = 150_000;

/// The claim of swap `id` that its own key, `terms.user`, sends as its first transaction, and
/// `sponsor` pays for: the key approves executing it, the sponsor approves paying, then the claim
/// and the withdrawal of its amount to `to`. Those two are separate frames, never one atomic
/// batch: a batch that failed would undo the claim with its share already on chain, while a
/// failed withdrawal leaves the amount credited in the contract.
pub fn claim_tx(
    chain_id: u64,
    contract: Address,
    sponsor: Address,
    id: B256,
    terms: &Terms,
    share: &SecretShare,
    to: Address,
) -> FrameTx {
    let words = terms_words(terms);
    let pay = IClaimSponsor::sponsorCall {
        id,
        terms: words.clone(),
    };
    let claim = IZecSwap::claimCall {
        id,
        terms: words,
        userSecret: U256::from_be_bytes(share.to_be_bytes()),
    };
    let withdraw = IZecSwap::withdrawCall {
        token: terms.token.into(),
        amount: U256::from(terms.amount),
        to,
    };
    let verify = |target, flags, execution, data: Vec<u8>| Frame {
        mode: Mode::Verify,
        flags,
        target,
        execution,
        state: 0,
        value: U256::ZERO,
        data: data.into(),
    };
    let send = |execution, state, data: Vec<u8>| Frame {
        mode: Mode::Sender,
        flags: 0,
        target: Some(contract),
        execution,
        state,
        value: U256::ZERO,
        data: data.into(),
    };
    FrameTx {
        chain_id,
        nonce: 0,
        sender: terms.user.into(),
        frames: vec![
            verify(None, APPROVE_EXECUTION, VERIFY_GAS, Vec::new()),
            verify(Some(sponsor), APPROVE_PAYMENT, PAY_GAS, pay.abi_encode()),
            send(CLAIM_GAS, CLAIM_STATE_GAS, claim.abi_encode()),
            send(WITHDRAW_GAS, WITHDRAW_STATE_GAS, withdraw.abi_encode()),
        ],
        max_priority_fee_per_gas: MAX_FEE_PER_GAS,
        max_fee_per_gas: MAX_FEE_PER_GAS,
        signature: Bytes::new(),
    }
}

/// Where a frame transaction's devnet explorer page is: under its "Miner" instance, whose history
/// holds what the miner's own mempool node admitted.
pub fn explorer_link(hash: B256) -> String {
    format!("https://explorer.altmempool.wtf/tx/{hash}")
}

/// An ERC-8421 mempool node: frame transactions reach a block only through one.
pub struct Mempool {
    provider: DynProvider,
}

impl Mempool {
    pub fn new(rpc_url: &str) -> Result<Self, Error> {
        Ok(Self {
            provider: signing_provider(rpc_url, None, &RpcMeter::default())?,
        })
    }

    /// Hands `tx` to the node, which admits it only if its checks pass the mempool's rules. One it
    /// holds already counts as sent.
    pub async fn send(&self, tx: &FrameTx) -> Result<B256, Error> {
        match self
            .provider
            .raw_request::<_, B256>("eth_sendFrameTransaction".into(), (tx.to_wire(),))
            .await
        {
            Ok(hash) => Ok(hash),
            Err(e) if e.to_string().contains("already known") => Ok(tx.hash()),
            Err(e) => Err(Error::Contract(format!("the mempool refused it: {e}"))),
        }
    }
}

impl Settlement {
    /// Whether each frame of the mined frame transaction `hash` succeeded; none until it is mined.
    /// From the node's receipt as JSON: alloy decodes no type-0x06 receipt.
    pub async fn frame_outcomes(&self, hash: B256) -> Result<Option<Vec<bool>>, Error> {
        let receipt: Option<Value> = self
            .provider
            .raw_request("eth_getTransactionReceipt".into(), (hash,))
            .await
            .map_err(Error::contract)?;
        Ok(receipt.map(|receipt| {
            receipt["frameReceipts"]
                .as_array()
                .map(|frames| {
                    frames
                        .iter()
                        .map(|frame| frame["status"] == "0x1")
                        .collect()
                })
                .unwrap_or_default()
        }))
    }
}
