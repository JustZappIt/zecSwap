//! The relayer's API. A relayer sends the transactions of users with no account on the chain:
//! each is authorized by the swap's own key or carries the revealed share, so a relayer can
//! delay a swap but never redirect it.

use alloy_primitives::{Address, B256, FixedBytes};
use serde::{Deserialize, Serialize};
use zecswap_railgun::{ShieldCiphertext, ShieldNote};

use crate::decimal;

/// `GET /v1/terms`: whom a payout signature must name, and what it must pay.
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Terms {
    pub relayer: Address,
    pub chain_id: u64,
    pub contract: Address,
    /// Token base units the relayer keeps from a payout, as a decimal string.
    #[serde(with = "decimal")]
    pub fee: u128,
}

/// `POST /v1/lock-claim`.
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct LockClaim {
    pub swap_id: B256,
    pub deadline: u64,
    pub signature: FixedBytes<65>,
}

/// `POST /v1/claim`: reveals the user share under the held claim lock, then pays out. The
/// payout comes first so that the relayer is sure of its fee before it reveals anything.
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct Claim {
    pub swap_id: B256,
    /// The user share, big-endian.
    pub secret: B256,
    pub payout: Payout,
}

/// `POST /v1/payout` retries a payout on its own.
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct Payout {
    pub swap_id: B256,
    pub note: Note,
    #[serde(with = "decimal")]
    pub fee: u128,
    pub signature: FixedBytes<65>,
}

/// A single, expiring approval to re-shield returned funds. Legacy payloads lack required fields.
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct Rescue {
    pub swap_id: B256,
    pub note: Note,
    #[serde(with = "decimal")]
    pub fee: u128,
    pub nonce: u64,
    pub deadline: u64,
    pub signature: FixedBytes<65>,
}

/// The note a payout shields to.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct Note {
    pub npk: B256,
    pub encrypted_bundle: [B256; 3],
    pub shield_key: B256,
}

/// The transactions a request sent, in order.
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Sent {
    pub transactions: Vec<B256>,
}

impl From<&ShieldNote> for Note {
    fn from(note: &ShieldNote) -> Self {
        Self {
            npk: note.npk.into(),
            encrypted_bundle: note.ciphertext.encrypted_bundle.map(B256::from),
            shield_key: note.ciphertext.shield_key.into(),
        }
    }
}

impl From<&Note> for ShieldNote {
    fn from(note: &Note) -> Self {
        Self {
            npk: note.npk.0,
            ciphertext: ShieldCiphertext {
                encrypted_bundle: note.encrypted_bundle.map(|word| word.0),
                shield_key: note.shield_key.0,
            },
        }
    }
}
