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
    /// The contract's kind, `windows` (`WINDOWS_CONTRACT`), and its minimum and late windows in
    /// seconds, as the contract reports them. Absent from a relayer on the lock contract.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub contract_kind: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub min_window: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub late_window: Option<u64>,
    /// Token base units the relayer keeps from a payout, as a decimal string. Where the relayer
    /// prices gas, the larger of its floor and the gas a swap's claim and payout (or refund and
    /// its payout) burn, at the gas price and ETH price now.
    #[serde(with = "decimal")]
    pub fee: u128,
    /// Until when this `fee` and `reverseFunding.fee` are honored: the relayer takes any fee at
    /// or above the lowest it quoted in the hour before. Absent where it prices no gas.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub fee_expires_at: Option<u64>,
    /// Absent when initial reverse funding is not sponsored.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reverse_funding: Option<ReverseFundingTerms>,
    /// Absent when the relayer sends no private Railgun sends or withdrawals.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub railgun_sends: Option<RailgunSendTerms>,
    /// Absent when the relayer takes no deposits.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub deposits: Option<DepositTerms>,
}

/// What the relayer takes to shield a deposit address's tokens into its owner's Railgun balance
/// (`POST /v1/deposits/shield`), and to shield again what Railgun sent back to its vault
/// (`POST /v1/deposits/rescue`).
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct DepositTerms {
    /// The Deposits contract: every authorization's `to`.
    pub contract: Address,
    /// The account the relayer sends deposits from: an authorization's nonce commits to it, and
    /// its fee goes there.
    pub relayer: Address,
    pub token: Address,
    /// Token base units of the fee before its percentage, as a decimal string: the floor, or
    /// where the relayer prices gas, a deposit's gas at the gas price and ETH price now if more.
    /// A rescue pays this alone.
    #[serde(with = "decimal")]
    pub fee: u128,
    /// The fee's part in basis points of what is deposited (`deposit_fee`).
    pub fee_bps: u32,
    /// Until when `fee` is honored, where gas prices it: an authorization signed with it is
    /// taken until then though gas rose since.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub fee_expires_at: Option<u64>,
    /// The least a deposit moves, in token base units, as a decimal string.
    #[serde(with = "decimal")]
    pub min_value: u128,
    /// The latest an authorization's `validBefore` may be, in seconds from now.
    pub max_validity: u64,
    /// The highest gas price the relayer pays, as a decimal string: above it deposits wait.
    #[serde(with = "decimal")]
    pub max_gas_price_wei: u128,
}

/// What a deposit of `value` pays: the terms' `fee` and its `fee_bps` part of `value`, rounded up.
pub fn deposit_fee(fee: u128, fee_bps: u32, value: u128) -> u128 {
    fee.saturating_add(value.saturating_mul(fee_bps.into()).div_ceil(10_000))
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
    /// The fee's gas-based part, as Railgun's broadcasters quote `feePerUnitGas`: token base
    /// units per 10^18 wei of gas cost, the relayer's margin included. A send pays the larger of
    /// `fee` and this times the wei its gas costs, over 10^18. Absent while the relayer can't price
    /// gas, or prices none: then `fee` alone.
    #[serde(
        default,
        with = "crate::optional_decimal",
        skip_serializing_if = "Option::is_none"
    )]
    pub fee_per_unit_gas: Option<u128>,
    /// Until when a proof whose fee was worked out at this rate is held to it, not a later one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub fee_expires_at: Option<u64>,
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
    /// What the funding must pay the relayer, priced like a payout's fee: its floor, or the gas
    /// of the funding and its ready at the prices now if more.
    #[serde(with = "decimal")]
    pub fee: u128,
}

/// `POST /v1/claim`: reveals the user share in the claim's window, then pays out. The payout is
/// checked first so that the relayer is sure of its fee before it reveals anything. It names the
/// same swap and terms as the claim.
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

