//! The relayer's API. A relayer sends the transactions of users with no account on the chain:
//! each is authorized by the swap's own key or carries the revealed share, so a relayer can
//! delay a swap but never redirect it. Each request that acts on a swap carries its terms
//! (`crate::Terms`), which the relayer checks against the chain before sending anything.

use alloy_primitives::{Address, B256, Bytes, FixedBytes};
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
    /// Absent when initial reverse funding is not sponsored.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reverse_funding: Option<ReverseFundingTerms>,
    /// Absent when the relayer sends no private Railgun sends or withdrawals.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub railgun_sends: Option<RailgunSendTerms>,
}

/// What the relayer takes to send a wallet's own Railgun transaction (`POST /v1/railgun/transact`)
/// as its broadcaster: the transaction's first output is a fee note to `railgunAddress`.
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RailgunSendTerms {
    /// The relayer's own 0zk address, which the fee note pays.
    pub railgun_address: String,
    /// Railgun's proxy on the relayer's chain, which the transaction calls.
    pub railgun_proxy: Address,
    /// The token the fee is paid in.
    pub token: Address,
    /// Token base units the fee note must carry at least, as a decimal string.
    #[serde(with = "decimal")]
    pub fee: u128,
    pub max_gas_limit: u64,
    /// The highest gas price the relayer pays, and so the highest minimum a proof may set.
    #[serde(with = "decimal")]
    pub max_gas_price_wei: u128,
    pub max_calldata_bytes: usize,
}

/// Sponsored Sepolia funding uses the V2 Relay Adapt ABI and no Railgun broadcaster fee note:
/// the funding instead transfers `fee` of the escrow token to the relayer in the same action.
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ReverseFundingTerms {
    pub relay_adapt: Address,
    pub token: Address,
    pub maker: Address,
    pub max_gas_limit: u64,
    #[serde(with = "decimal")]
    pub max_gas_price_wei: u128,
    pub max_calldata_bytes: usize,
    #[serde(with = "decimal")]
    pub fee: u128,
}

/// `POST /v1/lock-claim`.
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct LockClaim {
    pub swap_id: B256,
    pub terms: crate::Terms,
    pub deadline: u64,
    pub signature: FixedBytes<65>,
}

/// `POST /v1/claim`: reveals the user share under the held claim lock, then pays out. The
/// payout comes first so that the relayer is sure of its fee before it reveals anything. It
/// names the same swap and terms as the claim.
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct Claim {
    pub swap_id: B256,
    pub terms: crate::Terms,
    /// The user share, big-endian.
    pub secret: B256,
    pub payout: Payout,
}

/// `POST /v1/payout` retries a payout on its own.
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct Payout {
    pub swap_id: B256,
    pub terms: crate::Terms,
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
    pub terms: crate::Terms,
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

/// `POST /v1/railgun/transact`: a proved, unsigned Railgun `transact` call, as the wallet SDK
/// populates it for a broadcaster. Persist the exact bytes before posting, and post the same
/// bytes again after any answer but `200`, `400` or `409`.
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct RailgunTransact {
    pub chain_id: u64,
    pub to: Address,
    pub data: Bytes,
    #[serde(with = "decimal")]
    pub value: u128,
}

/// `409` from `POST /v1/railgun/transact`: a note the transaction spends is spent, or a
/// transaction the relayer sent spends it. `transactions` names the relayer's own that do, pending
/// or mined; it is empty when the notes went in one the relayer did not send or no longer
/// remembers.
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AlreadySpent {
    pub code: crate::service::ErrorCode,
    pub error: String,
    pub transactions: Vec<B256>,
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