/// `409`: what the request asks is done, or under way in another transaction. From
/// `POST /v1/railgun/transact` (`alreadySpent`): a note the transaction spends is spent, or a
/// transaction the relayer sent spends it. From `POST /v1/deposits/*` (`conflict`): the
/// authorization is used, or another of the same deposit address is on its way. `transactions`
/// names the relayer's own that do it, pending or mined; it is empty when that is one the relayer
/// did not send or no longer remembers.
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Conflict {
    pub code: crate::service::ErrorCode,
    pub error: String,
    pub transactions: Vec<B256>,
}

/// `POST /v1/deposits/shield`: a deposit address's EIP-3009 `receiveWithAuthorization` of `value`
/// to the Deposits contract, whose nonce commits to `note`, `fee` and the terms' `relayer`
/// (`zecswap_core::DepositsDomain::deposit_nonce`). Persist the exact body before posting, and
/// post the same bytes again after any answer but `200`, `400` or `409`.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct ShieldDeposit {
    pub chain_id: u64,
    pub contract: Address,
    /// The deposit address.
    pub from: Address,
    /// Token base units the authorization moves, as a decimal string.
    #[serde(with = "decimal")]
    pub value: u128,
    pub valid_after: u64,
    pub valid_before: u64,
    /// The note the deposit shields to.
    pub note: Note,
    /// What the relayer keeps, as a decimal string.
    #[serde(with = "decimal")]
    pub fee: u128,
    /// `r ‖ s ‖ v` over the token's EIP-712 digest of the authorization.
    pub signature: FixedBytes<65>,
}

/// `POST /v1/deposits/rescue`: shields what Railgun sent back to `from`'s vault again, to `note`,
/// on `from`'s signature (`zecswap_core::DepositsDomain::rescue`) naming the terms' `relayer`.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct RescueDeposit {
    pub chain_id: u64,
    pub contract: Address,
    pub from: Address,
    pub note: Note,
    #[serde(with = "decimal")]
    pub fee: u128,
    /// The vault's `rescueNonces(from)`.
    pub nonce: u64,
    pub deadline: u64,
    pub signature: FixedBytes<65>,
}

/// The transactions a request sent, in order.
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Sent {
    pub transactions: Vec<B256>,
}

impl Terms {
    /// Whether these terms fit a windows contract with these windows (`fits_windows`).
    pub fn fits_windows(&self, windows: (u64, u64)) -> bool {
        crate::fits_windows(
            self.contract_kind.as_deref(),
            self.min_window,
            self.late_window,
            windows,
        )
    }
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_deposits_percentage_is_rounded_up_onto_the_fee() {
        assert_eq!(deposit_fee(100_000, 10, 25_000_000), 125_000);
        assert_eq!(deposit_fee(100_000, 10, 1), 100_001);
        assert_eq!(deposit_fee(0, 10, 9_999), 10);
        assert_eq!(deposit_fee(0, 10, 10_000), 10);
        assert_eq!(deposit_fee(100_000, 0, 25_000_000), 100_000);
        assert_eq!(deposit_fee(u128::MAX, 10, u128::MAX), u128::MAX);
    }

    #[test]
    fn terms_name_the_windows_contract_and_a_lock_relayers_still_read() {
        let lock = r#"{"relayer":"0x2222222222222222222222222222222222222222","chainId":11155111,
            "contract":"0x3333333333333333333333333333333333333333","fee":"100000"}"#;
        let lock: Terms = serde_json::from_str(lock).unwrap();
        assert_eq!(lock.contract_kind, None);
        assert!(
            lock.fits_windows((1_800, 3_600)),
            "the wallet reads the contract itself"
        );
        let windows = Terms {
            contract_kind: Some(crate::WINDOWS_CONTRACT.into()),
            min_window: Some(1_800),
            late_window: Some(3_600),
            ..lock
        };
        let json = serde_json::to_value(&windows).unwrap();
        assert_eq!(json["contractKind"], "windows");
        assert_eq!(
            (json["minWindow"].clone(), json["lateWindow"].clone()),
            (1_800.into(), 3_600.into())
        );
        assert!(windows.fits_windows((1_800, 3_600)));
        assert!(!windows.fits_windows((1_800, 7_200)));
        assert!(!windows.fits_windows((600, 3_600)));
        let other = Terms {
            contract_kind: Some("locks".into()),
            ..windows
        };
        assert!(!other.fits_windows((1_800, 3_600)));
    }
}
